# Compliance & integrity rules

Distilled from the claudeskills.info real-estate skills (proptech-advisor, property-listing,
tenant-screening-guide, comparative-market-analysis, real-estate-expert) and US practice.

## Fair Housing Act (and state equivalents)
- Describe the **property and its surroundings**, never the people. No race, color, religion, national origin,
  sex, familial status or disability — not as data, not as adjectives ("family neighborhood", "exclusive").
- No "safe/unsafe", "good/bad neighborhood" labels; crime is presented only as agency-wide rates with caveats.
- Schools: cite public test data as facts with source and year. School numbers never enter the Open Estate Score;
  in Your Match they count only under the rules below.
- Do not rank or filter properties by demographic proxies (disparate-impact risk flagged by proptech-advisor).

## Two scores: Open Estate Score and Your Match
- **Two-score rule.** The Open Estate Score is objective: one fixed formula, identical for every buyer and in every
  language (`scoring.md`). Your Match is personal: it applies the weights, must-haves and deal-breakers the buyer
  declared. Keep them separate, label which one a number is, and never blend them. Both are computed
  deterministically; the LLM may explain a score but never sets one.
- **Schools and crime affect only Your Match, and only when the buyer gives them weight.** Neither enters the
  Open Estate Score. With a zero weight they change nothing in Your Match either.
- **School numbers come only from public state and federal data** (for example the California School Dashboard,
  CAASPP results, NCES). GreatSchools, Niche and other commercial ratings are never stored, scored or shown as
  Open Estate numbers; links to those sites are fine. Every school number shows its source and year.
- **No resident data in any factor.** No factor in either score describes the people who live somewhere: race,
  ethnicity, national origin, religion, sex, familial status, disability, age, income or education.
- **Buyer photos are analyzed for the building only and never stored.** Photos a buyer uploads are described for
  the house (style, finish level), never for people; images are processed in memory and discarded, and only the
  labels are kept.
- **Presets are lifestyle-based and identical in every language.** A preset ("Commute-first", "Investor", …)
  describes a way of living, never a group of people. Language, name and location never change the default
  weights: the same preset has the same weights in English, 中文 and Español.

## Accuracy
- Never invent comps, prices, permits or facts; unknown → `null` + warning; conflicts → noted.
- Values are ranges with methods shown; always "not an appraisal".
- Separate measured data (FEMA zone) from modeled data (CAMS air quality, BTS noise) and from tradition (feng shui).
- Date-stamp market data and AVMs.

## Regulated advice
- Not legal, tax, insurance or investment advice; recommend licensed professionals (inspector, appraiser, attorney,
  insurance broker, NHD provider).
- RESPA: no referral fees or steering to specific lenders/inspectors/insurers in the report.
- MLS data: don't scrape listing portals at scale or redistribute MLS content; for production use licensed feeds
  (RESO Web API / IDX via a broker, SimplyRETS, ATTOM) and honour their display rules.

## Data-provider terms
- OSM/Overpass: attribute "© OpenStreetMap contributors", cache, keep queries small (ODbL).
- Zillow Research data: free with attribution. FEMA/USGS/EPA/Census: public domain, cite.
- Open-Meteo free tier is non-commercial; use a paid key for a commercial deployment.
- Never bypass bot protection, paywalls or CAPTCHAs; use official APIs or document the gap.
