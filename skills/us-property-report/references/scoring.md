# Scoring (all 0–100, higher = better)

Open Estate shows two scores. The **Open Estate Score** below is objective and identical for everyone.
**Your Match** (last section) is personal. Rules for both: `compliance.md`.

## Open Estate Score

Computed by `summarize_report` in `src/web/summary.rs` and stored in `report.summary`.

| Score | Formula | Notes |
|---|---|---|
| Investment outlook | `round(clamp(55 + (CAGR10 − 4)·5 + YoY·0.8 + (grossYield − 4.5)·4 − max(0, priceToIncome − 6)·1.8, 0, 100))` | Inputs: `market.zhvi.cagr10` (or `cagr10y`; default 4), `market.zhvi.yoy` (default 0), gross yield % from `valuation.ownership.grossYieldPct`, else `valuation.investorMetrics.grossYieldPct`, else `market.rent.grossYieldPct` (default 4.0), `market.affordability.priceToIncome` (default 5.0). Computed only when `market.zhvi` exists. The Zillow 12-month forecast is shown with the valuation scenarios but is not scored. Market-level, not property-level: a coastal-metro home with high long-run growth but poor yield and stretched affordability lands mid-pack. |
| Hazard safety | natural-hazards `scores.overall` (flood, earthquake, wildfire, other) | Rubric in natural-hazards/SKILL.md. |
| Environment | environmental-risk `scores.overall` (contamination 35%, air 25%, water 20%, noise 20%) | Rubric in environmental-risk/SKILL.md. |
| Livability | amenity access (`amenities.accessScore`) 60% + walkability (`civic.scores.walkability`, else EPA index ÷ 20 × 100) 40% | Re-weighted when one part is missing. Destination proximity only; schools and crime are shown but never enter the Open Estate Score. |
| Feng shui | feng-shui-analysis `scores.overall` | Cultural heuristic; 10% weight. |
| **Overall** | weighted mean of the available scores: investment 30%, safety 25%, livability 20%, environment 15%, feng shui 10% | Missing modules are dropped and the weights renormalized; rounded to an integer. |

Verdict bands (`summary.verdict`, with `summary.verdictTone` for the color of the report's verdict pill):

| Overall | Verdict | Tone |
|---|---|---|
| ≥ 75 | "Strong overall profile" | `good` |
| 62–74 | "Solid, with specific risks to price in" | `watch` |
| 50–61 | "Mixed — proceed with diligence" | `watch` |
| < 50 | "High-risk profile" | `bad` |
| no score | `null` | `null` |

Version note: reports generated before the Rust server
stored investment scores that also added `forecast12m × 1.5`, so their stored `summary` can be a point or two
higher than a recomputation.

Scores are screening aids for comparing homes, not predictions or ratings of a neighborhood's residents.

## Your Match (personal)

Your Match answers "how well does this home fit what I said I want?". It is computed by the deterministic
engine in `src/scoring/` (Your Match plan, section 5.4); the server computes utilities, the browser only
re-weights them, and the LLM never sets a number. Weights come only from the buyer (quiz, a lifestyle preset or
sliders), are shown as percentages summing to 100, and cover eleven dimensions: cost, schools, commute and my
places, daily convenience, home fit, style and character, natural hazards, crime, environment and quiet,
investment and resale, feng shui and orientation.

1. **Utilities.** Each sub-factor maps to 0–100 through a documented curve (v0, to be calibrated):

   | Factor | Curve |
   |---|---|
   | Monthly cost ÷ budget | 100 at 0.80 or less, 70 at 1.00, 0 at 1.25 |
   | Commute minutes (free-flow × traffic buffer, default 1.3) vs. the buyer's maximum T | 100 at half of T, 60 at T, 0 at 1.75 × T |
   | Beds or baths vs. need | 100 if one more than needed, 85 if equal, 40 if one short, else 0 |
   | Size and lot vs. target | Linear up to the target, capped at 100 |
   | Effective age | 100 at 5 years, 75 at 20, 50 at 50, 30 at 80 |
   | Schools | Within-state percentile of the assigned school from public data, averaged over the levels the buyer selected; until that metric ships, California Dashboard academic tiers × 20 |
   | Style | 100 × Σ (probability of each style × preference), with love = 1.0, fine = 0.6, avoid = 0 |
   | Crime | 100 − 34 × (0.6 × violent rate ÷ national + 0.4 × property rate ÷ national), kept between 20 and 96 (the formula behind `decisionFactors.safety.agencySafetyScore`) |

2. **Dimension score** = weighted mean of the dimension's available sub-factors.
3. **Match score** = Σ (weight × dimension score) ÷ Σ weight.
4. **Gates sit outside the sum.** A failed must-have moves the home to a "near misses" list with the reason. A
   deal-breaker flags the home and hides it by default. A dimension marked critical that scores under 40 caps the
   total below "Strong Match".
5. **Coverage.** A missing factor takes the median of the search and is labelled "estimated". Each home shows its
   coverage, the share of weight backed by real data. If no home in the search has data for a dimension, that
   dimension is dropped and the buyer is told why.
6. **Explanations are computed, not written.** The top three reasons and top two trade-offs are the largest
   weighted differences from the median of the search, rendered from templates in each language.

Schools and crime count only when the buyer gives them weight, and nothing that describes residents is ever a
factor.
