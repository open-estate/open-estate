---
name: report-visualization
description: >-
  Render a us-property-report JSON into a single self-contained, interactive web page — hero with satellite
  footprint and value range, scorecard radar, Leaflet map with live FEMA flood layer and module layers (faults,
  contaminated sites, schools, amenities, roads/power/water, feng-shui facing line), ECharts value history, comps,
  scenario fan chart, ownership-cost donut, hazard/pollution/neighborhood charts, feng shui luopan, site plan,
  flying-star grids and Ba Zhai wheel, due-diligence checklist and sources. Use when a property report needs to be
  presented visually or re-rendered after editing report.json / narrative.json.
---

# Report Visualization

- Template: `assets/template.html` (vanilla JS + vendored MIT `i18next` + `Intl` EN/中文/ES localization; Leaflet 1.9 + ECharts 5; offline-capable embedded assets),
  compiled into the `open-estate-server` binary via `include_str!("../../assets/template.html")` in `src/web/render.rs`.
  The sanitized report JSON is embedded in `<script id="report-data" type="application/json">` (with internal model/provider metadata stripped via `sanitize_report_for_html`) and every section renders client-side, so the rendered HTML works as a standalone static page, an email attachment, or an HTTP response.
- Shared client core (`assets/shared/open_estate_core.js`, `window.OpenEstateCore`) is injected ahead of `assets/report_workspace.js`, `assets/walkin_media.js`, `assets/report_dashboard.js`, `assets/house3d.js`, and `assets/report_motion.js`, providing shared HTML escaping, locale/cookie (`open_estate_lang`) & USD formatters, prose scrubbing, `prefers-color-scheme` listeners, 3D tour URL normalization, and a single `three@0.170.0` WebGL loader singleton.
- `render_report_html(report: &Value) -> String` in `src/web/render.rs` fills `__REPORT_TITLE__`, `__REPORT_DESC__`, and `__REPORT_JSON__` (HTML-attribute-encoding title/description and escaping `<`, `U+2028`, `U+2029` inside the script tag) and is served on `GET /r/<slug>/` (`src/web/server.rs`) as well as cached alongside `report.json` in `DbStore`.
- Interactive Sections & Modules (`Open Estate` / `open-estate.local`): Overview (+ Official Building/Parcel Register, BCR/FAR & Permits, `summary.verdictTone`, Your Match personalized card, 24h Refresh button, ❤️ Like & Cross-Platform Hotness, Deal Room & Print/PDF) · House Profile & Style (`#house`, 16 RESO styles, 11 character features, ephemeral tour photos) · **3D Model (`#house3d`, `assets/house3d.js` inlined at `__HOUSE3D_JS__`: WebGL exterior model with LiDAR-measured roofs, terrain and trees from `GET /api/house3d/:slug`, sun study, and the listing's own 3D tour embedded — see the `house-3d-model` skill)** · Value (+ Multi-Factor Waterfall) · Sale, Official Tax/Land Roll & Deed/Title/Foreclosure History (`titleInsuranceAndDeed`) · Nearby Comps (Sold · Rental/Lease · Active Nearby Listings · `#rentTierChart`) · Market Trends · Upside & Downside · Payment (Optimal 760-FICO rate + interactive Credit Score / Loan Program / Term / Occupancy / Points / PMI / APR calculator), Always-On HOA Profile (`hoaProfile`), 7-Module Buyer Diligence (`buyerDiligence`), **Remodel, Tear-Down / Full House Rebuild & Lawn / Yard Quotes (`#remodel`, `remodelAndRebuild`: 12 regional Cost vs. Value remodel ROI projects, 3-tier tear-down & rebuild calculator w/ NAHB 8-stage breakdown, and lawn/yard quotes w/ municipal water-agency turf rebates)** & Insurance Shock / CapEx Condition Triage (`insurance`, `condition`, `/api/condition-score`) · 3-Vertical Marketplace (`marketplace` w/ `Equal Housing Opportunity.` footer) · Commercial Real Estate (`cre`) · Climate & Natural Hazards · Environment · Schools · Safety & Emergency · Neighborhood & Mobility Hub (+ My Places drive times) · **堪舆风水 (`#fengshui`, strictly Chinese-only)** · Inspection, Prelim Title, HOA, Multi-PDF/`.zip` Contradiction Matrix & Repair Addendum Workbench · AI Analyst & Q&A · Due-Diligence Checklist & Sources. Each section degrades gracefully ("data unavailable") if its module failed.
- Map layers come from any module's `data.mapLayers` (GeoJSON, WGS84, `properties.name/popup/color`) — new modules
  appear automatically in the layer switcher.
- Hero: the page opens on the live satellite map (`#gallery` / `#gMain`, `renderGallery` + `addPropertyOutlines`) with
  the lot (sky-blue dashed), main house (yellow) and garage / ADU (magenta) outlined, a hover area tip on each shape,
  a legend in the tag and the facing arrow, plus street / terrain tiles and 2D · 3D · Street View modes. Listing photos
  and aerial archive crops are the second hero tab (`report_workspace.js`, `#heroMediaBento`). The same outline helper
  draws the lot / house / garage on every section map; the feng-shui map draws them in Chinese.
- Video tour (`assets/report_motion.js`, inlined at `__REPORT_MOTION_JS__`; "🎬 Watch 50s Video Tour" in the hero
  tabs, chapter bar, walkthrough banner and the walkthrough deck's "Play as Motion Video"): a 50 s canvas film in six
  scenes — fly-in to the home · lot with the same lot / house / garage outlines, areas and facing arrow · value on one
  zoomed axis with the range and the report's own method values · monthly cost as one stacked bar from
  `valuation.ownership.monthly` (+ hatched optional quake cover, rent on the same scale) · hazards and everyday
  places (pins in view, edge arrows for the rest) · verdict, score and the deck's watch-outs before an offer.
  Imagery is Esri World Imagery map tiles (CDN-cached, levels ≤ 19, preloaded in playback order) plus one sharp
  `MapServer/export` image of the lot for the close-ups; every overlay is drawn in the same Web-Mercator projection
  as the imagery, so outlines sit exactly on the roofs. Missing data is left out, never invented. Burned-in EN / 中文 /
  ES captions are built from the numbers on screen; 16:9 or 9:16 (phones), `.webm` export at 1080p.
  The look follows the viewer's system setting (`prefers-color-scheme`, live): a light design (white frosted cards,
  dark ink, light washes over the imagery and a frosted light backdrop) or a dark one, canvas and modal chrome alike
  (`OpenEstateMotion.setTheme` pins one for previews). The walkthrough deck (`#walkthroughModal`) matches the light
  report page with a frosted light backdrop instead of switching on `prefers-color-scheme: dark`. There is no
  voiceover — captions carry the story, with optional Web Audio micro-SFX (`motionAudioBtn`). Test: `tests/report_motion_test.rs`.

## Re-render without refetching
```rust
use open_estate::web::render::render_report_html;

let html = render_report_html(&report);
```
Or over HTTP via the running server (`src/web/server.rs`): `GET /r/<slug>/` renders directly from the stored report
(and updates after `POST /api/translate` or `POST /api/report` with `refresh=1`).

## Design rules
Clean editorial layout, one idea per card, every chart labelled with its source and unit, color semantics fixed
(green good / amber watch / red risk; gold for feng shui; verdict pill driven by `summary.verdictTone`), numbers
tabular, mobile-friendly at 980 px, print-friendly. QA with a headless browser screenshot; there must be no
"Section failed to render" cards.
