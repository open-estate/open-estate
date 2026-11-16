---
name: property-facts
description: >-
  Gather and normalize the subject property's facts for a US address — type, beds, baths, living area, lot size and
  shape, year built, stories, garage, HOA, APN, zoning, sale & listing history, property-tax rate and assessments,
  third-party value/rent estimates, assigned schools, recent comparable sales and local context notes — with a
  citation for every field and explicit conflict notes. Uses the RentCast API when RENTCAST_API_KEY is set, otherwise
  guides agent web research into a facts.json file. Use before valuation/reporting or when asked for a home's
  details, sale history or comps.
---

# Property Facts

Facts drive valuation and several checks, so accuracy beats completeness. **Never invent a number; never fabricate
a comp.** Unknown → leave it out (the pipeline warns).

## Where it runs
`build_facts_and_mls_with_given` in `src/property/facts.rs`, called by the report orchestrator
(`src/web/orchestrator.rs`, second wave) for every report the `open-estate-server` binary builds; the result is
`report.facts`. It merges, in this order of precedence:
1. facts supplied by the caller (`given_facts`, see Option B);
2. the licensed connectors in `src/property/mls.rs` (`fetch_mls_and_records`): RESO Web API, SimplyRETS, RentCast,
   ATTOM — each runs only when its key is configured;
3. keyless county assessor, state controller and municipal GIS records (`src/property/public_records.rs`), queried
   for every report.

Later sources only fill fields that are still empty (tax history is merged by year, AVMs are appended). Owner names
and mailing addresses are never requested and are stripped before the facts are stored. When `beds`, `baths`, `sqft`,
`yearBuilt` or `lotSqft` is still missing, the module adds a "Missing fact" warning.

> Caveat: when neither the caller nor a connector supplies comps, the module fills `comps` with up to six
> neighbouring parcels priced from a recent recorded sale or, failing that, from the ZIP's Zillow index
> (`dealType: "Parcel + ZHVI benchmark"`, source "… + Zillow ZHVI Calibrated"). Those benchmark rows are estimates,
> not sales: never present them as comparable sales.

## Option A — licensed connectors (automatic)
Set `RENTCAST_API_KEY` (free tier available) in the environment (`.env` or a Kubernetes Secret). The connector calls
RentCast `/v1/properties`, `/v1/avm/value` (with comparables), `/v1/avm/rent/long-term` and `/v1/listings/sale`
for the subject, plus nearby active listings from `/v1/listings/sale` and `/v1/listings/rental/long-term`.
`RESO_API_URL` + `RESO_BEARER_TOKEN`, `SIMPLYRETS_AUTH` and `ATTOM_API_KEY` enable the other connectors.

## Option B — agent research → `given_facts`
Search in this order and record the source of each field:
1. Listing portals (Redfin, Movoto, Coldwell Banker, Compass, Zillow/Realtor if accessible): beds/baths/sqft/lot/
   year, listing remarks, sale & listing history, AVMs (record value + date), rent estimates.
2. County assessor / city GIS open data: APN, zoning, parcel geometry (frontage/rear width), building height,
   tax rate area, assessments, permits.
3. Assigned schools from the school district locator: record each school's name, grades, distance and official
   state or federal data only (for example CAASPP results or the California School Dashboard, with source and
   year), and mark them "verify with district". Do not record GreatSchools or Niche ratings (public-data-only
   rule, `us-property-report/references/compliance.md`); a link to those sites is fine.
4. Comps: 6–15 sales within ~0.5 mi in the last 12–18 months, same property type, with sold date, price, beds,
   baths, sqft, lot, year built, distance; note flips, fixers, off-MLS sales, and conflicts.
5. Local context worth surfacing: noise buffers, flight paths, advisories, commercial neighbours, permit gaps.

Write the result as one JSON object following `references/facts-schema.md`. The HTTP API has no facts upload:
researched facts enter through the Rust API, as `given_facts` in `AwaitReportOptions` (passed to
`ReportJobManager::await_or_build_report`) or in `BuildReportOptions` (`build_full_report`), both in
`src/web/orchestrator.rs`. Offline unit and integration tests construct synthetic `facts` payloads procedurally via `tests/common/mod.rs` (zero static captured property fixtures).

## Derived Register, Assessment & Title/Foreclosure Profiles (`src/property/facts.rs`)
- **`buildingRegister`**: APN, legal description, asset class, zoning + conformity check, living sq ft vs satellite OSM footprint sq ft, **Lot Coverage % (BCR)**, **Floor-Area Ratio (FAR)**, lot dimensions/acres, stories/unit floor, subterranean/foundation, est. height, garage/parking, `permits[]`, and municipal building-permit portal URL.
- **`officialAssessment`**: County Assessor multi-year roll (`taxHistory` with Total, Land, Improvements, YoY %, **`landPerLotSqft`**, effective tax rate, and statutory *"for tax administration only, not market value"* disclaimer).
- **`titleAndLiens` & `foreclosure`**: Deed transfer chain, owner-occupancy/vesting, recorded voluntary mortgages, involuntary liens, easements, pre-foreclosure/NOD/trustee-sale/REO status, nearby distressed/off-MLS comp scan, and official County Recorder / HUD Homestore / Fannie HomePath / Freddie HomeSteps portals.

## Quality checks
- Footprint cross-check: the pipeline compares OSM footprint area to living area (two-story hint + BCR/FAR).
- Conflicts (e.g. two lot sizes, two sale prices, unpermitted sunroom) go into `*Note` fields and `checks[]`; pick the official/primary value.
- Dates ISO `YYYY-MM-DD`; money in USD integers; `taxRate` as a fraction (0.012592).
- Blocked sites: don't bypass bot protection or paywalls; use search snippets only as "unverified".
