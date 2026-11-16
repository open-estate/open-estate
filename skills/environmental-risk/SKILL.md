---
name: environmental-risk
description: >-
  Environmental screening for a specific US property address (lat/lon). Use when a buyer, agent or report needs
  contaminated/regulated sites nearby — Superfund NPL sites and boundaries, Toxics Release Inventory facilities,
  brownfields, RCRA hazardous-waste TSDFs/large generators, leaking underground storage tanks, and in California
  State Water Board GeoTracker and DTSC EnviroStor cases — plus CalEnviroScreen 4.0 (CA) or EJScreen-mirror
  pollution percentiles, the last 30 days of modeled air quality (PM2.5, US AQI), the public water system serving
  the home with its SDWIS violation and lead history, and modeled road/aviation/rail noise. Returns sourced JSON
  with distances, map layers, 0–100 sub-scores and plain-language highlights. Not for natural hazards (flood,
  wildfire, earthquake, landslide — use the natural-hazards skill) and not a substitute for a Phase I ESA.
---

# Environmental risk

## Purpose
Give a home buyer a fast, sourced, factual screen of man-made environmental conditions around an address and
explain what they mean (and don't mean). Everything comes from keyless public APIs; nothing is fabricated — a
missing value is `null` with a warning.

## Inputs
`run_environment(http, env, lat, lon, geo)` in `src/modules/environment.rs`, per `skills/_shared/CONTRACT.md`:

| field | required | used for |
|---|---|---|
| `lat`, `lon` | yes | all queries (invalid coordinates return an error) |
| `geo.state` (or `stateFips`) | recommended | `CA` → GeoTracker, EnviroStor, CalEnviroScreen, CA water boundaries; others → EPA UST Finder; `AK`/`HI` noise layers |
| `AIRNOW_API_KEY` (environment) | optional | current monitor AQI (skipped silently if absent) |

## How to run
The report orchestrator (`src/web/orchestrator.rs`) calls it for every report with the rooftop-refined point; the
result is `report.modules.environment` (e.g. `GET /api/report?address=...`). From Rust:
```rust
use open_estate::modules::environment::run_environment;

let out = run_environment(&http, &env, lat, lon, &geo).await?;   // never fails for a failed source
```
Requests go through the shared `HttpClient` (cached, see `skills/_shared/CONTRACT.md`; noise tiles are fetched
directly). Typical cold run ≈ 8–10 s (EPA FRS layers are the slowest), ~25–35 requests. Tests:
`tests/environment_test.rs` (air, noise, contamination and water scoring offline; one live run against San Jose).

Files: `src/modules/environment.rs` holds the whole module — contaminated-site sources and contamination score
(`contamination_score`), CalEnviroScreen / EJScreen screening, air (`compute_air_stats`, `air_score`), drinking water
(`water_score`), BTS noise tiles with a small PNG decoder plus an OpenStreetMap road/rail/aviation corridor model
when the tiles are unavailable (`noise_score`), highlights and map layers. Endpoint catalog:
`references/data-sources.md`.

## Output (`data`)
```text
state
sites
  searchRadiiMi         {npl:5, sems:1, rcraTsd:1, dtscHw:1, rcraLqg:1, tri:3, brownfield:3, lust:1, cleanup:1, envirostor:3}
  counts[]              {program,label,radiusMi,total,active,managed,closed,referredOrUnknown,within:{0.25mi,0.5mi,1mi,3mi}}
  totalWithinSearchRadii
  nearest[25]           {program,programLabel,name,address,status,statusGroup,distanceMi,distanceM,direction,lat,lon,id,url,
                         chlorinatedSolvents?,details{…program specific: caseType, contaminants, mediaAffected, statusDate,
                         listed, constructionComplete, siteType, landUseRestriction…}, distanceToBoundaryMi? (NPL)}
  nearestByProgram      {program: item}
  insideNplBoundary     bool
  nplBoundaries[]       {epaId,name,feature,status,inside,distanceToEdgeM,distanceMi}
screening
  calEnviroScreen       {version,tract,overallPercentile,pollutionBurdenPercentile,indicators[{key,label,group,percentile,value}],note} | null
  ejscreen              {version,blockGroup,nationalPercentiles[{key,label,percentile}],note} | null
air                     {source,modelGridPoint,period{start,end,days},pm25{mean,maxDailyMean,maxHourly,daysOver35},
                         pm10{mean,maxDailyMean,maxHourly,daysOver50,daysOver150},
                         usAqi{meanDailyMax,max,daysOver50,daysOver100,pctDaysOver100},ozoneMeanUgM3,no2MeanUgM3,
                         daily[{date,pm25,pm10,aqi}], airnowCurrent?[]}
water                   {systems[],primary{pwsid,name,population,connections,sourceWater,owner,systemType,…},
                         violations10y{since,total,healthBased,healthBasedLast3y,unresolvedHealthBased,byCategory,recent[10]},
                         leadAndCopper{lead90thPctMgL,leadPeriod,leadActionLevelMgL,copper90thPctMgL,…},
                         recentCompliance{quartersReviewed,quartersWithViolation,firstQuarter,lastQuarter,statuses},links} 
noise                   {dataset,metric,year,region,radiusM,pixelSizeM,atProperty{all,road,aviation,rail},maxWithinRadius{…}}
                         each {label,lowDb,highDb}; "<45 dB" = below the mapped threshold
scores                  {contamination,air,water,noise,overall,airBasis,contaminationPenalties{total,…per program},weights}
highlights[]            {level:'good'|'watch'|'bad', text}
mapLayers[]             NPL boundary polygons + one point layer per program (≤60 points each), popups included
```
`statusGroup`: `active` (open/unresolved), `managed` (remedy in place, O&M or land-use restriction, "eligible for
closure"), `closed` (closed / no further action / no action required), `referred` (handed to another agency),
`unknown`.

## Scores (0–100, higher = better)
- **contamination** = `100·e^(−P/60)`. P sums, per site, `weight × status × type × max(0, 1 − d/influence)`,
  capped per program. Weight/influence(mi)/cap: NPL 40/3/60 (distance to boundary when mapped) · RCRA TSDF 15/1/30 ·
  DTSC HW 12/1/24 · cleanup (GeoTracker) 12/1/30 · EnviroStor 12×site-type/1.5/30 · Superfund non-NPL 12/1/24 ·
  LUST 10/0.5/25 · TRI 8/2/16 · brownfield 6/1/12 · RCRA LQG 2/0.5/6. Status: active 1, managed .5, unknown .5,
  referred .3, closed .1; chlorinated-solvent cases ×1.5. Inside an active NPL boundary +50 (deleted +25).
  Anchors: a few closed tank cases nearby ≈ 90–95; active NPL site 0.5 mi away ≈ 55–60; inside an NPL boundary < 30.
- **air** = `0.7 × (100 − (0.4·PM2.5 + 0.3·ozone + 0.3·diesel percentile))` (CalEnviroScreen statewide pct in CA,
  EJScreen national pct elsewhere) `+ 0.3 × clamp(125 − 30-day mean daily AQI) − % days AQI>100`.
  `scores.airBasis` names the basis used. CA scores are therefore relative to California (a tract at CES's 17th PM2.5
  percentile can sit at EJScreen's 90th nationally), so compare air scores within a state, not across states.
- **water** = 100 − health-based violations (15 each in last 3 y, 8 older; ≤45) − 25 if unresolved − 2 per
  monitoring/reporting violation (≤10) − 3 per ECHO quarter with violations (≤15) − lead 90th pct (>0.015 mg/L: 20,
  >0.010: 8) − CalEnviroScreen drinking-water pct × 0.2 (≤20) or EJScreen non-compliance pct × 0.15. `null` when no
  public system is mapped (possible private well).
- **noise** from combined LAeq,24h at the parcel: <45 dB 100 · 45–50 90 · 50–55 75 · 55–60 60 · 60–70 40 ·
  70–80 20 · 80–90 5 · ≥90 0; −5 if ≥60 dB within 150 m while the parcel is <55.
- **overall** = weighted mean (contamination .35, air .25, water .2, noise .2) of available sub-scores.
Scores are screening heuristics for comparing addresses, not risk estimates.

## Interpreting results for buyers
- **Distance matters more than counts.** Urban addresses almost always have closed gas-station tank cases and
  hazardous-waste generators within a mile; that is normal. What deserves attention is an *open* case within
  ~0.25 mi, an NPL site within ~1 mi, or being inside/adjacent to a mapped Superfund boundary.
- **Superfund (NPL).** NPL listing means EPA found a release serious enough for long-term federal cleanup. Most
  impacts are on-site or along a groundwater plume that moves with groundwater flow (direction matters — check the
  site's Record of Decision / five-year review on the EPA profile link). "Construction complete" means the remedy is
  built, not that contamination is gone; plumes and land-use controls can persist for decades. Deleted NPL = cleanup
  goals met.
- **TRI facilities** report *permitted* releases of listed chemicals; proximity is a proxy for air emissions and
  accident potential, not proof of exposure. Look at the facility's TRI/ECHO page for release amounts and violations.
- **LUST (leaking tanks).** Petroleum plumes are usually short (tens to a few hundred feet) and biodegrade; closed
  cases rarely matter to neighbors. Open cases within a block or two, especially upgradient, warrant questions.
- **Chlorinated solvents & vapor intrusion.** Dry cleaners, metal shops and electronics plants (PCE/TCE) create
  long, persistent groundwater plumes and soil vapor that can enter buildings (vapor intrusion) even when the
  water isn't used. A nearby solvent case — open or recently closed — is worth asking about: was indoor-air or
  sub-slab sampling done? Is there a land-use covenant or vapor-mitigation system?
- **Groundwater plumes vs drinking water.** In areas served by a public system (see `water`), plume groundwater is
  usually not what comes out of the tap; the concerns are vapor intrusion, restrictions on private wells, and
  excavation/construction.
- **CalEnviroScreen / EJScreen** rank census tracts / block groups against the state (CES) or nation (EJScreen).
  They describe the area, not the parcel, and CES's overall score also includes population factors.
- **Air** values are modeled and a 30-day window is seasonal; long-term percentiles (CES/EJScreen) are the better
  guide to chronic exposure.
- **Drinking water**: system-level compliance and lead 90th percentiles don't test this home's plumbing; homes built
  before 1986 may have lead solder/fixtures — ask for a tap test, and ask the utility about the service-line
  material (lead service line inventories are public under the 2021 Lead & Copper Rule revisions).
- **Noise** is a 24-h average (LAeq). Night-time peaks (trains, flight paths) are not captured; listen at night.

### What to ask / do
1. Ask the seller for any environmental reports, tank removal records (heating-oil tanks!), and known contamination;
   most states require disclosure of known hazards.
2. For anything within ~0.25 mi that is open, or a solvent case: read the regulator file (links in `url`), look for
   closure letters, land-use covenants, groundwater-flow direction and vapor-intrusion sampling.
3. Commission a **Phase I Environmental Site Assessment** (ASTM E1527-21) for unusual situations (former industrial
   / agricultural land, a site inside or adjoining a cleanup boundary, commercial or mixed-use purchases). A Phase I
   is a records-and-site-visit review; a Phase II samples soil/groundwater/soil gas. Lenders require them for
   commercial deals, rarely for houses.
4. Private well (no public system found)? Test for coliform bacteria, nitrate, arsenic, and local contaminants.
5. Radon is not covered here (geology-driven) — test during the inspection period.

### California: Natural Hazard Disclosure vs environmental reports
The NHD statement (Civil Code §1103) covers *natural* hazard zones (flood, fire, earthquake fault/seismic,
dam inundation). Commercial NHD reports often bundle an "environmental" add-on built from the same GeoTracker /
EnviroStor / EPA lists used here, but that is a separate product. Sellers' environmental duties come from other
rules: the Transfer Disclosure Statement (Civil Code §1102) asks about known environmental hazards, the
"Residential Environmental Hazards" booklet is customarily delivered, Health & Safety Code §25359.7 requires owners
of *non-residential* property to disclose known hazardous-substance releases, and DTSC/Water Board land-use
covenants are recorded against title. This module does not replace the NHD (zones), the TDS, or a Phase I.

## Caveats
- Databases list *known, reported* sites; absence of records is not evidence of a clean site. Agency coordinates
  can be off by tens to hundreds of meters (FRS accuracy field; UST Finder ZIP-level geocodes are excluded).
- Radii follow buyer-friendly conventions (wider than ASTM's minimums for most lists, narrower for some).
- EJScreen data come from a third-party mirror of EPA's 2024 release (EPA took EJScreen down in 2025); it may vanish.
- GeoTracker/EnviroStor profile links block scripted clients (HTTP 403) but open in browsers.
- Open-Meteo's free API is for non-commercial use; use a paid key for commercial hosting.
- Noise tiles are decoded from the BTS map rendering (30 m classes, 2022 traffic); no identify service exists.
- Fair-housing: only environmental indicators are requested; no demographic fields are fetched or reported.
