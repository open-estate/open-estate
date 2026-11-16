---
name: property-geocode
description: >-
  Resolve a US street address to coordinates and official Census geographies with the keyless U.S. Census
  Geocoder — matched address, lat/lon, state/county/place FIPS, census tract, block group and block, ZIP/ZCTA, metro
  (CBSA), school districts (elementary/secondary/unified), congressional and state legislative districts, plus
  Census Reporter geo ids for ACS lookups. Use as the first step of any US property analysis or whenever a task needs
  the census tract, school district or FIPS codes for an address. US addresses only.
---

# Property Geocode

Implemented in `src/property/geocode.rs` (`geocode_address`, tested in `tests/property_valuation_test.rs`) and called
in the first step of `build_full_report` (`src/web/orchestrator.rs`):
```rust
use open_estate::property::geocode_address;

let out = geocode_address(&http, &address_input).await?;
// out.data: matched, displayAddress, matchedAddress, lat, lon, state, stateFips, county, countyFips,
// tract, blockGroup, block, zip, place, msa, schoolDistricts, censusReporterIds ...
```

- Primary endpoint: `https://geocoding.geo.census.gov/geocoder/geographies/onelineaddress?benchmark=Public_AR_Current&vintage=Current_Current&layers=all&format=json`
  (automatically retries with apartment/unit suffixes stripped if needed). If Census returns no match, falls back to
  Esri ArcGIS World Geocoder (`findAddressCandidates`) and attaches official Census geographies via
  `geographies/coordinates`.
- Tolerates missing state ("<city>, <5-digit ZIP>") and lowercase; returns `matched: false` with guidance when no match
  (and `GET /api/report` answers `404 ADDRESS_NOT_FOUND`).
- Census points are interpolated along the street segment (often 20–40 m off). The orchestrator refines to the
  rooftop with the OSM building footprint (`fetch_rooftop_building` in `src/property/facts.rs` using `locate_house` in
  `src/modules/fengshui.rs`), stored as `geo.refined`.
- If several Census candidates match, `warnings` records the candidate count and the module uses the first match
  (`matches[0]`); interactive callers can disambiguate via typeahead before continuing.
- **Address typeahead (`src/property/suggest.rs` → `suggest_addresses`, served at `GET /api/suggest`)**: provides
  Google-Maps-style US street autocomplete — uses Google Places API (New) when `GOOGLE_MAPS_API_KEY` is set, with
  automatic keyless parallel fallback to Esri ArcGIS World Geocoder `suggest` and OpenStreetMap Photon.
- Not supported: PO boxes, non-US addresses, very new streets (fall back to asking for coordinates).
