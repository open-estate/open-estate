# neighborhood-livability — civic sources (`src/modules/civic.rs`)

Verified live (Nov 2026) against US residential Census block groups.
Run: `run_civic(http, env, lat, lon, geo)`, called by the report orchestrator for every report (live test in
`tests/livability_fengshui_test.rs`; `DATA_GOV_API_KEY` env var optional). Module output:
`data.{walkability, schools, crime, scores, highlights, mapLayers}`.

## 1. EPA National Walkability Index
| | |
|---|---|
| Primary | `https://services.arcgis.com/cJ9YHowT8TU7DUyn/ArcGIS/rest/services/Smart_Location_Database_/FeatureServer/0` (point query) |
| Fallback | `https://geodata.epa.gov/arcgis/rest/services/OA/WalkabilityIndex/MapServer/0` (same fields), then `where=GEOID20='{blockGroup}'` |
| Fields | `GEOID10, GEOID20, NatWalkInd` (1–20), `D2A_Ranked` (employment+household mix), `D2B_Ranked` (employment mix), `D3B_Ranked` + `D3B` (intersection density /sq mi), `D4A_Ranked` + `D4A` (m from population-weighted centroid to nearest transit stop; −99999 = none) |
| Formula | `NatWalkInd = D2A_Ranked/6 + D2B_Ranked/6 + D3B_Ranked/3 + D4A_Ranked/3` (verified: 15/6+15/6+14/3+15/3 = 14.67) |
| Categories | 1–5.75 Least walkable · 5.76–10.5 Below average · 10.51–15.25 Above average · 15.26–20 Most walkable |
| Caveats | SLD v3 (2021) uses 2018-era inputs and 2010/2018 block groups; block-group average, not parcel-level. The layer also holds demographic fields — **not requested** |
| Score | `scores.walkability = round((NatWalkInd − 1) / 19 × 100)` |

## 2. Schools (NCES EDGE, keyless ArcGIS MapServers)
| Layer | URL | Fields used |
|---|---|---|
| Public schools + admin data | `https://nces.ed.gov/opengis/rest/services/K12_School_Locations/EDGE_ADMINDATA_PUBLICSCH_{2425\|2324\|2223}/MapServer/1` (newest first) | `NCESSCH, SCH_NAME, LEA_NAME, LEAID, GSLO, GSHI, SCHOOL_LEVEL, CHARTER_TEXT, VIRTUAL, SCHOOL_TYPE_TEXT, SY_STATUS_TEXT, MEMBER/TOTAL` (enrollment), `STUTERATIO, LSTREET1, LCITY, PHONE` |
| Private schools (PSS) | `https://nces.ed.gov/opengis/rest/services/K12_School_Locations/EDGE_GEOCODE_PRIVATESCH_{2324\|2122}/MapServer/0` | `PPIN, NAME, STREET, CITY, SCHOOLYEAR` (no grades/enrollment in this layer) |
| Alternatives (verified) | `EDGE_GEOCODE_PUBLICSCH_2425/MapServer/0` (locations only); AGOL `https://services1.arcgis.com/Ua5sjt3LWTPigjyD/arcgis/rest/services/Public_School_Locations_Current/FeatureServer/0` and `.../Private_School_Locations_Current/FeatureServer/0` | |

Radius 2 mi (re-filtered by haversine). Closed/inactive/future and full-virtual schools are dropped. Level is
NCES `SCHOOL_LEVEL` (Elementary/Middle/High/Secondary) or derived from grade span. `inAssignedDistrict` compares the
normalized `LEA_NAME` with `geo.schoolDistricts` (Census geocoder) — e.g. "Union Elementary School District" ≡
"Union Elementary". District ≠ attendance zone; the module says so. Links:
`https://nces.ed.gov/ccd/schoolsearch/school_detail.asp?ID={NCESSCH}`,
`https://nces.ed.gov/surveys/pss/privateschoolsearch/school_detail.asp?ID={PPIN}`.
The admin layer also contains race/ethnicity counts and free/reduced-lunch fields — **never requested**.

## 3. Crime — FBI Crime Data Explorer
The agency directory is requested from the keyless `https://cde.ucr.cjis.gov/LATEST` root first, then from
`https://api.usa.gov/crime/fbi/cde` (same paths, plus `API_KEY={key}`); the monthly series use the root that
answered and switch to the other one if that fails.

| Step | Endpoint |
|---|---|
| Agencies by state | `https://api.usa.gov/crime/fbi/cde/agency/byStateAbbr/{ST}?API_KEY={key}` → `{COUNTY: [{ori, agency_name, agency_type_name, counties, is_nibrs, nibrs_start_date, latitude, longitude}]}` |
| Monthly series | `https://api.usa.gov/crime/fbi/cde/summarized/agency/{ORI}/{violent-crime\|property-crime}?from=MM-YYYY&to=MM-YYYY&API_KEY={key}` → `offenses.rates{"<State> Offenses","United States Offenses","<Agency> Offenses", …Clearances}` (per 100k per month), `offenses.actuals{"<Agency> Offenses"}`, `populations.population{…}`, `tooltips["Percent of Population Coverage"]`, `cde_properties.max_data_date.UCR` |

- Key (api.usa.gov fallback only): `DATA_GOV_API_KEY`, else `DEMO_KEY`. Observed `X-RateLimit-Limit: 10` with DEMO_KEY (per IP, rolling hour) —
  one run uses 3 calls (the agency list is cached for 14 days, the series for 7). Get a free key at https://api.data.gov/signup/ for
  production (1,000 req/h). Keys appear in request URLs, so they also end up in the local cache keys — don't share
  the HTTP cache directory (`HTTP_CACHE_DIR`, default `.cache/http`).
- Agency match: `geo.place` minus the Census suffix ("San Jose city" → "San Jose") against `City` agencies in the
  county list (`"{place} Police Department"` exact, then prefix); CDPs / unincorporated → county sheriff; a city
  agency with no complete reporting year falls back once to the sheriff (contract-policed cities).
- Annual figures: sum of 12 monthly agency counts (year left null if any month missing — e.g. San Jose 2021 during
  the NIBRS transition); agency rate = count / FBI population × 100k; state & US rates = sum of monthly CDE rates,
  omitted when the FBI estimate covered < 60 % of the population.
- Test result: San Jose PD (ORI CA0431300) 2025 — 5,025 violent (505.3/100k; CA 435.1, US 329.6), 23,902 property
  (2,403.6/100k; CA 1,822.3, US 1,552.3).

### City open-data alternative (documented, not used)
Many cities publish incident or calls-for-service data via CKAN/Socrata. San Jose:
`https://data.sanjoseca.gov/api/3/action/package_show?id=police-calls-for-service` → one CSV resource per year
(e.g. 2026 = `dc0ec99c-0c6b-45fb-b1ec-faf072fe4833`), queryable with
`https://data.sanjoseca.gov/api/3/action/datastore_search?resource_id={id}&limit=100` (fields `CALL_TYPE, PRIORITY,
OFFENSE_DATE, ADDRESS` (block-level, e.g. "[1100]-[1200] S KING RD"), `FINAL_DISPO`; no coordinates). These are
*calls for service*, not crimes, and need geocoding/aggregation — use only for context, never for "safety" labels.

## Fair-housing guardrails (enforced in code)
- No "safe/unsafe", "good/bad neighborhood" wording; crime highlights are agency-wide facts with explicit
  "not neighborhood-specific" text and state/US comparisons only. Levels are `good` (below both state and US rates)
  or `watch` (otherwise) — never `bad`.
- No demographic composition (race, ethnicity, income, free-lunch eligibility) is requested from any service.
- School information is factual (level, grades, enrollment, student/teacher ratio, district) — no rankings.
