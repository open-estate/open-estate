---
name: inspection-review
description: >-
  AI analysis of buyer-uploaded Home Inspection, Termite/WDO, Roof, Sewer Lateral, Foundation, Seller Disclosure
  (TDS/SPQ/NHD), HOA/Condo packets, multi-PDF/.zip disclosure packets, and US Preliminary Title Report (Prelim) /
  Deed / Lien documents, cross-referenced against a Open Estate (open-estate.local) US property report. Extracts itemized
  defects with page citations and regionally calibrated repair cost ranges (plus PII-redacted Schedule A/B vesting,
  deeds of trust, liens, easements, solar UCC-1 filings, HOA reserve/litigation alerts, cross-doc Contradiction
  Matrix, and trade-grouped Repair-Credit Addendum), contradictions/confirmations against public records, and
  copy-ready buyer negotiation scripts. Runs inside the Rust `open-estate-server`.
---

# Inspection, Disclosure & Preliminary Title Review (`inspection-review`)

Implementation (Rust): `src/ai/inspect.rs` (`analyze_inspection`), `src/ai/title_review.rs`
(`analyze_title_report`, `redact_title_pii`) and the PDF pipeline in `src/ai/pdf.rs`. The inspection review needs an
LLM (`OPENAI_BASE_URL` + `OPENAI_API_KEY`, see the `ai-analyst` skill); the title review parses the redacted text
deterministically and adds an AI note when an LLM is configured.

## HTTP (`src/web/server.rs`)
```http
POST /api/inspect          (or POST /api/title-review)
Content-Type: application/json
X-OpenEstate-Client-Id: <browser id>

{"slug": "<report-slug>",
 "documents": [{"name": "General-Home-Inspection.pdf", "base64": "..."}]}
```
- Each document carries `base64` (PDF bytes) or `text` (`[Page 1] ...`, already extracted). Request bodies may be up
  to 30 MB.
- The report is loaded from server storage by `slug`; the response is `{ ok, data, warnings }`.
- With an `X-OpenEstate-Client-Id` header the result is saved for that browser for 14 days:
  `GET /api/inspect?slug=...` / `GET /api/title-review?slug=...` return it, and `POST /api/ask` uses it as context.
- Same-origin JSON requests only (scripted user agents get `403 AUTOMATED_SCRAPER_BLOCKED`), with per-IP AI rate and
  concurrency limits.

## Rust API
```rust
use open_estate::ai::{analyze_inspection, analyze_title_report};

let insp = analyze_inspection(&http, &env, &report, documents, None).await?;     // ModuleOutput
let title = analyze_title_report(&http, &env, &report, documents, None).await?;  // documents: Vec<UploadedDocInput>
```

## Capabilities
- **API-First Fetch & Conditional Upload (`src/property/deep_research.rs`, `src/modules/agent_recommend.rs`)**: During report generation, Open Estate automatically checks RESO `$expand=Media` document attachments, RentCast/MLS `documents`/`inspectionUrl`/`disclosureUrl`/`prelimUrl` fields, and any configured `INSPECTION_API_URL` (`INSPECTION_API_KEY`) feed, producing PII-scrubbed `publicResearch.apiFetchedReviews` (`inspection`, `titleReview`, `hoaReview`, `packetReview`). When fetched via API, the report renders the analysis directly and tucks manual upload into a collapsed supplemental `<details>` drawer; we only ask the buyer to upload when API retrieval is unavailable (e.g., broker-gated Disclosures.io/Glide portals), hinting that they can request the packet via their buyer's agent and offering a RESPA-compliant local Buyer's Agent or licensed Home Inspector recommendation backed by the `recommended_agents` & `agent_match_requests` database tables (`GET /api/agent-match/recommendations`, `POST /api/agent-match/request` — `recommended_agents` starts empty with 0 rows).
- **PDF extraction (`src/ai/pdf.rs`)**: the web UI extracts page-indexed text in the browser with `pdf.js`; on the
  server, digital PDFs are read in pure Rust (`lopdf` / `pdf_extract`), and pages with fewer than 25 selectable
  characters go to local `tesseract` OCR (embedded JPEG/JP2 or raw image streams, or `pdftoppm` renders), falling back
  to LLM vision OCR when local OCR is unavailable (`OCR_MODE=llm` uses vision OCR directly; `TESSERACT_BIN`,
  `PDFTOPPM_BIN` and `OPEN_ESTATE_OCR_TIMEOUT_MS` tune local OCR). `[Page N]` citations are kept, and
  prompt-injection filtering (`validate_and_scrub_property_document`) runs before any text reaches the LLM.
- **US Preliminary Title Report (Prelim) & Deed/Lien Analyzer (`src/ai/title_review.rs`)**: Automatically masks owner/borrower names, SSNs/TINs, escrow/order IDs, and loan numbers (`redact_title_pii`) before processing; extracts Schedule A vesting, estate type, APN, deed transfer chain, active vs reconveyed Deeds of Trust, involuntary liens (tax, mechanic's, HOA, lis pendens, NOD), and Schedule B exceptions (easements, CC&Rs, solar UCC-1); cross-checks against `report.facts` (factual summary only — not legal advice or title insurance).
- **HOA, Multi-PDF/`.zip` Packet & Repair-Credit Addendum (`src/ai/{hoa_review,packet_review,repair_addendum}.rs`)**: `POST /api/hoa-review` inspects condo/HOA reserve studies, budgets, litigation, rental caps & special assessments; `POST /api/packet-review` unpacks multi-PDF and `.zip` packets (with zip-bomb & traversal guards) into a cross-document Contradiction Matrix with specialist-priority zero-double-counting deduplication; `POST /api/repair-addendum` generates trade-grouped repair-credit addendum drafts (`X-OpenEstate-Client-Id` scoped).
- **Cross-referenced against public data**: Links physical findings (foundation cracks, electrical panel era, roof age, drainage, unpermitted work) directly to seismic PGA, CGS liquefaction, FEMA flood zones, build year, and permit notes in `report.json`.
- **Grounded Q&A integration**: Saved inspection and title findings for the same browser are passed into `ask_property_question` (`POST /api/ask`) so follow-up buyer questions incorporate both public records and uploaded inspection results.
