# Open Estate — Self-Hosted US Residential, CRE & Farmland Property Research

[![License: Personal Research & Non-Commercial](https://img.shields.io/badge/license-Personal%20Research%20%26%20Non--Commercial-0f766e.svg)](#license)

> **Personal Research & Non-Commercial Edition**: This open-source release (**`open-estate/open-estate`**) is provided **free of charge** for individuals to self-host via Docker for **personal, non-commercial property research only**. Operating hosted/cloud/SaaS/API services for third parties, charging fees, or using the software in commercial real-estate, brokerage, lending, or investment operations is **strictly prohibited** — all commercial licensing and managed cloud hosting rights are reserved exclusively by **OmniSolo LLC** (see [`LICENSE`](LICENSE)).

**Open Estate** turns any US residential, commercial, land, or agricultural street address into a self-contained, interactive due-diligence report, 3D LiDAR lot/roof model, Commercial Land / Farmland & Plaza Store Profit simulator, side-by-side Deal Room comparison matrix, and natural-language AI Home Finder — compiled into a **single native Rust binary (`open-estate-server`)** and packaged for **Docker & Docker Compose** with zero Node.js or Python server runtime dependencies.

All ~50 public federal, state, municipal, and open-data property layers (U.S. Census Geocoder & ACS, County Assessor & municipal GIS, USDA NRCS Soil Data Access & USDA NASS agricultural land/cash-rent benchmarks, FEMA NFHL & NRI, USGS National Map aerial orthoimagery, ASCE 7-22 seismic & 3DEP LiDAR, CAL FIRE / USFS wildfire, EPA Superfund/TRI/RCRA/SDWIS, OpenStreetMap Overpass/Nominatim/Photon footprints & plaza co-tenants, NCES/CAASPP/EDFacts schools, FBI CDE crime, FHFA HPI, HUD SAFMR/FMR, BLS CPI, Freddie Mac PMMS, NOAA WMM2025 geomagnetism) work **out of the box with zero API keys and zero portal scraping**. Adding an optional `OPENAI_API_KEY` unlocks grounded AI Analyst synthesis, interactive buyer Q&A, multi-PDF/`.zip` inspection/HOA/title packet review, walkthrough slide decks, and natural-language search.

---

## Quick Start (Docker Compose — Recommended)

Pre-built multi-arch images (`linux/amd64` + `linux/arm64`) are published at **`docker.io/openestate/openestate:latest`**.

```bash
# 1. Copy optional environment settings (works with zero keys; set OPENAI_API_KEY to enable AI features)
cp .env.example .env

# 2. Start Open Estate from the pre-built Docker Hub image (SQLite WAL persisted in `open-estate-data`)
docker compose up -d
# Or build locally from source instead:
# docker compose up -d --build

# 3. Open the web UI or verify health
open http://localhost:8080
curl -fsS "http://localhost:8080/api/health"
```

Or run the container standalone with Docker:

```bash
docker run -d --name open-estate \
  -p 8080:8080 \
  -v open-estate-data:/data \
  -v open-estate-reports:/app/reports \
  openestate/openestate:latest
```

### Optional Docker Compose Profiles

- **Self-Hosted OSRM Road & Commute Router (`--profile osrm`)**:
  Downloads and verifies a Geofabrik `.osm.pbf` extract (California by default; configurable via `OSRM_EXTRACT_URL` / `OSRM_EXTRACT_NAME`), builds a Multi-Level Dijkstra graph, and serves `osrm-routed` internally:
  ```bash
  OSRM_BASE_URL=http://osrm:5000 docker compose --profile osrm up -d
  ```
  See [`deploy/osrm/README.md`](deploy/osrm/README.md) for custom states, multi-region `OSRM_REGIONS_JSON`, and zero-downtime graph refreshes.

- **MySQL 8.4 Backend (`--profile mysql`)**:
  ```bash
  DATABASE_PROVIDER=mysql \
  MYSQL_URL=mysql://openestate:openestate@mysql:3306/openestate \
  docker compose --profile mysql up -d
  ```

---

## Quick Start (Build & Run Natively with Cargo)

System prerequisites (optional, for scanned-PDF local OCR): `poppler-utils` (`pdftoppm`) and `tesseract-ocr`.

```bash
cp .env.example .env

# Run unit and offline integration tests
cargo test --release -- --skip live_ --skip e2e_browser

# Start open-estate-server on http://0.0.0.0:8080
cargo run --release --bin open-estate-server -- \
  --host 0.0.0.0 \
  --port 8080 \
  --db-file .cache/open-estate.sqlite \
  --reports-dir reports \
  --data-dir .cache/datasets
```

### Database Migration CLI (`SQLite WAL <-> MySQL`)

```bash
open-estate-server migrate \
  --from sqlite://.cache/open-estate.sqlite \
  --to mysql://openestate:openestate@127.0.0.1:3306/openestate \
  --batch-size 200
```

---

## Architecture Highlights

- **100% Pure Rust Single Binary (`axum` + `tokio` + `reqwest` `rustls`)**:
  - Fans out ~50 public federal, state, municipal, and open-data queries concurrently after US Census + OpenStreetMap rooftop geocoding, rendering interactive HTML reports in seconds while optional AI synthesis hydrates asynchronously in the background.
  - Self-contained [`assets/template.html`](assets/template.html) and Fluent bundles (`locales/{en-US,zh-CN}/main.ftl`) are compiled directly into `open-estate-server`, supporting `EN` / `中文` / `ES` localization (`i18next` + `Intl`) and offline `.html` report saving.
  - **Strictly Chinese-Only 堪舆风水 (`#fengshui`)**: Traditional Xuan Kong Flying Stars (玄空飞星), Eight Mansions (八宅), Personal Kua (命卦), Four Pillars (四柱五行), and 360° DEM Form School checks render exclusively in Simplified Chinese (`zh-CN`).
- **Portable Persistence (`sqlx` + `sea-query` — SQLite WAL & MySQL)**:
  - Embedded **SQLite WAL** by default for zero-ops single-container deployments, with seamless scaling to **MySQL 8.4/9.0** (`MYSQL_URL`) and L1 `moka` in-memory TTL caches.
- **3D LiDAR Massing & Sun/Shadow Study (`src/modules/house3d.rs`)**:
  - Streams public-domain USGS 3DEP airborne LiDAR `.laz` point clouds (`las`), fits roof planes, terrain DEMs, and individual tree canopies over OpenStreetMap footprints, drapes public-domain USGS National Map aerial orthoimagery, and renders an interactive Three.js clay model alongside solar/shadow simulation.
- **Commercial Land, Farmland & Plaza Store Profit Simulator (`src/cre/{land_farm,business_profit}.rs`)**:
  - **Commercial Land (`commercial_land`)**: Zoning-derived buildable envelope (`Max GFA`, `Allowable FAR`, highest-and-best-use), Residual Land Value (RLV) waterfall, and NNN pad / QSR drive-thru ground-lease capitalization.
  - **Agricultural / Farmland (`agricultural_farm`)**: Keyless USDA NRCS Soil Data Access (`mukey`, Prime Farmland status, Capability Class I–VIII, NCCPI productivity, slope/drainage) + 50-state USDA NASS land/cash-rent benchmarks, 4-mode farm operating profit (Row Crops, Irrigated Specialty, Orchard/Vineyard, Pasture/Ranch), state ag tax exemptions, and USDA FSA / Farm Credit financing.
  - **Store Profit & Plaza Co-Tenancy Simulator (`POST /api/cre/business-eval`)**: Evaluates 8 store concepts (Full-Service Restaurant, Café/QSR, Clothing & Apparel Store, Sporting Goods Store, Specialty Grocery, Fitness Studio, Wellness/Salon, General Retail) at any parcel or retail plaza — explicitly weighing OpenStreetMap anchor & complementary co-tenant foot-traffic boosts (`<= 600 ft`) against same-plaza and ½-mile direct competitor cannibalization to project gross sales, full unit P&L, EBITDA, startup CapEx, payback period, and Year-1 Cash-on-Cash ROI.
- **Hybrid 3-Tier PDF, Scanned OCR & `.zip` Disclosure Packet Review (`src/ai/`)**:
  - Native digital text extraction (`lopdf` / `pdf-extract`), on-demand local `pdftoppm` + `tesseract` OCR (`OMP_THREAD_LIMIT=1`), and multimodal Vision OCR fallback — with zip-bomb defenses, deterministic `redact_title_pii` scrubbing before LLM calls, cross-document Contradiction Matrix, and Repair-Credit Addendum generation.
- **Two-Lane Priority 10,000-Home Scanner (`src/scanner/`)**:
  - Dedicated **P0 Interactive Lane** isolated from the **P1 Bulk Worker Pool** so single-address requests never queue behind batch ZIP scans.
- **Direct-First Outbound Networking (`src/scrape.rs`, `src/proxy_pool/`)**:
  - Uses direct HTTPS with L1/disk singleflight caching by default (`FREE_PROXY_POOL_ENABLED=false`) and zero public free-proxy harvesting; supports optional operator-supplied `OUTBOUND_PROXY_URLS`. API-keyed endpoints (`OPENAI_BASE_URL`, MLS APIs, SMTP, OSRM, loopback) always bypass proxies.
- **Purpose-Locked AI & Fair Housing Guardrails (`src/guardrails.rs`)**:
  - Blocks direct LLM proxy routes (`403 DIRECT_LLM_BLOCKED`), enforces purpose-locked structured JSON schemas, strips internal model/provider metadata from rendered HTML, and enforces strict Fair Housing Act demographic invariance across scoring, search, and AI commentary.

---

## Domain Modules (`src/` & `skills/*/SKILL.md`)

| Domain | Rust Module | Capabilities |
|---|---|---|
| **Geocoding & Autocomplete** | `src/property/{geocode,suggest}.rs` | US Census Geocoder (tract/block/FIPS/CBSA/school districts), OSM/Nominatim/Photon rooftop & autocomplete, optional BYOK ArcGIS/Google Places |
| **Facts, MLS & Public Records** | `src/property/{facts,house_profile,mls,public_records}.rs` | County Assessor & municipal GIS open data, parcel/building register (BCR, FAR, permits), RESO architectural styles, user CSV ingestion, optional BYOK RESO/SimplyRETS/ATTOM connectors |
| **Market Trends & Valuation** | `src/property/{zillow,market,valuation,commercial_data}.rs` | US public-domain FHFA HPI, Census ACS, HUD SAFMR/FMR, BLS CPI, Freddie Mac PMMS mortgage rates (`PUBLIC_DATA_MODE=strict`), repeat-sales + adjusted comps blend, P10/P50/P90 scenarios, CA Prop 13 & loan math |
| **Your Match Scoring & Search** | `src/scoring/*`, `src/search/*` | Deterministic 11-dimension buyer utility engine, 60s Preference Studio (`/match`), natural-language search (`sea-query`), licensed feed sync, and `lettre` SMTP alerts |
| **HOA Profile & Buyer Diligence** | `src/modules/{buyer_diligence,move_in_cost,remodel_rebuild}.rs` | Always-on HOA benchmark & rules check, cash-to-close transfer taxes, special districts (Mello-Roos/MUD/CDD), FCC broadband, point-of-sale mandates, break-even horizon, and 33-trade renovation cost catalog |
| **Insurance, Permits & CapEx** | `src/modules/{insurance,insurance_quotes}.rs`, `src/property/permits_condition.rs` | Coverage A rebuild cost, state FAIR Plan / coastal wind / flood / earthquake estimates, optional BYOK Shovels & Zoneomics connectors, and 6-system CapEx lifecycle |
| **Natural Hazards & Environment** | `src/modules/{hazards,environment}.rs` | FEMA NFHL & NRI, USGS ASCE 7-22 seismic & Quaternary faults, CAL FIRE / USFS wildfire, EPA radon, Superfund NPL, TRI, RCRA, Brownfields, LUST, GeoTracker/EnviroStor, Open-Meteo PM2.5/AQI, SDWIS water, BTS noise |
| **Livability, Schools & Commute** | `src/modules/{amenities,access,civic,school_metrics,commute}.rs` | OpenStreetMap POIs & nuisances, self-hosted OSRM commute matrix, EPA Walkability, NCES + CAASPP/EDFacts state-isolated school metrics, FBI CDE + municipal Socrata crime context |
| **3D Lot Model & Media Studio** | `src/modules/house3d.rs`, `src/property/media.rs`, `src/ai/{slides,artifact_canvas}.rs` | USGS 3DEP LiDAR 3D roof/terrain/tree scene, USGS National Map aerial orthoimagery, Wikimedia Commons CC media, sun/shadow study, walkthrough slide deck, artifact canvases, and OS-theme-adaptive video tour |
| **Commercial, Land, Farmland & Store Profit (`CRE`)** | `src/cre/{mod,underwriting,trade_area,land_farm,business_profit,documents}.rs` | $1M–$25M CRE underwriting (NOI, NNN/MG/FSG, cap-rate matrix, DSCR debt sizing, SBA 504/7a), **Commercial Land** Residual Land Value (RLV) waterfall & NNN pad/drive-thru ground-lease valuation, **Agricultural / Farmland** valuation (USDA NRCS Soil Data Access Prime Farmland / Capability Class I–VIII / NCCPI + 50-state USDA NASS $/acre & cash-rent benchmarks, 4-mode farm operating profit, ag tax exemptions, USDA FSA & Farm Credit financing), **8-Concept Store Profit & Plaza Co-Tenancy Simulator** (`POST /api/cre/business-eval` — Restaurant, Café/QSR, Clothing Store, Sporting Goods Store, Specialty Grocery, Fitness Studio, Wellness/Salon, General Retail factoring in OSM plaza anchor boost vs. direct competitor cannibalization), and Phase I ESA / ALTA / lease abstract review |
| **Classical 风水 (Chinese-Only)** | `src/modules/{fengshui,xuankong}.rs` | NOAA WMM2025 magnetic & true facing, 24 mountains (二十四山), Xuan Kong Flying Stars (玄空飞星), Eight Mansions (八宅), Personal Kua & Four Pillars |
| **Browser Extension & Deal Room** | `extension/`, `src/web/deal_room.rs` | ToS-clean Manifest V3 Companion (`bash scripts/package_extension.sh`) for quick US address lookup, right-click selected-address research, and side-by-side `/deal-room` comparison |

---

## Upstream Data & Service License Compliance

Every upstream service queried by default is either **U.S. Federal / State Public Domain**, **ODbL / Creative Commons Open Data**, or **explicitly gated behind a user-supplied Bring-Your-Own-Key (BYOK) official API credential** — with zero third-party listing portal scraping or DOM injection:

| Data Category | Default Upstream Sources | License / Terms |
|---|---|---|
| **Geocoding, Demographics & Economics** | U.S. Census Geocoder, Census ACS 5-Year, BLS CPI | U.S. Federal Public Domain (17 U.S.C. § 105) |
| **Home Price Indexes, Rents & Rates** | FHFA HPI, HUD SAFMR/FMR, Freddie Mac PMMS (`PUBLIC_DATA_MODE=strict`) | U.S. Federal Public Domain / Published Public Series |
| **Soils, Agronomy & Farmland Benchmarks** | USDA NRCS Soil Data Access (`SDMDataAccess.sc.egov.usda.gov`), USDA NASS State Land Values & Cash Rents | U.S. Federal Public Domain (17 U.S.C. § 105) |
| **Aerial Imagery, LiDAR & Seismic** | USGS National Map (`USGSImageryOnly`), USGS 3DEP LiDAR, USGS ASCE 7-22 & ComCat | U.S. Federal Public Domain (17 U.S.C. § 105) |
| **Flood, Wildfire, Environment & Noise** | FEMA NFHL & NRI, CAL FIRE / USFS WHP, EPA Envirofacts/SDWIS/Radon, DOT/BTS Noise, FCC BDC, NOAA WMM2025 | U.S. Federal / California Public Records |
| **Footprints, Roads, POIs & Autocomplete** | OpenStreetMap (`overpass-api.de`, `overpass.private.coffee`, `nominatim.openstreetmap.org`, `photon.komoot.io`), Geofabrik `.osm.pbf` | Open Database License (ODbL 1.0) with attribution |
| **Weather & Air-Quality Telemetry** | Open-Meteo Weather & Air Quality APIs (`open-meteo.com`) | CC BY 4.0 (**Non-Commercial only** on free tier — matches this repository's non-commercial license) |
| **Neighborhood Context Photos** | Wikimedia Commons GeoSearch | Creative Commons (per-asset CC BY / CC BY-SA / Public Domain attribution) |
| **Optional BYOK Commercial & Rate-Limit APIs** | OpenAI, Mapbox, LocationIQ, FreeRoute, OpenRouteService, ATTOM, RESO Web API 2.0, SimplyRETS, Shovels, Zoneomics, CourtListener, ArcGIS Platform, Google Maps (Places + Street View Embed), Mapillary, HUD User, api.data.gov, EPA AirNow, Socrata, SchoolDigger / GreatSchools | Disabled (or lower anonymous federal/municipal quotas) unless you supply your own credentials in `.env` — see [Optional API Keys & Registration Guide](#optional-api-keys--registration-guide-bring-your-own-key) |

---

## Optional API Keys & Registration Guide (Bring Your Own Key)

**Open Estate requires zero API keys to run.** Out of the box, ~50 U.S. federal, state, municipal, and OpenStreetMap layers (plus keyless IP geolocation via `ip_geolocation` and deterministic road/valuation models) work immediately with zero external accounts.

Every credential below is an **optional Bring-Your-Own-Key (BYOK) upgrade** configured in `.env` (for Docker Compose or local `cargo run`) or container environment variables. Open Estate:
- Never requires keys for baseline operation and never scrapes third-party listing portals.
- Strips API keys, bearer tokens, and proxy credentials from logs, `/api/ops/telemetry`, `/api/health`, `/api/admin/extensions`, and generated HTML reports.
- Automatically bypasses `OUTBOUND_PROXY_URLS` for API-keyed endpoints so licensed requests go directly over TLS.

> [!TIP]
> Copy `.env.example` to `.env` (already gitignored), uncomment only the keys you want, and restart `open-estate-server`. Live readiness of configured connectors appears on `GET /api/health` (`connectors` / `syncConnectors`), `GET /api/ops/telemetry` (`/ops`), and `GET /api/admin/extensions` (`/admin`) without exposing secret values.

### Quick Summary Table

| Category | Service | Env Variable(s) | Default Base URL | Free Tier / Licensing | Unlocks |
|---|---|---|---|---|---|
| **1. AI & OCR** | **OpenAI Responses API** *(or compatible)* | `OPENAI_API_KEY` | `https://api.openai.com/v1` | Pay-as-you-go API billing *(separate from ChatGPT Plus)* | AI Analyst, buyer Q&A, PDF/HOA/title packet review, Vision OCR, walkthrough slides & NL search |
| **2. Routing Fallback** | **Mapbox Directions Matrix** | `MAPBOX_ACCESS_TOKEN` *(alias: `MAPBOX_API_KEY`)* | `https://api.mapbox.com` | Free monthly tier | Tier-2 traffic-aware driving matrix fallback (`mapbox_routing`) |
| **2. Routing Fallback** | **LocationIQ Matrix** | `LOCATIONIQ_API_KEY` *(alias: `LOCATIONIQ_ACCESS_TOKEN`)* | `https://us1.locationiq.com` | Free tier (`5,000 req/day`, 2 RPS, no card) | Tier-2 OSM driving matrix fallback (`locationiq_routing`) |
| **2. Routing Fallback** | **FreeRoute Matrix** | `FREEROUTE_API_KEY` | `https://api.maps.freeroute.org` | Free tier (`~5,000 req/day`, no card) | Tier-2 OSM driving matrix fallback (`freeroute_routing`) |
| **2. Routing Fallback** | **OpenRouteService (HeiGIT)** | `OPENROUTESERVICE_API_KEY` *(alias: `ORS_API_KEY`)* | `https://api.openrouteservice.org` | Free non-commercial Standard plan | Tier-2 OSM driving matrix fallback (`openrouteservice_routing`) |
| **3. Facts, AVM & MLS** | **ATTOM Data** | `ATTOM_API_KEY` | `https://api.gateway.attomdata.com` | 30-day free developer trial (`~1,000 hits`) | Expanded profile, ATTOM AVM, sale/mortgage recorder check & ZIP sold-comp sync |
| **3. Facts, AVM & MLS** | **RESO Web API 2.0 (IDX)** | `RESO_API_URL` + `RESO_BEARER_TOKEN` | *(operator-supplied)* | Licensed MLS / IDX credentials required | Live OData MLS active/pending/closed listings, media & disclosure PDFs |
| **3. Facts, AVM & MLS** | **SimplyRETS** | `SIMPLYRETS_AUTH` (`user:pass`) | `https://api.simplyrets.com` | Free test sandbox (`simplyrets:simplyrets`); paid for live MLS | Live MLS listings & ZIP sync (or sandbox test listings) |
| **4. Permits, Zoning & Legal** | **Shovels** | `SHOVELS_API_KEY` | `https://api.shovels.ai/v2` | Free non-expiring developer plan (no card) | 50-state building permit history, valuations, work scopes & licensed contractors |
| **4. Permits, Zoning & Legal** | **Zoneomics** | `ZONEOMICS_API_KEY` | `https://api.zoneomics.com/v2/zoneDetail` | Browser sandbox + trial/paid plans | Parcel zoning code, permitted uses, FAR, height, lot coverage, setbacks & ADU/STR context |
| **4. Permits, Zoning & Legal** | **CourtListener (Free Law Project)** | `COURTLISTENER_API_TOKEN` | `https://www.courtlistener.com/api/rest/v4/search/` | 100% free account token *(works keyless at lower limits)* | Higher rate limits on RECAP federal civil & bankruptcy docket screening in Title & Legal Radar |
| **4. Permits, Zoning & Legal** | **Custom Inspection / Disclosure API** | `INSPECTION_API_URL` + `INSPECTION_API_KEY` | *(operator-supplied)* | Self-hosted / partner endpoint | Auto-fetch pre-listing inspection, WDO/termite, sewer scope & disclosure PDFs |
| **5. Geocoding & Imagery** | **Esri ArcGIS Location Platform** | `ARCGIS_API_KEY` | `https://geocode.arcgis.com/arcgis/rest/services/World/GeocodeServer/findAddressCandidates` | Free monthly developer tier | Fallback street/subaddress geocoder, autocomplete & POI/airport directory fallback |
| **5. Geocoding & Imagery** | **Google Maps Places (New)** | `GOOGLE_MAPS_API_KEY` | `https://places.googleapis.com` | Free monthly SKU quota *(GCP billing required)* | Primary US street-address typeahead autocomplete |
| **5. Geocoding & Imagery** | **Google Maps Embed (Street View)** | `GOOGLE_MAPS_EMBED_KEY` | `https://www.google.com/maps/embed/v1/streetview` | Free Street View embed quota *(GCP billing required)* | Interactive in-page Street View panorama tab on property reports |
| **5. Geocoding & Imagery** | **Mapillary** | `MAPILLARY_ACCESS_TOKEN` | `https://graph.mapillary.com` | 100% free developer Client Token | Open street-level frontage photography (~95 m radius) in Media Studio |
| **5. Geocoding & Imagery** | **IP Geolocation (`ip_geolocation`)** | *(none — keyless)* | `ipwho.is` / `freeipapi.com` / `ipapi.co` | 100% free & keyless (`IP_GEOLOCATION_ENABLED=true`) | Edge-header + multi-provider IP-to-ZIP lookup verified with Census TIGER ZCTA5 (no IPinfo key needed) |
| **6. Civic, Env & Schools** | **HUD User API** | `HUD_API_TOKEN` | `https://www.huduser.gov/hudapi/public/fmr` | 100% free account token | Live ZIP Small Area Fair Market Rent (SAFMR) & county FMR queries |
| **6. Civic, Env & Schools** | **api.data.gov / FBI CDE** | `DATA_GOV_API_KEY` *(plus `CDE_API_KEY` for batch light scans)* | `https://api.usa.gov/crime/fbi/cde` | 100% free instant key (defaults to `DEMO_KEY`) | Increases `api.usa.gov` FBI Crime Data Explorer fallback quota from `30 req/hr` to `1,000 req/hr` |
| **6. Civic, Env & Schools** | **US EPA AirNow** | `AIRNOW_API_KEY` | `https://www.airnowapi.org/aq/observation/latLong/current/` | 100% free developer API key | Live EPA AirNow station AQI observations alongside always-on Open-Meteo 30-day series |
| **6. Civic, Env & Schools** | **Socrata Open Data** | `SOCRATA_APP_TOKEN` | City SODA portals (`data.sfgov.org`, `data.cityofnewyork.us`, etc.) | 100% free portal App Token *(works keyless at lower limits)* | Higher rate-limit quota on municipal Socrata crime/incident open-data supplements |
| **6. Civic, Env & Schools** | **SchoolDigger & GreatSchools** | `SCHOOLDIGGER_APP_ID` + `SCHOOLDIGGER_APP_KEY`; `GREATSCHOOLS_API_KEY` | `api.schooldigger.com` / `gs-api.greatschools.org` | SchoolDigger free dev tier; GreatSchools commercial | Tracked in `GET /api/health` (`connectors`) *(report school scores strictly use NCES + state CAASPP/EDFacts datasets)* |
| **7. Property Alerts** | **Outbound SMTP Relay** | `SMTP_HOST`, `SMTP_PORT`, `SMTP_SECURE`, `SMTP_USER`, `SMTP_PASS`, `SMTP_FROM`, `SMTP_FROM_NAME`, `SMTP_REPLY_TO` | *(operator-supplied)* | Free tiers on Gmail App Passwords, Brevo, Resend, or self-hosted | Saved-search & price/status alert emails with RFC 8058 one-click unsubscribe |

---

### How Each Capability Domain Works & Step-by-Step Key Setup

#### 1. AI Analyst, Buyer Q&A, PDF Disclosure Review & Natural-Language Search
- **Variables**: `OPENAI_API_KEY`, optional `OPENAI_BASE_URL` (default `https://api.openai.com/v1`), plus `LLM_MODEL` (`gpt-4.1`), `LLM_EFFORT` (`none`), `LLM_MATCH_MODEL` (`gpt-4o-mini`), `LLM_FALLBACK_MODEL` (`gpt-4o-mini`), `LLM_QA_EFFORT` (`medium`), `SLIDES_LLM_MODEL`, `SLIDES_LLM_EFFORT`, `LLM_TIMEOUT_MS` (`110000`), and `OCR_MODE` (`auto` | `local_only` | `vision_only` | `disabled`).
- **How to get `OPENAI_API_KEY`**:
  1. Sign in at [`https://platform.openai.com/`](https://platform.openai.com/) (OpenAI API Platform billing is separate from ChatGPT Plus/Pro).
  2. Open **Settings → Billing** to add a payment method and prepaid API credit balance.
  3. Open [`https://platform.openai.com/api-keys`](https://platform.openai.com/api-keys), click **Create new secret key**, copy the `sk-...` key into `OPENAI_API_KEY=` in `.env`.
  4. *(Optional)* To point Open Estate at an OpenAI Responses-API-compatible gateway (`POST {base}/responses`), set `OPENAI_BASE_URL=` and `OPENAI_API_KEY=`.

#### 2. Cloud Matrix Routing Fallback Pool
Open Estate resolves driving-time & distance commute matrices in **three tiers** (`src/modules/commute.rs`, `src/modules/routing_providers.rs`):
1. **Tier 1 (Primary) — Self-Hosted OSRM**: Configured via `OSRM_REGIONS_JSON` and/or `OSRM_BASE_URL` (see [`deploy/osrm/README.md`](deploy/osrm/README.md)). Public `router.project-osrm.org` is never called.
2. **Tier 2 (Fallback Pool) — Cloud Matrix Load Balancer**: Invoked **only** when Tier-1 OSRM has no matching regional bounding box or all matching OSRM instances are unreachable or in cooldown. Chunks of up to **25 total coordinates** distribute across all configured & enabled cloud routing extensions (`mapbox_routing`, `locationiq_routing`, `freeroute_routing`, `openrouteservice_routing`) using `ROUTING_FALLBACK_STRATEGY=round_robin` (default) or `priority` (ordered by `ROUTING_FALLBACK_ORDER`). Base URLs (`MAPBOX_BASE_URL`, `LOCATIONIQ_BASE_URL`, `FREEROUTE_BASE_URL`, `OPENROUTESERVICE_BASE_URL`), `ROUTING_EXTERNAL_TIMEOUT_MS` (`4500` ms default), and `ROUTING_EXTERNAL_COOLDOWN_SECS` (`25` s default; `120` s on 401/403/429) can also be overridden.
3. **Tier 3 — Deterministic Estimates**: If neither OSRM nor any cloud fallback provider is configured/healthy, commute distances fall back to labeled deterministic road-distance estimates (`estimated`).

- **Mapbox (`MAPBOX_ACCESS_TOKEN`, alias `MAPBOX_API_KEY`)**:
  1. Sign up or log in at [`https://account.mapbox.com/`](https://account.mapbox.com/) (free monthly tier; keep an eye on Directions Matrix API usage limits).
  2. Open [`https://console.mapbox.com/account/access-tokens/`](https://console.mapbox.com/account/access-tokens/) and copy your default public token (`pk....`) or click **Create a token** with standard public/navigation scopes.
  3. Set `MAPBOX_ACCESS_TOKEN=pk....` in `.env`.
- **LocationIQ (`LOCATIONIQ_API_KEY`, alias `LOCATIONIQ_ACCESS_TOKEN`)**:
  1. Register for a free account (5,000 requests/day, 2 RPS, no credit card) at [`https://locationiq.com/`](https://locationiq.com/).
  2. Log in to the dashboard at [`https://my.locationiq.com/`](https://my.locationiq.com/) and open **API Access Tokens**.
  3. Copy your API token and set `LOCATIONIQ_API_KEY=pk....` in `.env` (if your account uses EU servers, also set `LOCATIONIQ_BASE_URL=https://eu1.locationiq.com`).
- **FreeRoute (`FREEROUTE_API_KEY`)**:
  1. Visit [`https://maps.freeroute.org`](https://maps.freeroute.org) and click **Get API key** (register via email, GitHub, or Google — free tier ~5,000 req/day, no card required).
  2. Generate an API key in your FreeRoute dashboard (`fro_...`).
  3. Set `FREEROUTE_API_KEY=fro_...` in `.env` (sent as `Authorization: Bearer <token>` to `https://api.maps.freeroute.org`).
- **OpenRouteService by HeiGIT (`OPENROUTESERVICE_API_KEY`, alias `ORS_API_KEY`)**:
  1. Register a free non-commercial Standard account at [`https://openrouteservice.org/dev/#/signup`](https://openrouteservice.org/dev/#/signup) and verify your email.
  2. Log in at [`https://openrouteservice.org/dev/#/login`](https://openrouteservice.org/dev/#/login), open **Tokens**, and request/create a free API token.
  3. Copy the token into `OPENROUTESERVICE_API_KEY=` in `.env` (sent directly in the `Authorization` header to `https://api.openrouteservice.org`).

#### 3. Property Facts, AVM, Rent Comps & MLS Feeds
*(Optional base URL override: `ATTOM_BASE_URL`; keyless JSON/CSV listing feed override: `PUBLIC_LISTING_FEED_URL`.)*
- **ATTOM Data (`ATTOM_API_KEY`)**:
  1. Go to [`https://api.developer.attomdata.com/`](https://api.developer.attomdata.com/) and click **Get a Free Trial API Key** / **Sign Up** (30-day developer trial, typically ~1,000 API hits).
  2. Verify your email, log in, and open **Account → Applications** to view your API key.
  3. Set `ATTOM_API_KEY=` in `.env`.
- **RESO Web API 2.0 (`RESO_API_URL` + `RESO_BEARER_TOKEN`)**:
  1. Obtain licensed OData RESO Web API 2.0 access from your MLS or IDX vendor (e.g., Bridge Interactive, MLS Grid, CoreLogic Trestle, or Spark).
  2. Copy your OData resource root URL (e.g., `https://reso.example.com/odata`) into `RESO_API_URL=` and your OAuth/Bearer token into `RESO_BEARER_TOKEN=`.
- **SimplyRETS (`SIMPLYRETS_AUTH`)**:
  1. For immediate sandbox testing with dummy data, set `SIMPLYRETS_AUTH=simplyrets:simplyrets` (public test credentials from [`https://simplyrets.com/`](https://simplyrets.com/)).
  2. For live MLS listings, create a SimplyRETS account, connect your MLS RETS/RESO feed, and enter `SIMPLYRETS_AUTH=your_api_key:your_api_secret`.

#### 4. Building Permits, Zoning, Title/Court Dockets & Inspection Diligence
*(Optional base URL overrides: `SHOVELS_API_BASE_URL`, `ZONEOMICS_API_BASE_URL`, `COURTLISTENER_API_URL`.)*
- **Shovels (`SHOVELS_API_KEY`)**:
  1. Register at [`https://app.shovels.ai/register`](https://app.shovels.ai/register) (new accounts start on a non-expiring **Free** plan with monthly credits and no credit card; docs at [`https://www.shovels.ai/`](https://www.shovels.ai/)).
  2. Open the email magic link, sign in, and click your profile → **Account → API Key**.
  3. Set `SHOVELS_API_KEY=` in `.env`.
- **Zoneomics (`ZONEOMICS_API_KEY`)**:
  1. Visit [`https://www.zoneomics.com/`](https://www.zoneomics.com/) (API sandbox available for sample testing; developer plans/trials available via signup).
  2. Create a developer account / request trial API access and generate your key in account settings.
  3. Set `ZONEOMICS_API_KEY=` in `.env`.
- **CourtListener REST API v4 (`COURTLISTENER_API_TOKEN`)**:
  1. Register a free account at [`https://www.courtlistener.com/`](https://www.courtlistener.com/) (anonymous RECAP searches work keyless at lower limits).
  2. Log in and copy your personal token from [`https://www.courtlistener.com/profile/api-token/`](https://www.courtlistener.com/profile/api-token/).
  3. Set `COURTLISTENER_API_TOKEN=` in `.env` (sent as `Authorization: Token <token>`).
- **Custom Inspection & Disclosure API (`INSPECTION_API_URL` + `INSPECTION_API_KEY`)**:
  1. Point `INSPECTION_API_URL=https://inspection.example.com/api/docs` at an internal/partner HTTPS JSON or PDF endpoint accepting `address`, `zip`, `apn`, and `slug` query parameters.
  2. If the endpoint requires bearer authentication, set `INSPECTION_API_KEY=` (sent as `Authorization: Bearer <token>`).

#### 5. Geocoding, Places Autocomplete, Street-Level Imagery & IP Geolocation
*(Optional base URL overrides: `ARCGIS_GEOCODER_URL` — full `.../findAddressCandidates` endpoint, also enables POI/airport fallback when set — and `MAPILLARY_API_BASE`.)*
- **Esri ArcGIS Location Platform (`ARCGIS_API_KEY`)**:
  1. Create a free developer account at [`https://location.arcgis.com/`](https://location.arcgis.com/) (includes a free monthly geocode/places allowance).
  2. In the developer dashboard, go to **API Keys → New API Key**, enable **Geocoding**, and copy the token.
  3. Set `ARCGIS_API_KEY=` in `.env`.
- **Google Maps Places & Street View Embed (`GOOGLE_MAPS_API_KEY`, `GOOGLE_MAPS_EMBED_KEY`)**:
  1. Open [`https://console.cloud.google.com/`](https://console.cloud.google.com/), create/select a project, and attach a Google Cloud Billing account (required by Google Maps Platform; each SKU includes a monthly free usage tier).
  2. Under **APIs & Services → Library**, enable **Places API (New)** (for server-side `GOOGLE_MAPS_API_KEY`) and **Maps Embed API** (for browser-side `GOOGLE_MAPS_EMBED_KEY`).
  3. Under **APIs & Services → Credentials**, create **two separate API keys** (`AIza...`):
     - Restrict the `GOOGLE_MAPS_API_KEY` server key to **Places API (New)** (and optionally your server IP) for address autocomplete.
     - Restrict the `GOOGLE_MAPS_EMBED_KEY` browser key to **Maps Embed API** + **HTTP referrers** (your Open Estate hostname) to unlock the interactive **Street View** tab on reports.
- **Mapillary (`MAPILLARY_ACCESS_TOKEN`)**:
  1. Log in at [`https://www.mapillary.com/dashboard/developers`](https://www.mapillary.com/dashboard/developers) (free account).
  2. Click **Register Application**, enter an app name/description (read-only access), and submit.
  3. Click **View** under **Client Token** and set `MAPILLARY_ACCESS_TOKEN=` in `.env`.
- **IP Geolocation (`ip_geolocation`) — Zero Keys Required**:
  - Built-in [`extensions/ip_geolocation/manifest.yaml`](extensions/ip_geolocation/manifest.yaml) uses `auth_type: none` (reverse-proxy edge headers + keyless `ipwho.is` / `freeipapi.com` / `ipapi.co` verified against U.S. Census TIGER ZCTA5). No IPinfo or third-party geolocation token is required; set `IP_GEOLOCATION_ENABLED=false` in `.env` only if you want to disable `/api/geo/ip`.

#### 6. Schools, Air Quality & Public-Data Rate-Limit Boosters
*(Optional base URL override: `HUD_FMR_BASE_URL`.)*
- **HUD User (`HUD_API_TOKEN`)**:
  1. Register for free at [`https://www.huduser.gov/hudapi/public/register`](https://www.huduser.gov/hudapi/public/register) and select the **Fair Market Rents** dataset API.
  2. Verify your email, log in, and click **Create New Token**.
  3. Set `HUD_API_TOKEN=` in `.env` for live ZIP Small Area Fair Market Rent (SAFMR) lookups.
- **api.data.gov / FBI Crime Data Explorer (`DATA_GOV_API_KEY`, plus `CDE_API_KEY` for batch light scans)**:
  1. Request an instant free key at [`https://api.data.gov/signup/`](https://api.data.gov/signup/) (Open Estate defaults to `DEMO_KEY`, which is capped at `30 req/hr` / `50 req/day`; a personal key raises `api.usa.gov` fallback limits to `1,000 req/hr`).
  2. Copy the emailed key into `DATA_GOV_API_KEY=` in `.env` (and optionally `CDE_API_KEY=` for batch light scans in `src/search/light_scan.rs`, which checks `CDE_API_KEY` before `DATA_GOV_API_KEY`).
- **US EPA AirNow (`AIRNOW_API_KEY`)**:
  1. Request a free API account at [`https://docs.airnowapi.org/account/request/`](https://docs.airnowapi.org/account/request/).
  2. Copy the API key from your approval email into `AIRNOW_API_KEY=` in `.env` to add live EPA station AQI alongside Open-Meteo.
- **Socrata Open Data (`SOCRATA_APP_TOKEN`)**:
  1. Register a free user account on a Socrata-powered city open-data portal (for example [`https://data.sfgov.org/`](https://data.sfgov.org/) or [`https://data.cityofnewyork.us/`](https://data.cityofnewyork.us/) — docs at [`https://dev.socrata.com/`](https://dev.socrata.com/)).
  2. Open **My Profile → Developer Settings → Create New App Token** and copy the **App Token** (not Secret Token).
  3. Set `SOCRATA_APP_TOKEN=` in `.env` (sent as `X-App-Token` across municipal Socrata adapters).
- **SchoolDigger & GreatSchools (`SCHOOLDIGGER_APP_ID` + `SCHOOLDIGGER_APP_KEY`, `GREATSCHOOLS_API_KEY`)**:
  - Open Estate report school scoring uses strictly **official public academic datasets** (NCES + California School Dashboard / CAASPP / federal EDFacts percentiles; third-party school ratings are never mixed into report scores).
  - If you track connector credentials in `GET /api/health` (`connectors`), **SchoolDigger** offers a free `DEV/TEST` tier at `developer.schooldigger.com` (`SCHOOLDIGGER_APP_ID`, `SCHOOLDIGGER_APP_KEY`), and **GreatSchools** commercial API keys are requested via [`https://www.greatschools.org/api/`](https://www.greatschools.org/api/) (`GREATSCHOOLS_API_KEY`).

#### 7. Outbound SMTP Property Alerts
- **Variables**: `SMTP_HOST`, `SMTP_PORT` (`465` for `tls`, `587` for `starttls`, `25` for plain), `SMTP_SECURE` (`tls` | `starttls` | `none`), `SMTP_USER`, `SMTP_PASS`, `SMTP_FROM`, plus optional `SMTP_FROM_NAME` and `SMTP_REPLY_TO`.
- **How to configure**:
  1. Pick any SMTP relay with a free or personal tier — for example **Gmail** (enable 2-Step Verification and generate an App Password for `smtp.gmail.com:465`), **Brevo** (free 300 emails/day SMTP relay), **Resend** (free 3,000 emails/month SMTP mode), or your own mail server (`smtp.example.com`).
  2. Add `SMTP_HOST`, `SMTP_PORT`, `SMTP_SECURE`, `SMTP_USER`, `SMTP_PASS`, and `SMTP_FROM="Open Estate Alerts <alerts@example.com>"` to `.env` to enable saved-search and price/status email alerts.

---

## Key Environment Variables

See [`.env.example`](.env.example) for the complete annotated template and [Optional API Keys & Registration Guide](#optional-api-keys--registration-guide-bring-your-own-key) above for every BYOK API key, routing fallback provider, and SMTP credential.

| Variable | Default | Purpose |
|---|---|---|
| `HOST` / `PORT` | `0.0.0.0` / `8080` | HTTP bind address and port |
| `BRAND_NAME` / `BRAND_DOMAIN` / `PUBLIC_BASE_URL` | `Open Estate` / `open-estate.local` / *(request origin)* | Configurable brand identity and canonical external URL |
| `PUBLIC_DATA_MODE` | `strict` | `strict` (100% US federal public-domain + BYOK APIs) or `local_csv` (reads local user CSVs in `DATA_DIR`) |
| `DATABASE_PROVIDER` | `sqlite` | `sqlite` (embedded WAL) or `mysql` |
| `SQLITE_DB_PATH` | `.cache/open-estate.sqlite` (`/data/open-estate.db` in Docker) | SQLite database file path (`:memory:` supported) |
| `MYSQL_URL` | *(unset)* | MySQL 8/9 connection URL (`mysql://user:pass@host:3306/db`) |
| `OSRM_BASE_URL` / `OSRM_REGIONS_JSON` | *(unset = estimates or Tier-2 cloud pool)* | Self-hosted Tier-1 OSRM router URL or regional router bounding-box JSON |
| `ROUTING_FALLBACK_STRATEGY` / `ROUTING_FALLBACK_ORDER` | `round_robin` / *(all 4 enabled cloud routers)* | Tier-2 cloud matrix load-balancing mode (`round_robin` or `priority`) when OSRM is unset/unavailable |
| `OUTBOUND_PROXY_URLS` / `FREE_PROXY_POOL_ENABLED` | *(unset)* / `false` | Optional operator-supplied HTTP/SOCKS5 proxies (BYOK API hosts bypass proxies automatically) |
| `TRUST_PROXY_HEADERS` / `OPEN_ESTATE_TRUST_PROXY` | `false` | Set `true` only behind a trusted reverse proxy (Caddy, Nginx, Traefik) |
| `OPEN_ESTATE_TRUST_CF_HEADERS` | `false` | Trust `CF-Connecting-IP` / `CF-*` geo headers (requires trusted proxy peer) |
| `OPEN_ESTATE_REQUIRE_INTERNAL_AUTH` | `false` | Require signed `oe_session` + `X-OpenEstate-Token` or API key on `/api/*` & `/r/:slug/report.json` |
| `OPEN_ESTATE_INTERNAL_API_KEY` / `OPEN_ESTATE_API_KEY` | *(unset)* | Optional bearer / `X-OpenEstate-Internal-Key` / `X-OpenEstate-API-Key` for trusted callers |
| `OPEN_ESTATE_SESSION_SECRET` | *(ephemeral random)* | Shared HMAC-SHA256 signing secret for `oe_session` across replicas |
| `ALLOWED_EXTENSION_IDS` | *(unset = allow none)* | Comma-separated allowlist of WebExtension IDs/origins permitted for extension CORS |
| `OCR_MODE` | `auto` | PDF OCR mode (`auto`, `local_only`, `vision_only`, `disabled`) |
| `LLM_MODEL` / `LLM_EFFORT` / `LLM_FALLBACK_MODEL` | `gpt-4.1` / `none` / `gpt-4o-mini` | Primary synthesis model, reasoning effort (`none`/`low`/`medium`/`high`), and fallback model (active when `OPENAI_API_KEY` is set) |

---

## Limitations & Fair Housing Compliance

Open Estate is automated personal research software compiled from public datasets and user-uploaded documents — **not a licensed real-estate appraisal, commercial appraisal, home inspection, contractor bid, Phase I ESA, title insurance commitment, or legal/financial/tax advice**. Verify all figures against official county records, municipal permit offices, HOA disclosures, licensed specialists, and seller disclosures. 堪舆风水 content is cultural context only and presented exclusively in Chinese. Strict Fair Housing Act guardrails and Equal Housing Opportunity notices are enforced across all scoring, search, and AI modules.

---

## License

Licensed under the **Open Estate Personal Research & Non-Commercial License v1.0** — see [`LICENSE`](LICENSE).

- **Permitted (Free)**: Viewing, modifying, compiling, and self-hosting on your own hardware/Docker environment **solely for personal, individual, non-commercial property research**.
- **Prohibited**: Operating any hosted, cloud, multi-tenant, SaaS, or API service for third parties; charging fees or credits; using the software or its outputs in real-estate brokerage, lending, appraisal, property management, or commercial investment operations; or reselling/syndicating reports or datasets.
- **Commercial & Cloud Service**: Commercial licensing and hosted enterprise inquiries are handled exclusively by **OmniSolo LLC**.
