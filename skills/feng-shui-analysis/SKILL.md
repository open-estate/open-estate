---
name: feng-shui-analysis
description: >-
  Automated feng shui (风水) site reading for a US house from open map data: true and magnetic facing direction
  (WMM2025 declination) from the real OpenStreetMap footprint and street, 24-mountain (二十四山) classification with
  void-line (空亡) detection, Xuan Kong Flying Star natal chart (玄空飞星, 下卦) by construction period with classical
  patterns (旺山旺向, 上山下水, 伏吟/反吟, 合十, 三般卦), current-period timeliness, annual stars, Eight Mansions
  (八宅) house trigram and door sector, personal Kua (命卦), and form-school (峦头) checks — T-junction road arrows
  (路冲), road curves (反弓/玉带), busy roads on the Dragon/Tiger sides, backing hills (靠山) and open front (明堂)
  from terrain, water, nearby sha sources (power lines, substations, cemeteries, temples, hospitals, gas stations),
  missing corners (缺角), lot shape. Use when a user asks about 风水 / fengshui / feng shui, house facing, flying
  stars or Kua for a specific address. Cultural guidance only.
---

# Feng Shui Analysis 风水

## What it does
`run_fengshui_full` in `src/modules/fengshui.rs` (Rust; flying stars, 24 mountains, Ba Zhai, Kua and the WMM2025
declination live in `src/modules/xuankong.rs`) produces `data` with:
- `orientation` — `facingTrue`, `facingMagnetic`, `declination`, `uncertaintyDeg`, facing `mountain` (zh, palace,
  `boundaryDist`, `voidLine`, `jian` 兼向), the true-north chart name, method used.
- `period` — construction period (元运) from `facts.yearBuilt` (falls back to the census-tract median year with a
  warning) and the current period (Period 9 = 2024–2043).
- `charts.primary` / `charts.alternate` — full 9-palace natal charts (`mountain`, `facing`, `base`, annual star,
  timeliness in the current period, classical pair readings). An alternate chart is produced whenever the facing is
  within the estimate's uncertainty of a mountain boundary.
- `baZhai` — 宅卦 (house trigram from the sitting direction), East/West group, 8 sectors (生气 天医 延年 伏位 /
  祸害 六煞 五鬼 绝命) and the door sector (paired in the report UI with Personal Kua `命卦` and a Four-Pillars / Five-Elements `四柱五行 · BaZi / Saju` birth-date & house elemental harmony calculator).
- `form[]` and `liqi[]` — each check has `zh`, `en`, signed `impact` and a short explanation.
- `terrain` (Open-Meteo Copernicus DEM ring out to 10 km), `micro` (front/back/left/right points 15–18 m from the
  house, from the same Open-Meteo elevation request), `diagram` (local-meter geometry for the site plan), `scores`
  {form, liqi, overall, grade}, `remedies[]`.

## How facing is estimated
1. Find the subject building: OSM building with matching `addr:housenumber` + street → else the footprint containing
   the geocoded point → else the nearest footprint (≤45 m).
2. Facing = perpendicular to the addressed street, refined by the footprint's minimum-area-rectangle edges (mean of
   both when they agree within 15°). Uncertainty = disagreement + 1.5°, min 2°.
3. Magnetic facing = true − declination (WMM2025 coefficients, validated against all NOAA test values).
4. Always tell the user to confirm with a luopan at the front door when `boundaryDist` < uncertainty.

## Flying-star algorithm (下卦)
Period star in center, flown forward along 中→乾→兑→艮→离→坎→坤→震→巽. The base numbers in the facing / sitting
palaces become the facing / mountain stars; each flies forward if the same-position (地/天/人元) mountain of that
star's home trigram is yang, else backward; a 5 borrows the facing/sitting palace's own trigram. Verified against
known charts (e.g. P8 子山午向 = 双星到向, P8 艮山坤向 = 上山下水 + 伏吟). 替卦 (replacement charts) are not computed —
flag them in prose when the facing leans >3° (兼向).

## Scoring (cultural heuristic, documented so users can disagree)
`form = 70 + Σ form impacts`, `liqi = 62 + Σ li-qi impacts` (averaged across primary/alternate charts when
ambiguous), `overall = 0.55·form + 0.45·liqi`; grade A ≥ 80, B ≥ 68, C ≥ 55, else D.

## Run
Called by the report orchestrator (`src/web/orchestrator.rs`, third wave) via `run_fengshui_full`, or standalone via
`run_fengshui` in `src/modules/fengshui.rs` (tested in `tests/livability_fengshui_test.rs`):
```rust
use open_estate::modules::fengshui::{run_fengshui, run_fengshui_full};

let fs = run_fengshui(&http, &geo, &facts, rooftop.as_ref()).await?;
// Or pass pre-fetched `amenities.data` and `market.data` to avoid refetching:
let fs = run_fengshui_full(&http, &geo, &facts, rooftop.as_ref(), Some(&amen.data), Some(&mkt.data)).await?;
```
Inputs: `geo` (`lat`/`lon` or `refined.{lat,lon}`, `matchedAddress`/`displayAddress` for footprint matching), `facts`
(`yearBuilt`, optional `lotFrontageFt`/`lotRearFt`), optional `rooftop` (pre-fetched OSM `vicinity` elements / rooftop
coordinates), and optional pre-fetched `amenities` / `market` `data` to avoid refetching.

## Presenting results
- In the Open Estate report UI (`#fengshui` / `堪舆风水`) and AI synthesis (`ai.fengShui`), render all headings, compass labels, charts, Eight Mansions (`八宅`), Personal Kua (`命卦`), Four Pillars (`四柱五行`), and cultural disclaimers **exclusively in Simplified Chinese (`zh-CN`)** regardless of the global UI language toggle; preserve backend JSON enum fields (`baZhai.group: "East"|"West"`) for schema compatibility.
- Remedies should double as practical advice (e.g. a solid fence on the busy-road side also cuts noise).
- Never let feng shui override safety/financial facts; it carries 10% weight in the overall Open Estate Score
  (`us-property-report/references/scoring.md`).

## References
`references/method.md` — 24 mountains table, polarity rules, Ba Zhai tables, Kua formula, timeliness table,
pair readings and sources (沈氏玄空学, 紫白诀, 八宅明镜).
