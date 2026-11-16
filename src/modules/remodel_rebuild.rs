// Regionally calibrated Homeowner Remodel, Tear-Down / House Rebuilding & Lawn / Exterior Quotes
//
// Grounded in public government and industry cost benchmarks:
// 1. U.S. Census Bureau — Survey of Construction (SOC) Characteristics of New Housing (regional $/sq ft)
// 2. NAHB — Cost of Constructing a Home Survey (empirical 8-stage construction cost breakdown & soft-cost share)
// 3. Zonda / Journal of Light Construction (JLC) Cost vs. Value Report (regional remodel costs & resale recoup %)
// 4. U.S. Bureau of Labor Statistics (BLS) — Producer Price Index for Inputs to Residential Construction
//    (FRED WPUSI012011) & Construction Average Hourly Earnings (FRED CES2000000003)
// 5. Municipal & Regional Water Agency Turf-Replacement Rebate Schedules (CA, NV, AZ, UT, CO, TX, NM)
//
// Planning benchmarks only — never a contractor bid or appraisal; homeowners should obtain 3 licensed local bids.

use crate::registry::rates_and_costs::{state_cost_multiplier, COUNTY_COST_MULT};
use crate::registry::us_geo::census_division_and_region;
use crate::shared::{round_n, ModuleOutput, SourceItem};
use serde_json::{json, Value};

fn state_cost_and_division(state: &str) -> (f64, &'static str, &'static str) {
    let (div, reg) = census_division_and_region(state);
    (state_cost_multiplier(state), div, reg)
}

fn round_100(v: f64) -> i64 {
    ((v / 100.0).round() as i64) * 100
}

fn round_50(v: f64) -> i64 {
    ((v / 50.0).round() as i64) * 50
}

fn round_1000(v: f64) -> i64 {
    ((v / 1000.0).round() as i64) * 1000
}

fn division_recoup_factor(division: &str) -> f64 {
    match division {
        "Pacific" => 1.12,
        "New England" | "Middle Atlantic" => 1.06,
        "Mountain" | "South Atlantic" => 1.02,
        "West South Central" | "East South Central" => 0.96,
        "East North Central" | "West North Central" => 0.94,
        _ => 1.00,
    }
}

pub(crate) fn county_or_state_cost_context(
    state: &str,
    fips: &str,
    city_name: &str,
) -> (f64, String, &'static str, &'static str) {
    let (state_mult, census_division, census_region) = state_cost_and_division(state);
    let (geo_mult, area_label) = if let Some((cm, lbl)) = COUNTY_COST_MULT.get(fips) {
        (*cm, (*lbl).to_string())
    } else if !city_name.is_empty() && !state.is_empty() {
        (
            state_mult,
            format!("{city_name}, {state} ({census_division})"),
        )
    } else if !state.is_empty() {
        (state_mult, format!("{state} Statewide ({census_division})"))
    } else {
        (1.00, "U.S. National Average".to_string())
    };
    (geo_mult, area_label, census_division, census_region)
}

#[allow(dead_code)]
pub(crate) fn build_turf_rebate_info(state: &str, fips: &str, est_lawn_sqft: i64) -> Value {
    build_turf_rebate_info_opt(state, fips, Some(est_lawn_sqft))
}

pub(crate) fn build_turf_rebate_info_opt(
    state: &str,
    fips: &str,
    est_lawn_sqft: Option<i64>,
) -> Value {
    let (eligible, agency, rate_sqft, max_cap, url, rule_note): (
        bool,
        &str,
        f64,
        i64,
        &str,
        &str,
    ) = match (state, fips) {
        ("CA", "06085") => (
            true,
            "Santa Clara Valley Water District (Valley Water) Landscape Rebate Program",
            2.00,
            3_000,
            "https://www.valleywater.org/saving-water/rebates-surveys/landscape-rebates",
            "Base $2.00/sq ft (up to $3.00–$4.00/sq ft in cost-sharing cities like Palo Alto, San Jose, Santa Clara & Cupertino); pre-inspection required BEFORE removing existing lawn.",
        ),
        ("CA", "06075" | "06081") => (
            true,
            "SFPUC / BAWSCA Lawn Be Gone! Rebate Program",
            2.50,
            4_000,
            "https://bawsca.org/conserve/rebates/lawn-be-gone",
            "Typically $2.00–$4.00/sq ft depending on member water agency for replacing irrigated turf with drought-tolerant landscaping; pre-approval required before turf removal.",
        ),
        ("CA", "06001" | "06013") => (
            true,
            "EBMUD / Contra Costa Water District Lawn Conversion Rebate",
            2.00,
            2_500,
            "https://www.ebmud.com/water/conservation-and-rebates/rebates/lawn-conversion-rebate",
            "Up to $2.00/sq ft for sheet-mulching conversion from irrigated lawn to low-water plants & drip irrigation; pre-inspection mandatory.",
        ),
        ("CA", "06037" | "06059" | "06073" | "06111" | "06065" | "06071") => (
            true,
            "Metropolitan Water District (SoCal WaterSmart) Turf Replacement Program",
            2.50,
            10_000,
            "https://socalwatersmart.com/en/residential/rebates/available-rebates/turf-replacement-program/",
            "$2.00/sq ft regional base plus $1.00–$2.00/sq ft local agency add-ons up to 5,000 sq ft; requires stormwater retention feature & native/climate-appropriate plants.",
        ),
        ("CA", _) => (
            true,
            "California Save Our Water / Local Urban Water Supplier Turf Rebate",
            2.00,
            3_000,
            "https://saveourwater.com/",
            "Most California retail water agencies offer $1.50–$3.00/sq ft for converting live irrigated grass to drip-irrigated California-friendly landscaping.",
        ),
        ("NV", "32003") => (
            true,
            "Southern Nevada Water Authority (SNWA) Water Smart Landscapes",
            3.50,
            15_000,
            "https://www.snwa.com/rebates/wsl/index.html",
            "Up to $5.00/sq ft for the first 10,000 sq ft ($2.50/sq ft thereafter) converting grass to drip-irrigated desert landscaping; pre-conversion inspection mandatory.",
        ),
        ("AZ", "04013" | "04019") => (
            true,
            "Arizona Municipal Xeriscape / Grass-to-Xeriscape Rebate Programs",
            2.00,
            3_000,
            "https://www.amwua.org/what-you-can-do/landscape-and-garden",
            "Valley cities (Scottsdale, Chandler, Gilbert, Mesa, Tempe, Phoenix, Tucson) offer ~$1.50–$3.00/sq ft grass-removal incentives; verify city utility enrollment.",
        ),
        ("UT", _) => (
            true,
            "Utah Water Savers Landscape Incentive Program",
            2.50,
            5_000,
            "https://utahwatersavers.com/",
            "Up to $2.00–$3.00/sq ft in participating Utah districts for replacing lawn with drip-irrigated water-wise landscaping; do not kill or remove grass before pre-inspection.",
        ),
        ("CO", _) => (
            true,
            "Colorado Turf Replacement Program / Local Utility Cash-for-Grass",
            2.00,
            3_000,
            "https://cwcb.colorado.gov/",
            "Front Range utilities (Aurora, Boulder, Denver/Resource Central, Castle Rock, Fort Collins) offer $1.50–$3.00/sq ft for replacing Kentucky bluegrass with water-wise gardens.",
        ),
        ("TX", "48453" | "48029") => (
            true,
            "Austin Water WaterWise / SAWS WaterSaver Landscape Rebate",
            1.75,
            3_000,
            "https://www.austintexas.gov/department/waterwise-landscape-rebate",
            "Participating Central Texas water utilities offer rebates (~$1.75/sq ft or coupon packages) for converting healthy irrigated turfgrass to native beds.",
        ),
        ("NM", _) => (
            true,
            "Albuquerque Bernalillo County Water Authority / Santa Fe Xeriscape Rebate",
            2.00,
            3_000,
            "https://www.abcwua.org/conservation-rebates-xeriscape-rebates/",
            "Local New Mexico water utilities offer $1.50–$2.00/sq ft rebates for converting high-water turf to low-water xeriscape.",
        ),
        _ => (
            false,
            "Check Local Municipal Water Utility Conservation Incentives",
            0.0,
            0,
            "https://www.epa.gov/watersense/rebate-finder",
            "Check the EPA WaterSense Rebate Finder for local smart-controller, rain-sensor, or rain-garden rebates.",
        ),
    };

    let est_rebate = match est_lawn_sqft {
        Some(lawn_sq) => {
            let raw_rebate = if eligible && lawn_sq > 0 {
                ((lawn_sq as f64) * rate_sqft).round() as i64
            } else {
                0
            };
            json!(raw_rebate.min(max_cap))
        }
        None => {
            if eligible {
                Value::Null
            } else {
                json!(0)
            }
        }
    };

    json!({
        "eligibleRegion": eligible,
        "agencyName": agency,
        "rebatePerSqftUsd": round_n(rate_sqft, 2),
        "maxCapUsd": max_cap,
        "estimatedRebateUsd": est_rebate,
        "url": url,
        "note": rule_note,
    })
}

/// Builds the complete `remodelAndRebuild` module from a Open Estate report JSON.
pub fn build_remodel_and_rebuild(report: &Value) -> ModuleOutput {
    let empty = Value::Null;
    let facts = report.get("facts").unwrap_or(&empty);
    let geo = report.get("geo").unwrap_or(&empty);
    let valuation = report
        .pointer("/modules/valuation/data")
        .or_else(|| report.get("valuation"))
        .unwrap_or(&empty);
    let market = report
        .pointer("/modules/market/data")
        .or_else(|| report.get("market"))
        .unwrap_or(&empty);
    let hazards = report
        .pointer("/modules/hazards/data")
        .or_else(|| report.get("hazards"))
        .unwrap_or(&empty);
    let zoning_block = report
        .pointer("/permitsAndCondition/zoning")
        .or_else(|| report.get("zoning"))
        .unwrap_or(&empty);

    let state = geo
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = geo
        .get("county")
        .and_then(|v| v.as_str())
        .unwrap_or("Local County");
    let city_name = geo.get("city").and_then(|v| v.as_str()).unwrap_or("");

    let (geo_mult, area_label, census_division, census_region) =
        county_or_state_cost_context(&state, fips, city_name);

    let bls_factor = market
        .pointer("/constructionInflation/costInflationFactor")
        .and_then(|v| v.as_f64())
        .filter(|x| x.is_finite() && *x > 0.7 && *x < 1.5)
        .unwrap_or(1.00);
    let bls_ppi_yoy = market
        .pointer("/constructionInflation/ppiYoYPct")
        .and_then(|v| v.as_f64());
    let bls_wage_usd = market
        .pointer("/constructionInflation/avgHourlyWageUsd")
        .and_then(|v| v.as_f64());

    let regional_mult = round_n((geo_mult * bls_factor).clamp(0.82, 1.85), 3);
    let recoup_mult = division_recoup_factor(census_division);

    let pt_str = facts
        .get("propertyType")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let pt_lc = pt_str.to_lowercase();
    let is_attached_condo =
        pt_lc.contains("condo") || pt_lc.contains("co-op") || pt_lc.contains("apart");
    let is_townhome = pt_lc.contains("town");

    let raw_sqft = facts
        .get("sqft")
        .and_then(|v| v.as_f64())
        .filter(|&s| s.is_finite() && s > 0.0);

    let stories_opt = facts
        .get("stories")
        .and_then(|v| v.as_f64())
        .filter(|&s| s.is_finite() && (1.0..=10.0).contains(&s));

    let year_built = facts
        .get("yearBuilt")
        .and_then(|v| v.as_i64())
        .filter(|&y| (1700..=2035).contains(&y));
    let is_pre_1978 = year_built.map(|y| y < 1978).unwrap_or(false);

    let raw_lot = facts
        .get("lotSqft")
        .or_else(|| facts.get("landSqft"))
        .and_then(|v| v.as_f64())
        .filter(|&l| l.is_finite() && l > 0.0)
        .or_else(|| {
            facts
                .get("lotAcres")
                .or_else(|| facts.get("acres"))
                .and_then(|v| v.as_f64())
                .filter(|&a| a.is_finite() && a > 0.0)
                .map(|a| a * 43_560.0)
        });

    let explicit_footprint_sqft = facts
        .get("buildingFootprintSqft")
        .and_then(|v| v.as_f64())
        .filter(|&fp| fp.is_finite() && fp > 0.0);

    let est_market_val = valuation
        .pointer("/estimate/value")
        .and_then(|v| v.as_f64())
        .or_else(|| facts.get("listPrice").and_then(|v| v.as_f64()))
        .filter(|&v| v.is_finite() && v > 25_000.0);

    let est_rent_mo_opt = valuation
        .pointer("/ownership/rentEstimate")
        .or_else(|| valuation.pointer("/rentalSummary/subjectEstRent"))
        .or_else(|| valuation.pointer("/rentResearch/recommendedMonthlyRent"))
        .or_else(|| valuation.get("monthlyRent"))
        .and_then(|v| v.as_f64())
        .filter(|&r| r.is_finite() && r > 0.0);

    let pga = hazards
        .pointer("/earthquake/designMaps/pgaM")
        .or_else(|| hazards.pointer("/earthquake/pga"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let high_wildfire = hazards
        .pointer("/wildfire/california/inFhsz")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || hazards
            .pointer("/wildfire/whp/class")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase().contains("high"))
            .unwrap_or(false);

    // Vertical hard construction $/sq ft across 3 tiers (Census SOC + NAHB regionalized)
    // U.S. national custom single-family baseline: Standard ~$192/sf, Custom ~$285/sf, Luxury ~$435/sf
    let std_hard_psf = (192.0 * regional_mult).round() as i64;
    let cus_hard_psf = (285.0 * regional_mult).round() as i64;
    let lux_hard_psf = (435.0 * regional_mult).round() as i64;

    let garage_allowance_sqft: f64 = if is_attached_condo { 0.0 } else { 440.0 };
    let soft_arch_eng_pct = if state == "CA" || state == "NY" || state == "WA" || state == "HI" {
        0.105
    } else {
        0.090
    };
    let soft_permits_impact_pct = if state == "CA" {
        0.062 // building plan check + Title 24 + school impact + utility connection fees
    } else if regional_mult >= 1.25 {
        0.052
    } else {
        0.038
    };
    let contingency_pct = 0.10;

    let sod_rate_typ = round_n(3.15 * regional_mult, 2);
    let hydro_rate_typ = round_n(1.35 * regional_mult, 2);
    let irrig_rate_typ = round_n(1.75 * regional_mult, 2);
    let xeri_rate_typ = round_n(7.60 * regional_mult, 2);
    let synturf_rate_typ = round_n(13.80 * regional_mult, 2);
    let grading_prep_rate = round_n(1.15 * regional_mult, 2);
    let fence_rate_lf = round_n(54.0 * regional_mult, 2);

    let sources = vec![
        SourceItem::new(
            "U.S. Census Bureau — Survey of Construction (SOC) Characteristics of New Housing ($/sq ft by Region)",
            "https://www.census.gov/construction/chars/",
        ),
        SourceItem::new(
            "National Association of Home Builders (NAHB) — Cost of Constructing a Home Survey",
            "https://www.nahb.org/news-and-economics/housing-economics/special-studies/cost-of-constructing-a-home",
        ),
        SourceItem::new(
            "Zonda / Journal of Light Construction (JLC) — Regional Cost vs. Value Remodeling Report",
            "https://www.jlconline.com/cost-vs-value/",
        ),
        SourceItem::new(
            "U.S. Bureau of Labor Statistics — Residential Construction Inputs PPI & Wages (FRED WPUSI012011 / CES2000000003)",
            "https://fred.stlouisfed.org/series/WPUSI012011",
        ),
    ];

    let disclaimer = "Regional remodeling, tear-down/rebuilding, and landscaping figures are planning benchmarks calibrated from U.S. Census SOC, NAHB, Zonda/JLC Cost vs. Value, and BLS construction indices. Soil conditions, hillside engineering, HOA/historic review, concealed hazards, and finish selections can shift costs by ±25%+; always obtain 3 itemized bids from licensed, insured local contractors and verify municipal zoning/permits before starting work.";

    let mut warnings = Vec::new();
    let Some(sqft) = raw_sqft else {
        warnings.push(
            "Living area (sqft) is missing from public property records; size-dependent remodel and tear-down rebuild quotes are omitted."
                .to_string(),
        );
        if !is_attached_condo && raw_lot.is_none() {
            warnings.push(
                "Parcel lot area (lotSqft) is missing from public property records; lot-scaled lawn, turf, irrigation, and perimeter fence quotes are omitted."
                    .to_string(),
            );
        }
        let turf_rebate = build_turf_rebate_info_opt(
            &state,
            fips,
            if is_attached_condo { Some(0) } else { None },
        );
        let data = json!({
            "status": "missing_required_input",
            "regionalCostIndex": {
                "areaLabel": area_label,
                "countyName": county_name,
                "countyFips": if fips.is_empty() { Value::Null } else { json!(fips) },
                "state": state,
                "censusDivision": census_division,
                "censusRegion": census_region,
                "regionalMultiplier": regional_mult,
                "nationalIndexBase100": (regional_mult * 100.0).round() as i64,
                "blsCostInflationFactor": round_n(bls_factor, 3),
                "blsMaterialsPpiYoYPct": bls_ppi_yoy,
                "blsConstructionHourlyWageUsd": bls_wage_usd,
                "resaleRecoupFactor": recoup_mult,
            },
            "propertyBasis": {
                "propertyType": if pt_str.is_empty() { Value::Null } else { json!(pt_str) },
                "isAttachedCondo": is_attached_condo,
                "isTownhome": is_townhome,
                "livingSqft": Value::Null,
                "isAssumedSqft": false,
                "stories": stories_opt,
                "yearBuilt": year_built,
                "isPre1978LeadAsbestosEra": is_pre_1978,
                "lotSqft": if is_attached_condo {
                    json!(0)
                } else {
                    json!(raw_lot.map(|l| l.round() as i64))
                },
                "isAssumedLot": false,
                "buildingFootprintSqft": explicit_footprint_sqft.map(|fp| fp.round() as i64),
                "roofAreaEstSqft": explicit_footprint_sqft.map(|fp| (fp * 1.15).round() as i64),
                "maxZoningBuildableSqft": Value::Null,
            },
            "headlineBenchmarks": {
                "customRebuildHardPerSqftUsd": cus_hard_psf,
                "tearDownPlusCustomRebuildCurrentSizeUsd": Value::Null,
                "naturalSodLawnRebuildUsd": Value::Null,
                "minorKitchenRemodelUsd": Value::Null,
            },
            "remodelProjects": [],
            "tearDownAndRebuild": {
                "status": "missing_required_input",
                "applicableForTearDown": !is_attached_condo,
                "demolitionAndSitePrep": Value::Null,
                "softCostAssumptions": {
                    "architectureEngineeringPct": round_n(soft_arch_eng_pct * 100.0, 1),
                    "permitsImpactFeesPct": round_n(soft_permits_impact_pct * 100.0, 1),
                    "contingencyPct": round_n(contingency_pct * 100.0, 1),
                    "garageAllowanceSqft": garage_allowance_sqft.round() as i64,
                    "estimatedMonthlyRentCarryUsd": est_rent_mo_opt.map(|r| r.round() as i64),
                },
                "tiers": [],
                "nahbStageBreakdown": [],
                "feasibility": {
                    "applicable": false,
                    "status": "missing_required_input",
                    "verdict": "Requires Verified Living Area",
                    "summary": "Living square footage is missing from public property records; remodel and tear-down rebuild totals are omitted rather than synthesized from an assumed house size.",
                },
            },
            "lawnAndExterior": {
                "status": "missing_required_input",
                "applicable": !is_attached_condo,
                "parcelYardBasis": {
                    "lotSqft": if is_attached_condo {
                        json!(0)
                    } else {
                        json!(raw_lot.map(|l| l.round() as i64))
                    },
                    "buildingFootprintSqft": explicit_footprint_sqft.map(|fp| fp.round() as i64),
                    "hardscapeDrivewayPatioEstSqft": Value::Null,
                    "openLandscapingSqft": Value::Null,
                    "estimatedLawnSqft": if is_attached_condo { json!(0) } else { Value::Null },
                    "calculatorDefaultSqft": Value::Null,
                    "frontYardEstSqft": Value::Null,
                    "backYardEstSqft": Value::Null,
                    "perimeterFenceLinearFt": Value::Null,
                },
                "unitRatesRegionalUsd": {
                    "naturalSodInstalledPerSqft": sod_rate_typ,
                    "hydroseedingPerSqft": hydro_rate_typ,
                    "smartIrrigationPerSqft": irrig_rate_typ,
                    "xeriscapeConversionPerSqft": xeri_rate_typ,
                    "artificialTurfPerSqft": synturf_rate_typ,
                    "oldTurfRemovalAndGradingPerSqft": grading_prep_rate,
                    "privacyFencePerLinearFt": fence_rate_lf,
                },
                "turfRebateProgram": turf_rebate,
                "quotes": [],
            },
            "readyToMoveIn": crate::modules::move_in_cost::build_ready_to_move_in_from_report(report),
            "disclaimer": disclaimer,
        });
        return ModuleOutput {
            data,
            sources,
            warnings,
        };
    };

    if !is_attached_condo && raw_lot.is_none() {
        warnings.push(
            "Parcel lot area (lotSqft) is missing from public property records; lot-scaled lawn, turf, irrigation, and perimeter fence quotes are omitted."
                .to_string(),
        );
    }

    // -------------------------------------------------------------------------
    // 1. Area Remodel Projects & Cost vs. Resale Value Recoup (JLC / Zonda + RSMeans)
    // -------------------------------------------------------------------------
    let roof_footprint_sqft_opt = if is_attached_condo {
        None
    } else if let Some(st) = stories_opt {
        Some(((sqft / st.max(1.0)) * 1.28).clamp(800.0, 5_500.0))
    } else {
        explicit_footprint_sqft.map(|fp| (fp * 1.15).clamp(800.0, 5_500.0))
    };
    let window_count = ((sqft / 160.0).round() as i64).clamp(8, 28);
    let hvac_tons = round_n((sqft / 550.0).clamp(2.0, 5.0), 1);

    let mk_remodel = |id: &str,
                      name: &str,
                      category: &str,
                      base_us_typ: f64,
                      spread_low: f64,
                      spread_high: f64,
                      base_recoup_pct: f64,
                      permit_req: bool,
                      weeks: &str,
                      scope_note: String|
     -> Value {
        let typ = round_100(base_us_typ * regional_mult);
        let low = round_100((base_us_typ * regional_mult * spread_low).max(1_200.0));
        let high = round_100(base_us_typ * regional_mult * spread_high);
        let recoup_pct = round_n((base_recoup_pct * recoup_mult).clamp(38.0, 108.0), 1);
        let resale_added = round_100((typ as f64) * (recoup_pct / 100.0));
        json!({
            "id": id,
            "name": name,
            "category": category,
            "lowUsd": low,
            "typicalUsd": typ,
            "highUsd": high,
            "resaleValueAddedUsd": resale_added,
            "costRecoupedPct": recoup_pct,
            "permitRequired": permit_req,
            "timelineWeeks": weeks,
            "scope": scope_note,
        })
    };

    let mut remodel_projects = vec![
        mk_remodel(
            "kitchen-minor",
            "Minor Kitchen Refresh (Midrange)",
            "Interior",
            28_400.0,
            0.78,
            1.30,
            88.5,
            false,
            "3–5 wks",
            "Reface/repaint cabinets, quartz countertops, tile backsplash, sink/faucet, hardware & Energy Star appliance suite (~200 sq ft kitchen).".to_string(),
        ),
        mk_remodel(
            "kitchen-major-mid",
            "Major Kitchen Remodel (Midrange)",
            "Interior",
            82_500.0,
            0.80,
            1.32,
            52.0,
            true,
            "8–12 wks",
            "New semi-custom wood cabinetry, island, stone counters, recessed LED & under-cabinet circuits, flooring & new appliances.".to_string(),
        ),
        mk_remodel(
            "kitchen-upscale",
            "Chef's Kitchen Gut Remodel (Upscale)",
            "Interior",
            158_000.0,
            0.82,
            1.40,
            42.0,
            true,
            "12–18 wks",
            "Custom inset cabinetry, structural wall opening/beam, waterfall stone slab, pro-grade range/fridge & custom hood ventilation.".to_string(),
        ),
        mk_remodel(
            "bath-midrange",
            "Guest / Hall Bathroom Remodel (Midrange)",
            "Interior",
            26_200.0,
            0.78,
            1.32,
            74.0,
            true,
            "3–5 wks",
            "5×7 ft full bath: porcelain tile tub/shower surround, waterproofing membrane, solid-wood vanity, quartz top, Toto/Kohler fixtures & fan.".to_string(),
        ),
        mk_remodel(
            "bath-primary-upscale",
            "Primary Suite Bathroom Remodel (Upscale)",
            "Interior",
            78_500.0,
            0.80,
            1.36,
            46.5,
            true,
            "6–10 wks",
            "Frameless curbless walk-in shower, freestanding soaking tub, dual custom vanity, heated tile floor & updated plumbing rough-in.".to_string(),
        ),
    ];

    if !is_attached_condo {
        if let Some(roof_footprint_sqft) = roof_footprint_sqft_opt {
            let roof_base = (roof_footprint_sqft * 8.60).clamp(11_500.0, 48_000.0);
            remodel_projects.push(mk_remodel(
                "roof-replacement",
                "Architectural Cool-Roof Replacement",
                "Exterior & Envelope",
                roof_base,
                0.82,
                1.35,
                61.0,
                true,
                "1–2 wks",
                format!(
                    "Scaled to ~{} sq ft roof area (tear-off, synthetic underlayment, flashing, ridge vents{}).",
                    roof_footprint_sqft.round() as i64,
                    if high_wildfire || state == "CA" {
                        " & Class A fire-rated assembly"
                    } else {
                        ""
                    }
                ),
            ));
        }
    }

    let win_base = (window_count as f64) * 1_180.0;
    remodel_projects.push(mk_remodel(
        "window-replacement",
        "Whole-Home Dual-Pane Low-E Windows",
        "Exterior & Envelope",
        win_base,
        0.80,
        1.34,
        69.0,
        true,
        "1–2 wks",
        format!(
            "Estimated ~{} insulated vinyl/fiberglass Low-E windows scaled to {:.0} sq ft living area.",
            window_count, sqft
        ),
    ));

    let hvac_base = 13_800.0 + hvac_tons * 1_950.0;
    remodel_projects.push(mk_remodel(
        "hvac-heat-pump",
        "Heat-Pump HVAC Electrification",
        "Systems",
        hvac_base,
        0.82,
        1.30,
        94.0,
        true,
        "1 wk",
        format!(
            "~{:.1}-ton variable-speed inverter heat pump + air handler, duct sealing & smart thermostat (before federal 25C / utility rebates).",
            hvac_tons
        ),
    ));

    remodel_projects.push(mk_remodel(
        "electrical-panel-ev",
        "200A Main Panel Upgrade + Level 2 EV Circuit",
        "Systems",
        5_600.0,
        0.75,
        1.55,
        68.0,
        true,
        "2–4 days",
        format!(
            "200-amp main breaker panel, AFCI/GFCI protection, grounding & 240V 50A EV charger circuit{}.",
            if year_built.map(|y| y < 1985).unwrap_or(false) {
                " (recommended priority on pre-1985 service panels)"
            } else {
                ""
            }
        ),
    ));

    if !is_attached_condo {
        let add_sqft = 400.0;
        let add_base = add_sqft * 295.0;
        remodel_projects.push(mk_remodel(
            "home-addition-400",
            "Primary Suite / Family Room Addition (+400 sq ft)",
            "Expansion & ADU",
            add_base,
            0.84,
            1.38,
            56.0,
            true,
            "16–24 wks",
            format!(
                "Ground-floor 400 sq ft addition (~${}/sq ft regionalized): foundation tie-in, framing, roof integration, full bath & finishes.",
                ((295.0 * regional_mult).round() as i64)
            ),
        ));

        remodel_projects.push(mk_remodel(
            "garage-adu",
            "Garage Conversion ADU (~420 sq ft Studio/1BR)",
            "Expansion & ADU",
            108_000.0,
            0.82,
            1.34,
            74.0,
            true,
            "14–20 wks",
            "Convert existing 2-car garage into permitted rentable ADU: slab vapor barrier, insulation, kitchenette, 3/4 bath, mini-split & subpanel.".to_string(),
        ));

        if !is_townhome && raw_lot.map(|l| l >= 4_500.0).unwrap_or(false) {
            remodel_projects.push(mk_remodel(
                "detached-adu",
                "Detached Backyard ADU (~650 sq ft 1BR/1BA)",
                "Expansion & ADU",
                228_000.0,
                0.84,
                1.36,
                71.0,
                true,
                "24–36 wks",
                format!(
                    "New-build 650 sq ft backyard cottage (~${}/sq ft all-in incl. foundation, sewer/water/electric trenching, architecture & permits).",
                    ((228_000.0 * regional_mult / 650.0).round() as i64)
                ),
            ));
        }

        if (pga >= 0.28 || state == "CA" || state == "WA" || state == "OR")
            && year_built.map(|y| y < 1985).unwrap_or(false)
        {
            remodel_projects.push(mk_remodel(
                "seismic-retrofit",
                "Seismic Cripple-Wall Bracing & Sill Bolting",
                "Structural",
                7_400.0,
                0.72,
                1.45,
                65.0,
                true,
                "3–6 days",
                if state == "CA" {
                    "Anchor mudsills & plywood-brace crawlspace cripple walls (CEBC Chapter A3); pre-1980 raised-foundation CA homes may qualify for $3,000 Earthquake Brace + Bolt grant.".to_string()
                } else {
                    "Seismic anchor bolting and crawlspace shear-wall bracing to reduce earthquake displacement risk.".to_string()
                },
            ));
        }
    }

    let net_floor_sqft = (sqft * 0.85).clamp(350.0, 6_500.0);
    remodel_projects.push(mk_remodel(
        "floor-engineered-hardwood",
        "Whole-Home Engineered White Oak Flooring",
        "Interior",
        650.0 + net_floor_sqft * 11.80,
        0.80,
        1.34,
        82.0,
        false,
        "1–2 wks",
        format!(
            "Scaled to ~{} sq ft net floor area (~85% of {:.0} sq ft home): old carpet/floor tear-out, subfloor leveling, moisture barrier, engineered white oak & baseboards.",
            net_floor_sqft.round() as i64,
            sqft
        ),
    ));
    remodel_projects.push(mk_remodel(
        "floor-lvp",
        "Whole-Home Waterproof Luxury Vinyl Plank (LVP)",
        "Interior",
        550.0 + net_floor_sqft * 6.85,
        0.78,
        1.30,
        78.0,
        false,
        "4–7 days",
        format!(
            "Scaled to ~{} sq ft net floor area: old flooring removal, subfloor prep, 20-mil wear-layer waterproof LVP & shoe molding.",
            net_floor_sqft.round() as i64
        ),
    ));
    remodel_projects.push(mk_remodel(
        "paint-interior",
        "Whole-Home Interior Paint Refresh (Walls, Trim & Ceilings)",
        "Interior",
        450.0 + sqft * 3.45,
        0.76,
        1.32,
        92.0,
        false,
        "4–7 days",
        format!(
            "Scaled to {:.0} sq ft living area: drywall patching, caulking, primer & 2 coats low-VOC washable architectural paint on walls, ceilings, doors & trim{}.",
            sqft,
            if is_pre_1978 {
                " (EPA Lead-Safe RRP containment applies to pre-1978 surfaces)"
            } else {
                ""
            }
        ),
    ));

    // -------------------------------------------------------------------------
    // 2. Tear-Down & Full House Rebuilding Cost in That Area
    // -------------------------------------------------------------------------
    let max_far_sqft = zoning_block
        .get("maxBuildableSqft")
        .or_else(|| zoning_block.pointer("/metrics/maxBuildableSqft"))
        .and_then(|v| v.as_f64())
        .filter(|&m| m.is_finite() && m >= sqft * 0.9)
        .unwrap_or_else(|| {
            if is_attached_condo {
                sqft
            } else if let Some(lot_sq) = raw_lot {
                if lot_sq >= 8_000.0 {
                    (lot_sq * 0.42).max(sqft).min(5_200.0)
                } else {
                    (lot_sq * 0.48).max(sqft).min(3_800.0)
                }
            } else {
                sqft
            }
        })
        .round() as i64;

    // Demolition & site-prep components (scaled to existing structure + ~420 sq ft garage)
    let demo_gross_sqft = sqft + if is_attached_condo { 0.0 } else { 420.0 };
    let demo_structure_usd = if is_attached_condo {
        0
    } else {
        round_100((demo_gross_sqft * 11.50 * regional_mult).max(13_500.0 * regional_mult))
    };
    let abatement_usd = if is_attached_condo {
        0
    } else if is_pre_1978 {
        round_100((4_800.0 + sqft * 3.60) * regional_mult)
    } else {
        round_100(1_350.0 * regional_mult) // mandatory pre-demo asbestos/AQMD lab survey even on newer homes
    };
    let utility_cap_swppp_usd = if is_attached_condo {
        0
    } else {
        round_100(6_200.0 * regional_mult)
    };
    let site_grading_prep_usd = if is_attached_condo {
        0
    } else if let Some(lot_sq) = raw_lot {
        round_100((11_500.0 + (lot_sq.min(12_000.0) * 0.85)) * regional_mult)
    } else {
        round_100(11_500.0 * regional_mult)
    };
    let total_teardown_prep_usd =
        demo_structure_usd + abatement_usd + utility_cap_swppp_usd + site_grading_prep_usd;

    let mk_rebuild_tier = |tier_id: &str,
                           label: &str,
                           hard_psf: i64,
                           build_months: i64,
                           spec_summary: &str|
     -> Value {
        let calc_for_sqft = |target_sq: f64| -> Value {
            let hard_living = target_sq * (hard_psf as f64);
            let hard_garage = garage_allowance_sqft * (hard_psf as f64) * 0.52;
            let hard_total = round_1000(hard_living + hard_garage);
            let arch_eng = round_100((hard_total as f64) * soft_arch_eng_pct);
            let permits_impact = round_100((hard_total as f64) * soft_permits_impact_pct);
            let soft_total = arch_eng + permits_impact;
            let contingency = round_100((hard_total as f64) * contingency_pct);
            let turnkey_ex_carry = total_teardown_prep_usd + hard_total + soft_total + contingency;
            let temp_housing_opt = est_rent_mo_opt.map(|r| round_100(r * (build_months as f64)));
            let turnkey_with_carry_opt = temp_housing_opt.map(|th| turnkey_ex_carry + th);
            let all_in_psf = ((turnkey_ex_carry as f64) / target_sq.max(1.0)).round() as i64;
            json!({
                "targetSqft": target_sq.round() as i64,
                "hardVerticalUsd": hard_total,
                "architectureEngineeringUsd": arch_eng,
                "permitsAndImpactFeesUsd": permits_impact,
                "softCostsTotalUsd": soft_total,
                "contingency10PctUsd": contingency,
                "tearDownAndSitePrepUsd": total_teardown_prep_usd,
                "turnkeyRebuildTotalUsd": turnkey_ex_carry,
                "lowUsd": round_1000((turnkey_ex_carry as f64) * 0.88),
                "highUsd": round_1000((turnkey_ex_carry as f64) * 1.16),
                "temporaryHousingCarryUsd": temp_housing_opt,
                "turnkeyWithTempHousingUsd": turnkey_with_carry_opt,
                "allInPerSqftUsd": all_in_psf,
            })
        };

        json!({
            "id": tier_id,
            "label": label,
            "hardCostPerSqftUsd": hard_psf,
            "estimatedBuildMonths": build_months,
            "estimatedPermitMonths": if state == "CA" || regional_mult >= 1.35 { 6 } else { 4 },
            "specSummary": spec_summary,
            "currentSize": calc_for_sqft(sqft),
            "maxZoningFarSize": calc_for_sqft(max_far_sqft as f64),
        })
    };

    let custom_tier = mk_rebuild_tier(
        "custom",
        "Custom Neighborhood-Grade (Recommended Benchmark)",
        cus_hard_psf,
        13,
        "9–10 ft ceilings, custom cabinetry, quartz/stone slabs, engineered/solid oak floors, heat-pump MEP & architectural exterior.",
    );
    let rebuild_tiers = vec![
        mk_rebuild_tier(
            "standard",
            "Standard / Production-Spec Rebuild",
            std_hard_psf,
            11,
            "Code-compliant framing, architectural shingle roof, stock/semi-custom cabinets, LVP/carpet, standard builder fixtures.",
        ),
        custom_tier.clone(),
        mk_rebuild_tier(
            "luxury",
            "Luxury Architect-Designed Custom Home",
            lux_hard_psf,
            17,
            "Structural steel wide-span glazing, standing-seam/tile roof, bespoke millwork, pro chef's appliance suite & stonework.",
        ),
    ];

    // NAHB Cost of Constructing a Home empirical stage shares applied to Custom tier hard build cost
    let custom_hard_cur = custom_tier
        .pointer("/currentSize/hardVerticalUsd")
        .and_then(|v| v.as_f64())
        .unwrap_or(sqft * cus_hard_psf as f64);
    let nahb_stages = [
        ("site-work", "Site Work, Excavation & Utilities", 7.4),
        (
            "foundation",
            "Foundation, Footings & Concrete Slab/Crawl",
            11.0,
        ),
        ("framing", "Framing, Roof Trusses & Sheathing", 20.5),
        (
            "exterior",
            "Exterior Finishes (Roofing, Windows, Stucco/Siding)",
            11.8,
        ),
        (
            "mep-rough",
            "Major Systems Rough-Ins (Plumbing, Electrical, HVAC)",
            17.9,
        ),
        (
            "interior",
            "Interior Finishes (Drywall, Cabinets, Counters, Tile, Floors)",
            24.0,
        ),
        (
            "final-steps",
            "Final Fixtures, Appliances, Driveway & Punchlist",
            5.9,
        ),
        (
            "other-supervision",
            "Site Supervision, Cleanup & Other Hard Costs",
            1.5,
        ),
    ]
    .into_iter()
    .map(|(sid, slabel, pct)| {
        json!({
            "id": sid,
            "stage": slabel,
            "sharePct": pct,
            "estimatedUsd": round_100(custom_hard_cur * (pct / 100.0)),
        })
    })
    .collect::<Vec<Value>>();

    // Tear-down vs. Remodel financial feasibility
    let feasibility = if is_attached_condo {
        json!({
            "applicable": false,
            "verdict": "Condo / Attached HOA Structure — Tear-Down Not Applicable",
            "summary": "Individual condominium/co-op units share HOA structural walls, roof, and land; evaluate interior gut-remodeling instead of structure demolition.",
        })
    } else if let Some(mv) = est_market_val {
        let target_rebuild_sq = (max_far_sqft as f64).max(sqft);
        let custom_max_cost = custom_tier
            .pointer("/maxZoningFarSize/turnkeyRebuildTotalUsd")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let existing_psf = mv / sqft.max(1.0);
        // New-construction commands ~18%–28% $/sq ft premium over older neighborhood stock,
        // with diminishing marginal $/sq ft on expansions >1.35x existing size.
        let size_expansion_damp = (sqft / target_rebuild_sq).clamp(0.55, 1.0).powf(0.18);
        let est_new_psf = existing_psf * 1.24 * size_expansion_damp;
        let est_completed_val = round_1000(est_new_psf * target_rebuild_sq);
        let total_acquisition_plus_rebuild = round_1000(mv + custom_max_cost);
        let spread_usd = est_completed_val - total_acquisition_plus_rebuild;
        let land_share_est_pct = round_n(
            (((mv - (sqft * (std_hard_psf as f64) * 0.55)).max(mv * 0.25)) / mv) * 100.0,
            1,
        );
        let (verdict, summary) = if spread_usd >= 50_000 {
            (
                "Financially Viable Tear-Down / Expansion Candidate",
                format!(
                    "Estimated land value represents ~{}% of property value. Acquiring at ~${} + rebuilding ~{} sq ft (~${}) totals ~${} vs. ~${} projected new-construction value.",
                    land_share_est_pct,
                    fmt_k(mv),
                    target_rebuild_sq.round() as i64,
                    fmt_k(custom_max_cost),
                    fmt_k(total_acquisition_plus_rebuild as f64),
                    fmt_k(est_completed_val as f64)
                ),
            )
        } else if spread_usd >= -180_000 {
            (
                "Borderline — Lifestyle Custom Build or Major Addition Preferred",
                format!(
                    "All-in acquisition + {} sq ft custom rebuild (~${}) is within ~${} of projected new-build resale (~${}); a major remodel + addition often preserves existing structure equity at lower cost.",
                    target_rebuild_sq.round() as i64,
                    fmt_k(total_acquisition_plus_rebuild as f64),
                    fmt_k(spread_usd.abs() as f64),
                    fmt_k(est_completed_val as f64)
                ),
            )
        } else {
            (
                "Remodel / Addition Strongly Preferred Over Tear-Down",
                format!(
                    "Existing structure carries substantial contributory value: tearing down and rebuilding (~${} all-in incl. land) exceeds projected post-build value (~${}) unless replacing severe structural damage.",
                    fmt_k(total_acquisition_plus_rebuild as f64),
                    fmt_k(est_completed_val as f64)
                ),
            )
        };
        json!({
            "applicable": true,
            "estimatedLandValueSharePct": land_share_est_pct,
            "currentPropertyValueUsd": mv.round() as i64,
            "targetBuildSqft": target_rebuild_sq.round() as i64,
            "customRebuildTurnkeyUsd": custom_max_cost.round() as i64,
            "totalAcquisitionPlusRebuildUsd": total_acquisition_plus_rebuild,
            "projectedNewHomeValueUsd": est_completed_val,
            "netEquitySpreadUsd": spread_usd,
            "verdict": verdict,
            "summary": summary,
        })
    } else {
        json!({
            "applicable": true,
            "verdict": "Compare Rebuild Cost Against Neighborhood New-Build Sales",
            "summary": "Compare all-in demolition + custom rebuild cost against recent newly constructed sales per sq ft in this school district before committing to a full tear-down.",
        })
    };

    // -------------------------------------------------------------------------
    // 3. Average Quotes for Rebuilding Lawn, Landscaping & Exterior Projects
    // -------------------------------------------------------------------------
    let footprint_sqft_opt = if is_attached_condo {
        Some(0.0)
    } else if let Some(fp) = explicit_footprint_sqft {
        Some(fp)
    } else {
        stories_opt.map(|st| (sqft / st.max(1.0)) + if is_townhome { 220.0 } else { 440.0 })
    };

    let (
        hardscape_est_sqft_opt,
        open_landscaping_sqft_opt,
        est_lawn_sqft_opt,
        front_yard_sqft_opt,
        back_yard_sqft_opt,
        fence_linear_ft_opt,
    ) = if is_attached_condo {
        (Some(0), Some(0), Some(0), Some(0), Some(0), Some(0))
    } else if let Some(lot_sq) = raw_lot {
        let fp_for_yard = footprint_sqft_opt.unwrap_or_else(|| sqft.min(lot_sq * 0.65));
        let raw_open_yard = (lot_sq - fp_for_yard).max(0.0);
        let hardscape_sq = (raw_open_yard * 0.22)
            .clamp(350.0, 2_400.0)
            .min(raw_open_yard);
        let open_land_sq = (raw_open_yard - hardscape_sq).max(0.0);
        let lawn_sq = open_land_sq.clamp(400.0, 10_000.0).round() as i64;
        let front_sq = ((lawn_sq as f64) * 0.35).round() as i64;
        let back_sq = (lawn_sq - front_sq).max(0);
        let fence_lf = ((lot_sq.max(2_000.0).sqrt() * 2.6).round() as i64).clamp(85, 460);
        (
            Some(hardscape_sq.round() as i64),
            Some(open_land_sq.round() as i64),
            Some(lawn_sq),
            Some(front_sq),
            Some(back_sq),
            Some(fence_lf),
        )
    } else {
        (None, None, None, None, None, None)
    };

    let turf_rebate = build_turf_rebate_info_opt(&state, fips, est_lawn_sqft_opt);

    let mk_yard_quote = |id: &str,
                         name: &str,
                         unit_label: &str,
                         qty: i64,
                         unit_typ: f64,
                         fixed_mobilization: f64,
                         low_mult: f64,
                         high_mult: f64,
                         note: String|
     -> Value {
        let base_typ = fixed_mobilization * regional_mult + (qty as f64) * unit_typ;
        let typ = round_50(base_typ.max(450.0));
        let low = round_50((base_typ * low_mult).max(350.0));
        let high = round_50(base_typ * high_mult);
        json!({
            "id": id,
            "name": name,
            "unitBasis": unit_label,
            "quantity": qty,
            "unitRateTypicalUsd": round_n(unit_typ, 2),
            "lowUsd": low,
            "typicalUsd": typ,
            "highUsd": high,
            "note": note,
        })
    };

    let mut lawn_quotes: Vec<Value> = Vec::new();
    if !is_attached_condo {
        if let Some(lawn_sq) = est_lawn_sqft_opt.filter(|&sq| sq > 0) {
            lawn_quotes.push(mk_yard_quote(
                "lawn-sod-rebuild",
                "Complete Natural Sod Lawn Rebuild",
                "sq ft",
                lawn_sq,
                sod_rate_typ,
                650.0,
                0.78,
                1.32,
                "Includes stripping & hauling old dead turf, rototilling, 3–4 in. organic compost/topsoil blend, starter fertilizer, fine grading & fresh roll sod.".to_string(),
            ));
            lawn_quotes.push(mk_yard_quote(
                "lawn-hydroseed",
                "Hydroseeding / Overseed Lawn Rebuild",
                "sq ft",
                lawn_sq,
                hydro_rate_typ,
                550.0,
                0.76,
                1.35,
                "Budget-friendly alternative for >=2,000 sq ft lawns: soil prep + pressurized wood-fiber mulch, seed & tackifier slurry (requires 3–5 weeks establishment watering).".to_string(),
            ));
            lawn_quotes.push(mk_yard_quote(
                "irrigation-smart",
                "In-Ground Multi-Zone Smart Sprinkler & Drip System",
                "sq ft",
                lawn_sq,
                irrig_rate_typ,
                950.0,
                0.80,
                1.36,
                "Trenching, Schedule 40 PVC / poly laterals, pressure-regulated rotary heads, brass backflow preventer & EPA WaterSense Wi-Fi weather controller.".to_string(),
            ));
            lawn_quotes.push(mk_yard_quote(
                "xeriscape-native",
                "Drought-Tolerant / Native Xeriscape Conversion",
                "sq ft",
                lawn_sq,
                xeri_rate_typ,
                850.0,
                0.76,
                1.38,
                if turf_rebate["eligibleRegion"] == json!(true) {
                    format!(
                        "Sheet-mulching, drip conversion, decomposed granite/mulch & native perennials — eligible for up to ~${} ({}/sq ft) water-agency turf rebate before rebate deduction!",
                        turf_rebate["estimatedRebateUsd"].as_i64().unwrap_or(0),
                        format!("${:.2}", turf_rebate["rebatePerSqftUsd"].as_f64().unwrap_or(2.0))
                    )
                } else {
                    "Sheet-mulching turf removal, drip zones, decomposed granite pathway & low-water native perennials (cuts outdoor irrigation ~60–75%).".to_string()
                },
            ));
            lawn_quotes.push(mk_yard_quote(
                "artificial-turf",
                "Premium Permeable Pet / Play Artificial Turf",
                "sq ft",
                ((lawn_sq as f64) * 0.55).clamp(400.0, 3_500.0).round() as i64,
                synturf_rate_typ,
                900.0,
                0.82,
                1.30,
                "3–4 in. excavation, compacted Class II road base, gopher wire/weed barrier, UV-stabilized permeable turf & antimicrobial silica infill (sized to ~55% active play/pet zone).".to_string(),
            ));
        }

        lawn_quotes.push(mk_yard_quote(
            "grading-french-drain",
            "Yard Regrading & Perimeter French Drain",
            "linear ft",
            80,
            round_n(52.0 * regional_mult, 2),
            750.0,
            0.78,
            1.40,
            "Correct negative grading toward foundation + 80 lin ft perforated drain pipe in washed crushed rock wrapped in geotextile fabric to street/daylight.".to_string(),
        ));
        lawn_quotes.push(mk_yard_quote(
            "patio-pavers",
            "Backyard Paver or Architectural Concrete Patio",
            "sq ft",
            350,
            round_n(22.5 * regional_mult, 2),
            800.0,
            0.76,
            1.42,
            "350 sq ft outdoor entertaining patio: excavation, compacted aggregate base & interlocking pavers (broom-finish concrete ~$13–$18/sq ft; pavers ~$20–$34/sq ft).".to_string(),
        ));

        if let Some(fence_lf) = fence_linear_ft_opt.filter(|&lf| lf > 0) {
            lawn_quotes.push(mk_yard_quote(
                "fence-privacy",
                "6-Ft Cedar / Redwood or Composite Privacy Fence",
                "linear ft",
                fence_lf,
                fence_rate_lf,
                450.0,
                0.78,
                1.38,
                format!(
                    "Scaled to ~{} linear ft side/rear perimeter: old fence haul-off, posts set in concrete footings, rails, privacy boards & 1 walk gate.",
                    fence_lf
                ),
            ));
        }

        lawn_quotes.push(mk_yard_quote(
            "driveway-replace",
            "Concrete / Paver Driveway Replacement",
            "sq ft",
            600,
            round_n(15.5 * regional_mult, 2),
            900.0,
            0.80,
            1.45,
            "600 sq ft 2-car driveway: saw-cut & haul cracked asphalt/concrete, compacted base, #4 rebar grid & 4,000 PSI concrete pour.".to_string(),
        ));
        lawn_quotes.push(mk_yard_quote(
            "tree-arborist",
            "Certified Arborist Tree Pruning / Clearance (2–4 Trees)",
            "project",
            1,
            round_n(1_650.0 * regional_mult, 2),
            0.0,
            0.65,
            1.65,
            if high_wildfire {
                "ISA-certified arborist crown thinning, 10-ft chimney/roof clearance & Zone 0/1 wildfire defensible-space brush chipping.".to_string()
            } else {
                "ISA-certified arborist structural pruning, deadwood removal, roof/service-drop clearance & green-waste chipping.".to_string()
            },
        ));
    }

    let sod_turnkey_typ = lawn_quotes
        .iter()
        .find(|q| q.get("id").and_then(|v| v.as_str()) == Some("lawn-sod-rebuild"))
        .and_then(|q| q.get("typicalUsd"))
        .and_then(|v| v.as_i64());
    let custom_turnkey_cur = custom_tier
        .pointer("/currentSize/turnkeyRebuildTotalUsd")
        .and_then(|v| v.as_i64());

    let data = json!({
        "status": "ok",
        "regionalCostIndex": {
            "areaLabel": area_label,
            "countyName": county_name,
            "countyFips": if fips.is_empty() { Value::Null } else { json!(fips) },
            "state": state,
            "censusDivision": census_division,
            "censusRegion": census_region,
            "regionalMultiplier": regional_mult,
            "nationalIndexBase100": (regional_mult * 100.0).round() as i64,
            "blsCostInflationFactor": round_n(bls_factor, 3),
            "blsMaterialsPpiYoYPct": bls_ppi_yoy,
            "blsConstructionHourlyWageUsd": bls_wage_usd,
            "resaleRecoupFactor": recoup_mult,
        },
        "propertyBasis": {
            "propertyType": if pt_str.is_empty() { Value::Null } else { json!(pt_str) },
            "isAttachedCondo": is_attached_condo,
            "isTownhome": is_townhome,
            "livingSqft": sqft.round() as i64,
            "isAssumedSqft": false,
            "stories": stories_opt,
            "yearBuilt": year_built,
            "isPre1978LeadAsbestosEra": is_pre_1978,
            "lotSqft": if is_attached_condo {
                json!(0)
            } else {
                json!(raw_lot.map(|l| l.round() as i64))
            },
            "isAssumedLot": false,
            "buildingFootprintSqft": footprint_sqft_opt.map(|fp| fp.round() as i64),
            "roofAreaEstSqft": roof_footprint_sqft_opt.map(|r| r.round() as i64),
            "maxZoningBuildableSqft": max_far_sqft,
        },
        "headlineBenchmarks": {
            "customRebuildHardPerSqftUsd": cus_hard_psf,
            "tearDownPlusCustomRebuildCurrentSizeUsd": custom_turnkey_cur,
            "naturalSodLawnRebuildUsd": sod_turnkey_typ,
            "minorKitchenRemodelUsd": remodel_projects.first().and_then(|p| p.get("typicalUsd")).cloned().unwrap_or(Value::Null),
        },
        "remodelProjects": remodel_projects,
        "tearDownAndRebuild": {
            "status": "ok",
            "applicableForTearDown": !is_attached_condo,
            "demolitionAndSitePrep": {
                "structureDemolitionHaulOffUsd": demo_structure_usd,
                "environmentalAbatementUsd": abatement_usd,
                "isPre1978AbatementAllowance": is_pre_1978,
                "utilityDisconnectsAndErosionControlUsd": utility_cap_swppp_usd,
                "siteGradingAndExcavationUsd": site_grading_prep_usd,
                "totalTearDownAndSitePrepUsd": total_teardown_prep_usd,
            },
            "softCostAssumptions": {
                "architectureEngineeringPct": round_n(soft_arch_eng_pct * 100.0, 1),
                "permitsImpactFeesPct": round_n(soft_permits_impact_pct * 100.0, 1),
                "contingencyPct": round_n(contingency_pct * 100.0, 1),
                "garageAllowanceSqft": garage_allowance_sqft.round() as i64,
                "estimatedMonthlyRentCarryUsd": est_rent_mo_opt.map(|r| r.round() as i64),
            },
            "tiers": rebuild_tiers,
            "nahbStageBreakdown": nahb_stages,
            "feasibility": feasibility,
        },
        "lawnAndExterior": {
            "status": if is_attached_condo || est_lawn_sqft_opt.is_some() {
                "ok"
            } else {
                "missing_parcel_area"
            },
            "applicable": !is_attached_condo,
            "parcelYardBasis": {
                "lotSqft": if is_attached_condo {
                    json!(0)
                } else {
                    json!(raw_lot.map(|l| l.round() as i64))
                },
                "buildingFootprintSqft": footprint_sqft_opt.map(|fp| fp.round() as i64),
                "hardscapeDrivewayPatioEstSqft": hardscape_est_sqft_opt,
                "openLandscapingSqft": open_landscaping_sqft_opt,
                "estimatedLawnSqft": est_lawn_sqft_opt,
                "calculatorDefaultSqft": est_lawn_sqft_opt.filter(|&sq| sq > 0),
                "frontYardEstSqft": front_yard_sqft_opt,
                "backYardEstSqft": back_yard_sqft_opt,
                "perimeterFenceLinearFt": fence_linear_ft_opt.filter(|&lf| lf > 0),
            },
            "unitRatesRegionalUsd": {
                "naturalSodInstalledPerSqft": sod_rate_typ,
                "hydroseedingPerSqft": hydro_rate_typ,
                "smartIrrigationPerSqft": irrig_rate_typ,
                "xeriscapeConversionPerSqft": xeri_rate_typ,
                "artificialTurfPerSqft": synturf_rate_typ,
                "oldTurfRemovalAndGradingPerSqft": grading_prep_rate,
                "privacyFencePerLinearFt": fence_rate_lf,
            },
            "turfRebateProgram": turf_rebate,
            "quotes": lawn_quotes,
        },
        "readyToMoveIn": crate::modules::move_in_cost::build_ready_to_move_in_from_report(report),
        "disclaimer": disclaimer,
    });

    ModuleOutput {
        data,
        sources,
        warnings,
    }
}

fn fmt_k(v: f64) -> String {
    if v.abs() >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else {
        format!("{:.0}k", v / 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_silicon_valley_vs_midwest_remodel_rebuild_and_lawn_rebates() {
        let sj_report = crate::test_synth::synth_report_primary();
        let mut il_new_report =
            crate::test_synth::synth_report_from_holdout("synth-il-60201-newbuild-sfr");
        if let Some(f) = il_new_report.get_mut("facts").and_then(Value::as_object_mut) {
            f.remove("stories");
        }

        let sj = build_remodel_and_rebuild(&sj_report);
        let il = build_remodel_and_rebuild(&il_new_report);

        assert_eq!(sj.data["status"], json!("ok"));
        assert_eq!(il.data["status"], json!("ok"));

        let sj_mult = sj.data["regionalCostIndex"]["regionalMultiplier"]
            .as_f64()
            .unwrap();
        let il_mult = il.data["regionalCostIndex"]["regionalMultiplier"]
            .as_f64()
            .unwrap();
        assert!(
            sj_mult > 1.45 && sj_mult > il_mult + 0.20,
            "Silicon Valley mult ({sj_mult}) must substantially exceed Cook County IL ({il_mult})"
        );

        let sj_custom_psf = sj.data["headlineBenchmarks"]["customRebuildHardPerSqftUsd"]
            .as_i64()
            .unwrap();
        let il_custom_psf = il.data["headlineBenchmarks"]["customRebuildHardPerSqftUsd"]
            .as_i64()
            .unwrap();
        assert!(sj_custom_psf > il_custom_psf + 60);

        // Pre-1978 San Jose synthetic home (1974) includes asbestos/lead abatement allowance + Valley Water turf rebate
        assert_eq!(
            sj.data["tearDownAndRebuild"]["demolitionAndSitePrep"]["isPre1978AbatementAllowance"],
            json!(true)
        );
        assert_eq!(
            il.data["tearDownAndRebuild"]["demolitionAndSitePrep"]["isPre1978AbatementAllowance"],
            json!(false)
        );
        assert_eq!(
            sj.data["lawnAndExterior"]["turfRebateProgram"]["eligibleRegion"],
            json!(true)
        );
        assert!(
            sj.data["lawnAndExterior"]["turfRebateProgram"]["estimatedRebateUsd"]
                .as_i64()
                .unwrap()
                >= 2_000
        );
        assert_eq!(
            il.data["lawnAndExterior"]["turfRebateProgram"]["eligibleRegion"],
            json!(false)
        );
        assert!(
            sj.data["lawnAndExterior"]["parcelYardBasis"]["estimatedLawnSqft"]
                .as_i64()
                .unwrap()
                >= 2_500
        );
        // When `stories` is absent in public records, story count must remain null rather than invented
        assert!(il.data["propertyBasis"]["stories"].is_null());
        assert_eq!(
            sj.data["tearDownAndRebuild"]["nahbStageBreakdown"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
    }

    #[test]
    fn test_missing_sqft_and_missing_lot_area_never_invent_dimensions() {
        let mut no_sqft = crate::test_synth::synth_report_secondary();
        if let Some(f) = no_sqft.get_mut("facts").and_then(|v| v.as_object_mut()) {
            f.remove("sqft");
            f.remove("lotSqft");
            f.remove("landSqft");
            f.remove("lotAcres");
            f.remove("acres");
            f.remove("stories");
        }
        let missing = build_remodel_and_rebuild(&no_sqft);
        assert_eq!(missing.data["status"], json!("missing_required_input"));
        assert!(missing.data["propertyBasis"]["livingSqft"].is_null());
        assert!(missing.data["propertyBasis"]["lotSqft"].is_null());
        assert!(missing.data["propertyBasis"]["stories"].is_null());
        assert_eq!(missing.data["remodelProjects"].as_array().unwrap().len(), 0);
        assert_eq!(
            missing.data["tearDownAndRebuild"]["tiers"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(
            missing.data["headlineBenchmarks"]["tearDownPlusCustomRebuildCurrentSizeUsd"].is_null()
        );
        assert!(missing.data["headlineBenchmarks"]["naturalSodLawnRebuildUsd"].is_null());
        assert!(
            missing.data["lawnAndExterior"]["turfRebateProgram"]["estimatedRebateUsd"].is_null()
        );
        assert!(!missing.warnings.is_empty());

        // Verified sqft present, parcel lot area absent: interior remodel works, lawn/fence omitted
        let mut no_lot = crate::test_synth::synth_report_primary();
        if let Some(f) = no_lot.get_mut("facts").and_then(|v| v.as_object_mut()) {
            f.remove("lotSqft");
            f.remove("landSqft");
            f.remove("lotAcres");
            f.remove("acres");
        }
        let partial = build_remodel_and_rebuild(&no_lot);
        assert_eq!(partial.data["status"], json!("ok"));
        assert_eq!(partial.data["propertyBasis"]["livingSqft"], json!(2040));
        assert!(partial.data["propertyBasis"]["lotSqft"].is_null());
        assert_eq!(
            partial.data["lawnAndExterior"]["status"],
            json!("missing_parcel_area")
        );
        assert!(partial.data["lawnAndExterior"]["parcelYardBasis"]["estimatedLawnSqft"].is_null());
        assert!(partial.data["headlineBenchmarks"]["naturalSodLawnRebuildUsd"].is_null());
        let quote_ids: Vec<&str> = partial.data["lawnAndExterior"]["quotes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|q| q["id"].as_str())
            .collect();
        assert!(!quote_ids.contains(&"lawn-sod-rebuild"));
        assert!(!quote_ids.contains(&"fence-privacy"));
    }
}
