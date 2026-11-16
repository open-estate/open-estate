//! Generated Protocol Buffers (`openestate.extension.v1`, `openestate.scheduler.v1`,
//! `openestate.shared.v1`) types, descriptor reflection, binary wire encoding/decoding,
//! and canonical JSON bridge for Open Estate's modular extension system.

use crate::metering::{ActiveLlmSpec, ActiveLlmTaskSpec};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
use once_cell::sync::Lazy;
use prost::Message;
use prost_types::FileDescriptorSet;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

pub mod v1 {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/openestate.extension.v1.rs"));
}

pub mod openestate {
    pub mod extension {
        pub use crate::extensions::proto::v1;
    }
    pub mod scheduler {
        pub mod v1 {
            #![allow(clippy::all)]
            include!(concat!(env!("OUT_DIR"), "/openestate.scheduler.v1.rs"));
        }
    }
    pub mod shared {
        pub mod v1 {
            #![allow(clippy::all)]
            include!(concat!(env!("OUT_DIR"), "/openestate.shared.v1.rs"));
        }
    }
}

pub use openestate::scheduler::v1 as scheduler_v1;
pub use openestate::shared::v1 as shared_v1;
pub use v1::*;

/// Compiled `google.protobuf.FileDescriptorSet` emitted by `prost-build` (`extension.descriptor.bin`).
pub const EXTENSION_DESCRIPTOR_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/extension.descriptor.bin"));

static EXTENSION_DESCRIPTOR_SET: Lazy<FileDescriptorSet> = Lazy::new(|| {
    FileDescriptorSet::decode(EXTENSION_DESCRIPTOR_BYTES)
        .expect("embedded extension.descriptor.bin must decode as FileDescriptorSet")
});

/// Returns the decoded `FileDescriptorSet` for all Open Estate extension/metering/scheduler/shared protos.
pub fn extension_descriptor_set() -> &'static FileDescriptorSet {
    &EXTENSION_DESCRIPTOR_SET
}

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
        "PERMITS" | "BUILDING_PERMITS" | "PERMIT_HISTORY" => Some(CapabilityDomain::DomainPermits),
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

/// Parse `ExtensionTier` from snake_case (`"basic"`, `"premium"`) or Protobuf enum string.
pub fn parse_extension_tier(raw: &str) -> Option<ExtensionTier> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("EXTENSION_TIER_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(ExtensionTier::Unspecified),
        "BASIC" => Some(ExtensionTier::Basic),
        "PREMIUM" => Some(ExtensionTier::Premium),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for an `ExtensionTier`.
pub fn extension_tier_slug(tier: ExtensionTier) -> &'static str {
    match tier {
        ExtensionTier::Unspecified => "unspecified",
        ExtensionTier::Basic => "basic",
        ExtensionTier::Premium => "premium",
    }
}

/// Parse `LicenseClass` from snake_case or Protobuf enum string.
pub fn parse_license_class(raw: &str) -> Option<LicenseClass> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("LICENSE_CLASS_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(LicenseClass::Unspecified),
        "PUBLIC_OPEN_DATA" => Some(LicenseClass::PublicOpenData),
        "BYOK_COMMERCIAL_API" => Some(LicenseClass::ByokCommercialApi),
        "NON_OPEN_SCRAPE" => Some(LicenseClass::NonOpenScrape),
        "HUMAN_IN_LOOP" => Some(LicenseClass::HumanInLoop),
        "LLM_SERVICE" => Some(LicenseClass::LlmService),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `LicenseClass`.
pub fn license_class_slug(lc: LicenseClass) -> &'static str {
    match lc {
        LicenseClass::Unspecified => "unspecified",
        LicenseClass::PublicOpenData => "public_open_data",
        LicenseClass::ByokCommercialApi => "byok_commercial_api",
        LicenseClass::NonOpenScrape => "non_open_scrape",
        LicenseClass::HumanInLoop => "human_in_loop",
        LicenseClass::LlmService => "llm_service",
    }
}

/// Parse `ExecutionMode` from snake_case (`"eager"`, `"lazy"`) or Protobuf enum string.
pub fn parse_execution_mode(raw: &str) -> Option<ExecutionMode> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("EXECUTION_MODE_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(ExecutionMode::Unspecified),
        "EAGER" => Some(ExecutionMode::Eager),
        "LAZY" => Some(ExecutionMode::Lazy),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for an `ExecutionMode`.
pub fn execution_mode_slug(em: ExecutionMode) -> &'static str {
    match em {
        ExecutionMode::Unspecified => "unspecified",
        ExecutionMode::Eager => "eager",
        ExecutionMode::Lazy => "lazy",
    }
}

/// Parse `ReasoningEffort` (`none|minimal|low|medium|high|xhigh|max`).
pub fn parse_reasoning_effort(raw: &str) -> Option<ReasoningEffort> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("REASONING_EFFORT_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(ReasoningEffort::Unspecified),
        "NONE" | "OFF" => Some(ReasoningEffort::None),
        "MINIMAL" => Some(ReasoningEffort::Minimal),
        "LOW" => Some(ReasoningEffort::Low),
        "MEDIUM" => Some(ReasoningEffort::Medium),
        "HIGH" => Some(ReasoningEffort::High),
        "XHIGH" => Some(ReasoningEffort::Xhigh),
        "MAX" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `ReasoningEffort`.
pub fn reasoning_effort_slug(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Unspecified => "unspecified",
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

/// Parse `ShareScope` from snake_case (`parcel|building|hoa|subdivision|geohash7|geohash6|geohash5|census_block_group|census_tract|zip|city|county|state|utility_territory|global`).
pub fn parse_share_scope(raw: &str) -> Option<ShareScope> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("SHARE_SCOPE_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(ShareScope::Unspecified),
        "PARCEL" => Some(ShareScope::Parcel),
        "BUILDING" => Some(ShareScope::Building),
        "HOA" => Some(ShareScope::Hoa),
        "SUBDIVISION" => Some(ShareScope::Subdivision),
        "GEOHASH7" | "GEOHASH_7" => Some(ShareScope::Geohash7),
        "GEOHASH6" | "GEOHASH_6" => Some(ShareScope::Geohash6),
        "GEOHASH5" | "GEOHASH_5" => Some(ShareScope::Geohash5),
        "CENSUS_BLOCK_GROUP" | "BLOCK_GROUP" => Some(ShareScope::CensusBlockGroup),
        "CENSUS_TRACT" | "TRACT" => Some(ShareScope::CensusTract),
        "ZIP" | "ZIP_CODE" => Some(ShareScope::Zip),
        "CITY" => Some(ShareScope::City),
        "COUNTY" => Some(ShareScope::County),
        "STATE" => Some(ShareScope::State),
        "UTILITY_TERRITORY" => Some(ShareScope::UtilityTerritory),
        "GLOBAL" => Some(ShareScope::Global),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `ShareScope`.
pub fn share_scope_slug(scope: ShareScope) -> &'static str {
    match scope {
        ShareScope::Unspecified => "unspecified",
        ShareScope::Parcel => "parcel",
        ShareScope::Building => "building",
        ShareScope::Hoa => "hoa",
        ShareScope::Subdivision => "subdivision",
        ShareScope::Geohash7 => "geohash7",
        ShareScope::Geohash6 => "geohash6",
        ShareScope::Geohash5 => "geohash5",
        ShareScope::CensusBlockGroup => "census_block_group",
        ShareScope::CensusTract => "census_tract",
        ShareScope::Zip => "zip",
        ShareScope::City => "city",
        ShareScope::County => "county",
        ShareScope::State => "state",
        ShareScope::UtilityTerritory => "utility_territory",
        ShareScope::Global => "global",
    }
}

/// Parse `LazySectionStatusKind` from snake_case or Protobuf enum string.
pub fn parse_lazy_section_status_kind(raw: &str) -> Option<LazySectionStatusKind> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm
        .strip_prefix("LAZY_SECTION_STATUS_KIND_")
        .unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(LazySectionStatusKind::Unspecified),
        "PENDING" => Some(LazySectionStatusKind::Pending),
        "AWAITING_EXTERNAL_RESPONSE" => Some(LazySectionStatusKind::AwaitingExternalResponse),
        "AWAITING_APPROVAL" => Some(LazySectionStatusKind::AwaitingApproval),
        "NOT_ENABLED" => Some(LazySectionStatusKind::NotEnabled),
        "FAILED" => Some(LazySectionStatusKind::Failed),
        "READY" => Some(LazySectionStatusKind::Ready),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for a `LazySectionStatusKind`.
pub fn lazy_section_status_kind_slug(kind: LazySectionStatusKind) -> &'static str {
    match kind {
        LazySectionStatusKind::Unspecified => "unspecified",
        LazySectionStatusKind::Pending => "pending",
        LazySectionStatusKind::AwaitingExternalResponse => "awaiting_external_response",
        LazySectionStatusKind::AwaitingApproval => "awaiting_approval",
        LazySectionStatusKind::NotEnabled => "not_enabled",
        LazySectionStatusKind::Failed => "failed",
        LazySectionStatusKind::Ready => "ready",
    }
}

/// Parse `InvocationTrigger` from snake_case or Protobuf enum string.
pub fn parse_invocation_trigger(raw: &str) -> Option<InvocationTrigger> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("INVOCATION_TRIGGER_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(InvocationTrigger::Unspecified),
        "USER_REQUEST" => Some(InvocationTrigger::UserRequest),
        "PERIODIC_SCAN" | "SCAN" => Some(InvocationTrigger::PeriodicScan),
        "LAZY_JOB" | "LAZY" => Some(InvocationTrigger::LazyJob),
        "ADMIN_TEST" | "TEST_FETCH" => Some(InvocationTrigger::AdminTest),
        "DEPENDENCY" | "DEP" => Some(InvocationTrigger::Dependency),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for an `InvocationTrigger`.
pub fn invocation_trigger_slug(trigger: InvocationTrigger) -> &'static str {
    match trigger {
        InvocationTrigger::Unspecified => "unspecified",
        InvocationTrigger::UserRequest => "user_request",
        InvocationTrigger::PeriodicScan => "periodic_scan",
        InvocationTrigger::LazyJob => "lazy_job",
        InvocationTrigger::AdminTest => "admin_test",
        InvocationTrigger::Dependency => "dependency",
    }
}

/// Parse `InvocationOutcome` from snake_case or Protobuf enum string.
pub fn parse_invocation_outcome(raw: &str) -> Option<InvocationOutcome> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("INVOCATION_OUTCOME_").unwrap_or(&norm);
    match stripped {
        "UNSPECIFIED" => Some(InvocationOutcome::Unspecified),
        "OK" => Some(InvocationOutcome::Ok),
        "FAILED" | "ERROR" => Some(InvocationOutcome::Failed),
        "CACHED" | "CACHE_HIT" => Some(InvocationOutcome::Cached),
        "DENIED" => Some(InvocationOutcome::Denied),
        _ => None,
    }
}

/// Canonical lowercase snake_case slug for an `InvocationOutcome`.
pub fn invocation_outcome_slug(outcome: InvocationOutcome) -> &'static str {
    match outcome {
        InvocationOutcome::Unspecified => "unspecified",
        InvocationOutcome::Ok => "ok",
        InvocationOutcome::Failed => "failed",
        InvocationOutcome::Cached => "cached",
        InvocationOutcome::Denied => "denied",
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

fn descriptor_has_message(pkg: &str, short_name: &str) -> bool {
    extension_descriptor_set().file.iter().any(|file| {
        file.package.as_deref() == Some(pkg)
            && file
                .message_type
                .iter()
                .any(|m| m.name.as_deref() == Some(short_name))
    })
}

/// Validates that `proto_message_type` names a real `openestate.extension.v1.*` contract
/// present in `extension.descriptor.bin` and compatible with `domain`.
pub fn validate_proto_message_type(
    domain: CapabilityDomain,
    proto_message_type: &str,
) -> Result<()> {
    let trimmed = proto_message_type.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("proto_message_type cannot be empty"));
    }
    let short = trimmed
        .strip_prefix("openestate.extension.v1.")
        .unwrap_or(trimmed);
    if !descriptor_has_message("openestate.extension.v1", short) {
        return Err(anyhow!(
            "Unknown Protobuf contract '{proto_message_type}'; expected a defined openestate.extension.v1.*Proto message in extension.descriptor.bin"
        ));
    }
    let compatible = match domain {
        CapabilityDomain::DomainUnspecified => false,
        CapabilityDomain::DomainGeocode => {
            matches!(short, "GeocodeResultProto" | "PropertyFactsProto")
        }
        CapabilityDomain::DomainPropertyFacts => {
            matches!(
                short,
                "PropertyFactsProto"
                    | "ListingShoppingFacetsProto"
                    | "DisclosurePacketProto"
                    | "DisclosureFileProto"
                    | "DocumentForensicsVerdictProto"
            )
        }
        CapabilityDomain::DomainValuation | CapabilityDomain::DomainMarketTrends => {
            matches!(
                short,
                "MarketValuationProto"
                    | "PropertyFactsProto"
                    | "RentalListingProto"
                    | "RentalMarketCellProto"
            )
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
            matches!(
                short,
                "ListingsFeedBatchProto"
                    | "ListingFeedItemProto"
                    | "DisclosurePacketProto"
                    | "RentalListingProto"
            )
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
    let domain =
        CapabilityDomain::try_from(env.domain).unwrap_or(CapabilityDomain::DomainUnspecified);
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

/// Typed `ExtensionTier` for `manifest` (`ExtensionTier::Basic` when unset).
pub fn manifest_tier_enum(manifest: &ExtensionManifestProto) -> ExtensionTier {
    match ExtensionTier::try_from(manifest.tier) {
        Ok(ExtensionTier::Unspecified) | Err(_) => ExtensionTier::Basic,
        Ok(t) => t,
    }
}

/// Canonical monetization tier slug for `manifest` (`"basic"` when unset).
pub fn manifest_tier(manifest: &ExtensionManifestProto) -> &'static str {
    extension_tier_slug(manifest_tier_enum(manifest))
}

/// Typed `LicenseClass` for `manifest` (`LicenseClass::PublicOpenData` when unset).
pub fn manifest_license_class_enum(manifest: &ExtensionManifestProto) -> LicenseClass {
    match LicenseClass::try_from(manifest.license_class) {
        Ok(LicenseClass::Unspecified) | Err(_) => LicenseClass::PublicOpenData,
        Ok(lc) => lc,
    }
}

/// Canonical license classification slug for `manifest` (`"public_open_data"` when unset).
pub fn manifest_license_class(manifest: &ExtensionManifestProto) -> &'static str {
    license_class_slug(manifest_license_class_enum(manifest))
}

/// Typed `ExecutionMode` for `manifest` (`ExecutionMode::Eager` when unset).
pub fn manifest_execution_mode_enum(manifest: &ExtensionManifestProto) -> ExecutionMode {
    match ExecutionMode::try_from(manifest.execution_mode) {
        Ok(ExecutionMode::Unspecified) | Err(_) => ExecutionMode::Eager,
        Ok(em) => em,
    }
}

/// Canonical scheduling mode slug for `manifest` (`"eager"` when unset).
pub fn manifest_execution_mode(manifest: &ExtensionManifestProto) -> &'static str {
    execution_mode_slug(manifest_execution_mode_enum(manifest))
}

/// Returns true when `manifest` is scheduled lazily rather than during eager waves.
pub fn manifest_is_lazy(manifest: &ExtensionManifestProto) -> bool {
    manifest_execution_mode_enum(manifest) == ExecutionMode::Lazy
}

/// Returns true when `manifest` requires a commercial/premium entitlement tier.
pub fn manifest_is_premium(manifest: &ExtensionManifestProto) -> bool {
    manifest_tier_enum(manifest) == ExtensionTier::Premium
}

/// Effective default-enabled flag (`enabled_by_default` when set, otherwise `enabled`).
pub fn manifest_enabled_by_default(manifest: &ExtensionManifestProto) -> bool {
    manifest.enabled_by_default.unwrap_or(manifest.enabled)
}

/// Returns normalized `min_plan_tier` if configured on `manifest`.
pub fn manifest_min_plan_tier(manifest: &ExtensionManifestProto) -> Option<&str> {
    let s = manifest.min_plan_tier.trim();
    (!s.is_empty()).then_some(s)
}

/// Returns the target `section_id` declared on `manifest`, or empty string when unset.
pub fn manifest_section_id(manifest: &ExtensionManifestProto) -> &str {
    manifest.section_id.trim()
}

/// Returns the manifest's `LlmSpecProto` if configured.
pub fn manifest_llm_spec(manifest: &ExtensionManifestProto) -> Option<&LlmSpecProto> {
    manifest.llm.as_ref()
}

/// Returns the manifest's `SharingSpecProto` if configured.
pub fn manifest_sharing_spec(manifest: &ExtensionManifestProto) -> Option<&SharingSpecProto> {
    manifest.sharing.as_ref()
}

/// Converts `manifest.llm` into an `open_estate::metering::ActiveLlmSpec` ready for `MeterScope`.
pub fn manifest_to_active_llm_spec(manifest: &ExtensionManifestProto) -> Option<ActiveLlmSpec> {
    let llm = manifest.llm.as_ref()?;
    let effort_opt = ReasoningEffort::try_from(llm.effort)
        .ok()
        .filter(|e| *e != ReasoningEffort::Unspecified)
        .map(|e| reasoning_effort_slug(e).to_string());
    let tasks = llm
        .tasks
        .iter()
        .map(|t| {
            let t_eff = ReasoningEffort::try_from(t.effort)
                .ok()
                .filter(|e| *e != ReasoningEffort::Unspecified)
                .map(|e| reasoning_effort_slug(e).to_string());
            ActiveLlmTaskSpec {
                task: t.task.trim().to_string(),
                profile: (!t.profile.trim().is_empty()).then(|| t.profile.trim().to_string()),
                model: t
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                effort: t_eff,
                max_output_tokens: t.max_output_tokens,
            }
        })
        .collect();
    Some(ActiveLlmSpec {
        extension_id: manifest.extension_id.clone(),
        profile: (!llm.profile.trim().is_empty()).then(|| llm.profile.trim().to_string()),
        model: llm
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        effort: effort_opt,
        max_output_tokens: llm.max_output_tokens,
        tasks,
    })
}

/// Returns configured metering dimensions, defaulting to `["invocations", "wall_time_ms"]`.
pub fn manifest_metering_dimensions(manifest: &ExtensionManifestProto) -> Vec<String> {
    if let Some(ref m) = manifest.metering {
        let dims: Vec<String> = m
            .dimensions
            .iter()
            .map(|d| d.trim().to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if !dims.is_empty() {
            return dims;
        }
    }
    vec!["invocations".to_string(), "wall_time_ms".to_string()]
}
