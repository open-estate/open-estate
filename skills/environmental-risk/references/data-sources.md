# environmental-risk — data source catalog

All endpoints below were verified live (Nov 2026) across US residential coordinates. All are keyless unless noted. ArcGIS queries use
`geometry={lon},{lat}&geometryType=esriGeometryPoint&inSR=4326&spatialRel=esriSpatialRelIntersects` plus
`distance={meters}&units=esriSRUnit_Meter&outSR=4326&returnGeometry=true&f=json` for radius searches. Every result is
re-filtered by haversine distance (some services buffer in Web Mercator, which inflates the radius at mid-latitudes).

`EPA` below = `https://services.arcgis.com/cJ9YHowT8TU7DUyn/ArcGIS/rest/services` (US EPA ArcGIS Online org).
All US federal data is public domain; cite the agency.

## 1. Contaminated & regulated sites

| Program (radius) | Endpoint | Coverage | Key fields | Notes / failure modes |
|---|---|---|---|---|
| Superfund NPL points (5 mi) | `EPA/Superfund_National_Priorities_List_(NPL)_Sites_with_Status_Information/FeatureServer/0` | National, 1,840 sites (final, proposed, deleted) | `Site_Name, Site_EPA_ID, SEMS_ID, Status` (`NPL Site` / `Proposed NPL Site` / `Deleted NPL Site`), `Listing_Date, Construction_Completion_Date, Deletion_Date, Site_Progress_Profile` (HTML `<a href>` → parse URL) | 3–8 s latency. Dates are `MM/DD/YYYY` strings. Profile: `https://cumulis.epa.gov/supercpad/cursites/csitinfo.cfm?id={SEMS_ID 7-digit}` |
| Superfund NPL boundaries (5 mi) | `EPA/FAC_Superfund_Site_Boundaries_EPA_Public/FeatureServer/0` | National, 2,115 polygons (not every site has one) | `EPA_ID, SITE_NAME, NPL_STATUS_CODE` (F final, P proposed, D deleted, R removed, N not NPL), `SITE_FEATURE_NAME` | Request `maxAllowableOffset=0.0001&geometryPrecision=5` to keep payloads small. Boundaries are approximate (EPA disclaimer). Used for inside/outside test and distance-to-edge |
| Superfund non-NPL (1 mi) | `EPA/FRS_INTERESTS_SEMS/FeatureServer/0` | National | FRS fields (below), `INTEREST_TYPE='SUPERFUND (NON-NPL)'` | `ACTIVE_STATUS` usually null → status "unknown" |
| TRI reporters (3 mi) | `EPA/FRS_INTERESTS_TRI/FeatureServer/0` | National | FRS fields; `ACTIVE_STATUS` ACTIVE/INACTIVE | Location = FRS best pick (`ACCURACY_VALUE` m). Link: ECHO DFR `https://echo.epa.gov/detailed-facility-report?fid={REGISTRY_ID}` |
| Brownfields / ACRES (3 mi) | `EPA/FRS_INTERESTS_ACRES/FeatureServer/0` | National (EPA-grant properties only) | FRS fields | Only properties assessed/cleaned with EPA brownfield grants; state VCP sites are not here. Link: FRS facility page |
| RCRA TSDF (1 mi) | `EPA/FRS_INTERESTS_RCRA_TSD/FeatureServer/0` | National | FRS fields, `ACTIVE_STATUS` Y/N | |
| RCRA LQG (1 mi) | `EPA/FRS_INTERESTS_RCRA_LQG/FeatureServer/0` | National | FRS fields | Pharmacies, hospitals, labs, manufacturers. A generator ≠ contamination |
| LUST releases, non-CA (1 mi) | `EPA/UST_Finder_Feature_Layer_2/FeatureServer/1` (layer 0 = UST facilities) | National snapshot (EPA ORD/OUST + ASTSWMO; not live) | `LUST_ID, Name, Address, City, State, Status, Substance, Reported_Date, Address_Match_Type` | Many releases are geocoded to a ZIP centroid (`Address_Match_Type='Postal'`) → excluded (count reported in warnings). ~7 s latency. Used only outside CA (CA uses GeoTracker) |
| GeoTracker (CA, 1 mi) | `https://gispublic.waterboards.ca.gov/portalserver/rest/services/Hosted/geotracker_sites_download/FeatureServer/371` + `where=case_type IN ('LUST Cleanup Site','Military UST Site','Cleanup Program Site','Military Cleanup Site','Military Privatized Site','Land Disposal Site')` | California, 73k records | `global_id, business_n, street_num, street_nam, city, case_type, status` (`Open - …` / `Completed - Case Closed`), `status_dat, begin_date` (epoch ms), `potential_` (contaminants of concern), `potential1` (media affected) | maxRecordCount 75,000; <1 s. Some `status_dat` values are corrupt (year 2212) → dropped. Profile `https://geotracker.waterboards.ca.gov/profile_report?global_id={global_id}` returns 403 to scripted clients (bot protection) but is the standard public URL for browsers. © CA State Water Resources Control Board (public records) |
| EnviroStor cleanup sites (CA, 3 mi) | `https://services3.arcgis.com/Oy2JTCD10wkoelxS/arcgis/rest/services/Envirostor_Public_Data_Export/FeatureServer/0` | California (DTSC) | `project_name, site_type` (State Response, Federal Superfund, Voluntary Cleanup, School Investigation, Calmortgage, Tiered Permit, Evaluation …), `status` (Active, Certified, No Further Action, Refer: …, Certified O&M …), `status_date` (string), `envirostor_id, potential_coc, confirmed_coc, potential_media_affected, restricted_use, past_uses` | COC/media lists contain duplicates → de-duplicated. Profile `https://www.envirostor.dtsc.ca.gov/public/profile_report?global_id={envirostor_id}` (403 to scripts). © CA DTSC |
| DTSC hazardous-waste facilities (CA, 1 mi) | same service `/FeatureServer/1` (`/2` = inspection/enforcement sites, unused) | California | `epa_id, facility_name, facility_type, facility_status, permit_type` | |

FRS fields requested: `REGISTRY_ID, PRIMARY_NAME, LOCATION_ADDRESS, CITY_NAME, STATE_CODE, LATITUDE83, LONGITUDE83,
PGM_SYS_ID, INTEREST_TYPE, ACTIVE_STATUS`. FRS layers are slow (3–9 s each, maxRecordCount 2,000) but run in parallel.

Verified fallback (not used by default): `https://geopub.epa.gov/arcgis/rest/services/EMEF/efpoints/MapServer`
layers 0 Superfund, 1 TRI, 2 water dischargers, 3 air, 4 hazardous waste (all RCRA handler IDs incl. very small
generators — 701 records within 1 mi of the test point), 5 brownfields. Faster, but no status / generator class.

## 2. Screening indices

| Index | Endpoint | Coverage | Fields used | Notes |
|---|---|---|---|---|
| CalEnviroScreen 4.0 | `https://services1.arcgis.com/PCHfdHz4GlDNAhBb/arcgis/rest/services/CalEnviroScreen_4_0_Results_/FeatureServer/0` (point query) | California 2010 census tracts | `CIscoreP` (overall pct), `PollutionP` (pollution burden pct), indicator value + pct pairs: `ozone, pm, diesel, drink, lead, pest, RSEIhaz, traffic, cleanups, gwthreats, haz, iwb, swis` (+`P`) | OEHHA, Oct 2021. The layer also carries race/ethnicity and socioeconomic fields — **never requested** (fair-housing). Negative values = missing |
| EJScreen 2.3 (2024) mirror | `https://services.arcgis.com/gfpXnknjcY6QHKhU/arcgis/rest/services/EJSCREEN_Full_with_AS_CNMI_GU_VI/FeatureServer/0` | National block groups + territories | `ID` (block group), national percentiles `P_PM25, P_OZONE, P_DSLPM, P_NO2, P_RSEI_AIR, P_PTRAF, P_LDPNT, P_PNPL, P_PRMP, P_PTSDF, P_UST, P_PWDIS, P_DWATER` | EPA removed EJScreen in Feb 2025; EPA's own AGOL copies now return `499 Token Required` (verified). This is a **third-party copy** (owner: David Evans and Associates, "no warranty") — may vanish; module degrades to a warning. Demographic index fields are not requested |

## 3. Air quality

| Source | Endpoint | Notes |
|---|---|---|
| Open-Meteo Air Quality (keyless) | `https://air-quality-api.open-meteo.com/v1/air-quality?latitude={lat}&longitude={lon}&hourly=pm2_5,pm10,ozone,nitrogen_dioxide,us_aqi&start_date={today−30}&end_date={yesterday}&timezone=auto` | Copernicus CAMS global model (coarse grid; returned grid point is reported). Explicit dates make the cache roll daily. Free tier is for non-commercial use (<10k calls/day); a commercial deployment should switch to Open-Meteo's paid API (customer-* host + `apikey` parameter). Data CC BY 4.0 — attribute Open-Meteo / CAMS |
| EPA AirNow (optional) | `https://www.airnowapi.org/aq/observation/latLong/current/?format=application/json&latitude=..&longitude=..&distance=25&API_KEY=..` | Only when `AIRNOW_API_KEY` is set in the environment (not verified here — no key). Monitor-based current AQI |

Daily values: PM2.5 = mean of hourly values; AQI = max of hourly US AQI (Open-Meteo's hourly AQI already applies
24-h PM / 8-h O₃ averaging). Days with < 18 hourly values are skipped.

## 4. Drinking water

| Step | Endpoint | Notes |
|---|---|---|
| Service area (CA) | `https://gispublic.waterboards.ca.gov/portalserver/rest/services/Drinking_Water/California_Drinking_Water_System_Area_Boundaries/FeatureServer/0` | Fields `SABL_PWSID, WATER_SYSTEM_NAME, BOUNDARY_TYPE, POPULATION, SERVICE_CONNECTIONS, FEDERAL_CLASSIFICATION, REGULATING_AGENCY, OWNER_TYPE_CODE, VERIFIED_STATUS` |
| Service area (national) | `EPA/Water_System_Boundaries/FeatureServer/0` | EPA ORD/OW Community Water System Service Area Boundaries (state-provided + modeled). Fields `PWSID, PWS_Name, Population_Served_Count, Service_Connections_Count, Service_Area_Type` (wholesalers flagged), `Data_Provider_Type, Verification_Status`. ~5 s |
| System record | `https://data.epa.gov/efservice/WATER_SYSTEM/PWSID/{pwsid}/JSON` | `gw_sw_code`, `owner_type_code`, `pws_type_code`, population |
| Violations (10 y) | `https://data.epa.gov/efservice/VIOLATION/PWSID/{pwsid}/COMPL_PER_BEGIN_DATE/%3E/{YYYY-MM-DD}/JSON` | `violation_code, violation_category_code` (MCL/MRDL/TT/MR/Other), `is_health_based_ind, contaminant_code, rule_code, compliance_status_code, rtc_date`. Code dictionaries in `src/modules/environment.rs` cover common codes; unknown codes are passed through as "code X" |
| Lead & copper | `https://data.epa.gov/efservice/LCR_SAMPLE/PWSID/{pwsid}/LCR_SAMPLE_RESULT/JSON` (table join) | `contaminant_code` PB90/CU90, `sample_measure` (mg/L), monitoring period dates |
| Quarterly compliance | `https://echodata.epa.gov/echo/dfr_rest_services.get_sdwis_compliance?output=JSON&p_id={pwsid}` | 12 complete quarters + current ("In Progress"); per-quarter status text |

Envirofacts is keyless; very large result sets should be paged with `/ROWS/{start}:{end}`. SDWIS is refreshed
quarterly and lags ~1–2 quarters. No boundary hit → `primary: null` (possible private well) and the water score is
null (overall re-weighted).

## 5. Transportation noise

| Source | Endpoint | Notes |
|---|---|---|
| US DOT/BTS National Transportation Noise Map 2022 | `https://tiles.arcgis.com/tiles/xOi1kZaI0eWDREZv/arcgis/rest/services/NTAD_Noise_2022_{CONUS\|Alaska\|Hawaii}_{aviation_rail_road\|road\|aviation\|rail}/MapServer/tile/{z}/{row}/{col}` (Hawaii: `aviation_road`, no rail) | PNG8 cached tiles, **TilesOnly** (no identify/getSamples). Deepest cached level z=12 (~30 m px at 37°N ≈ the 30 m model grid); deeper levels 404/422. 404 tile = no area ≥ 45 dB. Legend (verified by decoding the swatches): #FFC107 45–50, #FF8000 50–55, #FF0000 55–60, #FF3399 60–70, #A300CC 70–80, #5200CC 80–90, #0000FF ≥ 90 dB LAeq,24h. 2016/2018/2020 vintages use the same naming |

Decoding uses a small built-in PNG decoder (`flate2` zlib inflate) in `src/modules/environment.rs`; when no tile can
be sampled the module falls back to an OpenStreetMap road/rail/aviation corridor model (with a warning). Esri Living Atlas
`USA_Transportation_Noise___Rail_Road_and_Aviation_2020/ImageServer` was also checked: TilesOnly, `identify` → 400.

## Scoring inputs (summary)

contamination ← sites (§1) · air ← CalEnviroScreen PM2.5/ozone/diesel percentiles (CA) or EJScreen mirror (elsewhere)
+ Open-Meteo 30-day AQI · water ← SDWIS/ECHO/LCR (+ CES drinking-water percentile in CA / EJScreen DWATER elsewhere)
· noise ← BTS combined layer. See SKILL.md for the rubric.

## Rate limits & etiquette

None of the services publish hard limits except Open-Meteo (non-commercial fair use) and AirNow (key). ArcGIS Online
hosted services throttle abusive clients — site, screening, water and air requests go through the shared
`HttpClient` cache (7 days for sites, screening and water; 1–6 hours for air quality); noise tiles are fetched
directly. One environment run issues ~25–35 requests (10 site layers, 2 screening, 1 air, up to 6 water, 4–16 noise
tiles).
