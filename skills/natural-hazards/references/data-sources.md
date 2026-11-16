# natural-hazards — data sources

Every endpoint used by `run_hazards` (`src/modules/hazards.rs`), all **keyless** and verified live across US regions
including California, Texas, and Florida. All ArcGIS queries are sent as
`f=json` GETs through `HttpClient::arcgis_query` in `src/shared.rs` (9.5 s timeout in this module, cached by URL for
7 days). Geometry is requested in WGS84 (`outSR=4326`), generalized server-side with `maxAllowableOffset` and
`geometryPrecision`, then thinned to a per-part vertex budget and rounded to 5 decimals before it goes into
`data.mapLayers`.

Legend: **Nat** = national (CONUS, usually AK/HI/PR too), **CA** = California-only (queried only when `geo.state`
is CA, `geo.stateFips` is 06 or `geo.countyFips` starts with 06).

## National

### 1. FEMA National Flood Hazard Layer (NFHL) — flood zone, nearby SFHAs, FIRM panel — Nat
- Zones: `https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28/query`
  - point: `geometry={lon},{lat}&geometryType=esriGeometryPoint&inSR=4326&spatialRel=esriSpatialRelIntersects&outFields=FLD_ZONE,ZONE_SUBTY,SFHA_TF,STATIC_BFE,DEPTH,DFIRM_ID&returnGeometry=false`
  - nearby SFHA (2 km): same + `where=SFHA_TF='T'&distance=2000&units=esriSRUnit_Meter&returnGeometry=true&outSR=4326&maxAllowableOffset=0.00003&orderByFields=OBJECTID`
- FIRM panel: `.../NFHL/MapServer/3/query` point, `outFields=FIRM_PAN,EFF_DATE,PANEL_TYP` (EFF_DATE = epoch ms).
- Key fields: `FLD_ZONE` (A, AE, AH, AO, AR, A99, V, VE, X, D, OPEN WATER, AREA NOT INCLUDED), `ZONE_SUBTY`
  (e.g. `0.2 PCT ANNUAL CHANCE FLOOD HAZARD`, `AREA OF MINIMAL FLOOD HAZARD`, `FLOODWAY`, `AREA WITH REDUCED FLOOD RISK DUE TO LEVEE`),
  `SFHA_TF` (T/F), `STATIC_BFE` / `DEPTH` (-9999 = none → `null`).
- License: US federal public domain. Attribution "FEMA National Flood Hazard Layer".
- Limits: maxRecordCount 2000; no published rate limit; responses 0.2–1 s.
- Failure modes: occasional 5xx / maintenance windows; areas without a digital FIRM return no polygon (→ `zone: null` + warning);
  Zone D means *not studied*, not low risk; large polygons — always use `maxAllowableOffset`.
- Test point: **Zone D** (SFHA F), panel 06085C0244H effective 2009-05-18; nearest SFHA = Zone A 448 m N, Zone AO 676 m.

### 2. FEMA National Risk Index (NRI) — census tracts — Nat
- `https://services.arcgis.com/XG15cJAlne2vxtgt/arcgis/rest/services/National_Risk_Index_Census_Tracts/FeatureServer/0/query`
  point query (fallback `where=TRACTFIPS='{11-digit tract}'`).
- Fields used: `TRACTFIPS, NRI_VER, POPULATION, BUILDVALUE, RISK_SCORE, RISK_RATNG, RISK_SPCTL, EAL_SCORE, EAL_RATNG, EAL_VALT,
  SOVI_SCORE, SOVI_RATNG, RESL_SCORE, RESL_RATNG` and per hazard `<H>_RISKS` (score 0–100, national percentile),
  `<H>_RISKR` (rating), `<H>_EALT` (expected annual loss, $) for H in ERQK, IFLD, CFLD, WFIR, HRCN, TRND, SWND, HAIL, HWAV, CWAV,
  WNTW, ISTM, DRGT, LTNG, LNDS, TSUN, VLCN, AVLN.
- Ratings: Very High / Relatively High / Relatively Moderate / Relatively Low / Very Low / No Rating / Not Applicable / Insufficient Data.
- License: public domain (FEMA). Version observed: **December 2025** (2020 census tracts).
- Failure modes: schema changes between releases (field names were stable 2023→2025); hazards that do not apply return `null`.
- Test tract 06085502907: **Relatively Moderate (71.8)**; EAL $3.09 M/yr (Relatively High); Earthquake 91.9 Relatively High,
  Riverine Flooding 73.3 Relatively Moderate, Heat Wave 45.9 Relatively Low, Landslide 32.1 Very Low; Wildfire No Rating.

### 3. USGS Seismic Design Maps — ASCE 7-22 — Nat (incl. AK, HI, PR, territories)
- `https://earthquake.usgs.gov/ws/designmaps/asce7-22.json?latitude={lat}&longitude={lon}&riskCategory=II&siteClass=Default&title=x`
  (302 → `https://earthquake.usgs.gov/ws/building-codes/asce7-22/calculate?...`; `HttpClient` follows up to 5 redirects).
- Fields (`response.data`): `pgam` (site-modified MCE_G PGA, g), `ss`, `s1`, `sms`, `sm1`, `sds`, `sd1`, `sdc` (Seismic Design Category),
  `response.metadata.vs30`, `modelVersion` (NSHM 2023 v5).
- Site class "Default" = envelope of C, CD, D (what a lender/building official uses when soil is unknown).
- License: public domain. Not a substitute for a site-specific geotechnical study.
- Test point: **PGA_M 0.76 g**, Ss 2.01, S1 0.71, SDS 1.42, SD1 1.14, SDC **D**.

### 4. USGS ANSS ComCat (FDSN event service) — Nat/global
- Count: `https://earthquake.usgs.gov/fdsnws/event/1/count?format=geojson&latitude=..&longitude=..&maxradiuskm=50&minmagnitude=4&starttime=1900-01-01&eventtype=earthquake`
- Largest: `.../fdsnws/event/1/query?format=geojson&...&maxradiuskm=100&minmagnitude=4&orderby=magnitude&limit=10`
- Chart list: `...&maxradiuskm=50&minmagnitude=3.5&orderby=magnitude&limit=1000` (then re-sorted by time).
- **Must pass `starttime`** (default window is the last 30 days). `eventtype=earthquake` drops quarry blasts/explosions.
- Limits: 20,000 events per query; be polite (we make 3 small calls). Pre-1932 magnitudes/locations are estimates; old events may lack depth.
- License: public domain.
- Test point: **160 M4+ within 50 km since 1900**; 373 M3.5+; largest within 100 km M7.9 1906 San Francisco (79.6 km),
  M6.9 1989 Loma Prieta (24.6 km), M6.45 1911 Morgan Hill (28.1 km), M6.2 1984 Morgan Hill (20.1 km), M6.2 1903 (9.8 km).

### 5. USGS Quaternary Fault and Fold Database (national, 2020 release) — Nat (western US mostly)
- `https://earthquake.usgs.gov/arcgis/rest/services/haz/Qfaults/MapServer/21/query` (layer 21 "National Database")
  radius 30 km, `outFields=OBJECTID,fault_name,section_name,fault_id,section_id,age,slip_rate,slip_sense,class`,
  `returnGeometry=true&outSR=4326&maxAllowableOffset=0.0003&geometryPrecision=5`, paginated via `resultOffset`.
- Fields: `age` (historic < 150 yr; latest Quaternary < 15 ka; late Quaternary < 130 ka; middle and late Quaternary < 750 ka;
  undifferentiated Quaternary < 1.6 Ma), `slip_rate` (categorical: "Greater than 5.0 mm/yr", "Between 1.0 and 5.0 mm/yr", ...),
  `slip_sense`, `class` (A = definite Quaternary deformation, B = less definitive). Report URL pattern (verified from the
  `fault_url` field): `https://earthquake.usgs.gov/cfusion/qfault/show_report_AB_archive.cfm?fault_id={id}&section_id={sid}`.
- Alternate (same data, AGOL): `https://services.arcgis.com/v01gqwM5QqNysAAi/arcgis/rest/services/USGS_Quaternary_Fault_and_Fold_Database/FeatureServer/5`.
- Limits: maxRecordCount 2000 (Bay Area 30 km = 1,383 segments). License: public domain.
- Map layer: segments grouped per fault/section; the 24 nearest groups are drawn (up to 4 geometries each, ≤ 60
  vertices per part).
- Failure modes: dense regions exceed one page; "Unnamed fault" rows have `fault_id=0` (no report URL); slip rate often "Unspecified".
- Test point nearest: Cascade fault 2.3 km SW, San Jose fault 2.8 km NE, Monte Vista-Shannon fault zone 3.5 km SW (late Quaternary,
  0.2–1 mm/yr), Coyote Creek 4.0 km NE, Stanford 4.7 km N, Berrocal 4.7 km SW. Nearest recently active: San Andreas fault zone
  (Santa Cruz Mountains section, historic, > 5 mm/yr) 12.2 km SW.

### 6. USFS Wildfire Hazard Potential 2023 (classified, 270 m) — CONUS
- `https://imagery.geoplatform.gov/iipp/rest/services/Fire_Aviation/USFS_EDW_RMRS_WildfireHazardPotentialClassified/ImageServer/identify?geometry={"x":lon,"y":lat,"spatialReference":{"wkid":4326}}&geometryType=esriGeometryPoint&returnGeometry=false&returnCatalogItems=false&f=json`
- Value classes: 1 Very Low, 2 Low, 3 Moderate, 4 High, 5 Very High, 6 Non-burnable, 7 Water.
- The old `apps.fs.usda.gov/arcx|fsgisx01/rest/services/RDW_Wildfire/...` endpoints now return HTTP 403 "migrated to IIPP".
- The continuous Wildfire Risk to Communities rasters on the same server (RiskToPotentialStructures, BurnProbability, ...) carry
  a note that pixel values were altered for web display — **not used for numbers**.
- License: public domain (USDA Forest Service, RMRS; Dillon 2023, RDS-2015-0047-4). Test point: **6 = Non-burnable**.

### 7. Geologic Radon Potential Map of the US (USGS OFR 93-292, 2025 digitization by EPA) — CONUS
- `https://services.arcgis.com/cJ9YHowT8TU7DUyn/arcgis/rest/services/Geologic_Radon_Potential_Map_of_the_United_States/FeatureServer/0/query` point.
- Fields: `GRP` (1 High: RI 12–16; 2 Moderate: RI 9–11; 3 Low: RI 5–8 — verified with outStatistics), `RI`, `AREANAME`, `STATE`.
- License: "No restrictions" (item credits: USGS, NIH/NIEHS). Test point: **GRP 2 Moderate** (Southern Coast Ranges, RI 9).

### 8. EPA Map of Radon Zones (1993) — county table — Nat (static, `RADON_TABLE` in `src/modules/hazards.rs`)
- No EPA API exists. The table (3,142 counties, 14 KB) is compiled into the binary. It was generated from a public
  ArcGIS digitization: `https://services7.arcgis.com/J9jeXlOE0ttzzuBr/arcgis/rest/services/EPA_Radon_Data/FeatureServer/0`
  (fields `STATEFP, COUNTYFP, RadonZone`); the generator and verifier scripts belonged to the earlier Node toolkit
  and are not in this repository.
- Verification: the official EPA state PDFs
  (`https://www.epa.gov/sites/default/files/2014-08/documents/<state>.pdf`; fills: red = Zone 1, orange = 2, yellow = 3) were parsed and
  county labels mapped to polygons. **California: 58/58 counties match.** TX 254/254, WA 39/39, IA 98/99, NY 61/62, OH 87/88, PA 65/67,
  CO 63/64 matched (the rest were labels the parser could not place); FL had 2 parser ambiguities (Hillsborough, Sarasota),
  where manual geometry inspection supports the table.
- Gaps → `null` + warning: counties created after 1993 (Broomfield CO 08014), Connecticut's 2022 planning regions (091xx), territories.
- License: public domain (EPA). Test county 06085 Santa Clara: **Zone 2**.

### 9. USGS 3DEP Elevation Point Query Service — Nat
- `https://epqs.nationalmap.gov/v1/json?x={lon}&y={lat}&units=Meters&wkid=4326&includeDate=false` → `value` (string, meters).
- Slow (5–15 s observed); returns -1000000 for no data (→ `null`). Public domain. Test point: **59.6 m (196 ft)**.

## California add-ons

### 10. CGS Earthquake Zones of Required Investigation (EQ Zapp layers) — CA
All on the California Department of Conservation AGOL org (`gis_cadoc`), `.../services2.arcgis.com/zr3KAIbsRSUyARHG/arcgis/rest/services/`:
- Alquist-Priolo Earthquake Fault Zones: `CGS_Alquist_Priolo_Fault_Zones/FeatureServer/0` (fields `QUAD_NAME, ZN_RELEASED, ZN_REVISED`) — point + 5 km radius.
- Liquefaction Zones: `CGS_Liquefaction_Zones/FeatureServer/0` (`QUAD_NAME, RELEASED, REVISED`) — point + 2 km radius.
- Earthquake-Induced Landslide Zones: `CGS_Landslide_Zones/FeatureServer/0` — point + 2 km radius.
- Unevaluated areas: `CGS_SHZ_Unevaluated_Areas/FeatureServer/0` — point.
- Quadrangle index: `IW_Regulatory/FeatureServer/0` (`CELL_NAME, ZoneType` = `all` | `ap` | `ll`, `PopupInfo` HTML with links to
  the EZRIM PDF map, GIS data and Seismic Hazard Zone Report). Tells whether "not in zone" means *evaluated* or *not mapped*.
- The legacy MapServer folder `gis.conservation.ca.gov/server/rest/services/CGS_Earthquake_Hazard_Zones` now only contains
  `SHP_Fault_Traces`. License: State of California public data (CGS disclaimers apply); attribute "California Geological Survey".
- Failure modes: liquefaction/landslide zones exist only in evaluated quadrangles (mostly urban Bay Area / SoCal);
  zone polygons are per-quad multipolygons (≈190 KB raw per 2 km query → we generalize to 0.0001°).
- Test point: San Jose West quad (`ll`, seismic hazard zones mapped; no AP zones in quad) — **not in AP, liquefaction or landslide
  zone**; nearest liquefaction zone 560 m; no landslide zone within 2 km; no AP zone within 5 km.

### 11. CGS Map Sheet 58 — deep-seated landslide susceptibility — CA
- `https://gis.conservation.ca.gov/server/rest/services/CGS/MS58_LandslideSusceptibility_Classes/MapServer/identify?geometry={lon},{lat}&geometryType=esriGeometryPoint&sr=4326&tolerance=0&layers=all&mapExtent=..&imageDisplay=400,400,96&returnGeometry=false&f=json`
- `Raster.Value` 0–10 (low → high). Our labels: 0–4 low, 5–6 moderate, 7–8 high, 9–10 very high. Test point: **0**.

### 12. CGS Indoor Radon Potential Zones — CA (study areas only)
- `https://gis.conservation.ca.gov/server/rest/services/CGS/RadonPotentialZones/MapServer/0/query` point,
  `outFields=Rn_ZONE_POTENTIAL,DESCRIPTION_RN_ZONE,CGS_REPORT,REPORT_NAME,LINK_REPORT,STUDY_AREA` (note the upper-case `RN`).
- Test point: **Moderate** — "5 to 19.9 percent of home indoor-radon measurements are likely to exceed 4 pCi/L" (CGS SR 244).

### 13. CGS Tsunami Hazard Areas — CA
- `https://gis.conservation.ca.gov/server/rest/services/CGS/IW_Tsunami_Hazard_Area/FeatureServer/0/query` point; `Evacuate`
  = "Yes, Tsunami Hazard Area" | "No, Outside Hazard Area", `Map_Link`. Test point: **No**.

### 14. CAL FIRE Fire Hazard Severity Zones — CA
- SRA (adopted, effective 2024-04-01): `https://services1.arcgis.com/jUJYIo9tSA7EHvfZ/arcgis/rest/services/FHSZSRA_23_3/FeatureServer/0`
- LRA (maps dated 2025-03-24, all rollout phases): `https://services1.arcgis.com/jUJYIo9tSA7EHvfZ/arcgis/rest/services/FHSALRA25_v1_All/FeatureServer/0`
  - Fields `SRA` (SRA/LRA), `FHSZ` (1 Moderate, 2 High, 3 Very High, -3 NonWildland in LRA), `FHSZ_Description`.
  - Point query + 5 km radius with `where=FHSZ >= 2` (never request the statewide LRA "NonWildland" polygon geometry —
    it is 14,700 km² / 4,900 km of perimeter).
- Responsibility area: `https://services1.arcgis.com/jUJYIo9tSA7EHvfZ/arcgis/rest/services/SRA25_2/FeatureServer/0` (`SRA` = SRA/LRA/FRA, July 2025).
- Token-protected (not usable): `.../FHSZ_SRA_LRA_Combined/FeatureServer`, `https://egis.fire.ca.gov/arcgis/rest/services/FRAP/FHSZ`.
  Not used: `FHSZSRA25_3` (revised SRA areas still in the regulatory process).
- LRA maps are CAL FIRE recommendations that each city/county adopts by ordinance; adoption dates vary.

### 15. Cal OES MyHazards hazard areas — CA
- Base: `https://services.arcgis.com/BLN4oKB0N1YSgvY8/arcgis/rest/services/MyHazards_Hazard_Areas/FeatureServer`
  - `/7` Dam Failure Inundation Areas (DWR Division of Safety of Dams maps; `DamName, NID, HazardCl, LoadingScn`) — point + 1 km
    attribute-only query (**never request geometry**: 2–3 MB for 4 dams).
  - `/4` Sea Level Rise Hazard Areas (USGS CoSMoS / Point Blue OCOF; `hazard_description` "Sea Level Rise 3 Feet" / "6 Feet").
  - `/6` Levee Flood Protection Zones (DWR, State Plan of Flood Control levees).

## Evaluated and not used
| Source | Why |
|---|---|
| NOAA SLR Viewer `coast.noaa.gov/arcgis/rest/services/dc_slr/slr_{n}ft/MapServer` | Point query on layer 0 ("Low-lying Areas") returns the national dissolved polygon even 60 m above sea level; depth raster identify returns nothing. Unreliable for point lookups. |
| CAL FIRE `FHSZ_SRA_LRA_Combined`, `egis.fire.ca.gov` FRAP/FHSZ | Token required (HTTP 499). |
| USFS `apps.fs.usda.gov/arcx`, `fsgisx01` RDW_Wildfire | HTTP 403, migrated to `imagery.geoplatform.gov/iipp`. |
| USFS WRC continuous rasters (RPS, BP) | Web-service pixel values altered for display; only relative. |
| Nominatim / overpass-api.de | Blocked / 406 from this host (not needed here). |

## Rate limits & etiquette
No source above publishes a hard rate limit for anonymous use. One report = ~12 national + ~20 CA requests, all cached by
URL in the shared `HttpClient` (in memory and in the server's disk cache, `HTTP_CACHE_DIR`, default `.cache/http`), with a per-host concurrency
limit; identical concurrent requests (for example neighbouring homes in a batch scan) share one fetch.
