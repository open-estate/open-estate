---
name: natural-hazards
description: >-
  Natural-hazard profile for one US property (lat/lon from property-geocode): FEMA flood zone and distance to the
  nearest high-risk flood zone (SFHA), earthquake design shaking (USGS ASCE 7-22 PGA/Ss/S1/SDC), earthquake history
  (USGS ComCat), nearest Quaternary faults, wildfire (CAL FIRE Fire Hazard Severity Zones in California, USFS Wildfire
  Hazard Potential elsewhere), the FEMA National Risk Index for the census tract, radon (EPA county zone, USGS geologic
  potential, CGS zones in CA) and California add-ons (Alquist-Priolo fault zones, liquefaction and earthquake-landslide
  zones, dam-failure inundation, tsunami, sea-level rise, Natural Hazard Disclosure checklist). Returns 0-100 safety
  scores, plain-English buyer highlights and map layers. Use when someone asks whether a specific US address is in a
  flood zone, near a fault, in a fire zone, what its natural-hazard / insurance / disclosure risks are, or needs the
  hazards section of a property report. Not for non-US locations, live emergency alerts, or region-wide statistics.
---

# Natural Hazards

## Purpose
Screen a single US property for the natural hazards a buyer, lender or insurer cares about, using only keyless public
data (FEMA, USGS, USFS, EPA, California Geological Survey, CAL FIRE, Cal OES). The module is `run_hazards` in
`src/modules/hazards.rs`, compiled into the Rust `open-estate-server` binary, and follows `skills/_shared/CONTRACT.md`.
Every endpoint, field and known failure mode is catalogued in [references/data-sources.md](references/data-sources.md).

## Inputs (`run_hazards(http, env, lat, lon, geo)`)
| Field | Required | Used for |
|---|---|---|
| `lat`, `lon` | yes (WGS84) | every lookup; invalid values return an error |
| `geo.state` / `geo.stateFips` / `geo.countyFips` | recommended | `'CA'`, `'06'` or a `06…` county FIPS turns on the California add-ons (they are **not** detected from the coordinates) |
| `geo.countyFips` (5-digit) | recommended | EPA radon zone lookup |
| `geo.tract` (11-digit) | optional | NRI fallback if the point query misses |
| `geo.county` | optional | wording only |
| `env` | — | no keys needed |

## How to run
The report orchestrator (`src/web/orchestrator.rs`) calls it for every report, with the rooftop-refined point; the
result is `report.modules.hazards` (e.g. `GET /api/report?address=...`). From Rust:
```rust
use open_estate::modules::hazards::run_hazards;

// `geo` = property-geocode data (state, stateFips, countyFips, tract, county ...)
let out = run_hazards(&http, &env, lat, lon, &geo).await?;   // never fails for a failed source
```
Tests: `tests/hazards_test.rs` (radon table and scoring offline; one live run against San Jose). A cold run makes
~12 requests (national) or ~32 (California), in parallel through the shared `HttpClient` (responses cached 7 days);
typically 5–15 s. Site elevation prefers USGS 3DEP (EPQS, hedged with the 3DEP ImageServer) for up to 6 s and
otherwise uses the Copernicus GLO-90 DEM (`location.elevationSource`). The EPA radon table is compiled in (`RADON_TABLE`
in `hazards.rs`).

## Output (`data`)
```text
location   {lat, lon, elevationM, elevationFt, elevationSource}
flood      {zone, zoneSubtype, sfha, risk: 'very high'|'high'|'moderate'|'minimal'|'undetermined'|'unknown', description,
            baseFloodElevationFt, depthFt, dfirmId, firmPanel, firmEffective, insuranceRequired,
            nearestSfha: {zone, distM, distMi, direction} | null (none within 2 km), sfhaWithin2km: [{zone, distM}]}
earthquake {designMaps: {standard:'ASCE 7-22', riskCategory:'II', siteClass:'Default', pgaM, ss, s1, sms, sm1, sds, sd1, sdc, vs30, modelVersion},
            history: {since, countM4Within50km, countM35Within50km, largestWithin100km: [{mag, place, date, distKm, depthKm, url}],
                      chartM35Within50km: [{t:'YYYY-MM-DD', mag, distKm}], chartTruncated},
            faults: {searchRadiusKm: 30, countWithinRadius, nearest: [{name, section, distM, distKm, distMi, direction, age, ageNote,
                     slipRate, slipSense, class, recentlyActive, url}], nearestRecentlyActive},
            california?: {quadrangle: {name, seismicHazardZonesMapped, faultZonesMapped, links[]},
                          alquistPriolo|liquefaction|landslide: {inZone, mapped, nearestDistM, nearestWithinM, quad?, released?},
                          unevaluatedArea, landslideSusceptibility: {class 0-10, label}}}
wildfire   {california?: {responsibilityArea:'SRA'|'LRA'|'FRA', fhsz, fhszClass: 0 none|1 Moderate|2 High|3 Very High, inFhsz, source,
                          nearestHighOrVeryHigh: {class, responsibility, distM, distMi, direction} | null, searchRadiusM},
            whp: {class 1-7, label}, nri: {score, rating}}
nri        {tract, version, population, buildingValueUSD, risk: {score, rating, statePercentile},
            expectedAnnualLoss: {totalUSD, score, rating}, socialVulnerability, communityResilience,
            topHazards: [{code, name, score, rating, ealUSD}]  // rated hazards only, highest first}
radon      {epaZone 1|2|3, epaZoneLabel, county, geologic: {potential, grp, radonIndex, province}, california?: {potential, description, study, report, reportUrl}}
other      (CA) {tsunamiHazardArea, tsunamiMapUrl, damInundation: {inArea, dams[], hazardClass, nearbyWithin1km[]},
            seaLevelRise: {exposed, firstExposedAtFt, source}, leveeProtected}
scores     {flood, earthquake, wildfire, other, overall, wind, radon}   // 0-100, higher = safer, null when no data;
                                                  // wind and radon are display-only (not part of overall)
scoreBasis {flood[], earthquake[], wildfire[], other[]}    // human-readable list of what moved each score
highlights [{level: 'good'|'watch'|'bad', text}]
caNaturalHazardDisclosure (CA) {specialFloodHazardArea, damInundationArea, veryHighFireHazardZone, fireHazardSeverityZone,
            stateResponsibilityArea, earthquakeFaultZone, seismicHazardZone, note}
mapLayers  [{id, title, kind, color, features}]  // fema-sfha, quaternary-faults, largest-earthquakes, ca-ap-zones,
                                                  // ca-liquefaction, ca-eq-landslide, ca-fhsz (only when non-empty)
```
Map geometry is generalized by the ArcGIS service (`maxAllowableOffset`), thinned to a per-part vertex budget and
rounded to 5 decimals (whole report ≈ 10–90 KB compact JSON). Distances/inside tests always use the service
geometry, not the thinned map copy.

Listing search stores a short wildfire label per home (`listings.wildfire_zone`, see the `property-search` skill):
the CAL FIRE class when the California block ran (`"CA FHSZ Very High"`, `"CA FHSZ none"`), else the WHP class
(`"WHP 4 High"`).

## Safety scores (0–100, higher = safer)
| Score | Base | Adjustments |
|---|---|---|
| **flood** | FEMA zone: V/VE 10 · A/AE/AH/AO 25 · AR/A99 35 · shaded X (0.2 %, levee, <1 ft) 65 · D 70 · X minimal 92 · no data 75 | outside SFHA but SFHA ≤100 m −15, ≤300 m −10, ≤1 km −5; NRI riverine/coastal Very High −10, Relatively High −5; CA: dam-inundation area −10, sea-level-rise exposure −10 (3 ft) / −5 (6 ft), levee-protected −5 |
| **earthquake** | ASCE 7-22 PGA_M (g), linear between 0.1→95, 0.2→85, 0.4→70, 0.6→55, 0.8→45, 1.2→30 (fallback: NRI earthquake rating) | recently active fault (historic / <15 ka) ≤1 km −15, ≤3 km −10, ≤10 km −5; CA Alquist-Priolo zone −20, liquefaction zone −10, EQ-landslide zone −10; floor 5 |
| **wildfire** | CA FHSZ: Very High 15 · High 35 · Moderate 60 · none 92; else USFS WHP: Very Low 90 · Low 80 · Moderate 60 · High 35 · Very High 15 · Non-burnable 92 · Water 95; else NRI rating | not in High/VH but one ≤300 m −15, ≤1 km −10, ≤2 km −5; NRI wildfire Very High −10, Relatively High −5 |
| **other** | 100 − 0.5 × (top NRI score among wind, tornado, hurricane, hail, heat, cold, winter, ice, drought, lightning, landslide, tsunami, volcano, avalanche) − 0.2 × (second) | radon: EPA Zone 1 / CGS High −10, Zone 2 / CGS Moderate −5; CA tsunami hazard area −15; CGS landslide susceptibility class ≥8 −10, 5–7 −5 |
| **overall** | 0.6 × mean(available sub-scores) + 0.4 × min(sub-scores) | the worst hazard dominates by design |
Rough reading: ≥ 80 low concern · 60–79 some concern, worth a closer look · 40–59 significant, budget for insurance /
mitigation · < 40 serious. Scores are a screening heuristic for comparing properties, not an actuarial rating.

## Interpreting the results for a buyer
**Flood (FEMA NFHL).**
- *Zones A, AE, AH, AO, AR, A99, V, VE = Special Flood Hazard Area* (≥1 % annual chance; ~26 % over a 30-year mortgage).
  Flood insurance is **mandatory** for loans from federally regulated/insured lenders (and FHA/VA/USDA, Fannie/Freddie).
  Standard homeowner policies exclude flood; NFIP caps building cover at $250k / contents $100k; new NFIP policies have a
  30-day wait unless bought at closing. V/VE adds wave action (stricter building rules, highest premiums); "FLOODWAY" in
  the subtype means the most restrictive development rules. Ask for an Elevation Certificate; if the house sits above the
  BFE a Letter of Map Amendment (LOMA) can remove the requirement.
- *Shaded X* (0.2 % / 500-year, levee-protected, or shallow 1 % flooding): moderate risk, insurance optional and usually
  inexpensive. *Unshaded X*: minimal mapped risk — but FEMA notes more than 20 % of NFIP claims come from outside high-risk
  zones. *Zone D*: never studied — risk is unknown, not low; get a quote and ask neighbors / the city about drainage.
- Under NFIP Risk Rating 2.0 premiums are property-specific (elevation, distance to water, rebuild cost), so
  `nearestSfha`, `elevationM` and the panel date (`firmEffective`) matter even outside an SFHA. Old panels may be
  superseded by pending preliminary maps (not included).

**Earthquake.**
- `pgaM` is the code-level (MCE, ~2 % in 50 years) site-adjusted peak ground acceleration: < 0.1 g low, 0.2–0.5 g
  moderate–strong, > 0.5 g strong, ≥ 0.8 g very strong (near major faults). `sdc` D–F = high seismic design category;
  E/F typically within a few km of a major active fault.
- Earthquake damage is excluded from homeowner policies; in California coverage comes from the California Earthquake
  Authority or private carriers with 5–25 % deductibles. Retrofits (foundation bolting, cripple-wall bracing, soft-story,
  water-heater strapping — the last is required at sale in CA) cut losses and can lower premiums (CA Earthquake Brace +
  Bolt grants).
- Faults: `recentlyActive` = historic or latest-Quaternary (< 15 ka) — the kind that can rupture the surface. Older
  Quaternary faults are much less likely to move. `slipRate` "Greater than 5.0 mm/yr" marks plate-boundary faults
  (San Andreas, Hayward, Calaveras). Distance to a fault matters less for shaking than site soils and building type; it
  matters a lot for surface rupture (that is what Alquist-Priolo zones capture).
- History: `countM4Within50km` and the largest events show how often shaking has been felt; the chart list feeds a
  magnitude/time plot. Pre-1932 magnitudes and locations are estimates.

**Wildfire.**
- California FHSZ: Very High / High / Moderate classes reflect hazard (fuels, slope, weather), not house-specific risk.
  In an FHSZ expect defensible-space rules (100 ft; ember-resistant "Zone 0" next to the house), Chapter 7A
  ember-resistant construction for new work/remodels, NHD disclosure, and — in High/Very High zones — additional
  defensible-space / home-hardening seller disclosures (AB 38, Civil Code §1102.19; confirm current requirements).
  Insurance can be scarce: get quotes before removing contingencies; the California FAIR Plan is the insurer of last
  resort. `nearestHighOrVeryHigh` within ~1 mile still signals ember exposure.
- SRA vs LRA: SRA = state (CAL FIRE) fire protection; the SRA map is in force since 2024-04-01. LRA maps (2025) are CAL
  FIRE recommendations that each city/county adopts by ordinance — check local adoption status.
- Outside California `whp` (USFS Wildfire Hazard Potential, 270 m): High/Very High = fuels likely to produce intense,
  hard-to-control fire; "Non-burnable" includes developed land, which can understate ember risk at the WUI edge.

**FEMA National Risk Index (tract).** Scores are national percentiles (e.g. 71.8 ⇒ riskier than ~72 % of US tracts);
ratings run Very Low → Very High. `expectedAnnualLoss.totalUSD` is for the whole tract (buildings + people + agriculture),
not one house. Use `topHazards` to see which perils drive local losses (often hurricane/tornado/hail/heat outside CA).
Social vulnerability and community resilience modify the composite score.

**Radon.** EPA Zone 1 (> 4 pCi/L predicted average), Zone 2 (2–4), Zone 3 (< 2) are county-level and cannot predict an
individual house; EPA recommends fixing at ≥ 4 pCi/L and considering it at 2–4. Test during the inspection period
(short-term kits are cheap); sub-slab depressurization typically costs ~$800–2,500. In California the CGS zones
(`radon.california`) give the share of homes likely to exceed 4 pCi/L where studies exist.

**California Natural Hazard Disclosure.** Sellers must disclose (Civil Code §1103) if the property is in: a FEMA SFHA,
a dam-failure inundation area, a Very High FHSZ, an SRA wildland area, an Alquist-Priolo Earthquake Fault Zone, or a
Seismic Hazard Zone (liquefaction / earthquake-induced landslide). `caNaturalHazardDisclosure` previews those items;
the binding answer comes from the NHD report ordered in escrow. "No (not mapped)" means CGS has not evaluated that
quadrangle — not that the hazard is absent. Inside AP or Seismic Hazard Zones, new construction and major additions
require site-specific geologic/geotechnical reports.

**Mitigation quick list:** flood — elevate utilities, flood vents, backflow valve, sump, grading; quake — bolt & brace,
strap water heater, secure tall furniture, evaluate soft-story/masonry; fire — Zone 0 clearance, ember-resistant vents,
Class A roof, gutter guards; radon — test, then mitigate if ≥ 4 pCi/L; landslide — drainage, retaining-wall inspection.

## Caveats
- Screening from public GIS layers; not an official flood determination (lenders use a Standard Flood Hazard
  Determination), NHD report, geotechnical study, or insurance quote. Map lines are approximate at the parcel scale.
- FEMA maps can be decades old; Zone D and unmapped areas exist; LOMRs/LOMAs are not applied.
- ASCE 7-22 values use the "Default" site class; real soils (e.g. bay mud) can amplify shaking. Absence of a mapped
  fault does not mean absence of hazard (blind thrusts).
- NRI is tract-level and model-based; values change between releases (`nri.version`).
- California add-ons only run when `geo` says California (`state` CA, `stateFips` 06 or a 06… county); other states'
  geological surveys (OR DOGAMI, WA DNR, UT UGS…) are not yet integrated. No national sea-level-rise layer (NOAA's
  service was unreliable for point queries).
- EPA radon zones come from a static table (verified 58/58 for California; see data sources); counties created after
  1993 return `null`.
- Each failed source adds a `warnings[]` entry and leaves its fields `null`; scores are computed from what is available.

## Files
- `src/modules/hazards.rs` — `run_hazards`, source queries, scoring (`compute_scores`), highlights
  (`build_highlights`), California NHD summary (`build_nhd`), distance helpers, map layers and the compiled-in EPA
  county radon-zone table (`RADON_TABLE`, `epa_radon_zone`).
- `src/shared.rs` — `HttpClient` (ArcGIS queries, caching) and geometry helpers (`point_in_ring`,
  `nearest_on_polyline`, `haversine_m`, ...).
- `tests/hazards_test.rs` — radon lookup, flood-zone parsing and scoring tests, plus a live run.
- `references/data-sources.md` — endpoint catalog, licenses, limits, failure modes, test-point results.
