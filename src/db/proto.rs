//! Canonical Protocol Buffers (`openestate.db.v1`) storage contracts, integer enum codecs,
//! bitmask codecs (Rapid-safe signed `i64`), binary SHA-256 helpers, and lossless domain
//! conversions (`HomeFactors`, `Profile`, `SearchFilters`, `ListingRow`).

use crate::scoring::catalog::{
    style_families_for_value, Dimension, Feature, LoanType, PropertyKind, SchoolLevel, Screen,
    StyleFamily, StylePreference,
};
use crate::scoring::factors::{
    Adu, CostFactors, Factor, GateInputs, Grade, HomeFactors, HomeFitFactors, LoanMonthlyCosts,
    SchoolFactors, SchoolMetric, SchoolScore, StyleClue, StyleEvidence, StyleSource,
    FACTORS_VERSION,
};
use crate::scoring::profile::{Budget, Place, Profile, TravelMode};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, SecondsFormat, TimeZone, Utc};
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

include!(concat!(env!("OUT_DIR"), "/openestate.db.v1.rs"));

// ---------------------------------------------------------------------------------------------
// Generic `prost` Wire Helpers & SHA-256 `BINARY(32)` Keys
// ---------------------------------------------------------------------------------------------

pub fn encode_proto_bytes<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_to_vec()
}

pub fn decode_proto_bytes<M: Message + Default>(bytes: &[u8]) -> Result<M> {
    M::decode(bytes).context("Failed to decode openestate.db.v1 Protobuf wire bytes")
}

pub fn sha256_bytes32(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    out
}

pub fn hex_to_bytes32(hex_str: &str) -> Option<[u8; 32]> {
    let trimmed = hex_str.trim();
    if trimmed.len() == 64 {
        let mut out = [0_u8; 32];
        if hex::decode_to_slice(trimmed, &mut out).is_ok() {
            return Some(out);
        }
    }
    None
}

pub fn hash_or_decode_key32(raw_key: &str) -> [u8; 32] {
    hex_to_bytes32(raw_key).unwrap_or_else(|| sha256_bytes32(raw_key.trim().as_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Timestamp (`BIGINT` unix seconds / ms) & Epoch-Day (`INT` days since 1970-01-01) Codecs
// ---------------------------------------------------------------------------------------------

const UNIX_EPOCH_CE_DAYS: i32 = 719_163;

pub fn iso_to_unix_ts(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    if let Ok(nd) = NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d") {
        if let Some(ndt) = nd.and_hms_opt(0, 0, 0) {
            return Some(Utc.from_utc_datetime(&ndt).timestamp());
        }
    }
    None
}

pub fn unix_ts_to_iso(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

pub fn iso_to_unix_ms(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    iso_to_unix_ts(s).map(|sec| sec.saturating_mul(1000))
}

pub fn unix_ms_to_iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

pub fn date_str_to_epoch_day(raw: Option<&str>) -> Option<i32> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    let date_prefix = if s.len() >= 10 { &s[..10] } else { s };
    let nd = NaiveDate::parse_from_str(date_prefix, "%Y-%m-%d").ok()?;
    use chrono::Datelike;
    Some(nd.num_days_from_ce() - UNIX_EPOCH_CE_DAYS)
}

pub fn epoch_day_to_date_str(days: Option<i32>) -> Option<String> {
    let d = days?;
    let nd = NaiveDate::from_num_days_from_ce_opt(d.saturating_add(UNIX_EPOCH_CE_DAYS))?;
    Some(nd.format("%Y-%m-%d").to_string())
}

// ---------------------------------------------------------------------------------------------
// Integer Enum Codecs (`TINYINT` / `SMALLINT` <-> Domain Strings)
// ---------------------------------------------------------------------------------------------

pub fn listing_status_to_code(raw: Option<&str>) -> i32 {
    match crate::db::schema::normalize_listing_status(raw) {
        "for_sale" => ListingStatus::ForSale as i32,
        "pending" => ListingStatus::Pending as i32,
        "sold" => ListingStatus::Sold as i32,
        "off_market" => ListingStatus::OffMarket as i32,
        _ => ListingStatus::ForSale as i32,
    }
}

pub fn listing_status_from_code(code: i32) -> &'static str {
    match ListingStatus::try_from(code).unwrap_or(ListingStatus::ForSale) {
        ListingStatus::ForSale | ListingStatus::Unspecified => "for_sale",
        ListingStatus::Pending => "pending",
        ListingStatus::Sold => "sold",
        ListingStatus::OffMarket => "off_market",
    }
}

pub fn property_category_to_code(raw: Option<&str>) -> i32 {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return PropertyCategory::Unspecified as i32;
    };
    let low = s.to_ascii_lowercase();
    if low.contains("agricult") || low.contains("farm") || low.contains("ranch land") || low.contains("orchard") {
        PropertyCategory::Agricultural as i32
    } else if low.contains("commercial")
        || low.contains("retail")
        || low.contains("office")
        || low.contains("industrial")
        || low.contains("mixed")
    {
        PropertyCategory::Commercial as i32
    } else if low == "land"
        || low == "lot"
        || low.contains("vacant land")
        || low.contains("land / lot")
    {
        PropertyCategory::LandLot as i32
    } else if low.contains("manufactured") || low.contains("mobile") {
        PropertyCategory::Manufactured as i32
    } else if low.contains("condo")
        || low.contains("co-op")
        || low.contains("coop")
        || low.contains("apartment")
        || low.contains("flat")
        || low.contains("high-rise")
        || low.contains("highrise")
        || low.contains("mid-rise")
        || low.contains("midrise")
        || low.contains("loft")
    {
        PropertyCategory::CondoCoop as i32
    } else if low.contains("townhouse")
        || low.contains("townhome")
        || low.contains("row")
        || low.contains("attached")
        || low.contains("pud")
        || low.contains("patio")
        || low.contains("duet")
    {
        PropertyCategory::TownhomeAttached as i32
    } else if low.contains("multi")
        || low.contains("duplex")
        || low.contains("triplex")
        || low.contains("fourplex")
        || low.contains("quad")
        || low.contains("2-4")
        || low.contains("2–4")
        || low.contains("income")
    {
        PropertyCategory::MultiFamily as i32
    } else if low.contains("single")
        || low.contains("sfr")
        || low.contains("detached")
        || low.contains("residential")
        || low.contains("house")
    {
        PropertyCategory::SingleFamilyDetached as i32
    } else {
        PropertyCategory::Unspecified as i32
    }
}

pub fn property_category_from_code(code: i32) -> Option<&'static str> {
    match PropertyCategory::try_from(code).unwrap_or(PropertyCategory::Unspecified) {
        PropertyCategory::Unspecified => None,
        PropertyCategory::SingleFamilyDetached => Some("Single-Family Detached"),
        PropertyCategory::TownhomeAttached => Some("Townhome / Attached"),
        PropertyCategory::CondoCoop => Some("Condo / Co-op"),
        PropertyCategory::MultiFamily => Some("2–4 Unit Multifamily"),
        PropertyCategory::Manufactured | PropertyCategory::LandLot => Some("Manufactured / Land"),
        PropertyCategory::Commercial => Some("Commercial / Mixed-Use"),
        PropertyCategory::Agricultural => Some("Agricultural / Farmland"),
    }
}

pub fn listing_event_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "new_listing" => ListingEventKind::ListingEventNewListing as i32,
        "price_change" => ListingEventKind::ListingEventPriceChange as i32,
        "status_change" => ListingEventKind::ListingEventStatusChange as i32,
        "sold" => ListingEventKind::ListingEventSold as i32,
        "back_on_market" => ListingEventKind::ListingEventBackOnMarket as i32,
        _ => ListingEventKind::ListingEventUnspecified as i32,
    }
}

pub fn listing_event_from_code(code: i32) -> &'static str {
    match ListingEventKind::try_from(code).unwrap_or(ListingEventKind::ListingEventUnspecified) {
        ListingEventKind::ListingEventNewListing | ListingEventKind::ListingEventUnspecified => {
            "new_listing"
        }
        ListingEventKind::ListingEventPriceChange => "price_change",
        ListingEventKind::ListingEventStatusChange => "status_change",
        ListingEventKind::ListingEventSold => "sold",
        ListingEventKind::ListingEventBackOnMarket => "back_on_market",
    }
}

pub fn electric_utility_to_code(raw: Option<&str>) -> i32 {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "municipal" | "public" => ElectricUtilityKind::ElectricUtilityMunicipal as i32,
        "iou" | "investor_owned" => ElectricUtilityKind::ElectricUtilityIou as i32,
        "deregulated_choice" | "deregulated" => {
            ElectricUtilityKind::ElectricUtilityDeregulatedChoice as i32
        }
        "coop" | "cooperative" => ElectricUtilityKind::ElectricUtilityCoop as i32,
        _ => ElectricUtilityKind::ElectricUtilityUnspecified as i32,
    }
}

pub fn electric_utility_from_code(code: i32) -> Option<&'static str> {
    match ElectricUtilityKind::try_from(code)
        .unwrap_or(ElectricUtilityKind::ElectricUtilityUnspecified)
    {
        ElectricUtilityKind::ElectricUtilityUnspecified => None,
        ElectricUtilityKind::ElectricUtilityMunicipal => Some("municipal"),
        ElectricUtilityKind::ElectricUtilityIou => Some("iou"),
        ElectricUtilityKind::ElectricUtilityDeregulatedChoice => Some("deregulated_choice"),
        ElectricUtilityKind::ElectricUtilityCoop => Some("coop"),
    }
}

pub fn heating_fuel_to_code(
    gas_carrier: Option<&str>,
    gas_rate: Option<f64>,
    has_heat_pump: Option<i64>,
) -> i32 {
    if has_heat_pump == Some(1) {
        return HeatingFuelKind::HeatingFuelElectricHeatPump as i32;
    }
    if let Some(gc) = gas_carrier.map(str::trim).filter(|s| !s.is_empty()) {
        let low = gc.to_ascii_lowercase();
        if low.contains("propane") || low.contains("oil") {
            return HeatingFuelKind::HeatingFuelPropaneOrOil as i32;
        }
        if low.contains("none") || low.contains("all-electric") {
            return HeatingFuelKind::HeatingFuelNone as i32;
        }
        return HeatingFuelKind::HeatingFuelNaturalGas as i32;
    }
    if gas_rate.is_some() {
        HeatingFuelKind::HeatingFuelNaturalGas as i32
    } else {
        HeatingFuelKind::HeatingFuelUnspecified as i32
    }
}

pub fn water_source_to_code(raw: Option<&str>) -> i32 {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "municipal" | "public" => WaterSourceKind::WaterSourceMunicipal as i32,
        "private_well" | "well" => WaterSourceKind::WaterSourcePrivateWell as i32,
        "mutual" => WaterSourceKind::WaterSourceMutual as i32,
        _ => WaterSourceKind::WaterSourceUnspecified as i32,
    }
}

pub fn water_source_from_code(code: i32) -> Option<&'static str> {
    match WaterSourceKind::try_from(code).unwrap_or(WaterSourceKind::WaterSourceUnspecified) {
        WaterSourceKind::WaterSourceUnspecified => None,
        WaterSourceKind::WaterSourceMunicipal => Some("municipal"),
        WaterSourceKind::WaterSourcePrivateWell => Some("private_well"),
        WaterSourceKind::WaterSourceMutual => Some("mutual"),
    }
}

pub fn sewer_kind_to_code(raw: Option<&str>) -> i32 {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "municipal" | "public" => SewerKind::SewerMunicipal as i32,
        "septic" => SewerKind::SewerSeptic as i32,
        _ => SewerKind::SewerUnspecified as i32,
    }
}

pub fn sewer_kind_from_code(code: i32) -> Option<&'static str> {
    match SewerKind::try_from(code).unwrap_or(SewerKind::SewerUnspecified) {
        SewerKind::SewerUnspecified => None,
        SewerKind::SewerMunicipal => Some("municipal"),
        SewerKind::SewerSeptic => Some("septic"),
    }
}

pub fn broadband_tier_to_code(raw: Option<&str>) -> i32 {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "fiber_confirmed" => BroadbandTier::FiberConfirmed as i32,
        "fiber_area" | "fiber" => BroadbandTier::FiberArea as i32,
        "cable_high_speed" | "cable" => BroadbandTier::CableHighSpeed as i32,
        "wireless_or_dsl" | "wireless" | "dsl" => BroadbandTier::WirelessOrDsl as i32,
        "satellite_only" | "satellite" => BroadbandTier::SatelliteOnly as i32,
        _ => BroadbandTier::Unspecified as i32,
    }
}

pub fn broadband_tier_from_code(code: i32) -> Option<&'static str> {
    match BroadbandTier::try_from(code).unwrap_or(BroadbandTier::Unspecified) {
        BroadbandTier::Unspecified => None,
        BroadbandTier::FiberConfirmed => Some("fiber_confirmed"),
        BroadbandTier::FiberArea => Some("fiber_area"),
        BroadbandTier::CableHighSpeed => Some("cable_high_speed"),
        BroadbandTier::WirelessOrDsl => Some("wireless_or_dsl"),
        BroadbandTier::SatelliteOnly => Some("satellite_only"),
    }
}

pub fn flood_risk_to_code(raw: Option<&str>) -> i32 {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return FloodRiskTier::FloodRiskUnspecified as i32;
    };
    let up = s.to_ascii_uppercase();
    if matches!(up.as_str(), "V" | "VE") || up.starts_with("VE ") {
        FloodRiskTier::FloodRiskSfhaCoastal as i32
    } else if matches!(up.as_str(), "A" | "AE" | "AO" | "AH" | "A99") || up.starts_with("AE ") {
        FloodRiskTier::FloodRiskSfhaInland as i32
    } else if up.contains("500") || up.contains("0.2") || up.contains("SHADED") {
        FloodRiskTier::FloodRiskModerateX500 as i32
    } else if up == "D" {
        FloodRiskTier::FloodRiskUndeterminedD as i32
    } else if up.starts_with('X') || up.contains("MINIMAL") {
        FloodRiskTier::FloodRiskMinimalX as i32
    } else {
        FloodRiskTier::FloodRiskUnspecified as i32
    }
}

pub fn wildfire_risk_to_code(raw: Option<&str>) -> i32 {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return WildfireRiskTier::WildfireRiskUnspecified as i32;
    };
    let low = s.to_ascii_lowercase();
    if low.contains("very high") || low == "vhfhsz" {
        WildfireRiskTier::WildfireRiskVeryHigh as i32
    } else if low.contains("high") {
        WildfireRiskTier::WildfireRiskHigh as i32
    } else if low.contains("moderate") || low.contains("medium") {
        WildfireRiskTier::WildfireRiskModerate as i32
    } else if low.contains("very low") {
        WildfireRiskTier::WildfireRiskVeryLow as i32
    } else if low.contains("low") {
        WildfireRiskTier::WildfireRiskLow as i32
    } else if low.contains("none") || low.contains("non-wildland") || low.contains("minimal") {
        WildfireRiskTier::WildfireRiskNone as i32
    } else {
        WildfireRiskTier::WildfireRiskUnspecified as i32
    }
}

pub fn wildfire_risk_from_code(code: i32) -> Option<&'static str> {
    match WildfireRiskTier::try_from(code).unwrap_or(WildfireRiskTier::WildfireRiskUnspecified) {
        WildfireRiskTier::WildfireRiskUnspecified => None,
        WildfireRiskTier::WildfireRiskNone => Some("None"),
        WildfireRiskTier::WildfireRiskVeryLow => Some("Very Low"),
        WildfireRiskTier::WildfireRiskLow => Some("Low"),
        WildfireRiskTier::WildfireRiskModerate => Some("Moderate"),
        WildfireRiskTier::WildfireRiskHigh => Some("High"),
        WildfireRiskTier::WildfireRiskVeryHigh => Some("Very High"),
    }
}

pub fn school_grade_to_code(raw: Option<&str>) -> Option<i32> {
    let g = raw?.trim().to_ascii_uppercase();
    if g.is_empty() {
        return None;
    }
    let c = match g.as_str() {
        "A+" => SchoolGradeCode::SchoolGradeAPlus,
        "A" => SchoolGradeCode::SchoolGradeA,
        "A-" => SchoolGradeCode::SchoolGradeAMinus,
        "B+" => SchoolGradeCode::SchoolGradeBPlus,
        "B" => SchoolGradeCode::SchoolGradeB,
        "B-" => SchoolGradeCode::SchoolGradeBMinus,
        "C+" => SchoolGradeCode::SchoolGradeCPlus,
        "C" => SchoolGradeCode::SchoolGradeC,
        "C-" => SchoolGradeCode::SchoolGradeCMinus,
        "D" | "D+" | "D-" => SchoolGradeCode::SchoolGradeD,
        "F" => SchoolGradeCode::SchoolGradeF,
        _ => return None,
    };
    Some(c as i32)
}

pub fn school_grade_from_code(code: Option<i32>) -> Option<String> {
    let c = SchoolGradeCode::try_from(code?).ok()?;
    let s = match c {
        SchoolGradeCode::SchoolGradeUnspecified => return None,
        SchoolGradeCode::SchoolGradeAPlus => "A+",
        SchoolGradeCode::SchoolGradeA => "A",
        SchoolGradeCode::SchoolGradeAMinus => "A-",
        SchoolGradeCode::SchoolGradeBPlus => "B+",
        SchoolGradeCode::SchoolGradeB => "B",
        SchoolGradeCode::SchoolGradeBMinus => "B-",
        SchoolGradeCode::SchoolGradeCPlus => "C+",
        SchoolGradeCode::SchoolGradeC => "C",
        SchoolGradeCode::SchoolGradeCMinus => "C-",
        SchoolGradeCode::SchoolGradeD => "D",
        SchoolGradeCode::SchoolGradeF => "F",
    };
    Some(s.to_string())
}

pub fn school_level_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "elementary" | "elem" | "primary" => SchoolLevelKind::SchoolLevelElementary as i32,
        "middle" | "mid" | "junior" => SchoolLevelKind::SchoolLevelMiddle as i32,
        "high" | "secondary" | "senior" => SchoolLevelKind::SchoolLevelHigh as i32,
        _ => SchoolLevelKind::SchoolLevelUnspecified as i32,
    }
}

pub fn school_level_from_code(code: i32) -> &'static str {
    match SchoolLevelKind::try_from(code).unwrap_or(SchoolLevelKind::SchoolLevelUnspecified) {
        SchoolLevelKind::SchoolLevelElementary | SchoolLevelKind::SchoolLevelUnspecified => {
            "elementary"
        }
        SchoolLevelKind::SchoolLevelMiddle => "middle",
        SchoolLevelKind::SchoolLevelHigh => "high",
    }
}

pub fn style_family_to_code(family: StyleFamily) -> i32 {
    match family {
        StyleFamily::Modern => ArchitecturalStyleCode::ArchStyleModern as i32,
        StyleFamily::MidCentury => ArchitecturalStyleCode::ArchStyleMidCentury as i32,
        StyleFamily::Ranch => ArchitecturalStyleCode::ArchStyleRanch as i32,
        StyleFamily::SplitLevel => ArchitecturalStyleCode::ArchStyleSplitLevel as i32,
        StyleFamily::Craftsman => ArchitecturalStyleCode::ArchStyleCraftsman as i32,
        StyleFamily::Farmhouse => ArchitecturalStyleCode::ArchStyleFarmhouse as i32,
        StyleFamily::Barn => ArchitecturalStyleCode::ArchStyleBarn as i32,
        StyleFamily::Colonial => ArchitecturalStyleCode::ArchStyleColonial as i32,
        StyleFamily::CapeCod => ArchitecturalStyleCode::ArchStyleCapeCod as i32,
        StyleFamily::Victorian => ArchitecturalStyleCode::ArchStyleVictorian as i32,
        StyleFamily::Tudor => ArchitecturalStyleCode::ArchStyleTudor as i32,
        StyleFamily::Mediterranean => ArchitecturalStyleCode::ArchStyleMediterranean as i32,
        StyleFamily::Southwestern => ArchitecturalStyleCode::ArchStyleSouthwestern as i32,
        StyleFamily::Cabin => ArchitecturalStyleCode::ArchStyleCabin as i32,
        StyleFamily::Traditional => ArchitecturalStyleCode::ArchStyleTraditional as i32,
        StyleFamily::OtherDistinctive => ArchitecturalStyleCode::ArchStyleOtherDistinctive as i32,
    }
}

pub fn style_family_from_code(code: i32) -> Option<StyleFamily> {
    match ArchitecturalStyleCode::try_from(code).ok()? {
        ArchitecturalStyleCode::ArchStyleUnspecified => None,
        ArchitecturalStyleCode::ArchStyleModern => Some(StyleFamily::Modern),
        ArchitecturalStyleCode::ArchStyleMidCentury => Some(StyleFamily::MidCentury),
        ArchitecturalStyleCode::ArchStyleRanch => Some(StyleFamily::Ranch),
        ArchitecturalStyleCode::ArchStyleSplitLevel => Some(StyleFamily::SplitLevel),
        ArchitecturalStyleCode::ArchStyleCraftsman => Some(StyleFamily::Craftsman),
        ArchitecturalStyleCode::ArchStyleFarmhouse => Some(StyleFamily::Farmhouse),
        ArchitecturalStyleCode::ArchStyleBarn => Some(StyleFamily::Barn),
        ArchitecturalStyleCode::ArchStyleColonial => Some(StyleFamily::Colonial),
        ArchitecturalStyleCode::ArchStyleCapeCod => Some(StyleFamily::CapeCod),
        ArchitecturalStyleCode::ArchStyleVictorian => Some(StyleFamily::Victorian),
        ArchitecturalStyleCode::ArchStyleTudor => Some(StyleFamily::Tudor),
        ArchitecturalStyleCode::ArchStyleMediterranean => Some(StyleFamily::Mediterranean),
        ArchitecturalStyleCode::ArchStyleSouthwestern => Some(StyleFamily::Southwestern),
        ArchitecturalStyleCode::ArchStyleCabin => Some(StyleFamily::Cabin),
        ArchitecturalStyleCode::ArchStyleTraditional => Some(StyleFamily::Traditional),
        ArchitecturalStyleCode::ArchStyleOtherDistinctive => Some(StyleFamily::OtherDistinctive),
    }
}

pub fn architectural_style_str_to_code(raw: Option<&str>) -> i32 {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return ArchitecturalStyleCode::ArchStyleUnspecified as i32;
    };
    if let Some(f) = StyleFamily::from_key(&s.to_ascii_lowercase().replace([' ', '-'], "_")) {
        return style_family_to_code(f);
    }
    if let Some(first) = style_families_for_value(s).into_iter().next() {
        return style_family_to_code(first);
    }
    ArchitecturalStyleCode::ArchStyleUnspecified as i32
}

pub fn style_basis_to_code(src: Option<StyleSource>) -> i32 {
    match src {
        None => StyleEvidenceBasis::StyleBasisNone as i32,
        Some(StyleSource::Mls) => StyleEvidenceBasis::StyleBasisMlsField as i32,
        Some(StyleSource::CountyRecord) => StyleEvidenceBasis::StyleBasisCountyRecord as i32,
        Some(StyleSource::ListingText) => StyleEvidenceBasis::StyleBasisListingText as i32,
        Some(StyleSource::EraPrior) => StyleEvidenceBasis::StyleBasisEraPrior as i32,
    }
}

pub fn style_basis_from_code(code: i32) -> Option<StyleSource> {
    match StyleEvidenceBasis::try_from(code).unwrap_or(StyleEvidenceBasis::StyleBasisNone) {
        StyleEvidenceBasis::StyleBasisNone => None,
        StyleEvidenceBasis::StyleBasisMlsField => Some(StyleSource::Mls),
        StyleEvidenceBasis::StyleBasisCountyRecord => Some(StyleSource::CountyRecord),
        StyleEvidenceBasis::StyleBasisListingText => Some(StyleSource::ListingText),
        StyleEvidenceBasis::StyleBasisEraPrior => Some(StyleSource::EraPrior),
    }
}

pub fn factor_tier_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "listing" => FactorSourceTier::FactorTierListing as i32,
        "light_scan" | "area" => FactorSourceTier::FactorTierLightScan as i32,
        "full_report" | "report" => FactorSourceTier::FactorTierFullReport as i32,
        _ => FactorSourceTier::FactorTierListing as i32,
    }
}

pub fn factor_tier_from_code(code: i32) -> &'static str {
    match FactorSourceTier::try_from(code).unwrap_or(FactorSourceTier::FactorTierListing) {
        FactorSourceTier::FactorTierListing | FactorSourceTier::FactorTierUnspecified => "listing",
        FactorSourceTier::FactorTierLightScan => "light_scan",
        FactorSourceTier::FactorTierFullReport => "full_report",
    }
}

pub fn portal_platform_to_code(raw: Option<&str>) -> i32 {
    let low = raw.unwrap_or("").trim().to_ascii_lowercase();
    if low.contains("zillow") {
        PortalPlatform::Zillow as i32
    } else if low.contains("redfin") {
        PortalPlatform::Redfin as i32
    } else if low.contains("realtor") {
        PortalPlatform::Realtor as i32
    } else if low.contains("homes") {
        PortalPlatform::Homes as i32
    } else if low.contains("mls") {
        PortalPlatform::MlsDirect as i32
    } else if low.contains("open estate") || low.contains("open_estate") || low.contains("report") {
        PortalPlatform::OpenEstate as i32
    } else {
        PortalPlatform::Unspecified as i32
    }
}

pub fn subscription_freq_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "daily" => SubscriptionFrequency::SubscriptionFreqDaily as i32,
        "weekly" => SubscriptionFrequency::SubscriptionFreqWeekly as i32,
        _ => SubscriptionFrequency::SubscriptionFreqInstant as i32,
    }
}

pub fn subscription_freq_from_code(code: i32) -> &'static str {
    match SubscriptionFrequency::try_from(code)
        .unwrap_or(SubscriptionFrequency::SubscriptionFreqInstant)
    {
        SubscriptionFrequency::SubscriptionFreqDaily => "daily",
        SubscriptionFrequency::SubscriptionFreqWeekly => "weekly",
        _ => "instant",
    }
}

pub fn delivery_status_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "sent" => NotificationDeliveryStatus::DeliveryStatusSent as i32,
        "queued_no_smtp" | "skipped_no_smtp" | "smtp_not_configured" => {
            NotificationDeliveryStatus::DeliveryStatusSkippedNoSmtp as i32
        }
        "failed" => NotificationDeliveryStatus::DeliveryStatusFailed as i32,
        "queued_digest" => NotificationDeliveryStatus::DeliveryStatusQueuedDigest as i32,
        "skipped" => NotificationDeliveryStatus::DeliveryStatusSkipped as i32,
        _ => NotificationDeliveryStatus::DeliveryStatusUnspecified as i32,
    }
}

pub fn delivery_status_from_code(code: i32) -> &'static str {
    match NotificationDeliveryStatus::try_from(code)
        .unwrap_or(NotificationDeliveryStatus::DeliveryStatusUnspecified)
    {
        NotificationDeliveryStatus::DeliveryStatusSent => "sent",
        NotificationDeliveryStatus::DeliveryStatusSkippedNoSmtp => "queued_no_smtp",
        NotificationDeliveryStatus::DeliveryStatusFailed => "failed",
        NotificationDeliveryStatus::DeliveryStatusQueuedDigest => "queued_digest",
        NotificationDeliveryStatus::DeliveryStatusSkipped
        | NotificationDeliveryStatus::DeliveryStatusUnspecified => "skipped",
    }
}

pub fn build_job_status_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "queued" => BuildJobStatus::BuildJobQueued as i32,
        "building" | "running" => BuildJobStatus::BuildJobRunning as i32,
        "complete" | "done" => BuildJobStatus::BuildJobDone as i32,
        "failed" => BuildJobStatus::BuildJobFailed as i32,
        "ai_pending" => BuildJobStatus::BuildJobAiPending as i32,
        _ => BuildJobStatus::BuildJobRunning as i32,
    }
}

pub fn build_job_status_from_code(code: i32) -> &'static str {
    match BuildJobStatus::try_from(code).unwrap_or(BuildJobStatus::BuildJobRunning) {
        BuildJobStatus::BuildJobQueued => "queued",
        BuildJobStatus::BuildJobRunning | BuildJobStatus::BuildJobUnspecified => "building",
        BuildJobStatus::BuildJobDone => "complete",
        BuildJobStatus::BuildJobFailed => "failed",
        BuildJobStatus::BuildJobAiPending => "ai_pending",
    }
}

pub fn feedback_action_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "save" | "like" | "saved" => FeedbackActionKind::FeedbackActionSave as i32,
        "dismiss" | "pass" => FeedbackActionKind::FeedbackActionDismiss as i32,
        "tour" => FeedbackActionKind::FeedbackActionTour as i32,
        "hide" => FeedbackActionKind::FeedbackActionHide as i32,
        _ => FeedbackActionKind::FeedbackActionUnspecified as i32,
    }
}

pub fn feedback_action_from_code(code: i32) -> &'static str {
    match FeedbackActionKind::try_from(code).unwrap_or(FeedbackActionKind::FeedbackActionUnspecified)
    {
        FeedbackActionKind::FeedbackActionSave | FeedbackActionKind::FeedbackActionUnspecified => {
            "save"
        }
        FeedbackActionKind::FeedbackActionDismiss => "dismiss",
        FeedbackActionKind::FeedbackActionTour => "tour",
        FeedbackActionKind::FeedbackActionHide => "hide",
    }
}

pub fn doc_review_kind_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "inspect" | "inspection" => DocReviewKind::DocReviewInspect as i32,
        "prelim" | "title" => DocReviewKind::DocReviewTitle as i32,
        "hoa" => DocReviewKind::DocReviewHoa as i32,
        "packet" => DocReviewKind::DocReviewPacket as i32,
        "disclosure" => DocReviewKind::DocReviewDisclosure as i32,
        "cre" => DocReviewKind::DocReviewCre as i32,
        "addendum" => DocReviewKind::DocReviewAddendum as i32,
        "copilot_comps" => DocReviewKind::DocReviewCopilotComps as i32,
        "copilot_offer" => DocReviewKind::DocReviewCopilotOffer as i32,
        "copilot_disclosures" => DocReviewKind::DocReviewCopilotDisclosures as i32,
        "copilot_basis" => DocReviewKind::DocReviewCopilotBasis as i32,
        "copilot_auth_disc" | "copilot_authorized_disclosures" => {
            DocReviewKind::DocReviewCopilotAuthorizedDisclosures as i32
        }
        "copilot_deal_team" => DocReviewKind::DocReviewCopilotDealTeam as i32,
        _ => DocReviewKind::DocReviewUnspecified as i32,
    }
}

pub fn doc_review_kind_from_code(code: i32) -> &'static str {
    match DocReviewKind::try_from(code).unwrap_or(DocReviewKind::DocReviewUnspecified) {
        DocReviewKind::DocReviewUnspecified => "unspecified",
        DocReviewKind::DocReviewInspect => "inspect",
        DocReviewKind::DocReviewTitle => "prelim",
        DocReviewKind::DocReviewHoa => "hoa",
        DocReviewKind::DocReviewPacket => "packet",
        DocReviewKind::DocReviewDisclosure => "disclosure",
        DocReviewKind::DocReviewCre => "cre",
        DocReviewKind::DocReviewAddendum => "addendum",
        DocReviewKind::DocReviewCopilotComps => "copilot_comps",
        DocReviewKind::DocReviewCopilotOffer => "copilot_offer",
        DocReviewKind::DocReviewCopilotDisclosures => "copilot_disclosures",
        DocReviewKind::DocReviewCopilotBasis => "copilot_basis",
        DocReviewKind::DocReviewCopilotAuthorizedDisclosures => "copilot_auth_disc",
        DocReviewKind::DocReviewCopilotDealTeam => "copilot_deal_team",
    }
}

pub fn specialist_role_to_code(raw: &str) -> i32 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "buyer_agent" | "agent" => SpecialistRole::BuyerAgent as i32,
        "home_inspector" | "inspector" => SpecialistRole::HomeInspector as i32,
        _ => SpecialistRole::Other as i32,
    }
}

pub fn specialist_role_from_code(code: i32) -> &'static str {
    match SpecialistRole::try_from(code).unwrap_or(SpecialistRole::BuyerAgent) {
        SpecialistRole::BuyerAgent | SpecialistRole::Unspecified => "buyer_agent",
        SpecialistRole::HomeInspector => "home_inspector",
        SpecialistRole::Other => "specialist",
    }
}

pub fn watched_sync_status_to_code(raw: Option<&str>) -> i32 {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "" => 0,
        "ok" => 1,
        "queued" => 2,
        "partial_or_no_keys" => 3,
        "error" | "failed" => 4,
        _ => 1,
    }
}

pub fn watched_sync_status_from_code(code: i32) -> Option<&'static str> {
    match code {
        0 => None,
        1 => Some("ok"),
        2 => Some("queued"),
        3 => Some("partial_or_no_keys"),
        4 => Some("error"),
        _ => Some("ok"),
    }
}

// ---------------------------------------------------------------------------------------------
// `FeatureTag` Bitmask Engine (Bits `1..=35`, Rapid-Safe Signed `i64`)
// ---------------------------------------------------------------------------------------------

pub const fn feature_tag_bit(tag: FeatureTag) -> i64 {
    let pos = tag as u32;
    if pos == 0 || pos >= 63 {
        0
    } else {
        1_i64 << pos
    }
}

pub fn feature_tag_from_slug(raw: &str) -> Option<FeatureTag> {
    let norm = raw.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match norm.as_str() {
        "fiber" | "fiber_available" => Some(FeatureTag::FiberAvailable),
        "high_speed_internet" | "gigabit" => Some(FeatureTag::HighSpeedInternet),
        "municipal_electric" | "public_power" => Some(FeatureTag::MunicipalElectric),
        "municipal_water" | "city_water" => Some(FeatureTag::MunicipalWater),
        "municipal_sewer" | "city_sewer" => Some(FeatureTag::MunicipalSewer),
        "has_special_tax" | "special_tax" | "mello_roos" => Some(FeatureTag::HasSpecialTax),
        "in_sfha_flood" | "sfha_flood" | "flood_zone" => Some(FeatureTag::InSfhaFlood),
        "high_wildfire" | "wildfire_risk" => Some(FeatureTag::HighWildfire),
        "solar" | "has_solar" | "solar_panels" => Some(FeatureTag::Solar),
        "ev_charger" | "has_ev_charger" | "ev_ready" => Some(FeatureTag::EvCharger),
        "heat_pump" | "has_heat_pump" => Some(FeatureTag::HeatPump),
        "adu" | "has_adu" | "guest_house" | "in_law_unit" => Some(FeatureTag::Adu),
        "adu_feasible" | "adu_potential" => Some(FeatureTag::AduFeasible),
        "pool" | "has_pool" | "swimming_pool" => Some(FeatureTag::Pool),
        "hardwood_floors" | "hardwood" | "has_hardwood_floors" => Some(FeatureTag::HardwoodFloors),
        "updated_kitchen" | "has_updated_kitchen" | "remodeled_kitchen" => {
            Some(FeatureTag::UpdatedKitchen)
        }
        "remodeled" | "has_remodeled" | "renovated" | "turnkey" => Some(FeatureTag::Remodeled),
        "quiet_street" => Some(FeatureTag::QuietStreet),
        "loud_street" => Some(FeatureTag::LoudStreet),
        "cul_de_sac" => Some(FeatureTag::CulDeSac),
        "fireplace" => Some(FeatureTag::Fireplace),
        "sunroom" => Some(FeatureTag::Sunroom),
        "central_ac" | "air_conditioning" => Some(FeatureTag::CentralAc),
        "high_ceilings" | "vaulted_ceilings" => Some(FeatureTag::HighCeilings),
        "fenced_yard" => Some(FeatureTag::FencedYard),
        "open_plan" | "open_floor_plan" => Some(FeatureTag::OpenPlan),
        "natural_light" => Some(FeatureTag::NaturalLight),
        "view" | "views" => Some(FeatureTag::View),
        "home_office" | "office" => Some(FeatureTag::HomeOffice),
        "workshop_garage" | "workshop" => Some(FeatureTag::WorkshopGarage),
        "single_story" | "one_story" => Some(FeatureTag::SingleStory),
        "low_noise" => Some(FeatureTag::LowNoise),
        "low_seismic" => Some(FeatureTag::LowSeismic),
        "high_safety" => Some(FeatureTag::HighSafety),
        "no_hoa" => Some(FeatureTag::NoHoa),
        _ => None,
    }
}

pub fn feature_tag_slug(tag: FeatureTag) -> Option<&'static str> {
    match tag {
        FeatureTag::Unspecified => None,
        FeatureTag::FiberAvailable => Some("fiber_available"),
        FeatureTag::HighSpeedInternet => Some("high_speed_internet"),
        FeatureTag::MunicipalElectric => Some("municipal_electric"),
        FeatureTag::MunicipalWater => Some("municipal_water"),
        FeatureTag::MunicipalSewer => Some("municipal_sewer"),
        FeatureTag::HasSpecialTax => Some("has_special_tax"),
        FeatureTag::InSfhaFlood => Some("in_sfha_flood"),
        FeatureTag::HighWildfire => Some("high_wildfire"),
        FeatureTag::Solar => Some("solar"),
        FeatureTag::EvCharger => Some("ev_charger"),
        FeatureTag::HeatPump => Some("heat_pump"),
        FeatureTag::Adu => Some("adu"),
        FeatureTag::AduFeasible => Some("adu_feasible"),
        FeatureTag::Pool => Some("pool"),
        FeatureTag::HardwoodFloors => Some("hardwood_floors"),
        FeatureTag::UpdatedKitchen => Some("updated_kitchen"),
        FeatureTag::Remodeled => Some("remodeled"),
        FeatureTag::QuietStreet => Some("quiet_street"),
        FeatureTag::LoudStreet => Some("loud_street"),
        FeatureTag::CulDeSac => Some("cul_de_sac"),
        FeatureTag::Fireplace => Some("fireplace"),
        FeatureTag::Sunroom => Some("sunroom"),
        FeatureTag::CentralAc => Some("central_ac"),
        FeatureTag::HighCeilings => Some("high_ceilings"),
        FeatureTag::FencedYard => Some("fenced_yard"),
        FeatureTag::OpenPlan => Some("open_plan"),
        FeatureTag::NaturalLight => Some("natural_light"),
        FeatureTag::View => Some("view"),
        FeatureTag::HomeOffice => Some("home_office"),
        FeatureTag::WorkshopGarage => Some("workshop_garage"),
        FeatureTag::SingleStory => Some("single_story"),
        FeatureTag::LowNoise => Some("low_noise"),
        FeatureTag::LowSeismic => Some("low_seismic"),
        FeatureTag::HighSafety => Some("high_safety"),
        FeatureTag::NoHoa => Some("no_hoa"),
    }
}

/// Computes `(tag_bits, known_bits, explicit_tags)` from structured `ListingRow` fields and `tags_json`.
pub fn compute_listing_tag_bits(row: &crate::db::schema::ListingRow) -> (i64, i64, Vec<String>) {
    let mut tag_bits = 0_i64;
    let mut known_bits = 0_i64;

    let mut set_opt = |tag: FeatureTag, val: Option<bool>| {
        if let Some(b) = val {
            let bit = feature_tag_bit(tag);
            known_bits |= bit;
            if b {
                tag_bits |= bit;
            }
        }
    };

    set_opt(FeatureTag::FiberAvailable, row.fiber_available.map(|v| v != 0));
    set_opt(
        FeatureTag::HighSpeedInternet,
        row.high_speed_internet.map(|v| v != 0),
    );
    set_opt(
        FeatureTag::MunicipalElectric,
        row.electric_utility_type
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("municipal")),
    );
    set_opt(
        FeatureTag::MunicipalWater,
        row.water_source_type
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("municipal")),
    );
    set_opt(
        FeatureTag::MunicipalSewer,
        row.sewer_type
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("municipal")),
    );
    set_opt(FeatureTag::HasSpecialTax, row.has_special_tax.map(|v| v != 0));
    set_opt(
        FeatureTag::InSfhaFlood,
        row.flood_zone.as_deref().map(|fz| {
            matches!(
                flood_risk_to_code(Some(fz)),
                x if x == FloodRiskTier::FloodRiskSfhaInland as i32
                    || x == FloodRiskTier::FloodRiskSfhaCoastal as i32
            )
        }),
    );
    set_opt(
        FeatureTag::HighWildfire,
        row.wildfire_zone.as_deref().map(|wz| {
            matches!(
                wildfire_risk_to_code(Some(wz)),
                x if x == WildfireRiskTier::WildfireRiskHigh as i32
                    || x == WildfireRiskTier::WildfireRiskVeryHigh as i32
            )
        }),
    );
    set_opt(FeatureTag::Solar, row.has_solar.map(|v| v != 0));
    set_opt(FeatureTag::EvCharger, row.has_ev_charger.map(|v| v != 0));
    set_opt(FeatureTag::HeatPump, row.has_heat_pump.map(|v| v != 0));
    set_opt(FeatureTag::Adu, row.has_adu.map(|v| v != 0));
    set_opt(FeatureTag::AduFeasible, row.adu_feasible.map(|v| v != 0));
    set_opt(FeatureTag::Pool, row.has_pool.map(|v| v != 0));
    set_opt(
        FeatureTag::HardwoodFloors,
        row.has_hardwood_floors.map(|v| v != 0),
    );
    set_opt(
        FeatureTag::UpdatedKitchen,
        row.has_updated_kitchen.map(|v| v != 0),
    );
    set_opt(FeatureTag::Remodeled, row.has_remodeled.map(|v| v != 0));
    set_opt(FeatureTag::QuietStreet, row.quiet_street.map(|v| v == 1));
    set_opt(FeatureTag::LoudStreet, row.quiet_street.map(|v| v == 0));
    set_opt(
        FeatureTag::SingleStory,
        row.stories.map(|s| s > 0.0 && s <= 1.2),
    );
    set_opt(FeatureTag::LowNoise, row.noise_max_db.map(|n| n <= 55.0));
    set_opt(FeatureTag::LowSeismic, row.seismic_pga.map(|p| p <= 0.30));
    set_opt(FeatureTag::HighSafety, row.safety_score.map(|s| s >= 75.0));
    set_opt(FeatureTag::NoHoa, Some(row.hoa_monthly.unwrap_or(0) == 0));

    let mut explicit_tags: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(raw_tags) = row.tags_json.as_deref() {
        if let Ok(arr) = serde_json::from_str::<Vec<String>>(raw_tags) {
            for t in arr {
                let clean: String = t
                    .trim()
                    .to_ascii_lowercase()
                    .replace([' ', '-'], "_")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(48)
                    .collect();
                if clean.is_empty() {
                    continue;
                }
                if let Some(ft) = feature_tag_from_slug(&clean) {
                    let bit = feature_tag_bit(ft);
                    tag_bits |= bit;
                    known_bits |= bit;
                }
                if seen.insert(clean.clone()) {
                    explicit_tags.push(clean);
                }
            }
        }
    }

    (tag_bits, known_bits, explicit_tags)
}

#[allow(clippy::too_many_arguments)]
pub fn build_search_filter_tag_masks(
    fiber_only: bool,
    high_speed_internet_only: bool,
    municipal_electric_only: bool,
    municipal_water_only: bool,
    municipal_sewer_only: bool,
    has_solar: bool,
    has_ev_charger: bool,
    has_heat_pump: bool,
    has_adu: bool,
    adu_feasible: bool,
    has_pool: bool,
    exclude_special_tax: bool,
    required_tags: &[String],
) -> (i64, i64) {
    let mut req = 0_i64;
    let mut excl = 0_i64;
    if fiber_only {
        req |= feature_tag_bit(FeatureTag::FiberAvailable);
    }
    if high_speed_internet_only {
        req |= feature_tag_bit(FeatureTag::HighSpeedInternet);
    }
    if municipal_electric_only {
        req |= feature_tag_bit(FeatureTag::MunicipalElectric);
    }
    if municipal_water_only {
        req |= feature_tag_bit(FeatureTag::MunicipalWater);
    }
    if municipal_sewer_only {
        req |= feature_tag_bit(FeatureTag::MunicipalSewer);
    }
    if has_solar {
        req |= feature_tag_bit(FeatureTag::Solar);
    }
    if has_ev_charger {
        req |= feature_tag_bit(FeatureTag::EvCharger);
    }
    if has_heat_pump {
        req |= feature_tag_bit(FeatureTag::HeatPump);
    }
    if has_adu {
        req |= feature_tag_bit(FeatureTag::Adu);
    }
    if adu_feasible {
        req |= feature_tag_bit(FeatureTag::AduFeasible);
    }
    if has_pool {
        req |= feature_tag_bit(FeatureTag::Pool);
    }
    if exclude_special_tax {
        excl |= feature_tag_bit(FeatureTag::HasSpecialTax);
    }
    for rt in required_tags {
        if let Some(ft) = feature_tag_from_slug(rt) {
            req |= feature_tag_bit(ft);
        }
    }
    (req, excl)
}

// ---------------------------------------------------------------------------------------------
// Lossless `HomeFactors` <-> `(HomeFactorsRowProto, FactorProvenanceProto)` Codec
// ---------------------------------------------------------------------------------------------

fn grade_to_i32(g: Grade) -> i32 {
    match g {
        Grade::Listing => 1,
        Grade::Area => 2,
        Grade::Report => 3,
    }
}

fn grade_from_i32(v: i32) -> Grade {
    match v {
        1 => Grade::Listing,
        2 => Grade::Area,
        _ => Grade::Report,
    }
}

fn school_metric_to_code(m: SchoolMetric) -> i32 {
    match m {
        SchoolMetric::StatePercentile => SchoolMetricKind::SchoolMetricStatePercentile as i32,
        SchoolMetric::CaDashboardTiers => SchoolMetricKind::SchoolMetricCaDashboardTiers as i32,
    }
}

fn school_metric_from_code(code: i32) -> SchoolMetric {
    match SchoolMetricKind::try_from(code).unwrap_or(SchoolMetricKind::SchoolMetricStatePercentile)
    {
        SchoolMetricKind::SchoolMetricCaDashboardTiers => SchoolMetric::CaDashboardTiers,
        _ => SchoolMetric::StatePercentile,
    }
}

fn property_kind_to_code(pk: PropertyKind) -> i32 {
    match pk {
        PropertyKind::SingleFamily => 1,
        PropertyKind::Townhouse => 2,
        PropertyKind::Condo => 3,
        PropertyKind::MultiFamily => 4,
        PropertyKind::Manufactured => 5,
        PropertyKind::Other => 6,
    }
}

fn property_kind_from_code(code: i32) -> Option<PropertyKind> {
    match code {
        1 => Some(PropertyKind::SingleFamily),
        2 => Some(PropertyKind::Townhouse),
        3 => Some(PropertyKind::Condo),
        4 => Some(PropertyKind::MultiFamily),
        5 => Some(PropertyKind::Manufactured),
        6 => Some(PropertyKind::Other),
        _ => None,
    }
}

pub fn home_factors_to_proto(
    listing_sk: i64,
    external_id: &str,
    source_tier: &str,
    completeness_pct: f64,
    walk_score: Option<f64>,
    school_score_override: Option<f64>,
    style_family_override: Option<&str>,
    wildfire_zone_override: Option<&str>,
    zip: Option<&str>,
    lat: Option<f64>,
    lon: Option<f64>,
    scanned_ts: i64,
    updated_ts: i64,
    hf: &HomeFactors,
) -> (HomeFactorsRowProto, FactorProvenanceProto) {
    let mut citations = Vec::new();
    let mut est_mask = 0_i32;

    macro_rules! rec_cite {
        ($key:expr, $bit:expr, $opt:expr) => {
            if let Some(ref f) = $opt {
                if f.estimated && $bit < 15 {
                    est_mask |= 1_i32 << $bit;
                }
                citations.push(FactorCitationProto {
                    field_key: $key.to_string(),
                    source: f.source.clone(),
                    grade: grade_to_i32(f.grade),
                    year: f.year,
                    estimated: f.estimated,
                });
            }
        };
    }

    rec_cite!("cost.monthly_total", 0, hf.cost.monthly_total);
    rec_cite!("cost.price", 1, hf.cost.price);
    rec_cite!("cost.estimate", 2, hf.cost.estimate);
    rec_cite!("cost.list_price", 3, hf.cost.list_price);
    rec_cite!("cost.hoa_monthly", 4, hf.cost.hoa_monthly);
    rec_cite!("schools.elementary", 5, hf.schools.elementary);
    rec_cite!("schools.middle", 6, hf.schools.middle);
    rec_cite!("schools.high", 7, hf.schools.high);
    rec_cite!("crime", 8, hf.crime);
    rec_cite!("convenience", 9, hf.convenience);
    rec_cite!("hazards", 10, hf.hazards);
    rec_cite!("environment", 11, hf.environment);
    rec_cite!("investment", 12, hf.investment);
    rec_cite!("fengshui", 13, hf.fengshui);
    rec_cite!("home.property_type", 15, hf.home.property_type);
    rec_cite!("home.beds", 15, hf.home.beds);
    rec_cite!("home.baths", 15, hf.home.baths);
    rec_cite!("home.sqft", 15, hf.home.sqft);
    rec_cite!("home.lot_sqft", 15, hf.home.lot_sqft);
    rec_cite!("home.stories", 15, hf.home.stories);
    rec_cite!("home.year_built", 15, hf.home.year_built);
    rec_cite!("home.effective_year", 15, hf.home.effective_year);
    rec_cite!("home.adu", 15, hf.home.adu);
    rec_cite!("home.adu_feasibility", 15, hf.home.adu_feasibility);
    rec_cite!("home.parking_spaces", 15, hf.home.parking_spaces);
    rec_cite!("gates.sfha", 15, hf.gates.sfha);
    rec_cite!("gates.very_high_fire", 15, hf.gates.very_high_fire);
    rec_cite!("gates.freeway_within_500ft", 15, hf.gates.freeway_within_500ft);
    rec_cite!("gates.t_junction", 15, hf.gates.t_junction);
    rec_cite!("features", 15, hf.features);

    let school_scores: Vec<f64> = [
        hf.schools.elementary.as_ref().map(|f| f.value.score),
        hf.schools.middle.as_ref().map(|f| f.value.score),
        hf.schools.high.as_ref().map(|f| f.value.score),
    ]
    .into_iter()
    .flatten()
    .collect();
    let computed_school_comp = if school_scores.is_empty() {
        None
    } else {
        Some(school_scores.iter().sum::<f64>() / school_scores.len() as f64)
    };

    let mut style_families_mask = 0_i32;
    let mut style_probabilities = Vec::with_capacity(hf.style.probabilities.len());
    for (&fam, &prob) in &hf.style.probabilities {
        let code = style_family_to_code(fam);
        if (1..=16).contains(&code) && prob > 0.0 {
            style_families_mask |= 1_i32 << (code - 1);
        }
        style_probabilities.push(StyleProbabilityProto {
            family: code,
            probability: prob,
        });
    }

    let style_clues: Vec<StyleClueProto> = hf
        .style
        .clues
        .iter()
        .map(|c| StyleClueProto {
            source: style_basis_to_code(Some(c.source)),
            raw: c.raw.clone(),
            families: c.families.iter().copied().map(style_family_to_code).collect(),
        })
        .collect();

    let primary_style = hf
        .style
        .likely_family()
        .map(style_family_to_code)
        .unwrap_or_else(|| architectural_style_str_to_code(style_family_override));

    let (features_known, features_mask) = match hf.features {
        Some(ref f) => {
            let mut m = 0_i32;
            for (idx, feat) in Feature::ALL.iter().enumerate() {
                if f.value.contains(feat) {
                    m |= 1_i32 << idx;
                }
            }
            (true, m)
        }
        None => (false, 0),
    };

    let mbl = hf.cost.monthly_by_loan.as_ref();
    let row = HomeFactorsRowProto {
        listing_sk,
        external_id: external_id.to_string(),
        schema_version: 3,
        source_tier: factor_tier_to_code(source_tier),
        as_of_year: hf.as_of_year,
        completeness_pct,
        estimated_mask: est_mask,
        monthly_total: hf.cost.monthly_total.as_ref().map(|f| f.value),
        monthly_pi: hf.cost.monthly_principal_interest,
        assumed_down_pct: hf.cost.assumed_down_payment_pct,
        cost_price: hf.cost.price.as_ref().map(|f| f.value),
        estimated_value: hf.cost.estimate.as_ref().map(|f| f.value),
        list_price: hf.cost.list_price.as_ref().map(|f| f.value),
        hoa_monthly: hf.cost.hoa_monthly.as_ref().map(|f| f.value),
        cash_to_close: hf.cost.cash_to_close,
        monthly_conv: mbl.and_then(|x| x.conventional),
        monthly_fha: mbl.and_then(|x| x.fha),
        monthly_va: mbl.and_then(|x| x.va),
        monthly_cash: mbl.and_then(|x| x.cash),
        school_elem_score: hf.schools.elementary.as_ref().map(|f| f.value.score),
        school_elem_metric: hf
            .schools
            .elementary
            .as_ref()
            .map(|f| school_metric_to_code(f.value.metric)),
        school_mid_score: hf.schools.middle.as_ref().map(|f| f.value.score),
        school_mid_metric: hf
            .schools
            .middle
            .as_ref()
            .map(|f| school_metric_to_code(f.value.metric)),
        school_high_score: hf.schools.high.as_ref().map(|f| f.value.score),
        school_high_metric: hf
            .schools
            .high
            .as_ref()
            .map(|f| school_metric_to_code(f.value.metric)),
        school_composite_score: school_score_override.or(computed_school_comp),
        crime_score: hf.crime.as_ref().map(|f| f.value),
        convenience_score: hf.convenience.as_ref().map(|f| f.value),
        walk_score,
        hazards_score: hf.hazards.as_ref().map(|f| f.value),
        environment_score: hf.environment.as_ref().map(|f| f.value),
        investment_score: hf.investment.as_ref().map(|f| f.value),
        fengshui_score: hf.fengshui.as_ref().map(|f| f.value),
        property_kind: hf
            .home
            .property_type
            .as_ref()
            .map(|f| property_kind_to_code(f.value)),
        beds: hf.home.beds.as_ref().map(|f| f.value),
        baths: hf.home.baths.as_ref().map(|f| f.value),
        sqft: hf.home.sqft.as_ref().map(|f| f.value),
        lot_sqft: hf.home.lot_sqft.as_ref().map(|f| f.value),
        stories: hf.home.stories.as_ref().map(|f| f.value),
        year_built: hf.home.year_built.as_ref().map(|f| f.value),
        effective_year: hf.home.effective_year.as_ref().map(|f| f.value),
        has_adu: Some(hf.home.adu.is_some()),
        adu_sqft: hf.home.adu.as_ref().and_then(|f| f.value.sqft),
        adu_beds: hf.home.adu.as_ref().and_then(|f| f.value.beds),
        adu_baths: hf.home.adu.as_ref().and_then(|f| f.value.baths),
        strong_adu_potential: Some(hf.home.strong_adu_potential()),
        main_level_beds: hf.home.main_level_bedrooms,
        main_level_baths: hf.home.main_level_bathrooms,
        parking_spaces: hf.home.parking_spaces.as_ref().map(|f| f.value),
        gate_sfha: hf.gates.sfha.as_ref().map(|f| f.value),
        flood_zone: hf.gates.flood_zone.clone(),
        gate_very_high_fire: hf.gates.very_high_fire.as_ref().map(|f| f.value),
        wildfire_tier: wildfire_zone_override.map(|wz| wildfire_risk_to_code(Some(wz))),
        freeway_distance_m: hf.gates.freeway_distance_m,
        gate_freeway_500ft: hf.gates.freeway_within_500ft.as_ref().map(|f| f.value),
        gate_t_junction: hf.gates.t_junction.as_ref().map(|f| f.value),
        house_number: hf.gates.house_number.clone(),
        style_basis: style_basis_to_code(hf.style.basis),
        primary_style,
        style_families_mask,
        features_known,
        features_mask,
        zip: zip.map(str::to_string),
        lat,
        lon,
        scanned_ts,
        updated_ts,
    };

    let prov = FactorProvenanceProto {
        id: if hf.id.is_empty() {
            external_id.to_string()
        } else {
            hf.id.clone()
        },
        factors_version: if hf.version == 0 {
            FACTORS_VERSION
        } else {
            hf.version
        },
        citations,
        elem_name: hf.schools.elementary.as_ref().map(|f| f.value.name.clone()),
        mid_name: hf.schools.middle.as_ref().map(|f| f.value.name.clone()),
        high_name: hf.schools.high.as_ref().map(|f| f.value.name.clone()),
        style_clues,
        style_probabilities,
        adu_feasibility: hf.home.adu_feasibility.as_ref().map(|f| f.value.clone()),
        wildfire_zone: wildfire_zone_override.map(str::to_string),
        style_family: style_family_override.map(str::to_string),
    };

    (row, prov)
}

pub fn home_factors_from_proto(
    row: &HomeFactorsRowProto,
    prov: Option<&FactorProvenanceProto>,
) -> HomeFactors {
    let default_grade = match FactorSourceTier::try_from(row.source_tier)
        .unwrap_or(FactorSourceTier::FactorTierListing)
    {
        FactorSourceTier::FactorTierListing | FactorSourceTier::FactorTierUnspecified => {
            Grade::Listing
        }
        FactorSourceTier::FactorTierLightScan => Grade::Area,
        FactorSourceTier::FactorTierFullReport => Grade::Report,
    };

    let mut cite_map: BTreeMap<&str, &FactorCitationProto> = BTreeMap::new();
    if let Some(p) = prov {
        for c in &p.citations {
            cite_map.insert(c.field_key.as_str(), c);
        }
    }

    let mk_factor = |key: &str, est_bit: Option<u32>, val: f64| -> Factor<f64> {
        if let Some(c) = cite_map.get(key) {
            let mut f = Factor::new(val, c.source.clone(), grade_from_i32(c.grade)).with_year(c.year);
            if c.estimated {
                f = f.mark_estimated();
            }
            f
        } else {
            let mut f = Factor::new(val, "Open Estate typed factor table", default_grade);
            if let Some(b) = est_bit {
                if (row.estimated_mask & (1_i32 << b)) != 0 {
                    f = f.mark_estimated();
                }
            }
            f
        }
    };

    fn wrap_with_cite<T>(
        cite_map: &BTreeMap<&str, &FactorCitationProto>,
        default_grade: Grade,
        key: &str,
        val: T,
    ) -> Factor<T> {
        if let Some(c) = cite_map.get(key) {
            let mut f = Factor::new(val, c.source.clone(), grade_from_i32(c.grade)).with_year(c.year);
            if c.estimated {
                f = f.mark_estimated();
            }
            f
        } else {
            Factor::new(val, "Open Estate typed factor table", default_grade)
        }
    }

    let monthly_by_loan = if row.monthly_conv.is_some()
        || row.monthly_fha.is_some()
        || row.monthly_va.is_some()
        || row.monthly_cash.is_some()
    {
        Some(LoanMonthlyCosts {
            conventional: row.monthly_conv,
            fha: row.monthly_fha,
            va: row.monthly_va,
            cash: row.monthly_cash,
        })
    } else {
        None
    };

    let cost = CostFactors {
        monthly_total: row
            .monthly_total
            .map(|v| mk_factor("cost.monthly_total", Some(0), v)),
        monthly_principal_interest: row.monthly_pi,
        assumed_down_payment_pct: row.assumed_down_pct,
        price: row.cost_price.map(|v| mk_factor("cost.price", Some(1), v)),
        estimate: row
            .estimated_value
            .map(|v| mk_factor("cost.estimate", Some(2), v)),
        list_price: row
            .list_price
            .map(|v| mk_factor("cost.list_price", Some(3), v)),
        hoa_monthly: row
            .hoa_monthly
            .map(|v| mk_factor("cost.hoa_monthly", Some(4), v)),
        cash_to_close: row.cash_to_close,
        monthly_by_loan,
    };

    let schools = SchoolFactors {
        elementary: row.school_elem_score.map(|sc| {
            wrap_with_cite(
                &cite_map,
                default_grade,
                "schools.elementary",
                SchoolScore {
                    name: prov
                        .and_then(|p| p.elem_name.clone())
                        .unwrap_or_else(|| "Elementary".to_string()),
                    score: sc,
                    metric: school_metric_from_code(row.school_elem_metric.unwrap_or(1)),
                },
            )
        }),
        middle: row.school_mid_score.map(|sc| {
            wrap_with_cite(
                &cite_map,
                default_grade,
                "schools.middle",
                SchoolScore {
                    name: prov
                        .and_then(|p| p.mid_name.clone())
                        .unwrap_or_else(|| "Middle".to_string()),
                    score: sc,
                    metric: school_metric_from_code(row.school_mid_metric.unwrap_or(1)),
                },
            )
        }),
        high: row.school_high_score.map(|sc| {
            wrap_with_cite(
                &cite_map,
                default_grade,
                "schools.high",
                SchoolScore {
                    name: prov
                        .and_then(|p| p.high_name.clone())
                        .unwrap_or_else(|| "High".to_string()),
                    score: sc,
                    metric: school_metric_from_code(row.school_high_metric.unwrap_or(1)),
                },
            )
        }),
    };

    let adu = if row.has_adu == Some(true)
        || row.adu_sqft.is_some()
        || row.adu_beds.is_some()
        || row.adu_baths.is_some()
    {
        Some(wrap_with_cite(
            &cite_map,
            default_grade,
            "home.adu",
            Adu {
                sqft: row.adu_sqft,
                beds: row.adu_beds,
                baths: row.adu_baths,
            },
        ))
    } else {
        None
    };

    let adu_feasibility = prov
        .and_then(|p| p.adu_feasibility.clone())
        .or_else(|| {
            if row.strong_adu_potential == Some(true) {
                Some("Strong".to_string())
            } else {
                None
            }
        })
        .map(|s| wrap_with_cite(&cite_map, default_grade, "home.adu_feasibility", s));

    let home = HomeFitFactors {
        property_type: row
            .property_kind
            .and_then(property_kind_from_code)
            .map(|pk| wrap_with_cite(&cite_map, default_grade, "home.property_type", pk)),
        beds: row.beds.map(|v| mk_factor("home.beds", None, v)),
        baths: row.baths.map(|v| mk_factor("home.baths", None, v)),
        sqft: row.sqft.map(|v| mk_factor("home.sqft", None, v)),
        lot_sqft: row.lot_sqft.map(|v| mk_factor("home.lot_sqft", None, v)),
        stories: row.stories.map(|v| mk_factor("home.stories", None, v)),
        year_built: row
            .year_built
            .map(|v| wrap_with_cite(&cite_map, default_grade, "home.year_built", v)),
        effective_year: row
            .effective_year
            .map(|v| wrap_with_cite(&cite_map, default_grade, "home.effective_year", v)),
        adu,
        adu_feasibility,
        main_level_bedrooms: row.main_level_beds,
        main_level_bathrooms: row.main_level_baths,
        parking_spaces: row
            .parking_spaces
            .map(|v| mk_factor("home.parking_spaces", None, v)),
    };

    let gates = GateInputs {
        sfha: row
            .gate_sfha
            .map(|b| wrap_with_cite(&cite_map, default_grade, "gates.sfha", b)),
        flood_zone: row.flood_zone.clone(),
        very_high_fire: row
            .gate_very_high_fire
            .map(|b| wrap_with_cite(&cite_map, default_grade, "gates.very_high_fire", b)),
        freeway_distance_m: row.freeway_distance_m,
        freeway_within_500ft: row
            .gate_freeway_500ft
            .map(|b| wrap_with_cite(&cite_map, default_grade, "gates.freeway_within_500ft", b)),
        t_junction: row
            .gate_t_junction
            .map(|b| wrap_with_cite(&cite_map, default_grade, "gates.t_junction", b)),
        house_number: row.house_number.clone(),
    };

    let basis = style_basis_from_code(row.style_basis);
    let mut probabilities: BTreeMap<StyleFamily, f64> = BTreeMap::new();
    if let Some(p) = prov.filter(|p| !p.style_probabilities.is_empty()) {
        for sp in &p.style_probabilities {
            if let Some(fam) = style_family_from_code(sp.family) {
                probabilities.insert(fam, sp.probability);
            }
        }
    } else if row.style_families_mask != 0 {
        let mut fams = Vec::new();
        for code in 1..=16 {
            if (row.style_families_mask & (1_i32 << (code - 1))) != 0 {
                if let Some(fam) = style_family_from_code(code) {
                    fams.push(fam);
                }
            }
        }
        if !fams.is_empty() {
            let mass = basis.map(|b| b.trust()).unwrap_or(0.75);
            let each = mass / (fams.len() as f64);
            for fam in fams {
                probabilities.insert(fam, each);
            }
        }
    } else if let Some(fam) = style_family_from_code(row.primary_style) {
        let mass = basis.map(|b| b.trust()).unwrap_or(0.75);
        probabilities.insert(fam, mass);
    }

    let clues = prov
        .map(|p| {
            p.style_clues
                .iter()
                .filter_map(|c| {
                    Some(StyleClue {
                        source: style_basis_from_code(c.source)?,
                        raw: c.raw.clone(),
                        families: c
                            .families
                            .iter()
                            .copied()
                            .filter_map(style_family_from_code)
                            .collect(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let style = StyleEvidence {
        probabilities,
        basis,
        clues,
    };

    let features = if row.features_known {
        let mut set = BTreeSet::new();
        for (idx, feat) in Feature::ALL.iter().enumerate() {
            if (row.features_mask & (1_i32 << idx)) != 0 {
                set.insert(*feat);
            }
        }
        Some(wrap_with_cite(&cite_map, default_grade, "features", set))
    } else {
        None
    };

    HomeFactors {
        id: prov
            .map(|p| p.id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| row.external_id.clone()),
        version: prov
            .map(|p| p.factors_version)
            .filter(|&v| v > 0)
            .unwrap_or(FACTORS_VERSION),
        as_of_year: row.as_of_year,
        cost,
        schools,
        crime: row.crime_score.map(|v| mk_factor("crime", Some(8), v)),
        home,
        convenience: row
            .convenience_score
            .map(|v| mk_factor("convenience", Some(9), v)),
        hazards: row.hazards_score.map(|v| mk_factor("hazards", Some(10), v)),
        environment: row
            .environment_score
            .map(|v| mk_factor("environment", Some(11), v)),
        investment: row
            .investment_score
            .map(|v| mk_factor("investment", Some(12), v)),
        fengshui: row
            .fengshui_score
            .map(|v| mk_factor("fengshui", Some(13), v)),
        gates,
        style,
        features,
    }
}

// ---------------------------------------------------------------------------------------------
// Lossless `Profile` (`scoring::profile::Profile`) <-> `UserProfileProto` Codec
// ---------------------------------------------------------------------------------------------

pub fn buyer_profile_to_proto(
    user_sk: i64,
    user_id: &str,
    profile: &Profile,
    created_ts: i64,
    updated_ts: i64,
) -> UserProfileProto {
    let w = |d: Dimension| profile.weights.get(&d).copied().unwrap_or(0.0);

    let loan_type = match profile.loan_type {
        LoanType::Conventional => 1,
        LoanType::Fha => 2,
        LoanType::Va => 3,
        LoanType::Cash => 4,
    };

    let mut school_levels_mask = 0_i32;
    for (idx, lvl) in SchoolLevel::ALL.iter().enumerate() {
        if profile.school_levels.contains(lvl) {
            school_levels_mask |= 1_i32 << idx;
        }
    }

    let mut property_types_mask = 0_i32;
    for (idx, pk) in PropertyKind::ALL.iter().enumerate() {
        if profile.property_types.contains(pk) {
            property_types_mask |= 1_i32 << idx;
        }
    }

    let mut style_prefs_mask = 0_i32;
    let mut style_avoid_mask = 0_i32;
    let mut style_fine_mask = 0_i32;
    for (&fam, &pref) in &profile.style_preferences {
        let code = style_family_to_code(fam);
        if (1..=16).contains(&code) {
            let bit = 1_i32 << (code - 1);
            match pref {
                StylePreference::Love => style_prefs_mask |= bit,
                StylePreference::Avoid => style_avoid_mask |= bit,
                StylePreference::Fine => style_fine_mask |= bit,
            }
        }
    }

    let mut feature_wishes_mask = 0_i32;
    for (idx, feat) in Feature::ALL.iter().enumerate() {
        if profile.feature_wishes.contains(feat) {
            feature_wishes_mask |= 1_i32 << idx;
        }
    }

    let mut must_have_screens_mask = 0_i32;
    let mut deal_breakers_mask = 0_i32;
    for (idx, scr) in Screen::ALL.iter().enumerate() {
        if profile.must_haves.contains(scr) {
            must_have_screens_mask |= 1_i32 << idx;
        }
        if profile.deal_breakers.contains(scr) {
            deal_breakers_mask |= 1_i32 << idx;
        }
    }

    let mut avoid_digits_mask = 0_i32;
    for &d in &profile.avoid_digits {
        if d <= 9 {
            avoid_digits_mask |= 1_i32 << (d as u32);
        }
    }

    let mut critical_dims_mask = 0_i32;
    for (idx, dim) in Dimension::ALL.iter().enumerate() {
        if profile.critical.contains(dim) {
            critical_dims_mask |= 1_i32 << idx;
        }
    }

    let places = profile
        .places
        .iter()
        .enumerate()
        .map(|(idx, p)| UserProfilePlaceProto {
            place_idx: idx as i32,
            label: p.label.clone(),
            lat: p.lat,
            lon: p.lon,
            travel_mode: match p.mode {
                TravelMode::Drive => 1,
            },
            trips_per_week: p.trips_per_week,
            max_minutes: p.max_minutes,
        })
        .collect();

    UserProfileProto {
        user_sk,
        user_id: user_id.to_string(),
        version: profile.version as i32,
        preset_code: profile.preset.clone(),
        w_commute: w(Dimension::Commute),
        w_cost: w(Dimension::Cost),
        w_schools: w(Dimension::Schools),
        w_convenience: w(Dimension::Convenience),
        w_homefit: w(Dimension::Homefit),
        w_style: w(Dimension::Style),
        w_hazards: w(Dimension::Hazards),
        w_crime: w(Dimension::Crime),
        w_environment: w(Dimension::Environment),
        w_investment: w(Dimension::Investment),
        w_fengshui: w(Dimension::Fengshui),
        budget_monthly: profile.budget.monthly,
        budget_max_price: profile.budget.max_price,
        loan_type,
        down_payment_pct: profile.down_payment_pct,
        cash_available: None,
        comfort_ratio: 1.0,
        school_levels_mask,
        min_beds: profile.min_beds,
        min_baths: profile.min_baths,
        min_sqft: profile.min_sqft,
        min_lot_sqft: profile.min_lot_sqft,
        single_story_pref: false,
        multigenerational: profile.multigenerational,
        hoa_max_monthly: profile.hoa_max_monthly,
        property_types_mask,
        style_prefs_mask,
        style_avoid_mask,
        feature_wishes_mask,
        must_have_screens_mask,
        deal_breakers_mask,
        avoid_digits_mask,
        prefer_digits_mask: 0,
        critical_dims_mask,
        traffic_buffer: profile.traffic_buffer,
        places,
        created_ts,
        updated_ts,
        style_fine_mask,
    }
}

pub fn buyer_profile_from_proto(p: &UserProfileProto) -> Profile {
    let mut weights = BTreeMap::new();
    let pairs = [
        (Dimension::Commute, p.w_commute),
        (Dimension::Cost, p.w_cost),
        (Dimension::Schools, p.w_schools),
        (Dimension::Convenience, p.w_convenience),
        (Dimension::Homefit, p.w_homefit),
        (Dimension::Style, p.w_style),
        (Dimension::Hazards, p.w_hazards),
        (Dimension::Crime, p.w_crime),
        (Dimension::Environment, p.w_environment),
        (Dimension::Investment, p.w_investment),
        (Dimension::Fengshui, p.w_fengshui),
    ];
    for (dim, val) in pairs {
        if val > 0.0 {
            weights.insert(dim, val);
        }
    }

    let loan_type = match p.loan_type {
        2 => LoanType::Fha,
        3 => LoanType::Va,
        4 => LoanType::Cash,
        _ => LoanType::Conventional,
    };

    let mut school_levels = Vec::new();
    for (idx, lvl) in SchoolLevel::ALL.iter().enumerate() {
        if (p.school_levels_mask & (1_i32 << idx)) != 0 {
            school_levels.push(*lvl);
        }
    }

    let mut property_types = Vec::new();
    for (idx, pk) in PropertyKind::ALL.iter().enumerate() {
        if (p.property_types_mask & (1_i32 << idx)) != 0 {
            property_types.push(*pk);
        }
    }

    let mut style_preferences = BTreeMap::new();
    for code in 1..=16 {
        if let Some(fam) = style_family_from_code(code) {
            let bit = 1_i32 << (code - 1);
            if (p.style_prefs_mask & bit) != 0 {
                style_preferences.insert(fam, StylePreference::Love);
            } else if (p.style_avoid_mask & bit) != 0 {
                style_preferences.insert(fam, StylePreference::Avoid);
            } else if (p.style_fine_mask & bit) != 0 {
                style_preferences.insert(fam, StylePreference::Fine);
            }
        }
    }

    let mut feature_wishes = Vec::new();
    for (idx, feat) in Feature::ALL.iter().enumerate() {
        if (p.feature_wishes_mask & (1_i32 << idx)) != 0 {
            feature_wishes.push(*feat);
        }
    }

    let mut must_haves = Vec::new();
    let mut deal_breakers = Vec::new();
    for (idx, scr) in Screen::ALL.iter().enumerate() {
        if (p.must_have_screens_mask & (1_i32 << idx)) != 0 {
            must_haves.push(*scr);
        }
        if (p.deal_breakers_mask & (1_i32 << idx)) != 0 {
            deal_breakers.push(*scr);
        }
    }

    let mut avoid_digits = Vec::new();
    for d in 0_u8..=9 {
        if (p.avoid_digits_mask & (1_i32 << (d as u32))) != 0 {
            avoid_digits.push(d);
        }
    }

    let mut critical = Vec::new();
    for (idx, dim) in Dimension::ALL.iter().enumerate() {
        if (p.critical_dims_mask & (1_i32 << idx)) != 0 {
            critical.push(*dim);
        }
    }

    let mut sorted_places = p.places.clone();
    sorted_places.sort_by_key(|pl| pl.place_idx);
    let places = sorted_places
        .into_iter()
        .map(|pl| Place {
            label: pl.label,
            lat: pl.lat,
            lon: pl.lon,
            max_minutes: pl.max_minutes,
            trips_per_week: pl.trips_per_week,
            mode: TravelMode::Drive,
        })
        .collect();

    Profile {
        version: if p.version > 0 { p.version as u32 } else { 1 },
        weights,
        preset: p.preset_code.clone(),
        budget: Budget {
            max_price: p.budget_max_price,
            monthly: p.budget_monthly,
        },
        down_payment_pct: p.down_payment_pct,
        loan_type,
        min_beds: p.min_beds,
        min_baths: p.min_baths,
        min_sqft: p.min_sqft,
        min_lot_sqft: p.min_lot_sqft,
        property_types,
        multigenerational: p.multigenerational,
        school_levels,
        places,
        traffic_buffer: if p.traffic_buffer > 0.0 {
            p.traffic_buffer
        } else {
            1.3
        },
        style_preferences,
        must_haves,
        deal_breakers,
        hoa_max_monthly: p.hoa_max_monthly,
        avoid_digits,
        feature_wishes,
        critical,
    }
}
