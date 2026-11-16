// Commercial Real Estate (CRE) Asset Classification, Institutional Financial Underwriting,
// Commercial Land & Agricultural/Farmland Valuation, Store Profit & Plaza Co-Tenancy Simulator,
// Trade-Area / Frontage AADT / Site Envelope Feasibility, and CRE Document AI Reconciliation
// ($1M–$25M Multifamily 5+, Mixed-Use, Retail, Office/Medical, Industrial/Warehouse/Flex,
// Hospitality, Commercial Land, and Agricultural / Farmland).

pub mod business_profit;
pub mod documents;
pub mod land_farm;
pub mod trade_area;
pub mod underwriting;

pub use business_profit::{
    analyze_location_business_profit, analyze_location_business_profit_live,
    build_plaza_overpass_query, evaluate_store_profit, extract_pois_from_report_amenities,
    fetch_osm_plaza_pois, parse_overpass_plaza_elements, seed_osm_plaza_cache, ObservedStorePoi,
    StoreConcept, StoreEvalRequest,
};
pub use documents::{
    analyze_cre_documents, parse_cre_documents_deterministic, redact_cre_document_pii,
    CrePiiRedactionResult,
};
pub use land_farm::{
    evaluate_commercial_land, evaluate_farmland, evaluate_land_and_farm,
    evaluate_land_and_farm_offline, fetch_usda_soil_profile, infer_baseline_soil_profile,
    lookup_nass_state_benchmark, seed_usda_soil_cache, LandFarmEvalOptions, UsdaSoilProfile,
};
pub use trade_area::{analyze_cre_trade_area, CreTradeAreaSummary};
pub use underwriting::{
    run_cre_underwriting, CreLeaseStructure, CreUnderwritingInput, CreUnderwritingOverrides,
};

use crate::shared::{EnvConfig, HttpClient, ModuleOutput, SourceItem};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreAssetClass {
    #[serde(rename = "multifamily_5_plus", alias = "multifamily")]
    Multifamily5Plus,
    #[serde(rename = "mixed_use", alias = "mixed-use")]
    MixedUse,
    #[serde(rename = "retail", alias = "strip_center")]
    Retail,
    #[serde(rename = "office", alias = "medical_condo")]
    Office,
    #[serde(rename = "industrial", alias = "warehouse")]
    Industrial,
    #[serde(rename = "hospitality", alias = "hotel")]
    Hospitality,
    #[serde(rename = "commercial_land", alias = "land")]
    CommercialLand,
    #[serde(
        rename = "agricultural_farm",
        alias = "farmland",
        alias = "farm",
        alias = "ag_land",
        alias = "ranch"
    )]
    AgriculturalFarmLand,
}

impl CreAssetClass {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Multifamily5Plus => "Multifamily (5+ Units)",
            Self::MixedUse => "Mixed-Use",
            Self::Retail => "Retail / Strip Center",
            Self::Office => "Office / Medical Condo",
            Self::Industrial => "Industrial / Warehouse / Flex",
            Self::Hospitality => "Hospitality",
            Self::CommercialLand => "Commercial Land",
            Self::AgriculturalFarmLand => "Agricultural / Farmland",
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Multifamily5Plus => "multifamily_5_plus",
            Self::MixedUse => "mixed_use",
            Self::Retail => "retail",
            Self::Office => "office",
            Self::Industrial => "industrial",
            Self::Hospitality => "hospitality",
            Self::CommercialLand => "commercial_land",
            Self::AgriculturalFarmLand => "agricultural_farm",
        }
    }

    pub fn parse_explicit(raw: &str) -> Option<Self> {
        let norm: String = raw
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let norm = norm.trim_matches('_');
        if norm.is_empty() {
            return None;
        }
        if norm.contains("farmland")
            || norm.contains("agricultural")
            || norm.contains("ag_land")
            || norm == "ag"
            || norm.contains("farm")
            || norm.contains("ranch")
            || norm.contains("orchard")
            || norm.contains("vineyard")
            || norm.contains("cropland")
            || norm.contains("pasture")
            || norm.contains("timberland")
            || norm == "timber"
        {
            return Some(Self::AgriculturalFarmLand);
        }
        if norm.contains("commercial_land")
            || norm.contains("industrial_land")
            || norm.contains("retail_land")
            || norm.contains("entitled_land")
            || norm == "land"
            || norm.contains("pad_site")
            || norm.contains("vacant_commercial")
        {
            return Some(Self::CommercialLand);
        }
        if norm.contains("multifamily")
            || norm.contains("multi_family")
            || norm.contains("apartment")
            || norm == "mf"
            || norm == "mf5"
            || norm.contains("5_unit")
            || norm.contains("5_plus")
        {
            return Some(Self::Multifamily5Plus);
        }
        if norm.contains("mixed") || norm == "mu" || norm == "mxd" || norm.contains("live_work") {
            return Some(Self::MixedUse);
        }
        if norm.contains("retail")
            || norm.contains("strip")
            || norm.contains("shopping")
            || norm.contains("storefront")
            || norm.contains("qsr")
            || norm.contains("restaurant")
        {
            return Some(Self::Retail);
        }
        if norm.contains("office")
            || norm.contains("medical")
            || norm.contains("dental")
            || norm.contains("clinic")
        {
            return Some(Self::Office);
        }
        if norm.contains("industrial")
            || norm.contains("warehouse")
            || norm.contains("flex")
            || norm.contains("logistics")
            || norm.contains("distribution")
            || norm.contains("manufacturing")
            || norm.contains("self_storage")
        {
            return Some(Self::Industrial);
        }
        if norm.contains("hospitality")
            || norm.contains("hotel")
            || norm.contains("motel")
            || norm.contains("lodging")
        {
            return Some(Self::Hospitality);
        }
        None
    }

    pub fn default_lease_structure(&self) -> CreLeaseStructure {
        match self {
            Self::Multifamily5Plus | Self::Hospitality => CreLeaseStructure::FullServiceGross,
            Self::MixedUse | Self::Office => CreLeaseStructure::ModifiedGross,
            Self::Retail | Self::Industrial | Self::CommercialLand | Self::AgriculturalFarmLand => {
                CreLeaseStructure::Nnn
            }
        }
    }

    pub fn default_vacancy_pct(&self) -> f64 {
        match self {
            Self::Multifamily5Plus => 5.0,
            Self::MixedUse => 6.5,
            Self::Retail => 7.0,
            Self::Industrial => 7.0,
            Self::Office => 12.0,
            Self::Hospitality => 22.0,
            Self::CommercialLand => 5.0,
            Self::AgriculturalFarmLand => 3.0,
        }
    }

    pub fn is_sba_owner_user_compatible(&self) -> bool {
        matches!(
            self,
            Self::Retail | Self::Office | Self::Industrial | Self::Hospitality | Self::MixedUse
        )
    }

    pub fn is_agency_multifamily_eligible(&self, units: u32) -> bool {
        match self {
            Self::Multifamily5Plus => units >= 5,
            Self::MixedUse => units >= 5,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreClassification {
    pub is_cre: bool,
    pub asset_class: Option<CreAssetClass>,
    pub asset_class_label: Option<String>,
    pub asset_class_code: Option<String>,
    pub confidence: String,
    pub detected_units: Option<u32>,
    pub detected_zoning: Option<String>,
    pub detected_use_code: Option<String>,
    pub triggers: Vec<String>,
    pub residential_reason: Option<String>,
    pub sba_owner_user_compatible: bool,
    pub agency_multifamily_eligible: bool,
}

static UNIT_COUNT_IN_TEXT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\d{1,4})\s*[-]?\s*(?:units?|apt|apartments?|plex)\b")
        .expect("unit count in text regex")
});

static ZONING_TOKEN_SPLIT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\s,;/()\[\]]+").expect("zoning token split regex"));

static ZONE_INDUSTRIAL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:M-?[123][A-Z0-9-]*|I-?[123LHG][A-Z0-9-]*|IL[A-Z0-9-]*|IH[A-Z0-9-]*|IG[A-Z0-9-]*|LI[A-Z0-9-]*|GI[A-Z0-9-]*|HI[A-Z0-9-]*|IND[A-Z0-9-]*|CM[A-Z0-9-]*|MP[A-Z0-9-]*|IP[A-Z0-9-]*)$",
    )
    .expect("industrial zone regex")
});

static ZONE_OFFICE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:C-?O[A-Z0-9-]*|CO|OP[A-Z0-9-]*|PO[A-Z0-9-]*|BP[A-Z0-9-]*|O-?[123][A-Z0-9-]*)$",
    )
    .expect("office zone regex")
});

static ZONE_MIXED_USE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:MU[A-Z0-9-]*|MX[A-Z0-9-]*|MXD[A-Z0-9-]*|CMU[A-Z0-9-]*|DMU[A-Z0-9-]*|NC-?[123T][A-Z0-9-]*|NCT[A-Z0-9-]*)$",
    )
    .expect("mixed use zone regex")
});

static ZONE_COMMERCIAL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:C-?[12345][A-Z0-9-]*|CG[A-Z0-9-]*|CN[A-Z0-9-]*|CR[A-Z0-9-]*|CC[A-Z0-9-]*|CS[A-Z0-9-]*|CB[A-Z0-9-]*|CBD[A-Z0-9-]*|CP[A-Z0-9-]*|CH[A-Z0-9-]*|B-?[1234][A-Z0-9-]*|BG[A-Z0-9-]*|BN[A-Z0-9-]*|BC[A-Z0-9-]*|COM[A-Z0-9-]*)$",
    )
    .expect("commercial zone regex")
});

static ZONE_HIGH_DENSITY_MF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:R-?[45][A-Z0-9-]*|RM-?[345][A-Z0-9-]*|RMF-?[345][A-Z0-9-]*|MF-?[345][A-Z0-9-]*|HDR[A-Z0-9-]*|RH-?[45][A-Z0-9-]*)$",
    )
    .expect("high density mf zone regex")
});

static ZONE_AGRICULTURAL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:A-?(?:1|2|3|4|5|10|20|40|80|160)(?:-[A-Z0-9]+)?|AG(?:-?[A-Z0-9]+)?|AE(?:-?[A-Z0-9]+)?|AP(?:-?[0-9]+)?|AL(?:-?[0-9]+)?|AW(?:-?[0-9]+)?|EFU(?:-?[A-Z0-9]+)?|FP(?:-?[0-9]+)?|F-?1(?:-?[A-Z0-9]+)?|TPZ(?:-?[0-9]+)?)$",
    )
    .expect("agricultural zone regex")
});

fn read_u32_field(v: &Value, keys: &[&str]) -> Option<u32> {
    for k in keys {
        if let Some(val) = v.get(*k) {
            if let Some(n) = val.as_u64() {
                if n > 0 && n <= 50_000 {
                    return Some(n as u32);
                }
            } else if let Some(f) = val.as_f64() {
                if f >= 1.0 && f <= 50_000.0 {
                    return Some(f.round() as u32);
                }
            } else if let Some(s) = val.as_str() {
                if let Ok(parsed) = s.trim().replace(',', "").parse::<u32>() {
                    if parsed > 0 && parsed <= 50_000 {
                        return Some(parsed);
                    }
                }
            }
        }
    }
    None
}

fn read_f64_field(v: &Value, keys: &[&str]) -> Option<f64> {
    for k in keys {
        if let Some(val) = v.get(*k) {
            if let Some(f) = val.as_f64() {
                if f.is_finite() {
                    return Some(f);
                }
            } else if let Some(i) = val.as_i64() {
                return Some(i as f64);
            } else if let Some(s) = val.as_str() {
                let clean: String = s
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                    .collect();
                if let Ok(parsed) = clean.parse::<f64>() {
                    if parsed.is_finite() {
                        return Some(parsed);
                    }
                }
            }
        }
    }
    None
}

fn read_string_field(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v
            .get(*k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_string());
        }
    }
    None
}

pub fn extract_unit_count(facts: &Value) -> Option<u32> {
    if let Some(explicit) = read_u32_field(
        facts,
        &[
            "units",
            "unitCount",
            "numberOfUnits",
            "totalUnits",
            "numUnits",
        ],
    ) {
        return Some(explicit);
    }

    let res_units = read_u32_field(facts, &["residentialUnits"]).unwrap_or(0);
    let com_units = read_u32_field(facts, &["commercialUnits"]).unwrap_or(0);
    if res_units + com_units > 0 {
        return Some(res_units + com_units);
    }

    if let Some(reg_cnt) = facts
        .pointer("/buildingRegister/totalUnits")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0 && n <= 50_000)
    {
        return Some(reg_cnt as u32);
    }

    let combined = [
        read_string_field(facts, &["propertyType", "propertySubType"]),
        read_string_field(facts, &["useDescription", "landUseDescription", "landUse"]),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");

    if !combined.is_empty() {
        let low = combined.to_ascii_lowercase();
        if let Some(caps) = UNIT_COUNT_IN_TEXT_RE.captures(&low) {
            if let Ok(n) = caps[1].parse::<u32>() {
                if n > 0 && n <= 10_000 {
                    return Some(n);
                }
            }
        }
        if low.contains("fourplex") || low.contains("quadplex") || low.contains("quadruplex") {
            return Some(4);
        }
        if low.contains("triplex") {
            return Some(3);
        }
        if low.contains("duplex") {
            return Some(2);
        }
        if low.contains("single family")
            || low.contains("single-family")
            || low.contains("townhouse")
            || low.contains("townhome")
            || low
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == "sfr")
        {
            return Some(1);
        }
    }

    None
}

fn classify_zoning_code(
    zoning_raw: &str,
    lot_sqft: Option<f64>,
    sqft: Option<f64>,
) -> Option<(CreAssetClass, String)> {
    let rural_acreage = lot_sqft.unwrap_or(0.0) >= 5.0 * 43_560.0
        || (sqft.unwrap_or(1.0) <= 0.0 && lot_sqft.unwrap_or(0.0) >= 2.0 * 43_560.0);

    for tok in ZONING_TOKEN_SPLIT_RE.split(zoning_raw.trim()) {
        let t = tok.trim().trim_matches('-');
        if t.is_empty() {
            continue;
        }
        let up = t.to_ascii_uppercase();
        if (up == "RA" || up.starts_with("RA-") || up == "R-A" || up.starts_with("R-A-"))
            && rural_acreage
        {
            return Some((CreAssetClass::AgriculturalFarmLand, up));
        }
        // Guard against single-family residential zones like R-1-8(PD), R-1, R-2, R-3, RS, RE, RD, PUD
        if up.starts_with("R-1")
            || up.starts_with("R1")
            || up.starts_with("R-2")
            || up.starts_with("R2")
            || up.starts_with("R-3")
            || up.starts_with("R3")
            || up.starts_with("RS")
            || up.starts_with("RE")
            || up.starts_with("RA")
            || up.starts_with("RD")
            || up == "PD"
            || up == "PUD"
            || up == "SFR"
        {
            continue;
        }
        if ZONE_AGRICULTURAL_RE.is_match(&up) {
            return Some((CreAssetClass::AgriculturalFarmLand, up));
        }
        if ZONE_INDUSTRIAL_RE.is_match(&up) {
            return Some((CreAssetClass::Industrial, up));
        }
        if ZONE_MIXED_USE_RE.is_match(&up) {
            return Some((CreAssetClass::MixedUse, up));
        }
        if ZONE_OFFICE_RE.is_match(&up) {
            return Some((CreAssetClass::Office, up));
        }
        if ZONE_COMMERCIAL_RE.is_match(&up) {
            return Some((CreAssetClass::Retail, up));
        }
        if ZONE_HIGH_DENSITY_MF_RE.is_match(&up) {
            return Some((CreAssetClass::Multifamily5Plus, up));
        }
    }
    None
}

fn has_agricultural_description(low: &str) -> bool {
    low.contains("farmland")
        || low.contains("farm land")
        || low.contains("agricultural")
        || low.contains("ag land")
        || low.contains("orchard")
        || low.contains("vineyard")
        || low.contains("pasture")
        || low.contains("cropland")
        || low.contains("crop land")
        || low.contains("row crop")
        || low.contains("timberland")
        || low.contains("timber land")
        || low.contains("rangeland")
        || low.contains("grazing")
        || low.contains("cattle ranch")
        || low.contains("working ranch")
        || low.contains("irrigated farm")
        || low.contains("dry farm")
        || (low.contains("timber") && !low.contains("timber frame") && !low.contains("timberline"))
        || (low
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w == "ranch")
            && !low.contains("ranch home")
            && !low.contains("ranch house")
            && !low.contains("ranch style")
            && !low.contains("ranch-style")
            && !low.contains("raised ranch")
            && !low.contains("brick ranch")
            && !low.contains("rambler")
            && !low.contains("single family")
            && !low.contains("single-family")
            && !low.contains("condo")
            && !low.contains("townhome")
            && !low.contains("townhouse"))
}

fn classify_use_code_and_description(
    use_code: Option<&str>,
    combined_desc: &str,
    units: Option<u32>,
    sqft: Option<f64>,
) -> Option<(CreAssetClass, String)> {
    let low = combined_desc.to_ascii_lowercase();
    let code_up = use_code.unwrap_or("").trim().to_ascii_uppercase();
    let ag_desc = has_agricultural_description(&low);

    // Check explicit small-residential exclusions first unless commercial or agricultural keywords appear
    let is_explicit_small_res = (low.contains("single family")
        || low.contains("single-family")
        || low == "sfr"
        || low.contains("townhouse")
        || low.contains("townhome")
        || low.contains("duplex")
        || low.contains("triplex")
        || low.contains("fourplex")
        || low.contains("quadplex"))
        && !low.contains("mixed")
        && !low.contains("retail")
        && !low.contains("commercial")
        && !low.contains("office")
        && !low.contains("medical")
        && !low.contains("industrial")
        && !low.contains("warehouse")
        && !ag_desc;

    if !is_explicit_small_res {
        if ag_desc
            && !low.contains("commercial")
            && !low.contains("retail")
            && !low.contains("industrial")
        {
            return Some((
                CreAssetClass::AgriculturalFarmLand,
                "Assessor/property description indicates agricultural, farmland, orchard, vineyard, or ranch land"
                    .to_string(),
            ));
        }

        if low.contains("hotel")
            || low.contains("motel")
            || low.contains("hospitality")
            || low.contains("lodging")
            || low.contains("extended stay")
            || low.contains("resort")
        {
            return Some((
                CreAssetClass::Hospitality,
                "Assessor/property description indicates hospitality/lodging asset".to_string(),
            ));
        }

        if low.contains("mixed-use")
            || low.contains("mixed use")
            || low.contains("live/work")
            || low.contains("live-work")
            || low.contains("store & residential")
            || low.contains("store/residential")
            || low.contains("retail/residential")
            || low.contains("office/residential")
            || low.contains("commercial/residential")
        {
            return Some((
                CreAssetClass::MixedUse,
                "Assessor/property description indicates mixed-use commercial/residential property"
                    .to_string(),
            ));
        }

        if low.contains("vacant commercial")
            || low.contains("commercial land")
            || low.contains("industrial land")
            || low.contains("retail land")
            || low.contains("entitled land")
            || low.contains("vacant industrial")
            || low.contains("commercial lot")
            || low.contains("pad site")
        {
            return Some((
                CreAssetClass::CommercialLand,
                "Assessor/property description indicates unimproved commercial/industrial land"
                    .to_string(),
            ));
        }

        if low.contains("industrial")
            || low.contains("warehouse")
            || low.contains("distribution")
            || low.contains("logistics")
            || low.contains("flex")
            || low.contains("manufacturing")
            || low.contains("cold storage")
            || low.contains("self-storage")
            || low.contains("self storage")
            || low.contains("mini-storage")
            || low.contains("truck terminal")
            || low.contains("r&d")
            || low.contains("auto body")
        {
            return Some((
                CreAssetClass::Industrial,
                "Assessor/property description indicates industrial, warehouse, or flex asset"
                    .to_string(),
            ));
        }

        if low.contains("medical condo")
            || low.contains("medical office")
            || low.contains("office condo")
            || low.contains("commercial condo")
            || low.contains("office building")
            || low.contains("professional building")
            || low.contains("dental office")
            || low == "office"
            || (low.contains("office") && !low.contains("home office"))
        {
            return Some((
                CreAssetClass::Office,
                "Assessor/property description indicates office or medical condo asset".to_string(),
            ));
        }

        if low.contains("retail")
            || low.contains("strip center")
            || low.contains("shopping center")
            || low.contains("neighborhood center")
            || low.contains("storefront")
            || low.contains("supermarket")
            || low.contains("restaurant")
            || low.contains("fast food")
            || low.contains("drive-thru")
            || low.contains("qsr")
            || low.contains("bank branch")
            || low.contains("service station")
            || low.contains("gas station")
            || low.contains("car wash")
            || low.contains("auto repair")
            || low.contains("convenience store")
            || low.contains("pharmacy")
        {
            return Some((
                CreAssetClass::Retail,
                "Assessor/property description indicates retail, strip center, or service commercial asset"
                    .to_string(),
            ));
        }

        if low.contains("5+ units")
            || low.contains("five or more")
            || low.contains("apartment")
            || low.contains("garden apt")
            || low.contains("student housing")
            || low.contains("senior housing")
            || low.contains("assisted living")
            || low.contains("mobile home park")
        {
            if units.map(|u| u >= 5).unwrap_or(true) {
                return Some((
                    CreAssetClass::Multifamily5Plus,
                    "Assessor/property description indicates 5+ unit commercial multifamily apartment asset"
                        .to_string(),
                ));
            }
        }

        if (low.contains("multifamily") || low.contains("multi-family"))
            && units.is_none()
            && sqft.unwrap_or(0.0) >= 4_200.0
        {
            return Some((
                CreAssetClass::Multifamily5Plus,
                "Multi-family property with commercial apartment scale (>= 4,200 sq ft)"
                    .to_string(),
            ));
        }
    }

    // Standard County Assessor / IAAO commercial & multifamily use code prefixes
    if !code_up.is_empty() {
        if matches!(
            code_up.as_str(),
            "05" | "005" | "0500" | "MFR5" | "APT" | "APT5" | "MF5" | "411"
        ) {
            return Some((
                CreAssetClass::Multifamily5Plus,
                format!("County Assessor use code '{code_up}' maps to 5+ unit multifamily"),
            ));
        }
        if code_up.starts_with("IND")
            || code_up.starts_with("WHSE")
            || matches!(
                code_up.as_str(),
                "3000" | "3100" | "3200" | "3300" | "400" | "410" | "420" | "480"
            )
        {
            return Some((
                CreAssetClass::Industrial,
                format!("County Assessor use code '{code_up}' maps to industrial/warehouse"),
            ));
        }
        if code_up.starts_with("RET")
            || matches!(
                code_up.as_str(),
                "1100" | "1200" | "1400" | "1500" | "1600" | "200" | "210" | "220"
            )
        {
            return Some((
                CreAssetClass::Retail,
                format!("County Assessor use code '{code_up}' maps to retail commercial"),
            ));
        }
        if code_up.starts_with("OFF")
            || matches!(code_up.as_str(), "1700" | "1800" | "1900" | "230" | "240")
        {
            return Some((
                CreAssetClass::Office,
                format!("County Assessor use code '{code_up}' maps to commercial/medical office"),
            ));
        }
        if code_up.starts_with("HOT") || matches!(code_up.as_str(), "2100" | "300" | "250") {
            return Some((
                CreAssetClass::Hospitality,
                format!("County Assessor use code '{code_up}' maps to hospitality/hotel"),
            ));
        }
        if code_up.starts_with("MIX") || matches!(code_up.as_str(), "1210" | "0700" | "280") {
            return Some((
                CreAssetClass::MixedUse,
                format!("County Assessor use code '{code_up}' maps to mixed-use"),
            ));
        }
        if matches!(
            code_up.as_str(),
            "1000" | "3000V" | "VAC-COM" | "CLND" | "COM-LND"
        ) {
            return Some((
                CreAssetClass::CommercialLand,
                format!("County Assessor use code '{code_up}' maps to commercial land"),
            ));
        }
        if code_up.starts_with("AG")
            || code_up.starts_with("FARM")
            || code_up.starts_with("ORCH")
            || code_up.starts_with("VINE")
            || matches!(
                code_up.as_str(),
                "5000"
                    | "5100"
                    | "5200"
                    | "5300"
                    | "5400"
                    | "5500"
                    | "6000"
                    | "6100"
                    | "700"
                    | "710"
                    | "720"
                    | "750"
                    | "800"
                    | "VAC-AG"
            )
        {
            return Some((
                CreAssetClass::AgriculturalFarmLand,
                format!("County Assessor use code '{code_up}' maps to agricultural/farmland"),
            ));
        }
    }

    None
}

/// Automatically or explicitly classifies whether a parcel is a Commercial Real Estate (CRE)
/// or 5+ Unit Investment Multifamily asset vs. 1–4 Unit Residential.
pub fn classify_cre_asset(
    facts: &Value,
    _geo: Option<&Value>,
    explicit_asset_class: Option<&str>,
    force_cre_mode: bool,
) -> CreClassification {
    let units = extract_unit_count(facts);
    let sqft = read_f64_field(
        facts,
        &["sqft", "grossSqft", "rentableSqft", "buildingSqft"],
    );
    let lot_sqft = read_f64_field(facts, &["lotSqft", "landSqft"]);
    let zoning = read_string_field(facts, &["zoning", "zoningCode", "zone"]);
    let use_code = read_string_field(
        facts,
        &[
            "useCode",
            "landUseCode",
            "countyUseCode",
            "assessorUseCode",
            "propertyCode",
        ],
    );

    let prop_type = read_string_field(facts, &["propertyType", "propertySubType", "buildingType"]);
    let use_desc = read_string_field(
        facts,
        &["useDescription", "landUseDescription", "landUse", "subType"],
    );
    let combined_desc = [prop_type.as_deref(), use_desc.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" | ");
    let low_desc = combined_desc.to_ascii_lowercase();

    let mut triggers: Vec<String> = Vec::new();

    // 1. Explicit asset_class parameter always wins
    if let Some(raw_cls) = explicit_asset_class
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(cls) = CreAssetClass::parse_explicit(raw_cls) {
            triggers.push(format!("Explicit asset_class override: {}", cls.label()));
            let eff_units = units.unwrap_or(if cls == CreAssetClass::Multifamily5Plus {
                8
            } else {
                1
            });
            return CreClassification {
                is_cre: true,
                asset_class: Some(cls),
                asset_class_label: Some(cls.label().to_string()),
                asset_class_code: Some(cls.code().to_string()),
                confidence: "explicit_override".to_string(),
                detected_units: units,
                detected_zoning: zoning,
                detected_use_code: use_code,
                triggers,
                residential_reason: None,
                sba_owner_user_compatible: cls.is_sba_owner_user_compatible(),
                agency_multifamily_eligible: cls.is_agency_multifamily_eligible(eff_units),
            };
        }
    }

    // 2. Mixed residential + commercial unit count check
    let res_units = read_u32_field(facts, &["residentialUnits"]).unwrap_or(0);
    let com_units = read_u32_field(facts, &["commercialUnits"]).unwrap_or(0);
    let mut detected_class: Option<CreAssetClass> = None;

    if res_units > 0 && com_units > 0 {
        detected_class = Some(CreAssetClass::MixedUse);
        triggers.push(format!(
            "Mixed commercial ({com_units}) and residential ({res_units}) units"
        ));
    }

    // 3. Assessor use code & property type / use description
    if let Some((cls, reason)) =
        classify_use_code_and_description(use_code.as_deref(), &combined_desc, units, sqft)
    {
        if detected_class.is_none() {
            detected_class = Some(cls);
        }
        triggers.push(reason);
    }

    // 4. Unit count >= 5 check (Investment Multifamily 5+)
    if let Some(u) = units {
        if u >= 5 {
            triggers.push(format!(
                "Unit count ({u} units) >= 5 commercial multifamily threshold"
            ));
            if detected_class.is_none() {
                detected_class = Some(CreAssetClass::Multifamily5Plus);
            }
        }
    }

    // 5. Commercial / Industrial / Mixed-Use / High-Density Zoning check
    let is_explicit_1_to_4_residential = (low_desc.contains("single family")
        || low_desc.contains("single-family")
        || low_desc == "sfr"
        || (low_desc.contains("condo")
            && !low_desc.contains("medical")
            && !low_desc.contains("office")
            && !low_desc.contains("commercial")
            && !low_desc.contains("retail"))
        || low_desc.contains("townhouse")
        || low_desc.contains("townhome")
        || low_desc.contains("duplex")
        || low_desc.contains("triplex")
        || low_desc.contains("fourplex"))
        && units.unwrap_or(1) < 5;

    if let Some(ref z_str) = zoning {
        if let Some((z_cls, z_tok)) = classify_zoning_code(z_str, lot_sqft, sqft) {
            // For R-4/R-5 high-density residential or legacy suburban A-1 zones, avoid false-flagging an explicit 1-4 unit condo/SFR
            let allow_zone_trigger = if z_cls == CreAssetClass::Multifamily5Plus {
                !is_explicit_1_to_4_residential
            } else if z_cls == CreAssetClass::MixedUse {
                !is_explicit_1_to_4_residential || force_cre_mode
            } else if z_cls == CreAssetClass::AgriculturalFarmLand {
                !is_explicit_1_to_4_residential
                    || lot_sqft.unwrap_or(0.0) >= 10.0 * 43_560.0
                    || low_desc.is_empty()
                    || force_cre_mode
            } else {
                // Strictly commercial/industrial zones (C-1, C-2, CG, CN, M-1, M-2, I-L, etc.)
                !is_explicit_1_to_4_residential || low_desc.is_empty() || force_cre_mode
            };

            if allow_zone_trigger {
                let refined_cls = if z_cls != CreAssetClass::AgriculturalFarmLand
                    && sqft.unwrap_or(1.0) <= 0.0
                    && lot_sqft.unwrap_or(0.0) > 0.0
                {
                    CreAssetClass::CommercialLand
                } else {
                    z_cls
                };
                if detected_class.is_none() {
                    detected_class = Some(refined_cls);
                }
                triggers.push(format!(
                    "Commercial/CRE zoning district '{}' ({})",
                    z_tok,
                    refined_cls.label()
                ));
            }
        }
    }

    // 6. Unimproved Commercial Land check when sqft == 0 and commercial keywords present
    if detected_class.is_some()
        && detected_class != Some(CreAssetClass::AgriculturalFarmLand)
        && sqft.map(|s| s <= 0.0).unwrap_or(false)
        && lot_sqft.unwrap_or(0.0) > 500.0
        && (low_desc.contains("land") || low_desc.contains("vacant") || low_desc.contains("lot"))
    {
        detected_class = Some(CreAssetClass::CommercialLand);
    }

    if let Some(cls) = detected_class {
        let eff_units = units.unwrap_or(if cls == CreAssetClass::Multifamily5Plus {
            6
        } else {
            1
        });
        let confidence = if triggers.len() >= 2 {
            "high"
        } else {
            "medium"
        };
        return CreClassification {
            is_cre: true,
            asset_class: Some(cls),
            asset_class_label: Some(cls.label().to_string()),
            asset_class_code: Some(cls.code().to_string()),
            confidence: confidence.to_string(),
            detected_units: units,
            detected_zoning: zoning,
            detected_use_code: use_code,
            triggers,
            residential_reason: None,
            sba_owner_user_compatible: cls.is_sba_owner_user_compatible(),
            agency_multifamily_eligible: cls.is_agency_multifamily_eligible(eff_units),
        };
    }

    // 7. On-demand CRE mode (`mode=cre` or `POST /api/cre/underwrite`) on a residential parcel
    if force_cre_mode {
        let fallback_cls = if units.unwrap_or(0) >= 5 {
            CreAssetClass::Multifamily5Plus
        } else if sqft.unwrap_or(1.0) <= 0.0 && lot_sqft.unwrap_or(0.0) > 0.0 {
            CreAssetClass::CommercialLand
        } else {
            CreAssetClass::MixedUse
        };
        triggers.push(
            "Explicit CRE underwriting mode requested on parcel (mixed-use conversion / portfolio evaluation)"
                .to_string(),
        );
        let eff_units = units.unwrap_or(1);
        return CreClassification {
            is_cre: true,
            asset_class: Some(fallback_cls),
            asset_class_label: Some(fallback_cls.label().to_string()),
            asset_class_code: Some(fallback_cls.code().to_string()),
            confidence: "explicit_override".to_string(),
            detected_units: units,
            detected_zoning: zoning,
            detected_use_code: use_code,
            triggers,
            residential_reason: None,
            sba_owner_user_compatible: fallback_cls.is_sba_owner_user_compatible(),
            agency_multifamily_eligible: fallback_cls.is_agency_multifamily_eligible(eff_units),
        };
    }

    // 8. Standard 1–4 Unit Residential property
    let res_reason = match units {
        Some(u) if (2..=4).contains(&u) => {
            format!("{u}-unit residential property (under 5-unit commercial multifamily threshold)")
        }
        _ => "1–4 unit residential parcel (no commercial use code, CRE zoning, or 5+ unit count)"
            .to_string(),
    };
    CreClassification {
        is_cre: false,
        asset_class: None,
        asset_class_label: None,
        asset_class_code: None,
        confidence: "residential".to_string(),
        detected_units: units,
        detected_zoning: zoning,
        detected_use_code: use_code,
        triggers,
        residential_reason: Some(res_reason),
        sba_owner_user_compatible: false,
        agency_multifamily_eligible: false,
    }
}

fn land_farm_opts_from_overrides(
    overrides: Option<&CreUnderwritingOverrides>,
) -> Option<LandFarmEvalOptions> {
    let ov = overrides?;
    if ov.acreage.is_none()
        && ov.lot_sqft.is_none()
        && ov.allowable_far.is_none()
        && ov.farming_mode.is_none()
        && ov.target_use.is_none()
        && ov.purchase_price.is_none()
    {
        return None;
    }
    Some(LandFarmEvalOptions {
        acreage: ov
            .acreage
            .or_else(|| ov.lot_sqft.filter(|&s| s > 0.0).map(|s| s / 43_560.0)),
        allowable_far: ov.allowable_far,
        farming_mode: ov.farming_mode.clone(),
        target_use: ov.target_use.clone(),
        asking_price: ov.purchase_price,
    })
}

fn store_req_from_overrides(
    overrides: Option<&CreUnderwritingOverrides>,
) -> Option<StoreEvalRequest> {
    let ov = overrides?;
    if ov.concept.is_none()
        && ov.store_sqft.is_none()
        && ov.annual_base_rent_psf.is_none()
        && ov.rent_per_sqft_yr.is_none()
        && ov.nnn_cam_psf.is_none()
        && ov.target_price_point.is_none()
    {
        return None;
    }
    Some(StoreEvalRequest {
        selected_concept: ov.concept.clone(),
        store_sqft: ov.store_sqft.or(ov.gross_sqft),
        annual_base_rent_psf: ov.annual_base_rent_psf.or(ov.rent_per_sqft_yr),
        nnn_cam_psf: ov.nnn_cam_psf,
        target_price_point: ov.target_price_point.clone(),
        plaza_name: None,
    })
}

fn has_land_farm_override_fields(overrides: Option<&CreUnderwritingOverrides>) -> bool {
    overrides
        .map(|ov| {
            ov.acreage.is_some()
                || ov.farming_mode.is_some()
                || ov.allowable_far.is_some()
                || ov.target_use.is_some()
        })
        .unwrap_or(false)
}

fn has_store_override_fields(overrides: Option<&CreUnderwritingOverrides>) -> bool {
    overrides
        .map(|ov| {
            ov.concept.is_some()
                || ov.store_sqft.is_some()
                || ov.annual_base_rent_psf.is_some()
                || ov.nnn_cam_psf.is_some()
                || ov.target_price_point.is_some()
        })
        .unwrap_or(false)
}

fn finish_cre_section_output(
    classification: CreClassification,
    asset_class: CreAssetClass,
    trade_out: ModuleOutput,
    mut uw_out: ModuleOutput,
    land_farm_out: Option<ModuleOutput>,
    business_profit_out: Option<ModuleOutput>,
    existing_cre: Option<&Value>,
    brand_name: &str,
) -> ModuleOutput {
    let eff_brand = crate::shared::effective_brand_name(brand_name);
    let mut sources = vec![
        SourceItem::new(
            format!(
                "{eff_brand} CRE Institutional Underwriting & Debt Sizing Engine (Conventional/CMBS, SBA 504, SBA 7(a), Agency DUS/SBL)"
            ),
            "https://www.sba.gov/funding-programs/loans/504-loans",
        ),
    ];
    sources.extend(trade_out.sources);
    sources.extend(uw_out.sources);

    let mut warnings = Vec::new();
    warnings.extend(trade_out.warnings);
    warnings.extend(uw_out.warnings);

    let land_and_farm_val = match land_farm_out {
        Some(lf) => {
            sources.extend(lf.sources);
            warnings.extend(lf.warnings);
            lf.data
        }
        None => existing_cre
            .and_then(|c| c.get("landAndFarm"))
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(Value::Null),
    };

    let business_profit_val = match business_profit_out {
        Some(bp) => {
            sources.extend(bp.sources);
            warnings.extend(bp.warnings);
            bp.data
        }
        None => existing_cre
            .and_then(|c| c.get("businessProfit"))
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(Value::Null),
    };

    let blended_land_est = land_and_farm_val
        .get("blendedEstimate")
        .or_else(|| land_and_farm_val.pointer("/valuation/blendedEstimate"))
        .filter(|v| v.is_object())
        .cloned();

    if matches!(
        asset_class,
        CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand
    ) {
        if let (Some(blended), Some(uw_obj)) = (&blended_land_est, uw_out.data.as_object_mut()) {
            uw_obj.insert("landValuationEstimate".into(), blended.clone());
        }
    }

    let mut data = json!({
        "enabled": classification.is_cre,
        "classification": classification,
        "underwriting": uw_out.data,
        "tradeArea": trade_out.data,
        "landAndFarm": land_and_farm_val,
        "businessProfit": business_profit_val,
        "disclaimer": underwriting::CRE_UNDERWRITING_DISCLAIMER,
    });

    if let (Some(blended), Some(dobj)) = (blended_land_est, data.as_object_mut()) {
        if matches!(
            asset_class,
            CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand
        ) {
            dobj.insert("blendedEstimate".into(), blended);
        }
    }

    ModuleOutput {
        data,
        sources,
        warnings,
    }
}

/// Builds the complete deterministic `report["cre"]` payload (synchronous / offline-safe) combining:
/// 1. CRE Asset Classification (`classification`)
/// 2. Institutional Financial Underwriting & 4-Way Debt Stack (`underwriting`)
/// 3. CRE Trade-Area, Frontage Traffic (AADT) & Site Envelope (`tradeArea`)
/// 4. Commercial Land & Agricultural/Farmland Valuation (`landAndFarm`)
/// 5. Store Profit & Plaza Co-Tenancy Simulator (`businessProfit`)
pub fn build_cre_report_section(
    report: &Value,
    overrides: Option<&CreUnderwritingOverrides>,
    force_cre_mode: bool,
) -> ModuleOutput {
    let facts = report.get("facts").unwrap_or(&Value::Null);
    let geo = report.get("geo");
    let explicit_cls = overrides.and_then(|o| o.asset_class.as_deref());

    let classification = classify_cre_asset(
        facts,
        geo,
        explicit_cls,
        force_cre_mode || overrides.is_some(),
    );

    let asset_class = classification
        .asset_class
        .unwrap_or(CreAssetClass::MixedUse);

    let trade_out = analyze_cre_trade_area(report, asset_class);

    let mut ctx_report = report.clone();
    if !ctx_report
        .get("cre")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        ctx_report["cre"] = json!({});
    }
    ctx_report["cre"]["tradeArea"] = trade_out.data.clone();

    let should_build_lf = matches!(
        asset_class,
        CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand
    ) || has_land_farm_override_fields(overrides);

    let lf_out = if should_build_lf {
        let is_farmland = asset_class == CreAssetClass::AgriculturalFarmLand
            || overrides.and_then(|o| o.farming_mode.as_ref()).is_some();
        let lf_opts = land_farm_opts_from_overrides(overrides);
        let out = evaluate_land_and_farm_offline(&ctx_report, is_farmland, None, lf_opts.as_ref());
        ctx_report["cre"]["landAndFarm"] = out.data.clone();
        Some(out)
    } else {
        None
    };

    let uw_input = CreUnderwritingInput::from_report_and_overrides(
        &ctx_report,
        asset_class,
        &classification,
        overrides,
    );
    let uw_out = run_cre_underwriting(&uw_input);
    ctx_report["cre"]["underwriting"] = uw_out.data.clone();

    let should_build_bp = matches!(
        asset_class,
        CreAssetClass::Retail
            | CreAssetClass::MixedUse
            | CreAssetClass::CommercialLand
            | CreAssetClass::Hospitality
    ) || has_store_override_fields(overrides);

    let bp_out = if should_build_bp {
        let sreq = store_req_from_overrides(overrides);
        Some(analyze_location_business_profit(
            &ctx_report,
            None,
            sreq.as_ref(),
        ))
    } else {
        None
    };

    finish_cre_section_output(
        classification,
        asset_class,
        trade_out,
        uw_out,
        lf_out,
        bp_out,
        report.get("cre"),
        crate::shared::report_brand(report).0,
    )
}

/// Async version of `build_cre_report_section` that hydrates live USDA NRCS SSURGO soil data
/// (for farmland) and live OpenStreetMap Overpass plaza co-tenants/competitors (for retail/store evaluation).
pub async fn build_cre_report_section_live(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    overrides: Option<&CreUnderwritingOverrides>,
    force_cre_mode: bool,
) -> ModuleOutput {
    let facts = report.get("facts").unwrap_or(&Value::Null);
    let geo = report.get("geo");
    let explicit_cls = overrides.and_then(|o| o.asset_class.as_deref());

    let classification = classify_cre_asset(
        facts,
        geo,
        explicit_cls,
        force_cre_mode || overrides.is_some(),
    );

    let asset_class = classification
        .asset_class
        .unwrap_or(CreAssetClass::MixedUse);

    let trade_out = analyze_cre_trade_area(report, asset_class);

    let mut ctx_report = report.clone();
    if !ctx_report
        .get("cre")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        ctx_report["cre"] = json!({});
    }
    ctx_report["cre"]["tradeArea"] = trade_out.data.clone();

    let should_build_lf = matches!(
        asset_class,
        CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand
    ) || has_land_farm_override_fields(overrides);

    let lf_out = if should_build_lf {
        let is_farmland = asset_class == CreAssetClass::AgriculturalFarmLand
            || overrides.and_then(|o| o.farming_mode.as_ref()).is_some();
        let lf_opts = land_farm_opts_from_overrides(overrides);
        let out =
            evaluate_land_and_farm(Some(http), &ctx_report, is_farmland, lf_opts.as_ref()).await;
        ctx_report["cre"]["landAndFarm"] = out.data.clone();
        Some(out)
    } else {
        None
    };

    let uw_input = CreUnderwritingInput::from_report_and_overrides(
        &ctx_report,
        asset_class,
        &classification,
        overrides,
    );
    let uw_out = run_cre_underwriting(&uw_input);
    ctx_report["cre"]["underwriting"] = uw_out.data.clone();

    let should_build_bp = matches!(
        asset_class,
        CreAssetClass::Retail
            | CreAssetClass::MixedUse
            | CreAssetClass::CommercialLand
            | CreAssetClass::Hospitality
    ) || has_store_override_fields(overrides);

    let bp_out = if should_build_bp {
        let sreq = store_req_from_overrides(overrides);
        Some(analyze_location_business_profit_live(http, env, &ctx_report, sreq.as_ref()).await)
    } else {
        None
    };

    finish_cre_section_output(
        classification,
        asset_class,
        trade_out,
        uw_out,
        lf_out,
        bp_out,
        report.get("cre"),
        crate::shared::report_brand(report).0,
    )
}

fn backfill_unimproved_land_valuation_if_needed(report: &mut Value, cre_data: &Value) {
    let ac_code = cre_data
        .pointer("/classification/assetClassCode")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !matches!(ac_code, "commercial_land" | "agricultural_farm") {
        return;
    }
    let sqft = read_f64_field(
        report.get("facts").unwrap_or(&Value::Null),
        &["sqft", "grossSqft", "rentableSqft", "buildingSqft"],
    )
    .unwrap_or(0.0);
    if sqft > 0.0 {
        return;
    }
    let Some(blended) = cre_data
        .pointer("/landAndFarm/blendedEstimate")
        .or_else(|| cre_data.pointer("/landAndFarm/valuation/blendedEstimate"))
        .filter(|v| v.is_object())
        .cloned()
    else {
        return;
    };

    let existing_val = report
        .pointer("/modules/valuation/data/estimate/value")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let is_low_conf = report
        .pointer("/modules/valuation/data/estimate/confidence")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("low"))
        .unwrap_or(false);

    if !report
        .get("modules")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        report["modules"] = json!({});
    }
    if !report
        .pointer("/modules/valuation")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        report["modules"]["valuation"] = json!({ "data": {}, "warnings": [] });
    }
    if !report
        .pointer("/modules/valuation/data")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        report["modules"]["valuation"]["data"] = json!({});
    }
    report["modules"]["valuation"]["data"]["creLandEstimate"] = blended.clone();
    if existing_val <= 0.0 || is_low_conf {
        report["modules"]["valuation"]["data"]["estimate"] = json!({
            "low": blended.get("low").cloned().unwrap_or(Value::Null),
            "value": blended.get("value").cloned().unwrap_or(Value::Null),
            "high": blended.get("high").cloned().unwrap_or(Value::Null),
            "perAcre": blended.get("perAcre").cloned().unwrap_or(Value::Null),
            "perLotSqft": blended.get("perLotSqft").cloned().unwrap_or(Value::Null),
            "confidence": blended.get("confidence").cloned().unwrap_or(json!("medium")),
            "source": "cre_land_farm_valuation",
        });
    }
}

fn write_cre_output_into_report(report: &mut Value, cre_out: ModuleOutput, cre_ms: u64) {
    if !report
        .get("modules")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        report["modules"] = json!({});
    }
    backfill_unimproved_land_valuation_if_needed(report, &cre_out.data);
    report["cre"] = cre_out.data.clone();
    report["modules"]["cre"] = json!({
        "data": cre_out.data,
        "warnings": cre_out.warnings.clone(),
    });
    if let Some(tmap) = report
        .pointer_mut("/meta/timingsMs")
        .and_then(|v| v.as_object_mut())
    {
        tmap.insert("cre".into(), json!(cre_ms));
    }
    if let Some(warr) = report.get_mut("warnings").and_then(|v| v.as_array_mut()) {
        for w in cre_out.warnings {
            warr.push(json!(format!("cre: {w}")));
        }
    }
    if let Some(sarr) = report.get_mut("sources").and_then(|v| v.as_array_mut()) {
        for s in cre_out.sources {
            if let Ok(sv) = serde_json::to_value(s) {
                sarr.push(sv);
            }
        }
    }
}

fn resolve_cre_attachment_intent(
    report: &Value,
    explicit_asset_class: Option<&str>,
    force_cre_flag: bool,
) -> (Option<String>, bool) {
    let eff_asset_class = explicit_asset_class
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            report
                .pointer("/facts/assetClass")
                .or_else(|| report.pointer("/facts/asset_class"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        });

    let cre_fact_flag = report
        .pointer("/facts/mode")
        .and_then(|v| v.as_str())
        .map(|m| m.eq_ignore_ascii_case("cre"))
        .unwrap_or(false)
        || report
            .pointer("/facts/cre")
            .map(|v| {
                v.as_bool() == Some(true)
                    || v.as_i64() == Some(1)
                    || matches!(v.as_str().map(str::trim), Some("1") | Some("true"))
            })
            .unwrap_or(false);

    let force_cre = force_cre_flag || cre_fact_flag || eff_asset_class.is_some();
    (eff_asset_class, force_cre)
}

/// Attaches `report["cre"]` and `report["modules"]["cre"]` whenever the parcel classifies as CRE
/// (multifamily 5+, retail, office, industrial, mixed-use, hospitality, commercial land, agricultural/farmland)
/// OR when explicit CRE mode (`cre=1` / `mode=cre` / `asset_class`) is requested.
pub fn attach_cre_section_if_applicable(
    report: &mut Value,
    explicit_asset_class: Option<&str>,
    force_cre_flag: bool,
) -> bool {
    let (eff_asset_class, force_cre) =
        resolve_cre_attachment_intent(report, explicit_asset_class, force_cre_flag);
    if !force_cre && report.get("cre").filter(|v| !v.is_null()).is_some() {
        return false;
    }

    let cre_cls = classify_cre_asset(
        report.get("facts").unwrap_or(&Value::Null),
        report.get("geo"),
        eff_asset_class.as_deref(),
        force_cre,
    );
    if !cre_cls.is_cre {
        return false;
    }

    let t_cre = std::time::Instant::now();
    let ov = eff_asset_class.map(|ac| CreUnderwritingOverrides {
        asset_class: Some(ac),
        ..Default::default()
    });
    let cre_out = build_cre_report_section(report, ov.as_ref(), force_cre);
    let cre_ms = t_cre.elapsed().as_millis() as u64;
    write_cre_output_into_report(report, cre_out, cre_ms);
    true
}

/// Async orchestrator helper that attaches `report["cre"]` with live USDA NRCS soil and
/// OpenStreetMap Overpass plaza co-tenancy hydration when applicable.
pub async fn attach_cre_section_with_live_hydration(
    http: &HttpClient,
    env: &EnvConfig,
    report: &mut Value,
    explicit_asset_class: Option<&str>,
    force_cre_flag: bool,
) -> bool {
    let (eff_asset_class, force_cre) =
        resolve_cre_attachment_intent(report, explicit_asset_class, force_cre_flag);
    if !force_cre && report.get("cre").filter(|v| !v.is_null()).is_some() {
        return false;
    }

    let cre_cls = classify_cre_asset(
        report.get("facts").unwrap_or(&Value::Null),
        report.get("geo"),
        eff_asset_class.as_deref(),
        force_cre,
    );
    if !cre_cls.is_cre {
        return false;
    }

    let t_cre = std::time::Instant::now();
    let ov = eff_asset_class.map(|ac| CreUnderwritingOverrides {
        asset_class: Some(ac),
        ..Default::default()
    });
    let cre_out = build_cre_report_section_live(http, env, report, ov.as_ref(), force_cre).await;
    let cre_ms = t_cre.elapsed().as_millis() as u64;
    write_cre_output_into_report(report, cre_out, cre_ms);
    true
}
