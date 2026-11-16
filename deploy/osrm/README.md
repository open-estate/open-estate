# Self-Hosted OSRM Routing (Docker Compose)

Open Estate uses self-hosted [OSRM](https://project-osrm.org/) (`v26.11.0`) for road drive-time and commute matrices (`src/modules/commute.rs`). It **never** calls the public `router.project-osrm.org` demo server. When `OSRM_BASE_URL` and `OSRM_REGIONS_JSON` are unset, Open Estate falls back automatically to deterministic road-distance estimates (`estimated`).

## Quick Start with Docker Compose

Run the built-in `osrm` profile from the repository root:

```bash
# Default extract: California (https://download.geofabrik.de/north-america/us/california-latest.osm.pbf)
OSRM_BASE_URL=http://osrm:5000 docker compose --profile osrm up -d
```

To build a different state or region (any Geofabrik `.osm.pbf` extract), set `OSRM_EXTRACT_URL` and `OSRM_EXTRACT_NAME` in `.env`:

```env
OSRM_BASE_URL=http://osrm:5000
OSRM_EXTRACT_URL=https://download.geofabrik.de/north-america/us/texas-latest.osm.pbf
OSRM_EXTRACT_NAME=texas-latest
```

## Refreshing the Road Graph Zero-Downtime

To download a fresh Geofabrik extract, build a new dated MLD dataset (`/data/YYYY-MM-DD`), and atomically repoint `/data/current` while `osrm` stays online:

```bash
SKIP_IF_CURRENT_EXISTS=0 docker compose --profile osrm run --rm osrm-download
SKIP_IF_CURRENT_EXISTS=0 docker compose --profile osrm run --rm osrm-build
```

`serve.sh` detects the updated `/data/current` symlink within 60 seconds and restarts `osrm-routed` automatically.

## Multi-Region Routing (`OSRM_REGIONS_JSON`)

If you run multiple regional OSRM containers (for example `osrm-west` and `osrm-east`), point `OSRM_REGIONS_JSON` at their bounding boxes (`[minLon, minLat, maxLon, maxLat]`):

```env
OSRM_REGIONS_JSON=[{"name":"us-west","bbox":[-125.0,31.0,-102.0,49.5],"baseUrl":"http://osrm-west:5000"},{"name":"us-east","bbox":[-102.0,24.0,-66.5,49.5],"baseUrl":"http://osrm-east:5000"}]
```
