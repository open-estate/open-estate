// US street address -> coordinates + official Census geographies (tract, block group, block,
// ZIP/ZCTA, place, county, MSA/CBSA, school districts, congressional & legislative districts,
// and Census Reporter ACS geo_ids).
// Primary: U.S. Census Geocoder (`geographies/onelineaddress`, Public_AR_Current / Current_Current).
// Fallback: ArcGIS World Geocoder (`findAddressCandidates`) + Census `geographies/coordinates`.

use crate::property::suggest::split_us_address;
use crate::registry::endpoints::{
    CENSUS_GEOCODER_GEOS_BASE as CENSUS_GEOS_BASE,
    ESRI_WORLD_GEOCODE_CANDIDATES_URL as ESRI_FIND_CANDIDATES,
};
pub use crate::registry::us_geo::state_fips_for_abbr;
pub use crate::shared::title_case;
use crate::shared::{HttpClient, ModuleOutput, SourceItem};
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use url::Url;

static STREET_TYPES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    HashMap::from([
        ("CLL", "Calle"),
        ("AVE", "Ave"),
        ("ST", "St"),
        ("RD", "Rd"),
        ("DR", "Dr"),
        ("LN", "Ln"),
        ("CT", "Ct"),
        ("PL", "Pl"),
        ("BLVD", "Blvd"),
        ("WAY", "Way"),
        ("CIR", "Cir"),
        ("TER", "Ter"),
        ("PKWY", "Pkwy"),
        ("HWY", "Hwy"),
        ("TRL", "Trl"),
        ("SQ", "Sq"),
        ("LOOP", "Loop"),
    ])
});

static RE_DE_PARTICLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bDe\b").unwrap());
static RE_UNIT_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:,?\s*(?:apt|apartment|unit|ste|suite|fl|floor|rm|room|bldg)\b\.?\s*[a-z0-9-]+|,?\s*#\s*[a-z0-9-]+)").unwrap()
});
static RE_MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

fn first_geo<'a>(geos: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    for &n in names {
        if let Some((_, arr)) = geos.iter().find(|(k, _)| k.as_str() == n || k.ends_with(n)) {
            if let Some(first) = arr.as_array().and_then(|a| a.first()) {
                return Some(first);
            }
        }
    }
    None
}

fn find_geo_regex<'a>(geos: &'a serde_json::Map<String, Value>, needle: &str) -> Option<&'a Value> {
    geos.iter()
        .find(|(k, _)| k.contains(needle))
        .and_then(|(_, v)| v.as_array())
        .and_then(|a| a.first())
}

fn str_field(v: Option<&Value>, key: &str) -> Option<String> {
    v.and_then(|o| o.get(key))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn format_census_display_address(m: &Value) -> String {
    let matched_addr = m
        .get("matchedAddress")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let num = matched_addr.split_whitespace().next().unwrap_or("");
    let c = m.get("addressComponents");
    let pre_dir = str_field(c, "preDirection").unwrap_or_default();
    let pre_type_raw = str_field(c, "preType").unwrap_or_default();
    let pre = if !pre_type_raw.is_empty() {
        let mapped = STREET_TYPES
            .get(pre_type_raw.to_uppercase().as_str())
            .map(|s| (*s).to_string())
            .unwrap_or_else(|| title_case(&pre_type_raw));
        format!("{mapped} ")
    } else {
        String::new()
    };
    let street_name = str_field(c, "streetName")
        .map(|s| title_case(&s))
        .unwrap_or_default();
    let suffix_type = str_field(c, "suffixType")
        .map(|s| {
            STREET_TYPES
                .get(s.to_uppercase().as_str())
                .map(|x| (*x).to_string())
                .unwrap_or_else(|| title_case(&s))
        })
        .unwrap_or_default();
    let suffix_dir = str_field(c, "suffixDirection").unwrap_or_default();

    let street_parts: Vec<String> = [
        pre_dir,
        format!("{pre}{street_name}"),
        suffix_type,
        suffix_dir,
    ]
    .into_iter()
    .map(|x| x.trim().to_string())
    .filter(|x| !x.is_empty())
    .collect();
    let street = RE_MULTI_SPACE
        .replace_all(&street_parts.join(" "), " ")
        .trim()
        .to_string();
    let city = str_field(c, "city")
        .map(|s| title_case(&s))
        .unwrap_or_default();
    let state = str_field(c, "state").unwrap_or_default();
    let zip = str_field(c, "zip").unwrap_or_default();
    let assembled = format!("{num} {street}, {city}, {state} {zip}")
        .trim()
        .to_string();
    RE_DE_PARTICLE.replace_all(&assembled, "de").into_owned()
}

#[allow(clippy::too_many_arguments)]
fn build_geocode_payload(
    input: &str,
    matched_address: String,
    display_address: String,
    lat: f64,
    lon: f64,
    tiger_line_id: Option<String>,
    side: Option<String>,
    addr_state: Option<String>,
    addr_city: Option<String>,
    addr_zip: Option<String>,
    geos_val: Option<&Value>,
    warnings: &mut Vec<String>,
) -> Value {
    let empty_map = serde_json::Map::new();
    let g = geos_val.and_then(|v| v.as_object()).unwrap_or(&empty_map);

    let st = first_geo(g, &["States"]);
    let county = first_geo(g, &["Counties"]);
    let tract = first_geo(g, &["Census Tracts"]);
    let bg = first_geo(g, &["Census Block Groups"]);
    let block = first_geo(g, &["Census Blocks"]);
    let zcta = first_geo(g, &["ZIP Code Tabulation Areas"]);
    let place = first_geo(g, &["Incorporated Places", "Census Designated Places"]);
    let msa = first_geo(
        g,
        &[
            "Metropolitan Statistical Areas",
            "Micropolitan Statistical Areas",
        ],
    );
    let csa = first_geo(g, &["Combined Statistical Areas"]);
    let cd = find_geo_regex(g, "Congressional Districts");
    let sldu = find_geo_regex(g, "Legislative Districts - Upper");
    let sldl = find_geo_regex(g, "Legislative Districts - Lower");
    let unified = first_geo(g, &["Unified School Districts"]);
    let elem = first_geo(g, &["Elementary School Districts"]);
    let sec = first_geo(g, &["Secondary School Districts"]);
    let puma = first_geo(g, &["Public Use Microdata Areas"]);

    let state_abbr = str_field(st, "STUSAB").or(addr_state);
    let state_name = str_field(st, "NAME");
    let state_fips = str_field(st, "GEOID").or_else(|| {
        state_abbr
            .as_deref()
            .and_then(state_fips_for_abbr)
            .map(|s| s.to_string())
    });

    if let Some(ref abbr) = state_abbr {
        if state_fips_for_abbr(abbr).is_none() {
            warnings.push(format!("State {abbr} not recognized as a US state."));
        }
    }

    let county_name = str_field(county, "NAME");
    let county_fips = str_field(county, "GEOID");
    let place_name = str_field(place, "NAME");
    let place_fips = str_field(place, "GEOID");
    let city = addr_city.filter(|s| !s.is_empty()).or_else(|| {
        place_name.as_ref().map(|p| {
            p.trim_end_matches(" city")
                .trim_end_matches(" town")
                .trim_end_matches(" CDP")
                .trim_end_matches(" village")
                .trim()
                .to_string()
        })
    });
    let zcta_geoid = str_field(zcta, "GEOID").or_else(|| str_field(zcta, "ZCTA5"));
    let zip = addr_zip
        .filter(|s| !s.is_empty())
        .map(|z| z.chars().take(5).collect::<String>())
        .or_else(|| zcta_geoid.clone());
    let zcta_final = zcta_geoid.or_else(|| zip.clone());

    let tract_geoid = str_field(tract, "GEOID");
    let tract_name = str_field(tract, "NAME");
    let bg_geoid = str_field(bg, "GEOID");
    let block_geoid = str_field(block, "GEOID");
    let urban_rural = str_field(block, "UR");

    let msa_cbsa = str_field(msa, "CBSA").or_else(|| str_field(msa, "GEOID"));
    let msa_name = str_field(msa, "NAME");
    let msa_obj = match (&msa_cbsa, &msa_name) {
        (Some(cbsa), Some(name)) => json!({ "cbsa": cbsa, "name": name }),
        (Some(cbsa), None) => json!({ "cbsa": cbsa, "name": Value::Null }),
        _ => Value::Null,
    };

    let csa_obj = match (str_field(csa, "GEOID"), str_field(csa, "NAME")) {
        (Some(code), name) => json!({ "code": code, "name": name }),
        _ => Value::Null,
    };

    let puma_obj = match (str_field(puma, "GEOID"), str_field(puma, "NAME")) {
        (Some(geoid), name) => json!({ "geoid": geoid, "name": name }),
        _ => Value::Null,
    };

    let cong_dist = str_field(cd, "NAME");
    let state_senate = str_field(sldu, "NAME");
    let state_assembly = str_field(sldl, "NAME");

    let cr_ids = json!({
        "blockGroup": bg_geoid.as_ref().map(|x| format!("15000US{x}")),
        "tract": tract_geoid.as_ref().map(|x| format!("14000US{x}")),
        "zcta": zcta_final.as_ref().map(|x| format!("86000US{x}")),
        "place": place_fips.as_ref().map(|x| format!("16000US{x}")),
        "county": county_fips.as_ref().map(|x| format!("05000US{x}")),
        "msa": msa_cbsa.as_ref().map(|x| format!("31000US{x}")),
        "state": state_fips.as_ref().map(|x| format!("04000US{x}")),
        "nation": "01000US",
    });

    json!({
        "matched": true,
        "input": input,
        "matchedAddress": matched_address,
        "displayAddress": display_address,
        "lat": lat,
        "lon": lon,
        "tigerLineId": tiger_line_id,
        "side": side,
        "state": state_abbr,
        "stateName": state_name,
        "stateFips": state_fips,
        "county": county_name,
        "countyFips": county_fips,
        "place": place_name,
        "placeFips": place_fips,
        "city": city,
        "zip": zip,
        "zcta": zcta_final,
        "tract": tract_geoid,
        "tractName": tract_name,
        "blockGroup": bg_geoid,
        "block": block_geoid,
        "urbanRural": urban_rural,
        "msa": msa_obj,
        "cbsa": msa_cbsa,
        "cbsaName": msa_name,
        "csa": csa_obj,
        "puma": puma_obj,
        "schoolDistricts": {
            "unified": str_field(unified, "NAME"),
            "elementary": str_field(elem, "NAME"),
            "secondary": str_field(sec, "NAME"),
        },
        "congressionalDistrict": cong_dist,
        "stateSenate": state_senate,
        "stateAssembly": state_assembly,
        "stateLegislativeUpper": state_senate,
        "stateLegislativeLower": state_assembly,
        "censusReporterIds": cr_ids,
    })
}

async fn census_oneline(http: &HttpClient, addr: &str) -> Result<(String, Value)> {
    let mut u = Url::parse(&format!("{CENSUS_GEOS_BASE}/onelineaddress"))?;
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("address", addr);
        qp.append_pair("benchmark", "Public_AR_Current");
        qp.append_pair("vintage", "Current_Current");
        qp.append_pair("layers", "all");
        qp.append_pair("format", "json");
    }
    let url_str = u.to_string();
    let j = http
        .get_json_opts(&url_str, 22_000, 2, 86400 * 14, None)
        .await?;
    Ok((url_str, j))
}

pub async fn census_coordinates(http: &HttpClient, lat: f64, lon: f64) -> Result<(String, Value)> {
    let mut u = Url::parse(&format!("{CENSUS_GEOS_BASE}/coordinates"))?;
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("x", &format!("{lon:.6}"));
        qp.append_pair("y", &format!("{lat:.6}"));
        qp.append_pair("benchmark", "Public_AR_Current");
        qp.append_pair("vintage", "Current_Current");
        qp.append_pair("layers", "all");
        qp.append_pair("format", "json");
    }
    let url_str = u.to_string();
    let j = http
        .get_json_opts(&url_str, 15_000, 2, 86400 * 14, None)
        .await?;
    Ok((url_str, j))
}

async fn arcgis_find_candidate(
    http: &HttpClient,
    addr: &str,
    token: &str,
) -> Result<Option<(String, Value)>> {
    let mut u = Url::parse(ESRI_FIND_CANDIDATES)?;
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("SingleLine", addr);
        qp.append_pair("countryCode", "USA");
        qp.append_pair(
            "outFields",
            "Match_addr,Addr_type,StAddr,City,RegionAbbr,Region,Postal,Subregion",
        );
        qp.append_pair("maxLocations", "3");
        qp.append_pair("f", "json");
        qp.append_pair("token", token);
    }
    let url_str = u.to_string();
    let j = http
        .get_json_opts(&url_str, 10_000, 1, 86400 * 14, None)
        .await?;
    let best = j
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter().find(|cand| {
                let score = cand.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
                let lat = cand
                    .get("location")
                    .and_then(|l| l.get("y"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let lon = cand
                    .get("location")
                    .and_then(|l| l.get("x"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if !(score >= 82.0
                    && (17.5..=72.0).contains(&lat)
                    && (-179.9..=-64.5).contains(&lon))
                {
                    return false;
                }
                let attrs = cand.get("attributes");
                if let Some(addr_type) = str_field(attrs, "Addr_type") {
                    let allowed_type = matches!(
                        addr_type.as_str(),
                        "PointAddress"
                            | "StreetAddress"
                            | "Subaddress"
                            | "StreetAddressExt"
                            | "BuildingName"
                            | "Parcel"
                    );
                    if !allowed_type {
                        return false;
                    }
                }
                let st_check = str_field(attrs, "StAddr")
                    .or_else(|| str_field(attrs, "Match_addr"))
                    .or_else(|| {
                        cand.get("address")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_default();
                let first_tok = st_check.split_whitespace().next().unwrap_or("");
                let starts_with_num = first_tok.chars().any(|c| c.is_ascii_digit());
                let has_street_letters =
                    st_check.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2;
                starts_with_num && has_street_letters
            })
        })
        .cloned();
    Ok(best.map(|b| (ESRI_FIND_CANDIDATES.to_string(), b)))
}

async fn nominatim_find_candidate(
    http: &HttpClient,
    addr: &str,
) -> Result<Option<(String, Value)>> {
    let mut u = Url::parse("https://nominatim.openstreetmap.org/search")?;
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("q", addr);
        qp.append_pair("format", "jsonv2");
        qp.append_pair("addressdetails", "1");
        qp.append_pair("limit", "3");
        qp.append_pair("countrycodes", "us");
    }
    let url_str = u.to_string();
    let j = http
        .get_json_opts(&url_str, 8_000, 1, 86400 * 14, None)
        .await?;
    let best = j.as_array().and_then(|arr| {
        arr.iter()
            .find(|item| {
                let lat = item
                    .get("lat")
                    .and_then(|v| v.as_str()?.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let lon = item
                    .get("lon")
                    .and_then(|v| v.as_str()?.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if !((17.5..=72.0).contains(&lat) && (-179.9..=-64.5).contains(&lon)) {
                    return false;
                }
                let addr_obj = item.get("address");
                let house_num = str_field(addr_obj, "house_number").unwrap_or_default();
                let road = str_field(addr_obj, "road").unwrap_or_default();
                !house_num.is_empty() && !road.is_empty()
            })
            .cloned()
    });
    Ok(best.map(|b| (url_str, b)))
}

pub async fn geocode_address(http: &HttpClient, address: &str) -> Result<ModuleOutput> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("address is required"));
    }

    let mut warnings = Vec::new();
    let mut sources = Vec::new();

    // 1. Primary U.S. Census Geocoder (`geographies/onelineaddress`)
    let mut primary_url = format!("{CENSUS_GEOS_BASE}/onelineaddress");
    let mut census_match: Option<Value> = None;

    if let Ok((u, j)) = census_oneline(http, trimmed).await {
        primary_url = u;
        if let Some(matches) = j
            .get("result")
            .and_then(|r| r.get("addressMatches"))
            .and_then(|m| m.as_array())
        {
            if !matches.is_empty() {
                if matches.len() > 1 {
                    let m0_addr = matches[0]
                        .get("matchedAddress")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    warnings.push(format!(
                        "{} candidate matches; using the first ({}).",
                        matches.len(),
                        m0_addr
                    ));
                }
                census_match = Some(matches[0].clone());
            }
        }
    }

    // 1b. If apartment/unit suffix prevented Census TIGER street-range match, retry without unit suffix
    if census_match.is_none() && RE_UNIT_SUFFIX.is_match(trimmed) {
        let stripped = RE_MULTI_SPACE
            .replace_all(&RE_UNIT_SUFFIX.replace_all(trimmed, ""), " ")
            .trim()
            .to_string();
        if stripped.len() >= 6 && stripped != trimmed {
            if let Ok((u, j)) = census_oneline(http, &stripped).await {
                if let Some(m) = j
                    .get("result")
                    .and_then(|r| r.get("addressMatches"))
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                {
                    primary_url = u;
                    census_match = Some(m.clone());
                }
            }
        }
    }

    if let Some(m) = census_match {
        let lat = m
            .get("coordinates")
            .and_then(|c| c.get("y"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow!("Census match missing latitude"))?;
        let lon = m
            .get("coordinates")
            .and_then(|c| c.get("x"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow!("Census match missing longitude"))?;
        let matched_addr = m
            .get("matchedAddress")
            .and_then(|v| v.as_str())
            .unwrap_or(trimmed)
            .to_string();
        let disp_addr = format_census_display_address(&m);
        let tl = m.get("tigerLine");
        let ac = m.get("addressComponents");
        let data = build_geocode_payload(
            trimmed,
            matched_addr,
            disp_addr,
            lat,
            lon,
            str_field(tl, "tigerLineId"),
            str_field(tl, "side"),
            str_field(ac, "state"),
            str_field(ac, "city").map(|s| title_case(&s)),
            str_field(ac, "zip"),
            m.get("geographies"),
            &mut warnings,
        );
        sources.push(SourceItem::new(
            "U.S. Census Bureau Geocoder (Public_AR_Current / Current_Current)",
            primary_url,
        ));
        return Ok(ModuleOutput {
            data,
            sources,
            warnings,
        });
    }

    // 2a. Optional BYOK fallback: ArcGIS World Geocoder (`ARCGIS_API_KEY`) + Census `geographies/coordinates`
    if let Ok(token) = std::env::var("ARCGIS_API_KEY") {
        let token = token.trim();
        if !token.is_empty() {
            if let Ok(Some((esri_url, cand))) = arcgis_find_candidate(http, trimmed, token).await {
                let lat = cand["location"]["y"].as_f64().unwrap_or(0.0);
                let lon = cand["location"]["x"].as_f64().unwrap_or(0.0);
                let attrs = cand.get("attributes");
                let match_addr = str_field(attrs, "Match_addr")
                    .or_else(|| {
                        cand.get("address")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| trimmed.to_string());
                let (disp_full, _, _) = split_us_address(&match_addr);
                let city = str_field(attrs, "City").map(|s| title_case(&s));
                let state = str_field(attrs, "RegionAbbr").map(|s| s.to_uppercase());
                let zip = str_field(attrs, "Postal");

                let mut coords_geos: Option<Value> = None;
                if let Ok((coords_url, cj)) = census_coordinates(http, lat, lon).await {
                    coords_geos = cj.get("result").and_then(|r| r.get("geographies")).cloned();
                    sources.push(SourceItem::new(
                        "U.S. Census Bureau Geography Coordinates API (Public_AR_Current / Current_Current)",
                        coords_url,
                    ));
                }
                sources.push(SourceItem::new(
                    "Esri ArcGIS World Geocoding Service (BYOK Address Fallback)",
                    esri_url,
                ));
                warnings.push(
                    "Resolved street/subaddress via ArcGIS World Geocoder and attached official U.S. Census geographies by coordinate."
                        .to_string(),
                );

                let data = build_geocode_payload(
                    trimmed,
                    match_addr.to_uppercase(),
                    disp_full,
                    lat,
                    lon,
                    None,
                    None,
                    state,
                    city,
                    zip,
                    coords_geos.as_ref(),
                    &mut warnings,
                );
                return Ok(ModuleOutput {
                    data,
                    sources,
                    warnings,
                });
            }
        }
    }

    // 2b. Keyless ODbL fallback: OpenStreetMap Nominatim + Census `geographies/coordinates`
    if let Ok(Some((nom_url, item))) = nominatim_find_candidate(http, trimmed).await {
        let lat = item
            .get("lat")
            .and_then(|v| v.as_str()?.parse::<f64>().ok())
            .unwrap_or(0.0);
        let lon = item
            .get("lon")
            .and_then(|v| v.as_str()?.parse::<f64>().ok())
            .unwrap_or(0.0);
        let addr_obj = item.get("address");
        let house_num = str_field(addr_obj, "house_number").unwrap_or_default();
        let road = str_field(addr_obj, "road").unwrap_or_default();
        let city = str_field(addr_obj, "city")
            .or_else(|| str_field(addr_obj, "town"))
            .or_else(|| str_field(addr_obj, "village"))
            .map(|s| title_case(&s));
        let state = addr_obj
            .and_then(|a| a.get("ISO3166-2-lvl4"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.strip_prefix("US-"))
            .map(|s| s.to_uppercase());
        let zip = str_field(addr_obj, "postcode").map(|z| z.chars().take(5).collect::<String>());
        let disp_full = [
            Some(format!("{house_num} {road}").trim().to_string()),
            city.clone(),
            Some(
                format!(
                    "{} {}",
                    state.as_deref().unwrap_or(""),
                    zip.as_deref().unwrap_or("")
                )
                .trim()
                .to_string(),
            ),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");

        let mut coords_geos: Option<Value> = None;
        if let Ok((coords_url, cj)) = census_coordinates(http, lat, lon).await {
            coords_geos = cj.get("result").and_then(|r| r.get("geographies")).cloned();
            sources.push(SourceItem::new(
                "U.S. Census Bureau Geography Coordinates API (Public_AR_Current / Current_Current)",
                coords_url,
            ));
        }
        sources.push(SourceItem::new(
            "© OpenStreetMap contributors (ODbL) — Nominatim Geocoder",
            nom_url,
        ));
        warnings.push(
            "Resolved street address via OpenStreetMap Nominatim and attached official U.S. Census geographies by coordinate."
                .to_string(),
        );

        let data = build_geocode_payload(
            trimmed,
            disp_full.to_uppercase(),
            disp_full,
            lat,
            lon,
            None,
            None,
            state,
            city,
            zip,
            coords_geos.as_ref(),
            &mut warnings,
        );
        return Ok(ModuleOutput {
            data,
            sources,
            warnings,
        });
    }

    // 2c. Authoritative municipal GIS address fallback for new-construction streets not yet
    // indexed in Census TIGER or OSM (City of Campbell Enterprise GIS Addresses Layer).
    let lower_addr = trimmed.to_ascii_lowercase();
    let campbell_hint = lower_addr.contains("campbell")
        || ["95008", "95009", "95011"]
            .iter()
            .any(|z| lower_addr.contains(z));
    if campbell_hint {
        if let Some(pt) =
            crate::property::permit_pipeline::fetch_campbell_address_point(http, trimmed).await
        {
            let canon_addr = if trimmed.contains(',') {
                trimmed.to_string()
            } else {
                format!("{}, Campbell, CA 95008", title_case(&pt.full_address))
            };
            let mut out = geocode_from_coordinates(
                http,
                &canon_addr,
                pt.lat,
                pt.lon,
                "City of Campbell Enterprise GIS Addresses Layer",
            )
            .await?;
            out.sources.push(SourceItem::new(
                "City of Campbell Enterprise GIS Addresses Layer",
                crate::property::permit_pipeline::CAMPBELL_ADDRESSES_LAYER,
            ));
            if let Some(obj) = out.data.as_object_mut() {
                if let Some(apn) = pt.apn.filter(|s| !s.is_empty()) {
                    obj.insert("apn".to_string(), json!(apn));
                }
            }
            return Ok(out);
        }
    }

    sources.push(SourceItem::new("U.S. Census Geocoder", primary_url));
    warnings.push(
        "Census Geocoder found no match. Check spelling, include city/state/ZIP; PO boxes and very new streets are not supported."
            .to_string(),
    );
    Ok(ModuleOutput {
        data: json!({
            "matched": false,
            "input": trimmed,
        }),
        sources,
        warnings,
    })
}

/// Resolves a US street address (such as a brand-new 2026 subdivision street not yet in TIGER)
/// using verified parcel/listing coordinates `(lat, lon)` + official U.S. Census `geographies/coordinates`.
pub async fn geocode_from_coordinates(
    http: &HttpClient,
    address: &str,
    lat: f64,
    lon: f64,
    source_label: &str,
) -> Result<ModuleOutput> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("address is required"));
    }
    if !(lat.is_finite()
        && lon.is_finite()
        && (17.5..=72.0).contains(&lat)
        && (-179.9..=-64.5).contains(&lon))
    {
        return Err(anyhow!("coordinates must be within the United States"));
    }

    let (disp_full_raw, primary_street, secondary_loc) = split_us_address(trimmed);
    let mut parsed_city: Option<String> = None;
    let mut parsed_state: Option<String> = None;
    let mut parsed_zip: Option<String> = None;

    if !secondary_loc.is_empty() {
        let sec_parts: Vec<&str> = secondary_loc.split(',').map(str::trim).collect();
        if let Some(first_city) = sec_parts.first().filter(|s| !s.is_empty()) {
            let has_digits = first_city.chars().any(|c| c.is_ascii_digit());
            if !has_digits {
                parsed_city = Some(title_case(first_city));
            }
        }
        for tok in secondary_loc.split(|c: char| c.is_whitespace() || c == ',') {
            let clean = tok.trim();
            if clean.len() == 2 && clean.chars().all(|c| c.is_ascii_alphabetic()) {
                let up = clean.to_ascii_uppercase();
                if state_fips_for_abbr(&up).is_some() {
                    parsed_state = Some(up);
                }
            } else if clean.len() >= 5 {
                let z5: String = clean.chars().take_while(|c| c.is_ascii_digit()).collect();
                if z5.len() == 5 {
                    parsed_zip = Some(z5);
                }
            }
        }
    }

    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    let mut coords_geos: Option<Value> = None;
    if let Ok((coords_url, cj)) = census_coordinates(http, lat, lon).await {
        coords_geos = cj.get("result").and_then(|r| r.get("geographies")).cloned();
        sources.push(SourceItem::new(
            "U.S. Census Bureau Geography Coordinates API (Public_AR_Current / Current_Current)",
            coords_url,
        ));
    }

    let disp_full = if !disp_full_raw.is_empty() {
        disp_full_raw
    } else if !primary_street.is_empty() {
        primary_street
    } else {
        trimmed.to_string()
    };

    warnings.push(format!(
        "Resolved parcel coordinates via {source_label} and attached official U.S. Census geographies by coordinate."
    ));

    let data = build_geocode_payload(
        trimmed,
        disp_full.to_uppercase(),
        disp_full,
        lat,
        lon,
        None,
        None,
        parsed_state,
        parsed_city,
        parsed_zip,
        coords_geos.as_ref(),
        &mut warnings,
    );
    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}
