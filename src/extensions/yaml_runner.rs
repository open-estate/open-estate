//! Declarative YAML & OpenAPI 3.x execution engine (`DRIVER_DECLARATIVE_YAML`, `DRIVER_OPENAPI_YAML`).
//!
//! Executes upstream HTTP calls and declarative tariff/footprint catalogs without custom per-vendor
//! Rust code:
//! - Resolves OpenAPI 3.x operations (`openapiv3::OpenAPI`) or declarative `request` specs.
//! - Interpolates `minijinja` query/path/header/body templates (`{{ lat }}`, `{{ lon }}`, `{{ zip }}`,
//!   `{{ state }}`, `{{ city }}`, `{{ county_fips }}`, `{{ address }}`).
//! - Enforces SSRF safety (`validate_ssrf_safe_public_url`), `governor` GCRA rate limits, and `moka`
//!   response caching.
//! - Extracts RFC 9535 JSONPath (`serde_json_path`) values into strongly-typed Protobuf domain
//!   envelopes with mandatory `SourceAttributionProto` citations.

use crate::extensions::manifest::{
    BroadbandRuleYaml, ElectricRuleYaml, GasRuleYaml, GovResponseFormat, NormalizedGeoQuery,
    ParsedExtensionPackage, ResponseMappingYaml, WaterSewerTrashRuleYaml,
};
use crate::extensions::proto::{
    extension_envelope_proto::Payload, BroadbandCatalogProto, BroadbandOptionProto,
    CapabilityDomain, CodeEnforcementIntelProto, DriverKind, ElectricUtilityProto,
    EnvironmentalSummaryProto, ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto,
    GasUtilityProto, GeocodeResultProto, HazardSummaryProto, House3dMetadataProto,
    InsuranceQuoteProto, LivabilitySummaryProto, MarketValuationProto, MepIntelProto,
    PermitRecordProto, PermitsHistoryProto, PropertyFactsProto, ProvenanceTier, RateLimitSpecProto,
    RemodelIntelProto, RoofIntelProto, RoutingMatrixProto, SolarIntelProto, SourceAttributionProto,
    TaxMandatesProto, WaterSewerTrashProto,
};
use crate::shared::helpers::validate_ssrf_safe_public_url;
use crate::shared::{now_iso, EnvConfig, HttpClient};
use anyhow::{anyhow, Context, Result};
use governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota};
use minijinja::Environment;
use moka::future::Cache;
use once_cell::sync::Lazy;
use openapiv3::{
    Operation, Parameter, ParameterData, ParameterSchemaOrContent, PathItem, ReferenceOr,
    SchemaKind, Type,
};
use serde_json::{json, Map, Value};
use serde_json_path::JsonPath;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};
use url::Url;

/// Per-cache entry storing fetch time so each extension's declared `cache_ttl_secs` is strictly enforced.
#[derive(Debug, Clone)]
pub struct CachedEnvelopeEntry {
    pub fetched_at: Instant,
    pub envelopes: Vec<ExtensionEnvelopeProto>,
}

/// Builds a per-extension GCRA `governor` rate limiter from `manifest.rate_limit`.
pub fn build_extension_rate_limiter(rl: Option<&RateLimitSpecProto>) -> DefaultDirectRateLimiter {
    let rps = rl
        .map(|r| r.requests_per_second)
        .filter(|&n| n > 0)
        .unwrap_or(5)
        .clamp(1, 200);
    let burst = rl
        .map(|r| r.burst)
        .filter(|&n| n > 0)
        .unwrap_or(rps.saturating_mul(2))
        .clamp(1, 400);
    let quota = Quota::per_second(NonZeroU32::new(rps).expect("non-zero rps"))
        .allow_burst(NonZeroU32::new(burst).expect("non-zero burst"));
    DefaultDirectRateLimiter::direct(quota)
}

/// Returns true when an extension declares `auth.optional_key: true` with a non-`none` auth scheme
/// and its environment variable is currently unset/blank.
pub fn is_optional_auth_missing(pkg: &ParsedExtensionPackage, env: &EnvConfig) -> bool {
    if !pkg.doc.auth.optional_key {
        return false;
    }
    let auth_type = pkg.doc.auth.auth_type.trim().to_ascii_lowercase();
    if auth_type == "none" || auth_type.is_empty() {
        return false;
    }
    pkg.doc
        .auth
        .env_var
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|var| {
            env.get(var)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
        })
        .unwrap_or(false)
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Canonical carrier brand key used across `yaml_runner` and `aggregator` so live FCC BDC
/// observations (`"AT&T Services"`, `"Comcast Cable"`) deduplicate cleanly against regional
/// carrier footprint entries (`"AT&T Fiber"`, `"Xfinity (Comcast)"`).
pub fn normalize_carrier_brand_key(raw: &str) -> String {
    let lc = raw.trim().to_ascii_lowercase();
    if lc.contains("at&t") || lc.contains("att ") || lc == "att" || lc.contains("southwestern bell") || lc.contains("pacific bell") {
        return "att".to_string();
    }
    if lc.contains("google fiber") || lc.contains("gfiber") || lc.contains("webpass") {
        return "google_fiber".to_string();
    }
    if lc.contains("verizon") || lc.contains("fios") {
        return "verizon".to_string();
    }
    if lc.contains("frontier") {
        return "frontier".to_string();
    }
    if lc.contains("quantum") || lc.contains("centurylink") || lc.contains("lumen") || lc.contains("qwest") {
        return "quantum_lumen".to_string();
    }
    if lc.contains("ziply") {
        return "ziply".to_string();
    }
    if lc.contains("metronet") {
        return "metronet".to_string();
    }
    if lc.contains("ezee") {
        return "ezee_fiber".to_string();
    }
    if lc.contains("tachus") {
        return "tachus".to_string();
    }
    if lc.contains("ting") {
        return "ting".to_string();
    }
    if lc.contains("epb") {
        return "epb".to_string();
    }
    if lc.contains("optimum") || lc.contains("altice") || lc.contains("cablevision") || lc.contains("suddenlink") {
        return "optimum".to_string();
    }
    if lc.contains("xfinity") || lc.contains("comcast") {
        return "xfinity".to_string();
    }
    if lc.contains("spectrum") || lc.contains("charter") {
        return "spectrum".to_string();
    }
    if lc.contains("cox") {
        return "cox".to_string();
    }
    if lc.contains("astound") || lc.contains("wave") || lc.contains("rcn") || lc.contains("grande") {
        return "astound".to_string();
    }
    if lc.contains("mediacom") {
        return "mediacom".to_string();
    }
    if lc.contains("t-mobile") || lc.contains("tmobile") {
        return "tmobile".to_string();
    }
    if lc.contains("starlink") || lc.contains("spacex") {
        return "starlink".to_string();
    }
    if lc.contains("viasat") {
        return "viasat".to_string();
    }
    if lc.contains("hughes") {
        return "hughesnet".to_string();
    }
    lc.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect::<String>()
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("_")
}

fn derive_house_number_and_street_stem(street: &str) -> (String, String) {
    let tokens: Vec<&str> = street.split_whitespace().collect();
    if tokens.is_empty() {
        return (String::new(), String::new());
    }
    let (house_num, rest) = if tokens[0]
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
        && tokens.len() > 1
    {
        (tokens[0].to_string(), &tokens[1..])
    } else {
        (String::new(), tokens.as_slice())
    };
    let mut upper_words: Vec<String> = rest
        .iter()
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_uppercase())
        .filter(|t| !t.is_empty())
        .collect();
    if upper_words.len() >= 2 {
        let last = upper_words.last().map(String::as_str).unwrap_or("");
        if matches!(
            last,
            "ST" | "STREET"
                | "AVE"
                | "AVENUE"
                | "RD"
                | "ROAD"
                | "DR"
                | "DRIVE"
                | "LN"
                | "LANE"
                | "BLVD"
                | "BOULEVARD"
                | "CT"
                | "COURT"
                | "PL"
                | "PLACE"
                | "WAY"
                | "PKWY"
                | "PARKWAY"
                | "TER"
                | "TERRACE"
                | "CIR"
                | "CIRCLE"
        ) {
            upper_words.pop();
        }
    }
    let stem = upper_words.join(" ").replace('\'', "''");
    (house_num.replace('\'', "''"), stem)
}

/// Build the `minijinja` template context map from `ExtensionQueryProto`.
pub fn build_template_context(query: &ExtensionQueryProto) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();
    let zip5: String = query.zip.trim().chars().take(5).collect();
    let addr_trimmed = query.address.trim();
    ctx.insert("address".to_string(), addr_trimmed.to_string());
    ctx.insert(
        "address_urlencoded".to_string(),
        urlencoding::encode(addr_trimmed).into_owned(),
    );
    let street_trimmed = query.street.trim();
    ctx.insert("street".to_string(), street_trimmed.to_string());
    ctx.insert(
        "street_urlencoded".to_string(),
        urlencoding::encode(street_trimmed).into_owned(),
    );
    let (house_number, street_stem) = derive_house_number_and_street_stem(street_trimmed);
    ctx.insert("house_number".to_string(), house_number);
    ctx.insert("street_stem".to_string(), street_stem);
    let city_trimmed = query.city.trim();
    ctx.insert("city".to_string(), city_trimmed.to_string());
    ctx.insert(
        "city_urlencoded".to_string(),
        urlencoding::encode(city_trimmed).into_owned(),
    );
    ctx.insert("state".to_string(), query.state.trim().to_ascii_uppercase());
    ctx.insert("zip".to_string(), zip5);
    ctx.insert(
        "county_fips".to_string(),
        query.county_fips.trim().to_string(),
    );
    ctx.insert(
        "county_name".to_string(),
        query
            .county_name
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
    );
    ctx.insert(
        "lat".to_string(),
        query
            .lat
            .filter(|v| v.is_finite())
            .map(|v| format!("{v:.6}"))
            .unwrap_or_default(),
    );
    ctx.insert(
        "lon".to_string(),
        query
            .lon
            .filter(|v| v.is_finite())
            .map(|v| format!("{v:.6}"))
            .unwrap_or_default(),
    );
    if let Some(sqft) = query.sqft {
        ctx.insert("sqft".to_string(), sqft.to_string());
    }
    if let Some(yb) = query.year_built {
        ctx.insert("year_built".to_string(), yb.to_string());
    }
    if let Some(ref apn) = query.apn {
        let apn_t = apn.trim();
        if !apn_t.is_empty() {
            let digits: String = apn_t.chars().filter(|c| c.is_ascii_digit()).collect();
            let formatted = if query.county_fips.trim() == "06085" && digits.len() == 8 {
                format!("{}-{}-{}", &digits[..3], &digits[3..5], &digits[5..8])
            } else {
                apn_t.to_string()
            };
            ctx.insert("apn".to_string(), formatted.clone());
            ctx.insert(
                "apn_urlencoded".to_string(),
                urlencoding::encode(&formatted).into_owned(),
            );
            if !digits.is_empty() {
                ctx.insert("apn_digits".to_string(), digits);
            }
        }
    }
    if let Some(ref pwsid) = query.sdwis_pwsid {
        let p_t = pwsid.trim();
        if !p_t.is_empty() {
            ctx.insert("pwsid".to_string(), p_t.to_string());
        }
    }
    if let Some(ref parent_addr) = query.parent_address {
        if !parent_addr.trim().is_empty() {
            ctx.insert("parent_address".to_string(), parent_addr.trim().to_string());
        }
    }
    for (k, v) in &query.extra_params {
        if !v.trim().is_empty() {
            ctx.insert(k.clone(), v.trim().to_string());
        }
    }
    if !ctx.contains_key("dest_lat") {
        if let Some(la) = query.lat.filter(|v| v.is_finite()) {
            ctx.insert("dest_lat".to_string(), format!("{:.6}", la + 0.01));
        }
    }
    if !ctx.contains_key("dest_lon") {
        if let Some(lo) = query.lon.filter(|v| v.is_finite()) {
            ctx.insert("dest_lon".to_string(), format!("{:.6}", lo + 0.01));
        }
    }
    ctx
}

static TPL_VAR_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"\{\{\s*([a-zA-Z0-9_]+)\s*\}\}").unwrap());

/// Resolves `source_attribution.reference_url_template` against `ctx` when all referenced
/// placeholders are populated, otherwise falling back cleanly to `sa.url` / `fallback_url`.
pub fn resolve_manifest_reference_url(
    pkg: &ParsedExtensionPackage,
    ctx: &BTreeMap<String, String>,
    fallback_url: &str,
) -> String {
    if let Some(sa) = pkg.doc.source_attribution.as_ref() {
        if let Some(tpl) = sa
            .reference_url_template
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !tpl.contains("{{") {
                return tpl.to_string();
            }
            let all_vars_present = TPL_VAR_RE.captures_iter(tpl).all(|cap| {
                let var = &cap[1];
                ctx.get(var).map(|v| !v.trim().is_empty()).unwrap_or(false)
            });
            if all_vars_present {
                if let Ok(rendered) = YAML_EXPR_JINJA_ENV.render_str(tpl, ctx) {
                    let trimmed = rendered.trim();
                    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
                        return trimmed.to_string();
                    }
                }
            }
        }
        if !sa.url.trim().is_empty() {
            if let Ok(rendered) = render_query_template(sa.url.trim(), ctx) {
                let trimmed = rendered.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
            return sa.url.trim().to_string();
        }
    }
    fallback_url.trim().to_string()
}

/// Resolves an extension manifest's live reference URL (including optional catalog `rule_id`)
/// from an `ExtensionManifestProto` + `ExtensionQueryProto` at redirect time.
pub fn resolve_manifest_proto_reference_url(
    manifest: &ExtensionManifestProto,
    rule_id: Option<&str>,
    query: &ExtensionQueryProto,
) -> Option<String> {
    if let Some(rid) = rule_id.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(tc) = manifest.tariff_catalog.as_ref() {
            for r in &tc.electric_rules {
                if r.id.eq_ignore_ascii_case(rid) && r.tariff_url.starts_with("https://") {
                    return Some(r.tariff_url.trim().to_string());
                }
            }
            for r in &tc.gas_rules {
                if r.id.eq_ignore_ascii_case(rid) && r.tariff_url.starts_with("https://") {
                    return Some(r.tariff_url.trim().to_string());
                }
            }
            for r in &tc.water_sewer_trash_rules {
                if r.id.eq_ignore_ascii_case(rid) {
                    if let Some(u) = r.water_tariff_url.as_deref() {
                        if u.starts_with("https://") {
                            return Some(u.trim().to_string());
                        }
                    }
                }
            }
        }
        if let Some(fc) = manifest.footprint_catalog.as_ref() {
            for r in &fc.broadband_rules {
                if r.id.eq_ignore_ascii_case(rid) && r.check_url.starts_with("https://") {
                    return Some(r.check_url.trim().to_string());
                }
            }
        }
    }

    let ctx = build_template_context(query);
    if let Some(sa) = manifest.source_attribution.as_ref() {
        let tpl = sa.reference_url_template.trim();
        if !tpl.is_empty() {
            if !tpl.contains("{{") {
                if tpl.starts_with("https://") {
                    return Some(tpl.to_string());
                }
            } else {
                let all_present = TPL_VAR_RE.captures_iter(tpl).all(|cap| {
                    ctx.get(&cap[1])
                        .map(|v| !v.trim().is_empty())
                        .unwrap_or(false)
                });
                if all_present {
                    if let Ok(rendered) = YAML_EXPR_JINJA_ENV.render_str(tpl, &ctx) {
                        let trimmed = rendered.trim();
                        if trimmed.starts_with("https://") {
                            return Some(trimmed.to_string());
                        }
                    }
                }
            }
        }
        let u = sa.url.trim();
        if u.starts_with("https://") {
            return Some(u.to_string());
        }
    }

    manifest
        .homepage_url
        .as_deref()
        .map(str::trim)
        .filter(|s| s.starts_with("https://"))
        .or_else(|| {
            manifest
                .documentation_url
                .as_deref()
                .map(str::trim)
                .filter(|s| s.starts_with("https://"))
        })
        .map(String::from)
}

static YAML_EXPR_JINJA_ENV: Lazy<Environment<'static>> = Lazy::new(Environment::new);

/// Render a `minijinja` template string against `ctx`, rejecting missing required template inputs
/// (e.g., `{{ lat }}` when `query.lat` is `None`).
pub fn render_query_template(tpl: &str, ctx: &BTreeMap<String, String>) -> Result<String> {
    if !tpl.contains("{{") {
        return Ok(tpl.to_string());
    }
    for required_var in ["lat", "lon", "address", "zip", "house_number", "street_stem"] {
        if (tpl.contains(&format!("{{{{ {required_var} }}}}"))
            || tpl.contains(&format!("{{{{{required_var}}}}}")))
            && ctx
                .get(required_var)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(anyhow!(
                "Query input '{required_var}' is required by template '{tpl}' but was empty"
            ));
        }
    }
    YAML_EXPR_JINJA_ENV
        .render_str(tpl, ctx)
        .with_context(|| format!("Failed to render template '{tpl}'"))
}

/// Synchronously evaluates declarative `tariff_catalog` and `footprint_catalog` rules for `pkg`.
pub fn execute_catalog_rules(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
) -> Vec<ExtensionEnvelopeProto> {
    let q_norm = NormalizedGeoQuery::from_proto(query);
    if pkg.compiled_geo_scope.match_specificity(&q_norm).is_none() {
        return Vec::new();
    }

    let now = now_iso();
    let mut out = Vec::new();

    if let Some(ref tc) = pkg.doc.tariff_catalog {
        if !tc.electric_rules.is_empty() {
            if let Some((spec, rule)) = pick_best_rule(&tc.electric_rules, query, |r| &r.geo_scope)
            {
                out.push(build_electric_envelope(
                    pkg, manifest, query, rule, spec, &now,
                ));
            }
        }
        if !tc.gas_rules.is_empty() {
            if let Some((spec, rule)) = pick_best_rule(&tc.gas_rules, query, |r| &r.geo_scope) {
                out.push(build_gas_envelope(pkg, manifest, rule, spec, &now));
            }
        }
        if !tc.water_sewer_trash_rules.is_empty() {
            if let Some((spec, rule)) =
                pick_best_rule(&tc.water_sewer_trash_rules, query, |r| &r.geo_scope)
            {
                out.push(build_water_envelope(
                    pkg, manifest, query, rule, spec, &now,
                ));
            }
        }
    }

    if let Some(ref fc) = pkg.doc.footprint_catalog {
        if !fc.broadband_rules.is_empty() {
            out.extend(build_broadband_footprint_envelopes(
                pkg,
                manifest,
                query,
                &fc.broadband_rules,
                &now,
            ));
        }
    }

    out
}

fn pick_best_rule<'a, R, F>(
    rules: &'a [R],
    query: &ExtensionQueryProto,
    scope_fn: F,
) -> Option<(i32, &'a R)>
where
    F: Fn(&R) -> &crate::extensions::manifest::GeoScopeYaml,
{
    let mut best: Option<(i32, &'a R)> = None;
    for rule in rules {
        if let Some(spec) = scope_fn(rule).match_specificity(query) {
            // Strictly `>` preserves YAML top-to-bottom declaration order on equal specificity.
            if best.as_ref().map(|(b_spec, _)| spec > *b_spec).unwrap_or(true) {
                best = Some((spec, rule));
            }
        }
    }
    best
}

fn build_electric_envelope(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    rule: &ElectricRuleYaml,
    spec: i32,
    now: &str,
) -> ExtensionEnvelopeProto {
    let st = query.state.trim().to_ascii_uppercase();
    let (mut carrier_name, mut short_name, mut avg_kwh, mut off_kwh, mut peak_kwh) = (
        rule.carrier_name.clone(),
        rule.short_name.clone(),
        rule.rate_per_kwh_usd,
        rule.off_peak_kwh_usd,
        rule.peak_kwh_usd,
    );

    if rule.id == "us_eia_regional_default" && !st.is_empty() {
        let r = crate::registry::rates_and_costs::regional_electricity_kwh_rate(&st);
        carrier_name = format!("Regional Regulated Utility ({st})");
        short_name = format!("{st} Regulated Electric");
        avg_kwh = r;
        // TOU peak/off-peak schedules are utility-specific; never synthesize fake 0.85x/1.25x spreads.
        off_kwh = 0.0;
        peak_kwh = 0.0;
    }

    let utility_type_label = rule.utility_type_label.clone().unwrap_or_else(|| {
        match rule.utility_type.as_str() {
            "municipal" => "Municipal Public Power",
            "deregulated_choice" => "Deregulated Retail Choice (TDU + REP)",
            "coop" => "Public Utility District / Electric Co-op",
            _ => "Investor-Owned Utility (IOU)",
        }
        .to_string()
    });

    let eff_avg = avg_kwh;

    let proto = ElectricUtilityProto {
        carrier_name,
        short_name,
        utility_type: rule.utility_type.clone(),
        utility_type_label,
        cca_program: rule.cca_program.clone(),
        rate_schedule: rule.rate_schedule.clone(),
        rate_per_kwh_usd: round3(eff_avg),
        off_peak_kwh_usd: round3(off_kwh),
        peak_kwh_usd: round3(peak_kwh),
        monthly_base_fee_usd: round2(rule.monthly_base_fee_usd),
        solar_nem_policy: rule.solar_nem_policy.clone(),
        tariff_url: rule.tariff_url.clone(),
        rate_source: rule.rate_source.clone(),
    };

    ExtensionEnvelopeProto {
        extension_id: pkg.extension_id.clone(),
        domain: CapabilityDomain::DomainElectricUtility as i32,
        priority: manifest.priority,
        geo_specificity: spec,
        attribution: Some(SourceAttributionProto {
            extension_id: pkg.extension_id.clone(),
            source_name: rule.rate_source.clone(),
            source_url: rule.tariff_url.clone(),
            provenance_tier: pkg.default_provenance as i32,
            fetched_at_rfc3339: now.to_string(),
            schedule_or_version: Some(rule.rate_schedule.clone()),
            notes: None,
        }),
        latency_ms: 0,
        cached: true,
        payload: Some(Payload::ElectricUtility(proto)),
    }
}

fn build_gas_envelope(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    rule: &GasRuleYaml,
    spec: i32,
    now: &str,
) -> ExtensionEnvelopeProto {
    let proto = GasUtilityProto {
        carrier_name: rule.carrier_name.clone(),
        heating_fuel_norm: rule.heating_fuel_norm.clone(),
        rate_schedule: rule.rate_schedule.clone(),
        rate_per_therm_usd: rule.rate_per_therm_usd.map(round2),
        monthly_base_fee_usd: round2(rule.monthly_base_fee_usd),
        tariff_url: rule.tariff_url.clone(),
        rate_source: rule.rate_source.clone(),
    };

    ExtensionEnvelopeProto {
        extension_id: pkg.extension_id.clone(),
        domain: CapabilityDomain::DomainGasUtility as i32,
        priority: manifest.priority,
        geo_specificity: spec,
        attribution: Some(SourceAttributionProto {
            extension_id: pkg.extension_id.clone(),
            source_name: rule.rate_source.clone(),
            source_url: rule.tariff_url.clone(),
            provenance_tier: pkg.default_provenance as i32,
            fetched_at_rfc3339: now.to_string(),
            schedule_or_version: Some(rule.rate_schedule.clone()),
            notes: None,
        }),
        latency_ms: 0,
        cached: true,
        payload: Some(Payload::GasUtility(proto)),
    }
}

fn build_water_envelope(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    rule: &WaterSewerTrashRuleYaml,
    spec: i32,
    now: &str,
) -> ExtensionEnvelopeProto {
    let sdwis_name = query
        .sdwis_water_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let sdwis_pwsid = query
        .sdwis_pwsid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| rule.water_pwsid.clone());

    let is_well = query.is_likely_private_well.unwrap_or(false);
    let is_septic = query.is_likely_private_septic.unwrap_or(false);
    let hoa_covers = query.hoa_covers_water_trash.unwrap_or(false);

    let resolved_rule_water_name = if rule.water_carrier_name.contains("{{ city }}") {
        let city_trim = query.city.trim();
        if city_trim.is_empty() {
            rule.fallback_water_carrier_name
                .clone()
                .unwrap_or_else(|| "Municipal Public Water Utility".to_string())
        } else {
            rule.water_carrier_name.replace("{{ city }}", city_trim)
        }
    } else {
        rule.water_carrier_name.clone()
    };

    let (water_carrier_name, water_source_type, water_rate_unit, water_volumetric_rate_usd, water_base_monthly_usd, water_tariff_url) =
        if is_well && sdwis_name.is_none() {
            (
                "Private Domestic Well (Owner-Operated)".to_string(),
                "private_well".to_string(),
                "Unmetered Owner-Operated Well".to_string(),
                None,
                0.0,
                None,
            )
        } else {
            (
                sdwis_name
                    .map(|s| s.to_string())
                    .unwrap_or(resolved_rule_water_name),
                rule.water_source_type.clone(),
                rule.water_rate_unit.clone(),
                rule.water_volumetric_rate_usd.map(round2),
                if hoa_covers {
                    0.0
                } else {
                    round2(rule.water_base_monthly_usd)
                },
                rule.water_tariff_url.clone(),
            )
        };

    let (sewer_carrier_name, sewer_type, sewer_monthly_usd) = if is_septic {
        (
            "Private Onsite Septic System (OWTS)".to_string(),
            "septic".to_string(),
            0,
        )
    } else {
        (
            rule.sewer_carrier_name.clone(),
            rule.sewer_type.clone(),
            if hoa_covers { 0 } else { rule.sewer_monthly_usd },
        )
    };

    let trash_monthly_usd = if hoa_covers { 0 } else { rule.trash_monthly_usd };

    let source_url = water_tariff_url
        .clone()
        .unwrap_or_else(|| {
            let ctx = build_template_context(query);
            resolve_manifest_reference_url(
                pkg,
                &ctx,
                "https://www.epa.gov/ground-water-and-drinking-water",
            )
        });
    let source_name = pkg
        .doc
        .source_attribution
        .as_ref()
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "Published Municipal Water & Sewer Schedule".to_string());

    let proto = WaterSewerTrashProto {
        water_carrier_name,
        water_pwsid: sdwis_pwsid,
        water_source_type,
        water_rate_unit,
        water_volumetric_rate_usd,
        water_base_monthly_usd,
        water_tariff_url,
        sewer_carrier_name,
        sewer_type,
        sewer_monthly_usd,
        trash_carrier_name: rule.trash_carrier_name.clone(),
        trash_monthly_usd,
    };

    ExtensionEnvelopeProto {
        extension_id: pkg.extension_id.clone(),
        domain: CapabilityDomain::DomainWaterSewerTrash as i32,
        priority: manifest.priority,
        geo_specificity: spec,
        attribution: Some(SourceAttributionProto {
            extension_id: pkg.extension_id.clone(),
            source_name,
            source_url,
            provenance_tier: pkg.default_provenance as i32,
            fetched_at_rfc3339: now.to_string(),
            schedule_or_version: None,
            notes: None,
        }),
        latency_ms: 0,
        cached: true,
        payload: Some(Payload::WaterSewerTrash(proto)),
    }
}

fn build_broadband_footprint_envelopes(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    rules: &[BroadbandRuleYaml],
    now: &str,
) -> Vec<ExtensionEnvelopeProto> {
    let is_rural = query.is_rural_area.unwrap_or(false);
    let mut matched_primary: Vec<(i32, &BroadbandRuleYaml)> = Vec::new();
    let mut fallback_rules: Vec<(i32, &BroadbandRuleYaml)> = Vec::new();

    for rule in rules {
        if rule.exclude_rural && is_rural {
            continue;
        }
        if let Some(spec) = rule.geo_scope.match_specificity(query) {
            if rule.only_when_category_empty {
                fallback_rules.push((spec, rule));
            } else {
                matched_primary.push((spec, rule));
            }
        }
    }

    for (spec, fb) in fallback_rules {
        let cat_present = matched_primary
            .iter()
            .any(|(_, r)| r.tier_category.eq_ignore_ascii_case(&fb.tier_category));
        if !cat_present {
            matched_primary.push((spec, fb));
        }
    }

    // Deduplicate within this extension by `(brand_key, tier_category)` keeping highest `spec`.
    let mut dedup: Vec<(String, i32, &BroadbandRuleYaml)> = Vec::new();
    for (spec, rule) in matched_primary {
        let key = format!(
            "{}::{}",
            normalize_carrier_brand_key(&rule.carrier_name),
            rule.tier_category.to_ascii_lowercase()
        );
        if let Some(existing) = dedup.iter_mut().find(|(k, _, _)| *k == key) {
            if spec > existing.1 {
                *existing = (key, spec, rule);
            }
        } else {
            dedup.push((key, spec, rule));
        }
    }

    dedup
        .into_iter()
        .map(|(_, spec, rule)| {
            let status_clean = if rule.availability_status.eq_ignore_ascii_case("nationwide_sky_view") {
                "nationwide_sky_view".to_string()
            } else {
                "regional_footprint_unverified".to_string()
            };
            let opt = BroadbandOptionProto {
                carrier_name: rule.carrier_name.clone(),
                technology: rule.technology.clone(),
                tier_category: rule.tier_category.to_ascii_lowercase(),
                max_download_mbps: rule.max_download_mbps,
                max_upload_mbps: rule.max_upload_mbps,
                symmetrical: rule.symmetrical,
                typical_monthly_usd: String::new(),
                availability_status: status_clean,
                check_url: rule.check_url.clone(),
            };
            ExtensionEnvelopeProto {
                extension_id: pkg.extension_id.clone(),
                domain: CapabilityDomain::DomainBroadband as i32,
                priority: manifest.priority,
                geo_specificity: spec,
                attribution: Some(SourceAttributionProto {
                    extension_id: pkg.extension_id.clone(),
                    source_name: pkg
                        .doc
                        .source_attribution
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| rule.carrier_name.clone()),
                    source_url: rule.check_url.clone(),
                    provenance_tier: pkg.default_provenance as i32,
                    fetched_at_rfc3339: now.to_string(),
                    schedule_or_version: None,
                    notes: None,
                }),
                latency_ms: 0,
                cached: true,
                payload: Some(Payload::BroadbandOption(opt)),
            }
        })
        .collect()
}

struct ResolvedHttpSpec {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

fn resolve_http_spec(
    pkg: &ParsedExtensionPackage,
    query: &ExtensionQueryProto,
    env: &EnvConfig,
) -> Result<ResolvedHttpSpec> {
    let ctx = build_template_context(query);

    let (mut method, mut base_url, mut query_pairs, mut headers, mut body) =
        if pkg.driver == DriverKind::DriverOpenapiYaml {
            let oas = pkg
                .openapi_doc
                .as_ref()
                .ok_or_else(|| anyhow!("Missing OpenAPI document for '{}'", pkg.extension_id))?;
            let binding = pkg.doc.openapi.as_ref();
            let server_base = oas
                .servers
                .first()
                .map(|s| s.url.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "OpenAPI spec for '{}' has no server URL",
                        pkg.extension_id
                    )
                })?;

            let (http_method, raw_path, op) = find_openapi_operation(oas, binding)?;
            let mut resolved_path = raw_path.to_string();
            let mut q_map: BTreeMap<String, String> = BTreeMap::new();
            let mut h_map: BTreeMap<String, String> = BTreeMap::new();

            // Populate default values declared in OpenAPI parameter schemas first
            for param_ref in &op.parameters {
                if let ReferenceOr::Item(param) = param_ref {
                    match param {
                        Parameter::Query { parameter_data, .. } => {
                            if let Some(def_val) = extract_oas_param_default(parameter_data) {
                                q_map.insert(parameter_data.name.clone(), def_val);
                            }
                        }
                        Parameter::Header { parameter_data, .. } => {
                            if let Some(def_val) = extract_oas_param_default(parameter_data) {
                                h_map.insert(parameter_data.name.clone(), def_val);
                            }
                        }
                        _ => {}
                    }
                }
            }

            if let Some(b) = binding {
                for (k, tpl) in &b.query_params {
                    let val = render_query_template(tpl, &ctx)?;
                    let placeholder = format!("{{{k}}}");
                    if resolved_path.contains(&placeholder) {
                        let enc: String =
                            url::form_urlencoded::byte_serialize(val.as_bytes()).collect();
                        resolved_path = resolved_path.replace(&placeholder, &enc);
                    } else if !val.is_empty() {
                        q_map.insert(k.clone(), val);
                    }
                }
                for (k, tpl) in &b.headers {
                    let val = render_query_template(tpl, &ctx)?;
                    if !val.is_empty() {
                        h_map.insert(k.clone(), val);
                    }
                }
            }

            let rendered_body = if let Some(bt) =
                binding.and_then(|b| b.body_template.as_deref()).map(str::trim).filter(|s| !s.is_empty())
            {
                Some(render_query_template(bt, &ctx)?)
            } else if matches!(http_method.as_str(), "POST" | "PUT" | "PATCH") {
                synthesize_openapi_request_body(op, query, &ctx)?
            } else {
                None
            };

            let joined_url = if resolved_path.starts_with("http://")
                || resolved_path.starts_with("https://")
            {
                resolved_path
            } else if resolved_path.starts_with('/') {
                format!("{server_base}{resolved_path}")
            } else {
                format!("{server_base}/{resolved_path}")
            };

            (http_method, joined_url, q_map, h_map, rendered_body)
        } else if let Some(ref req) = pkg.doc.request {
            let rendered_url = render_query_template(&req.url, &ctx)?;
            let mut q_map = BTreeMap::new();
            for (k, tpl) in &req.query_params {
                let val = render_query_template(tpl, &ctx)?;
                if !val.is_empty() {
                    q_map.insert(k.clone(), val);
                }
            }
            let mut h_map = BTreeMap::new();
            for (k, tpl) in &req.headers {
                let val = render_query_template(tpl, &ctx)?;
                if !val.is_empty() {
                    h_map.insert(k.clone(), val);
                }
            }
            let rendered_body = match req.body_template.as_deref() {
                Some(bt) if !bt.trim().is_empty() => Some(render_query_template(bt, &ctx)?),
                _ => None,
            };
            (
                req.method.trim().to_ascii_uppercase(),
                rendered_url,
                q_map,
                h_map,
                rendered_body,
            )
        } else {
            return Err(anyhow!(
                "Extension '{}' has no live HTTP operation configured",
                pkg.extension_id
            ));
        };

    // Resolve credential binding from environment variables (never hardcoded secrets)
    let auth_type = pkg.doc.auth.auth_type.trim().to_ascii_lowercase();
    if auth_type != "none" && !auth_type.is_empty() {
        let env_var = pkg
            .doc
            .auth
            .env_var
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "Extension '{}' auth_type '{}' requires auth.env_var",
                    pkg.extension_id,
                    auth_type
                )
            })?;
        let secret = env.get(env_var).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| {
            anyhow!(
                "AUTH_MISSING: environment variable '{}' is not set for extension '{}'",
                env_var,
                pkg.extension_id
            )
        })?;

        match auth_type.as_str() {
            "api_key_header" => {
                let hdr = pkg
                    .doc
                    .auth
                    .header_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("X-Api-Key");
                let val = match pkg.doc.auth.header_prefix.as_deref() {
                    Some(pfx) if !pfx.is_empty() => format!("{pfx}{secret}"),
                    _ => secret.to_string(),
                };
                headers.insert(hdr.to_string(), val);
            }
            "bearer_token" | "bearer" => {
                let hdr = pkg
                    .doc
                    .auth
                    .header_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Authorization");
                headers.insert(hdr.to_string(), format!("Bearer {secret}"));
            }
            "api_key_query" => {
                let qp = pkg
                    .doc
                    .auth
                    .query_param
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("api_key");
                query_pairs.insert(qp.to_string(), secret.to_string());
            }
            other => {
                return Err(anyhow!(
                    "Unsupported auth_type '{other}' in extension '{}'",
                    pkg.extension_id
                ));
            }
        }
    }

    if !headers.keys().any(|k| k.eq_ignore_ascii_case("accept")) {
        let accept_val = match pkg
            .doc
            .response_mapping
            .as_ref()
            .map(|rm| rm.format)
            .unwrap_or(GovResponseFormat::Auto)
        {
            GovResponseFormat::Geojson => "application/geo+json, application/json;q=0.9",
            GovResponseFormat::Csv => "text/csv, text/plain;q=0.9, */*;q=0.8",
            GovResponseFormat::Xml => "application/xml, text/xml;q=0.9, */*;q=0.8",
            GovResponseFormat::Html => "text/html, application/xhtml+xml;q=0.9, */*;q=0.8",
            _ => "application/json",
        };
        headers.insert("Accept".to_string(), accept_val.to_string());
    }
    if body.is_some() && !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
        headers.insert("Content-Type".to_string(), "application/json".to_string());
    }

    let mut parsed_url = Url::parse(&base_url)
        .with_context(|| format!("Invalid resolved URL '{base_url}' for '{}'", pkg.extension_id))?;
    if !query_pairs.is_empty() {
        let mut qp = parsed_url.query_pairs_mut();
        for (k, v) in &query_pairs {
            qp.append_pair(k, v);
        }
    }
    base_url = parsed_url.to_string();
    if method.is_empty() {
        method = "GET".to_string();
    }
    if method == "GET" {
        body = None;
    }

    Ok(ResolvedHttpSpec {
        method,
        url: base_url,
        headers: headers.into_iter().collect(),
        body,
    })
}

fn find_openapi_operation<'a>(
    oas: &'a openapiv3::OpenAPI,
    binding: Option<&crate::extensions::manifest::OpenApiBindingYaml>,
) -> Result<(String, &'a str, &'a Operation)> {
    let target_op = binding
        .and_then(|b| b.operation_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let target_path = binding
        .and_then(|b| b.path.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let target_method = binding
        .and_then(|b| b.method.as_deref())
        .map(|m| m.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty());

    for (path, item_ref) in &oas.paths.paths {
        let ReferenceOr::Item(item) = item_ref else {
            continue;
        };
        if let Some(tp) = target_path {
            if path != tp {
                continue;
            }
        }
        for (m_name, op_opt) in path_item_operations(item) {
            let Some(op) = op_opt else {
                continue;
            };
            if let Some(ref tm) = target_method {
                if m_name != tm {
                    continue;
                }
            }
            if let Some(top) = target_op {
                if op.operation_id.as_deref() == Some(top) {
                    return Ok((m_name.to_string(), path.as_str(), op));
                }
            } else {
                return Ok((m_name.to_string(), path.as_str(), op));
            }
        }
    }
    Err(anyhow!("Matching OpenAPI operation not found in openapi.yaml"))
}

fn synthesize_openapi_request_body(
    op: &Operation,
    query: &ExtensionQueryProto,
    ctx: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    if let Some(raw_json) = query
        .extra_params
        .get("body_json")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(Some(raw_json.to_string()));
    }
    let Some(ReferenceOr::Item(rb)) = op.request_body.as_ref() else {
        return Ok(None);
    };
    let Some(media_type) = rb
        .content
        .get("application/json")
        .or_else(|| rb.content.values().next())
    else {
        return Ok(None);
    };
    let Some(ReferenceOr::Item(schema)) = media_type.schema.as_ref() else {
        return Ok(None);
    };
    let SchemaKind::Type(Type::Object(obj)) = &schema.schema_kind else {
        return Ok(None);
    };

    let lat = query.lat.filter(|v| v.is_finite()).unwrap_or(37.3382);
    let lon = query.lon.filter(|v| v.is_finite()).unwrap_or(-121.8863);
    let dest_lat = (lat + 0.01).clamp(-89.9, 89.9);
    let dest_lon = (lon + 0.01).clamp(-179.9, 179.9);

    let mut body_obj = Map::new();
    for (prop_name, prop_ref) in &obj.properties {
        let is_required = obj.required.iter().any(|r| r == prop_name);
        if let Some(raw) = ctx.get(prop_name).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                body_obj.insert(prop_name.clone(), parsed);
            } else {
                body_obj.insert(prop_name.clone(), Value::String(raw.to_string()));
            }
            continue;
        }
        if let ReferenceOr::Item(boxed_schema) = prop_ref {
            if let Some(def_val) = boxed_schema.schema_data.default.as_ref() {
                body_obj.insert(prop_name.clone(), def_val.clone());
                continue;
            }
        }
        match prop_name.as_str() {
            "locations" | "coordinates" => {
                body_obj.insert(
                    prop_name.clone(),
                    json!([[lon, lat], [dest_lon, dest_lat]]),
                );
            }
            "sources" => {
                body_obj.insert(prop_name.clone(), json!([0]));
            }
            "destinations" => {
                body_obj.insert(prop_name.clone(), json!([1]));
            }
            "metrics" => {
                body_obj.insert(prop_name.clone(), json!(["duration", "distance"]));
            }
            _ if is_required => {
                body_obj.insert(prop_name.clone(), json!(null));
            }
            _ => {}
        }
    }

    if body_obj.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(&Value::Object(body_obj))?))
    }
}

fn path_item_operations(item: &PathItem) -> [(&'static str, Option<&Operation>); 5] {
    [
        ("GET", item.get.as_ref()),
        ("POST", item.post.as_ref()),
        ("PUT", item.put.as_ref()),
        ("DELETE", item.delete.as_ref()),
        ("PATCH", item.patch.as_ref()),
    ]
}

fn extract_oas_param_default(param_data: &ParameterData) -> Option<String> {
    let ParameterSchemaOrContent::Schema(ReferenceOr::Item(schema)) = &param_data.format else {
        return None;
    };
    if let Some(ref def_val) = schema.schema_data.default {
        return json_scalar_to_string(def_val);
    }
    if let SchemaKind::Type(Type::String(ref st)) = schema.schema_kind {
        if st.enumeration.len() == 1 {
            if let Some(Some(only)) = st.enumeration.first() {
                return Some(only.clone());
            }
        }
    }
    None
}

/// Executes a live OpenAPI 3.x or declarative HTTP extension, applying SSRF validation,
/// `governor` rate limiting, `moka` caching, unified `HttpClient::scrape` egress, multi-format
/// government portal normalization (`json`, `geojson`, `arcgis_features`, `socrata_rows`, `csv`,
/// `xml`, `html`), and RFC 9535 JSONPath extraction into typed Protobuf envelopes.
pub async fn execute_live_http(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    http: &HttpClient,
    env: &EnvConfig,
    cache: &Cache<String, CachedEnvelopeEntry>,
    limiter: &DefaultDirectRateLimiter,
) -> Result<Vec<ExtensionEnvelopeProto>> {
    let Some(spec_score) = pkg.compiled_geo_scope.match_query(query) else {
        return Ok(Vec::new());
    };

    let resolved = resolve_http_spec(pkg, query, env)?;
    let cache_key = format!(
        "{}|{}|{}|{}",
        pkg.extension_id,
        resolved.method,
        resolved.url,
        resolved.body.as_deref().unwrap_or("")
    );

    let force_shared_refresh = query
        .extra_params
        .get(crate::extensions::shared_cache::FORCE_REFRESH_PARAM)
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    let bypass_url_cache = force_shared_refresh
        || crate::extensions::shared_cache::sharing_spec_from_manifest(manifest)
            .map(|s| s.is_active())
            .unwrap_or(false);

    if !bypass_url_cache && pkg.doc.cache_ttl_secs > 0 {
        if let Some(hit) = cache.get(&cache_key).await {
            if hit.fetched_at.elapsed().as_secs() < pkg.doc.cache_ttl_secs {
                let mut out = hit.envelopes;
                for env_item in &mut out {
                    env_item.cached = true;
                    env_item.priority = manifest.priority;
                }
                return Ok(out);
            }
        }
    }

    let allow_loopback = env.get("ALLOW_EXTENSION_TEST_LOOPBACK") == Some("1");
    validate_ssrf_safe_public_url(&resolved.url, allow_loopback)
        .await
        .map_err(|e| anyhow!("SSRF guard blocked extension '{}' URL: {e}", pkg.extension_id))?;

    // Enforce this extension's declared `manifest.rate_limit` GCRA quota.
    limiter.until_ready().await;

    let rl = manifest.rate_limit.clone().unwrap_or_default();
    let timeout_dur = Duration::from_millis(rl.timeout_ms.clamp(400, 20_000) as u64);
    let retries = rl.max_retries.min(3) as usize;

    let started = Instant::now();
    let method = reqwest::Method::from_bytes(resolved.method.as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut scrape_req = http
        .scrape(&resolved.url)
        .method(method)
        .timeout(timeout_dur)
        .retries(retries)
        .max_bytes(16 * 1024 * 1024);

    for (hk, hv) in &resolved.headers {
        scrape_req = scrape_req.header(hk, hv);
    }
    if let Some(ref b) = resolved.body {
        let explicit_ct = resolved
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        let ct = explicit_ct.unwrap_or_else(|| "application/json".to_string());
        scrape_req = scrape_req.body_bytes(b.clone().into_bytes(), Some(ct));
    }

    let buffered = match scrape_req.send_buffered().await {
        Ok(resp) => resp,
        Err(e) => {
            http.record_external_http_response(&resolved.url, 0);
            return Err(anyhow!(
                "HTTP transport error in '{}': {e}",
                pkg.extension_id
            ));
        }
    };

    let body_bytes = buffered.bytes();
    http.record_external_http_response(&resolved.url, body_bytes.len() as u64);
    let content_type = buffered
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let json_body = parse_gov_response_payload(
        &pkg.extension_id,
        body_bytes,
        content_type.as_deref(),
        &resolved.url,
        pkg.doc.response_mapping.as_ref(),
    )?;

    let latency_ms = started.elapsed().as_millis() as u32;
    let envelopes = map_json_to_envelopes(
        pkg,
        manifest,
        query,
        &json_body,
        spec_score,
        latency_ms,
    )?;

    if !bypass_url_cache && pkg.doc.cache_ttl_secs > 0 && !envelopes.is_empty() {
        cache
            .insert(
                cache_key,
                CachedEnvelopeEntry {
                    fetched_at: Instant::now(),
                    envelopes: envelopes.clone(),
                },
            )
            .await;
    }

    Ok(envelopes)
}

fn snake_header_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !out.is_empty() && !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn insert_header_cell(row_obj: &mut Map<String, Value>, header: &str, cell_val: Value) {
    let h_trim = header.trim();
    if h_trim.is_empty() {
        return;
    }
    let snake = snake_header_key(h_trim);
    row_obj.insert(h_trim.to_string(), cell_val.clone());
    if !snake.is_empty() && snake != h_trim && !row_obj.contains_key(&snake) {
        row_obj.insert(snake, cell_val);
    }
}

fn extract_geojson_geometry_lon_lat(geom: &Value) -> Option<(f64, f64)> {
    let gtype = geom.get("type").and_then(Value::as_str).unwrap_or("");
    let coords = geom.get("coordinates")?;
    match gtype {
        "Point" => {
            let lon = coords.get(0).and_then(Value::as_f64)?;
            let lat = coords.get(1).and_then(Value::as_f64)?;
            (lon.is_finite()
                && lat.is_finite()
                && (-180.0..=180.0).contains(&lon)
                && (-90.0..=90.0).contains(&lat))
            .then_some((lon, lat))
        }
        "MultiPoint" | "LineString" => {
            let pt = coords.get(0)?;
            let lon = pt.get(0).and_then(Value::as_f64)?;
            let lat = pt.get(1).and_then(Value::as_f64)?;
            (lon.is_finite()
                && lat.is_finite()
                && (-180.0..=180.0).contains(&lon)
                && (-90.0..=90.0).contains(&lat))
            .then_some((lon, lat))
        }
        "Polygon" | "MultiPolygon" => {
            let ring = if gtype == "Polygon" {
                coords.get(0).and_then(Value::as_array)?
            } else {
                coords
                    .get(0)
                    .and_then(|p| p.get(0))
                    .and_then(Value::as_array)?
            };
            let mut sum_lon = 0.0_f64;
            let mut sum_lat = 0.0_f64;
            let mut cnt = 0_usize;
            for pt in ring {
                if let (Some(lo), Some(la)) = (
                    pt.get(0).and_then(Value::as_f64),
                    pt.get(1).and_then(Value::as_f64),
                ) {
                    if lo.is_finite()
                        && la.is_finite()
                        && (-180.0..=180.0).contains(&lo)
                        && (-90.0..=90.0).contains(&la)
                    {
                        sum_lon += lo;
                        sum_lat += la;
                        cnt += 1;
                    }
                }
            }
            (cnt > 0).then(|| (sum_lon / cnt as f64, sum_lat / cnt as f64))
        }
        _ => None,
    }
}

fn flatten_geojson_feature(feat: &Value) -> Option<Value> {
    let obj = feat.as_object()?;
    let mut rec = obj
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(id_v) = obj.get("id").filter(|v| !v.is_null()) {
        rec.entry("_id".to_string()).or_insert_with(|| id_v.clone());
    }
    if let Some(geom) = obj.get("geometry").filter(|v| !v.is_null()) {
        if let Some(gt) = geom.get("type").and_then(Value::as_str) {
            rec.entry("_geometry_type".to_string())
                .or_insert_with(|| Value::String(gt.to_string()));
        }
        if let Some((lon, lat)) = extract_geojson_geometry_lon_lat(geom) {
            if let (Some(lo_n), Some(la_n)) = (
                serde_json::Number::from_f64(lon),
                serde_json::Number::from_f64(lat),
            ) {
                rec.entry("lon".to_string())
                    .or_insert(Value::Number(lo_n.clone()));
                rec.entry("lat".to_string())
                    .or_insert(Value::Number(la_n.clone()));
                rec.insert("_lon".to_string(), Value::Number(lo_n));
                rec.insert("_lat".to_string(), Value::Number(la_n));
            }
        }
    }
    // Clone snake_case aliases for UPPERCASE GIS property keys (e.g. `FLD_ZONE` -> `fld_zone`)
    let keys: Vec<(String, Value)> = rec.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in keys {
        let snake = snake_header_key(&k);
        if !snake.is_empty() && snake != k && !rec.contains_key(&snake) {
            rec.insert(snake, v);
        }
    }
    Some(Value::Object(rec))
}

fn normalize_geojson_value(mut val: Value) -> Result<Value> {
    if let Some(obj) = val.as_object_mut() {
        if obj.contains_key("records") {
            return Ok(val);
        }
        let gtype = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if gtype == "FeatureCollection" {
            let recs: Vec<Value> = obj
                .get("features")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(flatten_geojson_feature).collect())
                .unwrap_or_default();
            let count = recs.len();
            obj.insert("records".to_string(), Value::Array(recs));
            obj.entry("count".to_string()).or_insert_with(|| json!(count));
        } else if gtype == "Feature" {
            let recs = flatten_geojson_feature(&Value::Object(obj.clone()))
                .into_iter()
                .collect::<Vec<_>>();
            let count = recs.len();
            obj.insert("records".to_string(), Value::Array(recs));
            obj.entry("count".to_string()).or_insert_with(|| json!(count));
        }
    }
    Ok(val)
}

fn flatten_arcgis_feature(feat: &Value) -> Option<Value> {
    let obj = feat.as_object()?;
    let mut rec = obj
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(geom) = obj.get("geometry").and_then(Value::as_object) {
        if let (Some(x), Some(y)) = (
            geom.get("x").and_then(Value::as_f64),
            geom.get("y").and_then(Value::as_f64),
        ) {
            if let (Some(xn), Some(yn)) = (
                serde_json::Number::from_f64(x),
                serde_json::Number::from_f64(y),
            ) {
                rec.insert("_x".to_string(), Value::Number(xn.clone()));
                rec.insert("_y".to_string(), Value::Number(yn.clone()));
                if (-180.0..=180.0).contains(&x) && (-90.0..=90.0).contains(&y) {
                    rec.entry("lon".to_string()).or_insert(Value::Number(xn));
                    rec.entry("lat".to_string()).or_insert(Value::Number(yn));
                }
            }
        } else if let Some(ring) = geom
            .get("rings")
            .and_then(|r| r.get(0))
            .and_then(Value::as_array)
        {
            let mut sx = 0.0_f64;
            let mut sy = 0.0_f64;
            let mut cnt = 0_usize;
            for pt in ring {
                if let (Some(x), Some(y)) = (
                    pt.get(0).and_then(Value::as_f64),
                    pt.get(1).and_then(Value::as_f64),
                ) {
                    if x.is_finite() && y.is_finite() {
                        sx += x;
                        sy += y;
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                let cx = sx / cnt as f64;
                let cy = sy / cnt as f64;
                if (-180.0..=180.0).contains(&cx) && (-90.0..=90.0).contains(&cy) {
                    if let (Some(xn), Some(yn)) = (
                        serde_json::Number::from_f64(cx),
                        serde_json::Number::from_f64(cy),
                    ) {
                        rec.entry("lon".to_string()).or_insert(Value::Number(xn));
                        rec.entry("lat".to_string()).or_insert(Value::Number(yn));
                    }
                }
            }
        }
    }
    let keys: Vec<(String, Value)> = rec.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in keys {
        let snake = snake_header_key(&k);
        if !snake.is_empty() && snake != k && !rec.contains_key(&snake) {
            rec.insert(snake, v);
        }
    }
    Some(Value::Object(rec))
}

fn normalize_arcgis_value(ext_id: &str, mut val: Value) -> Result<Value> {
    if let Some(obj) = val.as_object_mut() {
        if let Some(err_obj) = obj.get("error").and_then(Value::as_object) {
            if !obj.contains_key("features") && !obj.contains_key("results") {
                let code = err_obj.get("code").and_then(Value::as_i64).unwrap_or(500);
                let msg = err_obj
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("ArcGIS service error");
                return Err(anyhow!("ArcGIS error {code} from '{ext_id}': {msg}"));
            }
        }
        if !obj.contains_key("records") {
            let source_arr = obj
                .get("features")
                .and_then(Value::as_array)
                .or_else(|| obj.get("results").and_then(Value::as_array));
            if let Some(arr) = source_arr {
                let recs: Vec<Value> = arr.iter().filter_map(flatten_arcgis_feature).collect();
                let count = recs.len();
                obj.insert("records".to_string(), Value::Array(recs));
                obj.entry("count".to_string()).or_insert_with(|| json!(count));
            }
        }
    }
    Ok(val)
}

fn normalize_socrata_ckan_value(val: Value) -> Result<Value> {
    match val {
        Value::Array(rows) => {
            let count = rows.len();
            Ok(json!({
                "records": rows,
                "count": count,
            }))
        }
        Value::Object(mut obj) => {
            if obj.contains_key("records") {
                return Ok(Value::Object(obj));
            }
            // Socrata SODA tabular `{ "meta": { "view": { "columns": [...] } }, "data": [ [...] ] }`
            let cols_opt = obj
                .get("meta")
                .and_then(|m| m.get("view"))
                .and_then(|v| v.get("columns"))
                .and_then(Value::as_array);
            if let (Some(cols), Some(data_rows)) = (cols_opt, obj.get("data").and_then(Value::as_array))
            {
                if data_rows.first().map(Value::is_array).unwrap_or(true) {
                    let col_names: Vec<String> = cols
                        .iter()
                        .map(|c| {
                            c.get("fieldName")
                                .or_else(|| c.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim()
                                .to_string()
                        })
                        .collect();
                    let mut recs = Vec::with_capacity(data_rows.len());
                    for r in data_rows {
                        if let Some(cells) = r.as_array() {
                            let mut map = Map::new();
                            for (idx, cell) in cells.iter().enumerate() {
                                if let Some(cname) = col_names.get(idx).filter(|s| !s.is_empty()) {
                                    if !cname.starts_with(':') {
                                        insert_header_cell(&mut map, cname, cell.clone());
                                    } else {
                                        map.insert(cname.clone(), cell.clone());
                                    }
                                }
                            }
                            recs.push(Value::Object(map));
                        }
                    }
                    let count = recs.len();
                    obj.insert("records".to_string(), Value::Array(recs));
                    obj.entry("count".to_string()).or_insert_with(|| json!(count));
                    return Ok(Value::Object(obj));
                }
            }
            // CKAN Datastore `{ "result": { "records": [...] } }`
            if let Some(ckan_recs) = obj
                .get("result")
                .and_then(|r| r.get("records"))
                .and_then(Value::as_array)
                .cloned()
            {
                let count = ckan_recs.len();
                obj.insert("records".to_string(), Value::Array(ckan_recs));
                obj.entry("count".to_string()).or_insert_with(|| json!(count));
                return Ok(Value::Object(obj));
            }
            // SODA v3 `{ "data": [ {...}, ... ] }`
            if let Some(data_objs) = obj.get("data").and_then(Value::as_array).cloned() {
                let count = data_objs.len();
                obj.insert("records".to_string(), Value::Array(data_objs));
                obj.entry("count".to_string()).or_insert_with(|| json!(count));
            }
            Ok(Value::Object(obj))
        }
        other => Ok(other),
    }
}

fn detect_csv_delimiter(header_line: &str, explicit: Option<&str>) -> u8 {
    if let Some(d) = explicit.and_then(|s| s.as_bytes().first().copied()) {
        return d;
    }
    let tabs = header_line.bytes().filter(|&b| b == b'\t').count();
    let pipes = header_line.bytes().filter(|&b| b == b'|').count();
    let semis = header_line.bytes().filter(|&b| b == b';').count();
    let commas = header_line.bytes().filter(|&b| b == b',').count();
    if tabs > commas && tabs >= pipes && tabs >= semis {
        b'\t'
    } else if pipes > commas && pipes >= semis {
        b'|'
    } else if semis > commas {
        b';'
    } else {
        b','
    }
}

fn parse_csv_portal_to_json(
    ext_id: &str,
    bytes: &[u8],
    explicit_delim: Option<&str>,
) -> Result<Value> {
    let raw_txt = std::str::from_utf8(bytes)
        .map(Cow::Borrowed)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes));
    let clean_txt = raw_txt.trim_start_matches('\u{feff}').trim();
    if clean_txt.is_empty() {
        return Ok(json!({ "records": [], "headers": [], "count": 0 }));
    }
    let head_low: String = clean_txt.chars().take(32).collect::<String>().to_ascii_lowercase();
    if head_low.starts_with("<!doctype html") || head_low.starts_with("<html") {
        return Err(anyhow!(
            "CSV portal '{}' returned an unexpected HTML page instead of tabular CSV",
            ext_id
        ));
    }

    let first_line = clean_txt.lines().next().unwrap_or("");
    let delim = detect_csv_delimiter(first_line, explicit_delim);
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(clean_txt.as_bytes());

    let headers: Vec<String> = rdr
        .headers()
        .with_context(|| format!("Failed reading CSV headers from '{ext_id}'"))?
        .iter()
        .map(|h| h.trim().to_string())
        .collect();

    let mut records = Vec::new();
    for rec_res in rdr.records().take(2_000) {
        let rec = rec_res.with_context(|| format!("Invalid CSV row from '{ext_id}'"))?;
        let mut row_map = Map::new();
        let mut any_non_empty = false;
        for (idx, val) in rec.iter().enumerate() {
            let Some(h) = headers.get(idx).filter(|s| !s.is_empty()) else {
                continue;
            };
            let t = val.trim();
            let cell = if t.is_empty() {
                Value::Null
            } else {
                any_non_empty = true;
                Value::String(t.to_string())
            };
            insert_header_cell(&mut row_map, h, cell);
        }
        if any_non_empty {
            records.push(Value::Object(row_map));
        }
    }

    let count = records.len();
    Ok(json!({
        "records": records,
        "headers": headers,
        "count": count,
    }))
}

static XML_ATTR_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r#"([A-Za-z_][A-Za-z0-9_.:-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap()
});
static SCRIPT_STYLE_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?is)<(?:script|style|noscript)\b[^>]*>.*?</(?:script|style|noscript)>")
        .unwrap()
});
static HTML_COMMENT_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?s)<!--.*?-->").unwrap());
static HTML_TAG_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?s)<[^>]+>").unwrap());
static HTML_TABLE_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?is)<table\b[^>]*>(.*?)</table>").unwrap());
static HTML_TR_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>").unwrap());
static HTML_CELL_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?is)<(th|td)\b[^>]*>(.*?)</(?:th|td)>").unwrap());
static HTML_HREF_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*(?:"([^"]+)"|'([^']+)')"#).unwrap());
static HTML_DL_PAIR_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?is)<dt\b[^>]*>(.*?)</dt>\s*<dd\b[^>]*>(.*?)</dd>").unwrap()
});
static HTML_META_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(
        r#"(?is)<meta\b[^>]*\b(?:name|property)\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*\bcontent\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
    )
    .unwrap()
});
static HTML_DATA_ATTR_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r#"\bdata-([a-zA-Z0-9_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap()
});

fn local_xml_name(raw: &str) -> &str {
    raw.rsplit(':').next().unwrap_or(raw)
}

fn clean_html_text(fragment: &str) -> String {
    let stripped = HTML_TAG_RE.replace_all(fragment, " ");
    let decoded = html_escape::decode_html_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_xml_portal_to_json(
    ext_id: &str,
    bytes: &[u8],
    explicit_record_tag: Option<&str>,
) -> Result<Value> {
    let raw_txt = std::str::from_utf8(bytes)
        .map(Cow::Borrowed)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes));
    let clean = HTML_COMMENT_RE.replace_all(raw_txt.trim_start_matches('\u{feff}'), "");
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return Ok(json!({ "records": [], "count": 0 }));
    }
    let low_head: String = trimmed.chars().take(240).collect::<String>().to_ascii_lowercase();
    if low_head.contains("serviceexceptionreport") || low_head.contains("<exceptionreport") {
        let msg = clean_html_text(trimmed);
        return Err(anyhow!("OGC/XML exception from '{ext_id}': {msg}"));
    }

    #[derive(Default)]
    struct XmlFrame {
        tag: String,
        attrs: Map<String, Value>,
        children: BTreeMap<String, Vec<Value>>,
        text: String,
    }

    impl XmlFrame {
        fn into_value(mut self) -> Value {
            let text_trimmed = self.text.trim().to_string();
            if self.attrs.is_empty() && self.children.is_empty() {
                return if text_trimmed.is_empty() {
                    Value::Null
                } else {
                    Value::String(text_trimmed)
                };
            }
            let mut obj = Map::new();
            for (ak, av) in self.attrs {
                obj.insert(format!("@{ak}"), av.clone());
                insert_header_cell(&mut obj, &ak, av);
            }
            for (ck, mut cvals) in std::mem::take(&mut self.children) {
                if cvals.len() == 1 {
                    let single = cvals.pop().unwrap_or(Value::Null);
                    // Flatten `<Property name="FOO" value="BAR"/>` or `<Field name="FOO">BAR</Field>`
                    if matches!(ck.to_ascii_lowercase().as_str(), "property" | "field" | "attr") {
                        if let Some(sobj) = single.as_object() {
                            if let (Some(k), Some(v)) = (
                                sobj.get("name").and_then(Value::as_str),
                                sobj.get("value").or_else(|| sobj.get("#text")),
                            ) {
                                insert_header_cell(&mut obj, k, v.clone());
                            }
                        }
                    }
                    insert_header_cell(&mut obj, &ck, single);
                } else {
                    for item in &cvals {
                        if matches!(ck.to_ascii_lowercase().as_str(), "property" | "field" | "attr")
                        {
                            if let Some(sobj) = item.as_object() {
                                if let (Some(k), Some(v)) = (
                                    sobj.get("name").and_then(Value::as_str),
                                    sobj.get("value").or_else(|| sobj.get("#text")),
                                ) {
                                    insert_header_cell(&mut obj, k, v.clone());
                                }
                            }
                        }
                    }
                    obj.insert(ck.clone(), Value::Array(cvals));
                }
            }
            if !text_trimmed.is_empty() {
                obj.insert("#text".to_string(), Value::String(text_trimmed));
            }
            Value::Object(obj)
        }
    }

    let wanted_tag = explicit_record_tag
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| local_xml_name(s).to_ascii_lowercase());
    let default_record_tags = [
        "featuremember",
        "member",
        "record",
        "row",
        "item",
        "fields",
        "permit",
        "parcel",
        "assessment",
        "violation",
        "station",
        "observation",
    ];

    let mut stack: Vec<XmlFrame> = vec![XmlFrame {
        tag: "__root__".to_string(),
        ..Default::default()
    }];
    let mut records: Vec<Value> = Vec::new();
    let mut rest = trimmed;

    while let Some(lt) = rest.find('<') {
        let before = &rest[..lt];
        if !before.trim().is_empty() {
            if let Some(top) = stack.last_mut() {
                let dec = html_escape::decode_html_entities(before);
                if !top.text.is_empty() {
                    top.text.push(' ');
                }
                top.text.push_str(dec.trim());
            }
        }
        let after_lt = &rest[lt + 1..];
        if let Some(cdata_body) = after_lt.strip_prefix("![CDATA[") {
            if let Some(end_cdata) = cdata_body.find("]]>") {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&cdata_body[..end_cdata]);
                }
                rest = &cdata_body[end_cdata + 3..];
                continue;
            } else {
                break;
            }
        }
        let Some(gt) = after_lt.find('>') else {
            break;
        };
        let inside = after_lt[..gt].trim();
        rest = &after_lt[gt + 1..];

        if inside.is_empty() || inside.starts_with('?') || inside.starts_with('!') {
            continue;
        }
        if let Some(close_raw) = inside.strip_prefix('/') {
            let close_local = local_xml_name(close_raw.trim());
            if stack.len() > 1 {
                let frame = stack.pop().unwrap();
                let tag_low = frame.tag.to_ascii_lowercase();
                let is_tag_rec = |t: &str| -> bool {
                    let tl = t.to_ascii_lowercase();
                    match wanted_tag.as_deref() {
                        Some(wt) => tl == wt,
                        None => default_record_tags.contains(&tl.as_str()),
                    }
                };
                let ancestor_is_rec = stack.iter().skip(1).any(|anc| is_tag_rec(&anc.tag));
                let unwrapped_member = if matches!(tag_low.as_str(), "featuremember" | "member")
                    && frame.attrs.is_empty()
                    && frame.text.trim().is_empty()
                    && frame.children.len() == 1
                {
                    frame
                        .children
                        .values()
                        .next()
                        .and_then(|v| (v.len() == 1 && v[0].is_object()).then(|| v[0].clone()))
                } else {
                    None
                };
                let val = frame.into_value();
                if is_tag_rec(&tag_low) && !ancestor_is_rec {
                    let flat = unwrapped_member.unwrap_or_else(|| val.clone());
                    if flat.is_object() {
                        records.push(flat);
                    }
                }
                if let Some(parent) = stack.last_mut() {
                    parent
                        .children
                        .entry(close_local.to_string())
                        .or_default()
                        .push(val);
                }
            }
            continue;
        }

        let self_closing = inside.ends_with('/');
        let tag_body = if self_closing {
            inside[..inside.len() - 1].trim()
        } else {
            inside
        };
        let mut parts = tag_body.splitn(2, char::is_whitespace);
        let raw_name = parts.next().unwrap_or("");
        let attr_str = parts.next().unwrap_or("");
        let tag_local = local_xml_name(raw_name).to_string();
        if tag_local.is_empty() {
            continue;
        }

        let mut attrs = Map::new();
        for cap in XML_ATTR_RE.captures_iter(attr_str) {
            let ak = local_xml_name(&cap[1]);
            if ak.eq_ignore_ascii_case("xmlns") || cap[1].starts_with("xmlns:") {
                continue;
            }
            let av_raw = cap
                .get(2)
                .or_else(|| cap.get(3))
                .map(|m| m.as_str())
                .unwrap_or("");
            let av = html_escape::decode_html_entities(av_raw).trim().to_string();
            attrs.insert(ak.to_string(), Value::String(av));
        }

        let frame = XmlFrame {
            tag: tag_local.clone(),
            attrs,
            children: BTreeMap::new(),
            text: String::new(),
        };

        if self_closing {
            let tag_low = tag_local.to_ascii_lowercase();
            let val = frame.into_value();
            let is_rec = match wanted_tag.as_deref() {
                Some(wt) => tag_low == wt,
                None => default_record_tags.contains(&tag_low.as_str()),
            };
            if is_rec && val.is_object() {
                records.push(val.clone());
            }
            if let Some(parent) = stack.last_mut() {
                parent.children.entry(tag_local).or_default().push(val);
            }
        } else {
            stack.push(frame);
        }
    }

    while stack.len() > 1 {
        let frame = stack.pop().unwrap();
        let cname = frame.tag.clone();
        let val = frame.into_value();
        if let Some(parent) = stack.last_mut() {
            parent.children.entry(cname).or_default().push(val);
        }
    }

    let root_val = stack.pop().map(XmlFrame::into_value).unwrap_or_else(|| json!({}));
    let mut out_obj = match root_val {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };

    // Fallback: if no canonical record tag matched and root contains a single root element with an
    // array or object, promote it into `records` for zero-boilerplate mapping.
    if records.is_empty() {
        if let Some(first_root) = out_obj.values().next().cloned() {
            if let Some(ro) = first_root.as_object() {
                if let Some(arr) = ro.values().find_map(Value::as_array) {
                    for item in arr {
                        if item.is_object() {
                            records.push(item.clone());
                        }
                    }
                } else {
                    records.push(Value::Object(ro.clone()));
                }
            }
        }
    }

    let count = records.len();
    out_obj.insert("records".to_string(), Value::Array(records));
    out_obj.insert("count".to_string(), json!(count));
    Ok(Value::Object(out_obj))
}

fn parse_html_portal_to_json(
    _ext_id: &str,
    bytes: &[u8],
    table_idx: Option<usize>,
) -> Result<Value> {
    let raw_txt = std::str::from_utf8(bytes)
        .map(Cow::Borrowed)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes));
    let no_comments = HTML_COMMENT_RE.replace_all(&raw_txt, " ");
    let clean_html = SCRIPT_STYLE_RE.replace_all(&no_comments, " ");

    let mut attributes = Map::new();

    for cap in HTML_META_RE.captures_iter(&clean_html) {
        let name = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().trim())
            .unwrap_or("");
        let content = cap
            .get(3)
            .or_else(|| cap.get(4))
            .map(|m| html_escape::decode_html_entities(m.as_str()).trim().to_string())
            .unwrap_or_default();
        if !name.is_empty() && !content.is_empty() {
            insert_header_cell(&mut attributes, name, Value::String(content));
        }
    }

    for cap in HTML_DATA_ATTR_RE.captures_iter(&clean_html) {
        let key = cap[1].trim();
        let val = cap
            .get(2)
            .or_else(|| cap.get(3))
            .map(|m| html_escape::decode_html_entities(m.as_str()).trim().to_string())
            .unwrap_or_default();
        if !key.is_empty() && !val.is_empty() && !attributes.contains_key(key) {
            insert_header_cell(&mut attributes, key, Value::String(val));
        }
    }

    for cap in HTML_DL_PAIR_RE.captures_iter(&clean_html) {
        let k = clean_html_text(&cap[1]).trim_end_matches(':').trim().to_string();
        let v = clean_html_text(&cap[2]);
        if !k.is_empty() && !v.is_empty() {
            insert_header_cell(&mut attributes, &k, Value::String(v));
        }
    }

    let mut parsed_tables: Vec<Vec<Value>> = Vec::new();
    for tcap in HTML_TABLE_RE.captures_iter(&clean_html) {
        let tbody = &tcap[1];
        let mut raw_rows: Vec<(bool, Vec<String>, Option<String>)> = Vec::new();
        for rcap in HTML_TR_RE.captures_iter(tbody) {
            let rhtml = &rcap[1];
            let first_href = HTML_HREF_RE
                .captures(rhtml)
                .and_then(|c| c.get(1).or_else(|| c.get(2)))
                .map(|m| html_escape::decode_html_entities(m.as_str()).trim().to_string())
                .filter(|s| !s.is_empty());
            let mut cells = Vec::new();
            let mut all_th = true;
            for ccap in HTML_CELL_RE.captures_iter(rhtml) {
                if !ccap[1].eq_ignore_ascii_case("th") {
                    all_th = false;
                }
                cells.push(clean_html_text(&ccap[2]));
            }
            if !cells.is_empty() {
                raw_rows.push((all_th, cells, first_href));
            }
        }
        if raw_rows.is_empty() {
            continue;
        }

        // Detect 2-column key-value summary table vs multi-column tabular records
        let first_is_header = raw_rows[0].0 && raw_rows[0].1.len() >= 2 && raw_rows.len() >= 2;
        let is_kv_table = !first_is_header
            && raw_rows.iter().all(|(_, c, _)| c.len() == 2)
            && raw_rows.iter().any(|(_, c, _)| c[0].ends_with(':') || !c[0].is_empty());

        if is_kv_table {
            for (_, cells, _) in &raw_rows {
                let k = cells[0].trim_end_matches(':').trim();
                let v = cells[1].trim();
                if !k.is_empty() && !v.is_empty() {
                    insert_header_cell(&mut attributes, k, Value::String(v.to_string()));
                }
            }
            continue;
        }

        if raw_rows.len() >= 2 && raw_rows[0].1.len() >= 2 {
            let headers = raw_rows[0].1.clone();
            let mut table_records = Vec::new();
            for (_, cells, href) in raw_rows.into_iter().skip(1) {
                let mut row_map = Map::new();
                let mut non_empty = false;
                for (idx, cell) in cells.into_iter().enumerate() {
                    if let Some(h) = headers.get(idx).filter(|s| !s.is_empty()) {
                        if !cell.is_empty() {
                            non_empty = true;
                            insert_header_cell(&mut row_map, h, Value::String(cell));
                        }
                    }
                }
                if let Some(h) = href {
                    row_map.insert("_href".to_string(), Value::String(h));
                }
                if non_empty {
                    table_records.push(Value::Object(row_map));
                }
            }
            if !table_records.is_empty() {
                parsed_tables.push(table_records);
            }
        }
    }

    let records = if let Some(idx) = table_idx {
        parsed_tables.get(idx).cloned().unwrap_or_default()
    } else if let Some(first_tbl) = parsed_tables.first() {
        first_tbl.clone()
    } else if !attributes.is_empty() {
        vec![Value::Object(attributes.clone())]
    } else {
        Vec::new()
    };

    let count = records.len();
    let tables_val: Vec<Value> = parsed_tables.into_iter().map(Value::Array).collect();
    Ok(json!({
        "records": records,
        "attributes": Value::Object(attributes),
        "tables": tables_val,
        "count": count,
    }))
}

/// Parses and normalizes heterogeneous regional government portal responses (`json`, `geojson`,
/// `arcgis_features`, `socrata_rows`, `csv`, `xml`, `html`) into a unified `serde_json::Value`
/// ready for RFC 9535 JSONPath extraction.
pub fn parse_gov_response_payload(
    ext_id: &str,
    body_bytes: &[u8],
    content_type: Option<&str>,
    resolved_url: &str,
    mapping: Option<&ResponseMappingYaml>,
) -> Result<Value> {
    let fmt = mapping.map(|m| m.format).unwrap_or(GovResponseFormat::Auto);
    let ct_low = content_type.unwrap_or("").to_ascii_lowercase();
    let url_path_low = Url::parse(resolved_url)
        .ok()
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_else(|| resolved_url.to_ascii_lowercase());

    match fmt {
        GovResponseFormat::Csv => {
            parse_csv_portal_to_json(ext_id, body_bytes, mapping.and_then(|m| m.csv_delimiter.as_deref()))
        }
        GovResponseFormat::Xml => {
            parse_xml_portal_to_json(ext_id, body_bytes, mapping.and_then(|m| m.xml_record_tag.as_deref()))
        }
        GovResponseFormat::Html => {
            parse_html_portal_to_json(ext_id, body_bytes, mapping.and_then(|m| m.html_table_index))
        }
        GovResponseFormat::Geojson => {
            let v: Value = serde_json::from_slice(body_bytes)
                .map_err(|e| anyhow!("Invalid GeoJSON from '{ext_id}': {e}"))?;
            normalize_geojson_value(v)
        }
        GovResponseFormat::ArcgisFeatures => {
            let v: Value = serde_json::from_slice(body_bytes)
                .map_err(|e| anyhow!("Invalid ArcGIS JSON from '{ext_id}': {e}"))?;
            normalize_arcgis_value(ext_id, v)
        }
        GovResponseFormat::SocrataRows => {
            let v: Value = serde_json::from_slice(body_bytes)
                .map_err(|e| anyhow!("Invalid Socrata/CKAN JSON from '{ext_id}': {e}"))?;
            normalize_socrata_ckan_value(v)
        }
        GovResponseFormat::Json => serde_json::from_slice::<Value>(body_bytes)
            .map_err(|e| anyhow!("Invalid JSON from '{ext_id}': {e}")),
        GovResponseFormat::Auto => {
            let first_non_ws = body_bytes
                .iter()
                .copied()
                .find(|b| !b.is_ascii_whitespace() && *b != 0xEF && *b != 0xBB && *b != 0xBF);
            let looks_json = matches!(first_non_ws, Some(b'{' | b'['));

            if !looks_json
                && (ct_low.contains("text/csv")
                    || ct_low.contains("application/csv")
                    || ct_low.contains("tab-separated-values")
                    || url_path_low.ends_with(".csv")
                    || url_path_low.ends_with(".tsv"))
            {
                return parse_csv_portal_to_json(
                    ext_id,
                    body_bytes,
                    mapping.and_then(|m| m.csv_delimiter.as_deref()),
                );
            }

            if !looks_json && ct_low.contains("xml") {
                return parse_xml_portal_to_json(
                    ext_id,
                    body_bytes,
                    mapping.and_then(|m| m.xml_record_tag.as_deref()),
                );
            }

            let val = serde_json::from_slice::<Value>(body_bytes)
                .map_err(|e| anyhow!("Invalid JSON from '{ext_id}': {e}"))?;
            // Non-destructively enrich GeoJSON or ArcGIS responses with `.records` when absent.
            if let Some(obj) = val.as_object() {
                if !obj.contains_key("records") {
                    if matches!(
                        obj.get("type").and_then(Value::as_str),
                        Some("FeatureCollection" | "Feature")
                    ) {
                        return normalize_geojson_value(val);
                    }
                    if obj
                        .get("features")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_object)
                        .map(|f| f.contains_key("attributes"))
                        .unwrap_or(false)
                    {
                        return normalize_arcgis_value(ext_id, val);
                    }
                }
            }
            Ok(val)
        }
    }
}

/// Default `governor` keyed rate limiter for `ExtensionHub`.
pub fn build_default_keyed_limiter() -> DefaultKeyedRateLimiter<String> {
    let quota = Quota::per_second(NonZeroU32::new(8).expect("non-zero rps"))
        .allow_burst(NonZeroU32::new(16).expect("non-zero burst"));
    DefaultKeyedRateLimiter::keyed(quota)
}

fn json_scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn json_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s
            .trim()
            .replace([',', '$', '%'], "")
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite()),
        _ => None,
    }
}

fn json_to_i64(v: &Value) -> Option<i64> {
    json_to_f64(v).map(|x| x.round() as i64)
}

fn json_to_i32(v: &Value) -> Option<i32> {
    json_to_i64(v).and_then(|x| i32::try_from(x).ok())
}

fn json_to_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|x| x != 0),
        Value::String(s) => match s.trim().to_ascii_uppercase().as_str() {
            "TRUE" | "T" | "YES" | "Y" | "1" => Some(true),
            "FALSE" | "F" | "NO" | "N" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn eval_field_value<'a>(target: &'a Value, mapping: &ResponseMappingYaml, key: &str) -> Option<&'a Value> {
    let expr = mapping.fields.get(key)?.trim();
    if let Some(simple_prop) = expr.strip_prefix("$.") {
        if !simple_prop.is_empty()
            && simple_prop
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            if let Value::Object(map) = target {
                return map.get(simple_prop).filter(|v| !v.is_null());
            }
        }
    }
    let jp = JsonPath::parse(expr).ok()?;
    let nodes = jp.query(target);
    nodes.first().filter(|v| !v.is_null())
}

fn eval_str(target: &Value, mapping: &ResponseMappingYaml, key: &str) -> Option<String> {
    eval_field_value(target, mapping, key).and_then(json_scalar_to_string)
}

fn eval_f64(target: &Value, mapping: &ResponseMappingYaml, key: &str) -> Option<f64> {
    eval_field_value(target, mapping, key).and_then(json_to_f64)
}

fn eval_i64(target: &Value, mapping: &ResponseMappingYaml, key: &str) -> Option<i64> {
    eval_field_value(target, mapping, key).and_then(json_to_i64)
}

fn eval_i32(target: &Value, mapping: &ResponseMappingYaml, key: &str) -> Option<i32> {
    eval_field_value(target, mapping, key).and_then(json_to_i32)
}

fn eval_bool(target: &Value, mapping: &ResponseMappingYaml, key: &str) -> Option<bool> {
    eval_field_value(target, mapping, key).and_then(json_to_bool)
}

/// Maps a live upstream JSON payload into typed `ExtensionEnvelopeProto` messages via RFC 9535 JSONPath.
pub fn map_json_to_envelopes(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    raw_json: &Value,
    spec_score: i32,
    latency_ms: u32,
) -> Result<Vec<ExtensionEnvelopeProto>> {
    let ctx = build_template_context(query);
    let now = now_iso();
    let attr = build_live_attribution(pkg, &ctx, &now);

    let Some(ref mapping) = pkg.doc.response_mapping else {
        return Ok(vec![ExtensionEnvelopeProto {
            extension_id: pkg.extension_id.clone(),
            domain: pkg.primary_domain as i32,
            priority: manifest.priority,
            geo_specificity: spec_score,
            attribution: Some(attr),
            latency_ms,
            cached: false,
            payload: Some(Payload::RawJson(raw_json.to_string())),
        }]);
    };

    let is_normalized_gov_fmt = !matches!(
        mapping.format,
        GovResponseFormat::Auto | GovResponseFormat::Json
    );
    let default_records_items_path = (is_normalized_gov_fmt
        && matches!(
            pkg.primary_domain,
            CapabilityDomain::DomainBroadband | CapabilityDomain::DomainPermits
        )
        && raw_json.get("records").and_then(Value::as_array).is_some())
    .then_some("$.records[*]");

    let effective_items_path = mapping
        .items_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(default_records_items_path);

    // Multi-item array extraction (e.g., FCC BDC `items_path: "$.data[*]"` or normalized `$.records[*]`)
    if let Some(ip) = effective_items_path {
        let jp = JsonPath::parse(ip)?;
        let items: Vec<&Value> = jp.query(raw_json).into_iter().collect();
        if items.is_empty() && mapping.require_non_empty {
            return Ok(Vec::new());
        }
        if pkg.primary_domain == CapabilityDomain::DomainBroadband {
            return Ok(extract_broadband_items(
                pkg, manifest, query, mapping, &items, spec_score, latency_ms, &attr,
            ));
        }
    }

    let target: &Value = if let Some(rp) = mapping
        .root_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "$")
    {
        let jp = JsonPath::parse(rp)?;
        match jp.query(raw_json).first() {
            Some(node) if !node.is_null() => node,
            _ => {
                // If normalized Socrata/CSV wrapped rows into `{"records": [...]}` and manifest wrote `$[0]`
                if rp == "$[0]" {
                    if let Some(first_rec) = raw_json
                        .get("records")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .filter(|v| !v.is_null())
                    {
                        first_rec
                    } else if mapping.require_non_empty {
                        return Ok(Vec::new());
                    } else {
                        raw_json
                    }
                } else if mapping.require_non_empty {
                    return Ok(Vec::new());
                } else {
                    raw_json
                }
            }
        }
    } else if is_normalized_gov_fmt
        && !matches!(
            pkg.primary_domain,
            CapabilityDomain::DomainBroadband | CapabilityDomain::DomainPermits
        )
        && raw_json.get("records").and_then(Value::as_array).is_some()
    {
        match raw_json
            .get("records")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .filter(|v| !v.is_null())
        {
            Some(first_rec) => first_rec,
            None => {
                if mapping.require_non_empty {
                    return Ok(Vec::new());
                }
                raw_json
            }
        }
    } else {
        raw_json
    };

    let payload = match pkg.primary_domain {
        CapabilityDomain::DomainGeocode => {
            let lat = eval_f64(target, mapping, "lat");
            let lon = eval_f64(target, mapping, "lon");
            let zip = eval_str(target, mapping, "zip").unwrap_or_default();
            if lat.is_none() && lon.is_none() && zip.is_empty() && mapping.require_non_empty {
                return Ok(Vec::new());
            }
            let fallback_sd_geoid = ["Unified School Districts", "Elementary School Districts", "Secondary School Districts"]
                .iter()
                .find_map(|k| {
                    target
                        .get("geographies")
                        .and_then(|g| g.get(*k))
                        .and_then(|a| a.get(0))
                        .and_then(|o| o.get("GEOID"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                });
            let fallback_sd_name = ["Unified School Districts", "Elementary School Districts", "Secondary School Districts"]
                .iter()
                .find_map(|k| {
                    target
                        .get("geographies")
                        .and_then(|g| g.get(*k))
                        .and_then(|a| a.get(0))
                        .and_then(|o| o.get("NAME"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                });
            Payload::Geocode(GeocodeResultProto {
                matched_address: eval_str(target, mapping, "matched_address").unwrap_or_default(),
                lat: lat.unwrap_or(0.0),
                lon: lon.unwrap_or(0.0),
                street: eval_str(target, mapping, "street").unwrap_or_default(),
                city: eval_str(target, mapping, "city").unwrap_or_default(),
                state: eval_str(target, mapping, "state").unwrap_or_default(),
                zip,
                state_fips: eval_str(target, mapping, "state_fips").unwrap_or_default(),
                county_fips: eval_str(target, mapping, "county_fips").unwrap_or_default(),
                county_name: eval_str(target, mapping, "county_name").unwrap_or_default(),
                census_tract: eval_str(target, mapping, "census_tract"),
                block_group: eval_str(target, mapping, "block_group"),
                cbsa_code: eval_str(target, mapping, "cbsa_code"),
                cbsa_title: eval_str(target, mapping, "cbsa_title"),
                school_district_geoid: eval_str(target, mapping, "school_district_geoid")
                    .or(fallback_sd_geoid),
                school_district_name: eval_str(target, mapping, "school_district_name")
                    .or(fallback_sd_name),
            })
        }
        CapabilityDomain::DomainPropertyFacts => Payload::PropertyFacts(PropertyFactsProto {
            matched_address: eval_str(target, mapping, "matched_address")
                .unwrap_or_else(|| query.address.clone()),
            street: eval_str(target, mapping, "street").unwrap_or_else(|| query.street.clone()),
            city: eval_str(target, mapping, "city").unwrap_or_else(|| query.city.clone()),
            state: eval_str(target, mapping, "state").unwrap_or_else(|| query.state.clone()),
            zip: eval_str(target, mapping, "zip").unwrap_or_else(|| query.zip.clone()),
            county_fips: eval_str(target, mapping, "county_fips")
                .unwrap_or_else(|| query.county_fips.clone()),
            county_name: eval_str(target, mapping, "county_name")
                .or_else(|| query.county_name.clone())
                .unwrap_or_default(),
            lat: eval_f64(target, mapping, "lat").or(query.lat),
            lon: eval_f64(target, mapping, "lon").or(query.lon),
            apn: eval_str(target, mapping, "apn"),
            property_type: eval_str(target, mapping, "property_type"),
            beds: eval_f64(target, mapping, "beds"),
            baths: eval_f64(target, mapping, "baths"),
            sqft: eval_i64(target, mapping, "sqft"),
            lot_sqft: eval_i64(target, mapping, "lot_sqft"),
            year_built: eval_i64(target, mapping, "year_built"),
            stories: eval_f64(target, mapping, "stories"),
            garage_spaces: eval_f64(target, mapping, "garage_spaces"),
            hoa_monthly_usd: eval_i64(target, mapping, "hoa_monthly_usd"),
            list_price_usd: eval_i64(target, mapping, "list_price_usd"),
            last_sale_price_usd: eval_i64(target, mapping, "last_sale_price_usd"),
            last_sale_date: eval_str(target, mapping, "last_sale_date"),
            annual_property_tax_usd: eval_f64(target, mapping, "annual_property_tax_usd"),
            effective_tax_rate: eval_f64(target, mapping, "effective_tax_rate"),
            zoning: eval_str(target, mapping, "zoning"),
            architectural_style: eval_str(target, mapping, "architectural_style"),
            features: Vec::new(),
            has_solar: eval_bool(target, mapping, "has_solar"),
            has_ev_charger: eval_bool(target, mapping, "has_ev_charger"),
            has_heat_pump: eval_bool(target, mapping, "has_heat_pump"),
            has_adu: eval_bool(target, mapping, "has_adu"),
            adu_feasible: eval_bool(target, mapping, "adu_feasible"),
            has_pool: eval_bool(target, mapping, "has_pool"),
            mls_number: eval_str(target, mapping, "mls_number"),
            listing_status: eval_str(target, mapping, "listing_status"),
        }),
        CapabilityDomain::DomainNaturalHazards => {
            let h = HazardSummaryProto {
                fema_flood_zone: eval_str(target, mapping, "fema_flood_zone"),
                in_sfha: eval_bool(target, mapping, "in_sfha"),
                nearest_sfha_meters: eval_f64(target, mapping, "nearest_sfha_meters"),
                wildfire_zone: eval_str(target, mapping, "wildfire_zone"),
                seismic_pga_g: eval_f64(target, mapping, "seismic_pga_g"),
                seismic_ss_g: eval_f64(target, mapping, "seismic_ss_g"),
                seismic_s1_g: eval_f64(target, mapping, "seismic_s1_g"),
                seismic_design_category: eval_str(target, mapping, "seismic_design_category"),
                nearest_fault_km: eval_f64(target, mapping, "nearest_fault_km"),
                nearest_fault_name: eval_str(target, mapping, "nearest_fault_name"),
                epa_radon_zone: eval_i32(target, mapping, "epa_radon_zone"),
                fema_nri_composite_score: eval_f64(target, mapping, "fema_nri_composite_score"),
                overall_safety_score: eval_f64(target, mapping, "overall_safety_score"),
                highlights: Vec::new(),
            };
            Payload::HazardSummary(h)
        }
        CapabilityDomain::DomainEnvironmental => {
            let e = EnvironmentalSummaryProto {
                water_system_pwsid: eval_str(target, mapping, "water_system_pwsid"),
                water_system_name: eval_str(target, mapping, "water_system_name"),
                sdwis_violation_count: eval_i32(target, mapping, "sdwis_violation_count"),
                superfund_count_3mi: eval_i32(target, mapping, "superfund_count_3mi"),
                nearest_superfund_km: eval_f64(target, mapping, "nearest_superfund_km"),
                tri_facility_count_2mi: eval_i32(target, mapping, "tri_facility_count_2mi"),
                lust_open_count_1mi: eval_i32(target, mapping, "lust_open_count_1mi"),
                pm25_mean_ug_m3: eval_f64(target, mapping, "pm25_mean_ug_m3"),
                us_aqi_recent: eval_i32(target, mapping, "us_aqi_recent"),
                road_noise_dba: eval_f64(target, mapping, "road_noise_dba"),
                aviation_noise_dba: eval_f64(target, mapping, "aviation_noise_dba"),
                pollution_burden_percentile: eval_f64(
                    target,
                    mapping,
                    "pollution_burden_percentile",
                ),
                environmental_health_score: eval_f64(target, mapping, "environmental_health_score"),
                highlights: Vec::new(),
            };
            Payload::EnvironmentalSummary(e)
        }
        CapabilityDomain::DomainLivability
        | CapabilityDomain::DomainSchools
        | CapabilityDomain::DomainCrime => Payload::LivabilitySummary(LivabilitySummaryProto {
            walkability_index: eval_f64(target, mapping, "walkability_index"),
            amenity_access_score: eval_f64(target, mapping, "amenity_access_score"),
            grocery_count_1mi: eval_i32(target, mapping, "grocery_count_1mi"),
            restaurant_count_1mi: eval_i32(target, mapping, "restaurant_count_1mi"),
            park_count_1mi: eval_i32(target, mapping, "park_count_1mi"),
            transit_stop_count_half_mi: eval_i32(target, mapping, "transit_stop_count_half_mi"),
            nearest_freeway_meters: eval_f64(target, mapping, "nearest_freeway_meters"),
            nearest_high_voltage_meters: eval_f64(target, mapping, "nearest_high_voltage_meters"),
            school_composite_score: eval_f64(target, mapping, "school_composite_score"),
            school_grade: eval_str(target, mapping, "school_grade"),
            school_district: eval_str(target, mapping, "school_district"),
            schools: Vec::new(),
            violent_crime_per_100k: eval_f64(target, mapping, "violent_crime_per_100k"),
            property_crime_per_100k: eval_f64(target, mapping, "property_crime_per_100k"),
            crime_safety_score: eval_f64(target, mapping, "crime_safety_score"),
        }),
        CapabilityDomain::DomainValuation | CapabilityDomain::DomainMarketTrends => {
            Payload::MarketValuation(MarketValuationProto {
                estimated_value_usd: eval_i64(target, mapping, "estimated_value_usd"),
                value_low_usd: eval_i64(target, mapping, "value_low_usd"),
                value_high_usd: eval_i64(target, mapping, "value_high_usd"),
                confidence_0_to_1: eval_f64(target, mapping, "confidence_0_to_1"),
                estimated_monthly_rent_usd: eval_i64(target, mapping, "estimated_monthly_rent_usd"),
                zip_median_home_value_usd: eval_i64(target, mapping, "zip_median_home_value_usd"),
                yoy_appreciation_pct: eval_f64(target, mapping, "yoy_appreciation_pct"),
                cagr_5yr_pct: eval_f64(target, mapping, "cagr_5yr_pct"),
                mortgage_rate_30yr_pct: eval_f64(target, mapping, "mortgage_rate_30yr_pct"),
                median_household_income_usd: eval_i64(
                    target,
                    mapping,
                    "median_household_income_usd",
                ),
                tract_broadband_adoption_pct: eval_f64(
                    target,
                    mapping,
                    "tract_broadband_adoption_pct",
                ),
                estimated_annual_insurance_usd: eval_i64(
                    target,
                    mapping,
                    "estimated_annual_insurance_usd",
                ),
                effective_property_tax_rate: eval_f64(
                    target,
                    mapping,
                    "effective_property_tax_rate",
                ),
                has_special_tax_district: eval_bool(target, mapping, "has_special_tax_district"),
                special_tax_district_label: eval_str(
                    target,
                    mapping,
                    "special_tax_district_label",
                ),
            })
        }
        CapabilityDomain::DomainElectricUtility => {
            let Some(rate) = eval_f64(target, mapping, "rate_per_kwh_usd") else {
                return Ok(Vec::new());
            };
            Payload::ElectricUtility(ElectricUtilityProto {
                carrier_name: eval_str(target, mapping, "carrier_name").unwrap_or_default(),
                short_name: eval_str(target, mapping, "short_name").unwrap_or_default(),
                utility_type: eval_str(target, mapping, "utility_type")
                    .unwrap_or_else(|| "iou".to_string()),
                utility_type_label: eval_str(target, mapping, "utility_type_label")
                    .unwrap_or_else(|| "Investor-Owned Utility (IOU)".to_string()),
                cca_program: eval_str(target, mapping, "cca_program"),
                rate_schedule: eval_str(target, mapping, "rate_schedule").unwrap_or_default(),
                rate_per_kwh_usd: round3(rate),
                off_peak_kwh_usd: round3(
                    eval_f64(target, mapping, "off_peak_kwh_usd").unwrap_or(rate),
                ),
                peak_kwh_usd: round3(eval_f64(target, mapping, "peak_kwh_usd").unwrap_or(rate)),
                monthly_base_fee_usd: round2(
                    eval_f64(target, mapping, "monthly_base_fee_usd").unwrap_or(0.0),
                ),
                solar_nem_policy: eval_str(target, mapping, "solar_nem_policy").unwrap_or_default(),
                tariff_url: attr.source_url.clone(),
                rate_source: attr.source_name.clone(),
            })
        }
        CapabilityDomain::DomainGasUtility => Payload::GasUtility(GasUtilityProto {
            carrier_name: eval_str(target, mapping, "carrier_name").unwrap_or_default(),
            heating_fuel_norm: eval_str(target, mapping, "heating_fuel_norm")
                .unwrap_or_else(|| "natural_gas".to_string()),
            rate_schedule: eval_str(target, mapping, "rate_schedule").unwrap_or_default(),
            rate_per_therm_usd: eval_f64(target, mapping, "rate_per_therm_usd").map(round2),
            monthly_base_fee_usd: round2(
                eval_f64(target, mapping, "monthly_base_fee_usd").unwrap_or(0.0),
            ),
            tariff_url: attr.source_url.clone(),
            rate_source: attr.source_name.clone(),
        }),
        CapabilityDomain::DomainWaterSewerTrash => Payload::WaterSewerTrash(WaterSewerTrashProto {
            water_carrier_name: eval_str(target, mapping, "water_carrier_name").unwrap_or_default(),
            water_pwsid: eval_str(target, mapping, "water_pwsid"),
            water_source_type: eval_str(target, mapping, "water_source_type")
                .unwrap_or_else(|| "municipal".to_string()),
            water_rate_unit: eval_str(target, mapping, "water_rate_unit").unwrap_or_default(),
            water_volumetric_rate_usd: eval_f64(target, mapping, "water_volumetric_rate_usd")
                .map(round2),
            water_base_monthly_usd: round2(
                eval_f64(target, mapping, "water_base_monthly_usd").unwrap_or(0.0),
            ),
            water_tariff_url: Some(attr.source_url.clone()),
            sewer_carrier_name: eval_str(target, mapping, "sewer_carrier_name").unwrap_or_default(),
            sewer_type: eval_str(target, mapping, "sewer_type")
                .unwrap_or_else(|| "municipal".to_string()),
            sewer_monthly_usd: eval_i64(target, mapping, "sewer_monthly_usd").unwrap_or(0),
            trash_carrier_name: eval_str(target, mapping, "trash_carrier_name").unwrap_or_default(),
            trash_monthly_usd: eval_i64(target, mapping, "trash_monthly_usd").unwrap_or(0),
        }),
        CapabilityDomain::DomainInsurance => Payload::InsuranceQuote(InsuranceQuoteProto {
            estimated_annual_premium_usd: eval_i64(
                target,
                mapping,
                "estimated_annual_premium_usd",
            ),
            effective_pct_of_value: eval_f64(target, mapping, "effective_pct_of_value"),
            fair_plan_or_surplus_risk: eval_bool(target, mapping, "fair_plan_or_surplus_risk"),
            flood_insurance_annual_usd: eval_i64(target, mapping, "flood_insurance_annual_usd"),
            earthquake_insurance_annual_usd: eval_i64(
                target,
                mapping,
                "earthquake_insurance_annual_usd",
            ),
            carrier_market_status: eval_str(target, mapping, "carrier_market_status"),
            underwriting_notes: Vec::new(),
        }),
        CapabilityDomain::DomainTaxMandates => Payload::TaxMandates(TaxMandatesProto {
            effective_property_tax_rate: eval_f64(target, mapping, "effective_property_tax_rate"),
            state_transfer_tax_pct: eval_f64(target, mapping, "state_transfer_tax_pct"),
            local_transfer_tax_pct: eval_f64(target, mapping, "local_transfer_tax_pct"),
            estimated_transfer_tax_usd: eval_i64(target, mapping, "estimated_transfer_tax_usd"),
            has_special_tax_district: eval_bool(target, mapping, "has_special_tax_district"),
            special_tax_district_type: eval_str(target, mapping, "special_tax_district_type"),
            homestead_exemption_summary: eval_str(target, mapping, "homestead_exemption_summary"),
            point_of_sale_mandates: Vec::new(),
        }),
        CapabilityDomain::DomainHouse3d => Payload::House3d(House3dMetadataProto {
            roof_ridge_height_m: eval_f64(target, mapping, "roof_ridge_height_m"),
            ground_elevation_m: eval_f64(target, mapping, "ground_elevation_m"),
            dominant_roof_pitch_deg: eval_f64(target, mapping, "dominant_roof_pitch_deg"),
            roof_footprint_sqm: eval_f64(target, mapping, "roof_footprint_sqm"),
            tree_canopy_count: eval_i32(target, mapping, "tree_canopy_count"),
            lidar_project_name: eval_str(target, mapping, "lidar_project_name"),
            tour_embed_url: eval_str(target, mapping, "tour_embed_url"),
        }),
        CapabilityDomain::DomainRouting => {
            let sample_dur = eval_f64(target, mapping, "sample_duration_sec");
            let sample_dist = eval_f64(target, mapping, "sample_distance_m");
            let endpoint_host = pkg
                .openapi_doc
                .as_ref()
                .and_then(|oas| oas.servers.first())
                .and_then(|s| Url::parse(&s.url).ok())
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            Payload::RoutingMatrix(RoutingMatrixProto {
                provider_id: eval_str(target, mapping, "provider_id")
                    .unwrap_or_else(|| pkg.extension_id.clone()),
                provider_name: eval_str(target, mapping, "provider_name")
                    .unwrap_or_else(|| pkg.doc.name.clone()),
                tier: eval_str(target, mapping, "tier").unwrap_or_else(|| "fallback".to_string()),
                profile: eval_str(target, mapping, "profile")
                    .unwrap_or_else(|| "driving".to_string()),
                endpoint_host: eval_str(target, mapping, "endpoint_host").unwrap_or(endpoint_host),
                max_coordinates: eval_i32(target, mapping, "max_coordinates")
                    .map(|v| v.max(2) as u32)
                    .unwrap_or(25),
                origin_count: eval_i32(target, mapping, "origin_count")
                    .map(|v| v.max(1) as u32)
                    .unwrap_or(1),
                destination_count: eval_i32(target, mapping, "destination_count")
                    .map(|v| v.max(1) as u32)
                    .unwrap_or(1),
                sample_duration_sec: sample_dur,
                sample_distance_m: sample_dist,
                healthy: eval_bool(target, mapping, "healthy")
                    .unwrap_or_else(|| sample_dur.is_some() || sample_dist.is_some()),
            })
        }
        CapabilityDomain::DomainPermits => {
            if let Some(ip) = effective_items_path {
                let jp = JsonPath::parse(ip)?;
                let rows: Vec<&Value> = jp.query(raw_json).into_iter().collect();
                if rows.is_empty() && mapping.require_non_empty {
                    return Ok(Vec::new());
                }
                let permits = rows
                    .iter()
                    .map(|row| extract_permit_record_from_row(row, mapping, pkg, &attr))
                    .collect();
                Payload::PermitsHistory(PermitsHistoryProto {
                    permits,
                    sources_checked: vec![pkg.extension_id.clone()],
                    portal_name: pkg.doc.name.clone(),
                    portal_url: attr.source_url.clone(),
                    coverage_complete: eval_bool(target, mapping, "coverage_complete")
                        .unwrap_or(true),
                    queried_layers: Vec::new(),
                    open_code_enforcement_count: eval_i32(
                        target,
                        mapping,
                        "open_code_enforcement_count",
                    ),
                    closed_code_enforcement_count: eval_i32(
                        target,
                        mapping,
                        "closed_code_enforcement_count",
                    ),
                    notes: eval_str(target, mapping, "notes").unwrap_or_default(),
                })
            } else {
                Payload::PermitsHistory(PermitsHistoryProto {
                    permits: vec![extract_permit_record_from_row(target, mapping, pkg, &attr)],
                    sources_checked: vec![pkg.extension_id.clone()],
                    portal_name: pkg.doc.name.clone(),
                    portal_url: attr.source_url.clone(),
                    coverage_complete: eval_bool(target, mapping, "coverage_complete")
                        .unwrap_or(true),
                    queried_layers: Vec::new(),
                    open_code_enforcement_count: eval_i32(
                        target,
                        mapping,
                        "open_code_enforcement_count",
                    ),
                    closed_code_enforcement_count: eval_i32(
                        target,
                        mapping,
                        "closed_code_enforcement_count",
                    ),
                    notes: eval_str(target, mapping, "notes").unwrap_or_default(),
                })
            }
        }
        CapabilityDomain::DomainPermitIntel => {
            extract_permit_intel_payload(pkg, target, mapping)
        }
        _ => Payload::RawJson(target.to_string()),
    };

    Ok(vec![ExtensionEnvelopeProto {
        extension_id: pkg.extension_id.clone(),
        domain: pkg.primary_domain as i32,
        priority: manifest.priority,
        geo_specificity: spec_score,
        attribution: Some(attr),
        latency_ms,
        cached: false,
        payload: Some(payload),
    }])
}

/// Evaluates a declarative YAML Stage-2 extractor directly against serialized upstream envelopes
/// (`query.upstream_envelopes`) without performing a separate HTTP request.
pub fn execute_upstream_yaml_mapper(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
) -> Result<Vec<ExtensionEnvelopeProto>> {
    let Some(spec_score) = pkg.compiled_geo_scope.match_query(query) else {
        return Ok(Vec::new());
    };
    let mut flattened_permits: Vec<Value> = Vec::new();
    let mut sources_checked: Vec<String> = Vec::new();
    let mut upstream_json: Vec<Value> = Vec::with_capacity(query.upstream_envelopes.len());
    for env in &query.upstream_envelopes {
        sources_checked.push(env.extension_id.clone());
        if let Some(Payload::PermitsHistory(ref hist)) = env.payload {
            for p in &hist.permits {
                if let Ok(pv) = serde_json::to_value(p) {
                    flattened_permits.push(pv);
                }
            }
        }
        if let Ok(ev) = serde_json::to_value(env) {
            upstream_json.push(ev);
        }
    }
    let root = json!({
        "upstream": upstream_json,
        "permits": flattened_permits,
        "sources_checked": sources_checked,
    });
    map_json_to_envelopes(pkg, manifest, query, &root, spec_score, 0)
}

fn extract_permit_record_from_row(
    row: &Value,
    mapping: &ResponseMappingYaml,
    pkg: &ParsedExtensionPackage,
    attr: &SourceAttributionProto,
) -> PermitRecordProto {
    let permit_number = eval_str(row, mapping, "permit_number").unwrap_or_default();
    let raw_type = eval_str(row, mapping, "raw_type").unwrap_or_default();
    let category = eval_str(row, mapping, "category")
        .unwrap_or_else(|| "other".to_string())
        .to_ascii_lowercase();
    let status = eval_str(row, mapping, "status").unwrap_or_default();
    let final_date = eval_str(row, mapping, "final_date").unwrap_or_default();
    let issue_date = eval_str(row, mapping, "issue_date").unwrap_or_default();
    let applied_date = eval_str(row, mapping, "applied_date").unwrap_or_default();
    let year = eval_i32(row, mapping, "year").or_else(|| {
        [&final_date, &issue_date, &applied_date]
            .iter()
            .find_map(|d| d.get(0..4).and_then(|y| y.parse::<i32>().ok()))
    });
    let finaled = eval_bool(row, mapping, "finaled").unwrap_or_else(|| {
        !final_date.trim().is_empty()
            || status.to_ascii_lowercase().contains("final")
            || status.to_ascii_lowercase().contains("complete")
    });
    let is_parent_lot = eval_bool(row, mapping, "is_parent_lot").unwrap_or(false);
    PermitRecordProto {
        permit_number,
        category,
        raw_type,
        description: eval_str(row, mapping, "description").unwrap_or_default(),
        address: eval_str(row, mapping, "address").unwrap_or_default(),
        status,
        final_date,
        issue_date,
        applied_date,
        year,
        finaled,
        verified: eval_bool(row, mapping, "verified").unwrap_or(finaled && !is_parent_lot),
        source_verified: eval_bool(row, mapping, "source_verified").unwrap_or(true),
        is_parent_lot,
        scope: eval_str(row, mapping, "scope").unwrap_or_else(|| {
            if is_parent_lot {
                "parent_lot".to_string()
            } else {
                "subject".to_string()
            }
        }),
        solar_kw: eval_f64(row, mapping, "solar_kw"),
        solar_panels: eval_i32(row, mapping, "solar_panels").map(|n| n.max(0) as u32),
        valuation_usd: eval_i64(row, mapping, "valuation_usd"),
        contractor: eval_str(row, mapping, "contractor").unwrap_or_default(),
        source: eval_str(row, mapping, "source").unwrap_or_else(|| pkg.doc.name.clone()),
        source_url: eval_str(row, mapping, "source_url").unwrap_or_else(|| attr.source_url.clone()),
        apn: eval_str(row, mapping, "apn").unwrap_or_default(),
    }
}

fn extract_permit_intel_payload(
    pkg: &ParsedExtensionPackage,
    target: &Value,
    mapping: &ResponseMappingYaml,
) -> Payload {
    let short = pkg
        .proto_message_type
        .strip_prefix("openestate.extension.v1.")
        .unwrap_or(&pkg.proto_message_type);
    match short {
        "SolarIntelProto" => Payload::SolarIntel(SolarIntelProto {
            has_solar: eval_bool(target, mapping, "has_solar").unwrap_or(false),
            total_kw: eval_f64(target, mapping, "total_kw"),
            panel_count: eval_i32(target, mapping, "panel_count").map(|n| n.max(0) as u32),
            estimated_annual_kwh: eval_f64(target, mapping, "estimated_annual_kwh"),
            permits: Vec::new(),
            note: eval_str(target, mapping, "note").unwrap_or_default(),
        }),
        "MepIntelProto" => Payload::MepIntel(MepIntelProto {
            has_hvac_upgrade: eval_bool(target, mapping, "has_hvac_upgrade").unwrap_or(false),
            has_electrical_upgrade: eval_bool(target, mapping, "has_electrical_upgrade")
                .unwrap_or(false),
            has_plumbing_upgrade: eval_bool(target, mapping, "has_plumbing_upgrade")
                .unwrap_or(false),
            has_ev_charger: eval_bool(target, mapping, "has_ev_charger").unwrap_or(false),
            has_heat_pump: eval_bool(target, mapping, "has_heat_pump").unwrap_or(false),
            latest_hvac_year: eval_i32(target, mapping, "latest_hvac_year"),
            latest_electrical_year: eval_i32(target, mapping, "latest_electrical_year"),
            latest_plumbing_year: eval_i32(target, mapping, "latest_plumbing_year"),
            permits: Vec::new(),
            note: eval_str(target, mapping, "note").unwrap_or_default(),
        }),
        "RemodelIntelProto" => Payload::RemodelIntel(RemodelIntelProto {
            has_remodel_or_addition: eval_bool(target, mapping, "has_remodel_or_addition")
                .unwrap_or(false),
            has_adu_permit: eval_bool(target, mapping, "has_adu_permit").unwrap_or(false),
            has_seismic_foundation: eval_bool(target, mapping, "has_seismic_foundation")
                .unwrap_or(false),
            has_pool_permit: eval_bool(target, mapping, "has_pool_permit").unwrap_or(false),
            latest_remodel_year: eval_i32(target, mapping, "latest_remodel_year"),
            total_declared_valuation_usd: eval_i64(target, mapping, "total_declared_valuation_usd"),
            permits: Vec::new(),
            note: eval_str(target, mapping, "note").unwrap_or_default(),
        }),
        "CodeEnforcementIntelProto" => {
            let open_count = eval_i32(target, mapping, "open_count")
                .map(|n| n.max(0) as u32)
                .unwrap_or(0);
            Payload::CodeEnforcementIntel(CodeEnforcementIntelProto {
                open_count,
                closed_count: eval_i32(target, mapping, "closed_count")
                    .map(|n| n.max(0) as u32)
                    .unwrap_or(0),
                has_open_violation: eval_bool(target, mapping, "has_open_violation")
                    .unwrap_or(open_count > 0),
                cases: Vec::new(),
                note: eval_str(target, mapping, "note").unwrap_or_default(),
            })
        }
        _ => Payload::RoofIntel(RoofIntelProto {
            verification_status: eval_str(target, mapping, "verification_status")
                .unwrap_or_else(|| "unverified".to_string()),
            replacement_date_verified: eval_bool(target, mapping, "replacement_date_verified")
                .unwrap_or(false),
            reroof_verified: eval_bool(target, mapping, "reroof_verified").unwrap_or(false),
            verified_reroof_year: eval_i32(target, mapping, "verified_reroof_year"),
            issued_reroof_year: eval_i32(target, mapping, "issued_reroof_year"),
            new_construction_permit_year: eval_i32(target, mapping, "new_construction_permit_year"),
            effective_year: eval_i32(target, mapping, "effective_year"),
            effective_year_source: eval_str(target, mapping, "effective_year_source")
                .unwrap_or_else(|| "unknown".to_string()),
            material: eval_str(target, mapping, "material"),
            material_source: eval_str(target, mapping, "material_source")
                .unwrap_or_else(|| "unknown".to_string()),
            note: eval_str(target, mapping, "note").unwrap_or_default(),
            matching_permits: Vec::new(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_broadband_items(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    mapping: &ResponseMappingYaml,
    items: &[&Value],
    spec_score: i32,
    latency_ms: u32,
    attr: &SourceAttributionProto,
) -> Vec<ExtensionEnvelopeProto> {
    let fcc_url = match (query.lat, query.lon) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => format!(
            "https://broadbandmap.fcc.gov/location-summary/fixed?latlon={la:.6}%2C{lo:.6}"
        ),
        _ => attr.source_url.clone(),
    };

    let mut dedup: HashMap<String, BroadbandOptionProto> = HashMap::new();
    for row in items.iter().take(40) {
        let Some(name) = eval_str(row, mapping, "carrier_name")
            .or_else(|| eval_str(row, mapping, "fallback_carrier_name"))
        else {
            continue;
        };
        let tech_code = eval_i64(row, mapping, "technology_code");
        let tech_desc = eval_str(row, mapping, "technology_desc").unwrap_or_default();
        let down = eval_i64(row, mapping, "max_download_mbps").unwrap_or(0);
        let up = eval_i64(row, mapping, "max_upload_mbps").unwrap_or(0);

        let (tier_category, technology) = classify_fcc_technology(tech_code, &tech_desc);
        let symmetrical = down > 0 && up > 0 && up >= (down * 8 / 10);
        let opt = BroadbandOptionProto {
            carrier_name: name.clone(),
            technology,
            tier_category: tier_category.clone(),
            max_download_mbps: down,
            max_upload_mbps: up,
            symmetrical,
            typical_monthly_usd: String::new(),
            availability_status: "confirmed_fcc".to_string(),
            check_url: carrier_check_url_for_name(&name, &fcc_url),
        };
        let key = format!("{}::{}", normalize_carrier_brand_key(&name), tier_category);
        match dedup.get_mut(&key) {
            Some(existing) if down > existing.max_download_mbps => {
                *existing = opt;
            }
            None => {
                dedup.insert(key, opt);
            }
            _ => {}
        }
    }

    let mut fiber_carriers = Vec::new();
    let mut cable_carriers = Vec::new();
    let mut wireless_carriers = Vec::new();
    for opt in dedup.into_values() {
        match opt.tier_category.as_str() {
            "fiber" => fiber_carriers.push(opt),
            "cable" => cable_carriers.push(opt),
            _ => wireless_carriers.push(opt),
        }
    }
    fiber_carriers.sort_by(|a, b| b.max_download_mbps.cmp(&a.max_download_mbps));
    cable_carriers.sort_by(|a, b| b.max_download_mbps.cmp(&a.max_download_mbps));
    wireless_carriers.sort_by(|a, b| b.max_download_mbps.cmp(&a.max_download_mbps));

    let max_down = fiber_carriers
        .iter()
        .chain(&cable_carriers)
        .chain(&wireless_carriers)
        .map(|c| c.max_download_mbps)
        .max()
        .unwrap_or(0);
    let max_up = fiber_carriers
        .iter()
        .chain(&cable_carriers)
        .chain(&wireless_carriers)
        .map(|c| c.max_upload_mbps)
        .max()
        .unwrap_or(0);
    let fiber_avail = !fiber_carriers.is_empty();
    let high_speed = fiber_avail || !cable_carriers.is_empty() || max_down >= 100;
    let tier = if fiber_avail {
        "fiber_confirmed"
    } else if !cable_carriers.is_empty() || max_down >= 100 {
        "cable_high_speed"
    } else {
        "wireless_or_dsl"
    };

    let cat = BroadbandCatalogProto {
        high_speed_available: high_speed,
        fiber_available: fiber_avail,
        fiber_confirmed_fcc: fiber_avail,
        internet_tier: tier.to_string(),
        verification_status: "fcc_location_verified".to_string(),
        headline_verdict: if let Some(top) = fiber_carriers.first() {
            format!(
                "Symmetrical Fiber Confirmed (FCC BDC): {} up to {} Mbps",
                top.carrier_name, top.max_download_mbps
            )
        } else if let Some(cab) = cable_carriers.first() {
            format!(
                "High-Speed Cable Confirmed (FCC BDC): {} up to {} Mbps",
                cab.carrier_name, cab.max_download_mbps
            )
        } else {
            "Wireless / Satellite Connectivity Reported (FCC BDC)".to_string()
        },
        max_download_mbps: max_down,
        max_upload_mbps: max_up,
        fiber_carriers,
        cable_carriers,
        wireless_carriers,
        est_monthly_internet_usd: 0,
        fcc_map_url: fcc_url,
    };

    vec![ExtensionEnvelopeProto {
        extension_id: pkg.extension_id.clone(),
        domain: CapabilityDomain::DomainBroadband as i32,
        priority: manifest.priority,
        geo_specificity: spec_score,
        attribution: Some(attr.clone()),
        latency_ms,
        cached: false,
        payload: Some(Payload::BroadbandCatalog(cat)),
    }]
}

pub fn classify_fcc_technology(tech_code: Option<i64>, tech_desc: &str) -> (String, String) {
    let lc = tech_desc.to_ascii_lowercase();
    if tech_code == Some(50) || lc.contains("fiber") || lc.contains("fttp") || lc.contains("ftth") {
        ("fiber".to_string(), "Fiber (FTTH)".to_string())
    } else if tech_code == Some(40) || lc.contains("cable") || lc.contains("docsis") {
        ("cable".to_string(), "Cable (DOCSIS 3.1/4.0)".to_string())
    } else if matches!(tech_code, Some(60 | 61))
        || lc.contains("satellite")
        || lc.contains("gso")
        || lc.contains("ngso")
    {
        ("satellite".to_string(), "LEO / Geostationary Satellite".to_string())
    } else if tech_code == Some(10) || lc.contains("copper") || lc.contains("dsl") {
        ("dsl".to_string(), "DSL / Copper".to_string())
    } else if tech_code == Some(500) || lc.contains("5g-nr") || lc.contains("5g nr") {
        ("wireless".to_string(), "5G-NR Mobile".to_string())
    } else if tech_code == Some(400) || lc.contains("4g") || lc.contains("lte") {
        ("wireless".to_string(), "4G LTE Mobile".to_string())
    } else if matches!(tech_code, Some(70 | 71))
        || lc.contains("fixed wireless")
        || lc.contains("terrestrial")
    {
        ("wireless".to_string(), "Fixed Wireless".to_string())
    } else {
        ("wireless".to_string(), "Wireless".to_string())
    }
}

pub fn typical_isp_plan_label(_tier_category: &str, _down_mbps: i64) -> String {
    String::new()
}

pub fn carrier_check_url_for_name(name: &str, fallback_fcc_url: &str) -> String {
    match normalize_carrier_brand_key(name).as_str() {
        "att" => "https://www.att.com/internet/fiber/".to_string(),
        "google_fiber" => "https://fiber.google.com/".to_string(),
        "verizon" => "https://www.verizon.com/home/internet/fios-fastest-internet/".to_string(),
        "frontier" => "https://frontier.com/shop/internet/fiber-internet".to_string(),
        "quantum_lumen" => "https://www.quantumfiber.com/".to_string(),
        "ziply" => "https://ziplyfiber.com/".to_string(),
        "xfinity" => "https://www.xfinity.com/learn/internet-service".to_string(),
        "spectrum" => "https://www.spectrum.com/internet".to_string(),
        "cox" => "https://www.cox.com/residential/internet.html".to_string(),
        "optimum" => "https://www.optimum.com/internet".to_string(),
        "tmobile" => "https://www.t-mobile.com/home-internet".to_string(),
        "starlink" => "https://www.starlink.com/".to_string(),
        _ => fallback_fcc_url.to_string(),
    }
}

fn build_live_attribution(
    pkg: &ParsedExtensionPackage,
    ctx: &BTreeMap<String, String>,
    now: &str,
) -> SourceAttributionProto {
    let (name, fallback_url, sched, notes) = match pkg.doc.source_attribution.as_ref() {
        Some(sa) => (
            sa.name.clone(),
            sa.url.clone(),
            sa.schedule_or_version.clone(),
            sa.notes.clone(),
        ),
        None => (
            pkg.doc.name.clone(),
            pkg.doc
                .documentation_url
                .clone()
                .or_else(|| pkg.doc.homepage_url.clone())
                .unwrap_or_default(),
            None,
            None,
        ),
    };
    let rendered_url = resolve_manifest_reference_url(pkg, ctx, &fallback_url);
    SourceAttributionProto {
        extension_id: pkg.extension_id.clone(),
        source_name: name,
        source_url: rendered_url,
        provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
        fetched_at_rfc3339: now.to_string(),
        schedule_or_version: sched,
        notes,
    }
}

/// Builds a synthetic preview JSON summary describing resolved HTTP request details for Admin diagnostics.
pub fn preview_http_resolution(
    pkg: &ParsedExtensionPackage,
    query: &ExtensionQueryProto,
    env: &EnvConfig,
) -> Value {
    match resolve_http_spec(pkg, query, env) {
        Ok(spec) => {
            let safe_url = crate::modules::routing_providers::redact_url_secrets(
                &spec.url,
                pkg.doc.auth.query_param.as_deref(),
                &[],
            );
            json!({
                "resolved": true,
                "method": spec.method,
                "url": safe_url,
                "headerNames": spec.headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            })
        }
        Err(e) => json!({
            "resolved": false,
            "reason": crate::modules::routing_providers::redact_routing_error(&e.to_string(), &[]),
        }),
    }
}
