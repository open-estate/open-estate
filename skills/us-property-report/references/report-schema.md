# Report schema (`report.json`)

Built by `build_full_report` in `src/web/orchestrator.rs`; served at `/r/<slug>/report.json` and inside
`GET /api/report` responses.

```text
{
  meta: { version, generatedAt, elapsedMs, timingsMs, input, displayAddress, slug, aiPending, disclaimer },
  geo:  property-geocode data + refined { lat, lon, method },   // refined = OSM rooftop footprint
  facts: property-facts data (+ checks[], disclosures),
  modules: {
    market:      { data, warnings }   // market-trends
    amenities:   { data, warnings }   // neighborhood-livability/amenities
    access:      { data, warnings }   // neighborhood-livability/access (drive & walk times)
    civic:       { data, warnings }   // neighborhood-livability/civic
    hazards:     { data, warnings }   // natural-hazards
    environment: { data, warnings }   // environmental-risk
    valuation:   { data, warnings }   // property-valuation
    fengshui:    { data, warnings }   // feng-shui-analysis
  },                                   // a failed module → { data: null, error, warnings }
  decisionFactors: { lotAndAdu, solar, safety, connectivity, closingAndTax },
  summary: {
    scores: { investment, safety, environment, livability, fengshui, overall },   // Open Estate Score
    weights, verdict, verdictTone: 'good'|'watch'|'bad'|null, badges[{k,label,value,tone}],
    findings: { strengths[], risks[], watch[] },   // deterministic, from module highlights
    climate[],                                      // 1–10 climate risk factors
    narrative: { headline, paragraphs[], upside[], downside[], verify[], source: 'deterministic'|'ai' }
  },
  ai: ai-analyst output (when an LLM is configured),
  translations: { <lang>: translated text record },   // e.g. zh, added by /api/translate or ?lang=
  sources: [{ name, url, retrieved }],
  warnings: []
}
```

Every module `data` may contain:
- `highlights: [{ level: 'good'|'watch'|'bad', text }]` — plain-English, factual, cites numbers.
- `scores: { …, overall }` — 0–100, higher = better.
- `mapLayers: [{ id, title, kind: 'points'|'lines'|'polygons', color, defaultOn?, features: GeoJSON[] }]`.

Module-specific fields are documented in each skill's SKILL.md. Payload for the example ≈ 420 KB (fine for an
HTTP response; the server compresses responses, gzip ≈ 90 KB).
