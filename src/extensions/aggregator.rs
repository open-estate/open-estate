//! Deterministic multi-extension Protocol Buffers aggregator (`openestate.extension.v1`).
//!
//! Merges `Vec<ExtensionEnvelopeProto>` across all enabled extensions without vendor-specific
//! glue code, enforcing strict provenance ranking and zero-mock data honesty:
//! - Utilities (`electric`, `gas`, `water_sewer_trash`): `MERGE_GEO_MOST_SPECIFIC` — highest
//!   `(geo_specificity, provenance_rank, priority)` wins so a 5-digit ZIP or municipal utility
//!   automatically overrides a county or state IOU.
//! - Broadband (`broadband`): `MERGE_UNION_DEDUP` — unions carrier options across live FCC BDC
//!   and regional carrier footprints, upgrading regional footprints when `PROVENANCE_LIVE_VERIFIED`
//!   observations exist for the same carrier brand + technology tier.
//! - Valuation & Market (`valuation`, `market_trends`): `MERGE_CONSENSUS_MEDIAN` across verified
//!   numeric estimates.
//! - Core Property / Hazards / Environment / Livability: `MERGE_HIGHEST_PRIORITY` non-destructive
//!   field merge where missing fields remain `None` (`null` / SQL `NULL`).

use crate::extensions::proto::{
    extension_envelope_proto::Payload, provenance_rank, AggregatedPropertyBundleProto,
    BroadbandCatalogProto, BroadbandOptionProto, CapabilityDomain, CodeEnforcementIntelProto,
    DomainAggregationPolicyProto, DomainResolutionTraceProto, EnvironmentalSummaryProto,
    ExtensionEnvelopeProto, ExtensionQueryProto, GeocodeResultProto, HazardSummaryProto,
    House3dMetadataProto, InsuranceQuoteProto, ListingShoppingFacetsProto, LivabilitySummaryProto,
    MarketValuationProto, MepIntelProto, MergeStrategy, PermitRecordProto, PermitsHistoryProto,
    PropertyFactsProto, ProvenanceTier, RemodelIntelProto, RoofIntelProto, RoutingMatrixProto,
    SolarIntelProto, SourceAttributionProto, TaxMandatesProto,
};
use crate::extensions::yaml_runner::normalize_carrier_brand_key;
use crate::shared::{monthly_pi, now_iso};
use serde_json::{json, Map, Value};

/// Returns the canonical default `DomainAggregationPolicyProto` for a `CapabilityDomain`.
pub fn default_policy_for_domain(domain: CapabilityDomain) -> DomainAggregationPolicyProto {
    match domain {
        CapabilityDomain::DomainElectricUtility
        | CapabilityDomain::DomainGasUtility
        | CapabilityDomain::DomainWaterSewerTrash => DomainAggregationPolicyProto {
            domain: domain as i32,
            strategy: MergeStrategy::MergeGeoMostSpecific as i32,
            prefer_live_verified: true,
            min_consensus_sources: 1,
            dedup_key_fields: Vec::new(),
            allow_regional_footprint_fallback: true,
        },
        CapabilityDomain::DomainBroadband => DomainAggregationPolicyProto {
            domain: domain as i32,
            strategy: MergeStrategy::MergeUnionDedup as i32,
            prefer_live_verified: true,
            min_consensus_sources: 1,
            dedup_key_fields: vec!["carrier_name".to_string(), "tier_category".to_string()],
            allow_regional_footprint_fallback: true,
        },
        CapabilityDomain::DomainPermits => DomainAggregationPolicyProto {
            domain: domain as i32,
            strategy: MergeStrategy::MergeUnionDedup as i32,
            prefer_live_verified: true,
            min_consensus_sources: 1,
            dedup_key_fields: vec!["permit_number".to_string(), "issue_date".to_string()],
            allow_regional_footprint_fallback: false,
        },
        CapabilityDomain::DomainValuation | CapabilityDomain::DomainMarketTrends => {
            DomainAggregationPolicyProto {
                domain: domain as i32,
                strategy: MergeStrategy::MergeConsensusMedian as i32,
                prefer_live_verified: true,
                min_consensus_sources: 2,
                dedup_key_fields: Vec::new(),
                allow_regional_footprint_fallback: false,
            }
        }
        _ => DomainAggregationPolicyProto {
            domain: domain as i32,
            strategy: MergeStrategy::MergeHighestPriority as i32,
            prefer_live_verified: true,
            min_consensus_sources: 1,
            dedup_key_fields: Vec::new(),
            allow_regional_footprint_fallback: true,
        },
    }
}

fn envelope_provenance(env: &ExtensionEnvelopeProto) -> ProvenanceTier {
    env.attribution
        .as_ref()
        .and_then(|a| ProvenanceTier::try_from(a.provenance_tier).ok())
        .unwrap_or(ProvenanceTier::ProvenanceUnspecified)
}

fn geo_most_specific_key(env: &ExtensionEnvelopeProto, prefer_live: bool) -> (i32, i32, i32) {
    let prov = if prefer_live {
        provenance_rank(envelope_provenance(env))
    } else {
        0
    };
    (env.geo_specificity, prov, env.priority)
}

fn highest_priority_key(env: &ExtensionEnvelopeProto, prefer_live: bool) -> (i32, i32, i32) {
    let prov = if prefer_live {
        provenance_rank(envelope_provenance(env))
    } else {
        0
    };
    (prov, env.priority, env.geo_specificity)
}

fn resolve_policy(
    domain: CapabilityDomain,
    custom_policies: Option<&[DomainAggregationPolicyProto]>,
) -> DomainAggregationPolicyProto {
    if let Some(list) = custom_policies {
        if let Some(found) = list.iter().find(|p| p.domain == domain as i32) {
            return found.clone();
        }
    }
    default_policy_for_domain(domain)
}

fn record_trace(
    domain: CapabilityDomain,
    strategy: MergeStrategy,
    sorted: &[&ExtensionEnvelopeProto],
) -> Option<DomainResolutionTraceProto> {
    let winner = sorted.first()?;
    let mut contributors = Vec::new();
    for e in sorted {
        if !e.extension_id.is_empty() && !contributors.contains(&e.extension_id) {
            contributors.push(e.extension_id.clone());
        }
    }
    Some(DomainResolutionTraceProto {
        domain: domain as i32,
        strategy_used: strategy as i32,
        winning_extension_id: winner.extension_id.clone(),
        contributing_extension_ids: contributors,
        winning_geo_specificity: winner.geo_specificity,
        winning_priority: winner.priority,
        winning_provenance: envelope_provenance(winner) as i32,
        wave_index: 0,
        skipped_extension_ids: Vec::new(),
    })
}

/// Deterministically merges `envelopes` across all enabled extensions into a single
/// `AggregatedPropertyBundleProto` with full provenance citations and audit traces.
pub fn aggregate_envelopes(
    query: &ExtensionQueryProto,
    envelopes: &[ExtensionEnvelopeProto],
    custom_policies: Option<&[DomainAggregationPolicyProto]>,
) -> AggregatedPropertyBundleProto {
    let mut traces: Vec<DomainResolutionTraceProto> = Vec::new();
    let mut attributions: Vec<SourceAttributionProto> = Vec::new();

    for env in envelopes {
        if let Some(ref attr) = env.attribution {
            let dup = attributions.iter().any(|a| {
                a.extension_id == attr.extension_id
                    && a.source_name == attr.source_name
                    && a.source_url == attr.source_url
            });
            if !dup {
                attributions.push(attr.clone());
            }
        }
    }

    // 1. Geocode (`MERGE_HIGHEST_PRIORITY`)
    let geocode = {
        let pol = resolve_policy(CapabilityDomain::DomainGeocode, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::Geocode(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainGeocode,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_geocode(&cands)
    };

    // 2. Property Facts (`MERGE_HIGHEST_PRIORITY`)
    let property_facts = {
        let pol = resolve_policy(CapabilityDomain::DomainPropertyFacts, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::PropertyFacts(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainPropertyFacts,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_property_facts(&cands)
    };

    // 3. Environmental (`MERGE_HIGHEST_PRIORITY`) — resolved before Water so live SDWIS PWSID enriches Water!
    let environmental_summary = {
        let pol = resolve_policy(CapabilityDomain::DomainEnvironmental, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::EnvironmentalSummary(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainEnvironmental,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_environmental(&cands)
    };

    // 4. Electric Utility (`MERGE_GEO_MOST_SPECIFIC`)
    let electric_utility = {
        let pol = resolve_policy(CapabilityDomain::DomainElectricUtility, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::ElectricUtility(_))))
            .collect();
        cands.sort_by(|a, b| {
            geo_most_specific_key(b, pol.prefer_live_verified)
                .cmp(&geo_most_specific_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainElectricUtility,
            MergeStrategy::MergeGeoMostSpecific,
            &cands,
        ) {
            traces.push(tr);
        }
        cands.first().and_then(|e| match &e.payload {
            Some(Payload::ElectricUtility(p)) => Some(p.clone()),
            _ => None,
        })
    };

    // 5. Gas Utility (`MERGE_GEO_MOST_SPECIFIC`)
    let gas_utility = {
        let pol = resolve_policy(CapabilityDomain::DomainGasUtility, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::GasUtility(_))))
            .collect();
        cands.sort_by(|a, b| {
            geo_most_specific_key(b, pol.prefer_live_verified)
                .cmp(&geo_most_specific_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainGasUtility,
            MergeStrategy::MergeGeoMostSpecific,
            &cands,
        ) {
            traces.push(tr);
        }
        cands.first().and_then(|e| match &e.payload {
            Some(Payload::GasUtility(p)) => Some(p.clone()),
            _ => None,
        })
    };

    // 6. Water / Sewer / Trash (`MERGE_GEO_MOST_SPECIFIC` + live EPA SDWIS enrichment)
    let water_sewer_trash = {
        let pol = resolve_policy(CapabilityDomain::DomainWaterSewerTrash, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::WaterSewerTrash(_))))
            .collect();
        cands.sort_by(|a, b| {
            geo_most_specific_key(b, pol.prefer_live_verified)
                .cmp(&geo_most_specific_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainWaterSewerTrash,
            MergeStrategy::MergeGeoMostSpecific,
            &cands,
        ) {
            traces.push(tr);
        }
        let mut wst = cands.first().and_then(|e| match &e.payload {
            Some(Payload::WaterSewerTrash(p)) => Some(p.clone()),
            _ => None,
        });
        if let (Some(ref mut w), Some(ref env_s)) = (&mut wst, &environmental_summary) {
            if w.water_pwsid.is_none() {
                w.water_pwsid = env_s.water_system_pwsid.clone();
            }
            if let Some(ref live_name) = env_s.water_system_name {
                if !live_name.trim().is_empty() && w.water_source_type != "private_well" {
                    w.water_carrier_name = live_name.clone();
                }
            }
        }
        wst
    };

    // 7. Broadband (`MERGE_UNION_DEDUP`)
    let broadband_catalog = {
        let pol = resolve_policy(CapabilityDomain::DomainBroadband, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| {
                matches!(
                    e.payload,
                    Some(Payload::BroadbandOption(_)) | Some(Payload::BroadbandCatalog(_))
                )
            })
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainBroadband,
            MergeStrategy::MergeUnionDedup,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_broadband(query, &cands, &pol)
    };

    // 8. Natural Hazards (`MERGE_HIGHEST_PRIORITY`)
    let hazard_summary = {
        let pol = resolve_policy(CapabilityDomain::DomainNaturalHazards, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::HazardSummary(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainNaturalHazards,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_hazards(&cands)
    };

    // 9. Livability / Schools / Crime (`MERGE_HIGHEST_PRIORITY`)
    let livability_summary = {
        let pol = resolve_policy(CapabilityDomain::DomainLivability, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::LivabilitySummary(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainLivability,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_livability(&cands)
    };

    // 10. Market & Valuation (`MERGE_CONSENSUS_MEDIAN`)
    let market_valuation = {
        let pol = resolve_policy(CapabilityDomain::DomainValuation, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::MarketValuation(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainValuation,
            MergeStrategy::MergeConsensusMedian,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_market_valuation(&cands, pol.prefer_live_verified)
    };

    // 11. Insurance (`MERGE_HIGHEST_PRIORITY`)
    let insurance_quote = {
        let pol = resolve_policy(CapabilityDomain::DomainInsurance, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::InsuranceQuote(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainInsurance,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_insurance(&cands)
    };

    // 12. Tax Mandates (`MERGE_HIGHEST_PRIORITY`)
    let tax_mandates = {
        let pol = resolve_policy(CapabilityDomain::DomainTaxMandates, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::TaxMandates(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainTaxMandates,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_tax_mandates(&cands)
    };

    // 13. House 3D (`MERGE_HIGHEST_PRIORITY`)
    let house_3d = {
        let pol = resolve_policy(CapabilityDomain::DomainHouse3d, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::House3d(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainHouse3d,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_house3d(&cands)
    };

    // 14. Routing Matrix (`MERGE_HIGHEST_PRIORITY`)
    let routing_matrix: Option<RoutingMatrixProto> = {
        let pol = resolve_policy(CapabilityDomain::DomainRouting, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::RoutingMatrix(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainRouting,
            MergeStrategy::MergeHighestPriority,
            &cands,
        ) {
            traces.push(tr);
        }
        cands.first().and_then(|e| match &e.payload {
            Some(Payload::RoutingMatrix(rm)) => Some(rm.clone()),
            _ => None,
        })
    };

    // 15. Stage-1 Building Permits (`MERGE_UNION_DEDUP`)
    let permits_history: Option<PermitsHistoryProto> = {
        let pol = resolve_policy(CapabilityDomain::DomainPermits, custom_policies);
        let mut cands: Vec<&ExtensionEnvelopeProto> = envelopes
            .iter()
            .filter(|e| matches!(e.payload, Some(Payload::PermitsHistory(_))))
            .collect();
        cands.sort_by(|a, b| {
            highest_priority_key(b, pol.prefer_live_verified)
                .cmp(&highest_priority_key(a, pol.prefer_live_verified))
        });
        if let Some(tr) = record_trace(
            CapabilityDomain::DomainPermits,
            MergeStrategy::MergeUnionDedup,
            &cands,
        ) {
            traces.push(tr);
        }
        merge_permits_history(&cands)
    };

    // 16. Stage-2 Permit Intelligence (`MERGE_HIGHEST_PRIORITY` per specialized extractor)
    let mut intel_cands: Vec<&ExtensionEnvelopeProto> = envelopes
        .iter()
        .filter(|e| {
            matches!(
                e.payload,
                Some(
                    Payload::RoofIntel(_)
                        | Payload::SolarIntel(_)
                        | Payload::MepIntel(_)
                        | Payload::RemodelIntel(_)
                        | Payload::CodeEnforcementIntel(_)
                )
            )
        })
        .collect();
    intel_cands.sort_by(|a, b| highest_priority_key(b, true).cmp(&highest_priority_key(a, true)));
    if let Some(mut tr) = record_trace(
        CapabilityDomain::DomainPermitIntel,
        MergeStrategy::MergeHighestPriority,
        &intel_cands,
    ) {
        tr.wave_index = 1;
        traces.push(tr);
    }

    let roof_intel: Option<RoofIntelProto> = intel_cands.iter().find_map(|e| match &e.payload {
        Some(Payload::RoofIntel(v)) => Some(v.clone()),
        _ => None,
    });
    let solar_intel: Option<SolarIntelProto> = intel_cands.iter().find_map(|e| match &e.payload {
        Some(Payload::SolarIntel(v)) => Some(v.clone()),
        _ => None,
    });
    let mep_intel: Option<MepIntelProto> = intel_cands.iter().find_map(|e| match &e.payload {
        Some(Payload::MepIntel(v)) => Some(v.clone()),
        _ => None,
    });
    let remodel_intel: Option<RemodelIntelProto> =
        intel_cands.iter().find_map(|e| match &e.payload {
            Some(Payload::RemodelIntel(v)) => Some(v.clone()),
            _ => None,
        });
    let code_enforcement_intel: Option<CodeEnforcementIntelProto> =
        intel_cands.iter().find_map(|e| match &e.payload {
            Some(Payload::CodeEnforcementIntel(v)) => Some(v.clone()),
            _ => None,
        });

    let mut bundle = AggregatedPropertyBundleProto {
        query: Some(query.clone()),
        geocode,
        property_facts,
        electric_utility,
        gas_utility,
        water_sewer_trash,
        broadband_catalog,
        hazard_summary,
        environmental_summary,
        livability_summary,
        market_valuation,
        insurance_quote,
        tax_mandates,
        house_3d,
        shopping_facets: None,
        attributions,
        resolution_traces: traces,
        aggregated_at_rfc3339: now_iso(),
        routing_matrix,
        permits_history,
        roof_intel,
        solar_intel,
        mep_intel,
        remodel_intel,
        code_enforcement_intel,
    };
    bundle.shopping_facets = Some(to_listing_shopping_facets(&bundle));
    bundle
}

#[derive(Clone)]
struct RankedBroadbandEntry {
    key: String,
    prov_rank: i32,
    geo_spec: i32,
    priority: i32,
    opt: BroadbandOptionProto,
}

fn is_verified_broadband_status(status: &str) -> bool {
    matches!(
        status.trim(),
        "confirmed_fcc" | "confirmed_address" | "address_verified"
    )
}

fn merge_broadband(
    query: &ExtensionQueryProto,
    cands: &[&ExtensionEnvelopeProto],
    pol: &DomainAggregationPolicyProto,
) -> Option<BroadbandCatalogProto> {
    if cands.is_empty() {
        return None;
    }

    let mut has_live_fcc_envelope = false;
    let mut has_address_verified_envelope = false;
    let mut entries: Vec<RankedBroadbandEntry> = Vec::new();
    let mut fcc_map_url = match (query.lat, query.lon) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => format!(
            "https://broadbandmap.fcc.gov/location-summary/fixed?latlon={la:.6}%2C{lo:.6}"
        ),
        _ => "https://broadbandmap.fcc.gov/".to_string(),
    };

    for env in cands {
        let prov = envelope_provenance(env);
        if matches!(
            prov,
            ProvenanceTier::ProvenanceRegionalFootprint | ProvenanceTier::ProvenanceUnverified
        ) && !pol.allow_regional_footprint_fallback
        {
            continue;
        }
        let p_rank = provenance_rank(prov);
        match &env.payload {
            Some(Payload::BroadbandOption(opt)) => {
                if opt.availability_status == "confirmed_fcc" {
                    has_live_fcc_envelope = true;
                } else if matches!(
                    opt.availability_status.as_str(),
                    "confirmed_address" | "address_verified"
                ) {
                    has_address_verified_envelope = true;
                }
                let cat = opt.tier_category.trim().to_ascii_lowercase();
                let key = format!("{}::{cat}", normalize_carrier_brand_key(&opt.carrier_name));
                upsert_broadband_entry(
                    &mut entries,
                    RankedBroadbandEntry {
                        key,
                        prov_rank: p_rank,
                        geo_spec: env.geo_specificity,
                        priority: env.priority,
                        opt: opt.clone(),
                    },
                );
            }
            Some(Payload::BroadbandCatalog(cat)) => {
                if cat.verification_status == "fcc_location_verified" || cat.fiber_confirmed_fcc {
                    has_live_fcc_envelope = true;
                } else if matches!(
                    cat.verification_status.as_str(),
                    "confirmed_address" | "address_verified"
                ) {
                    has_address_verified_envelope = true;
                }
                if !cat.fcc_map_url.trim().is_empty() {
                    fcc_map_url = cat.fcc_map_url.clone();
                }
                for opt in cat
                    .fiber_carriers
                    .iter()
                    .chain(&cat.cable_carriers)
                    .chain(&cat.wireless_carriers)
                {
                    let tier_cat = opt.tier_category.trim().to_ascii_lowercase();
                    let key =
                        format!("{}::{tier_cat}", normalize_carrier_brand_key(&opt.carrier_name));
                    let eff_rank = if is_verified_broadband_status(&opt.availability_status) {
                        provenance_rank(ProvenanceTier::ProvenanceLiveVerified)
                    } else {
                        p_rank
                    };
                    upsert_broadband_entry(
                        &mut entries,
                        RankedBroadbandEntry {
                            key,
                            prov_rank: eff_rank,
                            geo_spec: env.geo_specificity,
                            priority: env.priority,
                            opt: opt.clone(),
                        },
                    );
                }
            }
            _ => {}
        }
    }

    if entries.is_empty() && !has_live_fcc_envelope && !has_address_verified_envelope {
        return None;
    }

    // When live FCC BDC or live address qualification returned confirmed wireline rows,
    // drop unconfirmed regional fiber/cable guesses that were NOT reported at this parcel
    // (strict data honesty: live parcel observation overrides regional metro guesses!).
    if has_live_fcc_envelope || has_address_verified_envelope {
        let any_confirmed = entries
            .iter()
            .any(|e| is_verified_broadband_status(&e.opt.availability_status));
        if any_confirmed {
            entries.retain(|e| {
                is_verified_broadband_status(&e.opt.availability_status)
                    || e.opt.availability_status == "nationwide_sky_view"
            });
        }
    }

    let mut fiber_carriers = Vec::new();
    let mut cable_carriers = Vec::new();
    let mut wireless_carriers = Vec::new();

    for e in entries {
        match e.opt.tier_category.as_str() {
            "fiber" => fiber_carriers.push(e.opt),
            "cable" => cable_carriers.push(e.opt),
            _ => wireless_carriers.push(e.opt),
        }
    }

    let sort_carriers = |list: &mut Vec<BroadbandOptionProto>| {
        list.sort_by(|a, b| {
            let a_conf = is_verified_broadband_status(&a.availability_status);
            let b_conf = is_verified_broadband_status(&b.availability_status);
            b_conf
                .cmp(&a_conf)
                .then_with(|| b.max_download_mbps.cmp(&a.max_download_mbps))
        });
    };
    sort_carriers(&mut fiber_carriers);
    sort_carriers(&mut cable_carriers);
    sort_carriers(&mut wireless_carriers);

    let fiber_confirmed_fcc = fiber_carriers
        .iter()
        .any(|c| c.availability_status == "confirmed_fcc");
    let fiber_confirmed_address = fiber_carriers
        .iter()
        .any(|c| is_verified_broadband_status(&c.availability_status));
    // Strict honesty: NEVER set fiber_available = true from unverified county/metro footprint guesses.
    let fiber_available = fiber_confirmed_fcc || fiber_confirmed_address;
    let cable_confirmed = cable_carriers
        .iter()
        .any(|c| is_verified_broadband_status(&c.availability_status));
    let cable_available = cable_confirmed;

    let max_down = fiber_carriers
        .iter()
        .chain(&cable_carriers)
        .chain(&wireless_carriers)
        .filter(|c| is_verified_broadband_status(&c.availability_status))
        .map(|c| c.max_download_mbps)
        .max()
        .unwrap_or(0);
    let max_up = fiber_carriers
        .iter()
        .chain(&cable_carriers)
        .chain(&wireless_carriers)
        .filter(|c| is_verified_broadband_status(&c.availability_status))
        .map(|c| c.max_upload_mbps)
        .max()
        .unwrap_or(0);

    let high_speed_available =
        fiber_available || cable_confirmed || (has_live_fcc_envelope && max_down >= 100);
    let internet_tier = if fiber_available {
        "fiber_confirmed"
    } else if cable_available || (has_live_fcc_envelope && max_down >= 100) {
        "cable_high_speed"
    } else if has_live_fcc_envelope {
        "wireless_or_dsl"
    } else {
        "unverified"
    }
    .to_string();

    let verification_status = if has_live_fcc_envelope {
        "fcc_location_verified"
    } else if fiber_confirmed_address || has_address_verified_envelope {
        "address_verified"
    } else {
        "unverified"
    }
    .to_string();

    let headline_verdict = if fiber_confirmed_fcc {
        let names: Vec<&str> = fiber_carriers
            .iter()
            .filter(|c| c.availability_status == "confirmed_fcc")
            .take(2)
            .map(|c| c.carrier_name.as_str())
            .collect();
        format!(
            "Symmetrical Fiber Confirmed (FCC BDC): {} (up to {} Mbps)",
            names.join(" / "),
            max_down
        )
    } else if fiber_confirmed_address {
        let names: Vec<&str> = fiber_carriers
            .iter()
            .filter(|c| is_verified_broadband_status(&c.availability_status))
            .take(2)
            .map(|c| c.carrier_name.as_str())
            .collect();
        format!(
            "Symmetrical Fiber Verified at Address: {} (up to {} Mbps)",
            names.join(" / "),
            max_down
        )
    } else if has_live_fcc_envelope && cable_available {
        let top = &cable_carriers[0];
        format!(
            "High-Speed Cable Confirmed (FCC BDC — No FTTH Fiber): {} (up to {} Mbps)",
            top.carrier_name, top.max_download_mbps
        )
    } else if has_live_fcc_envelope {
        "Wireless / Satellite Connectivity Reported (FCC BDC — No Verified Fiber)".to_string()
    } else {
        "Broadband Coverage Unverified — Address Check Required".to_string()
    };

    let est_monthly_internet_usd = fiber_carriers
        .iter()
        .chain(&cable_carriers)
        .chain(&wireless_carriers)
        .filter(|c| is_verified_broadband_status(&c.availability_status))
        .find_map(|c| parse_leading_dollar_from_plan(&c.typical_monthly_usd))
        .unwrap_or(0);

    Some(BroadbandCatalogProto {
        high_speed_available,
        fiber_available,
        fiber_confirmed_fcc,
        internet_tier,
        verification_status,
        headline_verdict,
        max_download_mbps: max_down,
        max_upload_mbps: max_up,
        fiber_carriers,
        cable_carriers,
        wireless_carriers,
        est_monthly_internet_usd,
        fcc_map_url,
    })
}

fn parse_leading_dollar_from_plan(s: &str) -> Option<i64> {
    let idx = s.find('$')?;
    let digits: String = s[idx + 1..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse::<f64>().ok().map(|v| v.round() as i64)
}

fn upsert_broadband_entry(entries: &mut Vec<RankedBroadbandEntry>, cand: RankedBroadbandEntry) {
    if let Some(existing) = entries.iter_mut().find(|e| e.key == cand.key) {
        let cand_tuple = (
            cand.prov_rank,
            cand.geo_spec,
            cand.priority,
            cand.opt.max_download_mbps,
        );
        let ex_tuple = (
            existing.prov_rank,
            existing.geo_spec,
            existing.priority,
            existing.opt.max_download_mbps,
        );
        if cand_tuple > ex_tuple {
            let mut merged = cand;
            // Preserve published carrier pricing/check link when upgrading a regional footprint with a live FCC speed row.
            if merged.opt.check_url.contains("broadbandmap.fcc.gov")
                && !existing.opt.check_url.contains("broadbandmap.fcc.gov")
                && !existing.opt.check_url.is_empty()
            {
                merged.opt.check_url = existing.opt.check_url.clone();
            }
            *existing = merged;
        }
    } else {
        entries.push(cand);
    }
}

fn merge_permits_history(cands: &[&ExtensionEnvelopeProto]) -> Option<PermitsHistoryProto> {
    if cands.is_empty() {
        return None;
    }
    let mut sources_checked: Vec<String> = Vec::new();
    let mut queried_layers: Vec<String> = Vec::new();
    let mut notes_list: Vec<String> = Vec::new();
    let mut portal_name = String::new();
    let mut portal_url = String::new();
    let mut coverage_complete = false;
    let mut open_ce: Option<i32> = None;
    let mut closed_ce: Option<i32> = None;

    // Canonical dedup by normalized `(permit_number, issue_date/applied_date, address)`.
    let mut by_key: Vec<(String, PermitRecordProto, (i32, i32, bool, bool))> = Vec::new();

    for env in cands {
        let Some(Payload::PermitsHistory(ref hist)) = env.payload else {
            continue;
        };
        if portal_name.is_empty() && !hist.portal_name.trim().is_empty() {
            portal_name = hist.portal_name.clone();
        }
        if portal_url.is_empty() && !hist.portal_url.trim().is_empty() {
            portal_url = hist.portal_url.clone();
        }
        if hist.coverage_complete {
            coverage_complete = true;
        }
        for s in &hist.sources_checked {
            if !s.trim().is_empty() && !sources_checked.contains(s) {
                sources_checked.push(s.clone());
            }
        }
        if !env.extension_id.is_empty() && !sources_checked.contains(&env.extension_id) {
            sources_checked.push(env.extension_id.clone());
        }
        for l in &hist.queried_layers {
            if !l.trim().is_empty() && !queried_layers.contains(l) {
                queried_layers.push(l.clone());
            }
        }
        if !hist.notes.trim().is_empty() && !notes_list.contains(&hist.notes) {
            notes_list.push(hist.notes.clone());
        }
        if let Some(c) = hist.open_code_enforcement_count {
            open_ce = Some(open_ce.unwrap_or(0).max(c));
        }
        if let Some(c) = hist.closed_code_enforcement_count {
            closed_ce = Some(closed_ce.unwrap_or(0).max(c));
        }

        let prov_rank = provenance_rank(envelope_provenance(env));
        for rec in &hist.permits {
            let num_norm = rec.permit_number.trim().to_ascii_uppercase();
            let date_norm = if !rec.final_date.trim().is_empty() {
                rec.final_date.trim()
            } else if !rec.issue_date.trim().is_empty() {
                rec.issue_date.trim()
            } else {
                rec.applied_date.trim()
            };
            let key = if !num_norm.is_empty() {
                format!("{num_norm}::{}", rec.scope)
            } else {
                format!(
                    "{}::{}::{}",
                    rec.category,
                    date_norm,
                    rec.description.trim().to_ascii_lowercase()
                )
            };
            let rank = (prov_rank, env.priority, rec.source_verified, rec.finaled);
            if let Some(existing) = by_key.iter_mut().find(|(k, _, _)| *k == key) {
                if rank > existing.2 {
                    *existing = (key, rec.clone(), rank);
                }
            } else {
                by_key.push((key, rec.clone(), rank));
            }
        }
    }

    let mut permits: Vec<PermitRecordProto> = by_key.into_iter().map(|(_, p, _)| p).collect();
    permits.sort_by(|a, b| {
        b.year
            .unwrap_or(0)
            .cmp(&a.year.unwrap_or(0))
            .then_with(|| a.permit_number.cmp(&b.permit_number))
    });

    Some(PermitsHistoryProto {
        permits,
        sources_checked,
        portal_name,
        portal_url,
        coverage_complete,
        queried_layers,
        open_code_enforcement_count: open_ce,
        closed_code_enforcement_count: closed_ce,
        notes: notes_list.join("; "),
    })
}

fn non_empty_str(a: &str, b: &str) -> String {
    if !a.trim().is_empty() {
        a.to_string()
    } else {
        b.to_string()
    }
}

fn merge_geocode(cands: &[&ExtensionEnvelopeProto]) -> Option<GeocodeResultProto> {
    let mut acc: Option<GeocodeResultProto> = None;
    for env in cands {
        let Some(Payload::Geocode(ref g)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => g.clone(),
            Some(cur) => GeocodeResultProto {
                matched_address: non_empty_str(&cur.matched_address, &g.matched_address),
                lat: if cur.lat != 0.0 { cur.lat } else { g.lat },
                lon: if cur.lon != 0.0 { cur.lon } else { g.lon },
                street: non_empty_str(&cur.street, &g.street),
                city: non_empty_str(&cur.city, &g.city),
                state: non_empty_str(&cur.state, &g.state),
                zip: non_empty_str(&cur.zip, &g.zip),
                state_fips: non_empty_str(&cur.state_fips, &g.state_fips),
                county_fips: non_empty_str(&cur.county_fips, &g.county_fips),
                county_name: non_empty_str(&cur.county_name, &g.county_name),
                census_tract: cur.census_tract.or_else(|| g.census_tract.clone()),
                block_group: cur.block_group.or_else(|| g.block_group.clone()),
                cbsa_code: cur.cbsa_code.or_else(|| g.cbsa_code.clone()),
                cbsa_title: cur.cbsa_title.or_else(|| g.cbsa_title.clone()),
                school_district_geoid: cur
                    .school_district_geoid
                    .or_else(|| g.school_district_geoid.clone()),
                school_district_name: cur
                    .school_district_name
                    .or_else(|| g.school_district_name.clone()),
            },
        });
    }
    acc
}

fn merge_property_facts(cands: &[&ExtensionEnvelopeProto]) -> Option<PropertyFactsProto> {
    let mut acc: Option<PropertyFactsProto> = None;
    for env in cands {
        let Some(Payload::PropertyFacts(ref p)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => p.clone(),
            Some(mut cur) => {
                cur.matched_address = non_empty_str(&cur.matched_address, &p.matched_address);
                cur.street = non_empty_str(&cur.street, &p.street);
                cur.city = non_empty_str(&cur.city, &p.city);
                cur.state = non_empty_str(&cur.state, &p.state);
                cur.zip = non_empty_str(&cur.zip, &p.zip);
                cur.county_fips = non_empty_str(&cur.county_fips, &p.county_fips);
                cur.county_name = non_empty_str(&cur.county_name, &p.county_name);
                cur.lat = cur.lat.or(p.lat);
                cur.lon = cur.lon.or(p.lon);
                cur.apn = cur.apn.or_else(|| p.apn.clone());
                cur.property_type = cur.property_type.or_else(|| p.property_type.clone());
                cur.beds = cur.beds.or(p.beds);
                cur.baths = cur.baths.or(p.baths);
                cur.sqft = cur.sqft.or(p.sqft);
                cur.lot_sqft = cur.lot_sqft.or(p.lot_sqft);
                cur.year_built = cur.year_built.or(p.year_built);
                cur.stories = cur.stories.or(p.stories);
                cur.garage_spaces = cur.garage_spaces.or(p.garage_spaces);
                cur.hoa_monthly_usd = cur.hoa_monthly_usd.or(p.hoa_monthly_usd);
                cur.list_price_usd = cur.list_price_usd.or(p.list_price_usd);
                cur.last_sale_price_usd = cur.last_sale_price_usd.or(p.last_sale_price_usd);
                cur.last_sale_date = cur.last_sale_date.or_else(|| p.last_sale_date.clone());
                cur.annual_property_tax_usd =
                    cur.annual_property_tax_usd.or(p.annual_property_tax_usd);
                cur.effective_tax_rate = cur.effective_tax_rate.or(p.effective_tax_rate);
                cur.zoning = cur.zoning.or_else(|| p.zoning.clone());
                cur.architectural_style = cur
                    .architectural_style
                    .or_else(|| p.architectural_style.clone());
                for f in &p.features {
                    if !cur.features.contains(f) {
                        cur.features.push(f.clone());
                    }
                }
                cur.has_solar = cur.has_solar.or(p.has_solar);
                cur.has_ev_charger = cur.has_ev_charger.or(p.has_ev_charger);
                cur.has_heat_pump = cur.has_heat_pump.or(p.has_heat_pump);
                cur.has_adu = cur.has_adu.or(p.has_adu);
                cur.adu_feasible = cur.adu_feasible.or(p.adu_feasible);
                cur.has_pool = cur.has_pool.or(p.has_pool);
                cur.mls_number = cur.mls_number.or_else(|| p.mls_number.clone());
                cur.listing_status = cur.listing_status.or_else(|| p.listing_status.clone());
                cur
            }
        });
    }
    acc
}

fn merge_hazards(cands: &[&ExtensionEnvelopeProto]) -> Option<HazardSummaryProto> {
    let mut acc: Option<HazardSummaryProto> = None;
    for env in cands {
        let Some(Payload::HazardSummary(ref h)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => h.clone(),
            Some(mut cur) => {
                cur.fema_flood_zone = cur.fema_flood_zone.or_else(|| h.fema_flood_zone.clone());
                cur.in_sfha = cur.in_sfha.or(h.in_sfha);
                cur.nearest_sfha_meters = cur.nearest_sfha_meters.or(h.nearest_sfha_meters);
                cur.wildfire_zone = cur.wildfire_zone.or_else(|| h.wildfire_zone.clone());
                cur.seismic_pga_g = cur.seismic_pga_g.or(h.seismic_pga_g);
                cur.seismic_ss_g = cur.seismic_ss_g.or(h.seismic_ss_g);
                cur.seismic_s1_g = cur.seismic_s1_g.or(h.seismic_s1_g);
                cur.seismic_design_category = cur
                    .seismic_design_category
                    .or_else(|| h.seismic_design_category.clone());
                cur.nearest_fault_km = cur.nearest_fault_km.or(h.nearest_fault_km);
                cur.nearest_fault_name = cur
                    .nearest_fault_name
                    .or_else(|| h.nearest_fault_name.clone());
                cur.epa_radon_zone = cur.epa_radon_zone.or(h.epa_radon_zone);
                cur.fema_nri_composite_score =
                    cur.fema_nri_composite_score.or(h.fema_nri_composite_score);
                cur.overall_safety_score = cur.overall_safety_score.or(h.overall_safety_score);
                for hl in &h.highlights {
                    if !cur.highlights.contains(hl) {
                        cur.highlights.push(hl.clone());
                    }
                }
                cur
            }
        });
    }
    acc
}

fn merge_environmental(cands: &[&ExtensionEnvelopeProto]) -> Option<EnvironmentalSummaryProto> {
    let mut acc: Option<EnvironmentalSummaryProto> = None;
    for env in cands {
        let Some(Payload::EnvironmentalSummary(ref e)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => e.clone(),
            Some(mut cur) => {
                cur.water_system_pwsid = cur
                    .water_system_pwsid
                    .or_else(|| e.water_system_pwsid.clone());
                cur.water_system_name = cur
                    .water_system_name
                    .or_else(|| e.water_system_name.clone());
                cur.sdwis_violation_count = cur.sdwis_violation_count.or(e.sdwis_violation_count);
                cur.superfund_count_3mi = cur.superfund_count_3mi.or(e.superfund_count_3mi);
                cur.nearest_superfund_km = cur.nearest_superfund_km.or(e.nearest_superfund_km);
                cur.tri_facility_count_2mi =
                    cur.tri_facility_count_2mi.or(e.tri_facility_count_2mi);
                cur.lust_open_count_1mi = cur.lust_open_count_1mi.or(e.lust_open_count_1mi);
                cur.pm25_mean_ug_m3 = cur.pm25_mean_ug_m3.or(e.pm25_mean_ug_m3);
                cur.us_aqi_recent = cur.us_aqi_recent.or(e.us_aqi_recent);
                cur.road_noise_dba = cur.road_noise_dba.or(e.road_noise_dba);
                cur.aviation_noise_dba = cur.aviation_noise_dba.or(e.aviation_noise_dba);
                cur.pollution_burden_percentile = cur
                    .pollution_burden_percentile
                    .or(e.pollution_burden_percentile);
                cur.environmental_health_score = cur
                    .environmental_health_score
                    .or(e.environmental_health_score);
                for hl in &e.highlights {
                    if !cur.highlights.contains(hl) {
                        cur.highlights.push(hl.clone());
                    }
                }
                cur
            }
        });
    }
    acc
}

fn merge_livability(cands: &[&ExtensionEnvelopeProto]) -> Option<LivabilitySummaryProto> {
    let mut acc: Option<LivabilitySummaryProto> = None;
    for env in cands {
        let Some(Payload::LivabilitySummary(ref l)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => l.clone(),
            Some(mut cur) => {
                cur.walkability_index = cur.walkability_index.or(l.walkability_index);
                cur.amenity_access_score = cur.amenity_access_score.or(l.amenity_access_score);
                cur.grocery_count_1mi = cur.grocery_count_1mi.or(l.grocery_count_1mi);
                cur.restaurant_count_1mi = cur.restaurant_count_1mi.or(l.restaurant_count_1mi);
                cur.park_count_1mi = cur.park_count_1mi.or(l.park_count_1mi);
                cur.transit_stop_count_half_mi = cur
                    .transit_stop_count_half_mi
                    .or(l.transit_stop_count_half_mi);
                cur.nearest_freeway_meters =
                    cur.nearest_freeway_meters.or(l.nearest_freeway_meters);
                cur.nearest_high_voltage_meters = cur
                    .nearest_high_voltage_meters
                    .or(l.nearest_high_voltage_meters);
                cur.school_composite_score =
                    cur.school_composite_score.or(l.school_composite_score);
                cur.school_grade = cur.school_grade.or_else(|| l.school_grade.clone());
                cur.school_district = cur.school_district.or_else(|| l.school_district.clone());
                if cur.schools.is_empty() && !l.schools.is_empty() {
                    cur.schools = l.schools.clone();
                }
                cur.violent_crime_per_100k =
                    cur.violent_crime_per_100k.or(l.violent_crime_per_100k);
                cur.property_crime_per_100k =
                    cur.property_crime_per_100k.or(l.property_crime_per_100k);
                cur.crime_safety_score = cur.crime_safety_score.or(l.crime_safety_score);
                cur
            }
        });
    }
    acc
}

fn median_i64(mut vals: Vec<i64>) -> Option<i64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_unstable();
    let mid = vals.len() / 2;
    if vals.len() % 2 == 1 {
        Some(vals[mid])
    } else {
        Some(((vals[mid - 1] as i128 + vals[mid] as i128) / 2) as i64)
    }
}

fn median_f64(mut vals: Vec<f64>) -> Option<f64> {
    vals.retain(|v| v.is_finite());
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = vals.len() / 2;
    if vals.len() % 2 == 1 {
        Some(vals[mid])
    } else {
        Some((vals[mid - 1] + vals[mid]) / 2.0)
    }
}

fn merge_market_valuation(
    cands: &[&ExtensionEnvelopeProto],
    prefer_live: bool,
) -> Option<MarketValuationProto> {
    if cands.is_empty() {
        return None;
    }
    let live_subset: Vec<&ExtensionEnvelopeProto> = if prefer_live {
        cands
            .iter()
            .copied()
            .filter(|e| envelope_provenance(e) == ProvenanceTier::ProvenanceLiveVerified)
            .collect()
    } else {
        Vec::new()
    };
    let pool: &[&ExtensionEnvelopeProto] = if !live_subset.is_empty() {
        &live_subset
    } else {
        cands
    };

    let collect_i64 = |f: fn(&MarketValuationProto) -> Option<i64>| -> Option<i64> {
        let v: Vec<i64> = pool
            .iter()
            .filter_map(|e| match &e.payload {
                Some(Payload::MarketValuation(m)) => f(m),
                _ => None,
            })
            .collect();
        if v.is_empty() {
            cands.iter().find_map(|e| match &e.payload {
                Some(Payload::MarketValuation(m)) => f(m),
                _ => None,
            })
        } else {
            median_i64(v)
        }
    };
    let collect_f64 = |f: fn(&MarketValuationProto) -> Option<f64>| -> Option<f64> {
        let v: Vec<f64> = pool
            .iter()
            .filter_map(|e| match &e.payload {
                Some(Payload::MarketValuation(m)) => f(m),
                _ => None,
            })
            .collect();
        if v.is_empty() {
            cands.iter().find_map(|e| match &e.payload {
                Some(Payload::MarketValuation(m)) => f(m),
                _ => None,
            })
        } else {
            median_f64(v)
        }
    };

    Some(MarketValuationProto {
        estimated_value_usd: collect_i64(|m| m.estimated_value_usd),
        value_low_usd: collect_i64(|m| m.value_low_usd),
        value_high_usd: collect_i64(|m| m.value_high_usd),
        confidence_0_to_1: collect_f64(|m| m.confidence_0_to_1),
        estimated_monthly_rent_usd: collect_i64(|m| m.estimated_monthly_rent_usd),
        zip_median_home_value_usd: collect_i64(|m| m.zip_median_home_value_usd),
        yoy_appreciation_pct: collect_f64(|m| m.yoy_appreciation_pct),
        cagr_5yr_pct: collect_f64(|m| m.cagr_5yr_pct),
        mortgage_rate_30yr_pct: collect_f64(|m| m.mortgage_rate_30yr_pct),
        median_household_income_usd: collect_i64(|m| m.median_household_income_usd),
        tract_broadband_adoption_pct: collect_f64(|m| m.tract_broadband_adoption_pct),
        estimated_annual_insurance_usd: collect_i64(|m| m.estimated_annual_insurance_usd),
        effective_property_tax_rate: collect_f64(|m| m.effective_property_tax_rate),
        has_special_tax_district: cands.iter().find_map(|e| match &e.payload {
            Some(Payload::MarketValuation(m)) => m.has_special_tax_district,
            _ => None,
        }),
        special_tax_district_label: cands.iter().find_map(|e| match &e.payload {
            Some(Payload::MarketValuation(m)) => m.special_tax_district_label.clone(),
            _ => None,
        }),
    })
}

fn merge_insurance(cands: &[&ExtensionEnvelopeProto]) -> Option<InsuranceQuoteProto> {
    let mut acc: Option<InsuranceQuoteProto> = None;
    for env in cands {
        let Some(Payload::InsuranceQuote(ref q)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => q.clone(),
            Some(mut cur) => {
                cur.estimated_annual_premium_usd = cur
                    .estimated_annual_premium_usd
                    .or(q.estimated_annual_premium_usd);
                cur.effective_pct_of_value =
                    cur.effective_pct_of_value.or(q.effective_pct_of_value);
                cur.fair_plan_or_surplus_risk = cur
                    .fair_plan_or_surplus_risk
                    .or(q.fair_plan_or_surplus_risk);
                cur.flood_insurance_annual_usd = cur
                    .flood_insurance_annual_usd
                    .or(q.flood_insurance_annual_usd);
                cur.earthquake_insurance_annual_usd = cur
                    .earthquake_insurance_annual_usd
                    .or(q.earthquake_insurance_annual_usd);
                cur.carrier_market_status = cur
                    .carrier_market_status
                    .or_else(|| q.carrier_market_status.clone());
                for n in &q.underwriting_notes {
                    if !cur.underwriting_notes.contains(n) {
                        cur.underwriting_notes.push(n.clone());
                    }
                }
                cur
            }
        });
    }
    acc
}

fn merge_tax_mandates(cands: &[&ExtensionEnvelopeProto]) -> Option<TaxMandatesProto> {
    let mut acc: Option<TaxMandatesProto> = None;
    for env in cands {
        let Some(Payload::TaxMandates(ref t)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => t.clone(),
            Some(mut cur) => {
                cur.effective_property_tax_rate = cur
                    .effective_property_tax_rate
                    .or(t.effective_property_tax_rate);
                cur.state_transfer_tax_pct =
                    cur.state_transfer_tax_pct.or(t.state_transfer_tax_pct);
                cur.local_transfer_tax_pct =
                    cur.local_transfer_tax_pct.or(t.local_transfer_tax_pct);
                cur.estimated_transfer_tax_usd = cur
                    .estimated_transfer_tax_usd
                    .or(t.estimated_transfer_tax_usd);
                cur.has_special_tax_district =
                    cur.has_special_tax_district.or(t.has_special_tax_district);
                cur.special_tax_district_type = cur
                    .special_tax_district_type
                    .or_else(|| t.special_tax_district_type.clone());
                cur.homestead_exemption_summary = cur
                    .homestead_exemption_summary
                    .or_else(|| t.homestead_exemption_summary.clone());
                for m in &t.point_of_sale_mandates {
                    if !cur.point_of_sale_mandates.contains(m) {
                        cur.point_of_sale_mandates.push(m.clone());
                    }
                }
                cur
            }
        });
    }
    acc
}

fn merge_house3d(cands: &[&ExtensionEnvelopeProto]) -> Option<House3dMetadataProto> {
    let mut acc: Option<House3dMetadataProto> = None;
    for env in cands {
        let Some(Payload::House3d(ref h)) = env.payload else {
            continue;
        };
        acc = Some(match acc {
            None => h.clone(),
            Some(mut cur) => {
                cur.roof_ridge_height_m = cur.roof_ridge_height_m.or(h.roof_ridge_height_m);
                cur.ground_elevation_m = cur.ground_elevation_m.or(h.ground_elevation_m);
                cur.dominant_roof_pitch_deg =
                    cur.dominant_roof_pitch_deg.or(h.dominant_roof_pitch_deg);
                cur.roof_footprint_sqm = cur.roof_footprint_sqm.or(h.roof_footprint_sqm);
                cur.tree_canopy_count = cur.tree_canopy_count.or(h.tree_canopy_count);
                cur.lidar_project_name = cur
                    .lidar_project_name
                    .or_else(|| h.lidar_project_name.clone());
                cur.tour_embed_url = cur.tour_embed_url.or_else(|| h.tour_embed_url.clone());
                cur
            }
        });
    }
    acc
}

/// Projects an `AggregatedPropertyBundleProto` onto the 77-column `listings` shopping table
/// (`ListingShoppingFacetsProto`). Missing inputs remain `None` (SQL `NULL`), never fabricated.
pub fn to_listing_shopping_facets(
    bundle: &AggregatedPropertyBundleProto,
) -> ListingShoppingFacetsProto {
    let q = bundle.query.as_ref();
    let facts = bundle.property_facts.as_ref();
    let elec = bundle.electric_utility.as_ref();
    let gas = bundle.gas_utility.as_ref();
    let wst = bundle.water_sewer_trash.as_ref();
    let bb = bundle.broadband_catalog.as_ref();
    let haz = bundle.hazard_summary.as_ref();
    let liv = bundle.livability_summary.as_ref();
    let mv = bundle.market_valuation.as_ref();
    let ins = bundle.insurance_quote.as_ref();
    let tax = bundle.tax_mandates.as_ref();

    let real_sqft = facts
        .and_then(|f| f.sqft)
        .or_else(|| q.and_then(|x| x.sqft))
        .filter(|&s| s > 0);

    // Compute `est_monthly_utilities` ONLY when real `sqft` and electric tariff are present.
    let est_monthly_utilities = match (real_sqft, elec) {
        (Some(sqft), Some(el)) => {
            let kwh_mo = 380.0 + (sqft as f64 * 0.22);
            let elec_usd = kwh_mo * el.rate_per_kwh_usd + el.monthly_base_fee_usd;
            let gas_usd = gas
                .map(|g| {
                    if let Some(tr) = g.rate_per_therm_usd {
                        g.monthly_base_fee_usd + (18.0 + sqft as f64 * 0.012) * tr
                    } else {
                        0.0
                    }
                })
                .unwrap_or(0.0);
            let wst_usd = wst
                .map(|w| {
                    let units = (3.5 + (sqft as f64 / 750.0)).clamp(4.0, 14.0);
                    let w_mo =
                        w.water_base_monthly_usd + units * w.water_volumetric_rate_usd.unwrap_or(0.0);
                    w_mo + w.sewer_monthly_usd as f64 + w.trash_monthly_usd as f64
                })
                .unwrap_or(0.0);
            Some((elec_usd + gas_usd + wst_usd).round() as i64)
        }
        _ => None,
    };

    let (internet_carriers, fiber_carriers) = match bb {
        Some(cat) => {
            let fib: Vec<String> = if cat.fiber_available {
                cat.fiber_carriers
                    .iter()
                    .filter(|c| is_verified_broadband_status(&c.availability_status))
                    .map(|c| c.carrier_name.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let mut all = fib.clone();
            for c in cat.cable_carriers.iter().chain(&cat.wireless_carriers) {
                if is_verified_broadband_status(&c.availability_status)
                    && !all.contains(&c.carrier_name)
                {
                    all.push(c.carrier_name.clone());
                }
            }
            (
                (!all.is_empty()).then(|| all.join(", ")),
                (!fib.is_empty()).then(|| fib.join(", ")),
            )
        }
        None => (None, None),
    };

    let property_tax_rate = facts
        .and_then(|f| f.effective_tax_rate)
        .or_else(|| tax.and_then(|t| t.effective_property_tax_rate))
        .or_else(|| mv.and_then(|m| m.effective_property_tax_rate));

    let has_special_tax = tax
        .and_then(|t| t.has_special_tax_district)
        .or_else(|| mv.and_then(|m| m.has_special_tax_district));

    let price_or_val = facts
        .and_then(|f| f.list_price_usd)
        .or_else(|| mv.and_then(|m| m.estimated_value_usd))
        .filter(|&p| p > 0);
    let rate30 = mv
        .and_then(|m| m.mortgage_rate_30yr_pct)
        .filter(|&r| r > 0.0);

    let all_in_monthly = match (price_or_val, rate30, property_tax_rate) {
        (Some(p), Some(r_pct), Some(tr)) => {
            let r_dec = if r_pct > 1.0 { r_pct / 100.0 } else { r_pct };
            let tr_dec = if tr > 0.20 { tr / 100.0 } else { tr };
            let pi = monthly_pi(p as f64 * 0.80, r_dec, 30);
            let tax_mo = (p as f64 * tr_dec) / 12.0;
            let ins_mo = ins
                .and_then(|i| i.estimated_annual_premium_usd)
                .or_else(|| mv.and_then(|m| m.estimated_annual_insurance_usd))
                .map(|a| a as f64 / 12.0)
                .unwrap_or(0.0);
            let hoa_mo = facts
                .and_then(|f| f.hoa_monthly_usd)
                .unwrap_or(0)
                .max(0) as f64;
            let util_mo = est_monthly_utilities.unwrap_or(0) as f64;
            Some((pi + tax_mo + ins_mo + hoa_mo + util_mo).round() as i64)
        }
        _ => None,
    };

    ListingShoppingFacetsProto {
        listing_id: q
            .and_then(|x| x.extra_params.get("listing_id").cloned())
            .unwrap_or_default(),
        electric_carrier: elec.map(|e| e.carrier_name.clone()),
        electric_utility_type: elec.map(|e| e.utility_type.clone()),
        electric_rate_kwh: elec.map(|e| e.rate_per_kwh_usd),
        gas_carrier: gas.map(|g| g.carrier_name.clone()),
        gas_rate_therm: gas.and_then(|g| g.rate_per_therm_usd),
        water_carrier: wst.map(|w| w.water_carrier_name.clone()),
        water_source_type: wst.map(|w| w.water_source_type.clone()),
        sewer_type: wst.map(|w| w.sewer_type.clone()),
        est_monthly_utilities,
        internet_tier: bb.map(|b| b.internet_tier.clone()),
        fiber_available: bb.map(|b| b.fiber_available),
        high_speed_internet: bb.map(|b| b.high_speed_available),
        internet_carriers,
        fiber_carriers,
        max_download_mbps: bb
            .filter(|b| b.max_download_mbps > 0)
            .map(|b| b.max_download_mbps),
        max_upload_mbps: bb
            .filter(|b| b.max_upload_mbps > 0)
            .map(|b| b.max_upload_mbps),
        all_in_monthly,
        property_tax_rate,
        has_special_tax,
        has_solar: facts
            .and_then(|f| f.has_solar)
            .or_else(|| bundle.solar_intel.as_ref().map(|s| s.has_solar))
            .or_else(|| q.and_then(|x| x.has_solar)),
        has_ev_charger: facts.and_then(|f| f.has_ev_charger),
        has_heat_pump: facts.and_then(|f| f.has_heat_pump),
        has_adu: facts.and_then(|f| f.has_adu),
        adu_feasible: facts.and_then(|f| f.adu_feasible),
        has_pool: facts.and_then(|f| f.has_pool),
        garage_spaces: facts.and_then(|f| f.garage_spaces),
        architectural_style: facts.and_then(|f| f.architectural_style.clone()),
        flood_zone: haz.and_then(|h| h.fema_flood_zone.clone()),
        wildfire_zone: haz.and_then(|h| h.wildfire_zone.clone()),
        school_score: liv.and_then(|l| l.school_composite_score),
        school_grade: liv.and_then(|l| l.school_grade.clone()),
        walk_score: liv.and_then(|l| l.walkability_index),
        estimated_value: mv.and_then(|m| m.estimated_value_usd),
    }
}

/// Converts an `AggregatedPropertyBundleProto` into a report JSON patch (`serde_json::Value`)
/// omitting any domain without verified upstream data.
pub fn to_report_json_patch(bundle: &AggregatedPropertyBundleProto) -> Value {
    let mut patch = Map::new();
    if let Some(ref g) = bundle.geocode {
        if let Ok(v) = serde_json::to_value(g) {
            patch.insert("geocode".to_string(), v);
        }
    }
    if let Some(ref pf) = bundle.property_facts {
        if let Ok(v) = serde_json::to_value(pf) {
            patch.insert("propertyFacts".to_string(), v);
        }
    }
    if let Some(ref el) = bundle.electric_utility {
        if let Ok(v) = serde_json::to_value(el) {
            patch.insert("electricUtility".to_string(), v);
        }
    }
    if let Some(ref gs) = bundle.gas_utility {
        if let Ok(v) = serde_json::to_value(gs) {
            patch.insert("gasUtility".to_string(), v);
        }
    }
    if let Some(ref wst) = bundle.water_sewer_trash {
        if let Ok(v) = serde_json::to_value(wst) {
            patch.insert("waterSewerTrash".to_string(), v);
        }
    }
    if let Some(ref bb) = bundle.broadband_catalog {
        if let Ok(v) = serde_json::to_value(bb) {
            patch.insert("broadbandCatalog".to_string(), v);
        }
    }
    if let Some(ref hz) = bundle.hazard_summary {
        if let Ok(v) = serde_json::to_value(hz) {
            patch.insert("hazardSummary".to_string(), v);
        }
    }
    if let Some(ref ev) = bundle.environmental_summary {
        if let Ok(v) = serde_json::to_value(ev) {
            patch.insert("environmentalSummary".to_string(), v);
        }
    }
    if let Some(ref lv) = bundle.livability_summary {
        if let Ok(v) = serde_json::to_value(lv) {
            patch.insert("livabilitySummary".to_string(), v);
        }
    }
    if let Some(ref mv) = bundle.market_valuation {
        if let Ok(v) = serde_json::to_value(mv) {
            patch.insert("marketValuation".to_string(), v);
        }
    }
    if let Some(ref iq) = bundle.insurance_quote {
        if let Ok(v) = serde_json::to_value(iq) {
            patch.insert("insuranceQuote".to_string(), v);
        }
    }
    if let Some(ref tm) = bundle.tax_mandates {
        if let Ok(v) = serde_json::to_value(tm) {
            patch.insert("taxMandates".to_string(), v);
        }
    }
    if let Some(ref h3d) = bundle.house_3d {
        if let Ok(v) = serde_json::to_value(h3d) {
            patch.insert("house3d".to_string(), v);
        }
    }
    if let Some(ref rm) = bundle.routing_matrix {
        if let Ok(v) = serde_json::to_value(rm) {
            patch.insert("routingMatrix".to_string(), v);
        }
    }
    if let Some(ref ph) = bundle.permits_history {
        if let Ok(v) = serde_json::to_value(ph) {
            patch.insert("permitsHistory".to_string(), v);
        }
    }
    if let Some(ref ri) = bundle.roof_intel {
        if let Ok(v) = serde_json::to_value(ri) {
            patch.insert("roofIntel".to_string(), v);
        }
    }
    if let Some(ref si) = bundle.solar_intel {
        if let Ok(v) = serde_json::to_value(si) {
            patch.insert("solarIntel".to_string(), v);
        }
    }
    if let Some(ref mi) = bundle.mep_intel {
        if let Ok(v) = serde_json::to_value(mi) {
            patch.insert("mepIntel".to_string(), v);
        }
    }
    if let Some(ref rmi) = bundle.remodel_intel {
        if let Ok(v) = serde_json::to_value(rmi) {
            patch.insert("remodelIntel".to_string(), v);
        }
    }
    if let Some(ref ce) = bundle.code_enforcement_intel {
        if let Ok(v) = serde_json::to_value(ce) {
            patch.insert("codeEnforcementIntel".to_string(), v);
        }
    }
    if let Some(ref sf) = bundle.shopping_facets {
        if let Ok(v) = serde_json::to_value(sf) {
            patch.insert("shoppingFacets".to_string(), v);
        }
    }
    patch.insert(
        "attributions".to_string(),
        serde_json::to_value(&bundle.attributions).unwrap_or_else(|_| json!([])),
    );
    patch.insert(
        "resolutionTraces".to_string(),
        serde_json::to_value(&bundle.resolution_traces).unwrap_or_else(|_| json!([])),
    );
    patch.insert(
        "aggregatedAt".to_string(),
        Value::String(bundle.aggregated_at_rfc3339.clone()),
    );
    Value::Object(patch)
}
