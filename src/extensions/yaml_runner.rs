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
    BroadbandRuleYaml, ElectricRuleYaml, GasRuleYaml, ParsedExtensionPackage, ResponseMappingYaml,
    WaterSewerTrashRuleYaml,
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
use openapiv3::{
    Operation, Parameter, ParameterData, ParameterSchemaOrContent, PathItem, ReferenceOr,
    SchemaKind, Type,
};
use serde_json::{json, Map, Value};
use serde_json_path::JsonPath;
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
    ctx.insert("address".to_string(), query.address.trim().to_string());
    let street_trimmed = query.street.trim();
    ctx.insert("street".to_string(), street_trimmed.to_string());
    let (house_number, street_stem) = derive_house_number_and_street_stem(street_trimmed);
    ctx.insert("house_number".to_string(), house_number);
    ctx.insert("street_stem".to_string(), street_stem);
    ctx.insert("city".to_string(), query.city.trim().to_string());
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
        if !apn.trim().is_empty() {
            ctx.insert("apn".to_string(), apn.trim().to_string());
        }
    }
    if let Some(ref parent_addr) = query.parent_address {
        if !parent_addr.trim().is_empty() {
            ctx.insert("parent_address".to_string(), parent_addr.trim().to_string());
        }
    }
    for (k, v) in &query.extra_params {
        ctx.insert(k.clone(), v.clone());
    }
    ctx
}

/// Render a `minijinja` template string against `ctx`, rejecting missing required template inputs
/// (e.g., `{{ lat }}` when `query.lat` is `None`).
pub fn render_query_template(tpl: &str, ctx: &BTreeMap<String, String>) -> Result<String> {
    if !tpl.contains("{{") {
        return Ok(tpl.to_string());
    }
    for required_var in ["lat", "lon", "address", "zip"] {
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
    let env = Environment::new();
    env.render_str(tpl, ctx)
        .with_context(|| format!("Failed to render template '{tpl}'"))
}

/// Synchronously evaluates declarative `tariff_catalog` and `footprint_catalog` rules for `pkg`.
pub fn execute_catalog_rules(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
) -> Vec<ExtensionEnvelopeProto> {
    if pkg.doc.geo_scope.match_specificity(query).is_none() {
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
        .or_else(|| pkg.doc.source_attribution.as_ref().map(|a| a.url.clone()))
        .unwrap_or_else(|| "https://www.epa.gov/ground-water-and-drinking-water".to_string());
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
        headers.insert("Accept".to_string(), "application/json".to_string());
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
/// `governor` rate limiting, `moka` caching, and RFC 9535 JSONPath extraction into typed Protobuf envelopes.
pub async fn execute_live_http(
    pkg: &ParsedExtensionPackage,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    http: &HttpClient,
    env: &EnvConfig,
    cache: &Cache<String, CachedEnvelopeEntry>,
    limiter: &DefaultDirectRateLimiter,
) -> Result<Vec<ExtensionEnvelopeProto>> {
    let Some(spec_score) = pkg.doc.geo_scope.match_specificity(query) else {
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
    let max_attempts = rl.max_retries.min(3) + 1;

    let started = Instant::now();
    let mut last_err: Option<anyhow::Error> = None;
    let mut response_json: Option<Value> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(180 * attempt as u64)).await;
        }
        let mut req_builder = match resolved.method.as_str() {
            "POST" => http.raw().post(&resolved.url),
            "PUT" => http.raw().put(&resolved.url),
            _ => http.raw().get(&resolved.url),
        }
        .timeout(timeout_dur);

        for (hk, hv) in &resolved.headers {
            req_builder = req_builder.header(hk, hv);
        }
        if let Some(ref b) = resolved.body {
            if !resolved
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            {
                req_builder = req_builder.header("Content-Type", "application/json");
            }
            req_builder = req_builder.body(b.clone());
        }

        match req_builder.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.bytes().await {
                        Ok(body_bytes) => {
                            http.record_external_http_response(
                                &resolved.url,
                                body_bytes.len() as u64,
                            );
                            match serde_json::from_slice::<Value>(&body_bytes) {
                                Ok(j) => {
                                    response_json = Some(j);
                                    break;
                                }
                                Err(e) => {
                                    last_err = Some(anyhow!(
                                        "Invalid JSON from '{}': {e}",
                                        pkg.extension_id
                                    ));
                                }
                            }
                        }
                        Err(e) => {
                            http.record_external_http_response(&resolved.url, 0);
                            last_err = Some(anyhow!(
                                "Failed reading response body from '{}': {e}",
                                pkg.extension_id
                            ));
                        }
                    }
                } else if status.is_server_error() || status.as_u16() == 429 {
                    http.record_external_http_response(&resolved.url, 0);
                    last_err = Some(anyhow!(
                        "Upstream HTTP {} from '{}'",
                        status.as_u16(),
                        pkg.extension_id
                    ));
                    continue;
                } else {
                    http.record_external_http_response(&resolved.url, 0);
                    return Err(anyhow!(
                        "Upstream HTTP {} from '{}'",
                        status.as_u16(),
                        pkg.extension_id
                    ));
                }
            }
            Err(e) => {
                http.record_external_http_response(&resolved.url, 0);
                last_err = Some(anyhow!("HTTP transport error in '{}': {e}", pkg.extension_id));
            }
        }
    }

    let json_body = match response_json {
        Some(j) => j,
        None => {
            return Err(
                last_err.unwrap_or_else(|| anyhow!("Empty response from '{}'", pkg.extension_id))
            )
        }
    };

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
    let expr = mapping.fields.get(key)?;
    let jp = JsonPath::parse(expr.trim()).ok()?;
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

    // Multi-item array extraction (e.g., FCC BDC `items_path: "$.data[*]"`)
    if let Some(ip) = mapping
        .items_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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
                school_district_geoid: eval_str(target, mapping, "school_district_geoid"),
                school_district_name: eval_str(target, mapping, "school_district_name"),
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
            if let Some(ip) = mapping
                .items_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
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
    let Some(spec_score) = pkg.doc.geo_scope.match_specificity(query) else {
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
    let (name, raw_url, sched, notes) = match pkg.doc.source_attribution.as_ref() {
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
    let rendered_url = render_query_template(&raw_url, ctx).unwrap_or(raw_url);
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
