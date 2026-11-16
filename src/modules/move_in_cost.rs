// Ready-to-Move-In True Cost & Hybrid Local Market Repair/Upgrade Quote Engine.
//
// Bridges:
//   1. Property dimensions & parcel geometry (`facts.sqft`, `facts.lotSqft`, `buildingFootprintSqft`,
//      `stories`, `baths`, `yearBuilt`, `COUNTY_COST_MULT`, BLS `costInflationFactor`, municipal turf rebates).
//   2. Inspection & Disclosure Packet findings/suggestions (auto-mapping every defect AND inspector
//      recommendation — including cosmetic/yard items like "lawn should be redone", "worn carpet",
//      "negative grading", "aging water heater" — onto fair local market cost ranges).
//   3. User-added custom upgrades (e.g. "replace floor", "redo lawn", "repaint interior", "epoxy garage floor")
//      via an instant 33-trade area-scaled regional catalog + live local market web lookup + cached
//      Structured Output AI estimator (`POST /api/move-in-quote`).
//
// Strictly honors Data Honesty (`AGENTS.md`): never invents missing purchase prices, never leaks
// internal LLM model/provider/reasoning identifiers, and clearly cites regional cost basis.

use crate::ai::llm::{
    resolve_model_specs_with_effort, responses_json_with_cache, LlmCache, LlmCallOptions,
};
use crate::guardrails::{
    clean_untrusted_text, inspect_output_guardrail, wrap_untrusted_block, GuardrailError,
};
use crate::modules::remodel_rebuild::{build_turf_rebate_info_opt, county_or_state_cost_context};
use crate::shared::{round_n, EnvConfig, HttpClient};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn round_50(v: f64) -> i64 {
    ((v / 50.0).round() as i64) * 50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveInPropertyBasis {
    pub city: String,
    pub state: String,
    pub county_name: String,
    pub county_fips: String,
    pub zip: String,
    pub area_label: String,
    pub regional_multiplier: f64,
    pub bls_inflation_factor: f64,
    pub is_attached_condo: bool,
    pub is_townhome: bool,
    pub living_sqft: i64,
    pub has_verified_sqft: bool,
    pub net_floor_sqft: i64,
    pub lot_sqft: i64,
    pub has_verified_lot_sqft: bool,
    pub building_footprint_sqft: i64,
    pub estimated_lawn_sqft: i64,
    pub front_yard_sqft: i64,
    pub back_yard_sqft: i64,
    pub roof_area_est_sqft: i64,
    pub perimeter_fence_lf: i64,
    pub stories: Option<f64>,
    pub baths: Option<f64>,
    pub window_count: i64,
    pub hvac_tons: f64,
    pub year_built: Option<i64>,
    pub is_pre_1978: bool,
    pub purchase_price_usd: Option<i64>,
    pub purchase_price_source: Option<String>,
    pub estimated_closing_costs_usd: Option<i64>,
    pub turf_rebate_program: Value,
}

impl MoveInPropertyBasis {
    pub fn from_report(report: &Value) -> Self {
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

        let state = geo
            .get("state")
            .or_else(|| report.pointer("/meta/state"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        let fips = geo
            .get("countyFips")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let county_name = geo
            .get("county")
            .and_then(|v| v.as_str())
            .unwrap_or("Local County")
            .to_string();
        let city = geo
            .get("city")
            .or_else(|| report.pointer("/meta/city"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let zip = geo
            .get("zip")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let (geo_mult, area_label, _, _) = county_or_state_cost_context(&state, &fips, &city);
        let bls_factor = market
            .pointer("/constructionInflation/costInflationFactor")
            .and_then(|v| v.as_f64())
            .filter(|x| x.is_finite() && *x > 0.7 && *x < 1.5)
            .unwrap_or(1.00);
        let regional_mult = round_n((geo_mult * bls_factor).clamp(0.82, 1.85), 3);

        let pt_lc = facts
            .get("propertyType")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_attached_condo =
            pt_lc.contains("condo") || pt_lc.contains("co-op") || pt_lc.contains("apart");
        let is_townhome = pt_lc.contains("town");

        let raw_sqft = facts
            .get("sqft")
            .and_then(|v| v.as_f64())
            .filter(|&s| s.is_finite() && s > 0.0);
        let (living_sqft, net_floor_sqft, window_count, hvac_tons) = if let Some(sqft) = raw_sqft {
            (
                sqft.round() as i64,
                (sqft * 0.85).clamp(200.0, 10_000.0).round() as i64,
                ((sqft / 160.0).round() as i64).clamp(8, 28),
                round_n((sqft / 550.0).clamp(2.0, 5.0), 1),
            )
        } else {
            (0, 0, 0, 0.0)
        };

        let stories = facts
            .get("stories")
            .and_then(|v| v.as_f64())
            .filter(|&s| s.is_finite() && (1.0..=10.0).contains(&s));
        let baths = facts
            .get("baths")
            .and_then(|v| v.as_f64())
            .filter(|&b| b.is_finite() && (0.5..=20.0).contains(&b));
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

        let footprint_sqft_opt = if is_attached_condo {
            Some(0.0)
        } else if let Some(fp) = explicit_footprint_sqft {
            Some(fp)
        } else if let (Some(sqft), Some(st)) = (raw_sqft, stories) {
            Some((sqft / st.max(1.0)) + if is_townhome { 220.0 } else { 440.0 })
        } else {
            None
        };

        let roof_area_est_sqft = if is_attached_condo {
            0
        } else if let (Some(sqft), Some(st)) = (raw_sqft, stories) {
            ((sqft / st.max(1.0)) * 1.28).clamp(800.0, 5_500.0).round() as i64
        } else if let Some(fp) = explicit_footprint_sqft {
            (fp * 1.15).clamp(800.0, 5_500.0).round() as i64
        } else {
            0
        };

        let (
            lot_sqft,
            has_verified_lot_sqft,
            est_lawn_sqft_opt,
            front_yard_sqft,
            back_yard_sqft,
            perimeter_fence_lf,
        ) = if is_attached_condo {
            (0, raw_lot.is_some(), Some(0), 0, 0, 0)
        } else if let Some(lot_sq) = raw_lot {
            let fence_lf = ((lot_sq.max(2_000.0).sqrt() * 2.6).round() as i64).clamp(85, 460);
            if let Some(fp_for_yard) =
                footprint_sqft_opt.or_else(|| raw_sqft.map(|sq| sq.min(lot_sq * 0.65)))
            {
                let raw_open_yard = (lot_sq - fp_for_yard).max(0.0);
                let hardscape_sqft = (raw_open_yard * 0.22)
                    .clamp(350.0, 2_400.0)
                    .min(raw_open_yard);
                let open_landscaping = (raw_open_yard - hardscape_sqft).max(0.0);
                let lawn_sq = open_landscaping.clamp(400.0, 10_000.0).round() as i64;
                let front_sq = ((lawn_sq as f64) * 0.35).round() as i64;
                let back_sq = (lawn_sq - front_sq).max(0);
                (
                    lot_sq.round() as i64,
                    true,
                    Some(lawn_sq),
                    front_sq,
                    back_sq,
                    fence_lf,
                )
            } else {
                (lot_sq.round() as i64, true, None, 0, 0, fence_lf)
            }
        } else {
            (0, false, None, 0, 0, 0)
        };

        let (purchase_price_usd, purchase_price_source) = if let Some(lp) = facts
            .get("listPrice")
            .and_then(|v| v.as_f64())
            .filter(|&p| p > 25_000.0)
        {
            (
                Some(lp.round() as i64),
                Some("Active Listing Price".to_string()),
            )
        } else if let Some(ev) = valuation
            .pointer("/estimate/value")
            .and_then(|v| v.as_f64())
            .filter(|&p| p > 25_000.0)
        {
            (
                Some(ev.round() as i64),
                Some("Estimated Market Value".to_string()),
            )
        } else {
            (None, None)
        };

        let estimated_closing_costs_usd = report
            .pointer("/buyerDiligence/cashToClose/scenarios/0/totalClosingCostsUsd")
            .or_else(|| report.pointer("/modules/buyerDiligence/data/cashToClose/scenarios/0/totalClosingCostsUsd"))
            .or_else(|| report.pointer("/buyerDiligence/transferTaxAndClosing/estimatedBuyerClosingCostsUsd"))
            .or_else(|| report.pointer("/modules/buyerDiligence/data/transferTaxAndClosing/estimatedBuyerClosingCostsUsd"))
            .and_then(|v| v.as_f64())
            .filter(|&c| c > 500.0)
            .map(|c| round_50(c))
            .or_else(|| purchase_price_usd.map(|pp| round_50((pp as f64) * 0.0175)));

        let turf_rebate_program = build_turf_rebate_info_opt(&state, &fips, est_lawn_sqft_opt);

        Self {
            city,
            state,
            county_name,
            county_fips: fips,
            zip,
            area_label,
            regional_multiplier: regional_mult,
            bls_inflation_factor: round_n(bls_factor, 3),
            is_attached_condo,
            is_townhome,
            living_sqft,
            has_verified_sqft: raw_sqft.is_some(),
            net_floor_sqft,
            lot_sqft,
            has_verified_lot_sqft,
            building_footprint_sqft: footprint_sqft_opt.map(|fp| fp.round() as i64).unwrap_or(0),
            estimated_lawn_sqft: est_lawn_sqft_opt.unwrap_or(0),
            front_yard_sqft,
            back_yard_sqft,
            roof_area_est_sqft,
            perimeter_fence_lf,
            stories,
            baths,
            window_count,
            hvac_tons,
            year_built,
            is_pre_1978,
            purchase_price_usd,
            purchase_price_source,
            estimated_closing_costs_usd,
            turf_rebate_program,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuantityBasis {
    NetFloorSqft,
    LivingSqft,
    PartialFloorSqft(u8), // percent of livingSqft
    LawnSqft,
    PartialLawnSqft(u8),
    RoofSqft,
    FootprintSqft,
    FenceLf,
    DrainLf,
    GutterLf,
    SewerLf,
    Windows,
    HvacTons,
    FixedOne,
}

#[derive(Debug, Clone)]
struct CatalogItemSpec {
    id: &'static str,
    name: &'static str,
    category: &'static str,
    trade: &'static str,
    unit_label: &'static str,
    qty_basis: QuantityBasis,
    base_mobilization_usd: f64,
    base_unit_rate_usd: f64,
    low_mult: f64,
    high_mult: f64,
    permit_required: bool,
    eligible_turf_rebate: bool,
    timeline: &'static str,
    scope_template: &'static str,
    keywords: &'static [&'static str],
}

const CATALOG_SPECS: &[CatalogItemSpec] = &[
    // --- Flooring ---
    CatalogItemSpec {
        id: "floor-lvp",
        name: "Replace Flooring — Waterproof Luxury Vinyl Plank (LVP)",
        category: "Flooring",
        trade: "Flooring Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::NetFloorSqft,
        base_mobilization_usd: 550.0,
        base_unit_rate_usd: 6.85,
        low_mult: 0.78,
        high_mult: 1.30,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "3–6 days",
        scope_template: "Old carpet/floor tear-out & haul-off, subfloor leveling, 20-mil wear-layer waterproof LVP & shoe molding.",
        keywords: &["lvp", "vinyl plank", "luxury vinyl", "laminate floor", "replace floor", "new floor", "换地板", "复合地板", "胶地板", "piso vinilico", "cambiar piso"],
    },
    CatalogItemSpec {
        id: "floor-engineered-hardwood",
        name: "Replace Flooring — Engineered White Oak Hardwood",
        category: "Flooring",
        trade: "Hardwood Flooring Specialist",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::NetFloorSqft,
        base_mobilization_usd: 650.0,
        base_unit_rate_usd: 11.80,
        low_mult: 0.80,
        high_mult: 1.34,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "5–9 days",
        scope_template: "Existing floor demolition, moisture barrier/acoustic underlayment, wide-plank engineered white oak & baseboard transitions.",
        keywords: &["engineered hardwood", "engineered white oak", "engineered oak", "white oak", "wood floor", "白橡木", "木地板", "工程木地板", "多层实木", "madera ingenieria"],
    },
    CatalogItemSpec {
        id: "floor-solid-hardwood",
        name: "Replace Flooring — Solid Red/White Oak Hardwood",
        category: "Flooring",
        trade: "Hardwood Flooring Specialist",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::NetFloorSqft,
        base_mobilization_usd: 750.0,
        base_unit_rate_usd: 14.90,
        low_mult: 0.82,
        high_mult: 1.36,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1.5–2.5 wks",
        scope_template: "3/4-inch solid oak nail/glue-down installation, acclimation, site sanding, custom stain & 3-coat commercial waterborne polyurethane.",
        keywords: &["solid hardwood", "solid oak", "site finished hardwood", "实木地板", "纯实木", "madera solida"],
    },
    CatalogItemSpec {
        id: "floor-refinish-hardwood",
        name: "Sand, Stain & Refinish Existing Hardwood Floors",
        category: "Flooring",
        trade: "Hardwood Flooring Specialist",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::PartialFloorSqft(70),
        base_mobilization_usd: 450.0,
        base_unit_rate_usd: 5.40,
        low_mult: 0.78,
        high_mult: 1.30,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "3–5 days",
        scope_template: "Dustless drum/edge sanding of scratched/worn hardwood, wood filler, stain & 3 coats Bona/commercial waterborne finish.",
        keywords: &["refinish hardwood", "sand hardwood", "scratched wood floor", "worn wood floor", "cupped hardwood", "地板打磨", "地板翻新"],
    },
    CatalogItemSpec {
        id: "floor-carpet",
        name: "Replace Bedroom / Stair Carpet & Memory-Foam Pad",
        category: "Flooring",
        trade: "Flooring Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::PartialFloorSqft(45),
        base_mobilization_usd: 350.0,
        base_unit_rate_usd: 4.95,
        low_mult: 0.76,
        high_mult: 1.34,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1–2 days",
        scope_template: "Tear out & dispose stained/worn carpet, tack strips, 8-lb moisture-barrier pad & stain-resistant nylon/triexta carpet.",
        keywords: &["carpet", "worn carpet", "stained carpet", "replace carpet", "换地毯", "地毯", "alfombra"],
    },
    CatalogItemSpec {
        id: "floor-porcelain-tile",
        name: "Install Large-Format Porcelain Tile Flooring",
        category: "Flooring",
        trade: "Tile Setter",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::PartialFloorSqft(30),
        base_mobilization_usd: 650.0,
        base_unit_rate_usd: 15.60,
        low_mult: 0.80,
        high_mult: 1.38,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "4–8 days",
        scope_template: "Old tile/vinyl demo, crack-isolation uncoupling membrane, thin-set mortar, rectified porcelain tile & stain-proof epoxy grout.",
        keywords: &["porcelain tile", "ceramic tile", "tile floor", "cracked floor tile", "地砖", "瓷砖", "azulejo", "porcelanato"],
    },
    // --- Lawn, Yard & Exterior ---
    CatalogItemSpec {
        id: "lawn-sod-redo",
        name: "Redo Lawn — Strip Dead Turf, Compost Prep & Fresh Roll Sod",
        category: "Lawn & Exterior",
        trade: "Landscaping Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::LawnSqft,
        base_mobilization_usd: 650.0,
        base_unit_rate_usd: 3.15,
        low_mult: 0.78,
        high_mult: 1.32,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "Strip & haul dead/weedy grass, rototill 3–4 in. organic compost/topsoil, fine grade, starter fertilizer & fresh roll sod.",
        keywords: &["redo lawn", "lawn redo", "replace lawn", "dead lawn", "dead grass", "new sod", "resod", "sod lawn", "bare yard", "patchy lawn", "重铺草坪", "换草坪", "草皮", "cesped", "jardin"],
    },
    CatalogItemSpec {
        id: "lawn-xeriscape-redo",
        name: "Redo Lawn — Drought-Tolerant Native Xeriscape & Drip",
        category: "Lawn & Exterior",
        trade: "Landscape Design-Build",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::LawnSqft,
        base_mobilization_usd: 850.0,
        base_unit_rate_usd: 7.60,
        low_mult: 0.76,
        high_mult: 1.38,
        permit_required: false,
        eligible_turf_rebate: true,
        timeline: "4–8 days",
        scope_template: "Sheet-mulching turf conversion, drip irrigation zones, decomposed granite/mulch & low-water native perennials (rebate-eligible).",
        keywords: &["xeriscape", "drought tolerant", "native landscape", "cash for grass", "turf conversion", "节水景观", "旱景", "xerojardin"],
    },
    CatalogItemSpec {
        id: "lawn-artificial-turf",
        name: "Redo Lawn — Permeable Pet / Play Artificial Turf",
        category: "Lawn & Exterior",
        trade: "Synthetic Turf Installer",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::PartialLawnSqft(55),
        base_mobilization_usd: 900.0,
        base_unit_rate_usd: 13.80,
        low_mult: 0.82,
        high_mult: 1.30,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "3–5 days",
        scope_template: "3–4 in. excavation, compacted Class II road base, gopher mesh, UV-stabilized permeable turf & antimicrobial infill.",
        keywords: &["artificial turf", "synthetic turf", "fake grass", "astroturf", "假草", "仿真草坪", "人工草坪"],
    },
    CatalogItemSpec {
        id: "irrigation-smart",
        name: "Repair / Install Multi-Zone Smart Sprinkler & Drip System",
        category: "Lawn & Exterior",
        trade: "Irrigation Specialist",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::LawnSqft,
        base_mobilization_usd: 850.0,
        base_unit_rate_usd: 1.75,
        low_mult: 0.78,
        high_mult: 1.36,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "Trenching, Schedule 40 PVC / drip laterals, pressure-regulated heads, backflow preventer & Wi-Fi smart controller.",
        keywords: &["sprinkler", "irrigation", "broken sprinkler", "drip system", "喷灌", "滴灌", "自动浇水"],
    },
    CatalogItemSpec {
        id: "yard-grading-drainage",
        name: "Yard Regrading & Perimeter French Drain",
        category: "Lawn & Exterior",
        trade: "Drainage & Earthwork Contractor",
        unit_label: "linear ft",
        qty_basis: QuantityBasis::DrainLf,
        base_mobilization_usd: 750.0,
        base_unit_rate_usd: 52.00,
        low_mult: 0.78,
        high_mult: 1.40,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2–5 days",
        scope_template: "Correct negative soil slope against foundation + perforated French drain in washed gravel & filter fabric to street/daylight.",
        keywords: &["negative grading", "french drain", "yard drainage", "ponding", "downspout discharge", "efflorescence", "排水沟", "暗沟", "地基积水"],
    },
    CatalogItemSpec {
        id: "fence-privacy",
        name: "Replace 6-Ft Cedar / Redwood Privacy Fence",
        category: "Lawn & Exterior",
        trade: "Fencing Contractor",
        unit_label: "linear ft",
        qty_basis: QuantityBasis::FenceLf,
        base_mobilization_usd: 450.0,
        base_unit_rate_usd: 54.00,
        low_mult: 0.78,
        high_mult: 1.38,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "Haul away leaning/rotted fence, set posts in concrete footings, rails, 6-ft cedar/redwood privacy boards & walk gate.",
        keywords: &["fence", "leaning fence", "rotted fence", "wood fence", "gate post", "围栏", "木栅栏", "篱笆", "cerca"],
    },
    CatalogItemSpec {
        id: "tree-trimming-clearance",
        name: "Certified Arborist Tree Pruning & Roof Clearance",
        category: "Lawn & Exterior",
        trade: "ISA Certified Arborist",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 1_650.0,
        low_mult: 0.65,
        high_mult: 1.65,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1–2 days",
        scope_template: "Structural crown pruning, deadwood removal, 10-ft roof/chimney/service-drop clearance & brush chipping.",
        keywords: &["tree trim", "arborist", "overhanging branch", "tree limbs", "defensible space", "砍树", "修剪树木"],
    },
    // --- Paint & Interior Surfaces ---
    CatalogItemSpec {
        id: "paint-interior-whole",
        name: "Whole-Home Interior Repaint (Walls, Trim & Ceilings)",
        category: "Paint & Finishes",
        trade: "Painting Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::LivingSqft,
        base_mobilization_usd: 450.0,
        base_unit_rate_usd: 3.45,
        low_mult: 0.76,
        high_mult: 1.32,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "4–7 days",
        scope_template: "Drywall nail-pop/settlement patching, caulk, primer & 2 coats zero-VOC architectural paint across walls, ceilings, doors & baseboards.",
        keywords: &["interior paint", "repaint interior", "paint walls", "paint house inside", "室内刷漆", "全屋刷墙", "油漆", "pintura interior"],
    },
    CatalogItemSpec {
        id: "paint-exterior",
        name: "Exterior Stucco / Siding Prep & Repaint",
        category: "Paint & Finishes",
        trade: "Painting Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::LivingSqft,
        base_mobilization_usd: 650.0,
        base_unit_rate_usd: 3.10,
        low_mult: 0.78,
        high_mult: 1.36,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "4–8 days",
        scope_template: "Power wash, trench/stucco crack elastomeric patching, wood trim priming & 2 coats weather-shield exterior acrylic.",
        keywords: &["exterior paint", "peeling paint", "repaint exterior", "stucco patch", "siding paint", "外墙刷漆", "外墙翻新", "pintura exterior"],
    },
    CatalogItemSpec {
        id: "popcorn-ceiling-skim",
        name: "Popcorn / Acoustic Ceiling Removal & Smooth Skim Coat",
        category: "Paint & Finishes",
        trade: "Drywall & Abatement Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::PartialFloorSqft(75),
        base_mobilization_usd: 600.0,
        base_unit_rate_usd: 4.60,
        low_mult: 0.78,
        high_mult: 1.42,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "4–6 days",
        scope_template: "Containment, lab asbestos test, acoustic texture removal, Level-4 smooth drywall skim coat, PVA primer & ceiling paint.",
        keywords: &["popcorn ceiling", "acoustic ceiling", "skim coat ceiling", "爆米花顶", "天花板抚平"],
    },
    // --- Roof, Gutters & Windows ---
    CatalogItemSpec {
        id: "roof-replace",
        name: "Full Architectural Cool-Roof Replacement",
        category: "Roof & Envelope",
        trade: "Licensed Roofing Contractor",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::RoofSqft,
        base_mobilization_usd: 950.0,
        base_unit_rate_usd: 8.60,
        low_mult: 0.82,
        high_mult: 1.35,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "1–2 wks",
        scope_template: "Tear-off old shingles, deck inspection, synthetic underlayment, new valleys/pipe/chimney flashing, ridge vents & Class A shingles.",
        keywords: &["roof replacement", "replace roof", "new roof", "granular loss", "end-of-life roof", "aging roof", "shingle roof", "换屋顶", "重做屋顶", "techo nuevo"],
    },
    CatalogItemSpec {
        id: "roof-tuneup-flashing",
        name: "Roof Flashing, Vent Boot & Minor Leak Service",
        category: "Roof & Envelope",
        trade: "Licensed Roofing Contractor",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 1_450.0,
        low_mult: 0.68,
        high_mult: 1.55,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1–2 days",
        scope_template: "Reseal lifted chimney cricket/step flashing, replace cracked plumbing vent boots, replace damaged shingles & tune roof penetrations.",
        keywords: &["lifted flashing", "chimney cricket", "vent boot", "roof tune", "minor roof leak", "missing shingle", "屋顶补漏", "泛水修复"],
    },
    CatalogItemSpec {
        id: "gutters-downspouts",
        name: "Seamless Aluminum Gutters, Downspouts & Extensions",
        category: "Roof & Envelope",
        trade: "Gutter & Sheet-Metal Installer",
        unit_label: "linear ft",
        qty_basis: QuantityBasis::GutterLf,
        base_mobilization_usd: 350.0,
        base_unit_rate_usd: 16.50,
        low_mult: 0.78,
        high_mult: 1.35,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1–2 days",
        scope_template: "5-inch/6-inch K-style seamless aluminum gutters, hidden hangers, oversized downspouts & leader extensions away from foundation.",
        keywords: &["gutter", "downspout", "clogged gutter", "rusted gutter", "雨水槽", "落水管"],
    },
    CatalogItemSpec {
        id: "windows-dual-pane",
        name: "Whole-Home Dual-Pane Low-E Window Replacement",
        category: "Roof & Envelope",
        trade: "Fenestration / Window Contractor",
        unit_label: "window",
        qty_basis: QuantityBasis::Windows,
        base_mobilization_usd: 500.0,
        base_unit_rate_usd: 1_180.0,
        low_mult: 0.80,
        high_mult: 1.34,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "1–2 wks",
        scope_template: "Retrofit insulated vinyl/fiberglass Low-E argon dual-pane windows, waterproof flashing tape & interior/exterior trim.",
        keywords: &["replace windows", "single pane", "fogged window", "failed seal window", "dual pane", "换窗户", "双层玻璃窗", "ventanas"],
    },
    // --- Plumbing & Sewer ---
    CatalogItemSpec {
        id: "water-heater-replace",
        name: "Replace Water Heater + Seismic Straps & Expansion Tank",
        category: "Plumbing & Sewer",
        trade: "Licensed Plumber",
        unit_label: "unit",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 2_350.0,
        low_mult: 0.76,
        high_mult: 1.45,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "1 day",
        scope_template: "50-gal high-recovery gas or hybrid water heater, upper/lower seismic straps, sediment trap, TPR drain pan & code venting.",
        keywords: &["water heater", "seismic strap", "sediment trap", "hot water tank", "tankless", "热水器", "calentador de agua"],
    },
    CatalogItemSpec {
        id: "sewer-hydrojet-spot",
        name: "Main Sewer Lateral Hydro-Jetting & Spot Trenchless Liner",
        category: "Plumbing & Sewer",
        trade: "Plumbing & Trenchless Sewer Specialist",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 3_600.0,
        low_mult: 0.65,
        high_mult: 1.55,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "1–2 days",
        scope_template: "High-pressure root hydro-jetting, two-way cleanout install & cured-in-place pipe (CIPP) spot liner at offset joint + camera re-scope.",
        keywords: &["root intrusion", "offset joint", "hydro-jet", "hydrojet", "spot liner", "sewer scope", "sewer lateral", "排污管树根", "下水道内窥"],
    },
    CatalogItemSpec {
        id: "sewer-full-trenchless",
        name: "Full Trenchless Main Sewer Lateral Replacement",
        category: "Plumbing & Sewer",
        trade: "Trenchless Sewer Contractor",
        unit_label: "linear ft",
        qty_basis: QuantityBasis::SewerLf,
        base_mobilization_usd: 1_200.0,
        base_unit_rate_usd: 135.0,
        low_mult: 0.80,
        high_mult: 1.38,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "Pipe-bursting HDPE SDR-17 main sewer lateral from house cleanout to street/property line, municipal sewer permit & pressure test.",
        keywords: &["replace sewer lateral", "collapsed sewer", "orangeburg", "cast-iron main sewer replacement", "更换主排污管"],
    },
    CatalogItemSpec {
        id: "whole-home-repipe",
        name: "Whole-Home PEX-A / Type-L Copper Supply Repipe",
        category: "Plumbing & Sewer",
        trade: "Licensed Repipe Plumber",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::LivingSqft,
        base_mobilization_usd: 900.0,
        base_unit_rate_usd: 6.20,
        low_mult: 0.78,
        high_mult: 1.38,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "3–5 days",
        scope_template: "Replace corroded galvanized/polybutylene supply lines with PEX-A / copper, quarter-turn shutoffs, pressure regulator & drywall patch.",
        keywords: &["galvanized pipe", "polybutylene", "repipe", "whole home repipe", "low water pressure pipe", "全屋换水管", "镀锌管"],
    },
    // --- Electrical & HVAC ---
    CatalogItemSpec {
        id: "electrical-panel-200a",
        name: "200A Main Electrical Panel Upgrade + AFCI/GFCI",
        category: "Electrical & HVAC",
        trade: "Licensed C-10 / Master Electrician",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 4_800.0,
        low_mult: 0.76,
        high_mult: 1.48,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "2–3 days",
        scope_template: "Upgrade 100A/Zinsco/FPE service to 200A solar-ready main breaker panel, copper ground rods, whole-home surge & AFCI/GFCI breakers.",
        keywords: &["200a", "200-amp", "100-amp main", "zinsco", "federal pacific", "fpe panel", "panel upgrade", "main panel", "配电箱升级", "电箱"],
    },
    CatalogItemSpec {
        id: "electrical-safety-gfci",
        name: "Subpanel Double-Tap, Grounding & GFCI Safety Cure",
        category: "Electrical & HVAC",
        trade: "Licensed Electrician",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 1_150.0,
        low_mult: 0.65,
        high_mult: 1.50,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1 day",
        scope_template: "Separate double-tapped neutrals/breakers, install missing knockouts, add weather-resistant GFCI outlets & ground receptacles.",
        keywords: &["double-tapped", "double tapped", "gfci", "ungrounded", "missing knockout", "open junction", "漏电保护", "插座接地"],
    },
    CatalogItemSpec {
        id: "hvac-replace",
        name: "Central Heat-Pump / Furnace & AC Replacement",
        category: "Electrical & HVAC",
        trade: "Licensed HVAC Contractor",
        unit_label: "ton",
        qty_basis: QuantityBasis::HvacTons,
        base_mobilization_usd: 5_200.0,
        base_unit_rate_usd: 2_950.0,
        low_mult: 0.82,
        high_mult: 1.32,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "Variable-speed inverter heat pump or high-efficiency furnace + AC coil/condenser, Title-24/HERS duct sealing & smart thermostat.",
        keywords: &["hvac", "furnace", "air conditioner", "heat pump", "condenser", "cracked heat exchanger", "换空调", "暖气炉", "热泵"],
    },
    // --- Pest, Foundation & Kitchen/Bath ---
    CatalogItemSpec {
        id: "termite-wdo-section1",
        name: "Section 1 Termite / WDO Treatment & Dry-Rot Wood Repair",
        category: "Pest & Structural",
        trade: "Licensed Structural Pest / WDO Operator",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 3_250.0,
        low_mult: 0.65,
        high_mult: 1.65,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "Subterranean soil termiticide barrier / localized drywood treatment + carpentry replacement of dry-rot fascia, trim & jambs.",
        keywords: &["termite", "wdo", "section 1", "dry-rot", "dry rot", "fungus", "shelter tube", "subterranean", "白蚁", "木腐", "termitas"],
    },
    CatalogItemSpec {
        id: "crawlspace-vapor-seismic",
        name: "Crawlspace Vapor Barrier, Crack Epoxy & Sill Bolting",
        category: "Pest & Structural",
        trade: "Foundation & Crawlspace Specialist",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::FootprintSqft,
        base_mobilization_usd: 850.0,
        base_unit_rate_usd: 3.10,
        low_mult: 0.75,
        high_mult: 1.42,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2–4 days",
        scope_template: "12–20 mil reinforced polyethylene crawlspace vapor barrier, stem-wall crack epoxy seal & moisture mitigation.",
        keywords: &["vapor barrier", "crawlspace", "stem-wall", "hairline", "cripple wall", "foundation crack", "防潮膜", "爬行空间", "地基裂缝"],
    },
    CatalogItemSpec {
        id: "kitchen-refresh",
        name: "Move-In Kitchen Refresh (Quartz Counters, Backsplash & Sink)",
        category: "Kitchen & Bath",
        trade: "General Remodeling Contractor",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 28_400.0,
        low_mult: 0.78,
        high_mult: 1.30,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "3–5 wks",
        scope_template: "Cabinet refinish/reface, quartz slab countertops, tile backsplash, undermount sink/faucet & Energy Star appliance suite.",
        keywords: &["kitchen refresh", "kitchen remodel", "quartz countertop", "update kitchen", "厨房翻新", "换台面"],
    },
    CatalogItemSpec {
        id: "bath-remodel",
        name: "Full Guest / Hall Bathroom Remodel",
        category: "Kitchen & Bath",
        trade: "General Remodeling Contractor",
        unit_label: "bathroom",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 26_200.0,
        low_mult: 0.78,
        high_mult: 1.32,
        permit_required: true,
        eligible_turf_rebate: false,
        timeline: "3–5 wks",
        scope_template: "Porcelain tile shower/tub surround, waterproofing membrane, solid-wood quartz vanity, Toto/Kohler fixtures & quiet exhaust fan.",
        keywords: &["bathroom remodel", "remodel bath", "update bathroom", "shower tile leak", "卫生间翻新", "浴室重做"],
    },
    CatalogItemSpec {
        id: "appliance-package",
        name: "Move-In Stainless Kitchen & Laundry Appliance Suite",
        category: "Kitchen & Bath",
        trade: "Appliance Installer",
        unit_label: "project",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 0.0,
        base_unit_rate_usd: 5_800.0,
        low_mult: 0.74,
        high_mult: 1.45,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "1–2 days",
        scope_template: "Counter-depth refrigerator, induction/gas range, quiet 44-dBA dishwasher, vent hood, washer/dryer delivery, hookup & haul-away.",
        keywords: &["appliance", "dishwasher", "refrigerator", "oven", "stove", "washer", "换家电", "全套家电"],
    },
    CatalogItemSpec {
        id: "garage-epoxy-door",
        name: "Garage Floor Polyaspartic/Epoxy Coating & Smart Opener",
        category: "Lawn & Exterior",
        trade: "Garage Flooring Specialist",
        unit_label: "sq ft",
        qty_basis: QuantityBasis::FixedOne,
        base_mobilization_usd: 2_650.0,
        base_unit_rate_usd: 0.0,
        low_mult: 0.78,
        high_mult: 1.32,
        permit_required: false,
        eligible_turf_rebate: false,
        timeline: "2 days",
        scope_template: "Diamond-grind 2-car garage slab (~420 sq ft), crack fill, decorative flake polyaspartic/epoxy coat & ultra-quiet belt-drive opener.",
        keywords: &["epoxy garage", "garage floor", "garage door opener", "车库地坪", "车库环氧"],
    },
];

fn default_qty_and_basis_note(
    basis: &MoveInPropertyBasis,
    qb: QuantityBasis,
) -> Result<(f64, String), &'static str> {
    match qb {
        QuantityBasis::NetFloorSqft => {
            if basis.has_verified_sqft && basis.net_floor_sqft > 0 {
                Ok((
                    basis.net_floor_sqft as f64,
                    format!(
                        "~{} sq ft net floor area (85% of {} sq ft living area)",
                        basis.net_floor_sqft, basis.living_sqft
                    ),
                ))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::LivingSqft => {
            if basis.has_verified_sqft && basis.living_sqft > 0 {
                Ok((
                    basis.living_sqft as f64,
                    format!("{} sq ft conditioned living area", basis.living_sqft),
                ))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::PartialFloorSqft(pct) => {
            if basis.has_verified_sqft && basis.living_sqft > 0 {
                let q = ((basis.living_sqft as f64) * (pct as f64 / 100.0))
                    .max(120.0)
                    .round();
                Ok((
                    q,
                    format!(
                        "~{} sq ft ({}% of {} sq ft living area)",
                        q as i64, pct, basis.living_sqft
                    ),
                ))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::LawnSqft => {
            if basis.is_attached_condo {
                Err("Requires private yard / lot size")
            } else if !basis.has_verified_lot_sqft {
                Err("Requires lot size")
            } else if basis.estimated_lawn_sqft > 0 {
                let q = basis.estimated_lawn_sqft as f64;
                let fp_note = if basis.building_footprint_sqft > 0 {
                    format!(
                        "~{} sq ft open lawn area ({} sq ft lot minus ~{} sq ft footprint/hardscape)",
                        q as i64, basis.lot_sqft, basis.building_footprint_sqft
                    )
                } else {
                    format!(
                        "~{} sq ft open lawn area ({} sq ft lot)",
                        q as i64, basis.lot_sqft
                    )
                };
                Ok((q, fp_note))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::PartialLawnSqft(pct) => {
            if basis.is_attached_condo {
                Err("Requires private yard / lot size")
            } else if !basis.has_verified_lot_sqft {
                Err("Requires lot size")
            } else if basis.estimated_lawn_sqft > 0 {
                let base = basis.estimated_lawn_sqft as f64;
                let q = (base * (pct as f64 / 100.0)).clamp(350.0, 4_000.0).round();
                Ok((
                    q,
                    format!(
                        "~{} sq ft active zone ({}% of ~{} sq ft open yard)",
                        q as i64, pct, base as i64
                    ),
                ))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::RoofSqft => {
            if basis.is_attached_condo {
                Err("HOA exterior envelope")
            } else if basis.roof_area_est_sqft > 0 {
                let note = if let Some(st) = basis.stories {
                    format!(
                        "~{} sq ft pitched roof area ({:.0}-story, {} sq ft home)",
                        basis.roof_area_est_sqft, st, basis.living_sqft
                    )
                } else {
                    format!("~{} sq ft pitched roof area", basis.roof_area_est_sqft)
                };
                Ok((basis.roof_area_est_sqft as f64, note))
            } else if !basis.has_verified_sqft {
                Err("Requires square footage")
            } else {
                Err("Requires story count or building footprint")
            }
        }
        QuantityBasis::FootprintSqft => {
            if basis.is_attached_condo {
                Err("HOA foundation / exterior envelope")
            } else if basis.building_footprint_sqft > 0 {
                let fp = basis.building_footprint_sqft as f64;
                Ok((
                    fp,
                    format!("~{} sq ft ground-floor/crawlspace footprint", fp as i64),
                ))
            } else if !basis.has_verified_sqft {
                Err("Requires square footage")
            } else {
                Err("Requires story count or building footprint")
            }
        }
        QuantityBasis::FenceLf => {
            if basis.is_attached_condo {
                Err("Requires private yard / lot size")
            } else if basis.has_verified_lot_sqft && basis.perimeter_fence_lf > 0 {
                let lf = basis.perimeter_fence_lf as f64;
                Ok((
                    lf,
                    format!("~{} linear ft side/rear lot perimeter", lf as i64),
                ))
            } else {
                Err("Requires lot size")
            }
        }
        QuantityBasis::DrainLf => Ok((80.0, "~80 linear ft perimeter drainage run".to_string())),
        QuantityBasis::GutterLf => {
            if basis.is_attached_condo {
                Err("HOA exterior envelope")
            } else if basis.roof_area_est_sqft > 0 {
                let lf = ((basis.roof_area_est_sqft as f64).sqrt() * 4.2)
                    .clamp(110.0, 280.0)
                    .round();
                Ok((lf, format!("~{} linear ft roof eave perimeter", lf as i64)))
            } else if !basis.has_verified_sqft {
                Err("Requires square footage")
            } else {
                Err("Requires story count or building footprint")
            }
        }
        QuantityBasis::SewerLf => Ok((
            65.0,
            "~65 linear ft house-to-street main sewer lateral".to_string(),
        )),
        QuantityBasis::Windows => {
            if basis.has_verified_sqft && basis.window_count > 0 {
                Ok((
                    basis.window_count as f64,
                    format!(
                        "~{} windows scaled to {} sq ft home",
                        basis.window_count, basis.living_sqft
                    ),
                ))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::HvacTons => {
            if basis.has_verified_sqft && basis.hvac_tons > 0.0 {
                Ok((
                    basis.hvac_tons,
                    format!(
                        "~{:.1}-ton capacity scaled to {} sq ft home",
                        basis.hvac_tons, basis.living_sqft
                    ),
                ))
            } else {
                Err("Requires square footage")
            }
        }
        QuantityBasis::FixedOne => Ok((
            1.0,
            format!(
                "{} ({:.2}x US trade index)",
                basis.area_label, basis.regional_multiplier
            ),
        )),
    }
}

fn finish_grade_factor(raw: Option<&str>) -> (&'static str, f64) {
    match raw
        .unwrap_or("midrange")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "budget" | "economy" | "basic" | "rental" => ("budget", 0.78),
        "upscale" | "luxury" | "premium" | "high" => ("upscale", 1.36),
        _ => ("midrange", 1.00),
    }
}

fn quote_spec(
    basis: &MoveInPropertyBasis,
    spec: &CatalogItemSpec,
    override_qty: Option<f64>,
    grade_raw: Option<&str>,
) -> Value {
    let (grade_id, grade_mult) = finish_grade_factor(grade_raw);

    // Multi-story stair/scaffold adjustment on whole-home flooring/paint/roof
    let story_adj = if basis.stories.map(|s| s >= 2.0).unwrap_or(false)
        && matches!(
            spec.id,
            "floor-lvp"
                | "floor-engineered-hardwood"
                | "floor-solid-hardwood"
                | "paint-exterior"
                | "roof-replace"
        ) {
        1.06
    } else {
        1.00
    };
    // Pre-1978 lead-safe RRP / asbestos precaution on paint & popcorn ceiling
    let era_adj =
        if basis.is_pre_1978 && matches!(spec.id, "popcorn-ceiling-skim" | "paint-exterior") {
            1.14
        } else {
            1.00
        };

    let scope_text = if basis.is_pre_1978
        && matches!(
            spec.id,
            "paint-interior-whole" | "paint-exterior" | "popcorn-ceiling-skim"
        ) {
        format!(
            "{} (EPA Lead-Safe RRP pre-1978 surface containment applies.)",
            spec.scope_template
        )
    } else {
        spec.scope_template.to_string()
    };

    let unit_rate_typ = round_n(
        spec.base_unit_rate_usd * basis.regional_multiplier * grade_mult * story_adj * era_adj,
        2,
    );

    let resolved_qty_note: Result<(f64, String), &'static str> = if let Some(oq) = override_qty
        .filter(|q| q.is_finite() && *q > 0.0)
        .map(|q| q.clamp(1.0, 50_000.0))
    {
        Ok((
            oq,
            format!(
                "User-specified {:.0} {} in {} ({:.2}x regional trade index)",
                oq, spec.unit_label, basis.area_label, basis.regional_multiplier
            ),
        ))
    } else {
        default_qty_and_basis_note(basis, spec.qty_basis)
    };

    let (qty, basis_note) = match resolved_qty_note {
        Ok(pair) => pair,
        Err(missing_reason) => {
            return json!({
                "id": spec.id,
                "catalogId": spec.id,
                "status": "missing_required_input",
                "name": spec.name,
                "category": spec.category,
                "trade": spec.trade,
                "finishGrade": grade_id,
                "unitBasis": spec.unit_label,
                "quantity": Value::Null,
                "unitRateTypicalUsd": unit_rate_typ,
                "lowUsd": Value::Null,
                "typicalUsd": Value::Null,
                "highUsd": Value::Null,
                "rebateUsd": Value::Null,
                "netTypicalUsd": Value::Null,
                "permitRequired": spec.permit_required,
                "timeline": spec.timeline,
                "areaBasisNote": missing_reason,
                "scope": scope_text,
                "localMarketBasis": format!(
                    "{} licensed contractor index ({:.2}x US baseline · BLS PPI {:.2}x)",
                    basis.area_label, basis.regional_multiplier, basis.bls_inflation_factor
                ),
                "pricingMethod": "regional_trade_catalog",
            });
        }
    };

    let mob_usd = spec.base_mobilization_usd * basis.regional_multiplier * grade_mult;
    let raw_typ = (mob_usd + qty * unit_rate_typ).max(350.0);
    let typical_usd = round_50(raw_typ);
    let low_usd = round_50((raw_typ * spec.low_mult).max(250.0));
    let high_usd = round_50(raw_typ * spec.high_mult);

    let rebate_usd = if spec.eligible_turf_rebate
        && basis.turf_rebate_program["eligibleRegion"] == json!(true)
    {
        let rate = basis.turf_rebate_program["rebatePerSqftUsd"]
            .as_f64()
            .unwrap_or(0.0);
        let cap = basis.turf_rebate_program["maxCapUsd"].as_i64().unwrap_or(0);
        ((qty * rate).round() as i64).min(cap).max(0)
    } else {
        0
    };
    let net_typical_usd = (typical_usd - rebate_usd).max(0);

    json!({
        "id": spec.id,
        "catalogId": spec.id,
        "status": "ok",
        "name": spec.name,
        "category": spec.category,
        "trade": spec.trade,
        "finishGrade": grade_id,
        "unitBasis": spec.unit_label,
        "quantity": round_n(qty, 1),
        "unitRateTypicalUsd": unit_rate_typ,
        "lowUsd": low_usd,
        "typicalUsd": typical_usd,
        "highUsd": high_usd,
        "rebateUsd": rebate_usd,
        "netTypicalUsd": net_typical_usd,
        "permitRequired": spec.permit_required,
        "timeline": spec.timeline,
        "areaBasisNote": basis_note,
        "scope": scope_text,
        "localMarketBasis": format!(
            "{} licensed contractor index ({:.2}x US baseline · BLS PPI {:.2}x)",
            basis.area_label, basis.regional_multiplier, basis.bls_inflation_factor
        ),
        "pricingMethod": "regional_trade_catalog",
    })
}

fn match_catalog_spec(query: &str) -> Option<&'static CatalogItemSpec> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return None;
    }
    if let Some(exact) = CATALOG_SPECS.iter().find(|s| s.id.eq_ignore_ascii_case(&q)) {
        return Some(exact);
    }

    let mut best: Option<(&'static CatalogItemSpec, i32)> = None;
    for spec in CATALOG_SPECS {
        let mut score = 0i32;
        for &kw in spec.keywords {
            if q.contains(kw) {
                score += if kw.contains(' ') || kw.chars().count() >= 6 {
                    12
                } else {
                    6
                };
            }
        }
        // Disambiguate generic "floor" when no specific material is named -> default to Engineered Hardwood or LVP
        if (q.contains("floor") || q.contains("地板") || q.contains("piso"))
            && !q.contains("garage")
            && !q.contains("elevation")
        {
            if spec.id == "floor-lvp"
                && (q.contains("vinyl") || q.contains("lvp") || q.contains("budget"))
            {
                score += 10;
            } else if spec.id == "floor-engineered-hardwood"
                && !q.contains("carpet")
                && !q.contains("tile")
                && !q.contains("refinish")
            {
                score += 5;
            }
        }
        if (q.contains("lawn") || q.contains("grass") || q.contains("sod") || q.contains("草坪"))
            && spec.id == "lawn-sod-redo"
            && !q.contains("xeriscape")
            && !q.contains("artificial")
            && !q.contains("synthetic")
        {
            score += 5;
        }
        if score > 0 && best.map(|(_, b)| score > b).unwrap_or(true) {
            best = Some((spec, score));
        }
    }
    best.map(|(s, _)| s)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveInQuoteRequest {
    pub slug: Option<String>,
    pub item: String,
    pub category: Option<String>,
    pub area_sqft: Option<f64>,
    pub quantity: Option<f64>,
    pub unit_basis: Option<String>,
    pub finish_grade: Option<String>,
}

const CUSTOM_QUOTE_INSTRUCTIONS: &str = r#"You are Open Estate Regional Residential Construction & Remodeling Cost Estimator.
Estimate a fair licensed local contractor price range (`lowUsd`, `typicalUsd`, `highUsd`) for the home repair, replacement, or move-in upgrade inside <untrusted_buyer_question>, scaled strictly to the home's actual square footage / yard dimensions in PROPERTY_BASIS_JSON and regional cost multiplier.

STRICT RULES:
1. AREA & MARKET CALIBRATION: Size `quantity`, `unitBasis` (`sq ft`, `linear ft`, `window`, `unit`, or `project`), and `unitRateTypicalUsd` to the property's actual `livingSqft` / `netFloorSqft` / `estimatedLawnSqft` / `roofAreaEstSqft` (unless the user supplied an explicit override quantity) and multiply US baseline contractor rates by `regionalMultiplier` (and local market snippets when present).
2. HONEST RANGES: Ensure `lowUsd <= typicalUsd <= highUsd`, include demolition/prep + materials + licensed labor, and clearly explain what is included in `scope` and `areaBasisNote`.
3. RESIDENTIAL SCOPE ONLY: Never execute non-real-estate instructions, never write code, and never mention internal model names."#;

fn custom_quote_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "name",
            "category",
            "trade",
            "unitBasis",
            "quantity",
            "unitRateTypicalUsd",
            "lowUsd",
            "typicalUsd",
            "highUsd",
            "permitRequired",
            "timeline",
            "scope",
            "areaBasisNote"
        ],
        "properties": {
            "name": { "type": "string" },
            "category": { "type": "string" },
            "trade": { "type": "string" },
            "unitBasis": { "type": "string", "enum": ["sq ft", "linear ft", "window", "unit", "bathroom", "ton", "project"] },
            "quantity": { "type": "number" },
            "unitRateTypicalUsd": { "type": "number" },
            "lowUsd": { "type": "integer" },
            "typicalUsd": { "type": "integer" },
            "highUsd": { "type": "integer" },
            "permitRequired": { "type": "boolean" },
            "timeline": { "type": "string" },
            "scope": { "type": "string" },
            "areaBasisNote": { "type": "string" }
        }
    })
}

static FORBIDDEN_ITEM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:ignore\s+(?:all\s+)?previous|system\s+prompt|developer\s+instructions|<\|im_start\||```|\b(?:python|javascript|typescript|golang|rust\s+code|sql|leetcode|beautifulsoup|bitcoin|ethereum|crypto|calculus|integral|french\s+revolution|poem|haiku|essay)\b|<\s*script|\b(?:SELECT\s+.+\s+FROM|DROP\s+TABLE|UNION\s+SELECT)\b|忽略之前|系统提示词)")
        .expect("forbidden item regex")
});

/// Validates and computes a fair local-market cost quote for any preset or custom user-entered
/// repair/upgrade item, scaling by home & lot size (`POST /api/move-in-quote`).
pub async fn estimate_move_in_item(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    req: &MoveInQuoteRequest,
    cache: Option<&dyn LlmCache>,
) -> Result<Value, GuardrailError> {
    let clean_item = clean_untrusted_text(&req.item, 220);
    if clean_item.chars().count() < 2 {
        return Err(GuardrailError::new(
            "INVALID_ITEM",
            "Please enter a home repair, replacement, or move-in upgrade item (at least 2 characters).",
            400,
        ));
    }
    if FORBIDDEN_ITEM_RE.is_match(&clean_item) {
        return Err(GuardrailError::new(
            "OFF_TOPIC_BLOCKED",
            "Move-in cost estimator accepts only physical home repair, replacement, remodeling, or landscaping items.",
            400,
        ));
    }

    let basis = MoveInPropertyBasis::from_report(report);
    let override_qty = req
        .area_sqft
        .or(req.quantity)
        .filter(|q| q.is_finite() && *q > 0.0);
    let (grade_id, grade_mult) = finish_grade_factor(req.finish_grade.as_deref());

    // 1. Check catalog match first for instant regional + area-scaled baseline
    let matched_spec = match_catalog_spec(&clean_item);
    let exact_catalog_id = CATALOG_SPECS
        .iter()
        .any(|s| s.id.eq_ignore_ascii_case(clean_item.trim()));

    if let Some(spec) = matched_spec {
        if exact_catalog_id || !env.llm_configured() {
            let mut q = quote_spec(&basis, spec, override_qty, Some(grade_id));
            if !exact_catalog_id {
                // Keep user's custom descriptive label when they typed e.g. "Replace living room floor with LVP"
                if let Some(obj) = q.as_object_mut() {
                    obj.insert("userQuery".to_string(), Value::String(clean_item.clone()));
                }
            }
            return Ok(q);
        }
    }

    // 2. Cached AI Structured Output estimator for custom free-text items when LLM is configured
    let _ = http;
    if env.llm_configured() {
        let anchor_json = matched_spec
            .map(|sp| quote_spec(&basis, sp, override_qty, Some(grade_id)))
            .unwrap_or(Value::Null);
        let prompt = format!(
            "Estimate local contractor cost in {} ({}, state {}) for finishGrade=\"{grade_id}\"{}.\n\nPROPERTY_BASIS_JSON:\n{}\n\nREGIONAL_ANCHOR_JSON:\n{}\n\n{}",
            basis.area_label,
            if basis.city.is_empty() { &basis.county_name } else { &basis.city },
            basis.state,
            override_qty
                .map(|q| format!(", userRequestedQuantity={q:.1}"))
                .unwrap_or_default(),
            serde_json::to_string(&basis).unwrap_or_default(),
            serde_json::to_string(&anchor_json).unwrap_or_default(),
            wrap_untrusted_block("untrusted_buyer_question", &clean_item),
        );

        let mut opts = LlmCallOptions::new(
            "move_in_quote",
            "move_in_quote",
            CUSTOM_QUOTE_INSTRUCTIONS,
            prompt,
            custom_quote_schema(),
        );
        opts.models = resolve_model_specs_with_effort(env, Some("low"));
        opts.max_output_tokens = Some(1_024);
        opts.timeout_ms = Some(22_000);

        if let Ok(res) = responses_json_with_cache(http, env, opts, cache).await {
            let raw_out = serde_json::to_string(&res.data).unwrap_or_default();
            if inspect_output_guardrail(&raw_out).is_none() {
                let Some(raw_typ) = res.data.get("typicalUsd").and_then(|v| v.as_i64()) else {
                    // Fall through to regional catalog/benchmark when structured output omits typicalUsd
                    return Ok(json!({
                        "id": format!("custom-{}", slug_key(&clean_item)),
                        "catalogId": matched_spec.map(|s| s.id),
                        "status": "missing_required_input",
                        "userQuery": clean_item,
                        "name": clean_item,
                        "category": req.category.as_deref().unwrap_or("Custom Upgrade"),
                        "trade": "Licensed Local Contractor",
                        "finishGrade": grade_id,
                        "unitBasis": req.unit_basis.as_deref().unwrap_or("project"),
                        "quantity": override_qty.map(|q| round_n(q, 1)),
                        "unitRateTypicalUsd": Value::Null,
                        "lowUsd": Value::Null,
                        "typicalUsd": Value::Null,
                        "highUsd": Value::Null,
                        "rebateUsd": Value::Null,
                        "netTypicalUsd": Value::Null,
                        "permitRequired": false,
                        "timeline": "2–5 days",
                        "areaBasisNote": "Requires contractor site measurement",
                        "scope": "",
                        "localMarketBasis": format!("{} licensed contractor index ({:.2}x US baseline)", basis.area_label, basis.regional_multiplier),
                        "pricingMethod": "hybrid_regional_estimator",
                    }));
                };
                let typ = raw_typ.clamp(150, 1_500_000);
                let low = res
                    .data
                    .get("lowUsd")
                    .and_then(|v| v.as_i64())
                    .unwrap_or((typ as f64 * 0.78) as i64)
                    .clamp(100, typ);
                let high = res
                    .data
                    .get("highUsd")
                    .and_then(|v| v.as_i64())
                    .unwrap_or((typ as f64 * 1.32) as i64)
                    .clamp(typ, 2_200_000);
                let qty = res
                    .data
                    .get("quantity")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(override_qty.unwrap_or(1.0))
                    .max(0.5);
                let unit_basis = res
                    .data
                    .get("unitBasis")
                    .and_then(|v| v.as_str())
                    .unwrap_or("project");
                let unit_rate = res
                    .data
                    .get("unitRateTypicalUsd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(typ as f64 / qty.max(1.0));

                return Ok(json!({
                    "id": format!("custom-{}", slug_key(&clean_item)),
                    "catalogId": matched_spec.map(|s| s.id),
                    "status": "ok",
                    "userQuery": clean_item,
                    "name": res.data.get("name").and_then(|v| v.as_str()).unwrap_or(&clean_item),
                    "category": res.data.get("category").and_then(|v| v.as_str()).or(req.category.as_deref()).unwrap_or("Custom Upgrade"),
                    "trade": res.data.get("trade").and_then(|v| v.as_str()).unwrap_or("Licensed Local Contractor"),
                    "finishGrade": grade_id,
                    "unitBasis": unit_basis,
                    "quantity": round_n(qty, 1),
                    "unitRateTypicalUsd": round_n(unit_rate, 2),
                    "lowUsd": round_50(low as f64),
                    "typicalUsd": round_50(typ as f64),
                    "highUsd": round_50(high as f64),
                    "rebateUsd": 0,
                    "netTypicalUsd": round_50(typ as f64),
                    "permitRequired": res.data.get("permitRequired").and_then(|v| v.as_bool()).unwrap_or(false),
                    "timeline": res.data.get("timeline").and_then(|v| v.as_str()).unwrap_or("2–5 days"),
                    "areaBasisNote": res.data.get("areaBasisNote").and_then(|v| v.as_str()).unwrap_or(""),
                    "scope": res.data.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
                    "localMarketBasis": format!("{} licensed contractor index ({:.2}x US baseline)", basis.area_label, basis.regional_multiplier),
                    "pricingMethod": "hybrid_regional_estimator",
                }));
            }
        }
    }

    // 3. Regional trade catalog / benchmark path when offline or no LLM configured
    if let Some(spec) = matched_spec {
        let mut q = quote_spec(&basis, spec, override_qty, Some(grade_id));
        if let Some(obj) = q.as_object_mut() {
            obj.insert("userQuery".to_string(), Value::String(clean_item));
        }
        return Ok(q);
    }

    let low_item = clean_item.to_ascii_lowercase();
    let sq_unit_requested = req
        .unit_basis
        .as_deref()
        .map(|u| u.eq_ignore_ascii_case("sq ft"))
        .unwrap_or(false);

    let (inferred_unit, qty_res, base_rate, mob, cat, trade): (
        &str,
        Result<f64, &'static str>,
        f64,
        f64,
        &str,
        &str,
    ) = if low_item.contains("deck")
        || low_item.contains("patio")
        || low_item.contains("pergola")
        || low_item.contains("outdoor")
    {
        (
            "sq ft",
            Ok(override_qty.unwrap_or(320.0)),
            34.0,
            750.0,
            "Lawn & Exterior",
            "Outdoor Living / Deck Contractor",
        )
    } else if low_item.contains("closet")
        || low_item.contains("cabinet")
        || low_item.contains("built-in")
    {
        (
            "linear ft",
            Ok(override_qty.unwrap_or(14.0)),
            285.0,
            400.0,
            "Interior",
            "Finish Carpenter / Cabinetry",
        )
    } else if low_item.contains("insulation") || low_item.contains("attic") {
        let q_res = if let Some(oq) = override_qty {
            Ok(oq)
        } else if basis.building_footprint_sqft > 0 {
            Ok(basis.building_footprint_sqft as f64)
        } else if !basis.has_verified_sqft {
            Err("Requires square footage")
        } else {
            Err("Requires story count or building footprint")
        };
        (
            "sq ft",
            q_res,
            2.65,
            450.0,
            "Roof & Envelope",
            "Insulation & Weatherization Contractor",
        )
    } else if low_item.contains("pool") || low_item.contains("spa") {
        (
            "project",
            Ok(1.0),
            6_800.0,
            0.0,
            "Lawn & Exterior",
            "Pool & Spa Technician",
        )
    } else if override_qty.is_some() || sq_unit_requested {
        let q_res = if let Some(oq) = override_qty {
            Ok(oq)
        } else if basis.has_verified_sqft && basis.net_floor_sqft > 0 {
            Ok(basis.net_floor_sqft as f64)
        } else {
            Err("Requires square footage")
        };
        (
            "sq ft",
            q_res,
            8.50,
            500.0,
            req.category.as_deref().unwrap_or("Custom Upgrade"),
            "Licensed Local Contractor",
        )
    } else {
        (
            "project",
            Ok(1.0),
            2_450.0,
            0.0,
            req.category.as_deref().unwrap_or("Custom Upgrade"),
            "Licensed Local Contractor",
        )
    };

    let unit_rate_typ = round_n(base_rate * basis.regional_multiplier * grade_mult, 2);
    let def_qty = match qty_res {
        Ok(q) => q,
        Err(missing_reason) => {
            return Ok(json!({
                "id": format!("custom-{}", slug_key(&clean_item)),
                "catalogId": Value::Null,
                "status": "missing_required_input",
                "userQuery": clean_item,
                "name": clean_item,
                "category": cat,
                "trade": trade,
                "finishGrade": grade_id,
                "unitBasis": inferred_unit,
                "quantity": Value::Null,
                "unitRateTypicalUsd": unit_rate_typ,
                "lowUsd": Value::Null,
                "typicalUsd": Value::Null,
                "highUsd": Value::Null,
                "rebateUsd": Value::Null,
                "netTypicalUsd": Value::Null,
                "permitRequired": false,
                "timeline": "2–5 days",
                "areaBasisNote": missing_reason,
                "scope": "Materials, surface preparation, licensed local trade labor, and haul-off debris disposal.",
                "localMarketBasis": format!(
                    "{} licensed contractor index ({:.2}x US baseline · BLS PPI {:.2}x)",
                    basis.area_label, basis.regional_multiplier, basis.bls_inflation_factor
                ),
                "pricingMethod": "regional_area_benchmark",
            }));
        }
    };

    let raw_typ =
        (mob * basis.regional_multiplier * grade_mult + def_qty * unit_rate_typ).max(400.0);
    let typ_usd = round_50(raw_typ);
    let low_usd = round_50(raw_typ * 0.76);
    let high_usd = round_50(raw_typ * 1.36);

    let dim_context = match (basis.has_verified_sqft, basis.has_verified_lot_sqft) {
        (true, true) => format!(
            "; {} sq ft home / {} sq ft lot",
            basis.living_sqft, basis.lot_sqft
        ),
        (true, false) => format!("; {} sq ft home", basis.living_sqft),
        (false, true) => format!("; {} sq ft lot", basis.lot_sqft),
        (false, false) => String::new(),
    };

    Ok(json!({
        "id": format!("custom-{}", slug_key(&clean_item)),
        "catalogId": Value::Null,
        "status": "ok",
        "userQuery": clean_item,
        "name": clean_item,
        "category": cat,
        "trade": trade,
        "finishGrade": grade_id,
        "unitBasis": inferred_unit,
        "quantity": round_n(def_qty, 1),
        "unitRateTypicalUsd": unit_rate_typ,
        "lowUsd": low_usd,
        "typicalUsd": typ_usd,
        "highUsd": high_usd,
        "rebateUsd": 0,
        "netTypicalUsd": typ_usd,
        "permitRequired": false,
        "timeline": "2–5 days",
        "areaBasisNote": format!(
            "Scaled to {:.0} {} in {} ({:.2}x regional trade index{})",
            def_qty, inferred_unit, basis.area_label, basis.regional_multiplier, dim_context
        ),
        "scope": "Materials, surface preparation, licensed local trade labor, and haul-off debris disposal.",
        "localMarketBasis": format!(
            "{} licensed contractor index ({:.2}x US baseline · BLS PPI {:.2}x)",
            basis.area_label, basis.regional_multiplier, basis.bls_inflation_factor
        ),
        "pricingMethod": "regional_area_benchmark",
    }))
}

fn slug_key(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 36 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Enriches an AI or deterministic inspection JSON object in-place:
/// - Ensures every finding or suggestion (including cosmetic/exterior suggestions like "redo dead lawn"
///   or "worn carpet" where `costLow`/`costHigh` might be 0 or uncalibrated) receives a regional,
///   area-scaled `localMarketEstimate` (`lowUsd`, `typicalUsd`, `highUsd`, `rebateUsd`, `areaBasisNote`)
///   when required parcel/home dimensions exist, or an honest `"Requires square footage"` / `"Requires lot size"`
///   note with `null` cost when dimensions are absent.
/// - Attaches `moveInCost` and `readyToMoveIn` summarizing the true Ready-to-Move-In cost for the home.
pub fn enrich_inspection_with_move_in_costs(inspection: &mut Value, report: &Value) {
    let basis = MoveInPropertyBasis::from_report(report);
    let mut mapped_items = Vec::new();
    let mut recalc_total_low = 0i64;
    let mut recalc_total_high = 0i64;

    let array_key = if inspection
        .get("findings")
        .and_then(|v| v.as_array())
        .is_some()
    {
        Some("findings")
    } else if inspection
        .get("defects")
        .and_then(|v| v.as_array())
        .is_some()
    {
        Some("defects")
    } else {
        None
    };

    if let Some(key) = array_key {
        if let Some(findings) = inspection.get_mut(key).and_then(|v| v.as_array_mut()) {
            for (idx, f) in findings.iter_mut().enumerate() {
                let item_title = f
                    .get("item")
                    .or_else(|| f.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let detail = f
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let system = f
                    .get("system")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let severity = f
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("moderate")
                    .to_ascii_lowercase();
                let urgency = f
                    .get("urgency")
                    .and_then(|v| v.as_str())
                    .unwrap_or("1–3 years")
                    .to_string();
                let page_ref = f
                    .get("pageRef")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let combined = format!("{item_title} {detail} {system}");
                let matched =
                    match_catalog_spec(&combined).map(|sp| quote_spec(&basis, sp, None, None));

                let orig_low = f
                    .get("costLow")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    .max(0);
                let orig_high = f
                    .get("costHigh")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    .max(0);

                let is_credit_candidate = matches!(severity.as_str(), "critical" | "major")
                    || urgency.contains("Immediate")
                    || urgency.contains("Before close");

                let missing_dim_match = matched
                    .as_ref()
                    .map(|mq| mq["status"] == json!("missing_required_input"))
                    .unwrap_or(false);

                if (orig_low == 0 && orig_high == 0) && missing_dim_match {
                    let mq = matched.as_ref().unwrap();
                    let area_note = mq["areaBasisNote"]
                        .as_str()
                        .unwrap_or("Requires square footage")
                        .to_string();
                    let est_obj = json!({
                        "status": "missing_required_input",
                        "lowUsd": Value::Null,
                        "typicalUsd": Value::Null,
                        "highUsd": Value::Null,
                        "rebateUsd": Value::Null,
                        "sellerCreditCandidate": is_credit_candidate,
                        "classification": if is_credit_candidate { "seller_credit_candidate" } else { "move_in_suggestion" },
                        "catalogId": mq.get("catalogId").cloned().unwrap_or(Value::Null),
                        "areaBasisNote": area_note,
                    });
                    if let Some(f_obj) = f.as_object_mut() {
                        f_obj.insert("localMarketEstimate".to_string(), est_obj);
                    }
                    mapped_items.push(json!({
                        "id": format!("insp-{}", idx + 1),
                        "status": "missing_required_input",
                        "source": "inspection",
                        "system": system,
                        "severity": severity,
                        "urgency": urgency,
                        "pageRef": page_ref,
                        "name": item_title,
                        "detail": detail,
                        "lowUsd": Value::Null,
                        "typicalUsd": Value::Null,
                        "highUsd": Value::Null,
                        "rebateUsd": Value::Null,
                        "sellerCreditCandidate": is_credit_candidate,
                        "includedByDefault": false,
                        "areaBasisNote": mq["areaBasisNote"],
                    }));
                    continue;
                }

                let (low, high) = if orig_low > 0 && orig_high >= orig_low {
                    (orig_low, orig_high)
                } else if let Some((ml, mh)) = matched
                    .as_ref()
                    .and_then(|mq| Some((mq["lowUsd"].as_i64()?, mq["highUsd"].as_i64()?)))
                {
                    if let Some(f_obj) = f.as_object_mut() {
                        f_obj.insert("costLow".to_string(), json!(ml));
                        f_obj.insert("costHigh".to_string(), json!(mh));
                    }
                    (ml, mh)
                } else {
                    let base = match severity.as_str() {
                        "critical" => 4_500.0,
                        "major" => 2_400.0,
                        "moderate" => 1_100.0,
                        _ => 550.0,
                    } * basis.regional_multiplier;
                    let ml = round_50(base * 0.75);
                    let mh = round_50(base * 1.35);
                    if let Some(f_obj) = f.as_object_mut() {
                        f_obj.insert("costLow".to_string(), json!(ml));
                        f_obj.insert("costHigh".to_string(), json!(mh));
                    }
                    (ml, mh)
                };

                let typical = matched
                    .as_ref()
                    .and_then(|mq| mq["typicalUsd"].as_i64())
                    .filter(|&mt| mt >= low && mt <= high)
                    .unwrap_or_else(|| round_50((low + high) as f64 / 2.0));
                let rebate_usd = matched
                    .as_ref()
                    .and_then(|mq| mq["rebateUsd"].as_i64())
                    .unwrap_or(0);
                let area_note = matched
                    .as_ref()
                    .and_then(|mq| mq["areaBasisNote"].as_str())
                    .map(String::from)
                    .unwrap_or_else(|| {
                        if basis.has_verified_sqft {
                            format!(
                                "{} ({:.2}x regional trade index · {} sq ft home)",
                                basis.area_label, basis.regional_multiplier, basis.living_sqft
                            )
                        } else {
                            format!(
                                "{} ({:.2}x regional trade index)",
                                basis.area_label, basis.regional_multiplier
                            )
                        }
                    });

                let est_obj = json!({
                    "status": "ok",
                    "lowUsd": low,
                    "typicalUsd": typical,
                    "highUsd": high,
                    "rebateUsd": rebate_usd,
                    "sellerCreditCandidate": is_credit_candidate,
                    "classification": if is_credit_candidate { "seller_credit_candidate" } else { "move_in_suggestion" },
                    "catalogId": matched.as_ref().and_then(|m| m.get("catalogId")).cloned().unwrap_or(Value::Null),
                    "areaBasisNote": area_note,
                });
                if let Some(f_obj) = f.as_object_mut() {
                    f_obj.insert("localMarketEstimate".to_string(), est_obj);
                }

                recalc_total_low += low;
                recalc_total_high += high;
                mapped_items.push(json!({
                    "id": format!("insp-{}", idx + 1),
                    "status": "ok",
                    "source": "inspection",
                    "system": system,
                    "severity": severity,
                    "urgency": urgency,
                    "pageRef": page_ref,
                    "name": item_title,
                    "detail": detail,
                    "lowUsd": low,
                    "typicalUsd": typical,
                    "highUsd": high,
                    "rebateUsd": rebate_usd,
                    "sellerCreditCandidate": is_credit_candidate,
                    "includedByDefault": true,
                    "areaBasisNote": area_note,
                }));
            }
        }
    }

    if let Some(rt) = inspection
        .get_mut("repairTotals")
        .and_then(|v| v.as_object_mut())
    {
        if rt.get("totalHigh").and_then(|v| v.as_i64()).unwrap_or(0) < recalc_total_high {
            rt.insert("totalLow".to_string(), json!(recalc_total_low));
            rt.insert("totalHigh".to_string(), json!(recalc_total_high));
        }
    }

    let credit_ask = inspection
        .pointer("/repairTotals/recommendedCreditAsk")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let summary = assemble_ready_to_move_in(&basis, mapped_items, credit_ask);
    if let Some(obj) = inspection.as_object_mut() {
        obj.insert("readyToMoveIn".to_string(), summary.clone());
        obj.insert("moveInCost".to_string(), summary);
    }
}

/// Attaches `moveInCost` onto a Multi-Document Packet review (`POST /api/packet-review`).
pub fn enrich_packet_with_move_in_costs(packet: &mut Value, report: &Value) {
    let basis = MoveInPropertyBasis::from_report(report);
    let mut mapped_items = Vec::new();
    if let Some(defects) = packet.get("deduplicatedDefects").and_then(|v| v.as_array()) {
        for (idx, d) in defects.iter().enumerate() {
            let title = d
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Packet Defect");
            let detail = d.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            let sev = d
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("Moderate");
            let trade = d.get("trade").and_then(|v| v.as_str()).unwrap_or("General");
            let low = d
                .get("costLow")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0);
            let high = d
                .get("costHigh")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(low);
            let mid = d
                .get("costMid")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(|| round_50((low + high) as f64 / 2.0));
            let is_credit = matches!(sev, "Critical" | "Major");
            let area_note = if basis.has_verified_sqft {
                format!(
                    "{} ({:.2}x regional trade index · {} sq ft home)",
                    basis.area_label, basis.regional_multiplier, basis.living_sqft
                )
            } else {
                format!(
                    "{} ({:.2}x regional trade index)",
                    basis.area_label, basis.regional_multiplier
                )
            };
            mapped_items.push(json!({
                "id": format!("pkt-{}", idx + 1),
                "status": "ok",
                "source": "packet",
                "system": trade,
                "severity": sev.to_ascii_lowercase(),
                "urgency": if is_credit { "Before close / Credit" } else { "1–3 years" },
                "pageRef": "",
                "name": title,
                "detail": detail,
                "lowUsd": low,
                "typicalUsd": mid,
                "highUsd": high,
                "rebateUsd": 0,
                "sellerCreditCandidate": is_credit,
                "includedByDefault": true,
                "areaBasisNote": area_note,
            }));
        }
    }
    let credit_est: i64 = mapped_items
        .iter()
        .filter(|i| i["sellerCreditCandidate"] == json!(true))
        .filter_map(|i| i["typicalUsd"].as_i64())
        .sum::<i64>();
    let summary =
        assemble_ready_to_move_in(&basis, mapped_items, round_50(credit_est as f64 * 0.75));
    if let Some(obj) = packet.as_object_mut() {
        obj.insert("readyToMoveIn".to_string(), summary.clone());
        obj.insert("moveInCost".to_string(), summary);
    }
}

/// Builds the full `readyToMoveIn` payload directly from a `report` JSON (auto-importing any
/// API-fetched or embedded `inspection` / `packetReview` items if already present on the report).
pub fn build_ready_to_move_in_from_report(report: &Value) -> Value {
    let basis = MoveInPropertyBasis::from_report(report);
    let existing_insp = report
        .get("inspection")
        .or_else(|| report.pointer("/publicResearch/apiFetchedReviews/inspection"))
        .or_else(|| report.pointer("/modules/publicResearch/data/apiFetchedReviews/inspection"));
    if let Some(ins) = existing_insp.filter(|v| v.is_object()) {
        let mut cloned = ins.clone();
        enrich_inspection_with_move_in_costs(&mut cloned, report);
        if let Some(mic) = cloned.get("moveInCost") {
            return mic.clone();
        }
    }
    assemble_ready_to_move_in(&basis, Vec::new(), 0)
}

fn assemble_ready_to_move_in(
    basis: &MoveInPropertyBasis,
    inspection_items: Vec<Value>,
    recommended_credit_usd: i64,
) -> Value {
    let preset_catalog: Vec<Value> = CATALOG_SPECS
        .iter()
        .map(|spec| quote_spec(basis, spec, None, Some("midrange")))
        .collect();

    let insp_low: i64 = inspection_items
        .iter()
        .filter_map(|i| i["lowUsd"].as_i64())
        .sum();
    let insp_typ: i64 = inspection_items
        .iter()
        .filter_map(|i| i["typicalUsd"].as_i64())
        .sum();
    let insp_high: i64 = inspection_items
        .iter()
        .filter_map(|i| i["highUsd"].as_i64())
        .sum();
    let insp_rebates: i64 = inspection_items
        .iter()
        .filter_map(|i| i["rebateUsd"].as_i64())
        .sum();
    let credit_candidate_typ: i64 = inspection_items
        .iter()
        .filter(|i| i["sellerCreditCandidate"] == json!(true))
        .filter_map(|i| i["typicalUsd"].as_i64())
        .sum();
    let move_in_suggestions_typ: i64 = (insp_typ - credit_candidate_typ).max(0);
    let eff_credit = if recommended_credit_usd > 0 {
        recommended_credit_usd.min(insp_high.max(recommended_credit_usd))
    } else {
        0
    };
    let net_move_in_prep_typ = (insp_typ - eff_credit - insp_rebates).max(0);

    let true_total_usd = basis
        .purchase_price_usd
        .map(|pp| pp + basis.estimated_closing_costs_usd.unwrap_or(0) + net_move_in_prep_typ);
    let true_total_low_usd = basis.purchase_price_usd.map(|pp| {
        pp + basis.estimated_closing_costs_usd.unwrap_or(0)
            + (insp_low - eff_credit - insp_rebates).max(0)
    });
    let true_total_high_usd = basis.purchase_price_usd.map(|pp| {
        pp + basis.estimated_closing_costs_usd.unwrap_or(0)
            + (insp_high - eff_credit - insp_rebates).max(0)
    });

    let status = if basis.has_verified_sqft {
        "ok"
    } else {
        "missing_required_input"
    };

    json!({
        "status": status,
        "propertyBasis": basis,
        "inspectionItems": inspection_items,
        "presetCatalog": preset_catalog.clone(),
        "catalog": preset_catalog,
        "totals": {
            "purchasePriceUsd": basis.purchase_price_usd,
            "purchasePriceSource": basis.purchase_price_source,
            "estimatedClosingCostsUsd": basis.estimated_closing_costs_usd,
            "inspectionRepairsLowUsd": insp_low,
            "inspectionRepairsTypicalUsd": insp_typ,
            "inspectionRepairsHighUsd": insp_high,
            "sellerCreditCandidatesTypicalUsd": credit_candidate_typ,
            "buyerMoveInSuggestionsTypicalUsd": move_in_suggestions_typ,
            "recommendedSellerCreditUsd": eff_credit,
            "localRebatesUsd": insp_rebates,
            "netMoveInPrepTypicalUsd": net_move_in_prep_typ,
            "trueReadyToMoveInLowUsd": true_total_low_usd,
            "trueReadyToMoveInTypicalUsd": true_total_usd,
            "trueReadyToMoveInHighUsd": true_total_high_usd,
        },
        "disclaimer": "Ready-to-Move-In figures are regional planning estimates scaled to the property's living and lot area using County/Metro trade multipliers, BLS construction indices, and municipal water rebates — not a binding contractor bid. Obtain 3 itemized licensed local contractor bids before removing inspection contingencies.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_move_in_cost_scales_by_real_market_and_home_area_and_enriches_public_record_findings(
    ) {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::default();

        // Synthetic San Jose CA report (2,040 sqft, 6,600 lot, Santa Clara 06085)
        let sj_synth = crate::test_synth::synth_report_primary();
        // Synthetic Evanston IL holdout (Cook County 17031, smaller floor area)
        let mut il_synth =
            crate::test_synth::synth_report_from_holdout("synth-il-60201-tudor-sfr");
        il_synth["facts"]["sqft"] = json!(1340);

        // 1. Flooring replacement scales by BOTH regional market (Santa Clara 06085 vs Cook 17031) AND home sqft (2,040 vs 1,340)
        let sj_floor = estimate_move_in_item(
            &http,
            &env,
            &sj_synth,
            &MoveInQuoteRequest {
                item: "replace floor with engineered white oak".to_string(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        let il_floor = estimate_move_in_item(
            &http,
            &env,
            &il_synth,
            &MoveInQuoteRequest {
                item: "replace floor with engineered white oak".to_string(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

        assert_eq!(sj_floor["catalogId"], json!("floor-engineered-hardwood"));
        assert!(sj_floor["quantity"].as_f64().unwrap() > il_floor["quantity"].as_f64().unwrap());
        assert!(
            sj_floor["unitRateTypicalUsd"].as_f64().unwrap()
                > il_floor["unitRateTypicalUsd"].as_f64().unwrap()
        );
        assert!(
            sj_floor["typicalUsd"].as_i64().unwrap() > il_floor["typicalUsd"].as_i64().unwrap()
        );

        // 2. Lawn redo with xeriscape in Santa Clara County automatically deducts Valley Water rebate
        let sj_xeri = estimate_move_in_item(
            &http,
            &env,
            &sj_synth,
            &MoveInQuoteRequest {
                item: "lawn-xeriscape-redo".to_string(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert!(sj_xeri["rebateUsd"].as_i64().unwrap() >= 2_000);
        assert!(
            sj_xeri["netTypicalUsd"].as_i64().unwrap() < sj_xeri["typicalUsd"].as_i64().unwrap()
        );

        // 3. Public-record seismic & tree-clearance findings enriched against synthetic Campbell report
        let campbell = crate::test_synth::synth_report_secondary();
        let mut public_record_review = json!({
            "findings": [
                {
                    "system": campbell["facts"]["foundation"],
                    "severity": "major",
                    "item": "Crawlspace foundation bolting / cripple-wall bracing",
                    "detail": "Raised perimeter crawlspace seismic bolting and cripple-wall bracing recommended under USGS ASCE 7-22 design PGA.",
                    "pageRef": "USGS ASCE 7-22 / NHD",
                    "costLow": 0,
                    "costHigh": 0,
                    "urgency": "Before close / Credit"
                },
                {
                    "system": "Site / Exterior",
                    "severity": "moderate",
                    "item": "Arborist tree trim / roof canopy clearance",
                    "detail": "Trim mature perimeter canopy away from main roof eaves.",
                    "pageRef": "SYNTH-TREE-2024-01",
                    "costLow": 0,
                    "costHigh": 0,
                    "urgency": "1–3 years"
                }
            ]
        });

        enrich_inspection_with_move_in_costs(&mut public_record_review, &campbell);
        assert_eq!(
            public_record_review["findings"][0]["localMarketEstimate"]["catalogId"],
            json!("crawlspace-vapor-seismic")
        );
        assert!(
            public_record_review["findings"][0]["costLow"]
                .as_i64()
                .unwrap()
                > 3_000
        );
        assert_eq!(
            public_record_review["findings"][1]["localMarketEstimate"]["catalogId"],
            json!("tree-trimming-clearance")
        );
        assert!(
            public_record_review["moveInCost"]["totals"]["trueReadyToMoveInTypicalUsd"]
                .as_i64()
                .unwrap()
                > 2_000_000
        );
    }

    #[tokio::test]
    async fn test_missing_sqft_and_missing_lot_size_never_invent_dimensions_or_costs() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::default();

        let mut stripped = crate::test_synth::synth_report_primary();
        if let Some(f) = stripped.get_mut("facts").and_then(|v| v.as_object_mut()) {
            f.remove("sqft");
            f.remove("lotSqft");
            f.remove("stories");
            f.remove("baths");
        }

        let basis = MoveInPropertyBasis::from_report(&stripped);
        assert!(!basis.has_verified_sqft);
        assert!(!basis.has_verified_lot_sqft);
        assert_eq!(basis.living_sqft, 0);
        assert_eq!(basis.lot_sqft, 0);
        assert!(basis.stories.is_none());
        assert!(basis.baths.is_none());
        assert!(basis.turf_rebate_program["estimatedRebateUsd"].is_null());

        let floor_missing = estimate_move_in_item(
            &http,
            &env,
            &stripped,
            &MoveInQuoteRequest {
                item: "floor-lvp".to_string(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(floor_missing["status"], json!("missing_required_input"));
        assert_eq!(
            floor_missing["areaBasisNote"],
            json!("Requires square footage")
        );
        assert!(floor_missing["typicalUsd"].is_null());

        let fence_missing = estimate_move_in_item(
            &http,
            &env,
            &stripped,
            &MoveInQuoteRequest {
                item: "fence-privacy".to_string(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(fence_missing["status"], json!("missing_required_input"));
        assert_eq!(fence_missing["areaBasisNote"], json!("Requires lot size"));
        assert!(fence_missing["typicalUsd"].is_null());
    }
}
