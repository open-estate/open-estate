// Complete Buyer Due-Diligence & Always-On HOA Profile module (`buyer_diligence`).
// Covers:
// 1. Always-on HOA Profile (`hoaProfile`): fee & regional benchmark, itemized inclusions/exclusions,
//    requirements/rules, super-priority lien state notice, NODA/assessment liens, reserves/litigation,
//    and Seller-Paid HOA Resale Certificate / Demand Statement contingency item (works pre-PDF and
//    consumes `report["hoaReview"]` when attached).
// 2. Remaining 7 buyer decision modules (`buyerDiligence`):
//    (a) Cash-to-Close, State/County/City Transfer Taxes & 2-1 Temporary Buydown vs. Price Cut
//    (b) Hidden Ongoing Costs: Mello-Roos CFD / MUD / CDD Special Districts & Modeled Monthly Utilities -> True All-In Burn
//    (c) Address-Level Broadband / Fiber & Remote-Work Connectivity (ACS B28002 + optional FCC BDC API)
//    (d) Water Source, Septic vs. Municipal Sewer & Point-of-Sale Municipal Mandates
//    (e) Adjacent Construction Pipeline, Upzoning & Historic Restrictions
//    (f) Exact Rent-vs-Buy Break-Even Horizon (6-8% round-trip friction) & Exit Liquidity Score (0-100)
//    (g) Day 1-30 Post-Closing Homestead Exemption & Seasonal Climate Maintenance Calendar

use crate::shared::{round_n, EnvConfig, HttpClient, SourceItem};
use futures::future::FutureExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::time::Duration;
use url::Url;

pub const BUYER_DILIGENCE_DISCLAIMER: &str = "Estimates for planning only; not an appraisal, legal advice, or tax advice. Confirm transfer taxes, HOA demands, Mello-Roos/MUD/CDD levies, and homestead rules with escrow, county tax collector, and HOA management.";

pub const HOA_PROFILE_DISCLAIMER: &str = "HOA intelligence compiled from public/listing records and uploaded HOA disclosures (when provided). Not legal, accounting, or underwriting advice. Confirm exact dues, transfer fees, reserve balances, litigation, and $0.00 balance via an official Seller-Paid HOA Resale Certificate / Demand Statement prior to contingency removal.";

pub const FCC_BDC_DEFAULT_API_URL: &str =
    "https://broadbandmap.fcc.gov/api/public/map/listAvailableFixedProvidersByLocation";

pub const PROPUBLICA_NONPROFIT_DEFAULT_API_URL: &str =
    "https://projects.propublica.org/nonprofits/api/v2/search.json";

#[derive(Debug, Clone)]
pub struct BuyerDiligenceBundle {
    pub hoa_profile: Value,
    pub buyer_diligence: Value,
    pub sources: Vec<SourceItem>,
}

// ------------------------- Static Regexes -------------------------

static RE_HOA_LIEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\b(?:hoa|homeowner(?:s)?(?:'s)?|condo(?:minium)?|property\s+owner|association|common\s+charge|assessment)\s+(?:assessment\s+)?lien\b|\blien\b.{0,40}\b(?:hoa|homeowner|condo|association|assessment)\b|\bsuper[- ]?priority\s+(?:hoa|association)?\s*lien\b|\bsuper[- ]?lien\b)")
        .expect("hoa lien regex")
});
static RE_NODA: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bnoda\b|notice\s+of\s+delinquent\s+assessment|delinquent\s+(?:hoa\s+)?assessment|hoa\s+foreclosure|association\s+foreclosure|hoa\s+arrears|unpaid\s+hoa\s+dues|civ\.?\s*code\s*§?\s*5675)")
        .expect("noda regex")
});
static RE_SPECIAL_ASSESSMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:special\s+assessment|emergency\s+assessment|capital\s+assessment|roof\s+assessment|sb\s*326\s+assessment|milestone\s+assessment)")
        .expect("special assessment regex")
});
static RE_NO_SPECIAL_ASSESSMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:no\s+(?:active|pending|known|current|levied)?\s*special\s+assessments?|none\s+pending|special\s+assessments?\s*:\s*(?:none|0|\$0|n/a))")
        .expect("no special assessment regex")
});
static RE_RENTAL_CAP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:rental\s+cap|leasing\s+(?:cap|restriction|limit)|owner[- ]?occupied\s+only|no\s+rentals|rental\s+waitlist|waitlist\s+for\s+leasing|(\d{1,3})\s*%\s*(?:rental|leasing)\s*(?:cap|limit))")
        .expect("rental cap regex")
});
static RE_STR_RULE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:no\s+short[- ]term\s+rentals?|short[- ]term\s+rentals?\s+(?:prohibited|banned|restricted)|no\s+(?:airbnb|vrbo)|(\d{1,3})[- ]?(?:day|month|year)\s+minimum\s+lease|minimum\s+lease\s+(?:term\s+of\s+)?(\d{1,3})\s+(days?|months?|years?))")
        .expect("str rule regex")
});
static RE_PET_RULE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:no\s+pets|pets?\s+(?:not\s+allowed|prohibited|restricted)|max(?:imum)?\s+(\d+)\s+pets?|(\d{1,3})\s*(?:lb|lbs|pound)s?\s*(?:pet|weight)?\s*limit|breed\s+restriction)")
        .expect("pet rule regex")
});
static RE_AGE_55: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\b(?:55\+|55\s+and\s+older|62\+|62\s+and\s+older|senior\s+(?:community|housing|living)|active\s+adult|hopa|age[- ]restricted)\b)")
        .expect("age 55 regex")
});
static RE_BOARD_MOVEIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:board\s+approval|board\s+interview|right\s+of\s+first\s+refusal|\brofr\b|move[- ]?in\s+fee|capital\s+contribution|working\s+capital\s+fee|flip\s+tax|co-?op\s+board)")
        .expect("board movein regex")
});
static RE_MELLO_ROOS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:mello[- ]?roos|\bcfd\b|community\s+facilities\s+district|special\s+tax\s+district|1915\s+act\s+bond)")
        .expect("mello roos regex")
});
static RE_MUD_TX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bmud\b|municipal\s+utility\s+district|\bpid\b|public\s+improvement\s+district|\bwcid\b|water\s+control\s+and\s+improvement)")
        .expect("mud tx regex")
});
static RE_CDD_FL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bcdd\b|community\s+development\s+district|stewardship\s+district)")
        .expect("cdd fl regex")
});
static RE_SEPTIC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bseptic\b|leach\s+field|cesspool|mound\s+system|aerobic\s+treatment)")
        .expect("septic regex")
});
static RE_WELL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bprivate\s+well\b|\bwell\s+water\b|\bdrilled\s+well\b|\bshared\s+well\b)")
        .expect("well regex")
});
static RE_POOL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bswimming\s+pool\b|\bin[- ]ground\s+pool\b|\bheated\s+pool\b|\bpool\s*/\s*spa\b|\bprivate\s+pool\b)")
        .expect("pool regex")
});
static RE_HISTORIC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bhistoric\s+(?:district|overlay|landmark|preservation|home|register)\b|\bmills\s+act\b|\bhpoz\b|landmark\s+designation|conservation\s+district)")
        .expect("historic regex")
});
static RE_HOA_MENTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\bhoa\b|homeowner(?:s)?\s+association|condo(?:minium)?\s+(?:fee|dues|association)|cc&r)")
        .expect("hoa mention regex")
});
static RE_NO_HOA: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:\bno\s+hoa\b|hoa\s*:\s*none|0\s+hoa)").expect("no hoa regex"));
static RE_MONEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$\s*([0-9]{1,3}(?:,[0-9]{3})*(?:\.[0-9]{1,2})?|[0-9]+(?:\.[0-9]{1,2})?)")
        .expect("money regex")
});

// ------------------------- Helpers & Context -------------------------

fn unwrap_data(v: &Value) -> &Value {
    v.get("data").filter(|d| d.is_object()).unwrap_or(v)
}

fn module_data<'a>(report: &'a Value, key: &str) -> &'a Value {
    report
        .pointer(&format!("/modules/{key}"))
        .map(unwrap_data)
        .unwrap_or(&Value::Null)
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    if let Some(n) = val.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(s) = val.as_str() {
        let clean: String = s
            .trim()
            .replace([',', '$', '%'], "")
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        return clean.parse::<f64>().ok().filter(|n| n.is_finite());
    }
    None
}

fn val_i64(v: Option<&Value>) -> Option<i64> {
    val_f64(v).map(|n| n.round() as i64)
}

fn val_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn clean_census_city_label(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    let lc = t.to_ascii_lowercase();
    for suffix in [
        " city",
        " town",
        " village",
        " borough",
        " cdp",
        " municipality",
    ] {
        if lc.ends_with(suffix) && t.len() > suffix.len() {
            return t[..t.len() - suffix.len()].trim().to_string();
        }
    }
    t.to_string()
}

fn infer_state_from_fips(fips: &str) -> Option<&'static str> {
    crate::registry::us_geo::state_abbr_for_fips(fips)
}

use crate::modules::broadband_live::{
    check_fcc_fixed_broadband, FccFixedBroadbandResult, FccLookupStatus,
};

#[derive(Debug, Clone)]
struct LiveBroadbandCheck {
    fcc_fixed: FccFixedBroadbandResult,
    confirmed_options: Vec<crate::extensions::proto::BroadbandOptionProto>,
    confirmed_sources: Vec<SourceItem>,
}
use crate::modules::live_tariffs::{
    compute_water_bill_usd, normalize_city_slug, resolve_live_utility_and_climate_bundle_with_db,
    LiveUtilityBundle, LiveUtilityQuery,
};
use crate::registry::rates_and_costs::{
    estimate_transfer_tax_schedule, persist_rates_and_tax_datasets,
    regional_electricity_kwh_rate as regional_kwh_rate,
    state_effective_insurance_pct_of_price as state_insurance_rate,
    state_effective_property_tax_rate as baseline_state_tax_rate, RatesAndCostsPersistInput,
    TransferTaxEstimate as TransferTaxResult,
};
use crate::registry::utilities::{
    resolve_broadband_catalog, resolve_electric_utility, resolve_gas_utility,
    resolve_water_sewer_trash, UtilityLookupInput,
};
use crate::shared::{monthly_pi, remaining_balance_after_months};

struct ReportContext<'a> {
    facts: &'a Value,
    geo: &'a Value,
    valuation: &'a Value,
    market: &'a Value,
    amenities: &'a Value,
    environment: &'a Value,
    hazards: &'a Value,
    civic: &'a Value,
    condition: &'a Value,
    hoa_review: Option<&'a Value>,
    title_review: Option<&'a Value>,
    state: String,
    county: String,
    county_fips: String,
    city: String,
    street: String,
    zip: String,
    lat: Option<f64>,
    lon: Option<f64>,
    property_type: String,
    sqft: Option<f64>,
    lot_sqft: Option<f64>,
    lot_acres: Option<f64>,
    beds: Option<i64>,
    baths: Option<f64>,
    year_built: Option<i64>,
    price: f64,
    price_reported: bool,
    rate30: f64,
    rate30_reported: bool,
    tax_rate: f64,
    tax_rate_reported: bool,
    annual_tax: f64,
    annual_ins: f64,
    combined_text: String,
}

fn collect_listing_text(facts: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in [
        "features_text",
        "featuresText",
        "remarks",
        "publicRemarks",
        "description",
        "zoningNote",
        "permitNotes",
        "taxNote",
        "taxHistoryNote",
        "legal",
        "legalDescription",
        "subdivision",
    ] {
        if let Some(s) = val_str(facts.get(key)) {
            parts.push(s.to_string());
        }
    }
    if let Some(arr) = facts.get("features").and_then(|v| v.as_array()) {
        for item in arr.iter().take(40) {
            if let Some(s) = val_str(Some(item)) {
                parts.push(s.to_string());
            }
        }
    } else if let Some(s) = val_str(facts.get("features")) {
        parts.push(s.to_string());
    }
    for sub in ["listing", "mlsListing", "hoa", "buildingRegister"] {
        if let Some(obj) = facts.get(sub).filter(|v| v.is_object()) {
            if let Ok(s) = serde_json::to_string(obj) {
                parts.push(s.chars().take(4000).collect());
            }
        }
    }
    if let Some(raw) = facts.get("raw_json").or_else(|| facts.get("rawJson")) {
        if let Some(s) = raw.as_str() {
            parts.push(s.chars().take(4000).collect());
        } else if raw.is_object() {
            if let Ok(s) = serde_json::to_string(raw) {
                parts.push(s.chars().take(4000).collect());
            }
        }
    }
    parts.join(" | ")
}

fn extract_context<'a>(report: &'a Value) -> ReportContext<'a> {
    let facts = report.get("facts").map(unwrap_data).unwrap_or(&Value::Null);
    let geo = report.get("geo").map(unwrap_data).unwrap_or(&Value::Null);
    let valuation = module_data(report, "valuation");
    let market = module_data(report, "market");
    let amenities = module_data(report, "amenities");
    let environment = module_data(report, "environment");
    let hazards = module_data(report, "hazards");
    let civic = module_data(report, "civic");
    let condition = module_data(report, "condition");
    let hoa_review = report
        .get("hoaReview")
        .or_else(|| report.pointer("/modules/hoaReview"))
        .map(unwrap_data)
        .filter(|v| v.is_object());
    let title_review = report
        .get("titleReview")
        .or_else(|| report.pointer("/modules/titleReview"))
        .map(unwrap_data)
        .filter(|v| v.is_object());

    let county_fips = val_str(geo.get("countyFips")).unwrap_or("").to_string();
    let state = val_str(geo.get("state"))
        .map(|s| s.to_ascii_uppercase())
        .or_else(|| {
            val_str(geo.get("stateFips"))
                .and_then(infer_state_from_fips)
                .map(String::from)
        })
        .or_else(|| infer_state_from_fips(&county_fips).map(String::from))
        .unwrap_or_default();
    let county = val_str(geo.get("county")).unwrap_or("").to_string();
    let raw_city = val_str(geo.get("city"))
        .or_else(|| val_str(facts.get("city")))
        .or_else(|| val_str(geo.get("place")))
        .unwrap_or("");
    let city = clean_census_city_label(raw_city);
    let street = val_str(geo.get("street"))
        .or_else(|| {
            val_str(geo.get("matchedAddress"))
                .or_else(|| val_str(facts.get("address")))
                .and_then(|full| full.split(',').next().map(str::trim))
        })
        .unwrap_or("")
        .to_string();
    let zip = val_str(geo.get("zip"))
        .or_else(|| val_str(geo.get("zcta")))
        .or_else(|| val_str(geo.get("postalCode")))
        .or_else(|| val_str(facts.get("zip")))
        .or_else(|| val_str(facts.get("postalCode")))
        .unwrap_or("")
        .to_string();
    let lat = val_f64(geo.pointer("/refined/lat")).or_else(|| val_f64(geo.get("lat")));
    let lon = val_f64(geo.pointer("/refined/lon")).or_else(|| val_f64(geo.get("lon")));

    let property_type = val_str(facts.get("propertyType"))
        .or_else(|| val_str(facts.pointer("/buildingRegister/propertyType")))
        .or_else(|| val_str(facts.pointer("/listing/propertyType")))
        .unwrap_or("Single-family")
        .to_string();
    let sqft = val_f64(facts.get("sqft"))
        .or_else(|| val_f64(facts.pointer("/buildingRegister/livingSqft")))
        .filter(|&x| x > 0.0);
    let lot_sqft = val_f64(facts.get("lotSqft"))
        .or_else(|| val_f64(facts.pointer("/buildingRegister/lotSqft")))
        .filter(|&x| x > 0.0);
    let lot_acres = lot_sqft
        .map(|s| s / 43_560.0)
        .or_else(|| val_f64(facts.get("lotAcres")))
        .or_else(|| val_f64(facts.pointer("/buildingRegister/lotAcres")))
        .filter(|&x| x > 0.0);
    let beds = val_i64(facts.get("beds")).filter(|&b| b >= 0);
    let baths = val_f64(facts.get("baths")).filter(|&b| b > 0.0);
    let year_built = val_i64(facts.get("yearBuilt"))
        .or_else(|| val_i64(facts.pointer("/buildingRegister/yearBuilt")))
        .filter(|&y| y > 1700);

    let price_opt = val_f64(valuation.pointer("/estimate/value"))
        .or_else(|| val_f64(valuation.pointer("/ownership/price")))
        .or_else(|| val_f64(facts.get("listPrice")))
        .or_else(|| val_f64(facts.pointer("/mlsListing/listPrice")))
        .or_else(|| val_f64(facts.pointer("/listing/listPrice")))
        .or_else(|| val_f64(facts.pointer("/lastSale/price")))
        .or_else(|| val_f64(market.pointer("/zhvi/latest")))
        .or_else(|| val_f64(market.pointer("/tiers/mid")))
        .filter(|&p| p > 10_000.0);
    let price_reported = price_opt.is_some();
    let price = price_opt.unwrap_or(500_000.0);

    let rate30_opt = val_f64(valuation.pointer("/ownership/rate30"))
        .or_else(|| val_f64(market.pointer("/rates/mort30")))
        .filter(|&r| r > 0.5 && r < 20.0);
    let rate30_reported = rate30_opt.is_some();
    let rate30 = rate30_opt.unwrap_or(6.65);

    let tax_rate_opt = val_f64(valuation.pointer("/ownership/taxRatePct"))
        .map(|p| p / 100.0)
        .or_else(|| val_f64(facts.get("taxRate")))
        .or_else(|| val_f64(market.pointer("/govTaxBenchmark/effectiveTaxRate")))
        .filter(|&r| r > 0.0 && r < 0.12);
    let tax_rate_reported = tax_rate_opt.is_some();
    let tax_rate = tax_rate_opt.unwrap_or_else(|| baseline_state_tax_rate(&state));

    let annual_tax = val_f64(valuation.pointer("/ownership/monthly/propertyTax"))
        .map(|m| m * 12.0)
        .filter(|&t| t > 0.0)
        .unwrap_or_else(|| price * tax_rate);

    let annual_ins = val_f64(valuation.pointer("/ownership/monthly/insurance"))
        .map(|m| m * 12.0)
        .or_else(|| val_f64(facts.get("insuranceAnnual")))
        .filter(|&i| i > 0.0)
        .unwrap_or_else(|| (price * state_insurance_rate(&state)).max(1200.0));

    let combined_text = collect_listing_text(facts);

    ReportContext {
        facts,
        geo,
        valuation,
        market,
        amenities,
        environment,
        hazards,
        civic,
        condition,
        hoa_review,
        title_review,
        state,
        county,
        county_fips,
        city,
        street,
        zip,
        lat,
        lon,
        property_type,
        sqft,
        lot_sqft,
        lot_acres,
        beds,
        baths,
        year_built,
        price,
        price_reported,
        rate30,
        rate30_reported,
        tax_rate,
        tax_rate_reported,
        annual_tax,
        annual_ins,
        combined_text,
    }
}

// ------------------------- 1. Always-On HOA Profile -------------------------

struct SuperPrioritySpec {
    is_super_priority: bool,
    months_priority: Option<u32>,
    statute: &'static str,
    note: &'static str,
}

fn hoa_super_priority_info(state: &str) -> SuperPrioritySpec {
    match state {
        "NV" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(9),
            statute: "NRS 116.3116",
            note: "Nevada is a strict HOA super-priority lien state (up to 9 months of unpaid assessments + statutory collection/attorney costs prime first-lien mortgages and can extinguish subordinate deeds of trust in HOA foreclosure).",
        },
        "CO" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "C.R.S. § 38-33.3-316",
            note: "Colorado grants HOAs a 6-month super-priority lien ahead of first mortgages for unpaid common expense assessments.",
        },
        "FL" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(12),
            statute: "Fla. Stat. § 718.116 / § 720.3085",
            note: "Florida grants HOA/condo super-priority (lesser of 12 months unpaid assessments or 1% of original mortgage debt for foreclosing first mortgagees), AND resale buyers inherit joint-and-several liability for all unpaid seller assessments if not cleared via estoppel at closing.",
        },
        "WA" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "RCW 64.34.364 / RCW 64.90.485",
            note: "Washington grants HOAs/condos a 6-month super-priority assessment lien ahead of first deeds of trust.",
        },
        "DC" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "D.C. Code § 42-1903.13",
            note: "District of Columbia grants condo associations a 6-month super-priority lien priming first mortgages.",
        },
        "MA" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "M.G.L. c. 183A, § 6",
            note: "Massachusetts grants condos a rolling 6-month super-priority lien plus reasonable attorney fees and enforcement costs ahead of first mortgages; a clean 6(d) certificate at closing is essential.",
        },
        "CT" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(9),
            statute: "Conn. Gen. Stat. § 47-258",
            note: "Connecticut grants common-interest associations a 9-month super-priority lien plus attorney fees priming first mortgages.",
        },
        "IL" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "765 ILCS 605/9(g)(4)",
            note: "Illinois grants condo associations a 6-month super-priority lien recovery following foreclosure and requires a paid-assessment letter under Section 22.1/9(g).",
        },
        "NJ" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "N.J.S.A. 46:8B-21",
            note: "New Jersey grants condo associations up to 6 months of customary aggregate assessments as a super-priority lien ahead of prior recorded mortgages.",
        },
        "HI" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "HRS § 514B-146",
            note: "Hawaii grants AOAO/condo associations a 6-month priority assessment recovery upon foreclosure/transfer.",
        },
        "PA" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "68 Pa.C.S. § 3315 / § 5315",
            note: "Pennsylvania grants condo and planned-community associations a 6-month super-priority lien priming first mortgages.",
        },
        "MN" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "Minn. Stat. § 515B.3-116",
            note: "Minnesota MCIOA grants associations a 6-month super-priority lien ahead of first mortgages.",
        },
        "RI" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "R.I. Gen. Laws § 34-36.1-3.16",
            note: "Rhode Island grants condo associations a 6-month super-priority lien plus attorney/foreclosure costs priming first mortgages.",
        },
        "MD" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(4),
            statute: "Md. Code, Real Prop. § 11-110 / § 11B-117",
            note: "Maryland grants up to 4 months (capped at $1,200) of unpaid HOA/condo assessments priority over first mortgages.",
        },
        "AK" | "AL" | "DE" | "MO" | "NH" | "OR" | "TN" | "VT" | "WV" => SuperPrioritySpec {
            is_super_priority: true,
            months_priority: Some(6),
            statute: "State UCIOA / Condominium Lien Statute",
            note: "This state has statutory limited HOA/condo priority-lien rules (typically 6 months of unpaid assessments or super-priority upon enforcement). Verify payoff in escrow.",
        },
        "CA" => SuperPrioritySpec {
            is_super_priority: false,
            months_priority: None,
            statute: "CA Civ. Code §§ 5660–5720 (Davis-Stirling Act)",
            note: "California is NOT a first-mortgage-priming super-priority lien state, but HOAs can record a Notice of Delinquent Assessment (NODA) and conduct non-judicial HOA foreclosure if arrears >= $1,800 or > 12 months delinquent.",
        },
        "TX" => SuperPrioritySpec {
            is_super_priority: false,
            months_priority: None,
            statute: "Tex. Prop. Code Ch. 82 / Ch. 209",
            note: "Texas HOA assessment liens generally subordinate to recorded purchase-money first mortgages, but carry expedited foreclosure remedies and resale certificate estoppel rules.",
        },
        _ => SuperPrioritySpec {
            is_super_priority: false,
            months_priority: None,
            statute: "State Common-Interest / Covenant Law",
            note: "Verify HOA lien priority and seller assessment payoff rules via an official HOA Resale Certificate / Demand Statement in escrow.",
        },
    }
}

fn extract_hoa_monthly(ctx: &ReportContext) -> Option<f64> {
    if let Some(dues) = ctx
        .hoa_review
        .and_then(|hr| val_f64(hr.pointer("/financialHealth/monthlyDuesUsd")))
        .filter(|&v| v >= 0.0)
    {
        return Some(dues);
    }
    if let Some(m) = val_f64(ctx.facts.get("hoaMonthly")).filter(|&v| v >= 0.0) {
        return Some(m);
    }
    if let Some(hoa_obj) = ctx.facts.get("hoa").filter(|v| v.is_object()) {
        if let Some(m) = val_f64(
            hoa_obj
                .get("monthlyFee")
                .or_else(|| hoa_obj.get("monthlyDues")),
        ) {
            return Some(m);
        }
        if let Some(fee) = val_f64(hoa_obj.get("fee").or_else(|| hoa_obj.get("dues"))) {
            let freq = val_str(hoa_obj.get("frequency"))
                .unwrap_or("monthly")
                .to_ascii_lowercase();
            let div = if freq.contains("annual") || freq.contains("year") {
                12.0
            } else if freq.contains("quarter") {
                3.0
            } else if freq.contains("semi") {
                6.0
            } else {
                1.0
            };
            return Some(round_n(fee / div, 2));
        }
    }
    for ptr in [
        "/listing/hoaMonthly",
        "/mlsListing/hoaMonthly",
        "/raw_json/AssociationFee",
        "/rawJson/AssociationFee",
    ] {
        if let Some(v) = val_f64(ctx.facts.pointer(ptr)).filter(|&x| x >= 0.0) {
            return Some(v);
        }
    }
    None
}

struct HoaBenchmarkResult {
    monthly: f64,
    label: String,
    source: &'static str,
    comp_count: usize,
    p25: Option<f64>,
    p75: Option<f64>,
}

fn regional_hoa_benchmark_fallback(state: &str, prop_type_lc: &str) -> (f64, &'static str) {
    let high_cost = matches!(
        state,
        "CA" | "NY" | "MA" | "WA" | "DC" | "HI" | "NJ" | "CT" | "CO" | "FL"
    );
    if prop_type_lc.contains("co-op") || prop_type_lc.contains("coop") {
        return (
            if high_cost { 980.0 } else { 580.0 },
            "Co-op Modeled Fallback",
        );
    }
    if prop_type_lc.contains("condo") || prop_type_lc.contains("high-rise") {
        let b = if matches!(state, "NY" | "HI" | "DC" | "CA") {
            585.0
        } else if high_cost {
            465.0
        } else {
            340.0
        };
        return (b, "Modeled Condo Benchmark Fallback");
    }
    if prop_type_lc.contains("town") || prop_type_lc.contains("pud") {
        return (
            if high_cost { 320.0 } else { 225.0 },
            "Modeled Townhome / PUD Fallback",
        );
    }
    (
        if high_cost { 125.0 } else { 85.0 },
        "Modeled HOA-Managed SFR Fallback",
    )
}

fn compute_hoa_benchmark(ctx: &ReportContext, prop_type_lc: &str) -> HoaBenchmarkResult {
    let mut all_dues: Vec<f64> = Vec::new();
    let mut matched_dues: Vec<f64> = Vec::new();
    let mut seen_addrs = std::collections::HashSet::new();
    let subj_is_attached = prop_type_lc.contains("condo")
        || prop_type_lc.contains("town")
        || prop_type_lc.contains("co-op")
        || prop_type_lc.contains("attached");

    for arr_key in ["comps", "nearbyListings"] {
        if let Some(arr) = ctx.facts.get(arr_key).and_then(|v| v.as_array()) {
            for item in arr {
                let Some(hoa) =
                    val_f64(item.get("hoaMonthly")).filter(|&h| (20.0..=5000.0).contains(&h))
                else {
                    continue;
                };
                let addr_key = val_str(item.get("address"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !addr_key.is_empty() && !seen_addrs.insert(addr_key) {
                    continue;
                }
                let pt = val_str(item.get("propertyType"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                all_dues.push(hoa);
                let comp_attached =
                    pt.contains("condo") || pt.contains("town") || pt.contains("co-op");
                if pt.is_empty() || comp_attached == subj_is_attached {
                    matched_dues.push(hoa);
                }
            }
        }
    }

    let mut sample = if matched_dues.len() >= 2 {
        matched_dues
    } else {
        all_dues
    };
    if sample.len() >= 2 {
        sample.sort_by(|a, b| a.total_cmp(b));
        let n = sample.len();
        let median = if n % 2 == 1 {
            sample[n / 2]
        } else {
            (sample[n / 2 - 1] + sample[n / 2]) / 2.0
        };
        let p25 = sample[n / 4];
        let p75 = sample[(3 * (n - 1)) / 4];
        return HoaBenchmarkResult {
            monthly: round_n(median, 2),
            label: format!(
                "Local MLS Comps Median ({} · n={n})",
                if ctx.city.is_empty() {
                    "Neighborhood"
                } else {
                    ctx.city.as_str()
                }
            ),
            source: "local_mls_comps",
            comp_count: n,
            p25: Some(round_n(p25, 0)),
            p75: Some(round_n(p75, 0)),
        };
    }

    let (fb_mo, fb_lbl) = regional_hoa_benchmark_fallback(&ctx.state, prop_type_lc);
    HoaBenchmarkResult {
        monthly: fb_mo,
        label: fb_lbl.to_string(),
        source: "modeled_regional_fallback",
        comp_count: 0,
        p25: None,
        p75: None,
    }
}

fn extract_hoa_text_blobs(ctx: &ReportContext) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = Vec::new();
    if let Some(arr) = ctx
        .facts
        .pointer("/titleAndLiens/involuntaryLiens")
        .and_then(|v| v.as_array())
    {
        for x in arr {
            if let Ok(s) = serde_json::to_string(x) {
                items.push(("facts.titleAndLiens.involuntaryLiens".into(), s));
            }
        }
    }
    if let Some(arr) = ctx
        .facts
        .pointer("/titleAndLiens/easementsAndRestrictions")
        .and_then(|v| v.as_array())
    {
        for x in arr {
            if let Ok(s) = serde_json::to_string(x) {
                items.push(("facts.titleAndLiens.easementsAndRestrictions".into(), s));
            }
        }
    }
    if let Some(fc) = ctx.facts.get("foreclosure").filter(|v| !v.is_null()) {
        if let Ok(s) = serde_json::to_string(fc) {
            items.push(("facts.foreclosure".into(), s));
        }
    }
    if let Some(hoa) = ctx.facts.get("hoa").filter(|v| !v.is_null()) {
        if let Ok(s) = serde_json::to_string(hoa) {
            items.push(("facts.hoa".into(), s));
        }
    }
    if let Some(br) = ctx.facts.get("buildingRegister").filter(|v| !v.is_null()) {
        if let Ok(s) = serde_json::to_string(br) {
            items.push(("facts.buildingRegister".into(), s));
        }
    }
    if !ctx.combined_text.is_empty() {
        items.push((
            "facts.listing_and_features".into(),
            ctx.combined_text.clone(),
        ));
    }
    if let Some(tr) = ctx.title_review {
        if let Ok(s) = serde_json::to_string(tr) {
            items.push(("titleReview".into(), s));
        }
    }
    items
}

fn build_hoa_profile(ctx: &ReportContext, nonprofit_filings: &[Value]) -> Value {
    let pt_lc = ctx.property_type.to_ascii_lowercase();
    let is_attached = pt_lc.contains("condo")
        || pt_lc.contains("town")
        || pt_lc.contains("co-op")
        || pt_lc.contains("coop")
        || pt_lc.contains("pud")
        || pt_lc.contains("strata");

    let explicit_hoa_mo = extract_hoa_monthly(ctx);
    let hoa_monthly = explicit_hoa_mo.unwrap_or(0.0);
    let hoa_annual = (hoa_monthly * 12.0).round() as i64;
    let hoa_per_sqft_month = ctx
        .sqft
        .filter(|&s| s > 0.0)
        .map(|s| round_n(hoa_monthly / s, 2));

    let mentions_hoa =
        RE_HOA_MENTION.is_match(&ctx.combined_text) && !RE_NO_HOA.is_match(&ctx.combined_text);
    let blobs = extract_hoa_text_blobs(ctx);

    // Inspect liens, NODA, special assessments & litigation early because an active HOA lien also means HOA exists!
    let mut hoa_liens: Vec<Value> = Vec::new();
    let mut noda_notices: Vec<Value> = Vec::new();
    let mut special_assessments: Vec<Value> = Vec::new();

    for (src, txt) in &blobs {
        if RE_HOA_LIEN.is_match(txt) {
            let amt = RE_MONEY
                .captures(txt)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().replace(',', "").parse::<f64>().ok())
                .map(|v| v.round() as i64);
            hoa_liens.push(json!({
                "source": src,
                "amountUsd": amt,
                "summary": txt.chars().take(240).collect::<String>(),
            }));
        }
        if RE_NODA.is_match(txt) {
            noda_notices.push(json!({
                "source": src,
                "summary": txt.chars().take(240).collect::<String>(),
            }));
        }
        if src != "facts.hoa"
            && RE_SPECIAL_ASSESSMENT.is_match(txt)
            && !RE_NO_SPECIAL_ASSESSMENT.is_match(txt)
        {
            let amt = RE_MONEY
                .captures(txt)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().replace(',', "").parse::<f64>().ok())
                .map(|v| v.round() as i64);
            special_assessments.push(json!({
                "source": src,
                "amountUsd": amt,
                "status": "Referenced in public/listing record — verify payoff",
                "summary": txt.chars().take(220).collect::<String>(),
            }));
        }
    }

    // Also inspect structured `facts.hoa` fields
    if let Some(hoa_obj) = ctx.facts.get("hoa").filter(|v| v.is_object()) {
        if hoa_obj.get("hasLien").and_then(|v| v.as_bool()) == Some(true)
            || val_f64(
                hoa_obj
                    .get("lienAmountUsd")
                    .or_else(|| hoa_obj.get("arrearsUsd")),
            )
            .map(|x| x > 0.0)
            .unwrap_or(false)
        {
            let amt = val_i64(
                hoa_obj
                    .get("lienAmountUsd")
                    .or_else(|| hoa_obj.get("arrearsUsd")),
            );
            hoa_liens.push(json!({
                "source": "facts.hoa",
                "amountUsd": amt,
                "summary": val_str(hoa_obj.get("lienNote")).unwrap_or("HOA lien / unpaid assessment balance reported in facts.hoa."),
            }));
        }
        if hoa_obj.get("noda").and_then(|v| v.as_bool()) == Some(true)
            || hoa_obj
                .get("noticeOfDelinquentAssessment")
                .and_then(|v| v.as_bool())
                == Some(true)
        {
            noda_notices.push(json!({
                "source": "facts.hoa",
                "summary": "Notice of Delinquent Assessment (NODA) flagged in facts.hoa.",
            }));
        }
        if let Some(sa_arr) = hoa_obj.get("specialAssessments").and_then(|v| v.as_array()) {
            for sa in sa_arr {
                special_assessments.push(sa.clone());
            }
        } else if let Some(sa_amt) = val_i64(hoa_obj.get("specialAssessmentUsd")).filter(|&a| a > 0)
        {
            special_assessments.push(json!({
                "source": "facts.hoa",
                "amountUsd": sa_amt,
                "status": val_str(hoa_obj.get("specialAssessmentStatus")).unwrap_or("Active / Reported"),
            }));
        }
    }

    // Consume `report["hoaReview"]` special assessments and lien/NODA findings if present
    if let Some(hr_sa) = ctx
        .hoa_review
        .and_then(|hr| hr.pointer("/financialHealth/specialAssessments"))
        .and_then(|v| v.as_array())
    {
        for sa in hr_sa {
            special_assessments.push(json!({
                "source": "hoaReview",
                "amountUsd": sa.get("amountUsd").cloned().unwrap_or(Value::Null),
                "perUnitAmountUsd": sa.get("perUnitAmountUsd").cloned().unwrap_or(Value::Null),
                "purpose": sa.get("purpose").cloned().unwrap_or(Value::Null),
                "status": sa.get("boardVoteStatus").cloned().unwrap_or_else(|| json!("Detected in HOA PDF")),
            }));
        }
    }
    if let Some(hr_findings) = ctx
        .hoa_review
        .and_then(|hr| hr.get("findings"))
        .and_then(|v| v.as_array())
    {
        for f in hr_findings {
            let txt = format!(
                "{} {}",
                val_str(f.get("title")).unwrap_or(""),
                val_str(f.get("detail")).unwrap_or("")
            );
            if RE_HOA_LIEN.is_match(&txt) {
                hoa_liens.push(json!({
                    "source": "hoaReview",
                    "amountUsd": Value::Null,
                    "summary": txt.chars().take(240).collect::<String>(),
                }));
            }
            if RE_NODA.is_match(&txt) {
                noda_notices.push(json!({
                    "source": "hoaReview",
                    "summary": txt.chars().take(240).collect::<String>(),
                }));
            }
        }
    }

    let hoa_obj_nonempty = ctx
        .facts
        .get("hoa")
        .map(|v| v.is_object() && !v.as_object().map(|m| m.is_empty()).unwrap_or(true))
        .unwrap_or(false);
    let has_hoa = hoa_monthly > 0.0
        || ctx.hoa_review.is_some()
        || hoa_obj_nonempty
        || mentions_hoa
        || !hoa_liens.is_empty()
        || !noda_notices.is_empty()
        || (is_attached && explicit_hoa_mo.is_none());

    let hoa_bench = compute_hoa_benchmark(ctx, &pt_lc);
    let bench_mo = hoa_bench.monthly;
    let bench_label = hoa_bench.label;
    let delta_mo: Option<i64> =
        (has_hoa && hoa_monthly > 0.0).then(|| (hoa_monthly - bench_mo).round() as i64);
    let ratio_bench = if bench_mo > 0.0 && hoa_monthly > 0.0 {
        Some(round_n(hoa_monthly / bench_mo, 2))
    } else {
        None
    };
    let fee_tier = if !has_hoa || hoa_monthly <= 0.0 {
        if explicit_hoa_mo == Some(0.0) && !is_attached {
            "No Mandatory HOA ($0/mo Reported)"
        } else if explicit_hoa_mo == Some(0.0) && is_attached {
            "Fee-Simple Attached / $0 Mandatory HOA Reported"
        } else if is_attached {
            "Unreported (Verify Condo/Townhome HOA Dues)"
        } else {
            "No Mandatory HOA Reported"
        }
    } else if hoa_monthly <= bench_mo * 0.75 {
        "Lean / Below Benchmark"
    } else if hoa_monthly <= bench_mo * 1.20 {
        if hoa_bench.source == "local_mls_comps" {
            "In-Line with Local MLS Comps"
        } else {
            "In-Line with Regional Benchmark"
        }
    } else if hoa_monthly <= bench_mo * 1.60 {
        if hoa_bench.source == "local_mls_comps" {
            "Elevated vs. Local MLS Comps"
        } else {
            "Elevated vs. Regional Benchmark"
        }
    } else {
        "High Monthly Carry (Material DTI Impact)"
    };

    // Coverage breakdown
    let master_ins_note = if let Some(mi) = ctx
        .hoa_review
        .and_then(|hr| hr.pointer("/governanceAndLifestyleRules/masterInsurance"))
        .filter(|v| v.is_object())
    {
        let cov = val_str(mi.get("coverageType")).unwrap_or("Verify CC&Rs");
        let ded_pct = val_f64(mi.get("deductiblePercent"));
        let shock = mi
            .get("deductibleShockWarning")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        format!(
            "Uploaded HOA review reports master coverage `{cov}`{}{}. Confirm HO-6 walls-in + loss assessment endorsement.",
            ded_pct.map(|p| format!(" with {p:.1}% deductible")).unwrap_or_default(),
            if shock { " (DEDUCTIBLE SHOCK WARNING — unit owners may be assessed master deductible share)" } else { "" }
        )
    } else if !has_hoa {
        "No HOA master policy applies; buyer must carry a full HO-3 dwelling replacement-cost policy covering roof, structure, and liability.".to_string()
    } else if pt_lc.contains("condo") || pt_lc.contains("co-op") {
        "Condo/co-op master policies typically cover exterior structure, roof, and common-area liability (Bare Walls or Walls-In per CC&Rs) while excluding unit interiors/personal property. Verify master wind/water deductible % (5%–10%+ master deductibles create special-assessment exposure; carry HO-6 with >= $50k loss-assessment coverage).".to_string()
    } else if pt_lc.contains("town") {
        "Townhome/PUD master coverage varies widely: confirm whether the HOA master policy insures the roof and exterior siding (HO-6) or if each owner must carry full structural HO-3 dwelling coverage.".to_string()
    } else {
        "Single-family HOA master policies normally cover common areas, gates, and amenities only; buyer still requires full HO-3 structural coverage.".to_string()
    };

    let low_text = ctx.combined_text.to_ascii_lowercase();
    let mut included: Vec<String> = Vec::new();
    let mut excluded_or_verify: Vec<String> = Vec::new();
    if let Some(inc_arr) = ctx
        .facts
        .pointer("/hoa/includes")
        .or_else(|| ctx.facts.pointer("/hoa/amenities"))
        .and_then(|v| v.as_array())
    {
        for item in inc_arr {
            if let Some(s) = val_str(Some(item)) {
                included.push(s.to_string());
            }
        }
    }
    if included.is_empty() && has_hoa {
        if pt_lc.contains("condo") || pt_lc.contains("co-op") {
            included.extend([
                "Exterior building envelope & roof maintenance".into(),
                "Common-area landscaping, lighting & master liability/hazard policy".into(),
                "Typically water, sewer, and/or trash (confirm on HOA budget)".into(),
            ]);
            excluded_or_verify.extend([
                "Interior unit electricity, gas/HVAC, and internet (unless bulk-billed)".into(),
                "HO-6 walls-in coverage, personal property, & loss-assessment endorsement".into(),
                "Earthquake / Flood master policy (frequently excluded — verify separately)".into(),
            ]);
        } else if pt_lc.contains("town") {
            included.extend([
                "Common grounds, private street/driveway paving & exterior maintenance (verify roof/paint split)".into(),
                "Shared amenities (pool, clubhouse, gate if applicable)".into(),
            ]);
            excluded_or_verify.extend([
                "Unit electricity, gas, water/sewer/trash (often individually metered)".into(),
                "Interior appliances, HVAC, and roof/siding if HOA is exterior-exempt".into(),
            ]);
        } else {
            included.push(
                "Common-area upkeep, neighborhood entry/landscaping & shared amenities".into(),
            );
            excluded_or_verify.extend([
                "All home structural maintenance, roof, lot landscaping & utilities".into(),
                "Full HO-3 dwelling, earthquake & flood insurance".into(),
            ]);
        }
    } else if !has_hoa {
        excluded_or_verify.push(
            "Owner responsible for 100% of utilities, roof, exterior, landscaping, and insurance."
                .into(),
        );
    }
    for kw in [
        "water",
        "trash",
        "sewer",
        "pool",
        "roof",
        "cable",
        "internet",
        "earthquake",
    ] {
        if has_hoa
            && low_text.contains(kw)
            && !included.iter().any(|x| x.to_ascii_lowercase().contains(kw))
        {
            included.push(format!(
                "Mentioned in listing/HOA notes: {kw} (verify exact scope)"
            ));
        }
    }

    // Reserves & financial health
    let percent_funded = ctx
        .hoa_review
        .and_then(|hr| val_f64(hr.pointer("/financialHealth/percentFunded")))
        .or_else(|| val_f64(ctx.facts.pointer("/hoa/percentFunded")));
    let delinquency_pct = ctx
        .hoa_review
        .and_then(|hr| val_f64(hr.pointer("/financialHealth/delinquencyRatePct")))
        .or_else(|| val_f64(ctx.facts.pointer("/hoa/delinquencyRatePct")));
    let reserve_alloc_pct = ctx
        .hoa_review
        .and_then(|hr| val_f64(hr.pointer("/financialHealth/reserveAllocationPctOfBudget")))
        .or_else(|| val_f64(ctx.facts.pointer("/hoa/reserveAllocationPct")));
    let hoa_debt_usd = ctx
        .hoa_review
        .and_then(|hr| val_i64(hr.pointer("/financialHealth/hoaDebtUsd")))
        .or_else(|| val_i64(ctx.facts.pointer("/hoa/debtUsd")));

    let litigation_items: Vec<Value> = ctx
        .hoa_review
        .and_then(|hr| hr.pointer("/structuralAndStatutoryMandates/litigation"))
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| {
            ctx.facts
                .pointer("/hoa/litigation")
                .and_then(|v| v.as_array())
                .cloned()
        })
        .unwrap_or_default();
    let has_litigation = !litigation_items.is_empty()
        || ctx
            .facts
            .pointer("/hoa/inLitigation")
            .and_then(|v| v.as_bool())
            == Some(true)
        || (has_hoa && low_text.contains("hoa litigation"));

    let reserves_risk = if !has_hoa {
        json!({
            "level": "Not Applicable",
            "percentFunded": Value::Null,
            "reserveAllocationPct": Value::Null,
            "summary": "No HOA detected; owner directly funds individual capital expenditure (CapEx) reserves.",
        })
    } else if let Some(pf) = percent_funded {
        let lvl = if pf < 30.0 {
            "Critical (<30% Funded)"
        } else if pf < 70.0 {
            "Moderate / Watch (30–69% Funded)"
        } else {
            "Strong (>=70% Funded)"
        };
        json!({
            "level": lvl,
            "percentFunded": round_n(pf, 1),
            "reserveAllocationPct": reserve_alloc_pct.map(|p| round_n(p, 1)),
            "summary": format!(
                "HOA reserve fund is {:.1}% funded ({lvl}){}.",
                pf,
                if pf < 30.0 {
                    " — severe special-assessment and sharp dues-hike risk"
                } else if pf < 70.0 {
                    " — monitor upcoming roof/siding/piping capital outlays"
                } else {
                    ""
                }
            ),
        })
    } else {
        let age_watch = ctx.year_built.map(|y| y < 1995).unwrap_or(false) && is_attached;
        let statutory_note = if ctx.state == "CA" && is_attached {
            " Check CA SB 326 / SB 721 Exterior Elevated Elements (balcony/deck) inspection compliance."
        } else if ctx.state == "FL" && is_attached {
            " Check FL Milestone Inspection & SIRS (Structural Integrity Reserve Study — statutory ban on waiving structural reserves)."
        } else {
            ""
        };
        json!({
            "level": if age_watch { "Elevated / Unverified (Pre-1995 Building)" } else { "Unverified Pre-PDF — Request Reserve Study" },
            "percentFunded": Value::Null,
            "reserveAllocationPct": reserve_alloc_pct.map(|p| round_n(p, 1)),
            "summary": format!(
                "No HOA Reserve Study uploaded yet; Fannie/Freddie require >=10% of annual budget allocated to reserves and healthy reserve funding (<30% funded = high special-assessment risk).{}",
                statutory_note
            ),
        })
    };

    // Requirements & Rules extraction
    let hr_rules = ctx
        .hoa_review
        .and_then(|hr| hr.get("governanceAndLifestyleRules"));
    let rental_cap = if let Some(rc) = hr_rules
        .and_then(|r| r.get("rentalCap"))
        .filter(|v| v.is_object())
    {
        json!({
            "restricted": rc.get("hasRentalCap").and_then(|v| v.as_bool()).unwrap_or(false),
            "capPercent": rc.get("capPercent").cloned().unwrap_or(Value::Null),
            "hasWaitlist": rc.get("hasWaitlist").and_then(|v| v.as_bool()).unwrap_or(false),
            "waitlistCount": rc.get("waitlistCount").cloned().unwrap_or(Value::Null),
            "summary": val_str(rc.get("summary")).unwrap_or("Extracted from HOA review."),
        })
    } else if let Some(caps) = RE_RENTAL_CAP.captures(&ctx.combined_text) {
        let pct = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok());
        let has_wl = low_text.contains("waitlist");
        json!({
            "restricted": true,
            "capPercent": pct,
            "hasWaitlist": has_wl,
            "waitlistCount": Value::Null,
            "summary": format!("Listing/HOA notes reference rental restriction{}.", pct.map(|p| format!(" ({p:.0}% cap)")).unwrap_or_default()),
        })
    } else {
        json!({
            "restricted": false,
            "capPercent": Value::Null,
            "hasWaitlist": false,
            "waitlistCount": Value::Null,
            "summary": if has_hoa { "No rental cap detected in public/listing data — verify leasing cap & waitlist in CC&Rs." } else { "No HOA rental restriction reported." },
        })
    };

    let str_rule = if let Some(st_o) = hr_rules
        .and_then(|r| r.get("shortTermRental"))
        .filter(|v| v.is_object())
    {
        json!({
            "restricted": st_o.get("strProhibited").and_then(|v| v.as_bool()).unwrap_or(false) || st_o.get("minimumLeaseDays").is_some(),
            "prohibited": st_o.get("strProhibited").and_then(|v| v.as_bool()).unwrap_or(false),
            "minimumLeaseDays": st_o.get("minimumLeaseDays").cloned().unwrap_or(Value::Null),
            "summary": val_str(st_o.get("summary")).unwrap_or("Extracted from HOA review."),
        })
    } else if let Some(caps) = RE_STR_RULE.captures(&ctx.combined_text) {
        let min_days = caps
            .get(1)
            .or_else(|| caps.get(2))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        json!({
            "restricted": true,
            "prohibited": low_text.contains("prohibit") || low_text.contains("no airbnb") || low_text.contains("no short"),
            "minimumLeaseDays": min_days,
            "summary": "Short-term rental / minimum lease restriction referenced; verify CC&R lease-term rules & local municipal STR permit rules.",
        })
    } else {
        json!({
            "restricted": false,
            "prohibited": false,
            "minimumLeaseDays": Value::Null,
            "summary": if has_hoa { "Not specified pre-PDF — many HOAs impose 30-day to 1-year minimum lease terms barring Airbnb/VRBO." } else { "Verify municipal short-term rental ordinance." },
        })
    };

    let pet_policy = if let Some(pr) = hr_rules
        .and_then(|r| r.get("petRestrictions"))
        .filter(|v| v.is_object())
    {
        pr.clone()
    } else if let Some(caps) = RE_PET_RULE.captures(&ctx.combined_text) {
        let max_pets = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
        let wt = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
        let no_pets = low_text.contains("no pets") || low_text.contains("pets not allowed");
        json!({
            "restricted": true,
            "petsAllowed": !no_pets,
            "maxCount": max_pets,
            "maxWeightLbs": wt,
            "summary": if no_pets { "Listing/HOA notes indicate pets prohibited (subject to statutory assistance-animal rules)." } else { "Pet weight/count/breed limits referenced — verify exact HOA pet rules." },
        })
    } else {
        json!({
            "restricted": false,
            "petsAllowed": Value::Null,
            "maxCount": Value::Null,
            "maxWeightLbs": Value::Null,
            "summary": if has_hoa { "Pet policy unverified pre-PDF — check HOA Rules & Regulations for weight, count, or breed caps." } else { "No HOA pet cap reported." },
        })
    };

    let age_55_hit = RE_AGE_55.is_match(&ctx.combined_text)
        || pt_lc.contains("55+")
        || pt_lc.contains("senior")
        || ctx
            .facts
            .pointer("/hoa/ageRestricted")
            .and_then(|v| v.as_bool())
            == Some(true);
    let age_min = if low_text.contains("62+") || low_text.contains("62 and older") {
        Some(62)
    } else if age_55_hit {
        Some(55)
    } else {
        None
    };
    let occupancy_rule = json!({
        "isAgeRestricted": age_55_hit,
        "minAge": age_min,
        "occupancyRule": if age_55_hit {
            format!("{}+ Senior / HOPA Community Covenant", age_min.unwrap_or(55))
        } else if low_text.contains("owner-occupied only") || low_text.contains("owner occupied only") {
            "Owner-occupancy covenant referenced".to_string()
        } else {
            "Standard residential occupancy (verify CC&Rs)".to_string()
        },
        "summary": if age_55_hit {
            "Age-restricted Housing for Older Persons Act (HOPA) covenant detected — confirm occupant eligibility and resale pool constraints."
        } else {
            "No age-restriction covenant detected in supplied listing/HOA metadata."
        },
    });

    let board_hit = RE_BOARD_MOVEIN.is_match(&ctx.combined_text)
        || pt_lc.contains("co-op")
        || pt_lc.contains("coop");
    let board_movein = json!({
        "requiresBoardApproval": board_hit && (low_text.contains("board") || pt_lc.contains("co-op") || pt_lc.contains("coop") || low_text.contains("rofr") || low_text.contains("first refusal")),
        "moveInOrTransferFeeMentioned": low_text.contains("move-in fee") || low_text.contains("move in fee") || low_text.contains("capital contribution") || low_text.contains("flip tax"),
        "summary": if board_hit {
            "Board approval, right of first refusal (ROFR), move-in fee, flip tax, or capital contribution fee detected — budget extra escrow time and closing fee."
        } else if has_hoa {
            "Confirm HOA transfer fee, move-in/elevator fee, and capital contribution fee on the Resale Certificate / Demand."
        } else {
            "No HOA board approval or move-in fee expected."
        },
    });

    let warrantability = ctx.hoa_review.and_then(|hr| hr.get("warrantability"));
    let is_warrantable_opt = warrantability
        .and_then(|w| w.get("isWarrantable"))
        .and_then(|v| v.as_bool());
    let fha_va_alert = if is_attached || has_hoa {
        let risk_lvl = if is_warrantable_opt == Some(false)
            || has_litigation
            || delinquency_pct.map(|d| d > 15.0).unwrap_or(false)
            || percent_funded.map(|p| p < 25.0).unwrap_or(false)
        {
            "High (Potential Non-Warrantable / FHA-VA Rejection)"
        } else if pt_lc.contains("condo") || pt_lc.contains("co-op") {
            "Verify Required (FHA/VA & Fannie Mae CPM Project Review)"
        } else {
            "Low / Standard (Confirm PUD/HOA Questionnaire)"
        };
        json!({
            "applicable": true,
            "riskLevel": risk_lvl,
            "warrantableStatus": warrantability.and_then(|w| val_str(w.get("status"))).unwrap_or(if pt_lc.contains("condo") { "Unverified — Requires Condo Questionnaire" } else { "Standard Review" }),
            "summary": "Condo/PUD buyers using FHA, VA, or low-down conventional loans must confirm project approval (delinquency <=15%, >=10% reserve budget allocation, no structural/defect litigation, owner-occupancy/single-entity concentration thresholds). Non-warrantable buildings typically require 20–25%+ down and higher non-QM rates.",
            "lookupUrls": [
                { "name": "HUD / FHA Approved Condo Lookup", "url": "https://entp.hud.gov/idapp/html/condlook.cfm" },
                { "name": "VA Condo Report Lookup", "url": "https://lgy.va.gov/lgyhub/condo-report" },
            ],
        })
    } else {
        json!({
            "applicable": false,
            "riskLevel": "Not Applicable",
            "warrantableStatus": "Detached / Non-Condo",
            "summary": "Condo project warrantability rules do not apply to fee-simple detached single-family properties.",
            "lookupUrls": [],
        })
    };

    // Super-priority lien state + overall debt/lien risk
    let sp = hoa_super_priority_info(&ctx.state);
    let low_reserves_flag = percent_funded.map(|p| p < 30.0).unwrap_or(false);
    let high_delinq_flag = delinquency_pct.map(|d| d > 15.0).unwrap_or(false);
    let high_debt_or_lien = !hoa_liens.is_empty()
        || !noda_notices.is_empty()
        || !special_assessments.is_empty()
        || has_litigation
        || low_reserves_flag
        || high_delinq_flag;

    let overall_lien_risk = if !hoa_liens.is_empty() || !noda_notices.is_empty() {
        "Critical — Active HOA Lien / Delinquency Notice Detected"
    } else if !special_assessments.is_empty()
        || has_litigation
        || low_reserves_flag
        || high_delinq_flag
    {
        "Elevated — Special Assessment / Reserve / Litigation Risk"
    } else if has_hoa && sp.is_super_priority {
        "Watch — Super-Priority HOA Lien State (Clear via Estoppel)"
    } else if has_hoa {
        "Standard — Verify $0.00 Balance via Resale Certificate"
    } else {
        "Low — No HOA Lien Signals"
    };

    let mut contingency_items: Vec<Value> = Vec::new();
    if has_hoa
        || is_attached
        || !hoa_liens.is_empty()
        || !noda_notices.is_empty()
        || !special_assessments.is_empty()
    {
        contingency_items.push(json!({
            "priority": "Mandatory",
            "title": "Require Seller-Paid HOA Resale Certificate / Demand Statement verifying $0.00 balance & no pending special assessments",
            "detail": "Condition closing in escrow on receipt of an updated HOA Demand / Estoppel Statement confirming $0.00 unpaid dues, late fees, fines, or special assessments, and allocating any transfer/admin/resale fees cleanly.",
        }));
    }
    if sp.is_super_priority && has_hoa {
        contingency_items.push(json!({
            "priority": "High",
            "title": format!("{} Super-Priority HOA Lien Clearance ({})", ctx.state, sp.statute),
            "detail": sp.note,
        }));
    }
    if !hoa_liens.is_empty() || !noda_notices.is_empty() {
        contingency_items.push(json!({
            "priority": "Critical",
            "title": "Record Formal HOA Lien / NODA Release Prior to Closing",
            "detail": "Require full payoff through escrow and a recorded Release of Lien / Rescission of Notice of Delinquent Assessment confirmed on the Preliminary Title Report.",
        }));
    }
    if !special_assessments.is_empty() {
        contingency_items.push(json!({
            "priority": "High",
            "title": "Negotiate Seller Payoff of Active / Pending Special Assessments",
            "detail": "Specify in purchase contract addenda that Seller pays 100% of any levied or board-approved special assessments in full at close of escrow.",
        }));
    }

    let assoc_candidates = extract_hoa_association_candidates(ctx);
    let primary_assoc = assoc_candidates.first().cloned();
    let funding_and_board = build_hoa_funding_and_board(
        ctx,
        has_hoa,
        is_attached,
        &pt_lc,
        hoa_monthly,
        bench_mo,
        percent_funded,
        reserve_alloc_pct,
        delinquency_pct,
        has_litigation,
        primary_assoc.as_deref(),
        nonprofit_filings,
    );
    let alloc_master_pct =
        val_f64(funding_and_board.pointer("/estimatedDuesAllocation/masterInsurancePct"));
    let master_insurance = build_hoa_master_insurance(
        ctx,
        has_hoa,
        is_attached,
        &pt_lc,
        hoa_monthly,
        alloc_master_pct,
    );

    json!({
        "hasHoa": has_hoa,
        "propertyType": ctx.property_type,
        "hoaMonthly": round_n(hoa_monthly, 2),
        "hoaAnnual": hoa_annual,
        "hoaPerSqftMonth": hoa_per_sqft_month,
        "hoaFeeReported": explicit_hoa_mo.is_some(),
        "feeVsRegionalBenchmark": {
            "regionalBenchmarkMonthly": bench_mo.round() as i64,
            "benchmarkLabel": bench_label,
            "benchmarkSource": hoa_bench.source,
            "compSampleCount": hoa_bench.comp_count,
            "compP25Monthly": hoa_bench.p25,
            "compP75Monthly": hoa_bench.p75,
            "deltaMonthly": delta_mo,
            "ratioVsBenchmark": ratio_bench,
            "level": fee_tier,
            "dtiNote": "Lenders count 100% of monthly HOA dues inside back-end DTI; every $400/mo in HOA dues reduces borrowing capacity by roughly $55k–$65k at ~6.65% rates.",
        },
        "coverageBreakdown": {
            "masterInsuranceNote": master_ins_note,
            "utilitiesAndMaintenanceIncluded": {
                "included": included,
                "excludedOrVerify": excluded_or_verify,
            },
            "reservesContributionRisk": reserves_risk,
        },
        "fundingAndBoard": funding_and_board,
        "masterInsurance": master_insurance,
        "requirements": {
            "rentalCapOrWaitlist": rental_cap,
            "shortTermRentalRestriction": str_rule,
            "petPolicy": pet_policy,
            "occupancyOrAgeRestriction": occupancy_rule,
            "boardApprovalOrMoveInFee": board_movein,
            "fhaVaCondoApprovalAlert": fha_va_alert,
        },
        "debtAndLienStatus": {
            "overallRisk": overall_lien_risk,
            "hasElevatedDebtOrLienRisk": high_debt_or_lien,
            "hoaAssessmentLiens": hoa_liens,
            "hasActiveHoaLien": !hoa_liens.is_empty(),
            "nodaDetected": !noda_notices.is_empty(),
            "nodaNotices": noda_notices,
            "superPriorityLienState": {
                "state": ctx.state,
                "isSuperPriorityState": sp.is_super_priority,
                "monthsPriority": sp.months_priority,
                "statute": sp.statute,
                "notice": sp.note,
            },
            "percentFunded": percent_funded.map(|p| round_n(p, 1)),
            "delinquencyRatePct": delinquency_pct.map(|d| round_n(d, 1)),
            "hoaDebtUsd": hoa_debt_usd,
            "specialAssessments": special_assessments,
            "hasSpecialAssessmentSignal": !special_assessments.is_empty(),
            "litigation": litigation_items,
            "hasLitigationSignal": has_litigation,
            "resaleCertificateRequirement": "Require Seller-Paid HOA Resale Certificate / Demand Statement verifying $0.00 balance & no pending special assessments.",
            "buyerContingencyChecklist": contingency_items,
        },
        "consumedHoaReview": ctx.hoa_review.is_some(),
        "disclaimer": HOA_PROFILE_DISCLAIMER,
    })
}

fn encode_text_fragment(s: &str) -> String {
    let trimmed: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(48)
        .collect();
    let mut out = String::with_capacity(trimmed.len() * 2);
    for b in trimmed.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'~' => out.push(*b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn with_data_point_anchor(base_url: &str, text_hint: &str) -> String {
    if base_url.contains('#') {
        return base_url.to_string();
    }
    crate::shared::with_data_point_anchor(base_url, None, Some(text_hint))
}

fn state_sos_business_search_url(state: &str) -> (&'static str, &'static str) {
    match state {
        "CA" => (
            "California Secretary of State — BizFile Corporate Search",
            "https://bizfileonline.sos.ca.gov/search/business",
        ),
        "FL" => (
            "Florida Division of Corporations — Sunbiz HOA/Condo Entity Search",
            "https://search.sunbiz.org/Inquiry/CorporationSearch/ByName",
        ),
        "TX" => (
            "Texas Comptroller / SOS Taxable Entity & HOA Search",
            "https://mycpa.cpa.state.tx.us/coa/",
        ),
        "NY" => (
            "New York Department of State — Corporation & Business Entity Database",
            "https://apps.dos.ny.gov/publicInquiry/",
        ),
        "WA" => (
            "Washington Secretary of State — Corporations & Charities Search",
            "https://ccfs.sos.wa.gov/",
        ),
        "NV" => (
            "Nevada Secretary of State — SilverFlume Business Search",
            "https://esos.nv.gov/EntitySearch/OnlineEntitySearch",
        ),
        "CO" => (
            "Colorado Secretary of State / DORA HOA Information & Business Search",
            "https://www.sos.state.co.us/biz/BusinessEntityCriteriaExt.do",
        ),
        "IL" => (
            "Illinois Secretary of State — Corporation/LLC Search",
            "https://apps.ilsos.gov/businessentitysearch/",
        ),
        "MA" => (
            "Massachusetts Secretary of the Commonwealth — Corporate Database",
            "https://corp.sec.state.ma.us/corpweb/CorpSearch/CorpSearch.aspx",
        ),
        "PA" => (
            "Pennsylvania Department of State — Business Filing Services",
            "https://file.dos.pa.gov/search/business",
        ),
        "NJ" => (
            "New Jersey Division of Revenue — Business Name Search",
            "https://www.njportal.com/DOR/BusinessNameSearch/",
        ),
        "VA" => (
            "Virginia SCC Clerk's Information System & CICB Search",
            "https://cis.scc.virginia.gov/",
        ),
        "MD" => (
            "Maryland Business Express Entity Search",
            "https://egov.maryland.gov/BusinessExpress/EntitySearch",
        ),
        "AZ" => (
            "Arizona Corporation Commission — eCorp Entity Search",
            "https://ecorp.azcc.gov/EntitySearch/Index",
        ),
        "GA" => (
            "Georgia Corporations Division — Business Search",
            "https://ecorp.sos.ga.gov/BusinessSearch",
        ),
        "NC" => (
            "North Carolina Secretary of State — Business Registration Search",
            "https://www.sosnc.gov/online_services/search/by_title/_Business_Registration",
        ),
        _ => (
            "National Association of Secretaries of State — Corporate Registry Directory",
            "https://www.nass.org/",
        ),
    }
}

fn extract_hoa_association_candidates(ctx: &ReportContext) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let ptrs = [
        "/hoaName",
        "/associationName",
        "/hoa/associationName",
        "/hoa/name",
        "/mlsListing/associationName",
        "/listing/associationName",
        "/raw_json/AssociationName",
        "/rawJson/AssociationName",
        "/subdivision",
        "/subdivisionName",
        "/mlsListing/subdivisionName",
        "/buildingRegister/subdivision",
        "/raw_json/SubdivisionName",
        "/rawJson/SubdivisionName",
    ];
    if let Some(hr) = ctx.hoa_review {
        for p in ["/associationName", "/financialHealth/associationName"] {
            if let Some(s) = val_str(hr.pointer(p)) {
                push_clean_assoc_name(&mut out, s);
            }
        }
    }
    for p in ptrs {
        if let Some(s) = val_str(ctx.facts.pointer(p)) {
            push_clean_assoc_name(&mut out, s);
        }
    }
    if let Some(nb) = val_str(ctx.facts.get("neighborhood")) {
        let nb_norm = normalize_city_slug(nb);
        if nb_norm != normalize_city_slug(&ctx.city)
            && nb_norm != normalize_city_slug(&ctx.county)
            && !nb.eq_ignore_ascii_case(&ctx.state)
        {
            push_clean_assoc_name(&mut out, nb);
        }
    }
    // Only trust county assessor HOA/subdivision attributes if situs matches the subject street number (prevents neighbor-parcel pollution)
    let situs_full = val_str(ctx.facts.pointer("/countyRawAttributes/SITUSFULL"))
        .or_else(|| val_str(ctx.facts.pointer("/countyRawAttributes/SitusFull")));
    let subj_num = ctx.street.split_whitespace().next().unwrap_or("");
    let situs_ok = situs_full
        .map(|sf| subj_num.is_empty() || sf.split_whitespace().next() == Some(subj_num))
        .unwrap_or(true);
    if situs_ok {
        for p in [
            "/countyRawAttributes/NeighborhoodAssociation",
            "/countyRawAttributes/HOA_NAME",
            "/countyRawAttributes/SUBDIVISION",
        ] {
            if let Some(s) = val_str(ctx.facts.pointer(p)) {
                push_clean_assoc_name(&mut out, s);
            }
        }
    }
    out
}

fn push_clean_assoc_name(out: &mut Vec<String>, raw: &str) {
    let cleaned = raw
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == ',');
    if cleaned.len() < 4 || cleaned.len() > 90 {
        return;
    }
    let lc = cleaned.to_ascii_lowercase();
    let skip_exact = [
        "none",
        "no hoa",
        "n/a",
        "na",
        "null",
        "unknown",
        "not applicable",
        "single family",
        "residential",
        "unincorporated",
        "metes and bounds",
    ];
    if skip_exact.contains(&lc.as_str()) {
        return;
    }
    if cleaned
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c == ' ')
    {
        return;
    }
    if !out.iter().any(|e| e.eq_ignore_ascii_case(cleaned)) {
        out.push(cleaned.to_string());
    }
}

#[allow(clippy::too_many_arguments)]
fn build_hoa_funding_and_board(
    ctx: &ReportContext,
    has_hoa: bool,
    is_attached: bool,
    pt_lc: &str,
    hoa_monthly: f64,
    bench_mo: f64,
    percent_funded: Option<f64>,
    reserve_alloc_pct: Option<f64>,
    delinquency_pct: Option<f64>,
    has_litigation: bool,
    association_name: Option<&str>,
    nonprofit_filings: &[Value],
) -> Value {
    let (benchmark_tier, special_risk) = if !has_hoa {
        ("Not Applicable (No Mandatory HOA)", "Not Applicable")
    } else if let Some(pf) = percent_funded {
        if pf >= 70.0 {
            ("Strong (>=70% Funded)", "Low")
        } else if pf >= 30.0 {
            ("Moderate (30–69% Funded)", "Moderate")
        } else {
            (
                "Underfunded (<30% Funded — High Special-Assessment Risk)",
                "High",
            )
        }
    } else if is_attached && ctx.year_built.map(|y| y < 1995).unwrap_or(false) {
        (
            "Unverified Pre-Disclosure (Aging Attached Stock — Request Reserve Study)",
            "Verify Reserve Study",
        )
    } else {
        (
            "Unverified Pre-Disclosure — Request Current Reserve Study",
            "Verify Reserve Study",
        )
    };

    let meets_reserve_gate = reserve_alloc_pct.map(|r| r >= 10.0);
    let reserve_summary = if !has_hoa {
        "Fee-simple home with no mandatory HOA reserve pool; set aside ~1% of home value annually for personal roof/HVAC/exterior CapEx.".to_string()
    } else if let Some(pf) = percent_funded {
        format!(
            "Reserve study reports {:.1}% funded ({benchmark_tier}). Industry benchmarks treat >=70% funded as strong, 30–69% as moderate watch, and <30% as high special-assessment exposure. Conventional Fannie/Freddie condo underwriting also requires >=10% of the annual HOA operating budget allocated to reserves and <=15% 60-day delinquency.",
            pf
        )
    } else {
        "Public property records do not publish internal HOA bank reserve balances prior to escrow disclosure. Request the most recent Reserve Study and Annual Budget: >=70% funded indicates strong reserves, 30–69% moderate, and <30% signals elevated special-assessment risk; Fannie/Freddie condo loans require >=10% annual budget allocation to reserves.".to_string()
    };

    // Calibrated monthly dues allocation breakdown (never fabricates dollar line items from regional benchmark when actual HOA dues are unreported)
    let has_reported_dues = has_hoa && hoa_monthly > 0.0;
    let hoa_basis_opt: Option<f64> = if !has_hoa {
        Some(0.0)
    } else if has_reported_dues {
        Some(round_n(hoa_monthly, 2))
    } else {
        None
    };

    let budget_obj = ctx.facts.pointer("/hoa/budgetBreakdown").or_else(|| {
        ctx.hoa_review
            .and_then(|hr| hr.pointer("/financialHealth/budgetBreakdown"))
    });
    let disc_ins_pct = val_f64(budget_obj.and_then(|b| b.get("masterInsurancePct")));
    let disc_res_pct = reserve_alloc_pct
        .filter(|&p| p > 0.0 && p <= 90.0)
        .or_else(|| val_f64(budget_obj.and_then(|b| b.get("reservesPct"))));
    let disc_mgmt_pct = val_f64(budget_obj.and_then(|b| b.get("managementAndAdminPct")));
    let disc_maint_pct =
        val_f64(budget_obj.and_then(|b| b.get("commonMaintenanceAndUtilitiesPct")));
    let full_budget_disclosed = disc_ins_pct.is_some() && disc_res_pct.is_some();
    let _ = (is_attached, pt_lc, bench_mo);

    let (res_pct_opt, ins_pct_opt, maint_pct_opt, mgmt_pct_opt) = if !has_hoa {
        (Some(0.0), Some(0.0), Some(0.0), Some(0.0))
    } else if full_budget_disclosed {
        let r = round_n(disc_res_pct.unwrap_or(0.0), 1);
        let i = round_n(disc_ins_pct.unwrap_or(0.0), 1);
        let mg = round_n(disc_mgmt_pct.unwrap_or(0.0), 1);
        let mt = disc_maint_pct
            .map(|x| round_n(x, 1))
            .unwrap_or_else(|| round_n((100.0 - r - i - mg).max(0.0), 1));
        (Some(r), Some(i), Some(mt), Some(mg))
    } else {
        (disc_res_pct.map(|p| round_n(p, 1)), None, None, None)
    };

    let estimated_dues_allocation = json!({
        "applicable": has_hoa,
        "disclosed": !has_hoa || full_budget_disclosed || disc_res_pct.is_some(),
        "hoaMonthlyBasisUsd": hoa_basis_opt,
        "usesRegionalBenchmarkBasis": false,
        "reservesPct": res_pct_opt,
        "reservesMonthlyUsd": res_pct_opt.and_then(|p| hoa_basis_opt.map(|m| round_n(m * p / 100.0, 2))),
        "masterInsurancePct": ins_pct_opt,
        "masterInsuranceMonthlyUsd": ins_pct_opt.and_then(|p| hoa_basis_opt.map(|m| round_n(m * p / 100.0, 2))),
        "commonMaintenanceAndUtilitiesPct": maint_pct_opt,
        "commonMaintenanceMonthlyUsd": maint_pct_opt.and_then(|p| hoa_basis_opt.map(|m| round_n(m * p / 100.0, 2))),
        "managementAndAdminPct": mgmt_pct_opt,
        "managementMonthlyUsd": mgmt_pct_opt.and_then(|p| hoa_basis_opt.map(|m| round_n(m * p / 100.0, 2))),
        "typicalBenchmarkRangesPct": if has_hoa {
            json!({
                "reserves": "15%–25%",
                "masterInsurance": if pt_lc.contains("condo") || pt_lc.contains("town") { "20%–32%" } else { "8%–15%" },
                "commonMaintenanceAndUtilities": "35%–45%",
                "managementAndAdmin": "10%–15%",
            })
        } else {
            Value::Null
        },
        "basisNote": if !has_hoa {
            "No mandatory HOA dues reported."
        } else if !has_reported_dues {
            "Exact monthly HOA dues were not reported in public parcel records; upload the HOA Annual Operating Budget to inspect exact line-item shares."
        } else if full_budget_disclosed {
            "Budget line-item shares extracted directly from supplied HOA financial disclosures."
        } else if disc_res_pct.is_some() {
            "Reserve allocation % taken from supplied HOA disclosure; insurance, maintenance, and management line items were not separately disclosed in public records — confirm exact breakdown on the HOA Annual Operating Budget."
        } else {
            "HOA dues allocation is not published in public parcel/MLS records (values omitted rather than guessed). Upload the HOA Operating Budget / Reserve Study to verify exact reserve, master insurance, maintenance, and management shares."
        },
    });

    // Statutory reserve & structural inspection mandates
    let mut statutory_mandates: Vec<Value> = Vec::new();
    if ctx.state == "CA" && (has_hoa || is_attached) {
        statutory_mandates.push(json!({
            "title": "California Davis-Stirling Triennial Reserve Study & Annual Budget Package",
            "codeCitation": "CA Civ. Code §§ 5300, 5550",
            "applicability": "All California CIDs / HOAs",
            "summary": "Requires a visual reserve study at least every 3 years, annual reserve funding plan disclosure (percent funded, per-unit deficiency, and whether special assessments or loans are anticipated), and buyer delivery under Civ. Code § 4525.",
            "sourceName": "California Legislative Information — Civil Code § 5550",
            "sourceUrl": "https://leginfo.legislature.ca.gov/faces/codes_displaySection.xhtml?lawCode=CIV&sectionNum=5550.#:~:text=5550",
        }));
        if is_attached {
            statutory_mandates.push(json!({
                "title": "California SB-326 Balcony & Exterior Elevated Elements (EEE) Structural Inspection",
                "codeCitation": "CA Civ. Code § 5551",
                "applicability": "CA condos/attached associations with 3+ multifamily units & wood-framed balconies/decks/walkways",
                "summary": "Mandates licensed structural engineer or architect visual/bore-scope inspection of exterior elevated wood-framed balconies, decks, stairs, and walkways every 9 years (initial statutory deadline Jan 1, 2025). Ask for the signed SB-326 report and any waterproofing repair assessment.",
                "sourceName": "California Legislative Information — Civil Code § 5551 (SB-326)",
                "sourceUrl": "https://leginfo.legislature.ca.gov/faces/codes_displaySection.xhtml?lawCode=CIV&sectionNum=5551.#:~:text=5551",
            }));
        }
    } else if ctx.state == "FL" && (has_hoa || is_attached) {
        if is_attached {
            statutory_mandates.push(json!({
                "title": "Florida SB-4D / HB-1021 Structural Integrity Reserve Study (SIRS) & Reserve-Waiver Ban",
                "codeCitation": "Fla. Stat. § 718.112(2)(g)",
                "applicability": "Florida condo & cooperative buildings 3+ habitable stories",
                "summary": "Mandates a 10-year Structural Integrity Reserve Study (roof, load-bearing walls, floor, foundation, fireproofing, plumbing, electrical, waterproofing, windows) and legally prohibits unit owners/boards from voting to waive or underfund SIRS structural reserves.",
                "sourceName": "Florida Senate Statutes — § 718.112",
                "sourceUrl": "https://www.flsenate.gov/Laws/Statutes/2024/0718.112#:~:text=Structural%20integrity%20reserve%20study",
            }));
            statutory_mandates.push(json!({
                "title": "Florida Milestone Structural Safety Inspection",
                "codeCitation": "Fla. Stat. § 553.899",
                "applicability": "Florida condo/co-op buildings 3+ stories at age 30 years (or 25 years within 3 miles of coastline)",
                "summary": "Requires Phase 1 licensed engineer/architect structural inspection (and Phase 2 destructive/NDT testing if substantial structural deterioration is found), with summary distributed to unit owners and prospective buyers.",
                "sourceName": "Florida Senate Statutes — § 553.899",
                "sourceUrl": "https://www.flsenate.gov/Laws/Statutes/2024/0553.899#:~:text=553.899",
            }));
        } else {
            statutory_mandates.push(json!({
                "title": "Florida Homeowners' Association Financial Reporting & Estoppel Rules",
                "codeCitation": "Fla. Stat. §§ 720.303, 720.30851",
                "applicability": "Florida mandatory SFR / PUD HOAs",
                "summary": "Governs annual HOA budgets, reserve account disclosures, member inspection of official records within 10 business days, and binding estoppel certificates capping closing arrears.",
                "sourceName": "Florida Senate Statutes — § 720.303",
                "sourceUrl": "https://www.flsenate.gov/Laws/Statutes/2024/0720.303",
            }));
        }
    } else if ctx.state == "WA" && (has_hoa || is_attached) {
        statutory_mandates.push(json!({
            "title": "Washington WUCIOA / Condo Annual Reserve Study & Resale Certificate",
            "codeCitation": "RCW 64.34.380 / RCW 64.90.545",
            "applicability": "Washington condominiums & common-interest communities",
            "summary": "Requires annual reserve study updates (with a visual site inspection every 3 years by a reserve study professional) and delivery of a signed Resale Certificate disclosing reserve balances, pending capital expenditures, and unpaid assessments.",
            "sourceName": "Washington State Legislature — RCW 64.90.545",
            "sourceUrl": "https://app.leg.wa.gov/RCW/default.aspx?cite=64.90.545#:~:text=64.90.545",
        }));
    } else if ctx.state == "NV" && (has_hoa || is_attached) {
        statutory_mandates.push(json!({
            "title": "Nevada Five-Year Reserve Study & NRED Registration",
            "codeCitation": "NRS 116.31152 / NRS 116.4109",
            "applicability": "Nevada common-interest communities",
            "summary": "Requires a full reserve study every 5 years by a Nevada-permitted reserve study specialist, annual summary budget ratification, and statutory resale package + 9-month super-priority lien payoff check.",
            "sourceName": "Nevada Legislature — NRS Chapter 116",
            "sourceUrl": "https://www.leg.state.nv.us/nrs/nrs-116.html#:~:text=116.31152",
        }));
    }

    if is_attached || has_hoa {
        statutory_mandates.push(json!({
            "title": "Fannie Mae / Freddie Mac Condo & PUD Project Eligibility Standards",
            "codeCitation": "Fannie Mae Selling Guide B4-2.1",
            "applicability": if is_attached { "Conventional conforming condo/townhome financing" } else { "HOA / PUD delinquency & litigation underwriting" },
            "summary": "Conventional warrantability requires >=10% of annual HOA budget allocated to replacement reserves, <=15% of units 60+ days delinquent on dues, no unresolved structural/safety critical repairs or deferred maintenance orders, and no non-minor structural defect litigation.",
            "sourceName": "Fannie Mae Selling Guide — Project Standards B4-2.1",
            "sourceUrl": "https://selling-guide.fanniemae.com/sel/b4-2.1-01/general-information-project-standards#:~:text=Project%20Standards",
        }));
    }

    // Registry & corporate lookup links (zero personal PII)
    let mut registry_links: Vec<Value> = Vec::new();
    let (sos_label, sos_url) = state_sos_business_search_url(&ctx.state);
    registry_links.push(json!({
        "name": sos_label,
        "category": "Corporate & Board Officer Filing Registry",
        "sourceUrl": sos_url,
    }));
    if let Some(assoc) = association_name {
        let q_enc: String = url::form_urlencoded::byte_serialize(assoc.as_bytes()).collect();
        let pp_search = format!(
            "https://projects.propublica.org/nonprofits/search?q={q_enc}"
        );
        registry_links.push(json!({
            "name": format!("ProPublica IRS Form 990 Search — {assoc}"),
            "category": "Nonprofit Tax & Revenue Filings",
            "sourceUrl": pp_search,
        }));
    } else if has_hoa {
        registry_links.push(json!({
            "name": "ProPublica Nonprofit Explorer (IRS Form 990 HOA Search)",
            "category": "Nonprofit Tax & Revenue Filings",
            "sourceUrl": "https://projects.propublica.org/nonprofits/",
        }));
    }
    if ctx.state == "FL" && (has_hoa || is_attached) {
        registry_links.push(json!({
            "name": "Florida DBPR Condominium & Cooperative Project Search",
            "category": "State Condo Regulatory Registry",
            "sourceUrl": "https://www.myfloridalicense.com/wl11.asp?mode=0&SID=",
        }));
    }
    if ctx.state == "CO" && (has_hoa || is_attached) {
        registry_links.push(json!({
            "name": "Colorado DORA HOA Information & Resource Center Registry",
            "category": "State HOA Regulatory Registry",
            "sourceUrl": "https://dre.colorado.gov/hoa-center",
        }));
    }
    if ctx.state == "NV" && (has_hoa || is_attached) {
        registry_links.push(json!({
            "name": "Nevada Real Estate Division — CICCH HOA Ombudsman Registry",
            "category": "State HOA Regulatory Registry",
            "sourceUrl": "https://red.nv.gov/Content/CIC/Main/",
        }));
    }
    if is_attached {
        registry_links.push(json!({
            "name": "HUD / FHA Approved Condominium Project Lookup",
            "category": "Federal Mortgage Warrantability",
            "sourceUrl": "https://entp.hud.gov/idapp/html/condlook.cfm",
        }));
        registry_links.push(json!({
            "name": "VA Loan Guaranty Condo Report Lookup",
            "category": "Federal Mortgage Warrantability",
            "sourceUrl": "https://lgy.va.gov/lgyhub/condo-report",
        }));
    }

    let board_governance = if has_hoa {
        json!({
            "structure": if pt_lc.contains("co-op") || pt_lc.contains("coop") {
                "Cooperative Housing Corporation Board of Directors (Shareholder-Elected)"
            } else {
                "Nonprofit Mutual-Benefit / Homeowners Association Board of Directors (Volunteer Owner-Elected + Community Management)"
            },
            "openMeetingAndMinutesRights": "Most state common-interest statutes (e.g. CA Open Meeting Act Civ. Code § 4900, FL Ch. 718/720, TX Prop. Code § 209.0051, WA RCW 64.90) guarantee owner access to open board meetings and approved minutes. Buyers in escrow should request the last 12–24 months of Board Meeting Minutes to spot pending roof/plumbing leaks, insurance non-renewals, or upcoming special assessments before they hit the budget.",
            "buyerInspectionRights": "During the HOA disclosure contingency window, verify: (1) CC&Rs, Bylaws & Rules, (2) Current Operating Budget & Year-to-Date Income/Balance Sheet, (3) Latest Reserve Study & Percent Funded, (4) Master Insurance Declarations Page (EOI) + Deductibles, (5) 12–24 months of Board Minutes, and (6) Pending Litigation & Structural Inspection disclosures.",
            "resaleDemandRequirement": "Require a Seller-Paid Official HOA Resale Certificate / Estoppel / Demand Statement confirming $0.00 delinquent seller balance, exact monthly dues, and all transfer/working-capital fees prior to closing.",
            "litigationWatch": has_litigation,
        })
    } else {
        json!({
            "structure": "Fee-Simple Individual Ownership (No HOA Board Detected)",
            "openMeetingAndMinutesRights": "Not applicable — no mandatory homeowners association governance detected in public or listing records.",
            "buyerInspectionRights": "Confirm on the Preliminary Title Report (Schedule B) that no dormant subdivision CC&Rs, private road maintenance agreements, or architectural review covenants encumber the parcel.",
            "resaleDemandRequirement": "No HOA Resale Certificate expected unless Title Preliminary Report reveals recorded subdivision covenants.",
            "litigationWatch": false,
        })
    };

    json!({
        "associationName": association_name,
        "reserveStatus": {
            "percentFunded": percent_funded.map(|p| round_n(p, 1)),
            "benchmarkTier": benchmark_tier,
            "specialAssessmentRisk": special_risk,
            "reserveAllocationPct": reserve_alloc_pct.map(|p| round_n(p, 1)),
            "fannieFreddieReserveGateMinPct": 10.0,
            "fannieFreddieMaxDelinquencyPct": 15.0,
            "delinquencyRatePct": delinquency_pct.map(|d| round_n(d, 1)),
            "meetsFannieFreddieReserveGate": meets_reserve_gate,
            "summary": reserve_summary,
        },
        "estimatedDuesAllocation": estimated_dues_allocation,
        "statutoryMandates": statutory_mandates,
        "boardGovernance": board_governance,
        "publicFilings": {
            "associationNameDetected": association_name,
            "nonprofit990Filings": nonprofit_filings,
            "filingLookupStatus": if !nonprofit_filings.is_empty() {
                "matched_501c_990"
            } else if association_name.is_none() {
                "no_association_name_in_public_feeds"
            } else {
                "no_form_990_match_likely_1120h"
            },
            "nonprofit990PrivacyNote": "Corporate nonprofit IRS Form 990 & Secretary of State entity records only (many mutual-benefit HOAs file IRS Form 1120-H rather than Form 990); private homeowner PII is never exposed.",
            "registryLookupLinks": registry_links,
        },
    })
}

fn build_hoa_master_insurance(
    ctx: &ReportContext,
    has_hoa: bool,
    is_attached: bool,
    pt_lc: &str,
    hoa_monthly: f64,
    alloc_master_pct: Option<f64>,
) -> Value {
    let hr_mi = ctx
        .hoa_review
        .and_then(|hr| hr.pointer("/governanceAndLifestyleRules/masterInsurance"))
        .filter(|v| v.is_object());
    let uploaded_cov = hr_mi.and_then(|m| val_str(m.get("coverageType")));
    let uploaded_ded_pct = hr_mi.and_then(|m| val_f64(m.get("deductiblePercent")));
    let deductible_shock = hr_mi
        .and_then(|m| m.get("deductibleShockWarning"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || matches!(ctx.state.as_str(), "FL" | "LA") && is_attached;

    let is_condo_coop =
        pt_lc.contains("condo") || pt_lc.contains("co-op") || pt_lc.contains("coop");
    let is_townhome = !is_condo_coop && pt_lc.contains("town");

    let policy_form = if let Some(cov) = uploaded_cov {
        let cov_lc = cov.to_ascii_lowercase();
        if cov_lc.contains("bare") || cov_lc.contains("walls-out") || cov_lc.contains("studs") {
            "Bare Walls / Walls-Out (Confirmed in HOA Disclosure)".to_string()
        } else if cov_lc.contains("all-in")
            || cov_lc.contains("walls-in")
            || cov_lc.contains("inclusive")
        {
            "All-In / Walls-In (Confirmed in HOA Disclosure)".to_string()
        } else if cov_lc.contains("single") || cov_lc.contains("original") {
            "Single Entity / Original Specifications (Confirmed in HOA Disclosure)".to_string()
        } else {
            format!("{cov} (Confirmed in HOA Disclosure)")
        }
    } else if !has_hoa {
        "None — Fee-Simple Detached (Owner Carries Full HO-3 Dwelling Policy)".to_string()
    } else if is_condo_coop {
        "Single Entity / Original Specs or Bare Walls (Typical Condo Master Form — Verify CC&Rs & EOI)".to_string()
    } else if is_townhome {
        "Exterior Shell (Walls-Out) or Common-Area Only (Townhome Split — Verify Roof Covenant in CC&Rs)".to_string()
    } else {
        "Common-Area Only (SFR / PUD Master Liability, Gates & Shared Amenities)".to_string()
    };

    let policy_form_guide = json!([
        {
            "form": "Bare Walls / Walls-Out (Studs-Out)",
            "scope": "HOA master policy stops at the unfinished framing, roof, exterior siding, and common elements. Unit owner's HO-6 policy must insure interior drywall, subfloors, cabinets, countertops, flooring, bathroom fixtures, and built-in appliances."
        },
        {
            "form": "Single Entity / Original Specifications",
            "scope": "HOA master policy covers the building shell plus standard developer-grade interior fixtures/flooring as originally built, but EXCLUDES subsequent kitchen/bath remodel upgrades (which require HO-6 Dwelling/Additions & Alterations coverage)."
        },
        {
            "form": "All-In / Walls-In (Inclusive)",
            "scope": "HOA master policy covers exterior structure, interior fixtures, and betterments/upgrades inside the unit, leaving the owner's HO-6 primarily responsible for personal property, loss of use, personal liability, and the master policy deductible."
        },
        {
            "form": "Common-Area Only (SFR / Fee-Simple PUD)",
            "scope": "HOA master policy covers shared roads, gates, parks, pools, and clubhouses only. The homeowner must carry a full HO-3 / HO-5 dwelling replacement-cost policy on 100% of the house structure and roof."
        }
    ]);

    // Master insurance per-unit dues share: ONLY populate when disclosed by HOA EOI / budget breakdown
    let effective_dues_mo = if has_hoa && hoa_monthly > 0.0 {
        hoa_monthly
    } else {
        0.0
    };
    let cat_mult = match ctx.state.as_str() {
        "FL" | "LA" => 1.55,
        "CA" | "TX" | "CO" | "OK" | "HI" => 1.28,
        _ => 1.0,
    };
    let sqft_opt = ctx.sqft.filter(|&s| s > 0.0);
    let disclosed_master_ann = hr_mi
        .and_then(|m| {
            val_f64(m.get("annualPerUnitUSD")).or_else(|| val_f64(m.get("annualPremiumPerUnit")))
        })
        .or_else(|| val_f64(ctx.facts.pointer("/hoa/masterInsuranceAnnualPerUnit")));

    let (master_ann_usd, master_share_pct): (Option<i64>, Option<f64>) = if !has_hoa {
        (Some(0), Some(0.0))
    } else if let Some(ann) = disclosed_master_ann.filter(|&a| a > 0.0) {
        let pct = (effective_dues_mo > 0.0)
            .then(|| round_n((ann / (effective_dues_mo * 12.0)) * 100.0, 1))
            .or(alloc_master_pct);
        (Some(ann.round() as i64), pct)
    } else if let Some(pct) = alloc_master_pct.filter(|&p| p > 0.0) {
        let ann = (effective_dues_mo > 0.0)
            .then(|| (effective_dues_mo * 12.0 * (pct / 100.0)).round() as i64);
        (ann, Some(pct))
    } else {
        (None, None)
    };
    let master_mo_usd: Option<f64> = master_ann_usd.map(|a| round_n(a as f64 / 12.0, 2));

    let ded_range = if let Some(dp) = uploaded_ded_pct {
        format!(
            "{dp:.1}% of insured building value per HOA disclosure{}",
            if deductible_shock {
                " (HIGH DEDUCTIBLE — carry strong HO-6 Loss Assessment coverage)"
            } else {
                ""
            }
        )
    } else if !has_hoa {
        "Not applicable (owner selects individual HO-3 deductible, typically $1,000–$5,000)"
            .to_string()
    } else if matches!(ctx.state.as_str(), "FL" | "LA" | "TX" | "HI") && is_attached {
        "$25,000–$100,000 standard water/all-peril deductible; 2%–5% named-storm/hurricane percentage deductible on total building value".to_string()
    } else if ctx.state == "CA" && is_attached {
        "$10,000–$50,000 water/fire master deductible (higher in wildfire brushstrokes); earthquake master coverage is typically excluded or carries a 10%–15% deductible".to_string()
    } else if is_attached {
        "$10,000–$50,000 per-occurrence master hazard/water deductible (verify exact amount on HOA Evidence of Insurance)".to_string()
    } else {
        "$5,000–$25,000 common-area commercial property/GL deductible".to_string()
    };

    // Owner individual policy (HO-6 vs HO-3) + Loss Assessment Coverage
    let (owner_policy_type, owner_ann_usd, loss_assess_usd): (&str, Option<i64>, i64) =
        if is_condo_coop {
            let ho6 =
                sqft_opt.map(|sqft| (sqft * 0.72 * cat_mult).clamp(620.0, 2400.0).round() as i64);
            let la = if deductible_shock || cat_mult > 1.2 {
                75_000
            } else {
                50_000
            };
            ("HO-6 Condo / Walls-In Policy", ho6, la)
        } else if is_townhome && has_hoa {
            let ho6_or_3 = (ctx.annual_ins * 0.68).clamp(850.0, 3400.0).round() as i64;
            let la = if deductible_shock || cat_mult > 1.2 {
                50_000
            } else {
                35_000
            };
            (
                "HO-6 Walls-In (if HOA insures roof/shell) or HO-3 Dwelling Policy (verify CC&Rs)",
                Some(ho6_or_3),
                la,
            )
        } else if has_hoa {
            (
                "HO-3 / HO-5 Full Dwelling Replacement-Cost Policy",
                Some(ctx.annual_ins.round() as i64),
                25_000,
            )
        } else {
            (
                "HO-3 / HO-5 Full Dwelling Replacement-Cost Policy",
                Some(ctx.annual_ins.round() as i64),
                0,
            )
        };

    // 8-row coverage matrix
    let coverage_matrix = if !has_hoa {
        json!([
            {
                "category": "Roof & Exterior Shell (Siding, Stucco, Windows)",
                "primaryPayer": "Homeowner (HO-3)",
                "masterPolicyCoverage": "None — no HOA master policy.",
                "ownerPolicyCoverage": "100% covered under owner's HO-3 Dwelling (Coverage A) subject to policy deductible and roof-age settlement terms.",
                "buyerVerificationTip": "Confirm Replacement Cost Value (RCV) rather than depreciated Actual Cash Value (ACV) on roofs >15 years old."
            },
            {
                "category": "Structural Framing & Foundation",
                "primaryPayer": "Homeowner (HO-3 + Endorsements)",
                "masterPolicyCoverage": "None — no HOA master policy.",
                "ownerPolicyCoverage": "Sudden fire/wind covered by HO-3; earth movement/quake and FEMA flood require separate policies.",
                "buyerVerificationTip": "Quote separate earthquake (e.g. CEA) or NFIP/private flood coverage if located in a seismic or flood zone."
            },
            {
                "category": "Common Areas, Pools, Clubhouses & Private Roads",
                "primaryPayer": "Not Applicable",
                "masterPolicyCoverage": "No shared HOA common elements reported.",
                "ownerPolicyCoverage": "Private yard, driveway, fences, and detached ADU/shed covered under HO-3 Coverage A / Coverage B (Other Structures).",
                "buyerVerificationTip": "Verify Coverage B limit (>=10% of Coverage A) if the lot has a detached garage, pool, or ADU."
            },
            {
                "category": "Interior Drywall, Cabinets, Flooring & Upgrades",
                "primaryPayer": "Homeowner (HO-3)",
                "masterPolicyCoverage": "None.",
                "ownerPolicyCoverage": "100% covered under HO-3 Dwelling (Coverage A); add Extended Replacement Cost (125%–150%) for remodel finishes.",
                "buyerVerificationTip": "Update Coverage A rebuild limit after any kitchen, bath, or addition remodel."
            },
            {
                "category": "Personal Property, Electronics & Furnishings",
                "primaryPayer": "Homeowner (HO-3 Coverage C)",
                "masterPolicyCoverage": "None.",
                "ownerPolicyCoverage": "Covered under HO-3 Personal Property (Coverage C, typically 50%–70% of Coverage A).",
                "buyerVerificationTip": "Elect Replacement Cost endorsement on Coverage C and schedule high-value jewelry/art separately."
            },
            {
                "category": "Interior Pipe Burst / Water Backup & Sewer Lateral",
                "primaryPayer": "Homeowner (HO-3 + Endorsements)",
                "masterPolicyCoverage": "None.",
                "ownerPolicyCoverage": "Sudden pipe burst covered; slow seepage excluded. Add Water/Sewer Backup ($10k–$25k) and Service Line coverage.",
                "buyerVerificationTip": "Add a $10k+ Service Line endorsement if the home has older clay, cast-iron, or tree-root-exposed sewer laterals."
            },
            {
                "category": "Common-Area Liability vs. Inside-Lot Personal Liability",
                "primaryPayer": "Homeowner (HO-3 Coverage E)",
                "masterPolicyCoverage": "None.",
                "ownerPolicyCoverage": "Owner carries 100% premises and personal liability ($300k–$500k recommended + personal umbrella).",
                "buyerVerificationTip": "Carry >=$500k Coverage E (plus umbrella) if the property has a pool, spa, trampoline, or steep stairs."
            },
            {
                "category": "Board D&O Liability & Fidelity / Crime Bond",
                "primaryPayer": "Not Applicable",
                "masterPolicyCoverage": "No HOA board or association funds.",
                "ownerPolicyCoverage": "Not required for fee-simple non-HOA homes.",
                "buyerVerificationTip": "Verify Schedule B of Preliminary Title Report confirms no inactive road or neighborhood association."
            }
        ])
    } else if is_attached {
        let bare_walls = policy_form.to_ascii_lowercase().contains("bare");
        json!([
            {
                "category": "Roof & Exterior Shell (Siding, Stucco, Windows)",
                "primaryPayer": if is_townhome { "Verify CC&Rs (HOA Master vs. Owner)" } else { "HOA Master Policy" },
                "masterPolicyCoverage": if is_townhome {
                    "Covered by HOA Master Policy in condo-mapped townhomes; excluded in fee-simple lot townhomes where CC&Rs assign roof/exterior to owners."
                } else {
                    "Covered by HOA Master Hazard Policy (funded via monthly HOA dues) subject to the master deductible."
                },
                "ownerPolicyCoverage": "Owner's HO-6 covers interior damage or master-deductible assessments via Loss Assessment Coverage (Coverage D/Loss Assessment).",
                "buyerVerificationTip": "Check the CC&R Maintenance Matrix and HOA Evidence of Insurance (EOI) to confirm whether windows/sliding doors and roofs are HOA-insured."
            },
            {
                "category": "Structural Framing & Foundation",
                "primaryPayer": "HOA Master Policy",
                "masterPolicyCoverage": "Load-bearing walls, floor slabs, framing, and foundation covered for fire/wind perils (seismic & flood usually excluded unless HOA buys DIC/NFIP RCBAP).",
                "ownerPolicyCoverage": "HO-6 Building Property + Loss Assessment endorsement helps cover seismic/hazard master deductible assessments if endorsed.",
                "buyerVerificationTip": "Ask whether the HOA carries a Residential Condominium Building Association Policy (RCBAP) for flood or a master earthquake policy."
            },
            {
                "category": "Common Areas, Pools, Clubhouses & Private Roads",
                "primaryPayer": "HOA Master Policy",
                "masterPolicyCoverage": "100% insured under HOA Commercial Property & General Liability policy.",
                "ownerPolicyCoverage": "Loss Assessment endorsement protects unit owner if a major uninsured/over-limit common-area loss is assessed across owners.",
                "buyerVerificationTip": "Confirm HOA Master General Liability limit is at least $1M–$2M per occurrence (Fannie Mae requirement)."
            },
            {
                "category": "Interior Drywall, Cabinets, Flooring & Upgrades",
                "primaryPayer": if bare_walls { "Unit Owner (HO-6 Building Coverage A)" } else { "Split (Master Original Specs + Owner HO-6 Upgrades)" },
                "masterPolicyCoverage": if bare_walls {
                    "EXCLUDED beyond unfinished perimeter studs/subfloor under a Bare Walls master policy."
                } else {
                    "Covers original developer-grade drywall/fixtures under Single-Entity form (or upgrades only if All-In); subject to high master deductible."
                },
                "ownerPolicyCoverage": "HO-6 Dwelling / Additions & Alterations (Coverage A) covers interior finishes, custom kitchen/bath remodels, hardwood floors, and the gap below the master deductible.",
                "buyerVerificationTip": "Carry at least 20%–30% of unit value in HO-6 Coverage A under Bare Walls, or $50k–$150k+ for remodeled interiors under Single Entity."
            },
            {
                "category": "Personal Property, Electronics & Furnishings",
                "primaryPayer": "Unit Owner (HO-6 Coverage C)",
                "masterPolicyCoverage": "0% — HOA master policy NEVER covers owner furniture, clothing, electronics, or personal belongings.",
                "ownerPolicyCoverage": "100% covered by owner's HO-6 Personal Property (Coverage C) + Loss of Use / Additional Living Expense (Coverage D).",
                "buyerVerificationTip": "Select Replacement Cost coverage on HO-6 Coverage C and verify Loss of Use covers >=12–24 months of local rent."
            },
            {
                "category": "Interior Pipe Burst / Sudden Water Damage",
                "primaryPayer": "Shared (Master Policy > Deductible; Owner HO-6 <= Master Deductible)",
                "masterPolicyCoverage": "Covers common-stack plumbing breaks above the master water deductible ($10k–$50k+); CC&Rs often charge the master deductible back to the source unit.",
                "ownerPolicyCoverage": "HO-6 Coverage A + Personal Liability + Loss Assessment covers damage below the master deductible and liability to downstairs/adjacent units.",
                "buyerVerificationTip": "Water leaks are the #1 condo claim — ensure your HO-6 explicitly covers 'Master Policy Deductible Assessment' without a $1,000 sub-limit."
            },
            {
                "category": "Common-Area Liability vs. Inside-Unit Personal Liability",
                "primaryPayer": "HOA Master (Common Areas) / Unit Owner HO-6 (Inside Unit)",
                "masterPolicyCoverage": "Covers slip-and-fall or injury in lobbies, hallways, sidewalks, parking structures, and pools.",
                "ownerPolicyCoverage": "HO-6 Personal Liability ($300k–$500k) covers guest injuries inside the unit, balcony, or caused by pets/water overflow into neighbor units.",
                "buyerVerificationTip": "Confirm zero coverage gap on exclusive-use patios/balconies (limited common elements) between the HOA GL policy and your HO-6."
            },
            {
                "category": "Board D&O Liability & Fidelity / Crime Bond",
                "primaryPayer": "HOA Master Policy",
                "masterPolicyCoverage": "Directors & Officers (D&O) liability protects volunteer board governance; Fidelity/Crime bond protects HOA reserve accounts from theft/fraud.",
                "ownerPolicyCoverage": "Loss Assessment coverage provides secondary protection against uninsured association liability judgments.",
                "buyerVerificationTip": "Fannie Mae requires Fidelity/Crime coverage >= 3 months of aggregate HOA assessments + reserve funds for projects >20 units."
            }
        ])
    } else {
        // Detached Single-Family Home inside an HOA / PUD
        json!([
            {
                "category": "Roof & Exterior Shell (Siding, Stucco, Windows)",
                "primaryPayer": "Homeowner (HO-3)",
                "masterPolicyCoverage": "0% — SFR/PUD HOA master policies almost never insure individual detached house roofs or exterior walls.",
                "ownerPolicyCoverage": "100% covered under owner's individual HO-3 / HO-5 Dwelling (Coverage A) policy.",
                "buyerVerificationTip": "Even if the HOA regulates roof tile/paint colors via Architectural Review, the homeowner pays 100% of roof insurance and replacement."
            },
            {
                "category": "Structural Framing & Foundation",
                "primaryPayer": "Homeowner (HO-3 + Flood/Quake)",
                "masterPolicyCoverage": "0% — individual lot structures are excluded from SFR HOA master coverage.",
                "ownerPolicyCoverage": "Covered by owner's HO-3 for fire/wind; add separate flood/earthquake coverage where applicable.",
                "buyerVerificationTip": "Verify HO-3 Coverage A reflects full local reconstruction cost per sq ft (excluding land value)."
            },
            {
                "category": "Common Areas, Pools, Clubhouses & Private Roads",
                "primaryPayer": "HOA Master Policy",
                "masterPolicyCoverage": "100% covered under HOA common-area property & commercial general liability policies (paid from HOA dues).",
                "ownerPolicyCoverage": "HO-3 Loss Assessment endorsement ($25,000 recommended) covers owner share if a major common-area casualty or lawsuit exceeds HOA limits.",
                "buyerVerificationTip": "Add $25k+ Loss Assessment coverage (~$15–$35/yr) on your HO-3 policy whenever buying into a gated, pool, or private-road HOA."
            },
            {
                "category": "Interior Drywall, Cabinets, Flooring & Upgrades",
                "primaryPayer": "Homeowner (HO-3)",
                "masterPolicyCoverage": "0%.",
                "ownerPolicyCoverage": "100% covered under owner's HO-3 Dwelling (Coverage A).",
                "buyerVerificationTip": "Include Building Ordinance or Law coverage (10%–25%) to fund code upgrades after any partial loss."
            },
            {
                "category": "Personal Property, Electronics & Furnishings",
                "primaryPayer": "Homeowner (HO-3 Coverage C)",
                "masterPolicyCoverage": "0%.",
                "ownerPolicyCoverage": "100% covered under owner's HO-3 Personal Property (Coverage C).",
                "buyerVerificationTip": "Elect Replacement Cost on Coverage C."
            },
            {
                "category": "Interior Pipe Burst / Water Backup & Sewer Lateral",
                "primaryPayer": "Homeowner (HO-3 + Service Line)",
                "masterPolicyCoverage": "HOA covers common storm drains/mains in private streets only; owner owns lateral from house to main.",
                "ownerPolicyCoverage": "Sudden pipe burst covered by HO-3; add Water Backup and Underground Service Line endorsements.",
                "buyerVerificationTip": "Verify where private HOA sewer/water mains end and homeowner lateral responsibility begins."
            },
            {
                "category": "Common-Area Liability vs. Inside-Lot Personal Liability",
                "primaryPayer": "HOA Master (Common Parks/Gates) / Owner HO-3 (Lot & Home)",
                "masterPolicyCoverage": "Covers HOA-owned parks, trails, gates, and clubhouses.",
                "ownerPolicyCoverage": "HO-3 Coverage E ($300k–$500k) covers injuries anywhere on your deeded lot and sidewalk frontage.",
                "buyerVerificationTip": "Check HOA master GL limits ($1M–$2M+) in the annual HOA insurance disclosure."
            },
            {
                "category": "Board D&O Liability & Fidelity / Crime Bond",
                "primaryPayer": "HOA Master Policy",
                "masterPolicyCoverage": "Covers HOA board decisions, architectural committee disputes, and reserve account fidelity.",
                "ownerPolicyCoverage": "HO-3 Loss Assessment protects against special assessments from underinsured HOA litigation.",
                "buyerVerificationTip": "Confirm active D&O coverage before closing (especially in California under Civ. Code § 5800/5805)."
            }
        ])
    };

    json!({
        "applicable": has_hoa,
        "policyForm": policy_form,
        "policyFormGuide": policy_form_guide,
        "estimatedMasterInsuranceCost": {
            "disclosed": !has_hoa || master_ann_usd.is_some(),
            "annualPerUnitUSD": master_ann_usd,
            "monthlyShareOfDuesUSD": master_mo_usd,
            "shareOfDuesPct": master_share_pct,
            "masterDeductibleTypicalRange": ded_range,
            "deductibleShockWarning": deductible_shock,
            "basisNote": if !has_hoa {
                "No HOA master policy applies; homeowner insures 100% of dwelling directly via an HO-3/HO-5 policy."
            } else if master_ann_usd.is_some() {
                "Per-unit HOA master hazard/liability share derived directly from supplied HOA insurance/budget disclosure — verify policy limits and deductibles on the Master Declarations Page."
            } else {
                "HOA Master Insurance premium and dues share are not disclosed in public parcel/MLS records — obtain the HOA Evidence of Insurance (EOI) & Annual Budget before removing contingencies."
            },
        },
        "ownerIndividualPolicy": {
            "recommendedPolicyType": owner_policy_type,
            "estimatedAnnualUSD": owner_ann_usd,
            "estimatedMonthlyUSD": owner_ann_usd.map(|a| round_n(a as f64 / 12.0, 2)),
            "recommendedLossAssessmentCoverageUSD": loss_assess_usd,
            "lossAssessmentExplanation": if has_hoa {
                format!(
                    "Standard policies often include only $1,000 of default Loss Assessment coverage. Raising Loss Assessment coverage to ${} (~$25–$60/yr) protects you if the HOA assesses unit owners to cover a $25k–$100k+ master policy wind/water deductible or uninsured common-area liability loss.",
                    loss_assess_usd
                )
            } else {
                "No HOA Loss Assessment endorsement needed unless title reveals a shared private road or amenity covenant.".to_string()
            },
        },
        "coverageMatrix": coverage_matrix,
    })
}

fn boxed_propublica_json<'a>(
    http: &'a HttpClient,
    url: String,
    send_timeout_ms: u64,
    body_timeout_ms: u64,
) -> futures::future::BoxFuture<'a, Option<Value>> {
    async move {
        let resp = tokio::time::timeout(
            Duration::from_millis(send_timeout_ms),
            http.raw()
                .get(&url)
                .header("Accept", "application/json")
                .send(),
        )
        .await
        .ok()?
        .ok()?;
        // ProPublica v2 API returns HTTP 404 when 0 organizations match `q` (common for Form 1120-H mutual-benefit HOAs).
        if !resp.status().is_success() {
            return None;
        }
        tokio::time::timeout(Duration::from_millis(body_timeout_ms), resp.json::<Value>())
            .await
            .ok()?
            .ok()
    }
    .boxed()
}

fn fetch_hoa_nonprofit_filings<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    candidates: &'a [String],
    state: &'a str,
    city: &'a str,
) -> futures::future::BoxFuture<'a, Vec<Value>> {
    async move {
        if candidates.is_empty() || http.is_offline_external_blocked() {
            return Vec::new();
        }
        let search_base = env
            .get("PROPUBLICA_NONPROFIT_API_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(PROPUBLICA_NONPROFIT_DEFAULT_API_URL);

        let subj_city_slug = normalize_city_slug(city);
        let mut matched: Vec<Value> = Vec::new();
        let mut seen_eins = std::collections::HashSet::new();

        for primary in candidates.iter().take(2) {
            let query_str = if primary.to_ascii_lowercase().contains("hoa")
                || primary.to_ascii_lowercase().contains("association")
                || primary.to_ascii_lowercase().contains("condo")
                || primary.to_ascii_lowercase().contains("owners")
            {
                primary.clone()
            } else {
                format!("{primary} homeowners association")
            };

            let Ok(mut url) = Url::parse(search_base) else {
                continue;
            };
            {
                let mut qp = url.query_pairs_mut();
                qp.append_pair("q", &query_str);
                if state.len() == 2 {
                    qp.append_pair("state[id]", state);
                }
            }

            let Some(payload) = boxed_propublica_json(http, url.into(), 3800, 2000).await else {
                continue;
            };
            let Some(orgs) = payload.get("organizations").and_then(|v| v.as_array()) else {
                continue;
            };

            let cand_tokens: Vec<String> = primary
                .to_ascii_lowercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|t| {
                    t.len() >= 3
                        && !matches!(
                            *t,
                            "the"
                                | "hoa"
                                | "inc"
                                | "assn"
                                | "association"
                                | "homeowners"
                                | "owners"
                                | "community"
                                | "condominium"
                                | "condo"
                        )
                })
                .map(String::from)
                .collect();

            for org in orgs.iter().take(12) {
                let name = val_str(org.get("name")).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let org_state = val_str(org.get("state")).unwrap_or("");
                if !state.is_empty() && !org_state.is_empty() && !org_state.eq_ignore_ascii_case(state) {
                    continue;
                }
                let org_city = val_str(org.get("city")).unwrap_or("");
                if !subj_city_slug.is_empty() && !org_city.is_empty() {
                    let org_city_slug = normalize_city_slug(org_city);
                    if org_city_slug != subj_city_slug
                        && !org_city_slug.contains(&subj_city_slug)
                        && !subj_city_slug.contains(&org_city_slug)
                    {
                        continue;
                    }
                }
                let name_lc = name.to_ascii_lowercase();
                let hoa_like = name_lc.contains("homeowner")
                    || name_lc.contains("owners")
                    || name_lc.contains("association")
                    || name_lc.contains("assn")
                    || name_lc.contains("condo")
                    || name_lc.contains("hoa")
                    || name_lc.contains("community")
                    || name_lc.contains("estates")
                    || name_lc.contains("village")
                    || name_lc.contains("towers")
                    || name_lc.contains("plaza");
                let token_match =
                    cand_tokens.is_empty() || cand_tokens.iter().any(|t| name_lc.contains(t));
                if !hoa_like || !token_match {
                    continue;
                }

                let ein_num = org
                    .get("ein")
                    .and_then(|v| {
                        v.as_i64()
                            .map(|n| n.to_string())
                            .or_else(|| v.as_str().map(String::from))
                    })
                    .unwrap_or_default();
                let clean_ein: String = ein_num.chars().filter(|c| c.is_ascii_digit()).collect();
                if clean_ein.is_empty() || !seen_eins.insert(clean_ein.clone()) {
                    continue;
                }
                let formatted_ein = if clean_ein.len() == 9 {
                    format!("{}-{}", &clean_ein[0..2], &clean_ein[2..9])
                } else {
                    clean_ein.clone()
                };

                let base_org_url = format!(
                    "https://projects.propublica.org/nonprofits/organizations/{clean_ein}"
                );
                let source_url = with_data_point_anchor(&base_org_url, name);

                let rev = val_i64(
                    org.get("income_amount")
                        .or_else(|| org.get("totrevenue"))
                        .or_else(|| org.get("revenue_amount")),
                );
                let assets = val_i64(org.get("asset_amount").or_else(|| org.get("totassetsend")));
                let liab = val_i64(org.get("totliabend").or_else(|| org.get("liabilities_amount")));
                let tax_prd = org
                    .get("tax_period")
                    .or_else(|| org.get("tax_prd_yr"))
                    .or_else(|| org.get("filing_year"))
                    .and_then(|v| {
                        v.as_i64()
                            .map(|n| n.to_string())
                            .or_else(|| v.as_str().map(String::from))
                    });
                let subsec = org
                    .get("subseccd")
                    .or_else(|| org.get("subsection_code"))
                    .and_then(|v| {
                        v.as_i64()
                            .map(|n| format!("501(c)({n})"))
                            .or_else(|| v.as_str().map(String::from))
                    });

                matched.push(json!({
                    "ein": formatted_ein,
                    "organizationName": name,
                    "city": val_str(org.get("city")),
                    "state": org_state,
                    "subsection": subsec,
                    "latestTaxPeriod": tax_prd,
                    "totalRevenueUsd": rev,
                    "totalAssetsUsd": assets,
                    "totalLiabilitiesUsd": liab,
                    "sourceName": "ProPublica Nonprofit Explorer (IRS Form 990)",
                    "sourceUrl": source_url,
                }));
                if matched.len() >= 3 {
                    break;
                }
            }
            if matched.len() >= 3 {
                break;
            }
        }

        // Enrich top match from `/organizations/{ein}.json` if revenue/liabilities were omitted in search summary
        if let Some(first) = matched.first_mut() {
            let needs_detail = first.get("totalRevenueUsd").map(|v| v.is_null()).unwrap_or(true)
                || first.get("totalLiabilitiesUsd").map(|v| v.is_null()).unwrap_or(true);
            if needs_detail {
                let ein_digits: String = first
                    .get("ein")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect();
                if !ein_digits.is_empty() {
                    let org_json_url = if let Some(prefix) = search_base.strip_suffix("/search.json") {
                        format!("{prefix}/organizations/{ein_digits}.json")
                    } else {
                        format!("https://projects.propublica.org/nonprofits/api/v2/organizations/{ein_digits}.json")
                    };
                    if let Some(detail) = boxed_propublica_json(http, org_json_url, 2200, 1500).await {
                        if let Some(filing) = detail
                            .get("filings_with_data")
                            .and_then(|v| v.as_array())
                            .and_then(|a| a.first())
                        {
                            if let Some(rv) = val_i64(filing.get("totrevenue")) {
                                first["totalRevenueUsd"] = json!(rv);
                            }
                            if let Some(asst) = val_i64(filing.get("totassetsend")) {
                                first["totalAssetsUsd"] = json!(asst);
                            }
                            if let Some(lb) = val_i64(filing.get("totliabend")) {
                                first["totalLiabilitiesUsd"] = json!(lb);
                            }
                            if let Some(yr) = filing
                                .get("tax_prd_yr")
                                .and_then(|v| {
                                    v.as_i64()
                                        .map(|n| n.to_string())
                                        .or_else(|| v.as_str().map(String::from))
                                })
                            {
                                first["latestTaxPeriod"] = json!(yr);
                            }
                        }
                    }
                }
            }
        }

        matched
    }
    .boxed()
}

// ------------------------- 2(a). Cash-to-Close, Transfer Taxes & 2-1 Buydown -------------------------

fn estimate_transfer_tax(ctx: &ReportContext, base_loan_for_mrt: f64) -> TransferTaxResult {
    estimate_transfer_tax_schedule(
        &ctx.state,
        &ctx.county,
        &ctx.county_fips,
        &ctx.city,
        ctx.price,
        base_loan_for_mrt,
    )
}

fn build_cash_to_close(ctx: &ReportContext, hoa_monthly: f64) -> Value {
    let price = ctx.price;
    let rate30 = ctx.rate30;
    let ttax_80 = estimate_transfer_tax(ctx, price * 0.80);

    let scenario_defs = [
        ("conv20", "20% Down (Conventional)", 20.0, 0.0, 0.0, 0.0),
        ("conv10", "10% Down (Conventional)", 10.0, 0.08, 0.42, 0.0),
        ("conv5", "5% Down (Conventional)", 5.0, 0.14, 0.58, 0.0),
        ("fha35", "3.5% Down (FHA)", 3.5, -0.25, 0.55, 1.75),
    ];

    let appraisal_usd: i64 =
        if matches!(ctx.state.as_str(), "CA" | "NY" | "MA" | "WA" | "HI" | "DC") {
            925
        } else {
            775
        };
    let title_escrow_usd = (price * 0.0045).clamp(1_650.0, 9_500.0).round() as i64;
    let recording_usd: i64 = 275;
    let ins_12mo_usd = ctx.annual_ins.round() as i64;
    let ins_cushion_2mo_usd = (ctx.annual_ins * 2.0 / 12.0).round() as i64;
    let tax_12mo_annual_usd = ctx.annual_tax.round() as i64;
    let tax_impound_4mo_usd = (ctx.annual_tax * 4.0 / 12.0).round() as i64;

    let scenarios: Vec<Value> = scenario_defs
        .into_iter()
        .map(|(id, label, down_pct, rate_delta, pmi_ann_pct, upfront_mip_pct)| {
            let down_usd = (price * (down_pct / 100.0)).round();
            let base_loan = (price - down_usd).max(0.0);
            let upfront_mip = (base_loan * (upfront_mip_pct / 100.0)).round();
            let financed_loan = base_loan + upfront_mip;
            let eff_rate = (rate30 + rate_delta).max(1.0);

            let tt = estimate_transfer_tax(ctx, base_loan);
            let orig_usd = (1_495.0 + base_loan * 0.0025).round() as i64;
            let buyer_tt_and_mrt = (tt.buyer_share_usd + tt.buyer_mortgage_recording_tax_usd).round() as i64 + recording_usd;
            let prepaid_int_15d = (financed_loan * (eff_rate / 100.0) / 365.0 * 15.0).round() as i64;

            let total_closing = orig_usd
                + appraisal_usd
                + title_escrow_usd
                + buyer_tt_and_mrt
                + prepaid_int_15d
                + ins_12mo_usd
                + ins_cushion_2mo_usd
                + tax_impound_4mo_usd;
            let total_cash = down_usd.round() as i64 + total_closing;

            let pi_mo = monthly_pi(financed_loan, eff_rate, 30);
            let pmi_mo = (base_loan * (pmi_ann_pct / 100.0)) / 12.0;
            let piti_mo = pi_mo + pmi_mo + (ctx.annual_tax / 12.0) + (ctx.annual_ins / 12.0) + hoa_monthly;

            json!({
                "id": id,
                "label": label,
                "downPct": down_pct,
                "downPaymentUsd": down_usd.round() as i64,
                "baseLoanAmountUsd": base_loan.round() as i64,
                "upfrontMipFinancedUsd": upfront_mip as i64,
                "financedLoanAmountUsd": financed_loan.round() as i64,
                "ratePct": round_n(eff_rate, 2),
                "itemizedClosingCosts": {
                    "lenderOriginationUsd": orig_usd,
                    "appraisalAndCreditUsd": appraisal_usd,
                    "titleAndEscrowUsd": title_escrow_usd,
                    "buyerTransferAndRecordingTaxUsd": buyer_tt_and_mrt,
                    "buyerTransferTaxUsd": tt.buyer_share_usd.round() as i64,
                    "buyerMortgageRecordingTaxUsd": tt.buyer_mortgage_recording_tax_usd.round() as i64,
                    "recordingFeeUsd": recording_usd,
                    "prepaidInterest15dUsd": prepaid_int_15d,
                    "insurance12MoUpfrontUsd": ins_12mo_usd,
                    "insuranceImpoundCushion2MoUsd": ins_cushion_2mo_usd,
                    "propertyTaxImpound4MoUsd": tax_impound_4mo_usd,
                    "annualPropertyTax12MoReferenceUsd": tax_12mo_annual_usd,
                },
                "totalClosingCostsUsd": total_closing,
                "totalCashToCloseUsd": total_cash,
                "monthlyPIUsd": pi_mo.round() as i64,
                "monthlyPMIUsd": pmi_mo.round() as i64,
                "monthlyPITIUsd": piti_mo.round() as i64,
            })
        })
        .collect();

    // 2-1 Temporary Rate Buydown vs. Direct Price Reduction (20% down baseline)
    let loan_80 = price * 0.80;
    let base_pi = monthly_pi(loan_80, rate30, 30);
    let y1_rate = (rate30 - 2.0).max(0.50);
    let y2_rate = (rate30 - 1.0).max(1.00);
    let y1_pi = monthly_pi(loan_80, y1_rate, 30);
    let y2_pi = monthly_pi(loan_80, y2_rate, 30);
    let y1_save_mo = (base_pi - y1_pi).max(0.0);
    let y2_save_mo = (base_pi - y2_pi).max(0.0);
    let buydown_cost = (y1_save_mo * 12.0 + y2_save_mo * 12.0).round();

    let cut_price = (price - buydown_cost).max(10_000.0);
    let cut_loan = cut_price * 0.80;
    let cut_pi = monthly_pi(cut_loan, rate30, 30);
    let perm_pi_save_mo = (base_pi - cut_pi).max(0.0);
    let perm_tax_save_mo = (buydown_cost * ctx.tax_rate) / 12.0;
    let perm_total_mo = (perm_pi_save_mo + perm_tax_save_mo).max(1.0);
    let upfront_down_save = buydown_cost * 0.20;
    let remaining_subsidy_gap = (buydown_cost - upfront_down_save).max(0.0);
    let breakeven_months = (remaining_subsidy_gap / perm_total_mo).round().max(24.0) as i64;
    let breakeven_years = round_n(breakeven_months as f64 / 12.0, 1);

    let buydown_vs_price_cut = json!({
        "noteRatePct": round_n(rate30, 2),
        "year1RatePct": round_n(y1_rate, 2),
        "year2RatePct": round_n(y2_rate, 2),
        "baseMonthlyPIUsd": base_pi.round() as i64,
        "year1MonthlyPIUsd": y1_pi.round() as i64,
        "year2MonthlyPIUsd": y2_pi.round() as i64,
        "year1MonthlySavingsUsd": y1_save_mo.round() as i64,
        "year2MonthlySavingsUsd": y2_save_mo.round() as i64,
        "buydownEscrowCostUsd": buydown_cost as i64,
        "equivalentPriceReductionUsd": buydown_cost as i64,
        "reducedPurchasePriceUsd": cut_price.round() as i64,
        "upfrontDownPaymentSavingsUsd": upfront_down_save.round() as i64,
        "permanentMonthlyPISavingsUsd": perm_pi_save_mo.round() as i64,
        "permanentMonthlyTaxSavingsUsd": perm_tax_save_mo.round() as i64,
        "permanentTotalMonthlySavingsUsd": perm_total_mo.round() as i64,
        "breakEvenMonths": breakeven_months,
        "breakEvenYears": breakeven_years,
        "verdict": format!(
            "A seller-funded 2-1 buydown (${} escrow) slashes Years 1–2 payments by ${}/mo and ${}/mo; if you refinance or sell within ~{:.1} years, the 2-1 buydown yields higher cash relief (unused escrow reduces payoff on refinance). Beyond ~{:.1} years, an equivalent ${} price cut wins permanently via lower principal, lower interest, and lower property taxes.",
            buydown_cost as i64,
            y1_save_mo.round() as i64,
            y2_save_mo.round() as i64,
            breakeven_years,
            breakeven_years,
            buydown_cost as i64,
        ),
    });

    json!({
        "purchasePriceUsd": price.round() as i64,
        "priceReported": ctx.price_reported,
        "rate30Pct": round_n(rate30, 2),
        "rate30Reported": ctx.rate30_reported,
        "taxRateReported": ctx.tax_rate_reported,
        "valuationInputsVerified": ctx.price_reported && ctx.rate30_reported,
        "transferTax": {
            "jurisdiction": ttax_80.jurisdiction,
            "state": ctx.state,
            "city": ctx.city,
            "county": ctx.county,
            "totalTransferTaxUsd": ttax_80.total_tax_usd.round() as i64,
            "buyerEstimatedShareUsd": ttax_80.buyer_share_usd.round() as i64,
            "sellerEstimatedShareUsd": ttax_80.seller_share_usd.round() as i64,
            "buyerMortgageRecordingTaxUsd": ttax_80.buyer_mortgage_recording_tax_usd.round() as i64,
            "effectiveRatePct": ttax_80.effective_rate_pct,
            "isHighTransferTaxJurisdiction": ttax_80.is_high_transfer_tax,
            "payerCustomNote": ttax_80.payer_custom_note,
            "breakdown": ttax_80.breakdown,
        },
        "scenarios": scenarios,
        "buydownVsPriceCut": buydown_vs_price_cut,
        "disclaimer": BUYER_DILIGENCE_DISCLAIMER,
    })
}

// ------------------------- 2(b). Hidden Ongoing Costs: Special Districts & Utilities -------------------------

fn estimate_climate_cdd_hdd(lat_opt: Option<f64>, st: &str) -> Option<(i64, i64)> {
    let lat_finite = lat_opt.filter(|l| l.is_finite());
    match st {
        "FL" | "HI" => Some((3600, 350)),
        "AZ" | "TX" | "LA" => Some((2900, 1450)),
        "GA" | "SC" | "AL" | "MS" => Some((2100, 2400)),
        "CA" => match lat_finite {
            Some(lat) if lat < 34.8 => Some((1450, 1300)),
            Some(_) => Some((750, 2500)),
            None => Some((1050, 2000)),
        },
        "WA" | "OR" => Some((300, 4600)),
        "NY" | "NJ" | "PA" | "CT" | "MA" | "IL" | "MI" | "OH" | "WI" | "MN" | "CO" => {
            Some((800, 5600))
        }
        "NC" | "TN" | "OK" | "AR" | "NV" | "NM" => Some((1950, 2900)),
        "VA" | "MD" | "DC" | "DE" | "KY" | "MO" | "KS" | "IN" | "UT" => Some((1300, 4300)),
        "NH" | "VT" | "ME" | "RI" | "IA" | "NE" | "SD" | "ND" | "MT" | "WY" | "ID" | "AK" => {
            Some((600, 6500))
        }
        _ => lat_finite.map(|lat| {
            if lat < 34.0 {
                (2300, 1700)
            } else if lat > 41.0 {
                (700, 5800)
            } else {
                (1250, 3800)
            }
        }),
    }
}

fn is_wholesaler_water_system(name: &str, pwsid: Option<&str>) -> bool {
    if pwsid == Some("CA4310027") {
        return true;
    }
    let lc = name.to_ascii_lowercase();
    lc.contains("wholesaler")
        || (lc.contains("santa clara valley") && lc.contains("water district"))
}

fn state_solar_yield_kwh_per_kw_mo(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "AZ" | "NM" | "NV" => 145.0,
        "CA" | "HI" | "UT" | "CO" => 129.0,
        "TX" | "FL" | "LA" | "GA" | "SC" | "NC" => 118.0,
        "NY" | "NJ" | "MA" | "CT" | "PA" | "MD" | "VA" => 104.0,
        "WA" | "OR" | "MI" | "WI" | "MN" => 96.0,
        _ => 112.0,
    }
}

fn extract_solar_kw(ctx: &ReportContext) -> Option<f64> {
    static RE_KW_NUM: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*k\s*w").expect("solar kw regex"));

    for opt in [
        val_f64(ctx.condition.pointer("/solar/totalKw")),
        val_f64(ctx.facts.pointer("/solarPv/totalKw")),
        val_f64(ctx.facts.get("solarPvKw")),
    ] {
        if let Some(kw) = opt.filter(|&k| k > 0.2 && k < 100.0) {
            return Some(round_n(kw, 3));
        }
    }
    for arr_opt in [
        ctx.facts.get("buildingPermits").and_then(|v| v.as_array()),
        ctx.facts.get("permits").and_then(|v| v.as_array()),
        ctx.condition
            .pointer("/solar/permits")
            .and_then(|v| v.as_array()),
    ] {
        let Some(arr) = arr_opt else {
            continue;
        };
        for p in arr {
            if let Some(kw) = val_f64(p.get("solarKw")).filter(|&k| k > 0.2 && k < 100.0) {
                return Some(round_n(kw, 3));
            }
            let blob = format!(
                "{} {} {}",
                val_str(p.get("category")).unwrap_or(""),
                val_str(p.get("type")).unwrap_or(""),
                val_str(p.get("description")).unwrap_or("")
            );
            let lc = blob.to_ascii_lowercase();
            if lc.contains("solar") || lc.contains("photovoltaic") || lc.contains("pv ") {
                if let Some(cap) = RE_KW_NUM.captures(&blob) {
                    if let Ok(kw) = cap[1].parse::<f64>() {
                        if (0.2..100.0).contains(&kw) {
                            return Some(round_n(kw, 3));
                        }
                    }
                }
            }
        }
    }
    None
}

fn build_ongoing_costs(
    ctx: &ReportContext,
    hoa_monthly: f64,
    is_private_septic: bool,
    is_private_well: bool,
    has_hoa_water_trash: bool,
    live_util_opt: Option<&LiveUtilityBundle>,
) -> Value {
    let mut flags: Vec<Value> = Vec::new();
    let mut est_annual_special_usd = 0.0_f64;
    let mut verified_on_roll = false;

    let explicit_direct_charges = val_f64(ctx.facts.get("directChargesAnnual")).or_else(|| {
        ctx.facts
            .get("taxHistory")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .max_by_key(|t| val_i64(t.get("year")).unwrap_or(0))
                    .and_then(|t| val_f64(t.get("directCharges")))
            })
    });
    if let Some(dc) = explicit_direct_charges.filter(|&d| d > 0.0) {
        est_annual_special_usd += dc;
        verified_on_roll = true;
    }

    // CA Mello-Roos CFD check (never fabricates dollar levies when parcel direct charges are unrecorded)
    let confirmed_ca_mello = val_f64(ctx.facts.get("melloRoosAnnual")).filter(|&x| x > 0.0);
    let ca_mello_explicit =
        RE_MELLO_ROOS.is_match(&ctx.combined_text) || confirmed_ca_mello.is_some();
    let ca_high_rate = ctx.state == "CA" && ctx.tax_rate > 0.0130;
    let ca_post_1998_subdiv =
        ctx.state == "CA" && ctx.year_built.map(|y| y >= 1998).unwrap_or(false);
    let mello_roos = if ctx.state == "CA"
        && (ca_mello_explicit || ca_high_rate || ca_post_1998_subdiv)
    {
        if let Some(amt) = confirmed_ca_mello {
            if explicit_direct_charges.is_none() {
                est_annual_special_usd += amt;
            }
            verified_on_roll = true;
        }
        let est_yr_opt: Option<i64> = confirmed_ca_mello.map(|v| v.round() as i64);
        let status = if confirmed_ca_mello.is_some() {
            "Confirmed Mello-Roos CFD Levy on Parcel Record"
        } else if ca_mello_explicit {
            "Explicit Mello-Roos CFD Signal (Verify Exact Levy on County Tax Bill)"
        } else {
            "Heuristic Risk Flag (Post-1998 CA Subdivision or >1.30% Tax Rate — Verify Tax Bill)"
        };
        flags.push(json!({
            "type": "CA_MELLO_ROOS_CFD",
            "label": "California Mello-Roos Community Facilities District (CFD)",
            "status": status,
            "estimatedAnnualUsd": est_yr_opt,
            "note": "Mello-Roos CFD parcel levies sit outside the 1% Prop 13 cap and often escalate +2%/yr for 20–40 years. Confirm exact direct-charge lines on the County Tax Collector bill & NHD Tax Addendum.",
        }));
        json!({
            "flagged": true,
            "status": status,
            "estimatedAnnualUsd": est_yr_opt,
        })
    } else {
        json!({ "flagged": false, "status": "Not flagged", "estimatedAnnualUsd": 0 })
    };

    // TX MUD / PID check (never fabricates % or dollar levies when `mudTaxAnnual` is unrecorded)
    let confirmed_tx_mud = val_f64(ctx.facts.get("mudTaxAnnual")).filter(|&x| x > 0.0);
    let tx_mud_explicit = RE_MUD_TX.is_match(&ctx.combined_text) || confirmed_tx_mud.is_some();
    let tx_high_rate = ctx.state == "TX" && ctx.tax_rate >= 0.0235;
    let tx_suburban_post_2000 =
        ctx.state == "TX" && ctx.year_built.map(|y| y >= 2000).unwrap_or(false);
    let mud_tx = if ctx.state == "TX" && (tx_mud_explicit || tx_high_rate || tx_suburban_post_2000)
    {
        if let Some(amt) = confirmed_tx_mud {
            if explicit_direct_charges.is_none() {
                est_annual_special_usd += amt;
            }
            verified_on_roll = true;
        }
        let est_yr_opt: Option<i64> = confirmed_tx_mud.map(|v| v.round() as i64);
        let status = if confirmed_tx_mud.is_some() {
            "Confirmed TX MUD / PID Levy on Parcel Record"
        } else if tx_mud_explicit || tx_high_rate {
            "Explicit TX MUD / PID Signal (Verify Statutory Notice to Purchaser)"
        } else {
            "Heuristic Risk Flag (Post-2000 TX Subdivision — Verify MUD/PID)"
        };
        flags.push(json!({
            "type": "TX_MUD_PID",
            "label": "Texas Municipal Utility District (MUD) / PID",
            "status": status,
            "estimatedAnnualUsd": est_yr_opt,
            "note": "TX MUD/PID utility bonds commonly add $0.40–$1.20 per $100 assessed value on top of county/ISD taxes. Request statutory Notice to Purchaser (Tex. Water Code § 49.452).",
        }));
        json!({
            "flagged": true,
            "status": status,
            "estimatedAnnualUsd": est_yr_opt,
        })
    } else {
        json!({ "flagged": false, "status": "Not flagged", "estimatedAnnualUsd": 0 })
    };

    // FL CDD check (never fabricates dollar levies when `cddAnnual` is unrecorded)
    let confirmed_fl_cdd = val_f64(ctx.facts.get("cddAnnual")).filter(|&x| x > 0.0);
    let fl_cdd_explicit = RE_CDD_FL.is_match(&ctx.combined_text) || confirmed_fl_cdd.is_some();
    let fl_post_2000_hoa = ctx.state == "FL"
        && ctx.year_built.map(|y| y >= 2000).unwrap_or(false)
        && (hoa_monthly > 0.0 || ctx.combined_text.to_ascii_lowercase().contains("master"));
    let cdd_fl = if ctx.state == "FL" && (fl_cdd_explicit || fl_post_2000_hoa) {
        if let Some(amt) = confirmed_fl_cdd {
            if explicit_direct_charges.is_none() {
                est_annual_special_usd += amt;
            }
            verified_on_roll = true;
        }
        let est_yr_opt: Option<i64> = confirmed_fl_cdd.map(|v| v.round() as i64);
        let status = if confirmed_fl_cdd.is_some() {
            "Confirmed FL CDD Assessment on Parcel Record"
        } else if fl_cdd_explicit {
            "Explicit FL CDD Signal (Verify Non-Ad-Valorem Tax Line)"
        } else {
            "Heuristic Risk Flag (Post-2000 FL Master-Planned Community)"
        };
        flags.push(json!({
            "type": "FL_CDD",
            "label": "Florida Community Development District (CDD)",
            "status": status,
            "estimatedAnnualUsd": est_yr_opt,
            "note": "FL CDD infrastructure bond + O&M assessments are billed separately on the non-ad-valorem county tax bill IN ADDITION to HOA dues. Verify CDD bond payoff vs. annual O&M.",
        }));
        json!({
            "flagged": true,
            "status": status,
            "estimatedAnnualUsd": est_yr_opt,
        })
    } else {
        json!({ "flagged": false, "status": "Not flagged", "estimatedAnnualUsd": 0 })
    };

    let special_mo = (est_annual_special_usd / 12.0).round() as i64;
    let special_yr = est_annual_special_usd.round() as i64;

    // Modeled Monthly Utilities Load + Carrier & Published Tariff Rate Quotes (never invents 1,800 sq ft when living area is missing)
    let sqft_opt = ctx.sqft.filter(|&s| s > 0.0);
    let age_factor = match ctx.year_built {
        Some(y) if y < 1970 => 1.20,
        Some(y) if y < 1995 => 1.10,
        Some(y) if y >= 2012 => 0.88,
        _ => 1.0,
    };
    let gov_kwh_bench = val_f64(ctx.market.pointer("/govEnergyBenchmark/electricityPerKwh"))
        .or_else(|| (!ctx.state.is_empty()).then(|| regional_kwh_rate(&ctx.state)));
    let water_prim = ctx
        .environment
        .get("water")
        .filter(|w| w.is_object())
        .and_then(|w| w.get("primary"))
        .filter(|p| {
            let nm = val_str(p.get("name")).unwrap_or("");
            let pid = val_str(p.get("pwsid"));
            !is_wholesaler_water_system(nm, pid)
        });
    let sdwis_water_name = water_prim.and_then(|p| val_str(p.get("name")));
    let sdwis_pwsid = water_prim.and_then(|p| val_str(p.get("pwsid")));
    let ptype_lc = ctx.property_type.to_ascii_lowercase();
    let is_condo_attached = ptype_lc.contains("condo")
        || ptype_lc.contains("town")
        || ptype_lc.contains("co-op")
        || ptype_lc.contains("attached");
    let solar_kw_opt = extract_solar_kw(ctx);
    let has_solar_signal = solar_kw_opt.is_some()
        || ctx
            .condition
            .pointer("/solar/hasSolar")
            .and_then(|v| v.as_bool())
            == Some(true)
        || ctx
            .facts
            .pointer("/solarPv/hasSolar")
            .and_then(|v| v.as_bool())
            == Some(true)
        || {
            let low = ctx.combined_text.to_ascii_lowercase();
            low.contains("solar") || low.contains("photovoltaic")
        };

    let u_inp = UtilityLookupInput {
        state: &ctx.state,
        county_fips: &ctx.county_fips,
        county_name: &ctx.county,
        city: &ctx.city,
        zip: &ctx.zip,
        sqft: sqft_opt.map(|s| s.round() as i64).unwrap_or(0),
        year_built: ctx.year_built,
        is_condo_or_attached: is_condo_attached,
        hoa_covers_water_trash: has_hoa_water_trash,
        is_likely_private_well: is_private_well,
        is_likely_private_septic: is_private_septic,
        sdwis_water_name,
        sdwis_pwsid,
        gov_kwh_benchmark: gov_kwh_bench,
        has_solar: has_solar_signal,
        acs_broadband_pct: None,
        is_rural_area: ctx.lot_acres.map(|a| a >= 1.5).unwrap_or(false),
    };

    let mut elec_quote = resolve_electric_utility(&u_inp);
    let mut elec_verified = false;
    let mut elec_provenance = if elec_quote.rate_source.contains("EIA") {
        "eia_state_fallback"
    } else {
        "static_extension_fallback"
    }
    .to_string();
    if let Some(le) = live_util_opt.and_then(|b| b.electric.as_ref()) {
        elec_quote.carrier_name = le.carrier_name.clone();
        elec_quote.short_name = le.short_name.clone();
        elec_quote.utility_type = le.utility_type.clone();
        elec_quote.utility_type_label = le.utility_type_label.clone();
        elec_quote.cca_program = le.cca_program.clone();
        elec_quote.rate_schedule = le.rate_schedule.clone();
        elec_quote.rate_per_kwh_usd = le.rate_per_kwh_usd;
        elec_quote.off_peak_kwh_usd = le.off_peak_kwh_usd;
        elec_quote.peak_kwh_usd = le.peak_kwh_usd;
        elec_quote.monthly_base_fee_usd = le.monthly_base_fee_usd;
        elec_quote.solar_nem_policy = le.solar_nem_policy.clone();
        elec_quote.tariff_url = le.tariff_url.clone();
        elec_quote.rate_source = le.rate_source.clone();
        elec_verified = le.is_live_verified;
        elec_provenance = le.provenance_label.clone();
    }

    let mut gas_quote = resolve_gas_utility(&u_inp);
    let mut gas_verified = false;
    let mut gas_provenance = "static_extension_fallback".to_string();
    if let Some(lg) = live_util_opt.and_then(|b| b.gas.as_ref()) {
        gas_quote.carrier_name = lg.carrier_name.clone();
        gas_quote.heating_fuel_norm = lg.heating_fuel_norm.clone();
        gas_quote.rate_schedule = lg.rate_schedule.clone();
        gas_quote.rate_per_therm_usd = lg.rate_per_therm_usd;
        gas_quote.monthly_base_fee_usd = lg.monthly_base_fee_usd;
        gas_quote.tariff_url = lg.tariff_url.clone();
        gas_quote.rate_source = lg.rate_source.clone();
        gas_verified = lg.is_live_verified;
        gas_provenance = lg.provenance_label.clone();
    }

    let mut wst_quote = resolve_water_sewer_trash(&u_inp);
    let mut water_verified = false;
    let mut water_tier_rates: Option<[f64; 3]> = None;
    let mut water_rate_source: Option<String> = None;
    let mut sewer_verified = is_private_septic;
    let mut sewer_monthly_opt: Option<i64> = if is_private_septic { Some(30) } else { None };
    let mut sewer_tariff_url: Option<String> = None;
    let mut sewer_rate_source: Option<String> = None;
    let mut trash_verified = has_hoa_water_trash;
    let mut trash_monthly_opt: Option<i64> = if has_hoa_water_trash { Some(0) } else { None };
    let mut trash_tariff_url: Option<String> = None;
    let mut trash_rate_source: Option<String> = None;

    if let Some(lw) = live_util_opt.and_then(|b| b.wst.as_ref()) {
        if let Some(ref wc) = lw.water_carrier_name {
            wst_quote.water_carrier_name = wc.clone();
        }
        if lw.water_pwsid.is_some() {
            wst_quote.water_pwsid = lw.water_pwsid.clone();
        }
        if let Some(ref ws) = lw.water_source_type {
            wst_quote.water_source_type = ws.clone();
        }
        if let Some(ref wru) = lw.water_rate_unit {
            wst_quote.water_rate_unit = wru.clone();
        }
        if lw.water_verified {
            wst_quote.water_volumetric_rate_usd = lw.water_volumetric_rate_usd;
            if let Some(base) = lw.water_base_monthly_usd {
                wst_quote.water_base_monthly_usd = base;
            }
            wst_quote.water_tariff_url = lw.water_tariff_url.clone();
            water_tier_rates = lw.water_tier_rates_ccf;
            water_rate_source = lw.water_rate_source.clone();
            water_verified = true;
        }
        if let Some(ref sc) = lw.sewer_carrier_name {
            wst_quote.sewer_carrier_name = sc.clone();
        }
        if let Some(ref st) = lw.sewer_type {
            wst_quote.sewer_type = st.clone();
        }
        if lw.sewer_verified {
            if let Some(sm) = lw.sewer_monthly_usd {
                wst_quote.sewer_monthly_usd = sm;
                sewer_monthly_opt = Some(sm);
            }
            sewer_tariff_url = lw.sewer_tariff_url.clone();
            sewer_rate_source = lw.sewer_rate_source.clone();
            sewer_verified = true;
        }
        if let Some(ref tc) = lw.trash_carrier_name {
            wst_quote.trash_carrier_name = tc.clone();
        }
        if lw.trash_verified {
            if let Some(tm) = lw.trash_monthly_usd {
                wst_quote.trash_monthly_usd = tm;
                trash_monthly_opt = Some(tm);
            }
            trash_tariff_url = lw.trash_tariff_url.clone();
            trash_rate_source = lw.trash_rate_source.clone();
            trash_verified = true;
        }
    }

    // Never leave dead/404 San Jose Water URLs or San Jose Recycle Plus attached to non-SJ cities
    let city_slug_cur = normalize_city_slug(&ctx.city);
    if wst_quote
        .water_tariff_url
        .as_deref()
        .map(|u| u.contains("/customer-care/help-information/rates-tariffs"))
        .unwrap_or(false)
    {
        wst_quote.water_tariff_url = Some("https://www.sjwater.com/2026-rates/".to_string());
    }
    if !has_hoa_water_trash
        && !trash_verified
        && city_slug_cur != "san jose"
        && wst_quote
            .trash_carrier_name
            .to_ascii_lowercase()
            .contains("recycle plus")
    {
        wst_quote.trash_carrier_name = if city_slug_cur == "campbell" {
            "West Valley Collection & Recycling (WVC&R)".to_string()
        } else {
            "Local Municipal / Franchise Hauler (Verify with City Public Works)".to_string()
        };
        trash_monthly_opt = None;
    }
    if !sewer_verified
        && wst_quote.sewer_type != "septic"
        && wst_quote
            .sewer_carrier_name
            .contains("City / County Sanitary Sewer Service")
    {
        sewer_monthly_opt = None;
    }

    let kwh_rate = elec_quote.rate_per_kwh_usd;
    let live_climate_opt = live_util_opt.and_then(|b| b.climate.as_ref());
    let (climate_dd, climate_verified, climate_source) = match live_climate_opt {
        Some(c) => (Some((c.cdd_65, c.hdd_65)), true, c.source_name.clone()),
        None => (
            estimate_climate_cdd_hdd(ctx.lat, &ctx.state),
            false,
            "modeled_regional_fallback".to_string(),
        ),
    };
    let cdd_opt = climate_dd.map(|(c, _)| c);
    let hdd_opt = climate_dd.map(|(_, h)| h);
    let has_pool = RE_POOL.is_match(&ctx.combined_text);
    let pool_and_lot_mo: i64 = (if has_pool { 175 } else { 0 })
        + if ctx.lot_sqft.map(|l| l >= 12_000.0).unwrap_or(false) {
            85
        } else {
            0
        };

    let nem_self_credit = if ctx.state == "CA" && ctx.year_built.unwrap_or(2020) >= 2023 {
        0.65
    } else {
        0.85
    };
    let solar_off_raw = solar_kw_opt
        .map(|kw| kw * state_solar_yield_kwh_per_kw_mo(&ctx.state) * nem_self_credit)
        .unwrap_or(0.0);
    let standalone_solar_off_i = (solar_off_raw > 1.0).then(|| solar_off_raw.round() as i64);

    let (
        gross_monthly_kwh,
        solar_offset_kwh,
        est_monthly_kwh,
        elec_mo,
        gas_heat_mo,
        est_monthly_therms,
        water_est_mo_opt,
        water_sewer_trash_mo,
        total_utilities_mo,
    ): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = if let (Some(sqft), Some((cdd, hdd))) = (sqft_opt, climate_dd) {
        if kwh_rate > 0.0 {
            let gross_kwh_mo = (420.0 + (sqft * 0.22) + (cdd as f64 * 0.14)) * age_factor;
            let gross_kwh_i = gross_kwh_mo.round() as i64;
            let solar_off_f = solar_off_raw.min(gross_kwh_mo * 0.85);
            let solar_off_i = (solar_off_f > 1.0).then(|| solar_off_f.round() as i64);
            let net_kwh_mo = (gross_kwh_mo - solar_off_f).max(0.0);
            let kwh_i = net_kwh_mo.round() as i64;
            let elec_i = (net_kwh_mo * kwh_rate + elec_quote.monthly_base_fee_usd).round() as i64;

            // Physical therms modeled FIRST from climate HDD + sqft + vintage, THEN billed by live gas tariff
            let (gas_mo_opt, therms_opt) = if gas_quote.heating_fuel_norm == "electric_heat_pump" {
                (Some(0), Some(0))
            } else {
                let therms_mo = (12.0 + (sqft * 0.0085) + (hdd as f64 * 0.0062)) * age_factor;
                let th_i = therms_mo.round() as i64;
                let g_bill = gas_quote
                    .rate_per_therm_usd
                    .map(|tr| (therms_mo * tr + gas_quote.monthly_base_fee_usd).round() as i64);
                (g_bill, Some(th_i))
            };

            let water_est_opt: Option<i64> = if has_hoa_water_trash {
                Some(0)
            } else if is_private_well {
                Some(18)
            } else if wst_quote.water_volumetric_rate_usd.is_some() || water_tier_rates.is_some() {
                let units_mo = (3.5 + (sqft / 750.0)).clamp(4.0, 14.0);
                let bill = compute_water_bill_usd(
                    units_mo,
                    wst_quote.water_base_monthly_usd,
                    wst_quote.water_volumetric_rate_usd,
                    water_tier_rates,
                );
                Some(bill.round() as i64)
            } else {
                None
            };

            let s_known = if has_hoa_water_trash {
                Some(0)
            } else if is_private_septic {
                Some(30)
            } else {
                sewer_monthly_opt.or_else(|| {
                    (sewer_verified && wst_quote.sewer_monthly_usd > 0)
                        .then_some(wst_quote.sewer_monthly_usd)
                })
            };
            let t_known = if has_hoa_water_trash {
                Some(0)
            } else {
                trash_monthly_opt.or_else(|| {
                    (trash_verified && wst_quote.trash_monthly_usd > 0)
                        .then_some(wst_quote.trash_monthly_usd)
                })
            };

            let wst_i_opt: Option<i64> = if has_hoa_water_trash {
                Some(0)
            } else {
                let known_parts: i64 =
                    water_est_opt.unwrap_or(0) + s_known.unwrap_or(0) + t_known.unwrap_or(0);
                (water_est_opt.is_some() || s_known.is_some() || t_known.is_some())
                    .then_some(known_parts)
            };
            let total_i =
                elec_i + gas_mo_opt.unwrap_or(0) + wst_i_opt.unwrap_or(0) + pool_and_lot_mo;
            (
                Some(gross_kwh_i),
                solar_off_i,
                Some(kwh_i),
                Some(elec_i),
                gas_mo_opt,
                therms_opt,
                water_est_opt,
                wst_i_opt,
                Some(total_i),
            )
        } else {
            (
                None,
                standalone_solar_off_i,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        }
    } else {
        (
            None,
            standalone_solar_off_i,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    };

    let gas_rate_str = match gas_quote.rate_per_therm_usd {
        Some(tr) => format!(
            "${tr:.2}/therm (+ ${:.2}/mo base)",
            gas_quote.monthly_base_fee_usd
        ),
        None if gas_quote.heating_fuel_norm == "unverified" => gas_quote.rate_schedule.clone(),
        None => "All-Electric HVAC / Heat Pump (no separate gas meter typical)".to_string(),
    };
    let water_rate_str = if let Some([t1, t2, t3]) = water_tier_rates {
        format!(
            "Tier 1 ${t1:.2} · Tier 2 ${t2:.2} · Tier 3 ${t3:.2}/CCF (+ ${:.2}/mo meter)",
            wst_quote.water_base_monthly_usd
        )
    } else {
        match wst_quote.water_volumetric_rate_usd {
            Some(vr) => format!(
                "${vr:.2} per {} (+ ${:.2}/mo meter base)",
                wst_quote.water_rate_unit, wst_quote.water_base_monthly_usd
            ),
            None => wst_quote.water_rate_unit.clone(),
        }
    };
    let water_sewer_only_mo: Option<i64> = match (water_est_mo_opt, sewer_monthly_opt) {
        (Some(w), Some(s)) => Some(w + s),
        (Some(w), None) => Some(w),
        (None, Some(s)) => Some(s),
        _ => water_sewer_trash_mo.map(|wst| (wst - trash_monthly_opt.unwrap_or(0)).max(0)),
    };
    let utility_carriers_summary = vec![
        json!({
            "category": "Electricity",
            "carrierName": elec_quote.carrier_name,
            "shortName": elec_quote.short_name,
            "utilityType": elec_quote.utility_type,
            "utilityTypeLabel": elec_quote.utility_type_label,
            "ccaProgram": elec_quote.cca_program,
            "rateQuote": if kwh_rate > 0.0 {
                format!(
                    "${:.3}/kWh avg (Off-Peak ${:.3} · Peak ${:.3}/kWh)",
                    elec_quote.rate_per_kwh_usd, elec_quote.off_peak_kwh_usd, elec_quote.peak_kwh_usd
                )
            } else {
                elec_quote.rate_schedule.clone()
            },
            "schedule": elec_quote.rate_schedule,
            "monthlyEstUsd": elec_mo,
            "tariffUrl": elec_quote.tariff_url,
            "rateVerified": elec_verified,
            "provenance": elec_provenance,
            "notes": match (solar_kw_opt, solar_offset_kwh) {
                (Some(kw), Some(off)) => format!("Permitted {kw:.2} kW solar PV offsets ~{off} kWh/mo · {}", elec_quote.solar_nem_policy),
                _ => elec_quote.solar_nem_policy.clone(),
            },
        }),
        json!({
            "category": "Natural Gas / Heating",
            "carrierName": gas_quote.carrier_name,
            "shortName": gas_quote.carrier_name,
            "utilityType": gas_quote.heating_fuel_norm,
            "utilityTypeLabel": if gas_quote.heating_fuel_norm == "electric_heat_pump" { "All-Electric Standard" } else { "Natural Gas Utility" },
            "ccaProgram": Value::Null,
            "rateQuote": gas_rate_str,
            "schedule": gas_quote.rate_schedule,
            "monthlyEstUsd": gas_heat_mo,
            "tariffUrl": gas_quote.tariff_url,
            "rateVerified": gas_verified,
            "provenance": gas_provenance,
            "notes": match est_monthly_therms {
                Some(th) => format!("~{th} therms/mo seasonal avg"),
                None if gas_quote.rate_per_therm_usd.is_some() => "Requires living area (sq ft) for usage estimate".to_string(),
                None => "Included in electric heat-pump load".to_string(),
            },
        }),
        json!({
            "category": "Water & Sanitary Sewer",
            "carrierName": format!("{} · {}", wst_quote.water_carrier_name, wst_quote.sewer_carrier_name),
            "shortName": wst_quote.water_carrier_name,
            "utilityType": wst_quote.water_source_type,
            "utilityTypeLabel": if wst_quote.water_source_type == "private_well" { "Private Well" } else { "Municipal / Public Water" },
            "ccaProgram": Value::Null,
            "rateQuote": if water_verified { water_rate_str } else { "Rate unverified — check utility tariff page".to_string() },
            "schedule": if wst_quote.sewer_type == "septic" {
                "Private On-Site Septic System".to_string()
            } else if let Some(sm) = sewer_monthly_opt {
                format!("{} (~${sm}/mo)", wst_quote.sewer_carrier_name)
            } else {
                format!("{} (Rate unverified)", wst_quote.sewer_carrier_name)
            },
            "monthlyEstUsd": water_sewer_only_mo,
            "tariffUrl": wst_quote.water_tariff_url,
            "rateVerified": water_verified || sewer_verified,
            "notes": if wst_quote.sewer_type == "septic" {
                "Private septic tank & leach field — order pump/load inspection".to_string()
            } else {
                format!(
                    "{}{} · EPA PWSID {}",
                    sewer_monthly_opt.map(|sm| format!("Sewer ~${sm}/mo")).unwrap_or_else(|| "Sewer rate unverified".to_string()),
                    sewer_rate_source.as_deref().map(|s| format!(" ({s})")).unwrap_or_default(),
                    wst_quote.water_pwsid.as_deref().unwrap_or("Local System")
                )
            },
        }),
        json!({
            "category": "Trash, Compost & Recycling",
            "carrierName": wst_quote.trash_carrier_name,
            "shortName": wst_quote.trash_carrier_name,
            "utilityType": if has_hoa_water_trash { "hoa_included" } else if trash_verified { "municipal_franchise" } else { "unverified" },
            "utilityTypeLabel": if has_hoa_water_trash { "Included in HOA Dues" } else if trash_verified { "Curbside Cart Service" } else { "Unverified Municipal Hauler" },
            "ccaProgram": Value::Null,
            "rateQuote": if has_hoa_water_trash {
                "$0 direct (covered by HOA)".to_string()
            } else if let Some(tm) = trash_monthly_opt {
                if tm == 0 { "Funded via Municipal Tax".to_string() } else { format!("~${tm}/mo standard 35-gal cart") }
            } else {
                "Rate unverified (confirm with city hauler)".to_string()
            },
            "schedule": if trash_verified { "Standard Residential Curbside Service" } else { "Verify Municipal Cart Tier" },
            "monthlyEstUsd": trash_monthly_opt,
            "tariffUrl": trash_tariff_url,
            "rateVerified": trash_verified,
            "notes": trash_rate_source.unwrap_or_else(|| "Solid waste, single-stream recycling & organics".to_string()),
        }),
    ];

    // PITI baseline (from valuation 20% down ownership or computed directly)
    let piti_mo = val_f64(
        ctx.valuation
            .pointer("/ownership/monthly/principalInterest"),
    )
    .and_then(|pi| {
        let tx = val_f64(ctx.valuation.pointer("/ownership/monthly/propertyTax"))
            .unwrap_or(ctx.annual_tax / 12.0);
        let ins = val_f64(ctx.valuation.pointer("/ownership/monthly/insurance"))
            .unwrap_or(ctx.annual_ins / 12.0);
        Some((pi + tx + ins).round() as i64)
    })
    .unwrap_or_else(|| {
        let pi = monthly_pi(ctx.price * 0.80, ctx.rate30, 30);
        (pi + ctx.annual_tax / 12.0 + ctx.annual_ins / 12.0).round() as i64
    });
    let hoa_mo_i = hoa_monthly.round() as i64;
    let maint_mo_i = val_i64(ctx.valuation.pointer("/ownership/monthly/maintenance"))
        .unwrap_or_else(|| ((ctx.price * 0.0075) / 12.0).round() as i64);
    let known_utilities_mo = total_utilities_mo.unwrap_or(0);
    let all_in_burn = piti_mo + hoa_mo_i + special_mo + known_utilities_mo;
    let all_in_with_maint = all_in_burn + maint_mo_i;
    let delta_over_piti = all_in_burn - piti_mo;
    let delta_pct = if piti_mo > 0 {
        round_n((delta_over_piti as f64 / piti_mo as f64) * 100.0, 1)
    } else {
        0.0
    };
    let assumptions_note = match (sqft_opt, est_monthly_kwh, solar_kw_opt, solar_offset_kwh) {
        (Some(sqft), Some(kwh_i), Some(kw), Some(off)) => format!(
            "Modeled for ~{} sq ft home (vintage factor {:.2}x, permitted {:.2} kW solar offsetting ~{} kWh/mo) on {} ({}) at ${:.3}/kWh (~{} net grid kWh/mo).",
            sqft.round() as i64,
            age_factor,
            kw,
            off,
            elec_quote.short_name,
            elec_quote.rate_schedule,
            kwh_rate,
            kwh_i
        ),
        (Some(sqft), Some(kwh_i), _, _) => format!(
            "Modeled for ~{} sq ft home (vintage factor {:.2}x) on {} ({}) at ${:.3}/kWh (~{} kWh/mo).",
            sqft.round() as i64,
            age_factor,
            elec_quote.short_name,
            elec_quote.rate_schedule,
            kwh_rate,
            kwh_i
        ),
        (_, None, Some(kw), Some(off)) => format!(
            "Living area (sq ft) was not reported in parcel records; showing {} ({}) unit tariffs without fabricated monthly usage totals (permitted {:.2} kW solar PV offsets ~{} kWh/mo).",
            elec_quote.short_name,
            elec_quote.rate_schedule,
            kw,
            off
        ),
        _ => format!(
            "Living area (sq ft) was not reported in parcel records; showing {} ({}) unit tariffs without fabricated monthly usage totals.",
            elec_quote.short_name,
            elec_quote.rate_schedule
        ),
    };

    json!({
        "specialTaxDistricts": {
            "riskLevel": if !flags.is_empty() || verified_on_roll { "Watch / Flagged" } else { "Low / None Detected" },
            "hasConfirmedOrSuspectedDistrict": !flags.is_empty() || verified_on_roll,
            "verifiedOnTaxRoll": verified_on_roll,
            "melloRoosCfd": mello_roos,
            "mudDistrictTx": mud_tx,
            "cddDistrictFl": cdd_fl,
            "estimatedAnnualSpecialTaxUsd": special_yr,
            "estimatedMonthlySpecialTaxUsd": special_mo,
            "flags": flags,
            "nonGuaranteeNotice": "Heuristic & supplied-record special district screen ONLY (not a parcel tax guarantee). Confirm exact direct-charge parcel levies, Mello-Roos CFD bonds, TX MUD/PID rates, and FL CDD bills directly with the County Tax Collector.",
        },
        "monthlyUtilities": {
            "electricityUsd": elec_mo,
            "heatingGasUsd": gas_heat_mo,
            "waterSewerTrashUsd": water_sewer_trash_mo,
            "poolAndLotUpkeepUsd": pool_and_lot_mo,
            "hasPoolSignal": has_pool,
            "hasSolarSignal": has_solar_signal,
            "solarKwDetected": solar_kw_opt,
            "grossMonthlyKwh": gross_monthly_kwh,
            "estimatedSolarOffsetKwh": solar_offset_kwh,
            "totalMonthlyUtilitiesUsd": total_utilities_mo,
            "electricityRateKwh": (kwh_rate > 0.0).then(|| round_n(kwh_rate, 3)),
            "estimatedMonthlyKwh": est_monthly_kwh,
            "estimatedMonthlyTherms": est_monthly_therms,
            "estimatedCdd": cdd_opt,
            "estimatedHdd": hdd_opt,
            "climateVerified": climate_verified,
            "climateSource": climate_source,
            "assumptions": assumptions_note,
            "electricCarrier": {
                "carrierName": elec_quote.carrier_name,
                "shortName": elec_quote.short_name,
                "utilityType": elec_quote.utility_type,
                "utilityTypeLabel": elec_quote.utility_type_label,
                "ccaProgram": elec_quote.cca_program,
                "rateSchedule": elec_quote.rate_schedule,
                "ratePerKwhUsd": elec_quote.rate_per_kwh_usd,
                "offPeakKwhUsd": elec_quote.off_peak_kwh_usd,
                "peakKwhUsd": elec_quote.peak_kwh_usd,
                "monthlyBaseFeeUsd": elec_quote.monthly_base_fee_usd,
                "solarNemPolicy": elec_quote.solar_nem_policy,
                "tariffUrl": elec_quote.tariff_url,
                "rateSource": elec_quote.rate_source,
                "rateVerified": elec_verified,
                "provenance": elec_provenance,
                "estimatedMonthlyKwh": est_monthly_kwh,
                "estimatedMonthlyUsd": elec_mo,
            },
            "gasCarrier": {
                "carrierName": gas_quote.carrier_name,
                "heatingFuelNorm": gas_quote.heating_fuel_norm,
                "rateSchedule": gas_quote.rate_schedule,
                "ratePerThermUsd": gas_quote.rate_per_therm_usd,
                "monthlyBaseFeeUsd": gas_quote.monthly_base_fee_usd,
                "tariffUrl": gas_quote.tariff_url,
                "rateSource": gas_quote.rate_source,
                "rateVerified": gas_verified,
                "provenance": gas_provenance,
                "estimatedMonthlyTherms": est_monthly_therms,
                "estimatedMonthlyUsd": gas_heat_mo,
            },
            "waterSewerTrash": {
                "waterCarrierName": wst_quote.water_carrier_name,
                "waterPwsid": wst_quote.water_pwsid,
                "waterSourceType": wst_quote.water_source_type,
                "waterRateUnit": wst_quote.water_rate_unit,
                "waterVolumetricRateUsd": wst_quote.water_volumetric_rate_usd,
                "waterTierRatesCcf": water_tier_rates,
                "waterBaseMonthlyUsd": wst_quote.water_base_monthly_usd,
                "waterMonthlyUsd": water_est_mo_opt,
                "waterTariffUrl": wst_quote.water_tariff_url,
                "waterRateSource": water_rate_source,
                "waterVerified": water_verified,
                "sewerCarrierName": wst_quote.sewer_carrier_name,
                "sewerType": wst_quote.sewer_type,
                "sewerMonthlyUsd": sewer_monthly_opt,
                "sewerTariffUrl": sewer_tariff_url,
                "sewerRateSource": sewer_rate_source,
                "sewerVerified": sewer_verified,
                "trashCarrierName": wst_quote.trash_carrier_name,
                "trashMonthlyUsd": trash_monthly_opt,
                "trashTariffUrl": trash_tariff_url,
                "trashVerified": trash_verified,
                "combinedMonthlyUsd": water_sewer_trash_mo,
            },
            "utilityCarriersSummary": utility_carriers_summary,
        },
        "allInMonthlyBurn": {
            "pitiUsd": piti_mo,
            "hoaMonthlyUsd": hoa_mo_i,
            "specialDistrictsMonthlyUsd": special_mo,
            "utilitiesMonthlyUsd": total_utilities_mo,
            "maintenanceReserveMonthlyUsd": maint_mo_i,
            "allInMonthlyBurnUsd": all_in_burn,
            "allInWithMaintenanceUsd": all_in_with_maint,
            "deltaOverPitiUsd": delta_over_piti,
            "deltaOverPitiPct": delta_pct,
        },
        "disclaimer": BUYER_DILIGENCE_DISCLAIMER,
    })
}

// ------------------------- 2(c). Broadband / Fiber & Remote-Work Connectivity -------------------------

fn build_connectivity(ctx: &ReportContext, live_bb_opt: Option<&LiveBroadbandCheck>) -> Value {
    let tract_acs = ctx.market.pointer("/acs/tract").or_else(|| {
        ctx.market
            .pointer("/acs/geos")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|g| g.get("level").and_then(|l| l.as_str()) == Some("tract"))
            })
            .and_then(|g| g.get("metrics"))
    });
    let tract_bb_pct = tract_acs.and_then(|t| val_f64(t.get("broadbandPct")));
    let zip_bb_pct = val_f64(ctx.market.pointer("/acs/zip/broadbandPct"));
    let wfh_pct = tract_acs.and_then(|t| val_f64(t.get("workFromHomePct")));
    let commute_min = tract_acs.and_then(|t| val_f64(t.get("meanCommuteMin")));

    let fcc_lookup_url = match (ctx.lat, ctx.lon) {
        (Some(la), Some(lo)) => {
            format!("https://broadbandmap.fcc.gov/location-summary/fixed?latlon={la:.6}%2C{lo:.6}")
        }
        _ => "https://broadbandmap.fcc.gov/".to_string(),
    };

    let walk_idx = val_f64(ctx.civic.pointer("/walkability/index"));
    let dense_amenities =
        val_i64(ctx.amenities.pointer("/counts/restaurant/oneMile")).unwrap_or(0) >= 5;
    let is_rural_profile = ctx.lot_acres.map(|a| a >= 1.5).unwrap_or(false)
        || (walk_idx.map(|w| w < 5.0).unwrap_or(false)
            && !dense_amenities
            && ctx.geo.pointer("/msa/cbsa").is_none())
        || tract_bb_pct.map(|b| b < 78.0).unwrap_or(false);

    let confirmed_opts: &[crate::extensions::proto::BroadbandOptionProto] = live_bb_opt
        .map(|b| b.confirmed_options.as_slice())
        .unwrap_or(&[]);
    let fcc_res_opt = live_bb_opt.map(|b| &b.fcc_fixed);
    let addr_verified_avail = !confirmed_opts.is_empty();
    let addr_verified_fiber = confirmed_opts
        .iter()
        .any(|o| o.tier_category.eq_ignore_ascii_case("fiber"));
    let addr_verified_cable = confirmed_opts
        .iter()
        .any(|o| o.tier_category.eq_ignore_ascii_case("cable"));
    let fcc_live_verified = fcc_res_opt
        .map(|f| f.status == FccLookupStatus::LiveVerified)
        .unwrap_or(false);
    let fcc_fiber = fcc_res_opt
        .map(|f| f.status == FccLookupStatus::LiveVerified && f.fiber_available)
        .unwrap_or(false);
    let fcc_cable = fcc_res_opt
        .map(|f| f.status == FccLookupStatus::LiveVerified && f.cable_available)
        .unwrap_or(false);

    let (fiber_likely, rural_warning, readiness_tier, fcc_json) =
        if let Some(fcc) = fcc_res_opt.filter(|f| f.status == FccLookupStatus::LiveVerified) {
            let fib = fcc.fiber_available || addr_verified_fiber;
            let cab = fcc.cable_available || addr_verified_cable;
            let sat_only = !fib && !cab;
            let tier = if fib || fcc.max_download_mbps.unwrap_or(0) >= 900 {
                "Gigabit / Fiber Confirmed (Live Address Check)"
            } else if cab || fcc.max_download_mbps.unwrap_or(0) >= 100 {
                "High-Speed Cable / Broadband Confirmed (FCC BDC)"
            } else {
                "Limited / Satellite-or-DSL Risk (Verify Speeds)"
            };
            (
                Some(fib),
                sat_only || fcc.max_download_mbps.map(|d| d < 100).unwrap_or(false),
                tier,
                json!({
                    "queried": fcc.queried,
                    "status": fcc.status.as_str(),
                    "hasCredentials": fcc.has_credentials,
                    "httpStatus": fcc.http_status,
                    "carriersCount": fcc.providers.len(),
                    "fiberAvailable": fcc.fiber_available,
                    "cableAvailable": fcc.cable_available,
                    "maxDownloadMbps": fcc.max_download_mbps,
                    "maxUploadMbps": fcc.max_upload_mbps,
                    "carriers": fcc.providers,
                    "reason": fcc.reason,
                }),
            )
        } else if addr_verified_avail {
            let max_dl = confirmed_opts
                .iter()
                .map(|o| o.max_download_mbps)
                .max()
                .filter(|&v| v > 0);
            let max_ul = confirmed_opts
                .iter()
                .map(|o| o.max_upload_mbps)
                .max()
                .filter(|&v| v > 0);
            let tier = if addr_verified_fiber || max_dl.unwrap_or(0) >= 900 {
                "Gigabit / Fiber Confirmed (Live Address Check)"
            } else if addr_verified_cable || max_dl.unwrap_or(0) >= 100 {
                "High-Speed Cable / Broadband Confirmed (Address Check)"
            } else {
                "Broadband Verified at Address"
            };
            (
                Some(addr_verified_fiber),
                false,
                tier,
                json!({
                    "queried": fcc_res_opt.map(|f| f.queried).unwrap_or(false),
                    "status": fcc_res_opt.map(|f| f.status.as_str()).unwrap_or("unverified"),
                    "hasCredentials": fcc_res_opt.map(|f| f.has_credentials).unwrap_or(false),
                    "httpStatus": fcc_res_opt.and_then(|f| f.http_status),
                    "carriersCount": confirmed_opts.len(),
                    "fiberAvailable": addr_verified_fiber,
                    "cableAvailable": addr_verified_cable,
                    "maxDownloadMbps": max_dl,
                    "maxUploadMbps": max_ul,
                    "carriers": [],
                    "reason": fcc_res_opt.and_then(|f| f.reason.clone()),
                }),
            )
        } else {
            let fib_verified_false = if is_rural_profile { Some(false) } else { None };
            let tier = if is_rural_profile {
                "Rural Satellite / DSL Risk — Verify ISP Line-of-Sight & Trenching"
            } else {
                "Broadband Unverified — Confirm Address-Level Fiber / Cable Drop"
            };
            let fcc_status_str = fcc_res_opt
                .map(|f| f.status.as_str())
                .unwrap_or("offline_deterministic_fallback");
            (
                fib_verified_false,
                is_rural_profile,
                tier,
                json!({
                    "queried": fcc_res_opt.map(|f| f.queried).unwrap_or(false),
                    "status": fcc_status_str,
                    "hasCredentials": fcc_res_opt.map(|f| f.has_credentials).unwrap_or(false),
                    "httpStatus": fcc_res_opt.and_then(|f| f.http_status),
                    "carriersCount": 0,
                    "fiberAvailable": Value::Null,
                    "cableAvailable": Value::Null,
                    "maxDownloadMbps": Value::Null,
                    "maxUploadMbps": Value::Null,
                    "carriers": [],
                    "reason": fcc_res_opt.and_then(|f| f.reason.clone()),
                }),
            )
        };

    let confirmed_opts_json: Vec<Value> = confirmed_opts
        .iter()
        .map(|o| {
            json!({
                "carrierName": o.carrier_name,
                "technology": o.technology,
                "tierCategory": o.tier_category,
                "maxDownloadMbps": o.max_download_mbps,
                "maxUploadMbps": o.max_upload_mbps,
                "symmetrical": o.symmetrical,
                "typicalMonthlyUsd": o.typical_monthly_usd,
                "availabilityStatus": o.availability_status,
                "checkUrl": o.check_url,
            })
        })
        .collect();

    let warning_msg = if rural_warning {
        Some("Rural/low-density parcel or low tract broadband adoption (<78% ACS B28002) indicates elevated DSL, fixed-wireless, or satellite-only (Starlink) risk — verify wired gigabit fiber/cable availability directly on the FCC National Broadband Map before removing contingencies.".to_string())
    } else {
        None
    };

    let bb_inp = UtilityLookupInput {
        state: &ctx.state,
        county_fips: &ctx.county_fips,
        county_name: &ctx.county,
        city: &ctx.city,
        zip: &ctx.zip,
        sqft: ctx.sqft.map(|s| s.round() as i64).unwrap_or(0),
        year_built: ctx.year_built,
        is_condo_or_attached: false,
        hoa_covers_water_trash: false,
        is_likely_private_well: false,
        is_likely_private_septic: false,
        sdwis_water_name: None,
        sdwis_pwsid: None,
        gov_kwh_benchmark: None,
        has_solar: false,
        acs_broadband_pct: tract_bb_pct.or(zip_bb_pct),
        is_rural_area: is_rural_profile,
    };
    let mut broadband_catalog = resolve_broadband_catalog(
        &bb_inp,
        &fcc_lookup_url,
        fcc_live_verified.then_some(&fcc_json),
    );
    if addr_verified_avail {
        broadband_catalog.high_speed_available = true;
        if addr_verified_fiber {
            broadband_catalog.fiber_available = true;
            broadband_catalog.fiber_confirmed_fcc = true;
            broadband_catalog.internet_tier = "fiber_confirmed".to_string();
        }
        if !fcc_live_verified {
            broadband_catalog.verification_status = "address_verified".to_string();
        }
        for opt in confirmed_opts {
            broadband_catalog.max_download_mbps = broadband_catalog
                .max_download_mbps
                .max(opt.max_download_mbps);
            broadband_catalog.max_upload_mbps =
                broadband_catalog.max_upload_mbps.max(opt.max_upload_mbps);
            let mapped = crate::registry::utilities::BroadbandCarrierOption {
                carrier_name: opt.carrier_name.clone(),
                technology: opt.technology.clone(),
                tier_category: opt.tier_category.clone(),
                max_download_mbps: opt.max_download_mbps,
                max_upload_mbps: opt.max_upload_mbps,
                symmetrical: opt.symmetrical,
                typical_monthly_usd: opt.typical_monthly_usd.clone(),
                availability_status: opt.availability_status.clone(),
                check_url: opt.check_url.clone(),
            };
            match opt.tier_category.trim().to_ascii_lowercase().as_str() {
                "fiber" => {
                    broadband_catalog
                        .fiber_carriers
                        .retain(|c| !c.carrier_name.eq_ignore_ascii_case(&opt.carrier_name));
                    broadband_catalog.fiber_carriers.insert(0, mapped);
                }
                "cable" => {
                    broadband_catalog
                        .cable_carriers
                        .retain(|c| !c.carrier_name.eq_ignore_ascii_case(&opt.carrier_name));
                    broadband_catalog.cable_carriers.insert(0, mapped);
                }
                _ => {
                    broadband_catalog
                        .wireless_carriers
                        .retain(|c| !c.carrier_name.eq_ignore_ascii_case(&opt.carrier_name));
                    broadband_catalog.wireless_carriers.insert(0, mapped);
                }
            }
        }
        if let Some(top_fiber) = broadband_catalog.fiber_carriers.first() {
            broadband_catalog.headline_verdict = format!(
                "{} confirmed available at parcel ({} Mbps)",
                top_fiber.carrier_name, top_fiber.max_download_mbps
            );
        }
    } else if !fcc_live_verified {
        // Strictly refuse unverified fiber headlines or fabricated speeds when address checks didn't confirm service
        broadband_catalog.fiber_available = false;
        broadband_catalog.fiber_confirmed_fcc = false;
        broadband_catalog.high_speed_available = false;
        broadband_catalog.max_download_mbps = 0;
        broadband_catalog.max_upload_mbps = 0;
        broadband_catalog.est_monthly_internet_usd = 0;
        broadband_catalog.verification_status = "unverified".to_string();
        broadband_catalog.fiber_carriers.clear();
        if broadband_catalog.internet_tier == "fiber_area" {
            broadband_catalog.internet_tier = "unverified".to_string();
        }
        broadband_catalog.headline_verdict =
            "Address-level broadband availability unverified — confirm fiber/cable drop at carrier or FCC BDC map.".to_string();
    } else if fcc_live_verified && !fcc_fiber {
        broadband_catalog.fiber_available = false;
        broadband_catalog.fiber_confirmed_fcc = false;
        if !fcc_cable {
            broadband_catalog.high_speed_available = false;
        }
    }

    json!({
        "tractBroadbandPct": tract_bb_pct,
        "zipBroadbandPct": zip_bb_pct,
        "workFromHomePct": wfh_pct,
        "meanCommuteMin": commute_min,
        "remoteWorkReadinessTier": readiness_tier,
        "fiberLikely": fiber_likely,
        "fiberConfirmedAddress": addr_verified_fiber || fcc_fiber,
        "verificationStatus": broadband_catalog.verification_status,
        "ruralSatelliteDslRiskWarning": rural_warning,
        "warningMessage": warning_msg,
        "addressVerifiedOptions": confirmed_opts_json,
        "fcc": fcc_json,
        "fccLookupUrl": fcc_lookup_url,
        "broadbandCatalog": broadband_catalog,
    })
}

// ------------------------- 2(d). Water Source, Septic vs. Sewer & Point-of-Sale Mandates -------------------------

fn is_east_bay_psl_jurisdiction(ctx: &ReportContext) -> bool {
    if ctx.state != "CA" {
        return false;
    }
    let city_lc = ctx.city.to_ascii_lowercase();
    if [
        "oakland",
        "berkeley",
        "alameda",
        "albany",
        "emeryville",
        "piedmont",
        "el cerrito",
        "richmond",
        "kensington",
        "san pablo",
        "pinole",
        "hercules",
        "rodeo",
        "crockett",
        "lafayette",
        "orinda",
        "moraga",
        "san leandro",
        "castro valley",
        "hayward",
        "walnut creek",
        "pleasant hill",
    ]
    .iter()
    .any(|c| city_lc.contains(c))
    {
        return true;
    }
    matches!(ctx.county_fips.as_str(), "06001" | "06013")
}

fn build_utilities_and_mandates(
    ctx: &ReportContext,
    live_util_opt: Option<&LiveUtilityBundle>,
) -> (Value, bool, bool) {
    let water_env = ctx.environment.get("water").filter(|w| w.is_object());
    let water_prim = water_env.and_then(|w| w.get("primary")).filter(|p| {
        let nm = val_str(p.get("name")).unwrap_or("");
        let pid = val_str(p.get("pwsid"));
        !is_wholesaler_water_system(nm, pid)
    });
    let live_water = live_util_opt.and_then(|b| b.wst.as_ref());
    let live_water_name = live_water.and_then(|w| w.water_carrier_name.as_deref());
    let live_water_pwsid = live_water.and_then(|w| w.water_pwsid.as_deref());
    let has_public_water =
        water_prim.map(|p| p.is_object()).unwrap_or(false) || live_water_name.is_some();
    let well_explicit = RE_WELL.is_match(&ctx.combined_text)
        || val_str(ctx.facts.get("waterSource"))
            .map(|s| s.to_ascii_lowercase().contains("well"))
            .unwrap_or(false);
    let large_rural_lot = ctx.lot_acres.map(|a| a >= 0.75).unwrap_or(false);

    // Suburban/urban parcels never get misclassified as "Likely Private Well" merely because an SDWIS polygon query returned null
    let is_private_well = well_explicit || (!has_public_water && large_rural_lot);

    let water_class = if well_explicit {
        "Confirmed / Reported Private Well"
    } else if !has_public_water && large_rural_lot {
        "Likely Private Well (No Public Water System Boundary + >=0.75 ac Lot)"
    } else if !has_public_water {
        "Unverified — Confirm Municipal Water Utility"
    } else {
        "Municipal / Community Water System"
    };

    let mut water_tests = Vec::new();
    if is_private_well {
        water_tests.extend([
            "Certified lab water test for PFAS, nitrates/nitrites, arsenic, coliform/E. coli, and lead".to_string(),
            "Well pump flow-rate (GPM) & static water-level yield test + casing inspection".to_string(),
        ]);
    } else {
        water_tests.push("Review EPA SDWIS Consumer Confidence Report & test interior solder/galvanized pipes for lead if pre-1986.".to_string());
    }

    let septic_explicit = RE_SEPTIC.is_match(&ctx.combined_text)
        || val_str(ctx.facts.get("sewer"))
            .map(|s| s.to_ascii_lowercase().contains("septic"))
            .unwrap_or(false);
    let muni_sewer_explicit = val_str(ctx.facts.get("sewer"))
        .map(|s| {
            s.to_ascii_lowercase().contains("municipal")
                || s.to_ascii_lowercase().contains("public")
        })
        .unwrap_or(false);
    let live_sewer_name = live_water.and_then(|w| w.sewer_carrier_name.as_deref());
    let live_sewer_verified = live_water.map(|w| w.sewer_verified).unwrap_or(false);
    let is_private_septic =
        septic_explicit || (!muni_sewer_explicit && live_sewer_name.is_none() && large_rural_lot);

    let sewer_class = if septic_explicit {
        "Confirmed Private Septic System".to_string()
    } else if is_private_septic {
        "Likely Private Septic System (Lot >= 0.75 Acres / Rural Indicators)".to_string()
    } else if let Some(dist) = live_sewer_name {
        format!("Verified Municipal Sewer District — {dist}")
    } else if muni_sewer_explicit {
        "Confirmed Municipal Sanitary Sewer (Recommend Camera Lateral Scope)".to_string()
    } else {
        "Municipal Sewer Unverified — Confirm Sanitation District on County Tax Bill".to_string()
    };

    let septic_recs = if is_private_septic {
        vec![
            "Require full septic tank pump-out, baffle check, and hydraulic leach-field load inspection.".to_string(),
            "Verify county septic permit bedroom-count capacity; reserve $20,000–$45,000 capital risk if drainfield fails or requires engineered mound replacement.".to_string(),
        ]
    } else {
        vec![
            "Order a main sewer lateral video camera inspection from house cleanout to street main ($250–$400; tree-root intrusion or clay/Orangeburg pipe replacement runs $5,000–$18,000+).".to_string(),
        ]
    };

    // Point-of-Sale Municipal Mandates
    let mut mandates: Vec<Value> = Vec::new();
    let mut jflags: Vec<String> = Vec::new();

    if is_east_bay_psl_jurisdiction(ctx) {
        jflags.push("EAST_BAY_EBMUD_PSL".into());
        mandates.push(json!({
            "key": "east_bay_sewer_lateral",
            "title": "East Bay EBMUD Regional Private Sewer Lateral (PSL) Compliance Certificate",
            "authority": "EBMUD Regional PSL Ordinance (Alameda & Contra Costa Counties)",
            "requiredAtSale": true,
            "estCostRangeUsd": "$500 testing to $4,000–$18,000+ replacement",
            "detail": "Title transfer in EBMUD PSL jurisdictions (Oakland, Berkeley, Alameda, Albany, Emeryville, Piedmont, El Cerrito, Richmond, etc.) triggers mandatory pressure testing and a Compliance Certificate before closing unless a valid exemption/time-extension exists.",
        }));
    }

    let is_ne_pnw_oil = matches!(
        ctx.state.as_str(),
        "NJ" | "PA" | "NY" | "CT" | "MA" | "RI" | "NH" | "VT" | "ME" | "MD" | "DE" | "WA" | "OR"
    );
    let pre_1985 = ctx.year_built.map(|y| y < 1985).unwrap_or(false);
    if is_ne_pnw_oil && pre_1985 {
        jflags.push("PRE_1985_OIL_TANK_SWEEP".into());
        mandates.push(json!({
            "key": "buried_heating_oil_tank_sweep",
            "title": format!("{} Pre-1985 Buried Heating-Oil Tank Sweep (UST Scan)", ctx.state),
            "authority": if ctx.state == "NJ" { "NJDEP Underground Storage Tank / Spill Act Standards" } else { "State Environmental / Lender Underwriting Best Practice" },
            "requiredAtSale": ctx.state == "NJ",
            "estCostRangeUsd": "$250–$600 sweep; $15,000–$100,000+ if soil remediation needed",
            "detail": format!(
                "Built in {} (pre-1985) in {}: legacy buried #2 heating-oil tanks frequently create strict environmental remediation liability and lender blocks. Order a magnetometer + ground-penetrating radar (GPR) oil-tank sweep before inspection contingency expires.",
                ctx.year_built.unwrap_or(1970),
                ctx.state
            ),
        }));
    }

    if ctx.state == "CA" {
        jflags.push("CA_SEISMIC_STRAP_AND_LOW_FLOW".into());
        mandates.push(json!({
            "key": "ca_water_heater_seismic_strap",
            "title": "CA Water-Heater Seismic Bracing & Strapping Certification",
            "authority": "CA Health & Safety Code § 19211",
            "requiredAtSale": true,
            "estCostRangeUsd": "$150–$450 retrofit",
            "detail": "California sellers must brace, anchor, or strap all tank-style water heaters to resist earthquake displacement and certify compliance in writing prior to transfer.",
        }));
        if ctx.year_built.map(|y| y <= 1994).unwrap_or(true) {
            mandates.push(json!({
                "key": "ca_low_flow_plumbing",
                "title": "CA Water-Conserving Plumbing Fixture Certification",
                "authority": "CA Civil Code § 1101.4",
                "requiredAtSale": true,
                "estCostRangeUsd": "$200–$1,200 fixture upgrade",
                "detail": "Pre-1994 California single-family homes must have compliant low-flow toilets (<=1.6 gpf), showerheads (<=2.5 gpm), and faucets with written point-of-sale seller disclosure.",
            }));
        }
    }

    if ctx.year_built.map(|y| y < 1978).unwrap_or(false) {
        jflags.push("FEDERAL_PRE_1978_LEAD_PAINT".into());
        mandates.push(json!({
            "key": "federal_lead_paint",
            "title": "Federal Pre-1978 Lead-Based Paint Disclosure & 10-Day Inspection Window",
            "authority": "42 U.S.C. § 4852d",
            "requiredAtSale": true,
            "estCostRangeUsd": "$0 disclosure / $350–$700 XRF test",
            "detail": "Mandatory federal lead-based paint disclosure and EPA pamphlet for homes built prior to 1978.",
        }));
    }

    let out = json!({
        "waterSource": {
            "classification": water_class,
            "isLikelyPrivateWell": is_private_well,
            "systemName": water_prim.and_then(|p| p.get("name")).cloned().or_else(|| live_water_name.map(|n| json!(n))).unwrap_or(Value::Null),
            "pwsid": water_prim.and_then(|p| p.get("pwsid")).cloned().or_else(|| live_water_pwsid.map(|p| json!(p))).unwrap_or(Value::Null),
            "healthViolations3y": ctx.environment.pointer("/water/violations10y/healthBasedLast3y").cloned().unwrap_or(Value::Null),
            "lead90thPpb": ctx.environment.pointer("/water/leadAndCopper/lead90thPpb").cloned().unwrap_or(Value::Null),
            "testingRecommendations": water_tests,
        },
        "sewerAndSeptic": {
            "classification": sewer_class,
            "sewerDistrictName": live_sewer_name,
            "sewerDistrictVerified": live_sewer_verified || live_sewer_name.is_some(),
            "isLikelyPrivateSeptic": is_private_septic,
            "lotAcres": ctx.lot_acres.map(|a| round_n(a, 2)),
            "replacementCostRangeUsd": if is_private_septic { "$20,000–$45,000" } else { "$5,000–$18,000 (lateral)" },
            "inspectionRecommendations": septic_recs,
        },
        "pointOfSaleMandates": {
            "jurisdictionFlags": jflags,
            "mandates": mandates,
        },
    });

    (out, is_private_septic, is_private_well)
}

// ------------------------- 2(e). Adjacent Construction Pipeline, Upzoning & Historic -------------------------

fn build_neighborhood_pipeline(ctx: &ReportContext) -> Value {
    let zoning = val_str(ctx.facts.get("zoning"))
        .or_else(|| val_str(ctx.facts.pointer("/buildingRegister/zoning")))
        .unwrap_or("");
    let zoning_lc = zoning.to_ascii_lowercase();

    let mut historic_signals: Vec<String> = Vec::new();
    if RE_HISTORIC.is_match(&ctx.combined_text)
        || ctx
            .facts
            .pointer("/buildingRegister/historic")
            .and_then(|v| v.as_bool())
            == Some(true)
        || val_str(ctx.facts.pointer("/buildingRegister/historicDistrict")).is_some()
        || ctx
            .facts
            .pointer("/buildingRegister/landmark")
            .and_then(|v| v.as_bool())
            == Some(true)
    {
        historic_signals.push("Listing/building register references historic district, landmark, or preservation rules.".into());
    }
    if zoning_lc.contains("hpoz")
        || zoning_lc.contains("hist")
        || zoning_lc.contains("(hd)")
        || zoning_lc.starts_with("hp")
    {
        historic_signals.push(format!(
            "Zoning code `{zoning}` indicates a historic preservation overlay."
        ));
    }
    if let Some(pois) = ctx.amenities.get("pois").and_then(|v| v.as_array()) {
        for p in pois {
            let p_str = serde_json::to_string(p)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let dm = val_i64(p.get("distM")).unwrap_or(9999);
            if dm <= 400
                && (p_str.contains("historic")
                    || p_str.contains("heritage")
                    || p_str.contains("monument"))
            {
                historic_signals.push(format!(
                    "Nearby OSM historic/heritage feature ({}) within {}m.",
                    val_str(p.get("name")).unwrap_or("landmark"),
                    dm
                ));
                break;
            }
        }
    }
    if ctx.year_built.map(|y| y < 1940).unwrap_or(false) {
        historic_signals.push(format!(
            "Pre-1940 construction ({}) may trigger municipal historic-resource screening prior to exterior demolition or window swaps.",
            ctx.year_built.unwrap_or(1930)
        ));
    }

    let is_historic = !historic_signals.is_empty();
    let alteration_constraints: Vec<String> = if is_historic {
        vec![
            "Exterior window replacements, siding changes, and front-elevation alterations often require Historic Preservation Commission / Design Review.".into(),
            "Street-facing solar arrays and rear/second-story additions may face heightened visibility setbacks and +20%–40% specialized restoration costs.".into(),
        ]
    } else {
        vec!["No explicit historic-landmark overlay detected; confirm local design-review guidelines before major exterior additions.".into()]
    };

    // Adjacent Zoning / Non-Residential Border & Infill Pressure
    let cnt_half = |cat: &str| -> i64 {
        ctx.amenities
            .pointer(&format!("/counts/{cat}/halfMile"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    let ind_near = cnt_half("industrial");
    let fuel_near = cnt_half("fuel");
    let retail_near = cnt_half("shopping") + cnt_half("restaurant");
    let transit_m = ctx
        .amenities
        .pointer("/nearest/transit/distM")
        .and_then(|v| v.as_i64());
    let arterial_m = ctx
        .amenities
        .pointer("/proximity/arterial/distM")
        .and_then(|v| v.as_i64());
    let fwy_m = ctx
        .amenities
        .pointer("/proximity/freeway/distM")
        .and_then(|v| v.as_i64());

    let non_res_border = ind_near > 0
        || fuel_near > 0
        || arterial_m.map(|d| d <= 120).unwrap_or(false)
        || fwy_m.map(|d| d <= 250).unwrap_or(false)
        || zoning_lc.starts_with("mu")
        || zoning_lc.starts_with('c');

    let transit_infill = transit_m.map(|d| d <= 805).unwrap_or(false) || retail_near >= 12;
    let ca_sb9_note = ctx.state == "CA"
        && ctx.lot_sqft.map(|l| l >= 2400.0).unwrap_or(false)
        && !ctx.property_type.to_ascii_lowercase().contains("condo");

    let infill_pressure = if non_res_border && transit_infill {
        "High — Commercial/Arterial Corridor & Transit-Oriented Infill Pressure"
    } else if transit_infill
        || zoning_lc.contains("r-3")
        || zoning_lc.contains("r-4")
        || zoning_lc.contains("rm")
    {
        "Elevated — Multi-Family / Transit-Adjacent Infill Zone"
    } else if ca_sb9_note {
        "Moderate — CA Statewide SB 9 / ADU Infill Eligible"
    } else {
        "Low — Established Residential Pattern"
    };

    let mut notes = Vec::new();
    if non_res_border {
        notes.push(format!(
            "Non-residential or corridor adjacency detected (industrial within 0.5 mi: {ind_near}, gas stations: {fuel_near}, arterial dist: {}m) — higher likelihood of adjacent commercial deliveries, lighting, or mid-rise redevelopment.",
            arterial_m.map(|d| d.to_string()).unwrap_or_else(|| "n/a".into())
        ));
    }
    if ca_sb9_note {
        notes.push("CA SB 9 & Gov Code §66314 allow ministerial lot splits / duplexes / detached ADUs on qualifying single-family parcels — check adjacent lot redevelopment potential.".into());
    }
    if notes.is_empty() {
        notes.push("Check city planning agenda & active building permits within 500 ft for pending subdivisions or upzoning.".into());
    }

    let permits_count = ctx
        .facts
        .pointer("/buildingRegister/permits")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let permit_portal = ctx
        .facts
        .pointer("/buildingRegister/permitPortal/url")
        .and_then(|v| v.as_str())
        .map(String::from);

    json!({
        "historicRestrictions": {
            "isHistoricOrSensitive": is_historic,
            "status": if is_historic { "Historic / Architectural Review Sensitivity Flagged" } else { "No Historic District Flag Detected" },
            "zoning": if zoning.is_empty() { Value::Null } else { json!(zoning) },
            "yearBuilt": ctx.year_built,
            "detectedSignals": historic_signals,
            "alterationConstraints": alteration_constraints,
            "lookupUrl": permit_portal.clone().unwrap_or_else(|| "https://www.nps.gov/subjects/nationalregister/database-research.htm".into()),
        },
        "adjacentZoningAndInfill": {
            "nonResidentialBorderRisk": non_res_border,
            "infillPressureLevel": infill_pressure,
            "nearbyNonResidentialCountsHalfMile": {
                "industrial": ind_near,
                "fuelStations": fuel_near,
                "retailAndDining": retail_near,
            },
            "arterialOrFreewayBorder": arterial_m.map(|d| d <= 120).unwrap_or(false) || fwy_m.map(|d| d <= 250).unwrap_or(false),
            "transitOrientedInfill": transit_infill,
            "permitsObservedCount": permits_count,
            "permitPortalUrl": permit_portal,
            "notes": notes,
        },
    })
}

// ------------------------- 2(f). Rent-vs-Buy Break-Even & Exit Liquidity Score -------------------------

fn build_exit_and_breakeven(
    ctx: &ReportContext,
    hoa_monthly: f64,
    special_district_annual: f64,
    seller_transfer_tax_usd: f64,
    buyer_closing_20_usd: f64,
) -> Value {
    let price = ctx.price;
    let down_usd = price * 0.20;
    let initial_loan = price * 0.80;
    let pi_mo = monthly_pi(initial_loan, ctx.rate30, 30);

    let rent_val_opt = val_f64(ctx.valuation.pointer("/ownership/rentEstimate"))
        .or_else(|| val_f64(ctx.facts.get("rentEstimate")))
        .filter(|&r| r > 200.0);
    let rent_zori_opt = val_f64(ctx.market.pointer("/rent/zori")).filter(|&r| r > 200.0);
    let rent_estimate_reported = rent_val_opt.is_some() || rent_zori_opt.is_some();
    let rent_estimate_source = if rent_val_opt.is_some() {
        "valuation_or_mls"
    } else if rent_zori_opt.is_some() {
        "zillow_zori"
    } else {
        "modeled_price_yield_fallback"
    };
    let est_rent_mo = rent_val_opt
        .or(rent_zori_opt)
        .unwrap_or_else(|| (price * 0.0048).clamp(1_200.0, 12_000.0));

    let maint_annual = price
        * if ctx.year_built.map(|y| y < 1980).unwrap_or(false) {
            0.010
        } else {
            0.0075
        };
    let cagr10_opt = val_f64(ctx.market.pointer("/zhvi/cagr10"));
    let cagr10_reported = cagr10_opt.is_some();
    let cagr10 = cagr10_opt.unwrap_or(4.2);
    let appr_pct = (cagr10 * 0.65).clamp(2.5, 5.2);
    let appr_rate = appr_pct / 100.0;
    let rent_inf_pct = 3.2;
    let rent_inf = rent_inf_pct / 100.0;
    let opp_rate_pct = 4.25; // 10-yr Treasury / after-tax opportunity benchmark
    let opp_rate = 0.036; // ~4.25% Treasury yield net of marginal federal tax drag
    let selling_comm_pct = 5.5;
    let seller_transfer_pct = if price > 0.0 {
        (seller_transfer_tax_usd / price) * 100.0
    } else {
        0.0
    };
    let round_trip_friction_pct = round_n(
        selling_comm_pct + seller_transfer_pct + (buyer_closing_20_usd / price) * 100.0,
        2,
    );

    let tax_deduct_note = if (initial_loan * (ctx.rate30 / 100.0) + ctx.annual_tax.min(10_000.0))
        > 29_200.0
    {
        format!(
            "Year-1 mortgage interest (~${}/yr on ${}k debt up to $750k cap) + SALT (capped at $10,000 under current rules) exceeds the $29,200 MFJ standard deduction — partial marginal federal itemization savings likely.",
            (initial_loan.min(750_000.0) * (ctx.rate30 / 100.0)).round() as i64,
            (initial_loan / 1000.0).round() as i64
        )
    } else {
        "Combined Year-1 mortgage interest + SALT ($10k federal cap) stays near or below the $29,200 married-filing-jointly standard deduction, so this model conservatively assumes zero extra mortgage-interest tax subsidy for MFJ filers.".to_string()
    };

    let mut renter_port = down_usd + buyer_closing_20_usd;
    let mut cur_rent_mo = est_rent_mo;
    let mut cur_tax_yr = ctx.annual_tax;
    let mut cur_ins_yr = ctx.annual_ins;
    let mut cur_hoa_yr = hoa_monthly * 12.0;
    let mut cur_maint_yr = maint_annual;
    let tax_growth = if ctx.state == "CA" { 0.02 } else { 0.025 };

    let mut breakeven_yrs: Option<f64> = None;
    let mut prev_diff: Option<f64> = None;
    let mut y5_adv: i64 = 0;
    let mut y10_adv: i64 = 0;
    let mut schedule = Vec::new();

    for yr in 1..=30usize {
        let buy_outflow_yr = pi_mo * 12.0
            + cur_tax_yr
            + cur_ins_yr
            + cur_hoa_yr
            + special_district_annual
            + cur_maint_yr;
        let rent_outflow_yr = cur_rent_mo * 12.0 + 300.0; // renters insurance
        let savings_delta = buy_outflow_yr - rent_outflow_yr;
        renter_port = renter_port * (1.0 + opp_rate) + savings_delta;

        let home_val = price * (1.0 + appr_rate).powi(yr as i32);
        let rem_debt = remaining_balance_after_months(initial_loan, ctx.rate30, 30, yr * 12);
        let sell_cost = home_val * ((selling_comm_pct + seller_transfer_pct) / 100.0);
        let buyer_net = home_val - rem_debt - sell_cost;
        let diff = buyer_net - renter_port;

        if breakeven_yrs.is_none() && diff >= 0.0 {
            let frac = match prev_diff {
                Some(pd) if pd < 0.0 && (diff - pd).abs() > 1.0 => (-pd) / (diff - pd),
                _ => 0.5,
            };
            breakeven_yrs = Some(round_n((yr as f64 - 1.0) + frac.clamp(0.05, 1.0), 1));
        }
        prev_diff = Some(diff);
        if yr == 5 {
            y5_adv = diff.round() as i64;
        }
        if yr == 10 {
            y10_adv = diff.round() as i64;
        }
        if matches!(yr, 1 | 3 | 5 | 7 | 10 | 15) {
            schedule.push(json!({
                "year": yr,
                "buyerNetProceedsUsd": buyer_net.round() as i64,
                "renterPortfolioUsd": renter_port.round() as i64,
                "buyAdvantageUsd": diff.round() as i64,
            }));
        }

        cur_rent_mo *= 1.0 + rent_inf;
        cur_tax_yr *= 1.0 + tax_growth;
        cur_ins_yr *= 1.03;
        cur_hoa_yr *= 1.03;
        cur_maint_yr *= 1.025;
    }

    let final_be_yrs = breakeven_yrs.unwrap_or(30.0);
    let price_to_rent = round_n(price / (est_rent_mo * 12.0), 1);

    // Exit Liquidity & Functional Obsolescence Score (0-100)
    let mut score: f64 = 88.0;
    let mut obs_flags: Vec<Value> = Vec::new();
    let mut pos_drivers: Vec<String> = Vec::new();

    if let Some(baths) = ctx.baths {
        if baths <= 1.0 {
            score -= 16.0;
            obs_flags.push(json!({
                "key": "single_bath_obsolescence",
                "severity": "high",
                "penaltyPts": -16,
                "detail": "1-bathroom floor plan narrows resale demand across multi-person households; verify space to add a half/second bath.",
            }));
        } else if baths >= 2.5 {
            score += 4.0;
            pos_drivers.push("2.5+ baths supports strong mainstream resale liquidity.".into());
        }
    }

    // 2-bed home in larger-home ZIP
    let comp_med_beds = ctx
        .facts
        .get("comps")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            let mut bs: Vec<i64> = arr.iter().filter_map(|c| val_i64(c.get("beds"))).collect();
            if bs.is_empty() {
                None
            } else {
                bs.sort();
                Some(bs[bs.len() / 2])
            }
        });
    let sfr_pct = val_f64(ctx.market.pointer("/acs/tract/singleFamilyDetachedPct"));
    let is_larger_home_zip = comp_med_beds.map(|b| b >= 3).unwrap_or(false)
        || sfr_pct.map(|p| p >= 65.0).unwrap_or(false);
    if ctx.beds.map(|b| b <= 2).unwrap_or(false)
        && !ctx.property_type.to_ascii_lowercase().contains("condo")
        && is_larger_home_zip
    {
        score -= 10.0;
        obs_flags.push(json!({
            "key": "two_bed_floorplan_pool",
            "severity": "moderate",
            "penaltyPts": -10,
            "detail": "<=2-bedroom house in a 3–4+ bedroom family-dominant ZIP reduces future buyer pool width.",
        }));
    }

    // >2x neighborhood median price ceiling
    let zip_med = val_f64(ctx.market.pointer("/zhvi/latest"))
        .or_else(|| val_f64(ctx.market.pointer("/tiers/mid")))
        .or_else(|| val_f64(ctx.market.pointer("/acs/tract/medianHomeValue")));
    if let Some(zm) = zip_med.filter(|&v| v > 50_000.0) {
        if price > zm * 2.0 {
            score -= 14.0;
            obs_flags.push(json!({
                "key": "over_improved_price_ceiling",
                "severity": "high",
                "penaltyPts": -14,
                "detail": format!(
                    "Subject value (${}) exceeds 2.0x neighborhood median (${}) — over-improved homes face thinner appraisal comps and longer days-on-market.",
                    price.round() as i64,
                    zm.round() as i64
                ),
            }));
        }
    }

    // Arterial / freeway / rail / high-voltage power line backing
    if let Some(d) = val_f64(ctx.amenities.pointer("/proximity/freeway/distM")) {
        if d <= 240.0 {
            score -= 10.0;
            obs_flags.push(json!({
                "key": "freeway_backing",
                "severity": "moderate",
                "penaltyPts": -10,
                "detail": format!("Within {:.0}m of freeway corridor — persistent noise/air-quality discount at resale.", d),
            }));
        }
    }
    if let Some(d) = val_f64(ctx.amenities.pointer("/proximity/arterial/distM")) {
        if d <= 50.0 {
            score -= 8.0;
            obs_flags.push(json!({
                "key": "arterial_frontage",
                "severity": "moderate",
                "penaltyPts": -8,
                "detail": format!("Directly adjacent to arterial roadway ({:.0}m) — street-noise and driveway-egress objection.", d),
            }));
        }
    }
    if let Some(d) = val_f64(ctx.amenities.pointer("/proximity/rail/distM")) {
        if d <= 200.0 {
            score -= 7.0;
            obs_flags.push(json!({
                "key": "rail_corridor_proximity",
                "severity": "moderate",
                "penaltyPts": -7,
                "detail": format!("Rail corridor within {:.0}m — vibration/horn noise narrows resale pool.", d),
            }));
        }
    }
    if let Some(d) = val_f64(ctx.amenities.pointer("/proximity/highVoltageLine/distM")) {
        if d <= 100.0 {
            score -= 8.0;
            obs_flags.push(json!({
                "key": "high_voltage_powerline",
                "severity": "moderate",
                "penaltyPts": -8,
                "detail": format!("High-voltage transmission line within {:.0}m — visual/EMF buyer resistance and FHA safety setback rules.", d),
            }));
        }
    }

    // Housing-stock vacancy & owner-occupancy share (non-demographic physical market liquidity)
    let vac_pct = val_f64(ctx.market.pointer("/acs/tract/vacancyPct"));
    let owner_pct = val_f64(ctx.market.pointer("/acs/tract/ownerOccupiedPct"));
    if let Some(vp) = vac_pct.filter(|&v| v >= 12.0) {
        score -= 8.0;
        obs_flags.push(json!({
            "key": "elevated_tract_vacancy",
            "severity": "moderate",
            "penaltyPts": -8,
            "detail": format!("Census tract housing vacancy rate is {:.1}% (>=12%) — slower resale absorption.", vp),
        }));
    }
    if let Some(op) = owner_pct.filter(|&o| o < 35.0) {
        score -= 6.0;
        obs_flags.push(json!({
            "key": "high_renter_stock_share",
            "severity": "watch",
            "penaltyPts": -6,
            "detail": format!("Low owner-occupied housing share ({:.1}%) may trigger condo/investor-concentration underwriting scrutiny.", op),
        }));
    }

    let dom = val_f64(ctx.market.pointer("/metroMarket/days_to_pending/value"));
    if let Some(d) = dom {
        if d > 60.0 {
            score -= 6.0;
        } else if d <= 21.0 {
            score += 4.0;
            pos_drivers.push(format!(
                "Fast metro absorption (~{:.0} median days to pending).",
                d
            ));
        }
    }

    let final_score = score.round().clamp(0.0, 100.0) as i64;
    let tier = if final_score >= 80 {
        "High Liquidity (80–100)"
    } else if final_score >= 65 {
        "Good Liquidity (65–79)"
    } else if final_score >= 45 {
        "Moderate / Niche Buyer Pool (45–64)"
    } else {
        "Thin Exit Liquidity (<45)"
    };

    json!({
        "rentVsBuy": {
            "purchasePriceUsd": price.round() as i64,
            "downPayment20Usd": down_usd.round() as i64,
            "buyerClosingCostsUsd": buyer_closing_20_usd.round() as i64,
            "monthlyRentYear1Usd": est_rent_mo.round() as i64,
            "rentEstimateReported": rent_estimate_reported,
            "rentEstimateSource": rent_estimate_source,
            "cagr10Reported": cagr10_reported,
            "priceReported": ctx.price_reported,
            "rate30Reported": ctx.rate30_reported,
            "monthlyBuyOutflowYear1Usd": (pi_mo + (ctx.annual_tax + ctx.annual_ins + special_district_annual + maint_annual) / 12.0 + hoa_monthly).round() as i64,
            "priceToRentRatio": price_to_rent,
            "appreciationRatePct": round_n(appr_pct, 2),
            "rentInflationPct": rent_inf_pct,
            "opportunityCostBenchmarkPct": opp_rate_pct,
            "sellingCommissionPct": selling_comm_pct,
            "sellerTransferTaxUsd": seller_transfer_tax_usd.round() as i64,
            "roundTripFrictionPct": round_trip_friction_pct,
            "taxVsStandardDeductionNote": tax_deduct_note,
            "breakEvenYears": final_be_yrs,
            "breaksEvenWithin30Years": breakeven_yrs.is_some(),
            "year5NetAdvantageUsd": y5_adv,
            "year10NetAdvantageUsd": y10_adv,
            "schedule": schedule,
        },
        "exitLiquidity": {
            "score": final_score,
            "liquidityTier": tier,
            "obsolescenceFlags": obs_flags,
            "positiveDrivers": pos_drivers,
            "metrics": {
                "baths": ctx.baths,
                "beds": ctx.beds,
                "zipMedianValueUsd": zip_med.map(|v| v.round() as i64),
                "tractVacancyPct": vac_pct,
                "tractOwnerOccupiedPct": owner_pct,
                "metroDaysToPending": dom,
            },
        },
        "disclaimer": BUYER_DILIGENCE_DISCLAIMER,
    })
}

// ------------------------- 2(g). Day 1-30 Post-Closing Homestead & Climate Calendar -------------------------

fn build_post_closing_and_climate(ctx: &ReportContext, has_hoa: bool) -> Value {
    let st = ctx.state.as_str();
    let price = ctx.price;

    let homestead = match st {
        "TX" => {
            let est_sav =
                (100_000.0 * (ctx.tax_rate * 0.62).max(0.008) + price * 0.001).round() as i64;
            json!({
                "state": "TX",
                "programName": "Texas Residence Homestead Exemption ($100,000 School District + 10% Appraisal Cap)",
                "available": true,
                "estimatedAnnualSavingsUsd": est_sav,
                "capProtectionNote": "10% annual homestead cap on County Appraisal District (CAD) taxable value increase (begins the year after Jan 1 residency qualification).",
                "filingDeadline": "Jan 1 – Apr 30 (new buyers may file immediately after closing for partial-year qualification under Tex. Tax Code § 11.42(f)).",
                "actionRequired": "Update TX Driver's License to the new property address and submit Form 50-114 online with your County Appraisal District.",
                "lookupUrl": "https://comptroller.texas.gov/taxes/property-tax/exemptions/",
            })
        }
        "FL" => {
            let est_sav = (50_000.0 * ctx.tax_rate.max(0.011)).round() as i64;
            json!({
                "state": "FL",
                "programName": "Florida Homestead Exemption ($50,000 Exemption + Save Our Homes 3% Cap)",
                "available": true,
                "estimatedAnnualSavingsUsd": est_sav,
                "capProtectionNote": "Save Our Homes (SOH) caps annual assessed-value increases at 3% or CPI, and allows porting accumulated SOH benefit from a prior FL homestead.",
                "filingDeadline": "March 1 of the tax year (must own & occupy as permanent primary residence on January 1).",
                "actionRequired": "Update FL Driver's License, vehicle registration, and voter registration, then file with the County Property Appraiser.",
                "lookupUrl": "https://floridarevenue.com/property/pages/taxpayers_exemptions.aspx",
            })
        }
        "CA" => {
            let est_sav = (7_000.0 * ctx.tax_rate.max(0.011)).round() as i64;
            json!({
                "state": "CA",
                "programName": "California Homeowners' Exemption ($7,000 Value Reduction) & Prop 19 Portability",
                "available": true,
                "estimatedAnnualSavingsUsd": est_sav,
                "capProtectionNote": "Prop 13 caps annual assessed value growth at +2%/yr; Prop 19 allows eligible buyers aged 55+, severely disabled, or wildfire victims to transfer a lower prior CA tax base anywhere statewide within 2 years.",
                "filingDeadline": "February 15 for full 100% exemption (80% partial credit if filed by December 10); file Prop 19 base-year transfer claim within 3 years.",
                "actionRequired": "Submit BOE-266 (Homeowners' Exemption) and, if applicable, BOE-19-B (Prop 19 transfer) to the County Assessor — AND budget for Year-1 Prop 13 Supplemental Tax Bills.",
                "lookupUrl": "https://www.boe.ca.gov/proptaxes/homeowners_exemption.htm",
            })
        }
        "NY" => {
            json!({
                "state": "NY",
                "programName": "New York STAR Credit / Enhanced STAR (65+) & NYC Co-op/Condo Abatement",
                "available": true,
                "estimatedAnnualSavingsUsd": 650,
                "capProtectionNote": "Basic STAR saves ~$300–$800+/yr in school taxes (income <= $500k); Enhanced STAR (65+) saves ~$1,000–$1,800+.",
                "filingDeadline": "March 1 / July 1 depending on municipality — register immediately upon taking title.",
                "actionRequired": "Register for the STAR check/credit directly with NYS Dept. of Taxation & Finance (and check NYC DOF Co-op/Condo Tax Abatement if NYC).",
                "lookupUrl": "https://www.tax.ny.gov/pit/property/star/",
            })
        }
        "MA" => {
            json!({
                "state": "MA",
                "programName": "Massachusetts Declaration of Homestead ($500,000 Equity Protection) + Municipal Residential Exemption",
                "available": true,
                "estimatedAnnualSavingsUsd": if ctx.city.to_ascii_lowercase().contains("boston") || ctx.city.to_ascii_lowercase().contains("cambridge") { 2_800 } else { 0 },
                "capProtectionNote": "Recording a Declaration of Homestead protects up to $500,000 of primary residence equity from unsecured creditors ($1,000,000 if 62+); Boston/Cambridge/Somerville/Brookline also offer 20–35% owner-occupied Residential Property Tax Exemptions.",
                "filingDeadline": "Record Homestead Declaration at closing; municipal Residential Exemption applications typically due April 1.",
                "actionRequired": "Ask closing attorney to record MA Declaration of Homestead with deed & check city assessor for Residential Exemption.",
                "lookupUrl": "https://www.sec.state.ma.us/divisions/registry-of-deeds/registry-of-deeds.htm",
            })
        }
        "IL" => json!({
            "state": "IL",
            "programName": "Illinois General Homestead Exemption",
            "available": true,
            "estimatedAnnualSavingsUsd": 950,
            "capProtectionNote": "Reduces equalized assessed value (EAV) by $6,000–$10,000 for owner-occupants (Cook County $10,000).",
            "filingDeadline": "Annual County Assessor exemption window (check Cook/Collar County schedule after Jan 1 occupancy).",
            "actionRequired": "Verify General Homestead Exemption transfer with County Assessor.",
            "lookupUrl": "https://tax.illinois.gov/localgovernments/property/taxrelief.html",
        }),
        "GA" => json!({
            "state": "GA",
            "programName": "Georgia Homestead Exemption & Assessment Freeze (where applicable)",
            "available": true,
            "estimatedAnnualSavingsUsd": 550,
            "capProtectionNote": "Multiple Georgia counties (Fulton, DeKalb, Cobb, Gwinnett) offer additional floating homestead assessment freezes.",
            "filingDeadline": "April 1 (must own and occupy on January 1).",
            "actionRequired": "File homestead application with County Tax Commissioner by April 1.",
            "lookupUrl": "https://dor.georgia.gov/property-tax-homestead-exemptions",
        }),
        "WA" => json!({
            "state": "WA",
            "programName": "Washington Senior / Disabled Property Tax Exemption (no general homestead tax break)",
            "available": false,
            "estimatedAnnualSavingsUsd": 0,
            "capProtectionNote": "WA offers targeted property tax exemptions for qualifying seniors (61+) / disabled homeowners meeting county income thresholds, plus $125,000 statutory creditor homestead equity protection.",
            "filingDeadline": "Apply with County Assessor once eligible.",
            "actionRequired": "Verify eligibility only if 61+/disabled/veteran; otherwise budget full assessed tax.",
            "lookupUrl": "https://dor.wa.gov/taxes-rates/property-tax/property-tax-exemptions-and-deferrals",
        }),
        _ => json!({
            "state": st,
            "programName": format!("{} Primary-Residence / Homestead Tax Exemption Check", if st.is_empty() { "State" } else { st }),
            "available": false,
            "verifiedStateRule": false,
            "estimatedAnnualSavingsUsd": Value::Null,
            "capProtectionNote": "State-specific homestead credit/cap formula not modeled for this jurisdiction; confirm eligibility and exemption amounts with your local County Assessor / Tax Collector.",
            "filingDeadline": "Typically Q1–Q2 (Jan 1 – Apr 30) following Jan 1 occupancy; verify county assessor calendar.",
            "actionRequired": "Confirm owner-occupancy tax exemption filing with County Assessor / Tax Collector within 30 days of closing.",
            "lookupUrl": "https://www.taxadmin.org/",
        }),
    };

    let mut calendar = vec![
        json!({
            "dayRange": "Day 1–3",
            "category": "Security & Utilities",
            "priority": "High",
            "title": "Re-Key All Exterior Locks, Reset Garage Codes & Activate Electric/Gas/Water/Trash/Fiber",
            "detail": "Label main water shutoff valve, gas shutoff wrench, sewer cleanout cap, and electrical breaker panel.",
        }),
        json!({
            "dayRange": "Day 4–10",
            "category": "Taxes & Escrow",
            "priority": "High",
            "title": format!("File {} Homestead / Primary-Residence Tax Exemption & Audit Closing Disclosure", if st.is_empty() { "State" } else { st }),
            "detail": homestead["actionRequired"].clone(),
        }),
        json!({
            "dayRange": "Day 11–20",
            "category": "Insurance & Records",
            "priority": "Medium",
            "title": "Confirm Recorded Deed, Owner's Title Policy & Home Insurance Endorsements",
            "detail": "Store recorded deed, Final Closing Disclosure, and photo/video home-inventory for insurance claims.",
        }),
    ];
    if has_hoa {
        calendar.push(json!({
            "dayRange": "Day 15–25",
            "category": "HOA",
            "priority": "High",
            "title": "Register with HOA Management Portal & Confirm $0.00 Opening Ledger",
            "detail": "Set up autopay after confirming escrow payoff posted, and obtain gate fobs/pool keys/architectural rules.",
        }));
    }
    calendar.push(json!({
        "dayRange": "Day 21–30",
        "category": "Preventive Maintenance",
        "priority": "Medium",
        "title": "Baseline HVAC Service, Water-Heater Flush, Dryer-Vent Cleaning & Smoke/CO Battery Swap",
        "detail": if st == "CA" {
            "Also set aside cash reserve for the County Assessor's Year-1 Prop 13 Supplemental Property Tax Bill (billed directly, often not escrowed)."
        } else {
            "Test sump pump/GFCI outlets and verify grading/downspouts drain >= 5 ft away from foundation."
        },
    }));

    // Seasonal Climate Maintenance Readiness Flags
    let extreme_heat = matches!(
        st,
        "AZ" | "TX" | "FL" | "NV" | "LA" | "GA" | "SC" | "AL" | "MS" | "NM" | "OK"
    ) || (st == "CA" && ctx.lat.map(|la| la < 38.8).unwrap_or(false));
    let hard_freeze = matches!(
        st,
        "MN" | "WI"
            | "MI"
            | "IL"
            | "NY"
            | "NJ"
            | "PA"
            | "MA"
            | "CT"
            | "RI"
            | "NH"
            | "VT"
            | "ME"
            | "OH"
            | "IN"
            | "IA"
            | "MO"
            | "KS"
            | "NE"
            | "ND"
            | "SD"
            | "CO"
            | "UT"
            | "WY"
            | "MT"
            | "ID"
            | "WA"
            | "OR"
            | "TX"
            | "VA"
            | "MD"
            | "DC"
    );
    let coastal_salt = matches!(st, "FL" | "HI" | "LA" | "SC" | "NC" | "DE" | "RI" | "MA")
        || ctx
            .hazards
            .pointer("/other/seaLevelRise/exposed")
            .and_then(|v| v.as_bool())
            == Some(true)
        || ctx
            .hazards
            .pointer("/flood/zone")
            .and_then(|v| v.as_str())
            .map(|z| z.starts_with('V'))
            .unwrap_or(false)
        || (st == "CA"
            && ctx
                .lon
                .map(|lo| {
                    lo < -122.35 || (ctx.lat.map(|la| la < 34.4).unwrap_or(false) && lo < -118.2)
                })
                .unwrap_or(false));
    let wildfire_ember = matches!(
        st,
        "CA" | "OR" | "WA" | "CO" | "MT" | "ID" | "NV" | "AZ" | "NM" | "UT"
    ) || ctx
        .hazards
        .pointer("/wildfire/california/inFhsz")
        .and_then(|v| v.as_bool())
        == Some(true)
        || ctx
            .hazards
            .pointer("/wildfire/whp/class")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 3;

    let mut climate_flags: Vec<String> = Vec::new();
    let mut seasonal_tasks: Vec<Value> = Vec::new();
    if extreme_heat {
        climate_flags.push("EXTREME_HEAT_COOLING_STRESS".into());
        seasonal_tasks.push(json!({
            "flag": "Extreme Heat Readiness",
            "season": "Spring / Pre-Summer",
            "task": "Clean HVAC condenser coils, verify refrigerant subcooling & capacitor health, and audit attic R-38 insulation / radiant barrier to prevent peak-summer compressor failure.",
        }));
    }
    if hard_freeze {
        climate_flags.push("HARD_FREEZE_PIPE_BURST".into());
        seasonal_tasks.push(json!({
            "flag": "Hard Freeze & Pipe-Burst Protection",
            "season": "Late Fall / Pre-Winter",
            "task": "Disconnect exterior hoses, insulate crawlspace/garage/attic water lines, install frost-free sillcocks or smart automatic water shutoff valve, and test sump backup/roof ice-dam flashing.",
        }));
    }
    if coastal_salt {
        climate_flags.push("COASTAL_SALT_AIR_CORROSION".into());
        seasonal_tasks.push(json!({
            "flag": "Coastal Salt-Air & Wind Readiness",
            "season": "Semi-Annual",
            "task": "Rinse HVAC condenser fins with fresh water, use 316 marine-grade stainless exterior fasteners, inspect stucco/flashing corrosion, and verify wind-rated garage door bracing.",
        }));
    }
    if wildfire_ember {
        climate_flags.push("WILDFIRE_EMBER_DEFENSIBLE_SPACE".into());
        seasonal_tasks.push(json!({
            "flag": "Wildfire Smoke & Ember Intrusion",
            "season": "Spring / Fire Season",
            "task": "Install 1/8-inch or 1/16-inch corrosion-resistant metal ember mesh on attic/gable/crawl vents, maintain 0–5 ft Zone-0 non-combustible perimeter, and stock MERV-13 HVAC filters.",
        }));
    }

    json!({
        "homesteadExemption": homestead,
        "day1To30Calendar": calendar,
        "climateMaintenance": {
            "flags": climate_flags,
            "extremeHeatRisk": extreme_heat,
            "hardFreezePipeRisk": hard_freeze,
            "coastalSaltAirRisk": coastal_salt,
            "wildfireEmberRisk": wildfire_ember,
            "seasonalTasks": seasonal_tasks,
        },
        "disclaimer": BUYER_DILIGENCE_DISCLAIMER,
    })
}

// ------------------------- Public Orchestration Entry Points -------------------------

fn build_bundle_with_live(
    report: &Value,
    live_bb_opt: Option<&LiveBroadbandCheck>,
    live_util_opt: Option<&LiveUtilityBundle>,
    nonprofit_filings: &[Value],
) -> BuyerDiligenceBundle {
    let ctx = extract_context(report);
    let hoa_profile = build_hoa_profile(&ctx, nonprofit_filings);
    let hoa_monthly = val_f64(hoa_profile.get("hoaMonthly")).unwrap_or(0.0);
    let has_hoa = hoa_profile
        .get("hasHoa")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_hoa_water_trash = hoa_profile
        .pointer("/coverageBreakdown/utilitiesAndMaintenanceIncluded/included")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|x| {
                let s = x.as_str().unwrap_or("").to_ascii_lowercase();
                s.contains("water") || s.contains("trash") || s.contains("sewer")
            })
        })
        .unwrap_or(false);

    let (utilities_and_mandates, is_septic, is_well) =
        build_utilities_and_mandates(&ctx, live_util_opt);
    let cash_to_close = build_cash_to_close(&ctx, hoa_monthly);
    let ongoing_costs = build_ongoing_costs(
        &ctx,
        hoa_monthly,
        is_septic,
        is_well,
        has_hoa_water_trash,
        live_util_opt,
    );
    let connectivity = build_connectivity(&ctx, live_bb_opt);
    let neighborhood_pipeline = build_neighborhood_pipeline(&ctx);

    let special_dist_ann =
        val_f64(ongoing_costs.pointer("/specialTaxDistricts/estimatedAnnualSpecialTaxUsd"))
            .unwrap_or(0.0);
    let seller_tt =
        val_f64(cash_to_close.pointer("/transferTax/sellerEstimatedShareUsd")).unwrap_or(0.0);
    let buyer_closing_20 = cash_to_close
        .get("scenarios")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| val_f64(s.get("totalClosingCostsUsd")))
        .unwrap_or(ctx.price * 0.02);
    let exit_and_break_even = build_exit_and_breakeven(
        &ctx,
        hoa_monthly,
        special_dist_ann,
        seller_tt,
        buyer_closing_20,
    );
    let post_closing_and_climate = build_post_closing_and_climate(&ctx, has_hoa);

    let mut sources: Vec<SourceItem> = Vec::new();
    if let Some(lu) = live_util_opt {
        for s in &lu.sources {
            if !sources.iter().any(|e| e.url == s.url) {
                sources.push(s.clone());
            }
        }
    }
    for (src_ptr, url_ptr, verified_ptr, fallback_label) in [
        (
            "/monthlyUtilities/electricCarrier/rateSource",
            "/monthlyUtilities/electricCarrier/tariffUrl",
            "/monthlyUtilities/electricCarrier/rateVerified",
            "Published Electric Utility Tariff Schedule",
        ),
        (
            "/monthlyUtilities/gasCarrier/rateSource",
            "/monthlyUtilities/gasCarrier/tariffUrl",
            "/monthlyUtilities/gasCarrier/rateVerified",
            "Published Natural Gas Utility Tariff Schedule",
        ),
        (
            "/monthlyUtilities/waterSewerTrash/waterCarrierName",
            "/monthlyUtilities/waterSewerTrash/waterTariffUrl",
            "/monthlyUtilities/waterSewerTrash/waterVerified",
            "Published Municipal Water Utility Schedule",
        ),
    ] {
        let is_ver = ongoing_costs
            .pointer(verified_ptr)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !is_ver {
            continue;
        }
        if let Some(url) = ongoing_costs
            .pointer(url_ptr)
            .and_then(|v| v.as_str())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        {
            let name = ongoing_costs
                .pointer(src_ptr)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(fallback_label);
            if !sources.iter().any(|s: &SourceItem| s.url == url) {
                sources.push(SourceItem::new(name, url));
            }
        }
    }
    if let Some(bb) = live_bb_opt {
        if bb.fcc_fixed.status == FccLookupStatus::LiveVerified {
            sources.push(SourceItem::new(
                bb.fcc_fixed.source_name.clone(),
                bb.fcc_fixed.source_url.clone(),
            ));
        }
        for s in &bb.confirmed_sources {
            if !sources.iter().any(|e| e.url == s.url) {
                sources.push(s.clone());
            }
        }
    }
    if !nonprofit_filings.is_empty() {
        let url = nonprofit_filings
            .first()
            .and_then(|f| f.get("sourceUrl"))
            .and_then(|v| v.as_str())
            .unwrap_or("https://projects.propublica.org/nonprofits/");
        sources.push(SourceItem::new(
            "ProPublica Nonprofit Explorer (IRS Form 990 HOA Filings)",
            url,
        ));
    }

    let buyer_diligence = json!({
        "cashToClose": cash_to_close,
        "ongoingCosts": ongoing_costs,
        "connectivity": connectivity,
        "utilitiesAndMandates": utilities_and_mandates,
        "neighborhoodPipeline": neighborhood_pipeline,
        "exitAndBreakEven": exit_and_break_even,
        "postClosingAndClimate": post_closing_and_climate,
        "compliance": {
            "fairHousingNote": "Describes physical property, financing, utility, tax, and covenant facts only — never evaluates protected classes or resident demographics.",
            "disclaimer": BUYER_DILIGENCE_DISCLAIMER,
        },
        "disclaimer": BUYER_DILIGENCE_DISCLAIMER,
    });

    BuyerDiligenceBundle {
        hoa_profile,
        buyer_diligence,
        sources,
    }
}

/// Purely offline, deterministic HOA Profile + 7-module Buyer Diligence builder.
pub fn build_buyer_diligence_and_hoa_deterministic(report: &Value) -> BuyerDiligenceBundle {
    build_bundle_with_live(report, None, None, &[])
}

fn boxed_check_broadband<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    bb_query: &'a crate::extensions::proto::ExtensionQueryProto,
    lat: Option<f64>,
    lon: Option<f64>,
    enabled: bool,
) -> futures::future::BoxFuture<'a, Option<LiveBroadbandCheck>> {
    async move {
        if !enabled {
            return None;
        }
        let fcc_fixed = check_fcc_fixed_broadband(http, env, lat, lon).await;
        let envs = crate::extensions::ExtensionHub::global()
            .execute_domain(
                crate::extensions::proto::CapabilityDomain::DomainBroadband,
                bb_query,
                http,
                env,
            )
            .await;
        let mut confirmed_options = Vec::new();
        let mut confirmed_sources: Vec<SourceItem> = Vec::new();
        let mut collect_opt =
            |opt: &crate::extensions::proto::BroadbandOptionProto,
             attr: Option<&crate::extensions::proto::SourceAttributionProto>| {
                if matches!(
                    opt.availability_status.as_str(),
                    "confirmed_address" | "address_verified"
                ) {
                    if let Some(a) = attr {
                        if !a.source_url.trim().is_empty()
                            && !confirmed_sources.iter().any(|s| s.url == a.source_url)
                        {
                            confirmed_sources
                                .push(SourceItem::new(a.source_name.clone(), a.source_url.clone()));
                        }
                    }
                    confirmed_options.push(opt.clone());
                }
            };
        for env_msg in &envs {
            let attr = env_msg.attribution.as_ref();
            match &env_msg.payload {
                Some(
                    crate::extensions::proto::extension_envelope_proto::Payload::BroadbandOption(
                        opt,
                    ),
                ) => collect_opt(opt, attr),
                Some(
                    crate::extensions::proto::extension_envelope_proto::Payload::BroadbandCatalog(
                        cat,
                    ),
                ) => {
                    for opt in cat
                        .fiber_carriers
                        .iter()
                        .chain(&cat.cable_carriers)
                        .chain(&cat.wireless_carriers)
                    {
                        collect_opt(opt, attr);
                    }
                }
                _ => {}
            }
        }
        Some(LiveBroadbandCheck {
            fcc_fixed,
            confirmed_options,
            confirmed_sources,
        })
    }
    .boxed()
}

fn boxed_resolve_live_utilities<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    db: Option<&'a crate::db::DbStore>,
    live_util_query: &'a LiveUtilityQuery,
    enabled: bool,
) -> futures::future::BoxFuture<'a, Option<LiveUtilityBundle>> {
    async move {
        if enabled {
            Some(
                resolve_live_utility_and_climate_bundle_with_db(http, env, db, live_util_query)
                    .await,
            )
        } else {
            None
        }
    }
    .boxed()
}

fn boxed_persist_rates_and_tax<'a, 'r: 'a>(
    db: &'a crate::db::DbStore,
    ctx: &'a ReportContext<'r>,
    bundle: &'a BuyerDiligenceBundle,
) -> futures::future::BoxFuture<'a, ()> {
    async move {
        let special = bundle
            .buyer_diligence
            .pointer("/ongoingCosts/specialTaxDistricts");
        let has_special = special
            .and_then(|s| s.get("hasConfirmedOrSuspectedDistrict"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let special_label = special
            .and_then(|s| s.get("flags"))
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                let items: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
                (!items.is_empty()).then(|| items.join("; "))
            });
        let mandates = bundle
            .buyer_diligence
            .pointer("/utilitiesAndMandates/pointOfSaleMandates/mandates")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("title").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let eff_tax_rate = if ctx.price > 0.0 && ctx.annual_tax > 0.0 {
            Some(ctx.annual_tax / ctx.price)
        } else {
            None
        };
        let input = RatesAndCostsPersistInput {
            state: ctx.state.clone(),
            county: ctx.county.clone(),
            county_fips: ctx.county_fips.clone(),
            city: ctx.city.clone(),
            zip: ctx.zip.clone(),
            purchase_price: ctx.price,
            effective_property_tax_rate: eff_tax_rate,
            tra_code: val_str(ctx.facts.pointer("/taxRateArea")).map(String::from),
            has_special_tax_district: has_special,
            special_tax_district_label: special_label,
            point_of_sale_mandates: mandates,
            annual_insurance_usd: (ctx.annual_ins > 0.0).then_some(ctx.annual_ins),
            ..Default::default()
        };
        let _ = persist_rates_and_tax_datasets(db, &input).await;
    }
    .boxed()
}

/// Async orchestrator entry point: concurrently queries live utility territory/tariff/climate
/// endpoints (`live_tariffs`), live FCC BDC + address-verified `ExtensionHub` broadband, and
/// city-matched ProPublica Nonprofit Explorer IRS Form 990 HOA filings with strict timeouts.
pub async fn run_buyer_diligence_and_hoa(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
) -> BuyerDiligenceBundle {
    async move {
        let ctx = extract_context(report);
        let assoc_candidates = extract_hoa_association_candidates(&ctx);
        let lat = ctx.lat;
        let lon = ctx.lon;
        let state = ctx.state.clone();
        let city = ctx.city.clone();

        let full_address = report
            .get("address")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("{}, {}, {} {}", ctx.street, ctx.city, ctx.state, ctx.zip));
        let bb_query = crate::extensions::proto::ExtensionQueryProto {
            address: full_address,
            street: ctx.street.clone(),
            city: ctx.city.clone(),
            state: ctx.state.clone(),
            zip: ctx.zip.clone(),
            county_fips: ctx.county_fips.clone(),
            county_name: (!ctx.county.is_empty()).then(|| ctx.county.clone()),
            lat,
            lon,
            ..Default::default()
        };

        let water_prim = ctx
            .environment
            .get("water")
            .filter(|w| w.is_object())
            .and_then(|w| w.get("primary"))
            .filter(|p| {
                let nm = val_str(p.get("name")).unwrap_or("");
                let pid = val_str(p.get("pwsid"));
                !is_wholesaler_water_system(nm, pid)
            });
        let gov_kwh_bench = val_f64(ctx.market.pointer("/govEnergyBenchmark/electricityPerKwh"));
        let ptype_lc = ctx.property_type.to_ascii_lowercase();
        let is_condo_attached = ptype_lc.contains("condo")
            || ptype_lc.contains("town")
            || ptype_lc.contains("co-op")
            || ptype_lc.contains("attached");
        let well_explicit = RE_WELL.is_match(&ctx.combined_text);
        let septic_explicit = RE_SEPTIC.is_match(&ctx.combined_text);
        let explicit_hoa_mo = extract_hoa_monthly(&ctx).unwrap_or(0.0);
        let hoa_covers_wt = is_condo_attached && explicit_hoa_mo >= 300.0 && {
            let low = ctx.combined_text.to_ascii_lowercase();
            low.contains("water") && low.contains("trash")
        };

        let live_util_query = LiveUtilityQuery {
            state: state.clone(),
            county_fips: ctx.county_fips.clone(),
            city: city.clone(),
            zip: ctx.zip.clone(),
            lat,
            lon,
            sdwis_water_name: water_prim
                .and_then(|p| val_str(p.get("name")))
                .map(String::from),
            sdwis_pwsid: water_prim
                .and_then(|p| val_str(p.get("pwsid")))
                .map(String::from),
            gov_kwh_benchmark: gov_kwh_bench,
            is_private_well: well_explicit,
            is_private_septic: septic_explicit,
            hoa_covers_water_trash: hoa_covers_wt,
            is_condo_or_attached: is_condo_attached,
        };

        let hub = crate::extensions::ExtensionHub::global();
        let active_db = hub.active_db();
        let util_bb_on = hub.is_module_enabled("utilities_broadband");

        let bb_fut = boxed_check_broadband(http, env, &bb_query, lat, lon, util_bb_on);
        let live_util_fut = boxed_resolve_live_utilities(
            http,
            env,
            active_db.as_ref(),
            &live_util_query,
            util_bb_on,
        );
        let hoa_990_fut = fetch_hoa_nonprofit_filings(http, env, &assoc_candidates, &state, &city);

        let (live_bb_opt, live_util_opt, nonprofit_filings) =
            tokio::join!(bb_fut, live_util_fut, hoa_990_fut);

        let bundle = build_bundle_with_live(
            report,
            live_bb_opt.as_ref(),
            live_util_opt.as_ref(),
            &nonprofit_filings,
        );
        if let Some(db) = active_db.as_ref() {
            boxed_persist_rates_and_tax(db, &ctx, &bundle).await;
        }
        bundle
    }
    .boxed()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_campbell_townhome(hoa_monthly: i64, include_solar: bool) -> Value {
        let mut rep = json!({
            "geo": {
                "city": "Campbell",
                "place": "Campbell city",
                "state": "CA",
                "county": "Santa Clara County",
                "countyFips": "06085",
                "zip": "95008",
                "street": "126 Synthway01 Ave",
                "lat": 37.28233,
                "lon": -121.96172
            },
            "facts": {
                "address": "126 Synthway01 Ave, Campbell, CA 95008",
                "propertyType": "Townhouse",
                "sqft": 1877,
                "lotSqft": 1500,
                "yearBuilt": 2025,
                "hoaMonthly": hoa_monthly,
                "comps": [
                    { "address": "420 Synthcomp01 Ave", "hoaMonthly": 474 },
                    { "address": "279 Synthcomp02 Way", "hoaMonthly": 400 },
                    { "address": "422 Synthcomp03 Ave", "hoaMonthly": 650 },
                    { "address": "503 Synthcomp04 Cir", "hoaMonthly": 425 },
                    { "address": "616 Synthcomp05 Way", "hoaMonthly": 334 },
                    { "address": "433 Synthcomp06 Ave", "hoaMonthly": 525 }
                ],
                "countyRawAttributes": {
                    "SITUSFULL": "341 Synthother01 Dr",
                    "RevComments": "SYNTHSUBDIVISION"
                }
            },
            "modules": {
                "valuation": { "data": { "estimate": { "value": 1712055 }, "ownership": { "rate30": 6.25 } } },
                "environment": { "data": { "water": { "primary": { "pwsid": "CA4310027", "name": "SANTA CLARA VALLEY WATER DISTRICT" } } } }
            }
        });
        if include_solar {
            rep["facts"]["buildingPermits"] = json!([
                {
                    "permitNumber": "BLD-2026-694",
                    "category": "solar",
                    "type": "Building Division",
                    "description": "Roof Mounted PV System 2.870 kW (7 Panels)"
                }
            ]);
        }
        rep
    }

    #[test]
    fn test_local_comps_hoa_benchmark_and_undisclosed_dues_allocation() {
        let rep = sample_campbell_townhome(450, false);
        let ctx = extract_context(&rep);
        // Census "Campbell city" should normalize to clean "Campbell"
        assert_eq!(ctx.city, "Campbell");
        // Mismatched SITUSFULL parcel ("341 Synthother01 Dr" / "SYNTHSUBDIVISION") must not leak into candidates
        assert!(extract_hoa_association_candidates(&ctx).is_empty());

        let out = build_buyer_diligence_and_hoa_deterministic(&rep);
        let hp = &out.hoa_profile;
        assert_eq!(hp["hasHoa"], json!(true));
        // Real median of [334, 400, 425, 474, 525, 650] = (425 + 474) / 2 = 449.5 -> 450
        assert_eq!(
            hp.pointer("/feeVsRegionalBenchmark/benchmarkSource"),
            Some(&json!("local_mls_comps"))
        );
        assert_eq!(
            hp.pointer("/feeVsRegionalBenchmark/compSampleCount"),
            Some(&json!(6))
        );
        assert_eq!(
            hp.pointer("/feeVsRegionalBenchmark/regionalBenchmarkMonthly"),
            Some(&json!(450))
        );

        // Undisclosed HOA budgets must NOT invent 20/24/42/14% dollar shares or conflicting master insurance
        assert_eq!(
            hp.pointer("/fundingAndBoard/estimatedDuesAllocation/disclosed"),
            Some(&json!(false))
        );
        assert_eq!(
            hp.pointer("/fundingAndBoard/estimatedDuesAllocation/reservesPct"),
            Some(&Value::Null)
        );
        assert_eq!(
            hp.pointer("/fundingAndBoard/estimatedDuesAllocation/masterInsuranceMonthlyUsd"),
            Some(&Value::Null)
        );
        assert_eq!(
            hp.pointer("/fundingAndBoard/reserveStatus/reserveAllocationPct"),
            Some(&Value::Null)
        );
        assert_eq!(
            hp.pointer("/masterInsurance/estimatedMasterInsuranceCost/disclosed"),
            Some(&json!(false))
        );
        assert_eq!(
            hp.pointer("/masterInsurance/estimatedMasterInsuranceCost/annualPerUnitUSD"),
            Some(&Value::Null)
        );
    }

    #[test]
    fn test_zero_hoa_attached_townhome_not_forced_true() {
        let rep = sample_campbell_townhome(0, false);
        let out = build_buyer_diligence_and_hoa_deterministic(&rep);
        assert_eq!(out.hoa_profile["hasHoa"], json!(false));
    }

    #[test]
    fn test_solar_pv_offset_physical_gas_and_honest_connectivity() {
        let no_solar =
            build_buyer_diligence_and_hoa_deterministic(&sample_campbell_townhome(450, false));
        let with_solar =
            build_buyer_diligence_and_hoa_deterministic(&sample_campbell_townhome(450, true));

        let kwh_no = no_solar
            .buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/estimatedMonthlyKwh")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let kwh_yes = with_solar
            .buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/estimatedMonthlyKwh")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let offset = with_solar
            .buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/estimatedSolarOffsetKwh")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        assert!(
            offset > 200,
            "2.870 kW PV must yield meaningful offset, got {offset}"
        );
        assert!(
            kwh_yes < kwh_no,
            "solar must reduce net grid kWh: with={kwh_yes} without={kwh_no}"
        );

        // Sub-0.75ac parcel with empty retail water (wholesaler filtered out) must NOT cry Private Well
        assert_eq!(
            with_solar
                .buyer_diligence
                .pointer("/utilitiesAndMandates/waterSource/isLikelyPrivateWell"),
            Some(&json!(false))
        );
        // Unverified offline connectivity must NEVER claim fiberLikely=true or confirmed address fiber
        assert_eq!(
            with_solar
                .buyer_diligence
                .pointer("/connectivity/fiberLikely"),
            Some(&Value::Null)
        );
        assert_eq!(
            with_solar
                .buyer_diligence
                .pointer("/connectivity/fiberConfirmedAddress"),
            Some(&json!(false))
        );
        assert_eq!(
            with_solar
                .buyer_diligence
                .pointer("/connectivity/broadbandCatalog/fiberAvailable"),
            Some(&json!(false))
        );

        let mut no_sqft_solar_rep = sample_campbell_townhome(450, true);
        if let Some(obj) = no_sqft_solar_rep
            .get_mut("facts")
            .and_then(Value::as_object_mut)
        {
            obj.insert("livingAreaSqFt".to_string(), Value::Null);
            obj.insert("sqft".to_string(), Value::Null);
        }
        let no_sqft_out = build_buyer_diligence_and_hoa_deterministic(&no_sqft_solar_rep);
        let standalone_offset = no_sqft_out
            .buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/estimatedSolarOffsetKwh")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        assert!(
            standalone_offset > 200,
            "2.870 kW PV offset must still surface without parcel sqft, got {standalone_offset}"
        );
        assert_eq!(
            no_sqft_out
                .buyer_diligence
                .pointer("/ongoingCosts/monthlyUtilities/grossMonthlyKwh"),
            Some(&Value::Null)
        );
    }
}
