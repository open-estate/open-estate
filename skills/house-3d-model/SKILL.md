---
name: house-3d-model
description: >-
  Interactive 3D model of a US home and its lot, built automatically from public data: roof heights and shape, terrain
  and individual trees measured by USGS 3DEP airborne LiDAR (keyless, public domain), building footprints from
  OpenStreetMap (or traced from the LiDAR building class where the map has none), a clean white "clay" model with roofs
  drawn as fitted flat faces, measurements shown on the model, aerial imagery on the ground, a sun-and-shadow study for
  any time and season, and the listing's own Matterport / Zillow 3D Home /
  iGUIDE walkthrough embedded live when the listing has one. Use when a user asks to "see the house in 3D", for roof
  height / pitch / roof faces (solar), lot slope, tree height or overhang, shadows, or wants the listing's 3D tour next
  to the exterior model. Exterior massing only — not a survey; interiors are never reconstructed.
---

# House 3D Model

The report page's **3D model** section (`#house3d`) and the `GET /api/house3d/:slug` endpoint behind it.

## What it shows
- **The home as a clean architectural model** — a white "clay" model on the aerial ground: this home in a restrained
  colour, neighbours matte white, thin edge lines, soft ambient shading and a soft contact shadow. Roofs are flat faces
  with straight ridge, hip and valley lines, drawn from planes fitted to the survey (`roofFit`); the raw surveyed
  surface is never drawn. A building whose roof could not be fitted (sparse or noisy survey, tree cover) or that has no
  survey is a flat-topped block at its measured (or estimated) height, and the UI says "simplified". No windows, doors
  or siding are drawn: there is no data for them. The aerial photo stays on the ground; the **Photo on roofs** chip
  drapes it back on the roofs.
- **Outline legend** — the same as the satellite map at the top of the report: a yellow roof-edge outline for the
  home, magenta for a garage / ADU on the lot, and a sky-blue dashed lot line (the **Outlines** and **Lot line** chips
  toggle them).
- **Trees that never hide the home** — simple symbols scaled to each tree's measured height and crown width (three
  outline styles; no species is guessed). A tree standing between the camera and the home, or over its roof in the top
  view, fades to about 20% opacity in every preset and while orbiting; faded trees keep casting their shadow.
- **Measurements on the model**, one kind at a time (**Measure on the model** chips; a kind with no data is left out):
  **Heights** (ridge and eave on a dimension pole, marked "est." when not measured), **Roof faces** (area and pitch of
  each face with a downslope arrow; faces under 20 sq ft are not labelled), **Lot edges** (edges over 5 ft, plus the
  ground slope), **Neighbor gap** (to the nearest neighbouring building) and **Tallest tree**. Tapping a roof face
  highlights it and shows that face's area, pitch and aspect. The measurement cards under the model remain.
- **Opening view** — a three-quarter view from the street side, on the sunlit side when there is one, in late-morning
  light (10:30 am) on today's date; a chip on the model says so and **Now** in the sun bar switches to the current
  time. The view sways slowly until first touched (not under `prefers-reduced-motion`, and only while on screen).
- **Sun study** — time-of-day slider, solstice/equinox presets and a "play the day" button, in the property's own
  time zone, with real shadows from the house, neighbours and trees.
- **Views** — 3D (the opening three-quarter view), Top, Front, Back; drag / pinch / scroll controls; tap a roof face,
  building or tree for its numbers; full screen; print snapshot.
- **Interior 3D tour** — when the listing record carries a tour link (RESO `VirtualTourURLUnbranded`, SimplyRETS
  `virtualTourUrl`, or `facts.virtualTours` from researched facts) it is offered click-to-load through the host's own
  embedded player. A visitor can also paste a link, which stays in their browser only.

## How the model is built (`src/modules/house3d/`)
| File | Role |
|---|---|
| `report_input.rs` | Maps a stored report onto scene inputs: house centroid, subject / accessory / neighbour footprints, parcel, streets, facing, storeys, year built, reference elevation (same local frame as the feng-shui site diagram). |
| `lidar.rs` | Finds the newest 3DEP survey covering the point (Hobu `resources.geojson` index, cached under `DATA_DIR/lidar/`), walks the Entwine Point Tile octree in the `usgs-lidar-public` AWS Open Data bucket, downloads ~25 LAZ nodes (5–10 MB) and decodes them (`las` + `laz`). Bulk open data goes out directly, not through the proxy tiers, unless `PROXY_POOL_STRICT` / `OUTBOUND_PROXY_STRICT` is set explicitly. |
| `patch_cache.rs` | Disk cache of each complete LiDAR window (`DATA_DIR/lidar/patches/`, columnar + gzip, ~1–3 MB per home, LRU-bounded by `HOUSE3D_LIDAR_CACHE_MB`): a home's survey is downloaded once, however often its scene is rebuilt. |
| `scene.rs` | Pure, deterministic raster pipeline (0.5 m grid): vertical-unit check against the DEM, ground model, building mask, footprint alignment to the surveyed roofs, outlines for unmapped structures, per-building roof height grids, roof form / pitch / faces, fitted roof planes (`fit_roof_planes`), tree detection, terrain grid, site metrics → compact scene JSON. |
| `mod.rs` | `build_house3d` / `build_house3d_with_budget` (LiDAR lookup + scene), cache packing, input fingerprint. |

Client: `assets/house3d.js` (inlined by `render_report_html` at `__HOUSE3D_JS__`). It draws an instant footprint-only
model from the embedded report and asks for the measured scene as soon as the page is idle — one request per page,
shared with the hero's 3D view (`OpenEstateHouse3D.requestScene` / `onScene`), polling while the server answers
`202` — so the roofs are usually ready before anyone scrolls to the section. three.js is loaded from a CDN when the
section scrolls into view. Opened as a standalone file it keeps the footprint model.

## Accuracy rules
- Every height carries its source: `lidar` (measured), `osm` (mapped tag), `levels` (storey count × 2.9 m) or
  `assumed`. The UI shows the source next to each number and badges the model "Measured" or "estimated".
- A survey flown before the home was built is never used for that home's roof (warning shown).
- The LiDAR vertical unit is verified against the report's independent ground elevation; a mismatch is reported, not
  silently corrected.
- Mapped footprints are only slid (never reshaped) onto the surveyed roof; when the map covers a roof only partly, the
  LiDAR outline replaces it and is labelled as such.
- Listing tours are embedded only from an allow-list of tour hosts, with the embed URL rebuilt from a validated id and
  the frame sandboxed. Tour content is never downloaded, stored or altered.

## API
`GET /api/house3d/:slug` → `{ ok, slug, scene, warnings, sources, settled, cached }`. Same access control as the
report (`authorize_and_resolve_report`). Builds run detached from the request, one per home at a time; a request waits
up to 12 s and otherwise answers `202 { ok, pending: true, retryAfterMs }` while the build finishes in the background
(the page polls). Interactive report builds start the build as soon as the report is saved
(`house3d_api::spawn_scene_warmup`), so the first view is normally served from cache; batch scans never run it.

Caching, fastest first: the browser (`ETag`, revalidated with `If-None-Match` → `304`; `private, max-age=300`) →
an in-process copy for 5 minutes → the key-value store for 60 days under a key derived from the scene inputs (only
10 minutes when a covering survey could not be read, `settled: false`, so a later view retries; the in-memory copy
honours that TTL) → the LiDAR window on disk (`patch_cache`), so even a rebuilt scene (refreshed report, new
`SCENE_VERSION`) never downloads the survey again.

Scene JSON (`SCENE_VERSION` in `scene.rs`): `quality` (`lidar` | `footprint` | `none`), `origin`, `facingDeg`,
`lidar` {project, surveyYear, densityPerM2, class flags, verticalCheck}, `terrain` {half, step, n, z[cm]},
`buildings[]` {role, ring, base, eave, ridge, meanHeight, heightSource, roofShape, pitchDeg, roof{step, rot, ox, oy, w,
h, z[cm]}, roofPlanes[], roofFit, outline, fit, alignedM}, `subject`, `trees[]` `[e, n, height, crownRadius, groundZ]`, `lot`, `streets[]`,
`metrics`.

`roofFit` (version 2; absent when planes do not describe the roof): `planes[]` {`a`, `b`, `c` — height above the
building base at lattice node (i, j) of `roof` is `a·i + b·j + c` —, `areaM2`, `pitchDeg`, `aspectDeg`, `aspect`,
`cu`, `cv` (label anchor), `n`}, `labels` (run-length pairs `[plane, count, …]` over the `w × h` lattice, `-1` = no
plane), `pairs` (`[i, j, rule]` for faces that meet along a crease: `0` ridge / hip — the lower plane owns a point,
`1` valley — the higher one), `inlier`, `rmseM`. The page cuts each lattice cell on the planes' intersection lines with
the same rule the server uses to assign nodes (`crease_winner`), which is what makes ridges and hips straight. The
`roof` height grid is still sent for other consumers (the hero's 3D view) but is no longer drawn here.

## Settings
- `HOUSE3D_LIDAR=off` — skip the LiDAR lookup (footprint model with labelled estimates only).
- `HOUSE3D_LIDAR_CACHE_MB` — disk budget for cached LiDAR windows (default `2048`; `0` turns the window cache off).
- LiDAR tiles are bulk public-domain downloads and go out directly (5–70 MB per home through free proxies is slow
  and often runs out of time); set `PROXY_POOL_STRICT=true` (or `OUTBOUND_PROXY_STRICT=true`) explicitly to route
  them through the egress pool like other data-plane requests.

## Tests
`tests/house3d_test.rs` (offline): fixture mapping, synthetic survey over a real footprint, API build / cache / ETag /
validation / access-control parity, background warm-up, template contract, and `assets/house3d.js` under Node (tour
allow-list, sun and time-zone helpers, browser/server footprint parity, the shared `202` → poll → scene request and
the outline legend, roof faces cut from fitted planes, the simplified-block fallback, tree fading, the measurement
layers and the opening state). `live_lidar_window_is_downloaded_once_then_read_from_disk` is `#[ignore]`d (needs network).
Unit tests live beside the code in `src/modules/house3d/` (including the disk window cache).

## Limits
Exterior massing, not a survey. Walls are plain (no public façade imagery). Roof detail is limited by the survey's
point density and date (faces under about 20 sq ft, chimneys and vents are absorbed into the face around them; a roof
that planes cannot describe is drawn as a simplified block); additions after the survey year are not shown. Without a mapped footprint, closely spaced
houses joined by a patio cover or touching eaves can merge into one LiDAR outline.
