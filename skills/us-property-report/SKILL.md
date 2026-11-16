---
name: us-property-report
description: >-
  Generate a comprehensive, visual research report for any US residential, commercial (CRE), commercial land, or
  agricultural/farmland address via Open Estate (open-estate.local) — estimated value with range, upside/downside scenarios,
  cost of ownership, always-on HOA profile & 7-module buyer diligence (transfer taxes, special districts, utilities,
  broadband, mandates, break-even, homestead), Commercial Land RLV/pad-site & Agricultural/Farmland USDA NRCS soil +
  NASS valuation, 8-concept Store Profit & Plaza Co-Tenancy simulator, insurance shock & CapEx/condition triage,
  deed/title & 3-vertical marketplace, natural hazards (flood, earthquake, wildfire, radon), pollution and contaminated
  sites, neighborhood amenities, schools, crime context, and a Chinese-only 堪舆风水 reading — rendered as a
  self-contained web page with interactive maps, charts, Deal Room comparison, and EN/中文/ES i18n (fluent-templates +
  i18next). Orchestrates property-geocode, property-facts, market-trends, property-valuation, natural-hazards,
  environmental-risk, neighborhood-livability, feng-shui-analysis, ai-analyst, inspection-review, and
  report-visualization skills. US only; not an appraisal or inspection.
---

# US Property Report (orchestrator)

One address in → one interactive web page out, served by the Rust binary `open-estate-server` at `/r/<slug>/`
(JSON at `/r/<slug>/report.json`). Every data module is compiled into the binary (`src/modules/`,
`src/property/`; contract in `skills/_shared/CONTRACT.md`). Reports are cached in the database (`cached_reports`,
SQLite WAL or MySQL); finished `reports/<slug>/report.json` (+ optional `index.html`) directories under
`--reports-dir` are loaded at startup and on demand.

## Start the server
```bash
cargo run --release --bin open-estate-server -- \
  --host 0.0.0.0 --port 8080 \
  --db-file .cache/open-estate.sqlite --reports-dir reports --data-dir .cache/datasets
```
API keys come from the environment or `.env` (README.md, "Environment Variables Reference"); the public-data
modules need none.

## Workflow (follow in order)

1. **Normalize & geocode** — `property-geocode` (`src/property/geocode.rs`, U.S. Census Geocoder, keyless). When
   nothing matches, `GET /api/report` answers 404 with code `ADDRESS_NOT_FOUND`: stop and ask the user. The
   orchestrator then snaps the point to the actual building ("rooftop") using the OpenStreetMap footprint
   (`fetch_rooftop_building`), stored as `geo.refined`.
2. **Collect property facts** — follow the `property-facts` skill. Preferred order:
   1. licensed connectors configured in the environment (RESO, SimplyRETS, ATTOM) → fetched
      automatically, together with keyless county assessor / city GIS records;
   2. otherwise do agent web research (listing sites, county assessor, city GIS) into a facts object following
      `skills/property-facts/references/facts-schema.md` and pass it as `given_facts` through the Rust API
      (`AwaitReportOptions` in `src/web/orchestrator.rs`). **Cite every field, note conflicts, never invent comps
      or numbers.** At minimum: beds, baths, sqft, lot, year built, sale history, 3–15 recent comps within ~0.5 mi.
3. **Build the report** — ask the running server:
   ```bash
   curl -G -s 'http://localhost:8080/api/report' --data-urlencode "address=$ADDRESS" -d 'ai=0' | jq .slug
   # or open http://localhost:8080/report?address=... (progress page, then a redirect to /r/<slug>/)
   ```
   `build_full_report` (`src/web/orchestrator.rs`) geocodes, then runs three waves of concurrent modules: rooftop
   footprint, Zillow + market trends, civic, city hall and NASA solar → facts, amenities, hazards, environment →
   valuation, feng shui and drive times (`access`). It then computes `decisionFactors` and the scorecard
   (`summary`, `src/web/summary.rs`) and stores the report. Builds run detached: closing the browser does not
   stop them, concurrent requests for the same address share one build, and `GET /api/job/<id>` reports progress.
4. **Dynamic AI analyst (`ai-analyst`)** — when `OPENAI_API_KEY` is set (and `ai=0` /
   `"ai": false` is not passed), the orchestrator runs `run_ai_analyst` (`src/ai/analyst.rs`, `gpt-4.1` →
   `gpt-4o-mini` fallback by default) after the deterministic report is saved, filling `report.ai` and
   `summary.narrative` (headline, executive paragraphs, upside/downside/verify, per-section analyst notes, 3×3 risk
   matrix, negotiation levers, seller questions, and Chinese-only feng shui synthesis). Interactive buyer Q&A
   uses `POST /api/ask` against the stored report.
5. **QA** — open the page (or screenshot it headless) and check: no "Section failed" cards, the map shows the
   footprint, numbers match `report.json`, warnings section lists real data gaps only.
6. **Deliver** — give the user the report URL (`/r/<slug>/`), the 3–5 most decision-relevant findings, and the
   open questions (things that need on-site or official verification).

## Output & HTTP Endpoints (`open-estate-server`, `src/web/server.rs`)
- `report.json` — `{ meta, geo, facts, modules: {market, amenities, access, hazards, environment, civic, valuation, fengshui, remodel}, decisionFactors, summary, hoaProfile, buyerDiligence, remodelAndRebuild, insurance, condition, titleInsuranceAndDeed, marketplace, cre, engagement, ai, translations, sources, warnings }`
  (schema: `references/report-schema.md`).
- `index.html` (`assets/template.html`, compiled into `open-estate-server` via `src/web/render.rs`) — self-contained interactive page (vendored MIT `i18next` + `Intl` EN/中文/ES toggle, strictly Chinese-only `#fengshui` 堪舆风水, Leaflet + ECharts, Your Match personalized card, Deal Room & Print/PDF, data embedded).
- **Open Estate Rust Server (`open-estate-server`) Endpoints**:
  - `GET /report?address=<addr>[&refresh=1][&ai=0]` & `POST /api/report/enqueue` / `GET /api/job/<id>` — detached report build & progress page.
  - `GET /r/<slug>/` & `GET /r/<slug>/report.json` — interactive visual report and stored JSON.
  - `GET|POST /api/report?address=<addr>[&lang=zh][&refresh=1]` — programmatic JSON report API (`86400s` per-home refresh cooldown returning `429 DAILY_REFRESH_LIMIT`).
  - `POST /api/report/:slug/slides` · `POST /api/report/:slug/artifact` · `GET|POST /api/media/:slug/*` — walkthrough slide deck, dynamic artifact canvases, listing/aerial media manifest, and walk-in media studio.
  - `POST /api/translate` — `EN | 中文 | ES` translation via `GOOGLE_TRANSLATE_API_KEY` or LLM fallback, cached by content hash; server pages localized with Mozilla `fluent-templates`.
  - `GET /match` · `GET /api/match/{catalog,areas,geocode}` · `POST /api/match/{profile,score,rank,feedback}` · `GET /api/match/rank/:id` · `GET|PUT /api/me/profile` · `POST /api/house/photos` — Your Match deterministic scoring, multi-area ranking, profile persistence, and ephemeral tour photo clues.
  - `POST /api/inspect` · `POST /api/title-review` · `POST /api/hoa-review` · `POST /api/packet-review` · `POST /api/repair-addendum` · `POST /api/condition-score` — API-first (`INSPECTION_API_URL` / RESO `Media`) or buyer-uploaded Inspection, Prelim Title, HOA, multi-PDF/`.zip` packet Contradiction Matrix, Repair Addendum, and CapEx/photo condition AI review (isolated per `X-OpenEstate-Client-Id`), plus `GET /api/agent-match/recommendations` & `POST /api/agent-match/request` when reports cannot be fetched via API.
  - `POST /api/cre/underwrite` · `POST /api/cre/business-eval` · `POST /api/cre-review` — Commercial Real Estate ($1M–$25M) underwriting (`multifamily_5_plus`, `retail`, `office`, `industrial`, `mixed_use`, `hospitality`, `commercial_land`, `agricultural_farm`), **Commercial Land** Residual Land Value (RLV) waterfall & NNN pad/drive-thru ground-lease valuation (`cre.landAndFarm`), **Agricultural / Farmland** USDA NRCS Soil Data Access (`mukey`, Prime Farmland, Capability Class I–VIII, NCCPI) + 50-state USDA NASS $/acre & 4-mode farm operating profit valuation (`cre.landAndFarm`), **8-Concept Store Profit & Plaza Co-Tenancy Simulator** (`cre.businessProfit` — Full-Service Restaurant, Café/QSR, Clothing & Apparel Store, Sporting Goods Store, Specialty Grocery, Fitness Studio, Wellness/Salon, General Retail modeling OpenStreetMap plaza anchor foot-traffic boost vs. direct competitor cannibalization), and Phase I ESA/ALTA/lease abstract & trade-area intelligence.
  - `GET /api/buyer-diligence/:slug` — always-on HOA Profile & 7 buyer-decision diligence modules.
  - `GET /api/house3d/:slug` — 3D scene for the report's `#house3d` section (USGS 3DEP LiDAR roofs, terrain and trees over the mapped footprints; warmed in the background after an interactive report build, otherwise built on first view, then cached — see `house-3d-model`).
  - `GET|POST /api/marketplace/quotes` · `POST /api/marketplace/click` — compliant Mortgage, Insurance & Title marketplace (`MARKETPLACE_PARTNERS_JSON` or PMMS benchmarks, `Equal Housing Opportunity.`).
  - `GET|POST /api/property/:slug/likes` · `GET /api/property/:slug/refresh-status` · `POST /api/property/:slug/platform-signals` — engagement, likes & hotness signals.
  - `GET|POST /api/extension/summary` · `GET /deal-room` · `POST /api/deal-room/compare` — Chrome/Edge MV3 Extension summary & side-by-side Deal Room matrix.
  - `POST /api/ask` — grounded buyer Q&A over `report.json` + uploaded inspection/HOA/title findings.
  - `POST /api/match` · `GET /api/listings` · `POST /api/listings/ingest` · `GET /api/watched-zips` · `POST /api/sync` · `GET|POST /api/subscriptions` · `GET|POST /api/subscriptions/unsubscribe` — NL search, ZIP sync, and SMTP alerts.
  - `POST /api/scan/batch` · `GET /api/scan/batch/<id>` · `POST /api/scan/batch/<id>/cancel` · `GET /api/scan/metrics` — 10,000-home priority batch scanner.
  - `GET /api/suggest?q=...` · `GET /api/health` · `GET /healthz` · `GET /readyz` · `GET /favicon.{svg,ico}` · `GET /apple-touch-icon.png`.
- CLI (`src/main.rs`): `open-estate-server [serve]`, `open-estate-server migrate --from <src> --to <dst>`, and `open-estate-server schools ingest --state <CA|EDFACTS|...>`.

## Scorecard (0–100, higher is better)
This is the Open Estate Score: one fixed formula, identical for everyone. The personal Your Match score is separate;
both are described in `references/scoring.md`.

| Score | Source | Weight in overall |
|---|---|---|
| Investment outlook | ZIP appreciation (10-yr CAGR), YoY momentum, rent yield, price-to-income | 30% |
| Hazard safety | natural-hazards `scores.overall` | 25% |
| Livability | amenity access (60%) + EPA walkability (40%) | 20% |
| Environment | environmental-risk `scores.overall` | 15% |
| Feng shui | feng-shui-analysis `scores.overall` | 10% |

Details and caveats: `references/scoring.md`.

## Rules
- Facts and comps come only from cited sources or APIs; unknown → `null` + warning.
- Value is always a **range** with the methods shown; label it "not an appraisal".
- Present schools, crime and demographics as facts; never label a neighborhood "good/bad/safe"; never use race,
  religion, national origin, familial status or disability (Fair Housing Act). Feng shui is cultural information.
  Two-score and Your Match rules: `references/compliance.md`.
- Respect provider terms: cache responses, keep Overpass/OSM queries small, attribute OSM/Zillow/FEMA/USGS.

## Related skills
`property-geocode` · `property-facts` · `market-trends` · `property-valuation` · `natural-hazards` ·
`environmental-risk` · `neighborhood-livability` · `feng-shui-analysis` · `house-3d-model` · `ai-analyst` · `report-visualization`
