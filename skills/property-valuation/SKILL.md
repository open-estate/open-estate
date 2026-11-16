---
name: property-valuation
description: >-
  Transparent value estimate for a US home — repeat-sales (last sale indexed by the ZIP Zillow Home Value Index),
  time- and feature-adjusted comparable sales, ZIP bedroom-tier anchor and cited third-party AVMs — blended into a
  value range with confidence, plus upside/downside scenarios from the ZIP's historical return distribution
  (P10/P50/P90, odds of loss, worst-drawdown stress test, Zillow 12-month forecast), cost of ownership at today's
  mortgage rate (P&I, property tax incl. California Prop 13 reassessment, insurance, maintenance), rent-vs-own and
  investor metrics (gross yield, cap rate, 1% rule) and data-driven upside/downside drivers. Use when asked what a
  house is worth, whether it is overpriced, what could happen to its value, or what it costs to own. Not an appraisal.
---

# Property Valuation — value, upside, downside, cost

Builds on the CMA discipline of the *comparative-market-analysis* and *real-estate-analyzer* skills
(claudeskills.info real-estate category): comps chosen for recency/proximity/similarity, adjustments explained,
**a range not a point**, never invented sales.

## Inputs
- `ctx.facts` (from property-facts): `sqft`, `beds`, `baths`, `lotSqft`, `yearBuilt`, `lastSale`, `saleHistory[]`,
  `comps[]` (`address, soldDate, price, beds, baths, sqft, lotSqft, yearBuilt, distanceMi`), `avms[]`,
  optional `rentEstimate`, `taxRate`, `hoaMonthly`, `insuranceAnnual`.
- `ctx.market` (from market-trends): `series.zipSfr|zip` (monthly ZHVI), `zhvi.forwardReturns`, `forecast`,
  `tiers`, `rates.mort30`, `rent`, `metroMarket`, `affordability`.
- `ctx.geo.state` for tax defaults.

## Methods & weights
| Method | How | Weight |
|---|---|---|
| Repeat sales | last sale × ZHVI(now)/ZHVI(sale month) | 1.2 (≤5 yrs) → 0.8 (≤10 yrs) → 0.4 (≤20 yrs) → 0.15 (>20 yrs) |
| Comparable sales | price × time adj × (sqft ratio)^0.75 × (lot ratio)^0.12 × bath ±2.5%/bath × age ±0.2%/yr; weighted median (similarity × distance × recency) | 2.2 (≥4 comps), else 1.4 |
| ZIP bedroom tier | Zillow mid-tier ZHVI for the bedroom count | 0.35 (with direct signals), else 0.85 |
| Third-party AVMs | as published, dated | 0.8 each |
| CA Prop 13 roll fallback | when no last sale in CA: deed-era assessed roll indexed to ZHVI | 0.45 (with comps), else 1.15 |
| Calibrated ZIP ZHVI fallback | when `comps` is empty: SFR/mid ZHVI adjusted for sqft/ADU/lot | 1.05 (calibrated), else 1.0 (0.45 with other direct signals) |
| Census Tract ACS fallback | when no direct signals: tract median value indexed from 2022-06 | 0.35 |
Range half-width = clamp(1.25 × dispersion + 3%, 5%, 15%). Confidence: High (<4% dispersion, ≥3 methods),
Medium (<8%), Low.

## Scenarios
Every historical 1/3/5/10-year window of the ZIP index since 2000 → P10/P50/P90 and share of losing windows, applied
to today's estimate (fan chart). Stress test = the ZIP's worst peak-to-trough drawdown (or a -24% GFC fallback when
the full series was not supplied). Shows Zillow's ZHVF 12-month forecast when available (note: shown with scenarios
only, not part of the Open Estate Score investment formula in `us-property-report/references/scoring.md`). These are
base rates, not predictions.

## Ownership cost, Rental Comps, Active Nearby Listings & 2026 Loan Programs
- **Ownership cost**: 20% down, 30-yr fixed at the latest Freddie Mac PMMS rate (6.65% benchmark when offline); property tax = `facts.taxRate` or `market.govTaxBenchmark.effectiveTaxRate` (else 1.00% Prop 13 statutory base in CA; omitted with a warning outside CA when unavailable rather than guessed); insurance, maintenance, closing cash, buy-vs-rent breakeven, rate-sensitivity ±1.5 pts, and income needed at 33% DTI.
- **Rental / Lease Comps (`rentalComps`, `rentalSummary`)**: Geocoded nearby lease comps + ZORI + RentCast rent range + statutory US/California Civil Code § 1950.5 (AB 12) security-deposit law summary.
- **Active Nearby Listings (`activeNearby`)**: Hyperlocal active for-sale and for-rent listings within ~0.6 mi, explicitly separated from closed sales as asking prices.
- **Assessed vs Market (`assessmentVsMarket`)**: Compares County Assessor roll value against estimated market value.
- **2026 Mortgage Limits & Assistance (`loanPrograms`)**: 2026 FHFA Conforming (`$832,750` baseline / `$1,249,125` high-cost county ceiling) & FHA limits, `Conforming` / `High-Balance Conforming` / `Jumbo` classification, side-by-side Conventional 20% / Conventional 5% / FHA 3.5% / VA 0% options, and State HFA DPA / local BMR portals.

## Run
Synchronous Rust module (`src/property/valuation.rs` → `run_valuation`), called by the orchestrator
(`src/web/orchestrator.rs`, third wave) and tested in `tests/property_valuation_test.rs`:
```rust
use open_estate::property::run_valuation;

let out = run_valuation(&facts_data, &market_data, &geo_data);
```

## Interpretation tips
- If AVMs and comps diverge by >10%, explain why (condition, unpermitted space, micro-location such as an arterial).
- A recent sale far above the index (e.g. bidding war) shows up as repeat-sales > comps — say so.
- Always pair upside drivers with downside drivers; show the stress-test number.
