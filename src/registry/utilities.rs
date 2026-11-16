//! Canonical US Residential Utility & Broadband Carrier Facade over `ExtensionHub`.
//!
//! All regional electric tariffs, natural-gas schedules, water/sewer/trash schedules, and fiber/cable/
//! wireless broadband carrier footprints live in declarative YAML & OpenAPI 3.x packages under
//! `extensions/<id>/manifest.yaml` (`us_municipal_electric`, `us_deregulated_electric`,
//! `us_iou_electric`, `us_natural_gas`, `us_water_sewer_trash`, `us_fiber_carriers`,
//! `us_cable_wireless_isp`, `fcc_broadband_bdc`, `epa_sdwis_water`).
//!
//! This module preserves the ergonomic typed Rust query/quote structs used by `buyer_diligence` and
//! `search::sync`, while routing every lookup dynamically through `ExtensionHub::global()` and the
//! deterministic Protobuf `aggregator` (`MERGE_GEO_MOST_SPECIFIC` for utilities, `MERGE_UNION_DEDUP`
//! for broadband). Toggling, reprioritizing, or installing a YAML extension in `/admin` takes effect
//! immediately with zero code changes and zero invented fallback numbers.

use crate::extensions::aggregator::aggregate_envelopes;
use crate::extensions::proto::v1::{
    extension_envelope_proto::Payload, BroadbandCatalogProto, BroadbandOptionProto,
    CapabilityDomain, ElectricUtilityProto, ExtensionEnvelopeProto, ExtensionQueryProto,
    GasUtilityProto, ProvenanceTier, SourceAttributionProto, WaterSewerTrashProto,
};
use crate::extensions::registry::ExtensionHub;
use crate::shared::now_iso;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElectricUtilityQuote {
    pub carrier_name: String,
    pub short_name: String,
    /// `"municipal"`, `"iou"`, `"deregulated_choice"`, `"coop"`, or `"unverified"`
    pub utility_type: String,
    pub utility_type_label: String,
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
#[serde(rename_all = "camelCase")]
pub struct GasUtilityQuote {
    pub carrier_name: String,
    /// `"natural_gas"`, `"electric_heat_pump"`, `"fuel_oil_or_propane"`, or `"unverified"`
    pub heating_fuel_norm: String,
    pub rate_schedule: String,
    pub rate_per_therm_usd: Option<f64>,
    pub monthly_base_fee_usd: f64,
    pub tariff_url: String,
    pub rate_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaterSewerTrashQuote {
    pub water_carrier_name: String,
    pub water_pwsid: Option<String>,
    /// `"municipal"`, `"private_well"`, or `"unverified"`
    pub water_source_type: String,
    pub water_rate_unit: String,
    pub water_volumetric_rate_usd: Option<f64>,
    pub water_base_monthly_usd: f64,
    pub water_tariff_url: Option<String>,
    pub sewer_carrier_name: String,
    /// `"municipal"`, `"septic"`, or `"unverified"`
    pub sewer_type: String,
    pub sewer_monthly_usd: i64,
    pub trash_carrier_name: String,
    pub trash_monthly_usd: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadbandCarrierOption {
    pub carrier_name: String,
    /// `"Fiber (FTTH)"`, `"Cable (DOCSIS 3.1/4.0)"`, `"5G Fixed Wireless"`, `"LEO Satellite"`, `"DSL / Copper"`
    pub technology: String,
    /// `"fiber"`, `"cable"`, `"wireless"`, `"satellite"`, `"dsl"`
    pub tier_category: String,
    pub max_download_mbps: i64,
    pub max_upload_mbps: i64,
    pub symmetrical: bool,
    pub typical_monthly_usd: String,
    /// `"confirmed_fcc"`, `"metro_footprint_verify_drop"`, `"nationwide_sky_view"`
    pub availability_status: String,
    pub check_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadbandCatalogQuote {
    pub high_speed_available: bool,
    pub fiber_available: bool,
    pub fiber_confirmed_fcc: bool,
    /// `"fiber_confirmed"`, `"fiber_area"`, `"cable_high_speed"`, `"wireless_or_dsl"`, `"unverified"`
    pub internet_tier: String,
    pub verification_status: String,
    pub headline_verdict: String,
    pub max_download_mbps: i64,
    pub max_upload_mbps: i64,
    pub fiber_carriers: Vec<BroadbandCarrierOption>,
    pub cable_carriers: Vec<BroadbandCarrierOption>,
    pub wireless_carriers: Vec<BroadbandCarrierOption>,
    pub est_monthly_internet_usd: i64,
    pub fcc_map_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct UtilityLookupInput<'a> {
    pub state: &'a str,
    pub county_fips: &'a str,
    pub county_name: &'a str,
    pub city: &'a str,
    pub zip: &'a str,
    pub sqft: i64,
    pub year_built: Option<i64>,
    pub is_condo_or_attached: bool,
    pub hoa_covers_water_trash: bool,
    pub is_likely_private_well: bool,
    pub is_likely_private_septic: bool,
    pub sdwis_water_name: Option<&'a str>,
    pub sdwis_pwsid: Option<&'a str>,
    pub gov_kwh_benchmark: Option<f64>,
    pub has_solar: bool,
    pub acs_broadband_pct: Option<f64>,
    pub is_rural_area: bool,
}

impl UtilityLookupInput<'_> {
    pub fn to_extension_query(&self) -> ExtensionQueryProto {
        ExtensionQueryProto {
            address: String::new(),
            street: String::new(),
            city: self.city.trim().to_string(),
            state: self.state.trim().to_ascii_uppercase(),
            zip: self.zip.trim().to_string(),
            county_fips: self.county_fips.trim().to_string(),
            lat: None,
            lon: None,
            extra_params: Default::default(),
            county_name: {
                let cn = self.county_name.trim();
                (!cn.is_empty()).then(|| cn.to_string())
            },
            sqft: (self.sqft > 0).then_some(self.sqft),
            year_built: self.year_built,
            is_condo_or_attached: Some(self.is_condo_or_attached),
            hoa_covers_water_trash: Some(self.hoa_covers_water_trash),
            is_likely_private_well: Some(self.is_likely_private_well),
            is_likely_private_septic: Some(self.is_likely_private_septic),
            sdwis_water_name: self
                .sdwis_water_name
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            sdwis_pwsid: self
                .sdwis_pwsid
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            gov_kwh_benchmark: self.gov_kwh_benchmark,
            has_solar: Some(self.has_solar),
            acs_broadband_pct: self.acs_broadband_pct,
            is_rural_area: Some(self.is_rural_area),
            ..Default::default()
        }
    }
}

impl From<ElectricUtilityProto> for ElectricUtilityQuote {
    fn from(p: ElectricUtilityProto) -> Self {
        Self {
            carrier_name: p.carrier_name,
            short_name: p.short_name,
            utility_type: p.utility_type,
            utility_type_label: p.utility_type_label,
            cca_program: p.cca_program.filter(|s| !s.trim().is_empty()),
            rate_schedule: p.rate_schedule,
            rate_per_kwh_usd: p.rate_per_kwh_usd,
            off_peak_kwh_usd: p.off_peak_kwh_usd,
            peak_kwh_usd: p.peak_kwh_usd,
            monthly_base_fee_usd: p.monthly_base_fee_usd,
            solar_nem_policy: p.solar_nem_policy,
            tariff_url: p.tariff_url,
            rate_source: p.rate_source,
        }
    }
}

impl From<GasUtilityProto> for GasUtilityQuote {
    fn from(p: GasUtilityProto) -> Self {
        Self {
            carrier_name: p.carrier_name,
            heating_fuel_norm: p.heating_fuel_norm,
            rate_schedule: p.rate_schedule,
            rate_per_therm_usd: p.rate_per_therm_usd,
            monthly_base_fee_usd: p.monthly_base_fee_usd,
            tariff_url: p.tariff_url,
            rate_source: p.rate_source,
        }
    }
}

impl From<WaterSewerTrashProto> for WaterSewerTrashQuote {
    fn from(p: WaterSewerTrashProto) -> Self {
        Self {
            water_carrier_name: p.water_carrier_name,
            water_pwsid: p.water_pwsid.filter(|s| !s.trim().is_empty()),
            water_source_type: p.water_source_type,
            water_rate_unit: p.water_rate_unit,
            water_volumetric_rate_usd: p.water_volumetric_rate_usd,
            water_base_monthly_usd: p.water_base_monthly_usd,
            water_tariff_url: p.water_tariff_url.filter(|s| !s.trim().is_empty()),
            sewer_carrier_name: p.sewer_carrier_name,
            sewer_type: p.sewer_type,
            sewer_monthly_usd: p.sewer_monthly_usd,
            trash_carrier_name: p.trash_carrier_name,
            trash_monthly_usd: p.trash_monthly_usd,
        }
    }
}

impl From<BroadbandOptionProto> for BroadbandCarrierOption {
    fn from(p: BroadbandOptionProto) -> Self {
        Self {
            carrier_name: p.carrier_name,
            technology: p.technology,
            tier_category: p.tier_category,
            max_download_mbps: p.max_download_mbps,
            max_upload_mbps: p.max_upload_mbps,
            symmetrical: p.symmetrical,
            typical_monthly_usd: p.typical_monthly_usd,
            availability_status: p.availability_status,
            check_url: p.check_url,
        }
    }
}

impl From<BroadbandCatalogProto> for BroadbandCatalogQuote {
    fn from(p: BroadbandCatalogProto) -> Self {
        Self {
            high_speed_available: p.high_speed_available,
            fiber_available: p.fiber_available,
            fiber_confirmed_fcc: p.fiber_confirmed_fcc,
            internet_tier: p.internet_tier,
            verification_status: p.verification_status,
            headline_verdict: p.headline_verdict,
            max_download_mbps: p.max_download_mbps,
            max_upload_mbps: p.max_upload_mbps,
            fiber_carriers: p.fiber_carriers.into_iter().map(Into::into).collect(),
            cable_carriers: p.cable_carriers.into_iter().map(Into::into).collect(),
            wireless_carriers: p.wireless_carriers.into_iter().map(Into::into).collect(),
            est_monthly_internet_usd: p.est_monthly_internet_usd,
            fcc_map_url: p.fcc_map_url,
        }
    }
}

/// Resolves the electric utility carrier, municipal vs. IOU vs. deregulated governance, residential
/// rate schedule, and published effective/off-peak/peak `$/kWh` quotes via `ExtensionHub`
/// (`MERGE_GEO_MOST_SPECIFIC`, so a 5-digit ZIP/city municipal utility overrides a state IOU).
pub fn resolve_electric_utility(inp: &UtilityLookupInput<'_>) -> ElectricUtilityQuote {
    resolve_electric_utility_with_hub(inp, ExtensionHub::global())
}

/// Hub-explicit variant of [`resolve_electric_utility`] avoiding split-brain global lookups when an `AppState.extension_hub` is active.
pub fn resolve_electric_utility_with_hub(
    inp: &UtilityLookupInput<'_>,
    hub: &ExtensionHub,
) -> ElectricUtilityQuote {
    let query = inp.to_extension_query();
    let envelopes = hub.execute_domain_sync(CapabilityDomain::DomainElectricUtility, &query);
    let bundle = aggregate_envelopes(&query, &envelopes, None);

    bundle
        .electric_utility
        .map(ElectricUtilityQuote::from)
        .unwrap_or_else(|| ElectricUtilityQuote {
            carrier_name: "Electric Utility Unresolved (Extension Disabled)".to_string(),
            short_name: "Unresolved".to_string(),
            utility_type: "unverified".to_string(),
            utility_type_label: "Unverified (Utility Extensions Disabled)".to_string(),
            cca_program: None,
            rate_schedule: "Unavailable".to_string(),
            rate_per_kwh_usd: 0.0,
            off_peak_kwh_usd: 0.0,
            peak_kwh_usd: 0.0,
            monthly_base_fee_usd: 0.0,
            solar_nem_policy: String::new(),
            tariff_url: String::new(),
            rate_source: "Extension Disabled".to_string(),
        })
}

/// Resolves the natural-gas / heating carrier, residential tariff schedule, and published `$/therm`
/// rate quote via `ExtensionHub` (`MERGE_GEO_MOST_SPECIFIC`).
pub fn resolve_gas_utility(inp: &UtilityLookupInput<'_>) -> GasUtilityQuote {
    resolve_gas_utility_with_hub(inp, ExtensionHub::global())
}

/// Hub-explicit variant of [`resolve_gas_utility`].
pub fn resolve_gas_utility_with_hub(
    inp: &UtilityLookupInput<'_>,
    hub: &ExtensionHub,
) -> GasUtilityQuote {
    let query = inp.to_extension_query();
    let envelopes = hub.execute_domain_sync(CapabilityDomain::DomainGasUtility, &query);
    let bundle = aggregate_envelopes(&query, &envelopes, None);

    bundle
        .gas_utility
        .map(GasUtilityQuote::from)
        .unwrap_or_else(|| GasUtilityQuote {
            carrier_name: "Heating Utility Unresolved (Extension Disabled)".to_string(),
            heating_fuel_norm: "unverified".to_string(),
            rate_schedule: "Unavailable".to_string(),
            rate_per_therm_usd: None,
            monthly_base_fee_usd: 0.0,
            tariff_url: String::new(),
            rate_source: "Extension Disabled".to_string(),
        })
}

/// Resolves the public water agency (combining EPA SDWIS PWSID metadata when supplied), sanitary
/// sewer authority, and solid-waste hauler via `ExtensionHub` (`MERGE_GEO_MOST_SPECIFIC`).
pub fn resolve_water_sewer_trash(inp: &UtilityLookupInput<'_>) -> WaterSewerTrashQuote {
    resolve_water_sewer_trash_with_hub(inp, ExtensionHub::global())
}

/// Hub-explicit variant of [`resolve_water_sewer_trash`].
pub fn resolve_water_sewer_trash_with_hub(
    inp: &UtilityLookupInput<'_>,
    hub: &ExtensionHub,
) -> WaterSewerTrashQuote {
    let query = inp.to_extension_query();
    let envelopes = hub.execute_domain_sync(CapabilityDomain::DomainWaterSewerTrash, &query);
    let bundle = aggregate_envelopes(&query, &envelopes, None);

    bundle
        .water_sewer_trash
        .map(WaterSewerTrashQuote::from)
        .unwrap_or_else(|| WaterSewerTrashQuote {
            water_carrier_name: inp
                .sdwis_water_name
                .map(String::from)
                .unwrap_or_else(|| "Water Utility Unresolved (Extension Disabled)".to_string()),
            water_pwsid: inp.sdwis_pwsid.map(String::from),
            water_source_type: if inp.is_likely_private_well {
                "private_well".to_string()
            } else {
                "unverified".to_string()
            },
            water_rate_unit: "Unavailable".to_string(),
            water_volumetric_rate_usd: None,
            water_base_monthly_usd: 0.0,
            water_tariff_url: None,
            sewer_carrier_name: "Unresolved".to_string(),
            sewer_type: if inp.is_likely_private_septic {
                "septic".to_string()
            } else {
                "unverified".to_string()
            },
            sewer_monthly_usd: 0,
            trash_carrier_name: "Unresolved".to_string(),
            trash_monthly_usd: 0,
        })
}

/// Resolves Home Internet & High-Speed Fiber / Cable / 5G Wireless carriers for a US parcel via
/// `ExtensionHub` (`MERGE_UNION_DEDUP`).
///
/// When `live_fcc` carries verified FCC BDC observations (`status == "live_fcc"`), those observations
/// enter the aggregator as `PROVENANCE_LIVE_VERIFIED` (`fcc_broadband_bdc`) using exact FCC-reported
/// speeds (never inflating download/upload speeds or fabricating fallback speeds when empty), and
/// override matching unverified regional footprint entries per carrier brand.
pub fn resolve_broadband_catalog(
    inp: &UtilityLookupInput<'_>,
    fcc_map_url: &str,
    live_fcc: Option<&Value>,
) -> BroadbandCatalogQuote {
    resolve_broadband_catalog_with_hub(inp, fcc_map_url, live_fcc, ExtensionHub::global())
}

/// Hub-explicit variant of [`resolve_broadband_catalog`].
pub fn resolve_broadband_catalog_with_hub(
    inp: &UtilityLookupInput<'_>,
    fcc_map_url: &str,
    live_fcc: Option<&Value>,
    hub: &ExtensionHub,
) -> BroadbandCatalogQuote {
    let query = inp.to_extension_query();

    let mut envelopes: Vec<ExtensionEnvelopeProto> = Vec::new();
    let is_live_fcc = live_fcc
        .and_then(|v| v.get("status"))
        .and_then(Value::as_str)
        == Some("live_fcc")
        && hub.is_enabled("fcc_broadband_bdc")
        && hub.is_module_enabled("utilities_broadband");

    if is_live_fcc {
        if let Some(arr) = live_fcc
            .and_then(|v| v.get("carriers").or_else(|| v.get("providers")))
            .and_then(Value::as_array)
        {
            let now = now_iso();
            for item in arr {
                let raw_name = item
                    .get("carrierName")
                    .or_else(|| item.get("providerName"))
                    .or_else(|| item.get("brand_name"))
                    .or_else(|| item.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if raw_name.is_empty() {
                    continue;
                }
                let tech_raw = item
                    .get("technology")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let dl = item
                    .get("maxDownloadMbps")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0);
                let ul = item
                    .get("maxUploadMbps")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0);
                let check_url = carrier_address_check_url(raw_name, fcc_map_url);

                let (technology, tier_category, symmetrical) =
                    if tech_raw.contains("fiber") || tech_raw.contains("fttp") || tech_raw == "50" {
                        (
                            "Fiber (FTTH)".to_string(),
                            "fiber".to_string(),
                            dl > 0 && ul > 0 && ul >= (dl * 8 / 10),
                        )
                    } else if tech_raw.contains("cable")
                        || tech_raw.contains("docsis")
                        || tech_raw == "40"
                    {
                        ("Cable (DOCSIS 3.1/4.0)".to_string(), "cable".to_string(), false)
                    } else if tech_raw.contains("satellite")
                        || raw_name.to_ascii_lowercase().contains("starlink")
                    {
                        ("LEO Satellite".to_string(), "satellite".to_string(), false)
                    } else if tech_raw.contains("dsl") || tech_raw.contains("copper") {
                        ("DSL / Copper".to_string(), "dsl".to_string(), false)
                    } else {
                        ("5G Fixed Wireless".to_string(), "wireless".to_string(), false)
                    };

                envelopes.push(ExtensionEnvelopeProto {
                    extension_id: "fcc_broadband_bdc".to_string(),
                    domain: CapabilityDomain::DomainBroadband as i32,
                    priority: 950,
                    geo_specificity: 500,
                    attribution: Some(SourceAttributionProto {
                        extension_id: "fcc_broadband_bdc".to_string(),
                        source_name:
                            "FCC National Broadband Map / Broadband Data Collection (BDC)"
                                .to_string(),
                        source_url: fcc_map_url.to_string(),
                        provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                        fetched_at_rfc3339: now.clone(),
                        schedule_or_version: Some("FCC BDC Location Filing".to_string()),
                        notes: None,
                    }),
                    latency_ms: 0,
                    cached: false,
                    payload: Some(Payload::BroadbandOption(BroadbandOptionProto {
                        carrier_name: raw_name.to_string(),
                        technology,
                        tier_category,
                        max_download_mbps: dl,
                        max_upload_mbps: ul,
                        symmetrical,
                        typical_monthly_usd: String::new(),
                        availability_status: "confirmed_fcc".to_string(),
                        check_url,
                    })),
                });
            }
        }
    }

    // When live FCC BDC is active, strictly preserve live parcel truth (never fabricate/fall back
    // to unverified regional footprint guesses when live FCC returned empty).
    if !is_live_fcc {
        envelopes.extend(hub.execute_domain_sync(CapabilityDomain::DomainBroadband, &query));
    }

    let bundle = aggregate_envelopes(&query, &envelopes, None);
    let mut quote = bundle
        .broadband_catalog
        .map(BroadbandCatalogQuote::from)
        .unwrap_or_else(|| BroadbandCatalogQuote {
            high_speed_available: false,
            fiber_available: false,
            fiber_confirmed_fcc: false,
            internet_tier: "unverified".to_string(),
            verification_status: if is_live_fcc {
                "fcc_location_verified".to_string()
            } else {
                "unverified".to_string()
            },
            headline_verdict: "Broadband Coverage Unverified — Check FCC Broadband Map".to_string(),
            max_download_mbps: 0,
            max_upload_mbps: 0,
            fiber_carriers: Vec::new(),
            cable_carriers: Vec::new(),
            wireless_carriers: Vec::new(),
            est_monthly_internet_usd: 0,
            fcc_map_url: fcc_map_url.to_string(),
        });

    if !fcc_map_url.trim().is_empty() {
        quote.fcc_map_url = fcc_map_url.to_string();
    }
    if inp.is_rural_area && !quote.fiber_confirmed_fcc {
        quote.high_speed_available = false;
        quote.fiber_available = false;
        if quote.internet_tier == "fiber_area" || quote.internet_tier == "cable_high_speed" {
            quote.internet_tier = "wireless_or_dsl".to_string();
        }
    }

    quote
}

fn carrier_address_check_url(name: &str, fcc_fallback: &str) -> String {
    let low = name.to_ascii_lowercase();
    if low.contains("at&t") || low.contains("att") {
        "https://www.att.com/internet/fiber/".to_string()
    } else if low.contains("google") {
        "https://fiber.google.com/".to_string()
    } else if low.contains("fios") || (low.contains("verizon") && !low.contains("5g")) {
        "https://www.verizon.com/home/internet/fios-fastest-internet/".to_string()
    } else if low.contains("frontier") {
        "https://frontier.com/shop/internet/fiber-internet".to_string()
    } else if low.contains("quantum") {
        "https://www.quantumfiber.com/".to_string()
    } else if low.contains("ziply") {
        "https://ziplyfiber.com/".to_string()
    } else if low.contains("xfinity") || low.contains("comcast") {
        "https://www.xfinity.com/learn/internet-service".to_string()
    } else if low.contains("spectrum") || low.contains("charter") {
        "https://www.spectrum.com/internet".to_string()
    } else if low.contains("cox") {
        "https://www.cox.com/residential/internet.html".to_string()
    } else if low.contains("optimum") || low.contains("altice") {
        "https://www.optimum.com/internet".to_string()
    } else if low.contains("t-mobile") {
        "https://www.t-mobile.com/home-internet".to_string()
    } else if low.contains("starlink") {
        "https://www.starlink.com/".to_string()
    } else {
        fcc_fallback.to_string()
    }
}
