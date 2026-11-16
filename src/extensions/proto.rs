//! Generated Protocol Buffers (`openestate.extension.v1`) types, binary wire encoding/decoding,
//! and canonical JSON bridge for Open Estate's modular extension system.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
use prost::Message;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

pub mod v1 {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/openestate.extension.v1.rs"));
}

pub use v1::*;

/// Encode any generated Protobuf message into binary wire format (`prost::Message::encode_to_vec`).
pub fn encode_proto_bytes<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_to_vec()
}

/// Decode binary Protobuf wire bytes into a generated message (`prost::Message::decode`).
pub fn decode_proto_bytes<M: Message + Default>(bytes: &[u8]) -> Result<M> {
    M::decode(bytes).context("Failed to decode binary Protobuf message")
}

/// Encode a generated Protobuf message as standard Base64 over its binary wire representation.
pub fn encode_proto_base64<M: Message>(msg: &M) -> String {
    BASE64_STD.encode(msg.encode_to_vec())
}

/// Decode a Base64-encoded binary Protobuf payload.
pub fn decode_proto_base64<M: Message + Default>(encoded: &str) -> Result<M> {
    let raw = BASE64_STD
        .decode(encoded.trim())
        .context("Invalid Base64 in Protobuf payload")?;
    decode_proto_bytes(&raw)
}

/// Serialize a Protobuf message to canonical camelCase JSON (`serde_json::Value`).
pub fn proto_to_canonical_json<T: Serialize>(msg: &T) -> Result<Value> {
    serde_json::to_value(msg).context("Failed to serialize Protobuf message to JSON")
}

/// Deserialize a Protobuf message from canonical JSON (`serde_json::Value`).
pub fn proto_from_canonical_json<T: DeserializeOwned>(val: &Value) -> Result<T> {
    serde_json::from_value(val.clone()).context("Failed to deserialize Protobuf message from JSON")
}

/// Parse a capability domain from either snake_case (`"electric_utility"`) or Protobuf enum name (`"DOMAIN_ELECTRIC_UTILITY"`).
pub fn parse_capability_domain(raw: &str) -> Option<CapabilityDomain> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("DOMAIN_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" | "" => Some(CapabilityDomain::DomainUnspecified),
        "GEOCODE" | "GEOCODING" => Some(CapabilityDomain::DomainGeocode),
        "PROPERTY_FACTS" | "FACTS" | "PROPERTY" => Some(CapabilityDomain::DomainPropertyFacts),
        "VALUATION" | "AVM" => Some(CapabilityDomain::DomainValuation),
        "MARKET_TRENDS" | "MARKET" => Some(CapabilityDomain::DomainMarketTrends),
        "ELECTRIC_UTILITY" | "ELECTRIC" | "ELECTRICITY" => {
            Some(CapabilityDomain::DomainElectricUtility)
        }
        "GAS_UTILITY" | "GAS" | "NATURAL_GAS" => Some(CapabilityDomain::DomainGasUtility),
        "WATER_SEWER_TRASH" | "WATER" | "MUNICIPAL_WATER" => {
            Some(CapabilityDomain::DomainWaterSewerTrash)
        }
        "BROADBAND" | "FIBER" | "INTERNET" => Some(CapabilityDomain::DomainBroadband),
        "NATURAL_HAZARDS" | "HAZARDS" => Some(CapabilityDomain::DomainNaturalHazards),
        "ENVIRONMENTAL" | "ENVIRONMENT" | "ENVIRONMENTAL_RISK" => {
            Some(CapabilityDomain::DomainEnvironmental)
        }
        "LIVABILITY" | "NEIGHBORHOOD" | "AMENITIES" => Some(CapabilityDomain::DomainLivability),
        "SCHOOLS" | "EDUCATION" => Some(CapabilityDomain::DomainSchools),
        "CRIME" | "SAFETY" => Some(CapabilityDomain::DomainCrime),
        "HOUSE_3D" | "LIDAR" | "HOUSE3D" => Some(CapabilityDomain::DomainHouse3d),
        "LISTINGS_FEED" | "LISTINGS" | "MLS_FEED" => Some(CapabilityDomain::DomainListingsFeed),
        "INSURANCE" | "HOME_INSURANCE" => Some(CapabilityDomain::DomainInsurance),
        "TAX_MANDATES" | "TAX" | "MANDATES" => Some(CapabilityDomain::DomainTaxMandates),
        "ROUTING" | "DRIVE_ROUTING" | "ROUTING_MATRIX" => Some(CapabilityDomain::DomainRouting),
        "PERMITS" | "BUILDING_PERMITS" | "PERMIT_HISTORY" => {
            Some(CapabilityDomain::DomainPermits)
        }
        "PERMIT_INTEL" | "PERMIT_EXTRACTORS" | "PROPERTY_SYSTEMS" => {
            Some(CapabilityDomain::DomainPermitIntel)
        }
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `CapabilityDomain`.
pub fn capability_domain_slug(domain: CapabilityDomain) -> &'static str {
    match domain {
        CapabilityDomain::DomainUnspecified => "unspecified",
        CapabilityDomain::DomainGeocode => "geocode",
        CapabilityDomain::DomainPropertyFacts => "property_facts",
        CapabilityDomain::DomainValuation => "valuation",
        CapabilityDomain::DomainMarketTrends => "market_trends",
        CapabilityDomain::DomainElectricUtility => "electric_utility",
        CapabilityDomain::DomainGasUtility => "gas_utility",
        CapabilityDomain::DomainWaterSewerTrash => "water_sewer_trash",
        CapabilityDomain::DomainBroadband => "broadband",
        CapabilityDomain::DomainNaturalHazards => "natural_hazards",
        CapabilityDomain::DomainEnvironmental => "environmental",
        CapabilityDomain::DomainLivability => "livability",
        CapabilityDomain::DomainSchools => "schools",
        CapabilityDomain::DomainCrime => "crime",
        CapabilityDomain::DomainHouse3d => "house_3d",
        CapabilityDomain::DomainListingsFeed => "listings_feed",
        CapabilityDomain::DomainInsurance => "insurance",
        CapabilityDomain::DomainTaxMandates => "tax_mandates",
        CapabilityDomain::DomainRouting => "routing",
        CapabilityDomain::DomainPermits => "permits",
        CapabilityDomain::DomainPermitIntel => "permit_intel",
    }
}

/// Parse an extension driver kind from snake_case or Protobuf enum string.
pub fn parse_driver_kind(raw: &str) -> Option<DriverKind> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("DRIVER_").unwrap_or(&norm);
    match stripped {
        "DECLARATIVE_YAML" | "YAML" | "DECLARATIVE" => Some(DriverKind::DriverDeclarativeYaml),
        "OPENAPI_YAML" | "OPENAPI" | "OPENAPI_3" => Some(DriverKind::DriverOpenapiYaml),
        "NATIVE_ADAPTER" | "NATIVE" | "RUST" => Some(DriverKind::DriverNativeAdapter),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `DriverKind`.
pub fn driver_kind_slug(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::DriverUnspecified => "unspecified",
        DriverKind::DriverDeclarativeYaml => "declarative_yaml",
        DriverKind::DriverOpenapiYaml => "openapi_yaml",
        DriverKind::DriverNativeAdapter => "native_adapter",
    }
}

/// Human-readable label for a `DriverKind`.
pub fn driver_kind_label(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::DriverUnspecified => "Unspecified",
        DriverKind::DriverDeclarativeYaml => "Declarative YAML",
        DriverKind::DriverOpenapiYaml => "OpenAPI 3.x",
        DriverKind::DriverNativeAdapter => "Native Rust Adapter",
    }
}

/// Parse a `ProvenanceTier` from snake_case or Protobuf enum string.
pub fn parse_provenance_tier(raw: &str) -> Option<ProvenanceTier> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("PROVENANCE_").unwrap_or(&norm);
    match stripped {
        "LIVE_VERIFIED" | "LIVE" => Some(ProvenanceTier::ProvenanceLiveVerified),
        "OFFICIAL_TARIFF" | "TARIFF" | "PUBLISHED_TARIFF" => {
            Some(ProvenanceTier::ProvenanceOfficialTariff)
        }
        "REGIONAL_FOOTPRINT" | "FOOTPRINT" | "METRO_FOOTPRINT" => {
            Some(ProvenanceTier::ProvenanceRegionalFootprint)
        }
        "UNVERIFIED" | "STATIC_HINT" | "HINT" => Some(ProvenanceTier::ProvenanceUnverified),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `ProvenanceTier`.
pub fn provenance_tier_slug(tier: ProvenanceTier) -> &'static str {
    match tier {
        ProvenanceTier::ProvenanceUnspecified => "unspecified",
        ProvenanceTier::ProvenanceLiveVerified => "live_verified",
        ProvenanceTier::ProvenanceOfficialTariff => "official_tariff",
        ProvenanceTier::ProvenanceRegionalFootprint => "regional_footprint",
        ProvenanceTier::ProvenanceUnverified => "unverified",
    }
}

/// Deterministic numeric rank for provenance tiers (`LIVE_VERIFIED` > `OFFICIAL_TARIFF` > `REGIONAL_FOOTPRINT` > `UNVERIFIED`).
pub fn provenance_rank(tier: ProvenanceTier) -> i32 {
    match tier {
        ProvenanceTier::ProvenanceLiveVerified => 30,
        ProvenanceTier::ProvenanceOfficialTariff => 20,
        ProvenanceTier::ProvenanceRegionalFootprint => 10,
        ProvenanceTier::ProvenanceUnverified => 5,
        ProvenanceTier::ProvenanceUnspecified => 0,
    }
}

/// Default fully-qualified Protobuf message contract for a `CapabilityDomain`.
pub fn default_proto_message_for_domain(domain: CapabilityDomain) -> &'static str {
    match domain {
        CapabilityDomain::DomainUnspecified => "openestate.extension.v1.ExtensionEnvelopeProto",
        CapabilityDomain::DomainGeocode => "openestate.extension.v1.GeocodeResultProto",
        CapabilityDomain::DomainPropertyFacts => "openestate.extension.v1.PropertyFactsProto",
        CapabilityDomain::DomainValuation | CapabilityDomain::DomainMarketTrends => {
            "openestate.extension.v1.MarketValuationProto"
        }
        CapabilityDomain::DomainElectricUtility => "openestate.extension.v1.ElectricUtilityProto",
        CapabilityDomain::DomainGasUtility => "openestate.extension.v1.GasUtilityProto",
        CapabilityDomain::DomainWaterSewerTrash => "openestate.extension.v1.WaterSewerTrashProto",
        CapabilityDomain::DomainBroadband => "openestate.extension.v1.BroadbandCatalogProto",
        CapabilityDomain::DomainNaturalHazards => "openestate.extension.v1.HazardSummaryProto",
        CapabilityDomain::DomainEnvironmental => {
            "openestate.extension.v1.EnvironmentalSummaryProto"
        }
        CapabilityDomain::DomainLivability
        | CapabilityDomain::DomainSchools
        | CapabilityDomain::DomainCrime => "openestate.extension.v1.LivabilitySummaryProto",
        CapabilityDomain::DomainHouse3d => "openestate.extension.v1.House3dMetadataProto",
        CapabilityDomain::DomainListingsFeed => "openestate.extension.v1.ListingsFeedBatchProto",
        CapabilityDomain::DomainInsurance => "openestate.extension.v1.InsuranceQuoteProto",
        CapabilityDomain::DomainTaxMandates => "openestate.extension.v1.TaxMandatesProto",
        CapabilityDomain::DomainRouting => "openestate.extension.v1.RoutingMatrixProto",
        CapabilityDomain::DomainPermits => "openestate.extension.v1.PermitsHistoryProto",
        CapabilityDomain::DomainPermitIntel => "openestate.extension.v1.RoofIntelProto",
    }
}

/// Validates that `proto_message_type` names a real `openestate.extension.v1.*` contract compatible with `domain`.
pub fn validate_proto_message_type(domain: CapabilityDomain, proto_message_type: &str) -> Result<()> {
    let trimmed = proto_message_type.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("proto_message_type cannot be empty"));
    }
    let short = trimmed
        .strip_prefix("openestate.extension.v1.")
        .unwrap_or(trimmed);
    let valid_known = matches!(
        short,
        "GeocodeResultProto"
            | "ElectricUtilityProto"
            | "GasUtilityProto"
            | "WaterSewerTrashProto"
            | "BroadbandOptionProto"
            | "BroadbandCatalogProto"
            | "PropertyFactsProto"
            | "HazardSummaryProto"
            | "EnvironmentalSummaryProto"
            | "LivabilitySummaryProto"
            | "SchoolSummaryItemProto"
            | "MarketValuationProto"
            | "InsuranceQuoteProto"
            | "TaxMandatesProto"
            | "House3dMetadataProto"
            | "ListingsFeedBatchProto"
            | "ListingFeedItemProto"
            | "ListingShoppingFacetsProto"
            | "RoutingMatrixProto"
            | "PermitsHistoryProto"
            | "PermitRecordProto"
            | "RoofIntelProto"
            | "SolarIntelProto"
            | "MepIntelProto"
            | "RemodelIntelProto"
            | "CodeEnforcementIntelProto"
            | "ExtensionEnvelopeProto"
    );
    if !valid_known {
        return Err(anyhow!(
            "Unknown Protobuf contract '{proto_message_type}'; expected a openestate.extension.v1.*Proto type"
        ));
    }
    let compatible = match domain {
        CapabilityDomain::DomainUnspecified => false,
        CapabilityDomain::DomainGeocode => {
            matches!(short, "GeocodeResultProto" | "PropertyFactsProto")
        }
        CapabilityDomain::DomainPropertyFacts => {
            matches!(short, "PropertyFactsProto" | "ListingShoppingFacetsProto")
        }
        CapabilityDomain::DomainValuation | CapabilityDomain::DomainMarketTrends => {
            matches!(short, "MarketValuationProto" | "PropertyFactsProto")
        }
        CapabilityDomain::DomainElectricUtility => matches!(short, "ElectricUtilityProto"),
        CapabilityDomain::DomainGasUtility => matches!(short, "GasUtilityProto"),
        CapabilityDomain::DomainWaterSewerTrash => {
            matches!(short, "WaterSewerTrashProto" | "EnvironmentalSummaryProto")
        }
        CapabilityDomain::DomainBroadband => {
            matches!(short, "BroadbandCatalogProto" | "BroadbandOptionProto")
        }
        CapabilityDomain::DomainNaturalHazards => matches!(short, "HazardSummaryProto"),
        CapabilityDomain::DomainEnvironmental => {
            matches!(short, "EnvironmentalSummaryProto" | "WaterSewerTrashProto")
        }
        CapabilityDomain::DomainLivability
        | CapabilityDomain::DomainSchools
        | CapabilityDomain::DomainCrime => {
            matches!(short, "LivabilitySummaryProto" | "SchoolSummaryItemProto")
        }
        CapabilityDomain::DomainHouse3d => matches!(short, "House3dMetadataProto"),
        CapabilityDomain::DomainListingsFeed => {
            matches!(short, "ListingsFeedBatchProto" | "ListingFeedItemProto")
        }
        CapabilityDomain::DomainInsurance => matches!(short, "InsuranceQuoteProto"),
        CapabilityDomain::DomainTaxMandates => matches!(short, "TaxMandatesProto"),
        CapabilityDomain::DomainRouting => matches!(short, "RoutingMatrixProto"),
        CapabilityDomain::DomainPermits => {
            matches!(short, "PermitsHistoryProto" | "PermitRecordProto")
        }
        CapabilityDomain::DomainPermitIntel => matches!(
            short,
            "RoofIntelProto"
                | "SolarIntelProto"
                | "MepIntelProto"
                | "RemodelIntelProto"
                | "CodeEnforcementIntelProto"
        ),
    };
    if !compatible {
        return Err(anyhow!(
            "Protobuf contract '{}' is not compatible with domain '{}'",
            proto_message_type,
            capability_domain_slug(domain)
        ));
    }
    Ok(())
}

/// Convert an `ExtensionEnvelopeProto` into an enriched Admin/API JSON object including
/// readable enum slugs alongside the canonical payload and binary wire size.
pub fn envelope_to_display_json(env: &ExtensionEnvelopeProto) -> Value {
    let domain = CapabilityDomain::try_from(env.domain).unwrap_or(CapabilityDomain::DomainUnspecified);
    let prov = env
        .attribution
        .as_ref()
        .and_then(|a| ProvenanceTier::try_from(a.provenance_tier).ok())
        .unwrap_or(ProvenanceTier::ProvenanceUnspecified);
    let wire_bytes = env.encode_to_vec();
    let base = serde_json::to_value(env).unwrap_or_else(|_| json!({}));
    json!({
        "extensionId": env.extension_id,
        "domain": capability_domain_slug(domain),
        "domainEnum": domain.as_str_name(),
        "priority": env.priority,
        "geoSpecificity": env.geo_specificity,
        "provenance": provenance_tier_slug(prov),
        "provenanceEnum": prov.as_str_name(),
        "latencyMs": env.latency_ms,
        "cached": env.cached,
        "protobufWireBytes": wire_bytes.len(),
        "envelope": base,
    })
}
