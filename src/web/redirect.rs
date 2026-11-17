//! Dynamic Reference Redirect & Click Telemetry Layer (`GET /redirect` & `GET /api/ops/telemetry/redirects`).
//!
//! Routes external property/extension/module reference links through `/redirect?type=extension_reference&data_id=...`
//! so that:
//! 1. Upstream URL template updates in `extensions/*/manifest.yaml` or canonical agency resolvers immediately
//!    heal all previously cached reports without regenerating them.
//! 2. Legacy broken URLs (`sccassessor.org`, disabled ArcGIS `f=html`, retired EJScreen/NFHL/SDWIS links,
//!    and bogus `#:~:text=` fragments) are transparently upgraded on click.
//! 3. Aggregate click telemetry (`reference_redirect_clicks`) records which data sources buyers inspect most,
//!    with strict open-redirect defense against untrusted domains, non-HTTPS schemes, and private IPs.

use crate::db::metering_store::{
    query_top_reference_redirect_clicks, record_reference_redirect_click,
};
use crate::extensions::proto::ExtensionQueryProto;
use crate::guardrails::apply_security_headers;
use crate::property::public_records::{parse_street, resolve_official_record_links};
use crate::shared::{now_iso, with_verified_data_anchor};
use crate::web::hooks::ReferenceRedirectEvent;
use crate::web::http_util::json_response;
use crate::web::server::AppState;
use axum::{
    extract::{Query, State},
    http::{header, Extensions, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::IpAddr;
use url::Url;

/// Query contract for `GET /redirect`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RedirectQuery {
    #[serde(rename = "type", alias = "link_type")]
    pub link_type: Option<String>,
    pub data_id: Option<String>,
    pub slug: Option<String>,
    pub section: Option<String>,
    pub rule_id: Option<String>,
    pub apn: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub zip: Option<String>,
    pub state: Option<String>,
    pub county_fips: Option<String>,
    pub county: Option<String>,
    pub city: Option<String>,
    pub tract: Option<String>,
    pub pwsid: Option<String>,
    pub fid: Option<String>,
    pub nces_id: Option<String>,
    pub address: Option<String>,
    #[serde(rename = "ref", alias = "url", alias = "fallback_url")]
    pub fallback_ref: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RedirectTelemetryQuery {
    pub limit: Option<i64>,
}

/// Trusted non-`.gov`/`.us`/`.edu`/`.mil` host suffixes allowed as redirect destinations.
const TRUSTED_REDIRECT_SUFFIXES: &[&str] = &[
    "telleronline.net",
    "mgoconnect.org",
    "lynxgis.com",
    "pedp-ejscreen.azurewebsites.net",
    "open-meteo.com",
    "openstreetmap.org",
    "arcgis.com",
    "arcgisonline.com",
    "opendata.arcgis.com",
    "censusreporter.org",
    "stlouisfed.org",
    "freddiemac.com",
    "fanniemae.com",
    "propublica.org",
    "courtlistener.com",
    "shovels.ai",
    "openei.org",
    "openrouteservice.org",
    "freeroute.org",
    "project-osrm.org",
    "ipwho.is",
    "ipwhois.io",
    "socrata.com",
    "accela.com",
    "carto.com",
    "greatschools.org",
    "niche.com",
    "caschooldashboard.org",
    "pge.com",
    "sce.com",
    "sdge.com",
    "socalgas.com",
    "ladwp.com",
    "smud.org",
    "ebmud.com",
    "sjwater.com",
    "valleywater.org",
    "bawsca.org",
    "saveourwater.com",
    "socalwatersmart.com",
    "westvalleyrecycles.com",
    "sanjosecleanenergy.org",
    "svcleanenergy.org",
    "siliconvalleypower.com",
    "alamedamp.com",
    "burbankwaterandpower.com",
    "glendalewaterandpower.com",
    "cityofpasadena.net",
    "anaheim.net",
    "pacificpower.net",
    "portlandgeneral.com",
    "rockymountainpower.net",
    "nwnatural.com",
    "pse.com",
    "snopud.com",
    "mytpu.org",
    "aps.com",
    "srpnet.com",
    "nvenergy.com",
    "swgas.com",
    "snwa.com",
    "amwua.org",
    "abcwua.org",
    "utahwatersavers.com",
    "austinenergy.com",
    "cpsenergy.com",
    "atmosenergy.com",
    "centerpointenergy.com",
    "texasgasservice.com",
    "powertochoose.org",
    "fpl.com",
    "duke-energy.com",
    "tampaelectric.com",
    "ouc.com",
    "jea.com",
    "georgiapower.com",
    "dominionenergy.com",
    "tva.com",
    "comed.com",
    "nicorgas.com",
    "dteenergy.com",
    "coned.com",
    "nationalgridus.com",
    "eversource.com",
    "psegliny.com",
    "pseg.com",
    "pepco.com",
    "papowerswitch.com",
    "chugachelectric.com",
    "hawaiianelectric.com",
    "epelectric.com",
    "xcelenergy.com",
    "xfinity.com",
    "att.com",
    "verizon.com",
    "t-mobile.com",
    "spectrum.com",
    "cox.com",
    "frontier.com",
    "optimum.com",
    "ziplyfiber.com",
    "quantumfiber.com",
    "starlink.com",
    "earthlink.net",
    "sonic.com",
    "earthquakeauthority.com",
    "naic.org",
    "alta.org",
    "tirsa.org",
    "caionline.org",
    "uniformlaws.org",
    "pria.us",
    "iccsafe.org",
    "nahb.org",
    "jlconline.com",
    "nass.org",
    "taxadmin.org",
    "myfloridalicense.com",
    "floridarevenue.com",
    "masslandrecords.com",
    "richmondcountyclerk.com",
    "ocrecorder.com",
    "ocgis.com",
    "acassessor.org",
    "acgov.org",
    "smcgov.org",
    "kingcounty.com",
    "cookcountyassessor.com",
    "cookcountytreasurer.com",
    "cityofchicago.org",
    "opendataphilly.org",
    "portlandmaps.com",
    "sfgov.org",
    "sfassessor.org",
    "sfplanninggis.org",
    "sftreasurer.org",
    "sjpermits.org",
    "lacity.org",
    "dallascityhall.com",
    "houstonpermittingcenter.org",
    "mybuildingpermit.com",
    "prodigycad.com",
    "county-taxes.com",
    "saccounty.net",
    "smgov.net",
    "ssf.net",
    "cityofberkeley.info",
    "burlingame.org",
    "cityofsanmateo.org",
    "cityofconcord.org",
    "cityofepa.org",
    "cityofirvine.org",
    "cityofmillvalley.org",
    "cityofsanrafael.org",
    "coralgables.com",
    "dalycity.org",
    "denvergov.org",
    "homesteps.com",
    "miamigov.com",
    "njportal.com",
    "redwoodcity.org",
    "santa-ana.org",
    "sdttc.com",
    "walnut-creek.org",
    "sunbiz.org",
    "zillow.com",
    "redfin.com",
    "realtor.com",
    "homes.com",
    "apartments.com",
    "hotpads.com",
    "craigslist.org",
    "reso.org",
    "google.com",
    "github.io",
    "tccsearch.org",
    "smcacre.org",
    "viewpointcloud.com",
    "wikipedia.org",
    "bing.com",
    "hud4.my.site.com",
    "municode.com",
    "cfpnet.com",
    "nachi.org",
    "nacrc.org",
    "astm.org",
    "ulc.org",
    "ncsha.org",
    "ncsl.org",
    "publicpower.org",
    "floir.com",
    "crsecurepayment.com",
    "marinmap.org",
    "marincounty.org",
    "hctx.net",
    "traviscad.org",
    "oakgov.com",
    "cagisonline.hamilton-co.org",
    "wprdc.org",
    "horrycounty.org",
    "matterport.com",
    "youriguide.com",
];

fn clean_opt(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn format_scc_apn_dashed(raw_apn: &str) -> String {
    let digits: String = raw_apn.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 8 {
        format!("{}-{}-{}", &digits[..3], &digits[3..5], &digits[5..8])
    } else {
        raw_apn.trim().to_string()
    }
}

fn resolve_census_tract_11(tract_raw: Option<&str>, county_fips: Option<&str>) -> Option<String> {
    let raw = tract_raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(stripped) = raw.strip_prefix("14000US") {
        if stripped.len() == 11 && stripped.chars().all(|c| c.is_ascii_digit()) {
            return Some(stripped.to_string());
        }
    }
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 11 {
        return Some(digits);
    }
    if digits.len() == 6 {
        let cf: String = county_fips
            .unwrap_or("")
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if cf.len() == 5 {
            return Some(format!("{cf}{digits}"));
        }
    }
    None
}

/// Validates that `candidate` is a strict `https://` URL on an authoritative public/agency/extension host,
/// blocking open redirects, non-HTTPS schemes, credentials, custom ports, and private/literal IPs.
pub fn validate_allowed_redirect_target(candidate: &str, extra_hosts: &[String]) -> Option<String> {
    let trimmed = candidate.trim();
    if !trimmed.starts_with("https://") || trimmed.len() > 2048 {
        return None;
    }
    let parsed = Url::parse(trimmed).ok()?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    if let Some(port) = parsed.port() {
        if port != 443 {
            return None;
        }
    }
    let host = parsed.host_str()?.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".invalid")
        || host.ends_with(".example")
        || host == "example.com"
        || host.ends_with(".example.com")
        || host == "example.org"
        || host.ends_with(".example.org")
        || host.parse::<IpAddr>().is_ok()
        || (host.starts_with('[') && host.ends_with(']'))
    {
        return None;
    }

    let gov_or_edu = host.ends_with(".gov")
        || host.ends_with(".us")
        || host.ends_with(".edu")
        || host.ends_with(".mil");
    let in_static_allowlist = TRUSTED_REDIRECT_SUFFIXES
        .iter()
        .any(|suf| host == *suf || host.ends_with(&format!(".{suf}")));
    let in_ext_hosts = extra_hosts
        .iter()
        .any(|eh| host == *eh || host.ends_with(&format!(".{eh}")));

    if !(gov_or_edu || in_static_allowlist || in_ext_hosts) {
        return None;
    }

    Some(with_verified_data_anchor(parsed.as_str(), None, None))
}

/// Upgrades retired/broken portal, GIS, and agency URLs embedded in legacy cached reports.
pub fn upgrade_legacy_reference_url(raw_url: &str, q: &RedirectQuery) -> String {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let before_hash = trimmed.split('#').next().unwrap_or(trimmed).trim();
    let low = before_hash.to_ascii_lowercase();

    if low.contains("sccassessor.org") || low.contains("payments.sccgov.org/propertytax") {
        if let Some(apn) = clean_opt(&q.apn) {
            let dashed = format_scc_apn_dashed(&apn);
            return format!(
                "https://santaclaracounty.telleronline.net/search/1/details?APN={}",
                urlencoding::encode(&dashed)
            );
        }
        return "https://santaclaracounty.telleronline.net/search/1".to_string();
    }
    if low.contains("clerkrecorder.sccgov.org") {
        return "https://clerkrecorder.santaclaracounty.gov/".to_string();
    }
    if low.contains("gis.campbellca.gov/arcgis/rest/services/mgo") {
        return "https://cityofcampbell.maps.arcgis.com/apps/instant/portfolio/index.html?appid=94865b6a1fbc49c293c0d854ee9d7593".to_string();
    }
    if low.contains("gis.campbellca.gov")
        && (low.contains("/arcgis/rest/services/") || low.contains("f=html") || before_hash.ends_with("gis.campbellca.gov") || before_hash.ends_with("gis.campbellca.gov/"))
    {
        return "https://gis.campbellca.gov/portal/apps/experiencebuilder/experience/?id=a9b10db4e5f04070ae15c3f745443cbf".to_string();
    }
    if low.contains("campbellca.gov/161/building") {
        return "https://www.mgoconnect.org/cp?JID=187".to_string();
    }
    if low.contains("campbellca.gov/183/zoning") || low.contains("campbellca.gov/185/zoning") {
        return "https://www.campbellca.gov/166/Zoning-and-Land-Use".to_string();
    }
    if low.contains("campbellca.gov/181/planning") {
        return "https://www.campbellca.gov/163/Planning".to_string();
    }
    if low.contains("campbellca.gov/199/code-enforcement") {
        return "https://www.campbellca.gov/134/Code-Enforcement".to_string();
    }
    if low.contains("epa.gov/ejscreen") {
        return "https://pedp-ejscreen.azurewebsites.net/".to_string();
    }
    if low.contains("bts.gov/geospatial/national-transportation-noise-map") {
        return "https://maps.dot.gov/BTS/NationalTransportationNoiseMap/".to_string();
    }
    if low.contains("enviro.epa.gov/envirofacts/sdwis") {
        let parsed_pwsid = Url::parse(before_hash)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k.eq_ignore_ascii_case("pwsid"))
                    .map(|(_, v)| v.into_owned())
            })
            .or_else(|| clean_opt(&q.pwsid));
        if let Some(id) = parsed_pwsid.filter(|s| !s.trim().is_empty()) {
            return format!(
                "https://echo.epa.gov/detailed-facility-report?fid={}&sys=SDWIS",
                urlencoding::encode(id.trim())
            );
        }
        return "https://echo.epa.gov/".to_string();
    }
    if low.contains("air-quality-api.open-meteo.com/v1/air-quality") {
        return "https://open-meteo.com/en/docs/air-quality-api".to_string();
    }
    if low.contains("archive-api.open-meteo.com/v1/archive") {
        return "https://open-meteo.com/en/docs/historical-weather-api".to_string();
    }
    if low.contains("earthquake.usgs.gov/ws/designmaps/asce7-22.json") {
        return "https://earthquake.usgs.gov/ws/designmaps/asce7-22.html".to_string();
    }
    if low.contains("hazards-fema.maps.arcgis.com/apps/webappviewer") {
        if let Some(addr) = clean_opt(&q.address) {
            return format!(
                "https://msc.fema.gov/portal/search?AddressQuery={}",
                urlencoding::encode(&addr)
            );
        }
        return "https://msc.fema.gov/nfhl".to_string();
    }
    if low.contains("5a6038b3a1684561a9b0aadf88412fcf")
        || low.contains("earthquake.usgs.gov/cfusion/qfault/")
    {
        if clean_opt(&q.state)
            .map(|s| s.eq_ignore_ascii_case("CA"))
            .unwrap_or(false)
        {
            return "https://maps.conservation.ca.gov/cgs/EQZApp/app/".to_string();
        }
        return "https://earthquake.usgs.gov/nshmp/".to_string();
    }
    if low.contains("api.censusreporter.org/1.0/data/") {
        if let Some(t11) = resolve_census_tract_11(q.tract.as_deref(), q.county_fips.as_deref()) {
            return format!("https://censusreporter.org/profiles/14000US{t11}/");
        }
        return "https://censusreporter.org/".to_string();
    }
    if low.contains("earthquakeauthority.com/")
        && low.contains("cea-earthquake-insurance-premium-calculator")
    {
        return "https://www.earthquakeauthority.com/california-earthquake-insurance-policies/earthquake-insurance-premium-calculator".to_string();
    }
    if low.contains("nahb.org/") && low.contains("cost-of-constructing-a-home") {
        return "https://www.nahb.org/news-and-economics/housing-economics".to_string();
    }
    if low.contains("insurance.ca.gov") && low.contains("apex_extprd") {
        return "https://www.insurance.ca.gov/0250-insurers/0800-rate-filings/".to_string();
    }
    if (low.contains("epqs.nationalmap.gov/v1/json") && !low.contains("x="))
        || (low.contains("elevation.nationalmap.gov/arcgis/rest/services/3depelevation/imageserver")
            && !low.contains("geometry="))
    {
        return "https://apps.nationalmap.gov/3depdem/".to_string();
    }

    with_verified_data_anchor(trimmed, None, None)
}

async fn hydrate_query_from_slug(state: &AppState, q: &mut RedirectQuery) {
    let Some(slug) = clean_opt(&q.slug) else {
        return;
    };
    if let Ok(Some(hdr)) = state.db.get_report_header(&slug).await {
        if q.address.as_deref().map(str::trim).unwrap_or("").is_empty()
            && !hdr.display_address.trim().is_empty()
        {
            q.address = Some(hdr.display_address.trim().to_string());
        }
        if q.zip.as_deref().map(str::trim).unwrap_or("").is_empty() && !hdr.zip.trim().is_empty() {
            q.zip = Some(hdr.zip.trim().to_string());
        }
        if q.state.as_deref().map(str::trim).unwrap_or("").is_empty() {
            q.state = hdr.state_code.clone().filter(|s| !s.trim().is_empty());
        }
        if q.city.as_deref().map(str::trim).unwrap_or("").is_empty() {
            q.city = hdr.city.clone().filter(|s| !s.trim().is_empty());
        }
        if q.lat.is_none() {
            q.lat = hdr.lat;
        }
        if q.lon.is_none() {
            q.lon = hdr.lon;
        }
    }

    let needs_deep_fields = q.apn.as_deref().map(str::trim).unwrap_or("").is_empty()
        || q.county_fips
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        || q.tract.as_deref().map(str::trim).unwrap_or("").is_empty()
        || q.pwsid.as_deref().map(str::trim).unwrap_or("").is_empty();
    if needs_deep_fields {
        if let Ok(Some(rec)) = state.db.get_cached_report(&slug).await {
            let r = &rec.report;
            if q.apn.as_deref().map(str::trim).unwrap_or("").is_empty() {
                q.apn = r
                    .pointer("/facts/apn")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
            if q.county_fips
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                q.county_fips = r
                    .pointer("/geo/countyFips")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
            if q.county.as_deref().map(str::trim).unwrap_or("").is_empty() {
                q.county = r
                    .pointer("/geo/county")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
            if q.tract.as_deref().map(str::trim).unwrap_or("").is_empty() {
                q.tract = r
                    .pointer("/geo/censusReporterIds/tract")
                    .or_else(|| r.pointer("/geo/tract"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
            if q.pwsid.as_deref().map(str::trim).unwrap_or("").is_empty() {
                q.pwsid = r
                    .pointer("/modules/environment/data/water/primary/pwsid")
                    .or_else(|| r.pointer("/environment/water/primary/pwsid"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
        }
    }
}

fn to_extension_query(q: &RedirectQuery) -> ExtensionQueryProto {
    let addr = clean_opt(&q.address).unwrap_or_default();
    let street_line = addr.split(',').next().unwrap_or("").trim().to_string();
    let _ = parse_street(&addr);
    let mut extra_params = HashMap::new();
    if let Some(tr) = clean_opt(&q.tract) {
        extra_params.insert("tract".to_string(), tr);
    }
    if let Some(fid) = clean_opt(&q.fid) {
        extra_params.insert("fid".to_string(), fid);
    }
    if let Some(nces) = clean_opt(&q.nces_id) {
        extra_params.insert("nces_id".to_string(), nces);
    }
    ExtensionQueryProto {
        address: addr,
        street: street_line,
        city: clean_opt(&q.city).unwrap_or_default(),
        state: clean_opt(&q.state).unwrap_or_default(),
        zip: clean_opt(&q.zip).unwrap_or_default(),
        county_fips: clean_opt(&q.county_fips).unwrap_or_default(),
        county_name: clean_opt(&q.county),
        lat: q.lat,
        lon: q.lon,
        apn: clean_opt(&q.apn),
        sdwis_pwsid: clean_opt(&q.pwsid),
        extra_params,
        ..Default::default()
    }
}

fn upgraded_deep_ref_on_domain(q: &RedirectQuery, domain_suffix: &str) -> Option<String> {
    let raw = clean_opt(&q.fallback_ref)?;
    let up = upgrade_legacy_reference_url(&raw, q);
    let parsed = Url::parse(&up).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let domain_ok = host == domain_suffix || host.ends_with(&format!(".{domain_suffix}"));
    let has_specific_path_or_query = parsed.path().trim_matches('/').len() > 2 || parsed.query().is_some();
    (domain_ok && has_specific_path_or_query).then_some(up)
}

fn resolve_builtin_canonical_url(data_id: &str, q: &RedirectQuery) -> Option<String> {
    let id = data_id.trim().to_ascii_lowercase();
    let fips = clean_opt(&q.county_fips).unwrap_or_default();
    let county = clean_opt(&q.county).unwrap_or_default();
    let st = clean_opt(&q.state).unwrap_or_default();
    let city = clean_opt(&q.city).unwrap_or_default();
    let apn = clean_opt(&q.apn);
    let addr = clean_opt(&q.address);

    match id.as_str() {
        "county_assessor" => {
            let links = resolve_official_record_links(
                &fips,
                &county,
                &st,
                &city,
                apn.as_deref(),
                addr.as_deref(),
                None,
                None,
            );
            Some(links.assessor_url)
        }
        "county_tax_collector" => {
            let links = resolve_official_record_links(
                &fips,
                &county,
                &st,
                &city,
                apn.as_deref(),
                addr.as_deref(),
                None,
                None,
            );
            Some(links.tax_collector_url)
        }
        "county_recorder" => {
            let links = resolve_official_record_links(
                &fips,
                &county,
                &st,
                &city,
                apn.as_deref(),
                addr.as_deref(),
                None,
                None,
            );
            Some(links.recorder_url)
        }
        "city_permits" | "permit_portal" => {
            let links = resolve_official_record_links(
                &fips,
                &county,
                &st,
                &city,
                apn.as_deref(),
                addr.as_deref(),
                None,
                None,
            );
            Some(links.permit_portal_url)
        }
        "city_zoning" | "zoning_portal" => {
            let links = resolve_official_record_links(
                &fips,
                &county,
                &st,
                &city,
                apn.as_deref(),
                addr.as_deref(),
                None,
                None,
            );
            links
                .zoning_portal_url
                .or(links.gis_parcel_query_url)
                .or(Some(links.assessor_url))
        }
        "city_gis" | "county_gis" => {
            let links = resolve_official_record_links(
                &fips,
                &county,
                &st,
                &city,
                apn.as_deref(),
                addr.as_deref(),
                None,
                None,
            );
            links
                .gis_parcel_query_url
                .or(links.gis_layer_url)
                .or(Some(links.assessor_url))
        }
        "open_meteo_air" => match (q.lat, q.lon) {
            (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => Some(format!(
                "https://open-meteo.com/en/docs/air-quality-api#latitude={la:.4}&longitude={lo:.4}&hourly=pm10,pm2_5,ozone,nitrogen_dioxide,us_aqi&past_days=30"
            )),
            _ => Some("https://open-meteo.com/en/docs/air-quality-api".to_string()),
        },
        "airnow_aq" => match (q.lat, q.lon) {
            (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => {
                Some(format!("https://fire.airnow.gov/#12/{la:.4}/{lo:.4}"))
            }
            _ => Some("https://www.airnow.gov/".to_string()),
        },
        "calenviroscreen_4" | "calenviroscreen" => Some(
            "https://experience.arcgis.com/experience/11d2f52282a54ceebcac7428e6184203/"
                .to_string(),
        ),
        "epa_ejscreen" | "ejscreen" => Some("https://pedp-ejscreen.azurewebsites.net/".to_string()),
        "epa_sdwis_water" | "epa_echo_sdwis" | "sdwis_water" => {
            if let Some(pwsid) = clean_opt(&q.pwsid) {
                Some(format!(
                    "https://echo.epa.gov/detailed-facility-report?fid={}&sys=SDWIS",
                    urlencoding::encode(&pwsid)
                ))
            } else {
                upgraded_deep_ref_on_domain(q, "epa.gov")
                    .or_else(|| Some("https://echo.epa.gov/".to_string()))
            }
        }
        "epa_superfund" => upgraded_deep_ref_on_domain(q, "epa.gov").or_else(|| {
            Some("https://www.epa.gov/superfund/search-superfund-sites-where-you-live".to_string())
        }),
        "epa_frs" | "epa_echo" => {
            if let Some(fid) = clean_opt(&q.fid) {
                Some(format!(
                    "https://echo.epa.gov/detailed-facility-report?fid={}",
                    urlencoding::encode(&fid)
                ))
            } else {
                upgraded_deep_ref_on_domain(q, "epa.gov")
                    .or_else(|| Some("https://echo.epa.gov/".to_string()))
            }
        }
        "ca_geotracker" | "geotracker" => {
            upgraded_deep_ref_on_domain(q, "waterboards.ca.gov").or_else(|| {
                if let Some(a) = addr.as_deref() {
                    Some(format!(
                        "https://geotracker.waterboards.ca.gov/map/?CMD=runreport&myaddress={}",
                        urlencoding::encode(a)
                    ))
                } else {
                    Some("https://geotracker.waterboards.ca.gov/".to_string())
                }
            })
        }
        "ca_envirostor" | "envirostor" => {
            upgraded_deep_ref_on_domain(q, "dtsc.ca.gov").or_else(|| {
                if let Some(a) = addr.as_deref() {
                    Some(format!(
                        "https://www.envirostor.dtsc.ca.gov/public/map/?myaddress={}",
                        urlencoding::encode(a)
                    ))
                } else {
                    Some("https://www.envirostor.dtsc.ca.gov/public/".to_string())
                }
            })
        }
        "dot_bts_noise" | "bts_noise" => {
            Some("https://maps.dot.gov/BTS/NationalTransportationNoiseMap/".to_string())
        }
        "fema_nfhl" => {
            if let Some(a) = addr.as_deref() {
                Some(format!(
                    "https://msc.fema.gov/portal/search?AddressQuery={}",
                    urlencoding::encode(a)
                ))
            } else {
                Some("https://msc.fema.gov/nfhl".to_string())
            }
        }
        "fema_nri" => upgraded_deep_ref_on_domain(q, "fema.gov")
            .or_else(|| Some("https://hazards.fema.gov/nri/map".to_string())),
        "usgs_asce7" | "usgs_asce7_seismic" => {
            Some("https://earthquake.usgs.gov/ws/designmaps/asce7-22.html".to_string())
        }
        "usgs_faults" => {
            if st.eq_ignore_ascii_case("CA") {
                Some("https://maps.conservation.ca.gov/cgs/EQZApp/app/".to_string())
            } else {
                Some("https://earthquake.usgs.gov/nshmp/".to_string())
            }
        }
        "usgs_earthquakes" => upgraded_deep_ref_on_domain(q, "usgs.gov")
            .or_else(|| Some("https://earthquake.usgs.gov/earthquakes/map/".to_string())),
        "calfire_fhsz" => upgraded_deep_ref_on_domain(q, "fire.ca.gov").or_else(|| {
            Some(
                "https://osfm.fire.ca.gov/what-we-do/community-wildfire-preparedness-and-mitigation/fire-hazard-severity-zones"
                    .to_string(),
            )
        }),
        "ca_dwr_dam" => upgraded_deep_ref_on_domain(q, "water.ca.gov")
            .or_else(|| Some("https://fmds.water.ca.gov/maps/damim/".to_string())),
        "epa_radon" => upgraded_deep_ref_on_domain(q, "epa.gov").or_else(|| {
            Some(
                "https://www.epa.gov/radon/find-information-about-local-radon-zones-and-state-contact-information"
                    .to_string(),
            )
        }),
        "census_geocoder" => upgraded_deep_ref_on_domain(q, "census.gov")
            .or_else(|| Some("https://geocoding.geo.census.gov/geocoder/".to_string())),
        "census_soc" => upgraded_deep_ref_on_domain(q, "census.gov")
            .or_else(|| Some("https://www.census.gov/construction/chars/".to_string())),
        "census_reporter_tract" | "census_tract" => {
            if let Some(t11) = resolve_census_tract_11(q.tract.as_deref(), q.county_fips.as_deref())
            {
                Some(format!("https://censusreporter.org/profiles/14000US{t11}/"))
            } else {
                Some("https://data.census.gov/".to_string())
            }
        }
        "nces_school" => {
            if let Some(nid) = clean_opt(&q.nces_id) {
                let digits: String = nid.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                if digits.len() >= 11 {
                    return Some(format!(
                        "https://nces.ed.gov/ccd/schoolsearch/school_detail.asp?ID={digits}"
                    ));
                }
                if !digits.is_empty() {
                    return Some(format!(
                        "https://nces.ed.gov/surveys/pss/privateschoolsearch/school_detail.asp?ID={digits}"
                    ));
                }
            }
            upgraded_deep_ref_on_domain(q, "nces.ed.gov")
                .or_else(|| Some("https://nces.ed.gov/ccd/schoolsearch/".to_string()))
        }
        "ca_school_dashboard" => upgraded_deep_ref_on_domain(q, "caschooldashboard.org")
            .or_else(|| upgraded_deep_ref_on_domain(q, "ca.gov"))
            .or_else(|| Some("https://www.caschooldashboard.org/".to_string())),
        "greatschools" => upgraded_deep_ref_on_domain(q, "greatschools.org")
            .or_else(|| Some("https://www.greatschools.org/".to_string())),
        "niche_schools" => upgraded_deep_ref_on_domain(q, "niche.com")
            .or_else(|| Some("https://www.niche.com/k12/rankings/".to_string())),
        "fred_pmms" | "freddie_mac_pmms" => upgraded_deep_ref_on_domain(q, "stlouisfed.org")
            .or_else(|| Some("https://www.freddiemac.com/pmms".to_string())),
        "fhfa_hpi" => Some("https://www.fhfa.gov/data/hpi".to_string()),
        "hud_fmr" => upgraded_deep_ref_on_domain(q, "huduser.gov")
            .or_else(|| Some("https://www.huduser.gov/portal/datasets/fmr.html".to_string())),
        "open_meteo_archive" => {
            Some("https://open-meteo.com/en/docs/historical-weather-api".to_string())
        }
        "usgs_3dep" | "usgs_3dep_elevation" | "3dep_elevation" | "apps_nationalmap_gov" => {
            Some("https://apps.nationalmap.gov/3depdem/".to_string())
        }
        "bls_cpi" => Some("https://www.bls.gov/cpi/".to_string()),
        "zillow_research" => Some("https://www.zillow.com/research/data/".to_string()),
        "usda_nrcs_soil" => Some("https://sdmdataaccess.sc.egov.usda.gov/".to_string()),
        "usda_nass" => {
            Some("https://www.nass.usda.gov/Surveys/Guide_to_NASS_Surveys/June_Area/".to_string())
        }
        "fbi_cde" => Some("https://cde.ucr.cjis.gov/LATEST/webapp/#/pages/home".to_string()),
        "fcc_broadband_bdc_live" | "fcc_broadband_bdc" | "fcc_mobile_5g" => {
            upgraded_deep_ref_on_domain(q, "fcc.gov").or_else(|| {
                Some("https://broadbandmap.fcc.gov/location-summary/fixed".to_string())
            })
        }
        "sonic_fiber_availability" => upgraded_deep_ref_on_domain(q, "sonic.com")
            .or_else(|| Some("https://www.sonic.com/availability".to_string())),
        "eia861_electric_rates" => upgraded_deep_ref_on_domain(q, "eia.gov")
            .or_else(|| Some("https://www.eia.gov/electricity/data/eia861/".to_string())),
        "eia_fuel" => upgraded_deep_ref_on_domain(q, "eia.gov")
            .or_else(|| Some("https://www.eia.gov/petroleum/gasdiesel/".to_string())),
        "pge_live_tariffs" => upgraded_deep_ref_on_domain(q, "pge.com").or_else(|| {
            Some(
                "https://www.pge.com/en/account/rate-plans/find-your-best-rate-plan.html"
                    .to_string(),
            )
        }),
        "sjwater_live_tariffs" => upgraded_deep_ref_on_domain(q, "sjwater.com")
            .or_else(|| Some("https://www.sjwater.com/for-my-home/bill-payment/rates".to_string())),
        "cec_electric_territory" => upgraded_deep_ref_on_domain(q, "arcgis.com").or_else(|| {
            Some(
                "https://cecgis-caenergy.opendata.arcgis.com/datasets/CAEnergy::california-electric-utility-service-areas"
                    .to_string(),
            )
        }),
        "campbell_mgo_permits" => Some("https://www.mgoconnect.org/cp?JID=187".to_string()),
        "osm_geometry" => upgraded_deep_ref_on_domain(q, "openstreetmap.org")
            .or_else(|| Some("https://www.openstreetmap.org/".to_string())),
        "doe_afdc_ev" => upgraded_deep_ref_on_domain(q, "energy.gov")
            .or_else(|| Some("https://afdc.energy.gov/stations".to_string())),
        "cms_care_compare" => upgraded_deep_ref_on_domain(q, "medicare.gov")
            .or_else(|| Some("https://www.medicare.gov/care-compare/".to_string())),
        _ => None,
    }
}

/// Public `GET /redirect` handler resolving dynamic extension/module/portal reference links,
/// recording aggregate click telemetry, and issuing `302 Found` with `Cache-Control: no-store`.
pub async fn handle_reference_redirect(
    State(state): State<AppState>,
    Query(mut q): Query<RedirectQuery>,
) -> Response {
    let link_type = clean_opt(&q.link_type).unwrap_or_else(|| "extension_reference".to_string());
    if !matches!(
        link_type.as_str(),
        "extension_reference" | "module_reference" | "portal_reference"
    ) {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_REDIRECT_TYPE",
                "error": "Unsupported redirect type; expected extension_reference, module_reference, or portal_reference.",
            }),
        );
    }

    let raw_data_id = clean_opt(&q.data_id).unwrap_or_default();
    if raw_data_id.is_empty() && clean_opt(&q.fallback_ref).is_none() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "MISSING_REDIRECT_TARGET",
                "error": "Missing required data_id or ref parameter.",
            }),
        );
    }

    hydrate_query_from_slug(&state, &mut q).await;
    let ext_query = to_extension_query(&q);
    let extra_hosts = state.extension_hub.registered_reference_hosts();

    // Priority (0): State-specific regulator / deep-document `&ref=` preservation before
    // static single-URL extension manifest templates (e.g. CA Dept. of Insurance vs Texas TDI,
    // or specific PG&E / SJW / EIA / FCC schedule documents vs generic landing page).
    let mut candidate_opt = if !raw_data_id.is_empty() && q.rule_id.is_none() {
        let low_id = raw_data_id.to_ascii_lowercase();
        match low_id.as_str() {
            "title_underwriter_rates" => upgraded_deep_ref_on_domain(&q, "insurance.ca.gov")
                .or_else(|| upgraded_deep_ref_on_domain(&q, "floir.gov"))
                .or_else(|| upgraded_deep_ref_on_domain(&q, "floir.com"))
                .or_else(|| upgraded_deep_ref_on_domain(&q, "osi.state.nm.us"))
                .or_else(|| upgraded_deep_ref_on_domain(&q, "tdi.texas.gov"))
                .or_else(|| upgraded_deep_ref_on_domain(&q, "dfs.ny.gov"))
                .or_else(|| upgraded_deep_ref_on_domain(&q, "oic.wa.gov"))
                .or_else(|| match clean_opt(&q.state).as_deref().map(str::to_ascii_uppercase).as_deref() {
                    Some("CA") => {
                        Some("https://www.insurance.ca.gov/0250-insurers/0800-rate-filings/".to_string())
                    }
                    Some("FL") => {
                        Some("https://floir.gov/property-casualty/title-insurance".to_string())
                    }
                    Some("NM") => {
                        Some("https://www.osi.state.nm.us/pages/bureaus/pc/title-insurance".to_string())
                    }
                    _ => None,
                }),
            "pge_live_tariffs" => upgraded_deep_ref_on_domain(&q, "pge.com"),
            "sjwater_live_tariffs" => upgraded_deep_ref_on_domain(&q, "sjwater.com"),
            "eia861_electric_rates" => upgraded_deep_ref_on_domain(&q, "eia.gov"),
            "fcc_broadband_bdc_live" | "fcc_broadband_bdc" | "fcc_mobile_5g" => {
                upgraded_deep_ref_on_domain(&q, "fcc.gov")
            }
            _ => None,
        }
    } else {
        None
    };

    // Priority (a): ExtensionHub live manifest / rule resolution
    if candidate_opt.is_none() && !raw_data_id.is_empty() {
        candidate_opt = state.extension_hub.resolve_extension_reference_url(
            &raw_data_id,
            q.rule_id.as_deref(),
            &ext_query,
        );
    }

    // Priority (b): Built-in canonical agency / county / civic reference IDs
    if candidate_opt.is_none() && !raw_data_id.is_empty() {
        candidate_opt = resolve_builtin_canonical_url(&raw_data_id, &q);
    }

    // Priority (c): Legacy-upgraded per-record `&ref=<url>` fallback
    if candidate_opt.is_none() {
        if let Some(raw_ref) = clean_opt(&q.fallback_ref) {
            candidate_opt = Some(upgrade_legacy_reference_url(&raw_ref, &q));
        }
    }

    let Some(candidate) = candidate_opt else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "UNRESOLVED_REFERENCE_ID",
                "error": "Unable to resolve reference destination for the requested data_id.",
            }),
        );
    };

    let Some(validated_target) = validate_allowed_redirect_target(&candidate, &extra_hosts) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "UNTRUSTED_REDIRECT_TARGET",
                "error": "Redirect destination must be an allowlisted https:// public data or utility domain.",
            }),
        );
    };

    let Ok(loc_header) = HeaderValue::from_str(&validated_target) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_REDIRECT_HEADER",
                "error": "Malformed redirect target URL.",
            }),
        );
    };

    let telemetry_data_id = if !raw_data_id.is_empty() {
        raw_data_id.clone()
    } else {
        Url::parse(&validated_target)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_else(|| "direct_ref".to_string())
    };
    let section = clean_opt(&q.section).unwrap_or_else(|| "general".to_string());
    let slug_str = clean_opt(&q.slug).unwrap_or_default();

    let _ = record_reference_redirect_click(
        &state.db,
        &link_type,
        &telemetry_data_id,
        &section,
        &slug_str,
        &validated_target,
    )
    .await;

    let hook_event = ReferenceRedirectEvent {
        link_type: link_type.clone(),
        data_id: telemetry_data_id,
        section,
        slug: (!slug_str.is_empty()).then_some(slug_str),
        rule_id: clean_opt(&q.rule_id),
        target_url: validated_target,
        timestamp: now_iso(),
    };
    state.hooks.on_reference_redirect(&state, &hook_event).await;

    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(header::LOCATION, loc_header);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

/// Admin/ops-protected JSON endpoint (`GET /api/ops/telemetry/redirects`) returning top clicked
/// `(type, data_id, section, click_count, last_target_url, updated_at)` entries.
pub async fn handle_ops_redirect_telemetry_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<RedirectTelemetryQuery>,
) -> Response {
    if let Err(auth_err) =
        crate::web::admin_console::verify_admin_access(&state, &headers, &extensions).await
    {
        let status = auth_err.status;
        let mut resp = json_response(
            status,
            json!({
                "ok": false,
                "code": auth_err.code,
                "error": auth_err.message,
            }),
        );
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, max-age=0"),
        );
        apply_security_headers(resp.headers_mut());
        return resp;
    }

    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let rows = query_top_reference_redirect_clicks(&state.db, limit)
        .await
        .unwrap_or_default();
    let total_clicks: u64 = rows.iter().map(|r| r.click_count).sum();
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "type": r.link_type,
                "linkType": r.link_type,
                "data_id": r.data_id,
                "dataId": r.data_id,
                "section": r.section,
                "click_count": r.click_count,
                "clickCount": r.click_count,
                "last_slug": r.last_slug,
                "lastSlug": r.last_slug,
                "last_target_url": r.last_target_url,
                "lastTargetUrl": r.last_target_url,
                "updated_at": r.updated_at,
                "updatedAt": r.updated_at,
            })
        })
        .collect();

    let mut resp = (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "totalClicks": total_clicks,
            "distinctTargets": items.len(),
            "items": items,
        })),
    )
        .into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}
