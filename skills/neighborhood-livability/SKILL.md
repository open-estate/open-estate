---
name: neighborhood-livability
description: >-
  Everyday-life context around a US address from open data: nearest grocery, restaurants, cafés, parks, schools,
  healthcare, transit and other amenities with counts within ¼/½/1 mile and an amenity-access score (OpenStreetMap
  via Overpass); proximity to nuisances (freeways, arterials, rail, high-voltage lines, substations, airports);
  EPA National Walkability Index; nearby public and private schools (NCES) with district flags; city-level FBI crime
  rates vs state and nation. Use when asked what's nearby, how walkable or convenient a home is, which schools are
  close, or how close it is to highways/power lines/airports. Facts only — no neighborhood "ratings" by residents.
---

# Neighborhood & Livability

Three Rust modules in `src/modules/`, run by the report orchestrator (`src/web/orchestrator.rs`) and stored under
`report.modules.<id>`:

| Module | Content | Sources |
|---|---|---|
| `amenities.rs` (`run_amenities`, `id: amenities`) | POIs within 1–2 mi (21 categories incl. supermarkets, EV charging, public parking, bike share, gas stations), `mobilityHub` (EV connectors + IRS §30C/state rebates + DOE AFDC, parking, bike share, regional AAA/EIA gas benchmark, CMS hospital Care Compare, 1-mi commercial vitality mix, municipal waste/311 links), nearest-of-each, counts by band, access score 0–100, lines (freeways/arterials/rail/power/water), map layers | OSM Overpass (multi-endpoint fallback), keyless Esri ArcGIS POI & airport fallbacks |
| `access.rs` (`run_access`, `id: access`) | Free-flow drive & walk times + estimated rideshare/taxi fare (`estRideshareUSD`) to grocery, transit, parks, hospital, fire station, airport & downtown | OSRM table service (road-distance speed estimate when it fails) |
| `civic.rs` (`run_civic`, `id: civic`) | EPA walkability index, NCES public/private schools ≤2 mi (+ GreatSchools/Niche links), FBI CDE agency crime trends | EPA SLD, NCES EDGE, FBI CDE (keyless `cde.ucr.cjis.gov`, then `api.usa.gov` with `DATA_GOV_API_KEY` or DEMO_KEY) |

## Amenity access score
Walk-Score-like but transparent: weighted categories (grocery 3, restaurants 10 nearest, cafés 2, shopping 5,
parks, schools, library/community, healthcare, transit 2, fitness, bank/post) × distance decay (full credit ≤ ¼ mi,
zero beyond 1¼ mi). Labels describe destination proximity only (not sidewalk quality or safety).

## Run
Called by the orchestrator for every report (`GET /api/report?address=...`): civic in the first wave, amenities in
the second (reusing the OSM vicinity already fetched for the rooftop), access in the third (it needs the amenities
and civic results). From Rust:
```rust
use open_estate::modules::{access::run_access, amenities::run_amenities, civic::run_civic};

let a = run_amenities(&http, &env, lat, lon, &geo, None).await?;   // None: no pre-fetched OSM vicinity
let c = run_civic(&http, &env, lat, lon, &geo).await?;
let t = run_access(&http, lat, lon, &geo, Some(&a.data), Some(&c.data), None).await?;   // None: no city hall hint
```
Tests: `tests/livability_fengshui_test.rs` (live amenities, access and civic run for San Jose).
Overpass etiquette: one combined query per theme, results cached. Public instances throttle and go offline, so
`HttpClient::overpass` hedges queries across `overpass-api.de` and `overpass.private.coffee` inside one
time budget (override with `OVERPASS_API_URLS`). When Overpass is unavailable each dataset falls back to Esri-hosted
OpenStreetMap layers, HIFLD transmission lines and FAA aviation facilities (`data.osmSource`). FBI crime series with
missing years (e.g. 2021 for most California agencies) are filled from California DOJ OpenJustice (`backfilled: true`)
and optionally supplemented by municipal Socrata open-data incident endpoints (`CRIME_MUNICIPAL_ENDPOINTS_JSON`).

## Fair-housing guardrails
Report distances, counts and official statistics. Do not characterize residents, do not call areas safe/unsafe or
good/bad, do not use demographic composition, and present crime only as jurisdiction-wide rates with caveats.
School figures are shown as cited public facts; they never enter the Open Estate Score, and schools and crime count in
Your Match only when the buyer gives them weight (`us-property-report/references/compliance.md`).
