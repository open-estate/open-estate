//! Canonical upstream API endpoints and public-data service base URLs.

// Commercial / MLS / Property APIs
pub const RENTCAST_BASE_URL: &str = "https://api.rentcast.io/v1";
pub const ATTOM_BASE_URL: &str = "https://api.gateway.attomdata.com/propertyapi/v1.0.0";
pub const SIMPLYRETS_BASE_URL: &str = "https://api.simplyrets.com";

// U.S. Census Geocoder & TIGERweb
pub const CENSUS_GEOCODER_GEOS_BASE: &str =
    "https://geocoding.geo.census.gov/geocoder/geographies";
pub const CENSUS_BATCH_GEOCODE_URL: &str =
    "https://geocoding.geo.census.gov/geocoder/geographies/addressbatch";
pub const CENSUS_BENCHMARK_PUBLIC_AR_CURRENT: &str = "Public_AR_Current";
pub const CENSUS_VINTAGE_CURRENT_CURRENT: &str = "Current_Current";

// Esri (BYOK) / USGS / Regional Ortho / FEMA / EPA / NCES / CDE Public Services
pub const ESRI_WORLD_GEOCODE_CANDIDATES_URL: &str =
    "https://geocode.arcgis.com/arcgis/rest/services/World/GeocodeServer/findAddressCandidates";
pub const USGS_IMAGERY_ONLY_BASE_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer";
pub const USGS_IMAGERY_ONLY_TILE_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/";
pub const USGS_IMAGERY_ONLY_EXPORT_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export";
pub const USGS_IMAGERY_TOPO_BASE_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryTopo/MapServer";
pub const USGS_IMAGERY_TOPO_EXPORT_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryTopo/MapServer/export";
pub const USGS_3DEP_INFO_URL: &str = "https://www.usgs.gov/3d-elevation-program";
pub const USGS_NATIONAL_MAP_URL: &str = "https://apps.nationalmap.gov/viewer/";

// Regional high-resolution (<0.30m) public-agency orthoimagery MapServers (keyless .gov)
pub const SCC_SJ_ORTHO_2025_BASE_URL: &str =
    "https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer";
pub const SCC_SJ_ORTHO_2025_TILE_URL: &str =
    "https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/tile/";
pub const SCC_SJ_ORTHO_2025_EXPORT_URL: &str =
    "https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/export";
pub const KINGCO_ORTHO_2023_BASE_URL: &str =
    "https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer";
pub const KINGCO_ORTHO_2023_TILE_URL: &str =
    "https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer/tile/";
pub const KINGCO_ORTHO_2023_EXPORT_URL: &str =
    "https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer/export";

// Commercial / BYOK high-resolution satellite providers (proxied server-side via media_api.rs)
pub const ARCGIS_BASEMAPS_WORLD_IMAGERY_BASE_URL: &str =
    "https://ibasemaps-api.arcgis.com/arcgis/rest/services/World_Imagery/MapServer";
pub const ARCGIS_WORLD_IMAGERY_EXPORT_FALLBACK_URL: &str =
    "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/export";
pub const MAPBOX_SATELLITE_TILES_BASE_URL: &str =
    "https://api.mapbox.com/v4/mapbox.satellite";
pub const MAPBOX_SATELLITE_STATIC_BASE_URL: &str =
    "https://api.mapbox.com/styles/v1/mapbox/satellite-v9/static";
pub const GOOGLE_MAP_TILES_SESSION_URL: &str =
    "https://tile.googleapis.com/v1/createSession";
pub const GOOGLE_MAP_TILES_2D_BASE_URL: &str =
    "https://tile.googleapis.com/v1/2dtiles";
pub const GOOGLE_STATIC_MAPS_URL: &str =
    "https://maps.googleapis.com/maps/api/staticmap";
pub const USA_STRUCTURES_FEATURE_URL: &str =
    "https://services2.arcgis.com/FiaPA4ga0iQKduv3/arcgis/rest/services/USA_Structures_View/FeatureServer/0";
pub const FEMA_NSI_STRUCTURES_URL: &str = "https://nsi.sec.usace.army.mil/nsiapi/structures";

// Public Education / School Performance Endpoints
pub const CAASPP_LIST_URL: &str = "https://caaspp-elpac.ets.org/caaspp/ResearchFileListSB";
pub const CAASPP_FILES_BASE: &str = "https://caaspp-elpac.ets.org/caaspp/researchfiles/";
pub const CDE_DIRECTORY_URL: &str =
    "https://www.cde.ca.gov/schooldirectory/report?rid=dl1&tp=txt";
pub const EDFACTS_INFO_URL: &str =
    "https://www.ed.gov/about/inits/ed/edfacts/data-files/school-status-data.html";

// Open Street-Level / Geosearch Imagery
pub const DEFAULT_MAPILLARY_API_BASE: &str = "https://graph.mapillary.com";
pub const DEFAULT_WIKIMEDIA_API_URL: &str = "https://commons.wikimedia.org/w/api.php";
