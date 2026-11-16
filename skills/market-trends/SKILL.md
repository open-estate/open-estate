---
name: market-trends
description: >-
  Local housing-market context for a US address: Zillow Home Value Index history for the ZIP (all homes,
  single-family, price tiers, bedroom counts), metro and U.S. comparisons, appreciation (YoY, 3/5/10/20-yr CAGR),
  peak/drawdown/recovery statistics, historical forward-return distributions, Zillow 12-month forecast, observed
  rents (ZORI) and price-to-rent, metro market temperature (days to pending, sale-to-list, price cuts, inventory,
  market heat index), Freddie Mac mortgage rates, FHFA house-price indexes back to 1975, and ACS neighborhood
  economics (income, home values, rents, ownership, commute, education, broadband) for tract/ZIP/city/county/metro/
  state/US. Use for "how is the market here", appreciation history, affordability, or as input to valuation.
---

# Market Trends

Implementation (Rust): `src/property/zillow.rs` (`load_zillow_for_zip`) extracts the Zillow series for one ZIP,
its metro and the U.S.; `src/property/market.rs` (`run_market_trends`) adds rates, FHFA, ACS and the statistics.
The report orchestrator runs both in its first wave and stores the result as `report.modules.market`.

## Data (all keyless)
| Data | Source | Access |
|---|---|---|
| ZHVI, ZORI, ZHVF forecast, metro market metrics | Zillow Research public CSVs | `load_zillow_for_zip` reads the CSVs from the data directory (`--data-dir` / `DATA_DIR`, default `.cache/datasets`) when present, otherwise streams the public files (the large bedroom and price-tier files are used only when on disk); it keeps a per-ZIP JSON cache (`zillow-zip-<zip>.json`, 14 days) and an in-memory cache |
| 30/15-yr mortgage rates | Freddie Mac PMMS via FRED `fredgraph.csv` | live |
| FHFA All-Transactions HPI (MSA, state, US) | FRED `ATNHPIUS{CBSA}Q`, `{ST}STHPI`, `USSTHPI` | live |
| ACS 5-year (B19013, B25077, B25064, B25003, B25035, B01003, B01002, B15003, B08303, B08013, B08301, B25002, B25024, B28002) | Census Reporter API | live (api.census.gov now requires a key) |

Zillow files use the long CBSA name in ZIP files and short names in metro files — the extractor matches both.

## Run
Called by the orchestrator for every report (`GET /api/report?address=...`; the result is under
`report.modules.market.data`). From Rust:
```rust
use open_estate::property::{load_zillow_for_zip, run_market_trends};

let zillow = load_zillow_for_zip(&http, &zip, cbsa.as_deref(), Some(Path::new(".cache/datasets"))).await?;
let market = run_market_trends(&http, &env, &geo, &zillow).await?;   // ModuleOutput { data, sources, warnings }
```
To refresh the Zillow data, replace the CSVs in the data directory (for example with a monthly job); per-ZIP
caches expire after 14 days.

## Output highlights
`zhvi` (stats incl. `forwardReturns`, `maxDrawdown`), `zhviSfr`, `metroZhvi`, `usZhvi`, `tiers`, `bedroomYoY`,
`rent`, `forecast`, `metroMarket` (+ `label` from the market heat index: ≥70 strong seller's … ≤28 strong buyer's),
`rates`, `fhfa`, `acs.geos[]`, `affordability`, `series` (chart-ready monthly arrays).

## Caveats
ZHVI is a smoothed "typical home" index, not a sale price; small ZIPs can be noisy. ACS 5-year estimates carry
margins of error. Never present demographic composition (race/ethnicity/religion/familial status) — not collected.
