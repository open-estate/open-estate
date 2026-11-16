//! Zero-knowledge brand-independence verification for the open-source `open-estate` repository.
//!
//! Enforces via positive allowlists (without embedding any downstream brand or private domain
//! literals in source) that branding defaults, manifests, locales, infra configs, and URLs
//! stay strictly scoped to `Open Estate` / `openestate.org` and approved public/OSS domains.

use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn is_allowed_ip_literal(host: &str, in_proxy_pool_test: bool) -> bool {
    let Ok(ip) = host.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let o = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || o == [1, 2, 3, 4]
        || in_proxy_pool_test
}

fn is_allowed_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty()
        || h == "localhost"
        || h == "openestate"
        || h == "osrm"
        || h == "osrm-ca"
        || h == "osrm-east"
        || h == "osrm-us"
        || h == "osrm-west"
        || h == "mysql"
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".svc")
        || h.ends_with(".example")
        || h.ends_with(".invalid")
        || h == "example.com"
        || h.ends_with(".example.com")
        || h == "example.org"
        || h.ends_with(".example.org")
        || h == "openestate.org"
        || h.ends_with(".openestate.org")
        || h == "open-estate.org"
        || h.ends_with(".open-estate.org")
    {
        return true;
    }

    // US federal/state/municipal public-domain & educational domains
    if h.ends_with(".gov")
        || h.ends_with(".us")
        || h.ends_with(".edu")
        || h.ends_with(".mil")
    {
        return true;
    }

    // Standard OSS / public-data / map / CDN / extension / official BYOK connector domains
    const ALLOWED_SUFFIXES: &[&str] = &[
        "github.com",
        "githubusercontent.com",
        "github.io",
        "ghcr.io",
        "docker.io",
        "debian.org",
        "crates.io",
        "w3.org",
        "openstreetmap.org",
        "overpass-api.de",
        "overpass.private.coffee",
        "geofabrik.de",
        "komoot.io",
        "open-meteo.com",
        "wikimedia.org",
        "wikipedia.org",
        "arcgis.com",
        "arcgisonline.com",
        "opendata.arcgis.com",
        "jsdelivr.net",
        "cloudflare.com",
        "unpkg.com",
        "googleapis.com",
        "gstatic.com",
        "google.com",
        "bing.com",
        "censusreporter.org",
        "stlouisfed.org",
        "freddiemac.com",
        "fanniemae.com",
        "zippopotam.us",
        "ipify.org",
        "icanhazip.com",
        "amazonaws.com",
        "freeipapi.com",
        "ipapi.co",
        "ip-api.com",
        "ipinfo.io",
        "ipwho.is",
        "openai.com",
        "rentcast.io",
        "attomdata.com",
        "simplyrets.com",
        "shovels.ai",
        "zoneomics.com",
        "mapbox.com",
        "locationiq.com",
        "openrouteservice.org",
        "freeroute.org",
        "mapillary.com",
        "socrata.com",
        "accela.com",
        "my.site.com",
        "ets.org",
        "caschooldashboard.org",
        "greatschools.org",
        "niche.com",
        "courtlistener.com",
        "matterport.com",
        "youriguide.com",
        "ricoh360.com",
        "cloudpano.com",
        "giraffe360.com",
        "kuula.co",
        "youtube.com",
        "youtube-nocookie.com",
        "vimeo.com",
        "hcaptcha.com",
        "claudeskills.info",
        // Public utility / insurance / civic portal links referenced in diligence modules
        "xcelenergy.com",
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
        "siliconvalleypower.com",
        "alamedamp.com",
        "burbankwaterandpower.com",
        "glendalewaterandpower.com",
        "pacificpower.net",
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
        "pepco.com",
        "papowerswitch.com",
        "energychoice.ohio.gov",
        "chugachelectric.com",
        "hawaiianelectric.com",
        "epelectric.com",
        "xfinity.com",
        "att.com",
        "verizon.com",
        "t-mobile.com",
        "spectrum.com",
        "cox.com",
        "frontier.com",
        "optimum.com",
        "sonic.com",
        "ziplyfiber.com",
        "quantumfiber.com",
        "starlink.com",
        "earthlink.net",
        "caionline.org",
        "earthquakeauthority.com",
        "floodsmart.gov",
        "alta.org",
        "tirsa.org",
        "naic.org",
        "astm.org",
        "nachi.org",
        "nahb.org",
        "jlconline.com",
        "iccsafe.org",
        "ulc.org",
        "cfpnet.com",
        "nass.org",
        "taxadmin.org",
        "ncsha.org",
        "publicpower.org",
        "utahwatersavers.com",
        "floridarevenue.com",
        "myfloridalicense.com",
        "floir.com",
        "masslandrecords.com",
        "richmondcountyclerk.com",
        "tccsearch.org",
        "crsecurepayment.com",
        "ocrecorder.com",
        "ocgis.com",
        "acassessor.org",
        "acgov.org",
        "sccassessor.org",
        "marinmap.org",
        "marincounty.org",
        "smcacre.org",
        "contracostacore.us",
        "kingcounty.com",
        "hctx.net",
        "traviscad.org",
        "dallascityhall.com",
        "cookcountyassessor.com",
        "cookcountytreasurer.com",
        "viewpointcloud.com",
        "oakgov.com",
        "cagisonline.hamilton-co.org",
        "alleghenycounty.us",
        "wprdc.org",
        "opendataphilly.org",
        "portlandmaps.com",
        "sfgov.org",
        "hennepin.us",
        "horrycounty.org",
        "oak-park.us",
        "ssf.net",
        "cityofpasadena.net",
        "anaheim.net",
        "cityofberkeley.info",
        "project-osrm.org",
        "openstreetmap.de",
        "proxyscrape.com",
        "geonode.com",
        "proxy-list.download",
        // Standard real-estate listing/portal references in portal crosswalk / scrubbing registry
        "zillow.com",
        "zillowstatic.com",
        "redfin.com",
        "cdn-redfin.com",
        "realtor.com",
        "rdcpix.com",
        "trulia.com",
        "homes.com",
        "homesstatic.com",
        "movoto.com",
        "compass.com",
        "opendoor.com",
        "loopnet.com",
        "crexi.com",
        "housecanary.com",
        "walkscore.com",
        "xome.com",
        "estately.com",
        "coldwellbanker.com",
        "kw.com",
        // Municipal, county, state open-data, recorder, tax, and permit portals
        "sccgov.org",
        "ocgov.com",
        "cityofchicago.org",
        "lacity.org",
        "tnris.org",
        "dcgis.org",
        "dupageco.org",
        "lojic.org",
        "montcopa.org",
        "smcgov.org",
        "charlestoncounty.org",
        "sedgwickcounty.org",
        "countyofkane.org",
        "westchestergov.com",
        "shields.io",
        "ipwhois.io",
        "sdtreastax.com",
        "jccal.org",
        "jacksongov.org",
        "incog.org",
        "bexar.org",
        "dcad.org",
        "stlouisco.com",
        "saccounty.net",
        "county-taxes.com",
        "mybuildingpermit.com",
        "pseg.com",
        "adobe.com",
        "oklahomacounty.org",
        "cityofpaloalto.org",
        "carto.com",
        "cupix.com",
        "portlandgeneral.com",
        "propublica.org",
        "sanjosecleanenergy.org",
        "svcleanenergy.org",
        "openei.org",
        "westvalleyrecycles.com",
        "saveourwater.com",
        "schema.org",
        "sunbiz.org",
        "sfassessor.org",
        "sfplanninggis.org",
        "sftreasurer.org",
        "sjpermits.org",
        "socalwatersmart.com",
        "newedgeservices.com",
        "prodigycad.com",
        "airnowapi.org",
        "burlingame.org",
        "cityofconcord.org",
        "cityofepa.org",
        "cityofirvine.org",
        "cityofmillvalley.org",
        "cityofsanmateo.org",
        "cityofsanrafael.org",
        "coralgables.com",
        "dalycity.org",
        "denvergov.org",
        "homesteps.com",
        "houstonpermittingcenter.org",
        "miamigov.com",
        "nacrc.org",
        "njportal.com",
        "redwoodcity.org",
        "santa-ana.org",
        "sdttc.com",
        "walnut-creek.org",
    ];

    ALLOWED_SUFFIXES
        .iter()
        .any(|suf| h == *suf || h.ends_with(&format!(".{suf}")))
}

fn visit_tree(root: &Path, dir: &Path, url_rx: &Regex, bad_hosts: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name_str = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name_str.as_str(), ".git" | "target" | ".cache" | "node_modules") {
                continue;
            }
            visit_tree(root, &path, url_rx, bad_hosts);
        } else {
            if name_str == ".env" || name_str == "Cargo.lock" {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let rel_str = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy();
            let in_tests = rel_str.starts_with("tests/");
            let in_proxy_pool_test = rel_str == "tests/free_proxy_pool_test.rs";
            for cap in url_rx.captures_iter(&text) {
                let host = &cap[1];
                if host.contains('$') || host.contains('{') {
                    continue;
                }
                // Skip regex/prefix fragments such as `https?://www\.` or `https?://ssl.`
                if host.ends_with('.') && !host.trim_end_matches('.').contains('.') {
                    continue;
                }
                if is_allowed_ip_literal(host, in_proxy_pool_test) {
                    continue;
                }
                if in_tests
                    && matches!(
                        host.to_ascii_lowercase().as_str(),
                        "attacker.evil.io" | "evil.io" | "some.api" | "idx-feed.mlsgrid-example.org"
                    )
                {
                    continue;
                }
                if !is_allowed_host(host) {
                    bad_hosts.insert(format!("{host} (in {})", path.display()));
                }
            }
        }
    }
}

#[test]
fn open_estate_uses_only_open_estate_brand_and_allowlisted_domains() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // 1. Core package, container, extension, and locale defaults must strictly brand as Open Estate
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(cargo_toml.contains("name = \"open-estate\""));
    assert!(cargo_toml.contains("default-run = \"open-estate-server\""));

    let dockerfile = fs::read_to_string(root.join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("BRAND_NAME=\"Open Estate\""));
    assert!(dockerfile.contains("BRAND_DOMAIN=open-estate.local"));

    let compose = fs::read_to_string(root.join("docker-compose.yml")).unwrap();
    assert!(compose.contains("open-estate:"));
    assert!(compose.contains("openestate/openestate"));

    let ext_manifest = fs::read_to_string(root.join("extension/manifest.json")).unwrap();
    assert!(ext_manifest.contains("Open Estate"));

    let ext_brand = fs::read_to_string(root.join("extension/brand.js")).unwrap();
    assert!(ext_brand.contains("Open Estate"));

    let i18n_rs = fs::read_to_string(root.join("src/web/i18n.rs")).unwrap();
    assert!(i18n_rs.contains("DEFAULT_BRAND_NAME: &str = \"Open Estate\""));

    // 2. Every HTTP(S) domain referenced anywhere in the repo must belong to the OSS allowlist
    let url_rx = Regex::new(r#"https?://(?:[A-Za-z0-9._-]+(?::[^/\s@"'`<>]+)?@)?([A-Za-z0-9._-]+)"#).unwrap();
    let mut bad_hosts = BTreeSet::new();
    visit_tree(root, root, &url_rx, &mut bad_hosts);
    assert!(
        bad_hosts.is_empty(),
        "Found unapproved / non-OSS external host(s):\n{}",
        bad_hosts.into_iter().collect::<Vec<_>>().join("\n")
    );
}
