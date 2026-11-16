# Module contract (all data modules)

Every data skill is implemented as one async Rust function inside the `open-estate-server` binary: the per-address
modules in `src/modules/` (`hazards.rs`, `environment.rs`, `amenities.rs`, `access.rs`, `civic.rs`,
`fengshui.rs`) and the property modules in `src/property/` (`geocode.rs`, `facts.rs`, `market.rs`,
`valuation.rs`). The report orchestrator (`src/web/orchestrator.rs`, `build_full_report`) calls them; new modules are
declared in `src/modules/mod.rs` and wired into one of the orchestrator's waves.

## Rules
1. **One shared HTTP client.** Make every outbound request through `crate::shared::HttpClient`
   (`src/shared.rs`). It provides timeouts, retries with backoff, a per-host concurrency limit, an in-memory cache
   (plus the server's disk cache in `HTTP_CACHE_DIR`, default `.cache/http`), single-flight de-duplication of
   identical cached GETs, and the egress-proxy rotation configured by `OUTBOUND_PROXY_URLS`. Modules do not build
   their own `reqwest` clients.
2. Use the shared helpers in `src/shared.rs`:
   - HTTP: `get_json_opts`, `get_text_opts` (timeout, retries, cache TTL, headers), `get_json`
   - ArcGIS REST: `arcgis_query`, `arcgis_point_query`, `arcgis_radius_query`, `arcgis_representative_point`,
     `arcgis_to_geojson_geometry`
   - OpenStreetMap: `overpass` (multi-endpoint fallback over `OVERPASS_ENDPOINTS`), `osm_map_bbox` (tiny bbox)
   - Geometry: `haversine_m`, `haversine_mi`, `bearing_deg`, `destination_point`, `bbox_around`,
     `nearest_on_polyline`, `point_in_ring`, `ring_area_centroid`, `dominant_axis_rect`, `compass8`, `compass16`,
     `round_n`, `MILE_M`, `FOOT_M`, ...
3. **Keyless public sources first.** Optional keyed sources read keys from `EnvConfig` (e.g.
   `env.get("ATTOM_API_KEY")`, loaded from the process environment and `.env`) and are skipped silently when
   the key is absent.
4. **Never fail for a single failed source.** Run sub-queries concurrently (`tokio::join!`), handle each `Result`,
   and push a human-readable string to `warnings`. Return `Err` only when the input itself is invalid (for
   example coordinates outside WGS84 ranges).
5. **Cite everything.** Every external dataset used goes into `sources` via `SourceItem::new(name, url)`.
6. **No fabricated numbers.** If a value is unknown, return `null` and say why in `warnings`.
7. Keep payloads lean (the report JSON is embedded in a web page). Round coordinates to 6 decimals, distances to
   meters (integers) and also provide miles where user-facing.

## Signature
```rust
use crate::shared::{EnvConfig, HttpClient, ModuleOutput, SourceItem};
use anyhow::Result;
use serde_json::Value;

/// `geo` is the property-geocode `data` (state, stateFips, countyFips, county, tract, blockGroup, zip,
/// place, msa, schoolDistricts, refined {lat, lon} ...).
pub async fn run_hazards(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<ModuleOutput>;

// src/shared.rs
pub struct ModuleOutput {
    pub data: Value,              // module-specific JSON
    pub sources: Vec<SourceItem>, // { name, url, retrieved }
    pub warnings: Vec<String>,
}
```
Modules that build on other modules take their outputs as extra arguments (for example `run_access` takes the
amenities and civic `data`; `run_valuation(facts, market, geo)` is synchronous and makes no requests).

The orchestrator stores each result as `report.modules.<id> = { data, warnings }`, or
`{ data: null, error, warnings }` when the module returned `Err`, and merges `sources` into `report.sources`
(de-duplicated by name).

## Map layers
If a module has things worth drawing, add `data.mapLayers = [{ id, title, kind: 'points'|'lines'|'polygons',
color, features: GeoJSON Feature[] }]` (WGS84, properties include `name` and a short `popup` string).
The report renderer draws any such layers automatically with a toggle in the layer control.

## Offline testing
Offline unit and integration tests construct synthetic property reports and benchmark cohorts procedurally via `tests/common/mod.rs` with zero hardcoded real street addresses or captured property fixtures.
