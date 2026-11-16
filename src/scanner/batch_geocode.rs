// US Census Bulk Address Geocoder (`POST .../geocoder/geographies/addressbatch`)
// Geocodes up to 10,000+ US street addresses by streaming parallel 1,000-row
// multipart CSV uploads (`benchmark=Public_AR_Current`, `vintage=Current_Current`)
// with `futures::stream::BufferUnordered` and parsing variable-width TIGER/Line
// geography CSV responses via `csv::ReaderBuilder::new().has_headers(false).flexible(true)`.

use crate::shared::HttpClient;
use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

pub use crate::registry::endpoints::{
    CENSUS_BATCH_GEOCODE_URL as CENSUS_BATCH_ENDPOINT,
    CENSUS_BENCHMARK_PUBLIC_AR_CURRENT as CENSUS_BATCH_BENCHMARK,
    CENSUS_VINTAGE_CURRENT_CURRENT as CENSUS_BATCH_VINTAGE,
};
pub const CENSUS_BATCH_CHUNK_SIZE: usize = 1_000;
pub const CENSUS_BATCH_CONCURRENCY: usize = 4;

static RE_ZIP_ONLY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d{5})(?:-\d{4})?$").expect("zip regex"));
static RE_TRAILING_ZIP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(.*?)\s+(\d{5})(?:-\d{4})?$").expect("trailing zip regex"));
static RE_NO_COMMA_ADDR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?i)(.+?)\s+([A-Z]{2})(?:\s+(\d{5})(?:-\d{4})?)?$").expect("oneline regex")
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BatchAddressInput {
    pub id: String,
    pub street: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub zip: String,
}

impl BatchAddressInput {
    /// Parse a single-line US address string (with optional unit and comma-separated or trailing
    /// `City, ST ZIP` components) into structured Census batch CSV fields `(id, street, city, state, zip)`.
    pub fn from_oneline(id: impl Into<String>, raw_addr: &str) -> Self {
        let id = id.into();
        let cleaned = collapse_ws(raw_addr);
        if cleaned.is_empty() {
            return Self {
                id,
                ..Self::default()
            };
        }

        let parts: Vec<String> = cleaned
            .split(',')
            .map(collapse_ws)
            .filter(|p| !p.is_empty())
            .collect();

        match parts.as_slice() {
            [] => Self {
                id,
                ..Self::default()
            },
            [single] => parse_uncomma_address(id, single),
            [first, second] => {
                // Two comma-delimited segments: `<street>, <ZIP>` or `<street>, <City> [ST] [ZIP]`
                if let Some(caps) = RE_ZIP_ONLY.captures(second) {
                    return Self {
                        id,
                        street: first.clone(),
                        city: String::new(),
                        state: String::new(),
                        zip: caps[1].to_string(),
                    };
                }
                let (city, state, zip) = split_city_state_zip(second);
                Self {
                    id,
                    street: first.clone(),
                    city,
                    state,
                    zip,
                }
            }
            many => {
                let last = &many[many.len() - 1];
                // Case 1: Last comma-part is purely a 5-digit ZIP
                if let Some(caps) = RE_ZIP_ONLY.captures(last) {
                    let zip = caps[1].to_string();
                    let prev = &many[many.len() - 2];
                    if let Some(st) = normalize_us_state(prev) {
                        if many.len() >= 4 {
                            let city = many[many.len() - 3].clone();
                            let street = many[..many.len() - 3].join(", ");
                            return Self {
                                id,
                                street,
                                city,
                                state: st.to_string(),
                                zip,
                            };
                        } else {
                            // [<street>, <ST>, <ZIP>]
                            return Self {
                                id,
                                street: many[0].clone(),
                                city: String::new(),
                                state: st.to_string(),
                                zip,
                            };
                        }
                    } else {
                        // Missing state: [<street>, <city>, <ZIP>]
                        let city = prev.clone();
                        let street = many[..many.len() - 2].join(", ");
                        return Self {
                            id,
                            street,
                            city,
                            state: String::new(),
                            zip,
                        };
                    }
                }

                // Case 2: Standard "Street[, Unit], City, ST [ZIP]"
                let (tail_extra_city, state, zip) = parse_state_zip_tail(last);
                let mut city = many[many.len() - 2].clone();
                let street = many[..many.len() - 2].join(", ");
                if !tail_extra_city.is_empty() && state.is_empty() && zip.is_empty() {
                    // Unrecognized trailing token (e.g., "USA"): check if `city` actually holds "CA 95124"
                    if last.eq_ignore_ascii_case("usa")
                        || last.eq_ignore_ascii_case("us")
                        || last.eq_ignore_ascii_case("united states")
                    {
                        let mut trimmed = many[..many.len() - 1].join(", ");
                        if trimmed.is_empty() {
                            trimmed = cleaned;
                        }
                        return Self::from_oneline(id, &trimmed);
                    }
                } else if !tail_extra_city.is_empty() && city.is_empty() {
                    city = tail_extra_city;
                }

                Self {
                    id,
                    street,
                    city,
                    state,
                    zip,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BatchGeocodeHit {
    pub id: String,
    pub matched: bool,
    pub matched_address: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub state_fips: Option<String>,
    pub county_fips: Option<String>,
    pub tract: Option<String>,
    pub block: Option<String>,
}

impl BatchGeocodeHit {
    pub fn unmatched(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            matched: false,
            matched_address: None,
            lat: None,
            lon: None,
            state_fips: None,
            county_fips: None,
            tract: None,
            block: None,
        }
    }

    /// 11-digit Census Tract GEOID (`SSCCCTTTTTT`) when state, county, and tract are present.
    pub fn tract_geoid(&self) -> Option<String> {
        Some(format!(
            "{}{}{}",
            self.state_fips.as_deref()?,
            self.county_fips.as_deref()?,
            self.tract.as_deref()?
        ))
    }
}

/// Bulk-geocode US street addresses via the official US Census Geocoder `geographies/addressbatch`
/// CSV multipart endpoint (or `endpoint_override` for local high-speed mock testing).
///
/// Splits large inputs into 1,000-row CSV chunks and executes up to 4 chunk uploads in parallel,
/// returning results aligned 1-to-1 in the exact order of `inputs`.
pub async fn census_batch_geocode(
    http: &HttpClient,
    inputs: &[BatchAddressInput],
    endpoint_override: Option<&str>,
) -> Result<Vec<BatchGeocodeHit>> {
    census_batch_geocode_with_timeout(http, inputs, endpoint_override, 90_000).await
}

/// `Send`-safe bulk Census batch geocoder with explicit per-chunk `timeout_ms`.
/// Collects owned chunk vectors so the returned `Future` satisfies `Send + 'static` inside
/// `tokio::spawn` and Axum handlers without HRTB borrow issues.
pub async fn census_batch_geocode_with_timeout(
    http: &HttpClient,
    inputs: &[BatchAddressInput],
    endpoint_override: Option<&str>,
    timeout_ms: u64,
) -> Result<Vec<BatchGeocodeHit>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }

    let endpoint = endpoint_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(CENSUS_BATCH_ENDPOINT)
        .to_string();

    let indexed_chunks: Vec<(usize, Vec<BatchAddressInput>)> = inputs
        .chunks(CENSUS_BATCH_CHUNK_SIZE)
        .map(|c| c.to_vec())
        .enumerate()
        .collect();

    let mut chunk_results: Vec<(usize, Vec<BatchGeocodeHit>)> = stream::iter(indexed_chunks)
        .map(|(chunk_idx, chunk)| {
            let endpoint = endpoint.clone();
            async move {
                let hits = geocode_single_chunk(http, &endpoint, &chunk, timeout_ms).await?;
                Ok::<(usize, Vec<BatchGeocodeHit>), anyhow::Error>((chunk_idx, hits))
            }
        })
        .buffer_unordered(CENSUS_BATCH_CONCURRENCY)
        .collect::<Vec<Result<(usize, Vec<BatchGeocodeHit>)>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    chunk_results.sort_by_key(|(idx, _)| *idx);

    let mut out = Vec::with_capacity(inputs.len());
    for (_, hits) in chunk_results {
        out.extend(hits);
    }
    Ok(out)
}

async fn geocode_single_chunk(
    http: &HttpClient,
    endpoint: &str,
    chunk: &[BatchAddressInput],
    timeout_ms: u64,
) -> Result<Vec<BatchGeocodeHit>> {
    let csv_bytes = encode_census_batch_csv(chunk)?;
    let boundary = format!("----openestatecensus{}", Uuid::new_v4().simple());
    let body = build_census_multipart_body(
        &boundary,
        CENSUS_BATCH_BENCHMARK,
        CENSUS_BATCH_VINTAGE,
        &csv_bytes,
    );
    let content_type = format!("multipart/form-data; boundary={boundary}");

    let raw_response = http
        .scrape_post(endpoint)
        .timeout_ms(timeout_ms)
        .retries(2)
        .body_bytes(body, Some(content_type))
        .bytes()
        .await
        .with_context(|| format!("Census batch POST {endpoint}"))?;

    parse_census_batch_csv(&raw_response, chunk)
}

fn encode_census_batch_csv(chunk: &[BatchAddressInput]) -> Result<Vec<u8>> {
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(Vec::with_capacity(chunk.len() * 64));
    for row in chunk {
        wtr.write_record([&row.id, &row.street, &row.city, &row.state, &row.zip])
            .context("Failed writing Census batch CSV record")?;
    }
    wtr.into_inner()
        .map_err(|e| anyhow!("Failed finalizing Census batch CSV buffer: {}", e))
}

/// Build an RFC 7578 `multipart/form-data` request body with `benchmark`, `vintage`, and `addressFile`.
fn build_census_multipart_body(
    boundary: &str,
    benchmark: &str,
    vintage: &str,
    csv_bytes: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(csv_bytes.len() + 512);
    append_text_field(&mut buf, boundary, "benchmark", benchmark);
    append_text_field(&mut buf, boundary, "vintage", vintage);

    buf.extend_from_slice(b"--");
    buf.extend_from_slice(boundary.as_bytes());
    buf.extend_from_slice(
        b"\r\nContent-Disposition: form-data; name=\"addressFile\"; filename=\"addresses.csv\"\r\n",
    );
    buf.extend_from_slice(b"Content-Type: text/csv; charset=utf-8\r\n\r\n");
    buf.extend_from_slice(csv_bytes);
    buf.extend_from_slice(b"\r\n--");
    buf.extend_from_slice(boundary.as_bytes());
    buf.extend_from_slice(b"--\r\n");
    buf
}

fn append_text_field(buf: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    buf.extend_from_slice(b"--");
    buf.extend_from_slice(boundary.as_bytes());
    buf.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(b"\"\r\n\r\n");
    buf.extend_from_slice(value.as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// Parse the Census `geographies/addressbatch` CSV response:
/// - Unmatched (`No_Match` / `Tie`): `id, input_addr, indicator`
/// - Matched (`Match`): `id, input_addr, "Match", match_type, matched_addr, "lon,lat", tiger_id, side, state_fips, county_fips, tract, block`
fn parse_census_batch_csv(
    csv_bytes: &[u8],
    expected_chunk: &[BatchAddressInput],
) -> Result<Vec<BatchGeocodeHit>> {
    // Strip UTF-8 BOM if present
    let slice = csv_bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(csv_bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::Fields)
        .from_reader(slice);

    let mut by_id: HashMap<String, VecDeque<BatchGeocodeHit>> =
        HashMap::with_capacity(expected_chunk.len());

    for result in rdr.records() {
        let rec = match result {
            Ok(r) => r,
            Err(_) => continue,
        };
        let Some(raw_id) = rec.get(0).map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let indicator = rec.get(2).map(str::trim).unwrap_or("");
        let is_match = indicator.eq_ignore_ascii_case("match");

        let hit = if is_match {
            let matched_address = non_empty_opt(rec.get(4));
            let (lon, lat) = rec
                .get(5)
                .and_then(parse_lon_lat_pair)
                .map(|(lo, la)| (Some(lo), Some(la)))
                .unwrap_or((None, None));
            let coords_valid = lat.is_some() && lon.is_some();

            BatchGeocodeHit {
                id: raw_id.to_string(),
                matched: coords_valid,
                matched_address,
                lat,
                lon,
                state_fips: non_empty_opt(rec.get(8)),
                county_fips: non_empty_opt(rec.get(9)),
                tract: non_empty_opt(rec.get(10)),
                block: non_empty_opt(rec.get(11)),
            }
        } else {
            BatchGeocodeHit::unmatched(raw_id)
        };

        by_id.entry(hit.id.clone()).or_default().push_back(hit);
    }

    let mut ordered = Vec::with_capacity(expected_chunk.len());
    for input in expected_chunk {
        if let Some(hit) = by_id.get_mut(&input.id).and_then(|q| q.pop_front()) {
            ordered.push(hit);
        } else {
            ordered.push(BatchGeocodeHit::unmatched(&input.id));
        }
    }
    Ok(ordered)
}

fn parse_lon_lat_pair(raw: &str) -> Option<(f64, f64)> {
    let mut parts = raw.split(',');
    let lon = parts.next()?.trim().parse::<f64>().ok()?;
    let lat = parts.next()?.trim().parse::<f64>().ok()?;
    if lon.is_finite() && lat.is_finite() && (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat) {
        Some((lon, lat))
    } else {
        None
    }
}

fn non_empty_opt(val: Option<&str>) -> Option<String> {
    val.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_uncomma_address(id: String, single: &str) -> BatchAddressInput {
    if let Some(caps) = RE_NO_COMMA_ADDR.captures(single) {
        let prefix = caps[1].trim().to_string();
        let st_cand = &caps[2];
        let zip = caps.get(3).map(|m| m.as_str().to_string()).unwrap_or_default();
        if let Some(state) = normalize_us_state(st_cand) {
            return BatchAddressInput {
                id,
                street: prefix,
                city: String::new(),
                state: state.to_string(),
                zip,
            };
        }
    }
    if let Some(caps) = RE_TRAILING_ZIP.captures(single) {
        return BatchAddressInput {
            id,
            street: caps[1].trim().to_string(),
            city: String::new(),
            state: String::new(),
            zip: caps[2].to_string(),
        };
    }
    BatchAddressInput {
        id,
        street: single.to_string(),
        city: String::new(),
        state: String::new(),
        zip: String::new(),
    }
}

fn split_city_state_zip(segment: &str) -> (String, String, String) {
    let (rem, state, zip) = parse_state_zip_tail(segment);
    if !state.is_empty() || !zip.is_empty() {
        (rem, state, zip)
    } else {
        (segment.to_string(), String::new(), String::new())
    }
}

fn parse_state_zip_tail(segment: &str) -> (String, String, String) {
    let seg = segment.trim();
    if seg.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    if let Some(caps) = RE_ZIP_ONLY.captures(seg) {
        return (String::new(), String::new(), caps[1].to_string());
    }
    if let Some(st) = normalize_us_state(seg) {
        return (String::new(), st.to_string(), String::new());
    }

    let (before_zip, zip) = if let Some(caps) = RE_TRAILING_ZIP.captures(seg) {
        let bz = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        let zp = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
        (bz, zp)
    } else {
        (seg, String::new())
    };

    if before_zip.is_empty() {
        return (String::new(), String::new(), zip);
    }
    if let Some(st) = normalize_us_state(before_zip) {
        return (String::new(), st.to_string(), zip);
    }

    // Try matching 1-word or 2-word trailing state name (e.g. "San Jose CA" or "albany New York")
    let tokens: Vec<&str> = before_zip.split_whitespace().collect();
    if tokens.len() >= 2 {
        let two_word = format!("{} {}", tokens[tokens.len() - 2], tokens[tokens.len() - 1]);
        if let Some(st) = normalize_us_state(&two_word) {
            let prefix = tokens[..tokens.len() - 2].join(" ");
            return (prefix, st.to_string(), zip);
        }
    }
    if let Some(&last_tok) = tokens.last() {
        if let Some(st) = normalize_us_state(last_tok) {
            let prefix = tokens[..tokens.len() - 1].join(" ");
            return (prefix, st.to_string(), zip);
        }
    }

    (before_zip.to_string(), String::new(), zip)
}

fn normalize_us_state(raw: &str) -> Option<&'static str> {
    let s = raw.trim().trim_matches('.');
    if s.is_empty() || s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    crate::registry::us_geo::normalize_us_state_abbr(s)
}
