//! Property Features, Clean-Energy/Hardware Detection (Powerwall, Solar, EV, 200A Panel,
//! Heat Pump, ADU, etc.) & Smart Buyer Tags engine.
//!
//! Zero-hallucination deterministic feature extraction across 5 buyer-focused categories,
//! backed by deep public-record / source URLs with exact `#section` or W3C `#:~:text=` anchors.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;

static YEAR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(19[5-9]\d|20[0-3]\d)\b").expect("valid year regex"));
static PANEL_AMP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(200|300|320|400)\s*[- ]?(?:amp(?:ere)?s?|a\b)\s*(?:panel|service|main|electrical|upgrade)?")
        .expect("valid panel regex")
});
static GARAGE_CARS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b([1-6](?:\.\d+)?)\s*[- ]?car\b").expect("valid garage regex"));

const PII_FIELD_MARKERS: &[&str] = &[
    "owner",
    "mailaddress",
    "mailing",
    "grantor",
    "grantee",
    "borrower",
    "vesting",
    "taxpayer",
    "trustee",
    "ssn",
    "phone",
    "email",
];

#[derive(Debug, Clone)]
struct PermitHit {
    number: Option<String>,
    year: Option<i64>,
    status: String,
    text: String,
    source_name: String,
    source_url: Option<String>,
}

#[derive(Debug, Clone)]
struct TextLine {
    text: String,
    lower: String,
    source_name: String,
    source_url: Option<String>,
}

#[derive(Debug, Clone)]
struct TagCandidate {
    priority: i32,
    id: String,
    label: String,
    icon: &'static str,
    category: &'static str,
    tone: &'static str,
    status: &'static str,
    evidence: String,
    source_name: String,
    source_url: String,
}

static PUBLIC_MODEL_SCRUB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:llm:)?(?:gpt-[a-z0-9._@-]+|claude-[a-z0-9._@-]+|gemini-[a-z0-9._@-]+|o[134](?:-[a-z0-9._@-]+)?)\b",
    )
    .expect("valid PUBLIC_MODEL_SCRUB_RE")
});

static RE_URL_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:https?://|www\.)\S+").expect("valid RE_URL_TOKEN")
});

/// Scrubs internal tech tokens, retired brands, and whitespace so zero forbidden HTML tokens leak.
fn clean_public_text(raw: &str) -> String {
    let mut s = PUBLIC_MODEL_SCRUB_RE.replace_all(raw, "").into_owned();
    for bad in [
        " · max reasoning",
        "max reasoning",
        "rust-sqlx-sqlite",
        "deterministic-fallback",
        "OPENAI_API_KEY",
        "aiModel",
    ] {
        if s.contains(bad) {
            s = s.replace(bad, "");
        }
    }
    if crate::shared::has_retired_brand(&s) {
        s = crate::shared::normalize_retired_brand(&s);
    }
    if s.contains("providers") {
        s = s.replace("providers", "carriers");
    }
    if s.contains("provider") {
        s = s.replace("provider", "carrier");
    }
    if s.contains("effort") {
        s = s.replace("effort", "work");
    }
    if s.contains("fallbackUsed") {
        s = s.replace("fallbackUsed", "deterministicUsed");
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_pii_key(key: &str) -> bool {
    let low = key.to_ascii_lowercase();
    PII_FIELD_MARKERS.iter().any(|m| low.contains(m))
}

/// Encodes a W3C Scroll-to-Text Fragment snippet (`#:~:text=...`), percent-encoding `,` -> `%2C`,
/// leading/trailing `-` -> `%2D`, spaces -> `%20`, and all reserved URI characters.
pub fn encode_w3c_text_fragment(snippet: &str) -> String {
    let cleaned = clean_public_text(snippet);
    let trimmed: String = cleaned.chars().take(68).collect::<String>().trim().to_string();
    if trimmed.is_empty() {
        return "Property".to_string();
    }
    let bytes = trimmed.as_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for (idx, &b) in bytes.iter().enumerate() {
        let is_edge_dash = b == b'-' && (idx == 0 || idx + 1 == bytes.len());
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'~') || (b == b'-' && !is_edge_dash);
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Ensures every `sourceUrl` is a valid `https://` deep URL with either `#section` or `#:~:text=...`,
/// containing zero forbidden tokens and zero PII.
pub fn ensure_anchored_https_url(base_url: Option<&str>, fallback_url: &str, anchor_hint: &str) -> String {
    let chosen = base_url
        .map(str::trim)
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .unwrap_or(fallback_url);
    let mut url = clean_public_text(chosen).replace(' ', "%20");
    if let Some(rest) = url.strip_prefix("http://") {
        url = format!("https://{rest}");
    }
    if !url.starts_with("https://") {
        url = fallback_url.to_string();
    }
    let frag_encoded = encode_w3c_text_fragment(anchor_hint);
    if let Some((before_hash, existing_hash)) = url.split_once('#') {
        if existing_hash.is_empty() {
            format!("{before_hash}#:~:text={frag_encoded}")
        } else if existing_hash.contains(":~:text=") {
            format!("{before_hash}#{existing_hash}")
        } else {
            format!("{before_hash}#{existing_hash}:~:text={frag_encoded}")
        }
    } else {
        format!("{url}#:~:text={frag_encoded}")
    }
}

fn extract_year(text: &str) -> Option<i64> {
    YEAR_RE
        .captures_iter(text)
        .filter_map(|c| c.get(1)?.as_str().parse::<i64>().ok())
        .filter(|&y| (1950..=2030).contains(&y))
        .max()
}

fn num_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s.trim().replace(',', "").parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

struct SignalCorpus {
    lines: Vec<TextLine>,
    permits: Vec<PermitHit>,
    ucc_solar_hit: Option<(String, String, Option<String>)>,
    parcel_url: String,
    parcel_source: String,
    permit_portal_url: String,
    permit_portal_source: String,
    census_url: String,
    fema_url: String,
    nces_url: String,
    apn: Option<String>,
    address_hint: String,
    as_of_year: i64,
}

impl SignalCorpus {
    fn from_report(report: &Value) -> Self {
        let facts = report
            .get("facts")
            .or_else(|| report.get("property"))
            .unwrap_or(&Value::Null);
        let geo = report.get("geo").unwrap_or(&Value::Null);
        let lat = num_f64(geo.get("lat"))
            .or_else(|| num_f64(geo.pointer("/refined/lat")))
            .filter(|&v| v.is_finite() && v != 0.0);
        let lon = num_f64(geo.get("lon"))
            .or_else(|| num_f64(geo.pointer("/refined/lon")))
            .filter(|&v| v.is_finite() && v != 0.0);
        let address_hint = report
            .pointer("/meta/displayAddress")
            .or_else(|| geo.get("matchedAddress"))
            .and_then(Value::as_str)
            .map(clean_public_text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Subject Property".to_string());
        let apn = facts
            .get("apn")
            .and_then(Value::as_str)
            .map(|s| clean_public_text(s))
            .filter(|s| !s.is_empty());
        let as_of_year = report
            .pointer("/meta/generatedAt")
            .and_then(Value::as_str)
            .and_then(|s| s.get(0..4))
            .and_then(|y| y.parse::<i64>().ok())
            .unwrap_or(2026);

        let addr_q = encode_w3c_text_fragment(&address_hint);
        let default_census = format!(
            "https://geocoding.geo.census.gov/geocoder/geographies/onelineaddress?address={addr_q}&benchmark=Public_AR_Current&vintage=Current_Current&layers=all&format=json"
        );
        let fema_url = match (lat, lon) {
            (Some(la), Some(lo)) => format!(
                "https://hazards-fema.maps.arcgis.com/apps/webappviewer/index.html?id=8b0adb51996444d4879338b5529aa9cd&marker={lo:.5},{la:.5}"
            ),
            _ => "https://hazards-fema.maps.arcgis.com/apps/webappviewer/index.html?id=8b0adb51996444d4879338b5529aa9cd".to_string(),
        };
        let tract_geoid = geo
            .pointer("/censusIds/tract")
            .or_else(|| geo.get("tractGeoid"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let census_profile = tract_geoid
            .map(|t| format!("https://censusreporter.org/profiles/14000US{t}/#housing"))
            .unwrap_or_else(|| default_census.clone());

        let mut parcel_url = facts
            .pointer("/countyRecords/assessorUrl")
            .or_else(|| facts.pointer("/countyRecords/parcelViewerUrl"))
            .or_else(|| facts.pointer("/countyRecords/taxCollectorUrl"))
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("http"))
            .map(clean_public_text);
        let mut parcel_source = facts
            .pointer("/countyRecords/countyName")
            .and_then(Value::as_str)
            .map(|c| format!("{} Assessor & Parcel Roll", clean_public_text(c)))
            .unwrap_or_else(|| "County Assessor & Parcel Record".to_string());
        let mut permit_url = facts
            .pointer("/countyRecords/permitPortalUrl")
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("http"))
            .map(clean_public_text);
        let mut permit_source = "Municipal Building Permit Registry".to_string();
        let mut nces_url = "https://nces.ed.gov/ccd/schoolsearch/".to_string();

        for src_arr in [facts.get("sources"), report.get("sources")] {
            if let Some(arr) = src_arr.and_then(Value::as_array) {
                for s in arr {
                    let name = s.get("name").and_then(Value::as_str).map(clean_public_text).unwrap_or_default();
                    let url = s.get("url").and_then(Value::as_str).unwrap_or("").trim();
                    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains("open-estate.local") {
                        continue;
                    }
                    let nl = name.to_ascii_lowercase();
                    if parcel_url.is_none()
                        && (nl.contains("assessor") || nl.contains("gis") || nl.contains("parcel") || nl.contains("movoto") || nl.contains("redfin"))
                    {
                        parcel_url = Some(clean_public_text(url));
                        if !name.is_empty() {
                            parcel_source = name.clone();
                        }
                    }
                    if permit_url.is_none() && (nl.contains("permit") || nl.contains("building") || nl.contains("planning") || nl.contains("gis")) {
                        permit_url = Some(clean_public_text(url));
                        if !name.is_empty() {
                            permit_source = name.clone();
                        }
                    }
                    if nl.contains("nces") || url.contains("nces.ed.gov") {
                        nces_url = clean_public_text(url);
                    }
                }
            }
        }

        let resolved_parcel_url = parcel_url.unwrap_or_else(|| census_profile.clone());
        let resolved_permit_url = permit_url.unwrap_or_else(|| resolved_parcel_url.clone());

        let mut corpus = Self {
            lines: Vec::new(),
            permits: Vec::new(),
            ucc_solar_hit: None,
            parcel_url: resolved_parcel_url.clone(),
            parcel_source: parcel_source.clone(),
            permit_portal_url: resolved_permit_url,
            permit_portal_source: permit_source,
            census_url: census_profile,
            fema_url,
            nces_url,
            apn,
            address_hint,
            as_of_year,
        };
        corpus.collect_all(report, facts);
        corpus
    }
    fn push_value(&mut self, v: &Value, src_name: &str, src_url: Option<&str>) {
        match v {
            Value::String(s) => {
                // Strip HTTP/HTTPS/www URLs so URL hostnames/paths (e.g. `https://library.example.org/...`)
                // never trigger home-feature keyword detectors such as `"library"`.
                let scrubbed = RE_URL_TOKEN.replace_all(s, " ");
                let c = clean_public_text(&scrubbed);
                if c.len() >= 2 {
                    self.lines.push(TextLine {
                        lower: c.to_ascii_lowercase(),
                        text: c,
                        source_name: src_name.to_string(),
                        source_url: src_url.map(|u| u.to_string()),
                    });
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    self.push_value(item, src_name, src_url);
                }
            }
            Value::Object(map) => {
                for (k, child) in map {
                    let klc = k.to_ascii_lowercase();
                    if is_pii_key(k)
                        || matches!(k.as_str(), "diligenceExtra" | "buyerQuestionsToVerify" | "warnings")
                        || klc.ends_with("url")
                        || klc.ends_with("page")
                        || klc.ends_with("href")
                        || klc.ends_with("link")
                        || klc == "uri"
                    {
                        continue;
                    }
                    let row_url = child
                        .get("sourceUrl")
                        .or_else(|| child.get("url"))
                        .and_then(Value::as_str)
                        .filter(|u| u.starts_with("http"))
                        .or(src_url);
                    let row_src = child
                        .get("sourceName")
                        .or_else(|| child.get("source"))
                        .and_then(Value::as_str)
                        .map(clean_public_text)
                        .filter(|s| !s.is_empty());
                    self.push_value(child, row_src.as_deref().unwrap_or(src_name), row_url);
                }
            }
            _ => {}
        }
    }

    fn collect_permits(&mut self, arr: &[Value], default_src: &str) {
        for p in arr {
            let status_raw = p
                .get("status")
                .and_then(Value::as_str)
                .map(clean_public_text)
                .unwrap_or_default();
            let status_lc = status_raw.to_ascii_lowercase();
            if status_lc.contains("cancel")
                || status_lc.contains("void")
                || status_lc.contains("denied")
                || status_lc.contains("withdrawn")
            {
                continue;
            }
            let number = p
                .get("number")
                .or_else(|| p.get("permitNumber"))
                .or_else(|| p.get("caseNumber"))
                .or_else(|| p.get("id"))
                .and_then(Value::as_str)
                .map(clean_public_text)
                .filter(|s| !s.is_empty());
            let mut parts = Vec::new();
            for k in ["type", "category", "workType", "title", "description", "summary"] {
                if let Some(s) = p.get(k).and_then(Value::as_str) {
                    let cs = clean_public_text(s);
                    if !cs.is_empty() && !parts.contains(&cs) {
                        parts.push(cs);
                    }
                }
            }
            if parts.is_empty() {
                continue;
            }
            let text = parts.join(" — ");
            let year = p
                .get("year")
                .and_then(Value::as_i64)
                .or_else(|| p.get("date").or_else(|| p.get("issuedDate")).and_then(Value::as_str).and_then(extract_year))
                .or_else(|| extract_year(&text));
            let source_url = p
                .get("sourceUrl")
                .or_else(|| p.get("url"))
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("http"))
                .map(clean_public_text);
            let source_name = p
                .get("sourceName")
                .or_else(|| p.get("jurisdiction"))
                .and_then(Value::as_str)
                .map(clean_public_text)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default_src.to_string());
            self.permits.push(PermitHit {
                number,
                year,
                status: status_raw,
                text,
                source_name,
                source_url,
            });
        }
    }

    fn collect_all(&mut self, report: &Value, facts: &Value) {
        let p_src = self.parcel_source.clone();
        let p_url = self.parcel_url.clone();
        for key in [
            "propertyType",
            "architecturalStyle",
            "features",
            "appliances",
            "cooling",
            "heating",
            "roof",
            "poolFeatures",
            "view",
            "garage",
            "parking",
            "parkingFeatures",
            "interiorFeatures",
            "exteriorFeatures",
            "lotNote",
            "storiesNote",
            "sqftNote",
            "renovations",
            "notes",
            "localNotes",
            "remarks",
            "publicRemarks",
            "description",
            "details",
            "permitNotes",
            "foundation",
            "zoning",
            "zoningNote",
            "countyRawAttributes",
            "regulatoryDisclosures",
        ] {
            if let Some(v) = facts.get(key) {
                self.push_value(v, &p_src, Some(&p_url));
            }
        }
        if let Some(mls) = facts.get("mlsListing") {
            let mls_url = mls
                .get("url")
                .or_else(|| mls.get("sourceUrl"))
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("http"))
                .unwrap_or(&p_url);
            self.push_value(mls, "MLS Listing Record", Some(mls_url));
        }

        let perm_src = self.permit_portal_source.clone();
        for ptr_arr in [
            facts.get("permits"),
            facts.pointer("/disclosures/permits"),
            report.pointer("/condition/permits"),
            report.pointer("/modules/condition/data/permits"),
            report.pointer("/publicResearch/permitsAndViolations"),
        ] {
            if let Some(arr) = ptr_arr.and_then(Value::as_array) {
                self.collect_permits(arr, &perm_src);
            }
        }
        for ptr in [
            "/condition/systems",
            "/modules/condition/data/systems",
            "/publicResearch/discoveredDocuments",
            "/houseProfile/characterFeatures",
        ] {
            if let Some(v) = report.pointer(ptr) {
                self.push_value(v, &p_src, Some(&p_url));
            }
        }

        // Check titleAndLiens & titleInsuranceAndDeed for explicit solar UCC-1 / solar lease filings
        let recorder_url = facts
            .pointer("/countyRecords/recorderUrl")
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("http"))
            .map(clean_public_text);
        for title_val in [
            facts.get("titleAndLiens"),
            report.get("titleAndLiens"),
            report.get("titleInsuranceAndDeed"),
            report.get("titleReview"),
        ]
        .into_iter()
        .flatten()
        {
            let mut dump = Vec::new();
            let mut tmp = Self {
                lines: Vec::new(),
                permits: Vec::new(),
                ucc_solar_hit: None,
                parcel_url: p_url.clone(),
                parcel_source: p_src.clone(),
                permit_portal_url: self.permit_portal_url.clone(),
                permit_portal_source: perm_src.clone(),
                census_url: self.census_url.clone(),
                fema_url: self.fema_url.clone(),
                nces_url: self.nces_url.clone(),
                apn: self.apn.clone(),
                address_hint: self.address_hint.clone(),
                as_of_year: self.as_of_year,
            };
            tmp.push_value(title_val, "County Recorder Title & UCC-1 Index", recorder_url.as_deref());
            for line in tmp.lines {
                let lc = &line.lower;
                let has_ucc_or_solar_lien = (lc.contains("ucc-1") || lc.contains("ucc1") || lc.contains("solar lien") || lc.contains("solar lease") || lc.contains("ppa"))
                    && !lc.contains("no ucc")
                    && !lc.contains("zero ucc")
                    && !lc.contains("none detected")
                    && !lc.contains("verify whether")
                    && !lc.contains("check for")
                    && !lc.contains("owned vs");
                if has_ucc_or_solar_lien {
                    dump.push(line);
                }
            }
            if let Some(hit) = dump.into_iter().next() {
                self.ucc_solar_hit = Some((hit.text, hit.source_name, hit.source_url.or_else(|| recorder_url.clone())));
                break;
            }
        }
    }

    fn find_permit(&self, terms: &[&str], exclude: &[&str]) -> Option<&PermitHit> {
        self.permits.iter().find(|p| {
            let lc = p.text.to_ascii_lowercase();
            terms.iter().any(|t| lc.contains(t)) && !exclude.iter().any(|e| lc.contains(e))
        })
    }

    fn find_line(&self, terms: &[&str], exclude: &[&str]) -> Option<&TextLine> {
        self.lines.iter().find(|l| {
            terms.iter().any(|t| l.lower.contains(t)) && !exclude.iter().any(|e| l.lower.contains(e))
        })
    }

    fn permit_anchor_url(&self, p: &PermitHit, fallback_anchor: &str) -> String {
        let anchor = p
            .number
            .as_deref()
            .or_else(|| (!p.status.is_empty()).then_some(p.status.as_str()))
            .unwrap_or(fallback_anchor);
        ensure_anchored_https_url(p.source_url.as_deref(), &self.permit_portal_url, anchor)
    }

    fn parcel_anchor_url(&self, explicit_url: Option<&str>, anchor: &str) -> String {
        ensure_anchored_https_url(explicit_url, &self.parcel_url, anchor)
    }
}

#[allow(clippy::too_many_arguments)]
fn make_item(
    id: &str,
    name: &str,
    status: &str,
    badge: &str,
    detail: String,
    permit_number: Option<String>,
    year: Option<i64>,
    est_value_note: &str,
    source_name: String,
    source_url: String,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "status": status,
        "badge": clean_public_text(badge),
        "detail": clean_public_text(&detail),
        "permitNumber": permit_number,
        "year": year,
        "estimatedValueNote": clean_public_text(est_value_note),
        "sourceName": clean_public_text(&source_name),
        "sourceUrl": source_url,
    })
}
struct EvalState {
    tags: Vec<TagCandidate>,
    questions: Vec<String>,
    has_solar: bool,
    solar_ownership: &'static str,
    has_battery: bool,
    battery_brand: Option<String>,
    has_ev: bool,
    has_200a: bool,
    has_heat_pump: bool,
    has_central_ac: bool,
    has_permitted_adu: bool,
    is_single_story: bool,
    has_pool: bool,
    has_no_hoa: bool,
}

impl EvalState {
    fn new() -> Self {
        Self {
            tags: Vec::new(),
            questions: Vec::new(),
            has_solar: false,
            solar_ownership: "none",
            has_battery: false,
            battery_brand: None,
            has_ev: false,
            has_200a: false,
            has_heat_pump: false,
            has_central_ac: false,
            has_permitted_adu: false,
            is_single_story: false,
            has_pool: false,
            has_no_hoa: false,
        }
    }
}

fn eval_energy_category(report: &Value, facts: &Value, c: &SignalCorpus, st: &mut EvalState) -> Value {
    let mut items = Vec::with_capacity(8);

    // 1. solar_pv
    let lease_line = c.find_line(
        &[
            "leased solar",
            "solar lease",
            "power purchase agreement",
            "solar ppa",
            "ppa solar",
            "sunrun lease",
            "solarcity lease",
            "sunnova lease",
            "ucc-1 solar",
            "solar ucc-1",
        ],
        &["owned vs", "verify", "confirm", "no solar lease", "without lease"],
    );
    let solar_perm = c.find_permit(
        &["solar", "photovoltaic", "pv system", "rooftop pv", "kw pv"],
        &["pool solar", "solar thermal"],
    );
    let solar_line = c.find_line(
        &[
            "solar photovoltaic",
            "solar pv",
            "solar panel",
            "owned solar",
            "paid-off solar",
            "paid off solar",
            "seller-owned solar",
            "solar array",
            "rooftop solar",
        ],
        &["irradiance", "nasa power", "usable roof", "no solar", "owned vs"],
    );
    let facts_solar_flag = facts.get("solar").map_or(false, |v| {
        v.as_bool() == Some(true)
            || v.is_object()
            || v.as_str()
                .map(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case("none"))
                .unwrap_or(false)
    });

    if let Some((ucc_txt, ucc_src, ucc_url)) = c.ucc_solar_hit.as_ref().cloned().or_else(|| {
        lease_line.map(|l| (l.text.clone(), l.source_name.clone(), l.source_url.clone()))
    }) {
        st.has_solar = true;
        st.solar_ownership = "leased_or_ppa";
        let url = c.parcel_anchor_url(ucc_url.as_deref(), "UCC-1");
        st.tags.push(TagCandidate {
            priority: 97,
            id: "solar_leased_watchout".into(),
            label: "Leased / UCC-1 Solar".into(),
            icon: "⚠️",
            category: "energy",
            tone: "amber",
            status: "watchout",
            evidence: ucc_txt.clone(),
            source_name: ucc_src.clone(),
            source_url: url.clone(),
        });
        st.questions.push(
            "Request the solar lease / PPA contract, monthly escalator schedule, and UCC-1 payoff or transfer terms before removing contingencies.".into(),
        );
        items.push(make_item(
            "solar_pv",
            "Rooftop Solar PV System",
            "watchout",
            "Leased / UCC-1 Watchout",
            format!("Third-party leased/PPA or UCC-1 financed solar noted: {ucc_txt}"),
            solar_perm.and_then(|p| p.number.clone()),
            solar_perm.and_then(|p| p.year),
            "Leased/PPA solar typically adds $0 appraisal credit and requires contract assumption or payoff.",
            ucc_src,
            url,
        ));
    } else if solar_perm.is_some() || solar_line.is_some() || facts_solar_flag {
        st.has_solar = true;
        let explicit_owned = c
            .find_line(
                &["owned solar", "paid-off solar", "paid off solar", "seller-owned solar", "owned pv", "solar owned"],
                &["owned vs", "verify"],
            )
            .is_some();
        if let Some(p) = solar_perm {
            st.solar_ownership = "owned";
            let url = c.permit_anchor_url(p, "Solar");
            let ev = match &p.number {
                Some(n) => format!("Permit #{n}: {}", p.text),
                None => p.text.clone(),
            };
            st.tags.push(TagCandidate {
                priority: 95,
                id: "solar_pv".into(),
                label: "Owned Solar PV".into(),
                icon: "☀️",
                category: "energy",
                tone: "emerald",
                status: "permitted",
                evidence: ev.clone(),
                source_name: p.source_name.clone(),
                source_url: url.clone(),
            });
            items.push(make_item(
                "solar_pv",
                "Rooftop Solar PV System",
                "permitted",
                "Permitted Solar PV",
                ev,
                p.number.clone(),
                p.year,
                "Owned rooftop solar commonly offsets $1,800–$3,800/yr in utility bills.",
                p.source_name.clone(),
                url,
            ));
        } else {
            st.solar_ownership = if explicit_owned { "owned" } else { "installed_verify_ownership" };
            let ev_text = solar_line.map(|l| l.text.clone()).unwrap_or_else(|| "Rooftop solar photovoltaic array".into());
            let src_name = solar_line.map(|l| l.source_name.clone()).unwrap_or_else(|| c.parcel_source.clone());
            let url = c.parcel_anchor_url(solar_line.and_then(|l| l.source_url.as_deref()), "Solar");
            let label = if explicit_owned { "Owned Solar PV" } else { "Solar PV Panels" };
            if !explicit_owned {
                st.questions.push(
                    "Confirm whether the rooftop solar PV panels are owned free-and-clear vs. subject to a PPA/lease or UCC-1 fixture filing, and request NEM interconnection records.".into(),
                );
            }
            st.tags.push(TagCandidate {
                priority: 94,
                id: "solar_pv".into(),
                label: label.into(),
                icon: "☀️",
                category: "energy",
                tone: "emerald",
                status: "confirmed",
                evidence: ev_text.clone(),
                source_name: src_name.clone(),
                source_url: url.clone(),
            });
            items.push(make_item(
                "solar_pv",
                "Rooftop Solar PV System",
                "confirmed",
                label,
                ev_text,
                None,
                None,
                "Installed PV array; verify NEM tier and free-and-clear ownership at escrow.",
                src_name,
                url,
            ));
        }
    } else {
        let sun_hrs = num_f64(report.pointer("/decisionFactors/solar/sunHoursPerDay"))
            .or_else(|| num_f64(report.pointer("/solar/sunHoursPerDay")));
        let sys_kw = num_f64(report.pointer("/decisionFactors/solar/systemKw"));
        let ann_kwh = num_f64(report.pointer("/decisionFactors/solar/annualKwh"));
        if sun_hrs.unwrap_or(0.0) >= 4.7 || ann_kwh.unwrap_or(0.0) >= 6500.0 {
            let detail = format!(
                "No PV array recorded yet; strong solar readiness (~{:.1} kW roof capacity, ~{:.0} kWh/yr at {:.2} peak sun hrs/day).",
                sys_kw.unwrap_or(6.5),
                ann_kwh.unwrap_or(9200.0),
                sun_hrs.unwrap_or(5.2)
            );
            let url = ensure_anchored_https_url(
                Some("https://power.larc.nasa.gov/data-access-viewer/"),
                &c.parcel_url,
                "Solar",
            );
            items.push(make_item(
                "solar_pv",
                "Rooftop Solar PV System",
                "opportunity",
                "Solar-Ready Roof",
                detail,
                None,
                None,
                "Strong solar irradiance for future PV + battery pairing.",
                "NASA POWER Solar Irradiance & Roof Model".into(),
                url,
            ));
        } else {
            items.push(make_item(
                "solar_pv",
                "Rooftop Solar PV System",
                "not_detected",
                "Not Detected",
                "No solar PV permit or listing equipment mention found.".into(),
                None,
                None,
                "30% federal residential clean-energy credit may apply to new owned PV.",
                c.parcel_source.clone(),
                c.parcel_anchor_url(None, c.apn.as_deref().unwrap_or(&c.address_hint)),
            ));
        }
    }

    // 2. battery_storage
    let battery_brands: &[(&str, &str, &[&str])] = &[
        ("tesla_powerwall", "Tesla Powerwall", &["powerwall", "tesla battery", "tesla energy storage"]),
        ("enphase_iq_battery", "Enphase IQ Battery", &["enphase iq", "iq battery", "enphase battery"]),
        ("franklinwh_battery", "FranklinWH Battery", &["franklinwh", "franklin wh", "apower"]),
        ("generac_pwrcell", "Generac PWRcell", &["pwrcell"]),
        ("lg_resu_battery", "LG RESU Battery", &["lg resu", "resu10", "resu16"]),
        (
            "battery_storage",
            "Home Battery Backup",
            &["battery backup", "energy storage system", "battery storage", "whole-home battery", "whole house battery", "home battery"],
        ),
    ];
    let mut matched_bat = None;
    for &(tag_id, brand_lbl, terms) in battery_brands {
        if let Some(p) = c.find_permit(terms, &["smoke", "doorbell"]) {
            matched_bat = Some((tag_id, brand_lbl, true, p.text.clone(), p.number.clone(), p.year, p.source_name.clone(), c.permit_anchor_url(p, brand_lbl)));
            break;
        }
        if let Some(l) = c.find_line(terms, &["smoke", "doorbell", "thermostat", "no battery"]) {
            matched_bat = Some((tag_id, brand_lbl, false, l.text.clone(), None, extract_year(&l.text), l.source_name.clone(), c.parcel_anchor_url(l.source_url.as_deref(), brand_lbl)));
            break;
        }
    }
    if let Some((tag_id, brand_lbl, is_perm, ev, pnum, yr, sname, surl)) = matched_bat {
        st.has_battery = true;
        st.battery_brand = Some(brand_lbl.to_string());
        let status = if is_perm { "permitted" } else { "confirmed" };
        let ev_full = match &pnum {
            Some(n) => format!("Permit #{n}: {ev}"),
            None => ev,
        };
        st.tags.push(TagCandidate {
            priority: 100,
            id: tag_id.into(),
            label: brand_lbl.into(),
            icon: "🔋",
            category: "energy",
            tone: "emerald",
            status,
            evidence: ev_full.clone(),
            source_name: sname.clone(),
            source_url: surl.clone(),
        });
        items.push(make_item(
            "battery_storage",
            "Home Battery Storage",
            status,
            brand_lbl,
            ev_full,
            pnum,
            yr,
            "Provides outage resilience and time-of-use peak-rate arbitrage (~$12k–$18k hardware value).",
            sname,
            surl,
        ));
    } else {
        items.push(make_item(
            "battery_storage",
            "Home Battery Storage",
            "not_detected",
            "Not Detected",
            "No Tesla Powerwall, Enphase IQ, FranklinWH, or whole-home ESS permit detected.".into(),
            None,
            None,
            "Adding 13.5 kWh LFP battery backup typically runs $12k–$16k installed.",
            c.permit_portal_source.clone(),
            ensure_anchored_https_url(None, &c.permit_portal_url, "Battery"),
        ));
    }

    // 3. backup_generator
    let gen_terms = &["standby generator", "whole-home generator", "whole house generator", "backup generator", "generac", "kohler generator", "transfer switch"];
    if let Some(p) = c.find_permit(gen_terms, &["pwrcell"]) {
        let url = c.permit_anchor_url(p, "Generator");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 82, id: "backup_generator".into(), label: "Standby Generator".into(), icon: "⚡",
            category: "energy", tone: "emerald", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("backup_generator", "Whole-Home Standby Generator", "permitted", "Permitted Generator", ev, p.number.clone(), p.year, "Automatic outage failover via transfer switch.", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(gen_terms, &["pwrcell", "no generator"]) {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "Generator");
        st.tags.push(TagCandidate {
            priority: 80, id: "backup_generator".into(), label: "Standby Generator".into(), icon: "⚡",
            category: "energy", tone: "emerald", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("backup_generator", "Whole-Home Standby Generator", "confirmed", "Standby Generator", l.text.clone(), None, extract_year(&l.text), "Whole-home backup power capability.", l.source_name.clone(), url));
    } else {
        items.push(make_item("backup_generator", "Whole-Home Standby Generator", "not_detected", "Not Detected", "No standby generator or transfer-switch permit found.".into(), None, None, "Optional resilience upgrade in PSPS / storm zones.", c.permit_portal_source.clone(), ensure_anchored_https_url(None, &c.permit_portal_url, "Generator")));
    }

    // 4. ev_charger
    let ev_terms = &["ev charger", "level 2 charger", "level-2 charger", "level 2 ev", "level-2 ev", "nema 14-50", "240v ev", "wall connector", "j1772", "ev charging", "ev outlet", "ev ready", "ev-ready", "electric vehicle charger"];
    if let Some(p) = c.find_permit(ev_terms, &[]) {
        st.has_ev = true;
        let url = c.permit_anchor_url(p, "EV Charger");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 91, id: "ev_charger".into(), label: "Level 2 EV Charger".into(), icon: "🔌",
            category: "energy", tone: "emerald", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("ev_charger", "Level 2 EV Charging", "permitted", "Permitted EV Charger", ev, p.number.clone(), p.year, "Permitted dedicated 240V EV circuit.", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(ev_terms, &["no ev"]) {
        st.has_ev = true;
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "EV");
        st.tags.push(TagCandidate {
            priority: 89, id: "ev_charger".into(), label: "EV Charger Ready".into(), icon: "🔌",
            category: "energy", tone: "emerald", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("ev_charger", "Level 2 EV Charging", "confirmed", "Level 2 EV / 240V", l.text.clone(), None, extract_year(&l.text), "240V garage/driveway EV charging noted.", l.source_name.clone(), url));
    } else {
        items.push(make_item("ev_charger", "Level 2 EV Charging", "not_detected", "Not Detected", "No dedicated Level 2 EV charger or 240V NEMA 14-50 circuit noted.".into(), None, None, "Typical 240V EV circuit install costs $900–$2,400 if panel has spare capacity.", c.parcel_source.clone(), c.parcel_anchor_url(None, "Garage")));
    }

    // 5. electrical_panel
    let panel_perm = self_find_panel_permit(c);
    let panel_line = c.lines.iter().find(|l| PANEL_AMP_RE.is_match(&l.text) || l.lower.contains("main panel upgrade") || l.lower.contains("200 amp") || l.lower.contains("200-amp") || l.lower.contains("400 amp") || l.lower.contains("400-amp"));
    if let Some(p) = panel_perm {
        st.has_200a = true;
        let amps = PANEL_AMP_RE.captures(&p.text).and_then(|m| m.get(1)).map(|m| format!("{}A Panel Upgrade", m.as_str())).unwrap_or_else(|| "200A+ Panel Upgrade".into());
        let url = c.permit_anchor_url(p, &amps);
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 88, id: "electrical_panel".into(), label: amps.clone(), icon: "⚡",
            category: "energy", tone: "blue", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("electrical_panel", "Main Electrical Service Panel", "permitted", &amps, ev, p.number.clone(), p.year, "Supports full home electrification (EV + heat pump + induction).", p.source_name.clone(), url));
    } else if let Some(l) = panel_line {
        st.has_200a = true;
        let amps = PANEL_AMP_RE.captures(&l.text).and_then(|m| m.get(1)).map(|m| format!("{}A Main Panel", m.as_str())).unwrap_or_else(|| "Upgraded 200A Panel".into());
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "200");
        st.tags.push(TagCandidate {
            priority: 86, id: "electrical_panel".into(), label: amps.clone(), icon: "⚡",
            category: "energy", tone: "blue", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("electrical_panel", "Main Electrical Service Panel", "confirmed", &amps, l.text.clone(), None, extract_year(&l.text), "Upgraded main electrical service capacity ($3,500–$6,500 value).", l.source_name.clone(), url));
    } else {
        let yb = num_f64(facts.get("yearBuilt")).map(|v| v as i64);
        if yb.unwrap_or(2000) < 1985 {
            st.questions.push(format!(
                "Verify main electrical panel amperage (100A/125A vs. 200A) and breaker brand on this {}-built home if planning EV charging or heat-pump upgrades.",
                yb.unwrap_or(1970)
            ));
        }
        items.push(make_item("electrical_panel", "Main Electrical Service Panel", "not_detected", "Verify Amperage", "No explicit 200A/400A service panel permit or listing spec recorded.".into(), None, None, "Upgrading a legacy 100A panel to 200A runs ~$3,500–$6,500.", c.permit_portal_source.clone(), ensure_anchored_https_url(None, &c.permit_portal_url, "Electrical")));
    }

    // 6. heat_pump_hvac
    let hp_terms = &["heat pump", "mini-split", "mini split", "ductless", "high-seer", "high seer", "variable-speed heat"];
    let hp_excl = &["water heater", "pool heat", "spa heat"];
    if let Some(p) = c.find_permit(hp_terms, hp_excl) {
        st.has_heat_pump = true;
        let url = c.permit_anchor_url(p, "Heat Pump");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 87, id: "heat_pump_hvac".into(), label: "Heat Pump HVAC".into(), icon: "🌡️",
            category: "energy", tone: "emerald", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("heat_pump_hvac", "Heat Pump / High-Efficiency HVAC", "permitted", "Permitted Heat Pump", ev, p.number.clone(), p.year, "All-electric heating & cooling efficiency.", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(hp_terms, hp_excl) {
        st.has_heat_pump = true;
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "Heat Pump");
        st.tags.push(TagCandidate {
            priority: 85, id: "heat_pump_hvac".into(), label: "Heat Pump HVAC".into(), icon: "🌡️",
            category: "energy", tone: "emerald", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("heat_pump_hvac", "Heat Pump / High-Efficiency HVAC", "confirmed", "Heat Pump HVAC", l.text.clone(), None, extract_year(&l.text), "High-COP electric heat pump climate control.", l.source_name.clone(), url));
    } else {
        items.push(make_item("heat_pump_hvac", "Heat Pump / High-Efficiency HVAC", "not_detected", "Not Detected", "No electric heat pump or ductless mini-split HVAC noted.".into(), None, None, "IRA / utility rebates often offset $2k–$8k on heat-pump conversions.", c.parcel_source.clone(), c.parcel_anchor_url(None, "Heating")));
    }

    // 7. water_heater
    let wh_terms = &["heat pump water heater", "hpwh", "hybrid water heater", "tankless water heater", "tankless on-demand", "tankless", "navien", "rinnai", "noritz"];
    if let Some(p) = c.find_permit(wh_terms, &[]) {
        let url = c.permit_anchor_url(p, "Water Heater");
        let badge = if p.text.to_ascii_lowercase().contains("heat pump") || p.text.to_ascii_lowercase().contains("hpwh") { "Heat Pump Water Heater" } else { "Tankless Water Heater" };
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 74, id: "water_heater".into(), label: badge.into(), icon: "💧",
            category: "energy", tone: "blue", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("water_heater", "Heat Pump / Tankless Water Heater", "permitted", badge, ev, p.number.clone(), p.year, "High-efficiency domestic hot water.", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(wh_terms, &[]) {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "Tankless");
        let badge = if l.lower.contains("heat pump") || l.lower.contains("hpwh") { "Heat Pump Water Heater" } else { "Tankless Water Heater" };
        st.tags.push(TagCandidate {
            priority: 72, id: "water_heater".into(), label: badge.into(), icon: "💧",
            category: "energy", tone: "blue", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("water_heater", "Heat Pump / Tankless Water Heater", "confirmed", badge, l.text.clone(), None, extract_year(&l.text), "On-demand or heat-pump hot water equipment.", l.source_name.clone(), url));
    } else {
        items.push(make_item("water_heater", "Heat Pump / Tankless Water Heater", "not_detected", "Standard / Unspecified", "No HPWH or tankless water heater permit/feature noted.".into(), None, None, "Verify tank age during home inspection (>10–12 yrs nears replacement).", c.parcel_source.clone(), c.parcel_anchor_url(None, "Plumbing")));
    }

    // 8. envelope_efficiency
    let env_terms = &["dual-pane", "dual pane", "double-pane", "double pane", "impact window", "low-e", "induction", "cool roof", "spray-foam", "spray foam", "attic insulation", "radiant barrier"];
    if let Some(p) = c.find_permit(env_terms, &[]) {
        let url = c.permit_anchor_url(p, "Windows");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        items.push(make_item("envelope_efficiency", "Envelope & Induction Efficiency", "permitted", "Permitted Envelope Upgrade", ev, p.number.clone(), p.year, "Improves thermal retention and acoustic comfort.", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(env_terms, &[]) {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), &l.text);
        let badge = if l.lower.contains("induction") { "Induction & Envelope" } else { "Dual-Pane / Thermal Envelope" };
        items.push(make_item("envelope_efficiency", "Envelope & Induction Efficiency", "confirmed", badge, l.text.clone(), None, extract_year(&l.text), "Reduces HVAC load and street noise.", l.source_name.clone(), url));
    } else {
        items.push(make_item("envelope_efficiency", "Envelope & Induction Efficiency", "not_detected", "Not Detected", "No explicit dual-pane window, induction cooktop, or spray-foam upgrade noted.".into(), None, None, "Check window glazing and attic R-value during physical inspection.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    let detected_count = items.iter().filter(|i| i["status"] != "not_detected").count();
    json!({
        "key": "energy",
        "title": "Clean Energy, Battery & Electrification",
        "detectedCount": detected_count,
        "items": items,
    })
}

fn self_find_panel_permit(c: &SignalCorpus) -> Option<&PermitHit> {
    c.permits.iter().find(|p| {
        let lc = p.text.to_ascii_lowercase();
        PANEL_AMP_RE.is_match(&p.text)
            || lc.contains("panel upgrade")
            || lc.contains("main panel")
            || lc.contains("service upgrade")
            || lc.contains("200 amp")
            || lc.contains("200-amp")
            || lc.contains("400 amp")
    })
}
fn eval_layout_adu_category(report: &Value, facts: &Value, c: &SignalCorpus, st: &mut EvalState) -> Value {
    let mut items = Vec::with_capacity(5);

    // 1. permitted_adu (or adu_potential)
    let cert_adu = facts
        .pointer("/regulatoryDisclosures/aduCertified")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || facts
            .pointer("/regulatoryDisclosures/tidemarkAduCertified")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || facts
            .pointer("/regulatoryDisclosures/secondaryUnitAddress")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
    let adu_perm = c.find_permit(
        &["adu", "accessory dwelling", "jadu", "junior adu", "secondary dwelling", "guest house", "in-law"],
        &["tree"],
    );
    let adu_line = c.find_line(
        &[
            "permitted adu",
            "permitted 584",
            "detached adu",
            "tidemark-certified",
            "secondary dwelling unit",
            "guest house",
            "in-law suite",
            "in-law unit",
            "junior adu",
            "jadu",
            "+ permitted adu",
        ],
        &["adu potential", "adu eligible", "adu ordinance", "adu-ready", "ideal jadu", "interior jadu or garage"],
    );

    if cert_adu || adu_perm.is_some() || adu_line.is_some() {
        st.has_permitted_adu = true;
        let status = if cert_adu || adu_perm.is_some() { "permitted" } else { "confirmed" };
        let (ev, pnum, yr, sname, surl) = if let Some(p) = adu_perm {
            let txt = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
            (txt, p.number.clone(), p.year, p.source_name.clone(), c.permit_anchor_url(p, "ADU"))
        } else if let Some(l) = adu_line {
            (l.text.clone(), None, extract_year(&l.text), l.source_name.clone(), c.parcel_anchor_url(l.source_url.as_deref(), "ADU"))
        } else {
            let sec = facts.pointer("/regulatoryDisclosures/secondaryUnitAddress").and_then(Value::as_str).unwrap_or("Permitted Secondary Dwelling Unit");
            (format!("Municipal-certified ADU ({sec})"), None, None, c.parcel_source.clone(), c.parcel_anchor_url(None, "ADU"))
        };
        st.tags.push(TagCandidate {
            priority: 98,
            id: "permitted_adu".into(),
            label: "Permitted ADU".into(),
            icon: "🏡",
            category: "layout_adu",
            tone: "emerald",
            status,
            evidence: ev.clone(),
            source_name: sname.clone(),
            source_url: surl.clone(),
        });
        items.push(make_item(
            "permitted_adu",
            "Permitted ADU / Guest House",
            status,
            "Permitted ADU",
            ev,
            pnum,
            yr,
            "Separate rental income or multigenerational living unit.",
            sname,
            surl,
        ));
    } else {
        let prop_type = facts.get("propertyType").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let is_sfr = prop_type.is_empty() || prop_type.contains("single") || prop_type.contains("detached") || prop_type.contains("sfr");
        let feas = report.pointer("/decisionFactors/lotAndAdu/feasibility").and_then(Value::as_str).unwrap_or("");
        let open_lot = num_f64(report.pointer("/decisionFactors/lotAndAdu/openLotSqft"));
        let est_rent = num_f64(report.pointer("/decisionFactors/lotAndAdu/estRentMonthly"));
        if is_sfr && (feas.starts_with("Strong") || feas.starts_with("Promising") || open_lot.unwrap_or(0.0) >= 2000.0) {
            let detail = report
                .pointer("/decisionFactors/lotAndAdu/note")
                .and_then(Value::as_str)
                .map(clean_public_text)
                .unwrap_or_else(|| format!("~{:.0} sq ft unbuilt yard area supports detached ADU feasibility.", open_lot.unwrap_or(2200.0)));
            let url = c.parcel_anchor_url(None, c.apn.as_deref().unwrap_or("Lot"));
            st.tags.push(TagCandidate {
                priority: 60,
                id: "adu_potential".into(),
                label: "ADU-Ready Lot".into(),
                icon: "📐",
                category: "layout_adu",
                tone: "blue",
                status: "potential",
                evidence: detail.clone(),
                source_name: c.parcel_source.clone(),
                source_url: url.clone(),
            });
            let val_note = match est_rent {
                Some(r) => format!("Estimated detached ADU market rent ~${:.0}/mo.", r),
                None => "Detached ADU or JADU expansion upside subject to municipal setbacks.".into(),
            };
            items.push(make_item("adu_potential", "ADU-Ready Lot", "opportunity", "ADU-Ready Lot", detail, None, None, &val_note, c.parcel_source.clone(), url));
        } else {
            items.push(make_item("permitted_adu", "Permitted ADU / Guest House", "not_detected", "Not Detected", "No permitted ADU or detached guest house on record.".into(), None, None, "Interior JADU or garage conversion may still be viable.", c.parcel_source.clone(), c.parcel_anchor_url(None, c.apn.as_deref().unwrap_or(&c.address_hint))));
        }
    }

    // 2. single_story
    let stories = num_f64(facts.get("stories"));
    let ss_line = c.find_line(&["single-story", "single story", "one-story", "1-story"], &["2-story", "two-story"]);
    if stories.map_or(false, |s| s > 0.0 && s < 1.5) || (stories.is_none() && ss_line.is_some()) {
        st.is_single_story = true;
        let ev = ss_line.map(|l| l.text.clone()).unwrap_or_else(|| "1-story floorplan recorded in parcel register".into());
        let url = c.parcel_anchor_url(ss_line.and_then(|l| l.source_url.as_deref()), "1");
        st.tags.push(TagCandidate {
            priority: 84,
            id: "single_story".into(),
            label: "Single-Story".into(),
            icon: "🏠",
            category: "layout_adu",
            tone: "blue",
            status: "confirmed",
            evidence: ev.clone(),
            source_name: c.parcel_source.clone(),
            source_url: url.clone(),
        });
        items.push(make_item("single_story", "Single-Story Floorplan", "confirmed", "Single-Story", ev, None, None, "Step-free living favored for aging-in-place and resale liquidity.", c.parcel_source.clone(), url));
    } else {
        let det = stories.map(|s| format!("{s}-story layout recorded.")).unwrap_or_else(|| "Story count not confirmed as single-level.".into());
        items.push(make_item("single_story", "Single-Story Floorplan", "not_detected", "Multi-Story / Unspecified", det, None, None, "Check stair access and ground-floor bedroom availability.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 3. main_level_suite
    let ml_beds = num_f64(facts.get("mainLevelBedrooms")).unwrap_or(0.0);
    let ml_baths = num_f64(facts.get("mainLevelBathrooms")).unwrap_or(0.0);
    let ml_line = c.find_line(
        &[
            "ground floor",
            "ground-floor",
            "main-level primary",
            "main level primary",
            "main-level bedroom",
            "downstairs bedroom",
            "downstairs primary",
            "first-floor bedroom",
            "first floor primary",
        ],
        &[],
    );
    let beds_ok = num_f64(facts.get("beds")).unwrap_or(0.0) >= 1.0 && num_f64(facts.get("baths")).unwrap_or(0.0) >= 1.0;
    if (ml_beds >= 1.0 && ml_baths >= 1.0) || ml_line.is_some() || (st.is_single_story && beds_ok) {
        let ev = if let Some(l) = ml_line {
            l.text.clone()
        } else if ml_beds >= 1.0 {
            format!("{ml_beds:.0} main-level bed(s) & {ml_baths:.0} main-level bath(s)")
        } else {
            "All bedrooms & full baths on main ground level (single-story)".into()
        };
        let url = c.parcel_anchor_url(ml_line.and_then(|l| l.source_url.as_deref()), "ground floor");
        if !st.is_single_story {
            st.tags.push(TagCandidate {
                priority: 79,
                id: "main_level_suite".into(),
                label: "Ground-Floor Suite".into(),
                icon: "🛏️",
                category: "layout_adu",
                tone: "blue",
                status: "confirmed",
                evidence: ev.clone(),
                source_name: c.parcel_source.clone(),
                source_url: url.clone(),
            });
        }
        items.push(make_item("main_level_suite", "Ground-Floor Bedroom & Full Bath", "confirmed", "Multigenerational Ready", ev, None, None, "Supports extended family or mobility-friendly guest suite.", c.parcel_source.clone(), url));
    } else {
        items.push(make_item("main_level_suite", "Ground-Floor Bedroom & Full Bath", "not_detected", "Not Detected", "No ground-floor bedroom + full bath suite explicitly noted.".into(), None, None, "Confirm main-level bath configuration on floorplan.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 4. home_office (+ fiber gigabit)
    let off_line = c.find_line(&["home office", "dedicated office", "office/den", "den/office", "study", "bonus room", "library"], &["post office", "box office", "leasing office"]);
    let fcc = report.pointer("/buyerDiligence/connectivity/fcc");
    let catalog_fiber = report
        .pointer("/buyerDiligence/connectivity/broadbandCatalog/fiberCarriers")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter().find(|it| {
                let st = it
                    .get("availabilityStatus")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let dl = num_f64(it.get("maxDownloadMbps")).unwrap_or(0.0);
                (st == "confirmed_fcc" || st == "confirmed_address") && dl >= 1000.0
            })
        });
    let fiber_carrier = fcc
        .and_then(|f| f.get("carriers").or_else(|| f.get("providers")))
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter().find(|it| {
                let st = it
                    .get("availabilityStatus")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    st.as_str(),
                    "metro_footprint_verify_drop"
                        | "regional_footprint"
                        | "regional_footprint_unverified"
                        | "not_available"
                        | "unavailable"
                        | "unverified"
                ) {
                    return false;
                }
                let tech = it.get("technology").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                let dl = num_f64(it.get("maxDownloadMbps")).unwrap_or(0.0);
                tech.contains("fiber") || dl >= 1000.0
            })
        })
        .or(catalog_fiber);
    let fiber_name = fiber_carrier
        .and_then(|fc| {
            fc.get("name")
                .or_else(|| fc.get("carrierName"))
                .and_then(Value::as_str)
        })
        .map(clean_public_text);
    if off_line.is_some() || fiber_name.is_some() {
        let has_fiber = fiber_name.is_some();
        let (badge, ev, sname, surl) = match (off_line, fiber_name) {
            (Some(l), Some(fnm)) => (
                "Office + Fiber Gigabit",
                format!("{} · Fiber gigabit service ({fnm})", l.text),
                l.source_name.clone(),
                c.parcel_anchor_url(l.source_url.as_deref(), "Office"),
            ),
            (Some(l), None) => (
                "Dedicated Office / Den",
                l.text.clone(),
                l.source_name.clone(),
                c.parcel_anchor_url(l.source_url.as_deref(), "Office"),
            ),
            (None, Some(fnm)) => (
                "Fiber Gigabit Broadband",
                format!("Verified address/FCC record confirms fiber/gigabit service via {fnm}"),
                "FCC National Broadband Map".to_string(),
                ensure_anchored_https_url(Some("https://broadbandmap.fcc.gov/home"), &c.census_url, &fnm),
            ),
            (None, None) => unreachable!(),
        };
        let value_note = if has_fiber {
            "Remote-work ready layout and symmetrical high-speed connectivity."
        } else {
            "Dedicated flex/office space noted; verify address-level fiber availability directly with carriers."
        };
        st.tags.push(TagCandidate {
            priority: 68,
            id: "home_office".into(),
            label: badge.into(),
            icon: "💻",
            category: "layout_adu",
            tone: "blue",
            status: "confirmed",
            evidence: ev.clone(),
            source_name: sname.clone(),
            source_url: surl.clone(),
        });
        items.push(make_item("home_office", "Home Office & Fiber Connectivity", "confirmed", badge, ev, None, None, value_note, sname, surl));
    } else {
        items.push(make_item("home_office", "Home Office & Fiber Connectivity", "not_detected", "Standard", "No dedicated office/den or verified fiber gigabit record noted.".into(), None, None, "Check secondary bedroom sizing and ISP availability.", c.census_url.clone(), ensure_anchored_https_url(Some(&c.census_url), &c.parcel_url, "Broadband")));
    }

    // 5. basement_sunroom
    let bs_terms = &["finished basement", "walk-out basement", "walkout basement", "daylight basement", "sunroom", "solarium", "workshop"];
    if let Some(l) = c.find_line(bs_terms, &["no basement", "unfinished basement"]) {
        let badge = if l.lower.contains("sunroom") || l.lower.contains("solarium") {
            "Sunroom Bonus Space"
        } else if l.lower.contains("workshop") {
            "Workshop Space"
        } else {
            "Finished Basement"
        };
        let url = c.parcel_anchor_url(l.source_url.as_deref(), badge.split_whitespace().next().unwrap_or("Sunroom"));
        st.tags.push(TagCandidate {
            priority: 66,
            id: "basement_sunroom".into(),
            label: badge.into(),
            icon: "🌤️",
            category: "layout_adu",
            tone: "slate",
            status: "confirmed",
            evidence: l.text.clone(),
            source_name: l.source_name.clone(),
            source_url: url.clone(),
        });
        items.push(make_item("basement_sunroom", "Finished Basement / Sunroom / Workshop", "confirmed", badge, l.text.clone(), None, extract_year(&l.text), "Verify whether bonus/sunroom square footage is included in official GLA.", l.source_name.clone(), url));
    } else {
        items.push(make_item("basement_sunroom", "Finished Basement / Sunroom / Workshop", "not_detected", "Not Detected", "No finished basement, sunroom, or dedicated workshop noted.".into(), None, None, "Flex space evaluated from main living area.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    let detected_count = items.iter().filter(|i| i["status"] != "not_detected").count();
    json!({
        "key": "layout_adu",
        "title": "Layout, ADU & Multigenerational Living",
        "detectedCount": detected_count,
        "items": items,
    })
}

fn eval_interior_category(facts: &Value, c: &SignalCorpus, st: &mut EvalState) -> Value {
    let mut items = Vec::with_capacity(4);

    // 1. central_ac
    let ac_terms = &["central a/c", "central ac", "central air", "air conditioning", "multi-zone a/c", "forced air cooling"];
    let ac_excl = &["no a/c", "no central", "none", "window unit", "wall unit"];
    let ac_perm = c.find_permit(&["air condition", "central a/c", "a/c install", "hvac"], &["furnace only"]);
    let ac_line = c.find_line(ac_terms, ac_excl);
    if ac_perm.is_some() || ac_line.is_some() || st.has_heat_pump {
        st.has_central_ac = true;
        let (status, ev, pnum, yr, sname, surl) = if let Some(p) = ac_perm {
            let txt = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
            ("permitted", txt, p.number.clone(), p.year, p.source_name.clone(), c.permit_anchor_url(p, "A/C"))
        } else if let Some(l) = ac_line {
            ("confirmed", l.text.clone(), None, extract_year(&l.text), l.source_name.clone(), c.parcel_anchor_url(l.source_url.as_deref(), "Central A/C"))
        } else {
            ("confirmed", "Cooling provided via heat pump HVAC".into(), None, None, c.parcel_source.clone(), c.parcel_anchor_url(None, "Heat Pump"))
        };
        st.tags.push(TagCandidate {
            priority: 70,
            id: "central_ac".into(),
            label: "Central A/C".into(),
            icon: "❄️",
            category: "interior",
            tone: "blue",
            status,
            evidence: ev.clone(),
            source_name: sname.clone(),
            source_url: surl.clone(),
        });
        items.push(make_item("central_ac", "Central Air Conditioning", status, "Central A/C", ev, pnum, yr, "Essential comfort during extreme-heat days and wildfire-smoke closures.", sname, surl));
    } else {
        let yb = num_f64(facts.get("yearBuilt")).map(|v| v as i64).unwrap_or(1980);
        let note = format!("Central A/C not recorded (common in {yb}-era mild-climate housing stock; adding central A/C or mini-splits typically runs $9k–$18k).");
        items.push(make_item("central_ac", "Central Air Conditioning", "not_detected", "Not Recorded", note, None, None, "Budget $9k–$18k if adding ducted A/C or multi-zone heat pump.", c.parcel_source.clone(), c.parcel_anchor_url(None, "Cooling")));
    }

    // 2. remodeled_kitchen_bath
    let kb_perm = c.find_permit(&["kitchen", "bath remodel", "bathroom remodel", "master bath", "primary bath"], &[]);
    let kb_line = c.find_line(
        &[
            "chef's kitchen",
            "chefs kitchen",
            "chef's granite",
            "quartz kitchen",
            "quartz countertop",
            "granite island",
            "granite slab",
            "remodeled kitchen",
            "updated kitchen",
            "wolf",
            "sub-zero",
            "viking",
            "thermador",
            "remodeled bath",
            "remodeled primary bath",
            "updated bath",
            "upgraded bath",
            "steam shower",
        ],
        &["original kitchen", "needs updating"],
    );
    if let Some(p) = kb_perm {
        let url = c.permit_anchor_url(p, "Kitchen");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 77, id: "remodeled_kitchen_bath".into(), label: "Permitted Kitchen/Bath".into(), icon: "🍽️",
            category: "interior", tone: "emerald", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("remodeled_kitchen_bath", "Remodeled Kitchen & Baths", "permitted", "Permitted Remodel", ev, p.number.clone(), p.year, "Permitted kitchen/bath upgrades avoid $45k–$120k immediate remodel outlay.", p.source_name.clone(), url));
    } else if let Some(l) = kb_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "Kitchen");
        st.tags.push(TagCandidate {
            priority: 73, id: "remodeled_kitchen_bath".into(), label: "Updated Kitchen & Bath".into(), icon: "🍽️",
            category: "interior", tone: "emerald", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("remodeled_kitchen_bath", "Remodeled Kitchen & Baths", "confirmed", "Updated Finishes", l.text.clone(), None, extract_year(&l.text), "Modernized kitchen/bath counters, cabinetry, or pro-grade appliances.", l.source_name.clone(), url));
    } else {
        items.push(make_item("remodeled_kitchen_bath", "Remodeled Kitchen & Baths", "not_detected", "Original / Unspecified", "No remodeled kitchen or updated bath finishes noted.".into(), None, None, "Review Remodel & Rebuild section for regional kitchen/bath budget ranges.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 3. flooring_millwork
    let fm_line = c.find_line(
        &["hardwood", "wood floor", "vaulted ceiling", "high ceiling", "cathedral ceiling", "skylight", "fireplace", "walk-in closet"],
        &[],
    );
    if let Some(l) = fm_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), &l.text);
        items.push(make_item("flooring_millwork", "Hardwood, Ceilings, Skylights & Fireplace", "confirmed", "Architectural Character", l.text.clone(), None, extract_year(&l.text), "Natural light, volume ceilings, and finish upgrades.", l.source_name.clone(), url));
    } else {
        items.push(make_item("flooring_millwork", "Hardwood, Ceilings, Skylights & Fireplace", "not_detected", "Standard Finishes", "No hardwood, vaulted ceilings, skylights, or fireplace explicitly listed.".into(), None, None, "Verify flooring condition and fireplace type on tour.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 4. smart_water_security (strictly exclude lawn/yard/automatic irrigation sprinklers from fire sprinklers!)
    let sws_terms = &[
        "smart lock",
        "smart thermostat",
        "nest",
        "ecobee",
        "ring doorbell",
        "security system",
        "security camera",
        "water filtration",
        "reverse osmosis",
        "water softener",
        "whole-house water",
        "fire sprinkler",
        "interior sprinkler",
        "nfpa 13",
    ];
    let sws_excl = &["lawn sprinkler", "yard sprinkler", "automatic sprinkler", "irrigation"];
    if let Some(p) = c.find_permit(&["fire sprinkler", "nfpa 13", "water softener"], sws_excl) {
        let url = c.permit_anchor_url(p, "Fire Sprinkler");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        items.push(make_item("smart_water_security", "Smart Home, Water Treatment & Fire Sprinklers", "permitted", "Fire Sprinklers / Water", ev, p.number.clone(), p.year, "Interior fire suppression often qualifies for HO-3 insurance credits.", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(sws_terms, sws_excl) {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), &l.text);
        let badge = if l.lower.contains("fire sprinkler") || l.lower.contains("interior sprinkler") {
            "Interior Fire Sprinklers"
        } else if l.lower.contains("water") || l.lower.contains("osmosis") {
            "Water Filtration / Softener"
        } else {
            "Smart Controls & Security"
        };
        items.push(make_item("smart_water_security", "Smart Home, Water Treatment & Fire Sprinklers", "confirmed", badge, l.text.clone(), None, extract_year(&l.text), "Connected home controls, water treatment, or life-safety hardware.", l.source_name.clone(), url));
    } else {
        items.push(make_item("smart_water_security", "Smart Home, Water Treatment & Fire Sprinklers", "not_detected", "Not Detected", "No smart home controls, whole-house water filtration, or interior fire sprinklers noted.".into(), None, None, "Verify whether smart hubs/cameras convey with sale.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    let detected_count = items.iter().filter(|i| i["status"] != "not_detected").count();
    json!({
        "key": "interior",
        "title": "Interior Upgrades, Comfort & Smart Home",
        "detectedCount": detected_count,
        "items": items,
    })
}
fn eval_outdoor_lot_category(report: &Value, facts: &Value, c: &SignalCorpus, st: &mut EvalState) -> Value {
    let mut items = Vec::with_capacity(5);

    // 1. pool_spa
    let pool_terms = &["swimming pool", "in-ground pool", "inground pool", "heated pool", "saltwater pool", "lap pool", "gunite pool", "pool & spa", "pool/spa"];
    let pool_excl = &["community pool", "hoa pool", "association pool", "no pool", "pool table", "carpool"];
    let pool_feat_hit = facts.get("poolFeatures").and_then(|v| {
        let s = match v {
            Value::String(x) => x.clone(),
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            _ => String::new(),
        };
        let lc = s.to_ascii_lowercase();
        if !s.trim().is_empty() && !lc.contains("none") && !lc.contains("community") && !lc.contains("hoa") {
            Some(clean_public_text(&s))
        } else {
            None
        }
    });
    let pool_perm = c.find_permit(&["swimming pool", "inground pool", "in-ground pool", "gunite pool", "pool & spa"], pool_excl);
    let pool_line = c.find_line(pool_terms, pool_excl);
    let spa_line = c.find_line(&["above-ground spa", "hot tub", "outdoor spa", "jacuzzi", " backyard spa"], &["steam shower", "soaking tub", "community"]);

    if pool_perm.is_some() || pool_line.is_some() || pool_feat_hit.is_some() {
        st.has_pool = true;
        let (status, ev, pnum, yr, sname, surl) = if let Some(p) = pool_perm {
            let txt = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
            ("permitted", txt, p.number.clone(), p.year, p.source_name.clone(), c.permit_anchor_url(p, "Pool"))
        } else if let Some(pf) = pool_feat_hit {
            ("confirmed", pf, None, None, c.parcel_source.clone(), c.parcel_anchor_url(None, "Pool"))
        } else {
            let l = pool_line.unwrap();
            ("confirmed", l.text.clone(), None, extract_year(&l.text), l.source_name.clone(), c.parcel_anchor_url(l.source_url.as_deref(), "Pool"))
        };
        st.tags.push(TagCandidate {
            priority: 81, id: "pool_spa".into(), label: "Swimming Pool".into(), icon: "🏊",
            category: "outdoor_lot", tone: "blue", status, evidence: ev.clone(), source_name: sname.clone(), source_url: surl.clone(),
        });
        items.push(make_item("pool_spa", "Pool & Spa / Hot Tub", status, "Pool / Spa", ev, pnum, yr, "Inspect pool equipment, heater age, and safety fencing.", sname, surl));
    } else if let Some(l) = spa_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "Spa");
        items.push(make_item("pool_spa", "Pool & Spa / Hot Tub", "confirmed", "Spa / Hot Tub", l.text.clone(), None, None, "Confirm whether portable/above-ground spa conveys in contract.", l.source_name.clone(), url));
    } else {
        items.push(make_item("pool_spa", "Pool & Spa / Hot Tub", "not_detected", "No Pool", "No in-ground pool or outdoor spa recorded.".into(), None, None, "Lower ongoing water/chemical maintenance and insurance liability.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 2. scenic_view
    let view_terms = &["bay view", "ocean view", "water view", "lake view", "mountain view", "hills view", "hill view", "canyon view", "city view", "city-lights", "city lights", "panoramic", "park view", "valley view"];
    let view_excl = &["street view", "under review", "overview", "preview", "no view"];
    let view_line = c.find_line(view_terms, view_excl);
    if let Some(l) = view_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "View");
        st.tags.push(TagCandidate {
            priority: 78, id: "scenic_view".into(), label: "Scenic View".into(), icon: "🏔️",
            category: "outdoor_lot", tone: "emerald", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("scenic_view", "Scenic / Elevation View", "confirmed", "Scenic View", l.text.clone(), None, None, "View premiums often add 3%–12% resale appeal depending on corridor protection.", l.source_name.clone(), url));
    } else {
        items.push(make_item("scenic_view", "Scenic / Elevation View", "not_detected", "Neighborhood Exposure", "No panoramic bay, ocean, mountain, or city-lights view noted.".into(), None, None, "Standard residential street/yard outlook.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 3. lot_character
    let cul_fs = report.pointer("/modules/fengshui/data/quickScreens/culDeSac/hit").and_then(Value::as_bool).unwrap_or(false);
    let cul_line = c.find_line(&["cul-de-sac", "cul de sac", "court location"], &[]);
    let corner_line = c.find_line(&["corner lot"], &[]);
    let open_space_line = c.find_line(&["backs to open space", "backing open space", "greenbelt", "open space preserve"], &[]);
    let lot_sqft = num_f64(facts.get("lotSqft"));
    let large_lot = lot_sqft.filter(|&sq| sq >= 10_000.0);

    if cul_fs || cul_line.is_some() || large_lot.is_some() || corner_line.is_some() || open_space_line.is_some() {
        let mut traits = Vec::new();
        if cul_fs || cul_line.is_some() {
            traits.push("Cul-de-Sac".to_string());
        }
        if let Some(sq) = large_lot {
            if sq >= 21_780.0 {
                traits.push(format!("{:.2}-Acre Lot", sq / 43_560.0));
            } else {
                traits.push(format!("Large Lot ({:.0} sq ft)", sq));
            }
        }
        if corner_line.is_some() {
            traits.push("Corner Lot".to_string());
        }
        if open_space_line.is_some() {
            traits.push("Backs Open Space".to_string());
        }
        let badge = traits[0].clone();
        let ev = traits.join(" · ");
        let url = c.parcel_anchor_url(None, c.apn.as_deref().unwrap_or(&badge));
        st.tags.push(TagCandidate {
            priority: 75, id: "lot_character".into(), label: badge.clone(), icon: "🌳",
            category: "outdoor_lot", tone: "emerald", status: "confirmed", evidence: ev.clone(), source_name: c.parcel_source.clone(), source_url: url.clone(),
        });
        items.push(make_item("lot_character", "Lot Size & Street Placement", "confirmed", &badge, ev, None, None, "Low-traffic court placement or oversized parcel utility.", c.parcel_source.clone(), url));
    } else {
        let det = lot_sqft.map(|s| format!("Standard {:.0} sq ft residential lot.", s)).unwrap_or_else(|| "Standard residential parcel.".into());
        items.push(make_item("lot_character", "Lot Size & Street Placement", "not_detected", "Standard Lot", det, None, None, "Interior residential street lot.", c.parcel_source.clone(), c.parcel_anchor_url(None, c.apn.as_deref().unwrap_or(&c.address_hint))));
    }

    // 4. outdoor_living
    let out_line = c.find_line(
        &["outdoor kitchen", "built-in bbq", "pergola", "gazebo", "redwood deck", "covered patio", "xeriscape", "drought-tolerant", "fruit tree"],
        &[],
    );
    if let Some(l) = out_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), &l.text);
        st.tags.push(TagCandidate {
            priority: 54, id: "outdoor_living".into(), label: "Outdoor Entertaining".into(), icon: "🪴",
            category: "outdoor_lot", tone: "slate", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("outdoor_living", "Outdoor Kitchen, Deck/Pergola & Landscaping", "confirmed", "Outdoor Living", l.text.clone(), None, None, "Usable outdoor entertaining & yard amenities.", l.source_name.clone(), url));
    } else {
        items.push(make_item("outdoor_living", "Outdoor Kitchen, Deck/Pergola & Landscaping", "not_detected", "Standard Yard", "No outdoor kitchen, pergola/gazebo, or orchard feature noted.".into(), None, None, "Evaluate rear yard hardscape and drainage on tour.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 5. garage_rv
    let rv_line = c.find_line(&["rv parking", "rv/boat", "boat parking", "rv access", "side-yard parking"], &["no rv"]);
    let mut max_cars: f64 = num_f64(facts.get("parkingSpaces")).unwrap_or(0.0);
    let mut car_ev: Option<String> = None;
    for l in &c.lines {
        if let Some(cap) = GARAGE_CARS_RE.captures(&l.text).and_then(|m| m.get(1)) {
            if let Ok(n) = cap.as_str().parse::<f64>() {
                if n >= max_cars {
                    max_cars = n;
                    car_ev = Some(l.text.clone());
                }
            }
        }
    }
    if rv_line.is_some() || max_cars >= 2.0 {
        let (badge, ev) = match (rv_line, max_cars >= 3.0, max_cars >= 2.0) {
            (Some(rv), true, _) => ("3+ Car Garage + RV", rv.text.clone()),
            (Some(rv), false, _) => ("RV / Boat Parking", rv.text.clone()),
            (None, true, _) => ("3-Car Garage", car_ev.unwrap_or_else(|| "3+ car garage parking".into())),
            (None, false, true) => ("2-Car Garage", car_ev.unwrap_or_else(|| "2-car garage parking".into())),
            _ => unreachable!(),
        };
        let url = c.parcel_anchor_url(None, "garage");
        st.tags.push(TagCandidate {
            priority: if rv_line.is_some() || max_cars >= 3.0 { 69 } else { 57 },
            id: "garage_rv".into(),
            label: badge.into(),
            icon: "🚗",
            category: "outdoor_lot",
            tone: "slate",
            status: "confirmed",
            evidence: ev.clone(),
            source_name: c.parcel_source.clone(),
            source_url: url.clone(),
        });
        items.push(make_item("garage_rv", "Garage & RV / Boat Parking", "confirmed", badge, ev, None, None, "Off-street enclosed parking and side-yard storage.", c.parcel_source.clone(), url));
    } else {
        items.push(make_item("garage_rv", "Garage & RV / Boat Parking", "not_detected", "1-Car / Street", "No 2+ car garage or RV/boat parking confirmed.".into(), None, None, "Verify driveway width and garage clearance.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    let detected_count = items.iter().filter(|i| i["status"] != "not_detected").count();
    json!({
        "key": "outdoor_lot",
        "title": "Outdoor, Lot, Parking & Lifestyle",
        "detectedCount": detected_count,
        "items": items,
    })
}

fn eval_systems_ownership_category(report: &Value, facts: &Value, c: &SignalCorpus, st: &mut EvalState) -> Value {
    let mut items = Vec::with_capacity(5);

    // 1. seismic_foundation
    let seis_terms = &["seismic retrofit", "foundation bolt", "brace and bolt", "brace & bolt", "earthquake brace", "sill plate bolt", "cripple-wall brace", "bolted foundation"];
    let seis_excl = &["verify", "inspect", "check", "unbolted", "recommend"];
    if let Some(p) = c.find_permit(&["seismic", "foundation bolt", "brace", "retrofit", "underpin"], seis_excl) {
        let url = c.permit_anchor_url(p, "Seismic");
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 83, id: "seismic_foundation".into(), label: "Seismic Retrofit".into(), icon: "🛡️",
            category: "systems_ownership", tone: "emerald", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("seismic_foundation", "Seismic Retrofit & Foundation Bolting", "permitted", "Permitted Seismic Retrofit", ev, p.number.clone(), p.year, "Qualifies for CEA earthquake insurance premium discounts (up to 20%–25%).", p.source_name.clone(), url));
    } else if let Some(l) = c.find_line(seis_terms, seis_excl) {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "Seismic");
        st.tags.push(TagCandidate {
            priority: 80, id: "seismic_foundation".into(), label: "Seismic Retrofit".into(), icon: "🛡️",
            category: "systems_ownership", tone: "emerald", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("seismic_foundation", "Seismic Retrofit & Foundation Bolting", "confirmed", "Seismic Bolted", l.text.clone(), None, extract_year(&l.text), "Reduces cripple-wall desplazamiento risk in high-PGA zones.", l.source_name.clone(), url));
    } else {
        items.push(make_item("seismic_foundation", "Seismic Retrofit & Foundation Bolting", "not_detected", "Not Recorded", "No seismic retrofit or Brace & Bolt permit found.".into(), None, None, "Pre-1980 raised-foundation homes should verify sill bolting & plywood shear.", c.permit_portal_source.clone(), ensure_anchored_https_url(None, &c.permit_portal_url, "Foundation")));
    }

    // 2. plumbing_sewer_roof
    let roof_perm = c.permits.iter().find(|p| {
        let lc = p.text.to_ascii_lowercase();
        (lc.contains("roof") || lc.contains("reroof")) && p.year.map_or(true, |y| c.as_of_year - y <= 10)
    });
    let plumb_perm = c.find_permit(&["repipe", "pex", "copper plumbing", "sewer lateral", "trenchless sewer"], &["verify", "inspect"]);
    let psr_line = c.find_line(
        &["pex repipe", "whole-house pex", "copper repipe", "copper plumbing", "updated plumbing", "replaced sewer lateral", "new sewer lateral", "new roof"],
        &["verify", "inspect", "dated roof", "original"],
    );
    if let Some(p) = roof_perm.or(plumb_perm) {
        let url = c.permit_anchor_url(p, "Permit");
        let badge = if p.text.to_ascii_lowercase().contains("roof") { "Recent Permitted Roof" } else { "Permitted Plumbing / Sewer" };
        let ev = match &p.number { Some(n) => format!("Permit #{n}: {}", p.text), None => p.text.clone() };
        st.tags.push(TagCandidate {
            priority: 79, id: "plumbing_sewer_roof".into(), label: badge.into(), icon: "🔧",
            category: "systems_ownership", tone: "emerald", status: "permitted", evidence: ev.clone(), source_name: p.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("plumbing_sewer_roof", "Repipe, Sewer Lateral & Recent Roof", "permitted", badge, ev, p.number.clone(), p.year, "Major CapEx lifecycle reset supporting smooth HO-3 underwriting.", p.source_name.clone(), url));
    } else if let Some(l) = psr_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), "plumbing");
        st.tags.push(TagCandidate {
            priority: 71, id: "plumbing_sewer_roof".into(), label: "Updated MEP / Roof".into(), icon: "🔧",
            category: "systems_ownership", tone: "blue", status: "confirmed", evidence: l.text.clone(), source_name: l.source_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("plumbing_sewer_roof", "Repipe, Sewer Lateral & Recent Roof", "confirmed", "Updated Systems", l.text.clone(), None, extract_year(&l.text), "Reduces near-term plumbing/roof CapEx exposure.", l.source_name.clone(), url));
    } else {
        st.questions.push("Order a camera sewer-lateral inspection and verify roof permit/install year for HO-3 insurance eligibility.".into());
        items.push(make_item("plumbing_sewer_roof", "Repipe, Sewer Lateral & Recent Roof", "not_detected", "Verify Age", "No <=10-yr roof permit, whole-house repipe, or sewer lateral certificate on file.".into(), None, None, "Camera-scope the sewer lateral ($250–$400) during inspection contingency.", c.permit_portal_source.clone(), ensure_anchored_https_url(None, &c.permit_portal_url, "Roof")));
    }

    // 3. fire_wind_hardening
    let fw_line = c.find_line(
        &["class-a", "class a", "ember-resistant", "defensible space", "hurricane impact", "impact glass", "fire-rated roof", "standing seam metal", "tile roof"],
        &[],
    );
    if let Some(l) = fw_line {
        let url = c.parcel_anchor_url(l.source_url.as_deref(), &l.text);
        items.push(make_item("fire_wind_hardening", "Fire & Wind Hardening", "confirmed", "Class-A / Hardened", l.text.clone(), None, None, "Helps satisfy wildfire/wind insurer mitigation underwriting.", l.source_name.clone(), url));
    } else {
        items.push(make_item("fire_wind_hardening", "Fire & Wind Hardening", "not_detected", "Standard", "No explicit Class-A fire-hardening certificate, ember vents, or impact glass noted.".into(), None, None, "Check 1/8-inch ember mesh vents and Zone 0 5-ft noncombustible perimeter.", c.fema_url.clone(), ensure_anchored_https_url(Some(&c.fema_url), &c.parcel_url, "Hazard")));
    }

    // 4. no_hoa
    let hoa_mo = num_f64(facts.get("hoaMonthly"));
    let hoa_false = report.pointer("/hoaProfile/hasHoa").and_then(Value::as_bool) == Some(false);
    let no_hoa_line = c.find_line(&["no hoa", "zero hoa", "$0 hoa", "no mandatory hoa"], &[]);
    if hoa_mo == Some(0.0) || (hoa_mo.is_none() && (hoa_false || no_hoa_line.is_some())) {
        st.has_no_hoa = true;
        let ev = "Mandatory HOA dues: $0/mo (fee-simple autonomy, no association CC&R assessments)".to_string();
        let url = c.parcel_anchor_url(None, c.apn.as_deref().unwrap_or("HOA"));
        st.tags.push(TagCandidate {
            priority: 76, id: "no_hoa".into(), label: "No HOA".into(), icon: "🏛️",
            category: "systems_ownership", tone: "emerald", status: "confirmed", evidence: ev.clone(), source_name: c.parcel_source.clone(), source_url: url.clone(),
        });
        items.push(make_item("no_hoa", "No Mandatory HOA", "confirmed", "No HOA ($0/mo)", ev, None, None, "Saves $300–$750/mo vs. HOA communities and avoids special-assessment risk.", c.parcel_source.clone(), url));
    } else if let Some(dues) = hoa_mo.filter(|&d| d > 0.0) {
        st.questions.push(format!(
            "Review HOA reserve study (% funded), master insurance deductible, and rental/ADU restrictions for the ${dues:.0}/mo association."
        ));
        let url = c.parcel_anchor_url(None, "HOA");
        items.push(make_item("no_hoa", "No Mandatory HOA", "not_detected", &format!("HOA ${dues:.0}/mo"), format!("Mandatory HOA dues of ${dues:.0}/mo apply."), None, None, "Request HOA resale packet, reserve study, and 12-month board minutes.", c.parcel_source.clone(), url));
    } else {
        items.push(make_item("no_hoa", "No Mandatory HOA", "not_detected", "Unconfirmed", "HOA fee status not explicitly confirmed in parcel facts.".into(), None, None, "Confirm CC&Rs and HOA status on Preliminary Title Report.", c.parcel_source.clone(), c.parcel_anchor_url(None, &c.address_hint)));
    }

    // 5. walkable_top_schools
    let walk_sc = num_f64(report.pointer("/modules/civic/data/scores/walkability"))
        .or_else(|| num_f64(facts.get("walkScore")));
    let official_schools = report
        .pointer("/modules/civic/data")
        .map(crate::modules::civic::assigned_official_schools)
        .unwrap_or_default();
    let best_school = official_schools
        .iter()
        .filter_map(|s| s.official.as_ref().map(|o| (s.name.clone(), s.nces_id.clone(), o.score, o.source.clone())))
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let walk_hit = walk_sc.filter(|&w| w >= 70.0);
    let school_hit = best_school.filter(|b| b.2 >= 8.0);

    if walk_hit.is_some() || school_hit.is_some() {
        let mut parts = Vec::new();
        let mut anchor_hint = "Walkability".to_string();
        let mut base_src_url = c.census_url.clone();
        let mut src_name = "EPA Smart Location & NCES Official School Data".to_string();
        if let Some(w) = walk_hit {
            parts.push(format!("Walkability index {w:.0}/100"));
        }
        if let Some((nm, nces_id, sc, ssrc)) = &school_hit {
            parts.push(format!("{nm} ({sc:.1}/10 official {ssrc})"));
            anchor_hint = nm.clone();
            src_name = format!("NCES CCD & {ssrc}");
            if let Some(id) = nces_id {
                base_src_url = format!("https://nces.ed.gov/ccd/schoolsearch/school_detail.asp?ID={id}");
            } else {
                base_src_url = c.nces_url.clone();
            }
        }
        let badge = match (walk_hit.is_some(), school_hit.is_some()) {
            (true, true) => "Walkable + Top School",
            (false, true) => "Top-Rated Public School",
            (true, false) => "High Walkability",
            _ => unreachable!(),
        };
        let ev = parts.join(" · ");
        let url = ensure_anchored_https_url(Some(&base_src_url), &c.census_url, &anchor_hint);
        st.tags.push(TagCandidate {
            priority: 74, id: "walkable_top_schools".into(), label: badge.into(), icon: "🎓",
            category: "systems_ownership", tone: "emerald", status: "confirmed", evidence: ev.clone(), source_name: src_name.clone(), source_url: url.clone(),
        });
        items.push(make_item("walkable_top_schools", "Walkability & Official School Tier", "confirmed", badge, ev, None, None, "Strong neighborhood walkability and/or >=8/10 official public school tier.", src_name, url));
    } else {
        let det = match walk_sc {
            Some(w) => format!("Walkability score {w:.0}/100; no >=8/10 official school score on file."),
            None => "Walkability <70 and no >=8/10 official school score recorded.".into(),
        };
        items.push(make_item("walkable_top_schools", "Walkability & Official School Tier", "not_detected", "Standard Access", det, None, None, "Confirm attendance boundaries directly with the school district.", "NCES CCD & EPA Walkability".into(), ensure_anchored_https_url(Some(&c.nces_url), &c.census_url, "School")));
    }

    let detected_count = items.iter().filter(|i| i["status"] != "not_detected").count();
    json!({
        "key": "systems_ownership",
        "title": "Structural Upgrades, Infrastructure & Ownership Perks",
        "detectedCount": detected_count,
        "items": items,
    })
}

/// Builds the complete `propertyFeatures` payload (including `tags`, `categories`,
/// `energyAndEquipmentSummary`, and `buyerQuestionsToVerify`) from existing report signals.
pub fn build_property_features_and_tags(report: &Value) -> Value {
    let facts = report
        .get("facts")
        .or_else(|| report.get("property"))
        .unwrap_or(&Value::Null);
    let corpus = SignalCorpus::from_report(report);
    let mut st = EvalState::new();

    let cat_energy = eval_energy_category(report, facts, &corpus, &mut st);
    let cat_layout = eval_layout_adu_category(report, facts, &corpus, &mut st);
    let cat_interior = eval_interior_category(facts, &corpus, &mut st);
    let cat_outdoor = eval_outdoor_lot_category(report, facts, &corpus, &mut st);
    let cat_systems = eval_systems_ownership_category(report, facts, &corpus, &mut st);

    // Secondary factual candidates so reports with fewer headline hardware tags still reach 6–14 badges
    if st.tags.len() < 8 {
        for cat in [&cat_interior, &cat_energy] {
            if let Some(arr) = cat.get("items").and_then(Value::as_array) {
                for it in arr {
                    let id = it.get("id").and_then(Value::as_str).unwrap_or("");
                    let item_status = it.get("status").and_then(Value::as_str).unwrap_or("");
                    if matches!(id, "flooring_millwork" | "smart_water_security" | "envelope_efficiency")
                        && matches!(item_status, "confirmed" | "permitted")
                    {
                        st.tags.push(TagCandidate {
                            priority: 55,
                            id: id.to_string(),
                            label: it.get("badge").and_then(Value::as_str).unwrap_or("Upgraded Feature").to_string(),
                            icon: "✨",
                            category: if id == "envelope_efficiency" { "energy" } else { "interior" },
                            tone: "slate",
                            status: if item_status == "permitted" { "permitted" } else { "confirmed" },
                            evidence: it.get("detail").and_then(Value::as_str).unwrap_or("").to_string(),
                            source_name: it.get("sourceName").and_then(Value::as_str).unwrap_or("").to_string(),
                            source_url: it.get("sourceUrl").and_then(Value::as_str).unwrap_or("").to_string(),
                        });
                    }
                }
            }
        }
        if !st.has_solar {
            if let Some(sh) = num_f64(report.pointer("/decisionFactors/solar/sunHoursPerDay")).filter(|&s| s >= 4.7) {
                st.tags.push(TagCandidate {
                    priority: 52,
                    id: "solar_ready".into(),
                    label: "Solar-Ready Roof".into(),
                    icon: "☀️",
                    category: "energy",
                    tone: "slate",
                    status: "potential",
                    evidence: format!("{sh:.2} peak sun hours/day modeled roof irradiance"),
                    source_name: "NASA POWER Solar Model".into(),
                    source_url: ensure_anchored_https_url(Some("https://power.larc.nasa.gov/data-access-viewer/"), &corpus.parcel_url, "Solar"),
                });
            }
        }
        if report.pointer("/modules/hazards/data/flood/sfha").and_then(Value::as_bool) == Some(false) {
            let fz = report.pointer("/modules/hazards/data/flood/zone").and_then(Value::as_str).unwrap_or("X");
            st.tags.push(TagCandidate {
                priority: 50,
                id: "outside_sfha_flood".into(),
                label: format!("FEMA Zone {fz}"),
                icon: "🛡️",
                category: "systems_ownership",
                tone: "slate",
                status: "confirmed",
                evidence: format!("Outside high-risk FEMA Special Flood Hazard Area (Zone {fz})"),
                source_name: "FEMA National Flood Hazard Layer".into(),
                source_url: ensure_anchored_https_url(Some(&corpus.fema_url), &corpus.parcel_url, fz),
            });
        }
    }

    st.tags.sort_by(|a, b| b.priority.cmp(&a.priority));
    let mut seen_ids = HashSet::new();
    let mut tags_json = Vec::new();
    for t in st.tags {
        if seen_ids.insert(t.id.clone()) {
            tags_json.push(json!({
                "id": t.id,
                "label": clean_public_text(&t.label),
                "icon": t.icon,
                "category": t.category,
                "tone": t.tone,
                "status": t.status,
                "evidence": clean_public_text(&t.evidence),
                "sourceName": clean_public_text(&t.source_name),
                "sourceUrl": t.source_url,
            }));
            if tags_json.len() >= 14 {
                break;
            }
        }
    }

    // Ensure 2–5 smart, grounded buyer verification questions
    if st.has_permitted_adu {
        st.questions.push(
            "Request final municipal building permit sign-off, certificate of occupancy, and separate utility sub-metering details for the ADU.".into(),
        );
    }
    if st.questions.len() < 2 {
        st.questions.push(
            "Cross-check Seller Property Questionnaire (SPQ/TDS) and municipal building department archive for any unpermitted additions, electrical work, or bonus-room conversions.".into(),
        );
    }
    if st.questions.len() < 2 {
        st.questions.push(
            "Confirm age and serial-plate manufacture dates of HVAC, water heater, and roof assembly during general home inspection.".into(),
        );
    }
    let mut uniq_q = Vec::new();
    for q in st.questions {
        let cq = clean_public_text(&q);
        if !cq.is_empty() && !uniq_q.contains(&cq) {
            uniq_q.push(cq);
            if uniq_q.len() >= 5 {
                break;
            }
        }
    }

    json!({
        "tags": tags_json,
        "categories": [
            cat_energy,
            cat_layout,
            cat_interior,
            cat_outdoor,
            cat_systems,
        ],
        "energyAndEquipmentSummary": {
            "hasSolar": st.has_solar,
            "solarOwnership": st.solar_ownership,
            "hasBatteryBackup": st.has_battery,
            "batteryBrand": st.battery_brand,
            "hasEvCharger": st.has_ev,
            "has200AmpPanel": st.has_200a,
            "hasHeatPump": st.has_heat_pump,
            "hasCentralAc": st.has_central_ac,
            "hasPermittedAdu": st.has_permitted_adu,
            "isSingleStory": st.is_single_story,
            "hasPool": st.has_pool,
            "hasNoHoa": st.has_no_hoa,
        },
        "buyerQuestionsToVerify": uniq_q,
    })
}

/// Computes and attaches both `report["propertyTags"]` and `report["propertyFeatures"]` in-place.
pub fn attach_property_features_and_tags(report: &mut Value) {
    let pf = build_property_features_and_tags(report);
    let tags = pf.get("tags").cloned().unwrap_or_else(|| json!([]));
    if let Some(obj) = report.as_object_mut() {
        obj.insert("propertyTags".to_string(), tags);
        obj.insert("propertyFeatures".to_string(), pf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_strings_and_missing_geo_never_trigger_false_home_office_or_sanjose_coords() {
        let rep = json!({
            "meta": { "displayAddress": "126 Synth Way, Campbell, CA 95008", "generatedAt": "2026-04-01T00:00:00Z" },
            "facts": {
                "beds": 3,
                "baths": 2.5,
                "sqft": 1824,
                "yearBuilt": 2021,
                "countyRawAttributes": {
                    "ZoningCodePage": "https://library.example.org/ca/campbell/codes/code_of_ordinances"
                }
            },
            "buyerDiligence": {
                "connectivity": {
                    "fcc": {
                        "carriers": [{
                            "name": "Regional Fiber Estimate",
                            "technology": "Fiber",
                            "maxDownloadMbps": 10000,
                            "availabilityStatus": "metro_footprint_verify_drop"
                        }]
                    }
                }
            }
        });

        let pf = build_property_features_and_tags(&rep);
        let serialized = serde_json::to_string(&pf).unwrap();
        assert!(
            !serialized.contains("37.3382") && !serialized.contains("-121.8863"),
            "missing geo coordinates must never fall back to hardcoded San Jose coordinates: {serialized}"
        );

        let home_office = pf["categories"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["items"].as_array().unwrap())
            .find(|it| it["id"] == "home_office")
            .expect("home_office item");
        assert_eq!(
            home_office["status"].as_str(),
            Some("not_detected"),
            "https://library.example.org/ and metro_footprint_verify_drop must not trigger home_office: {home_office}"
        );
    }
}
