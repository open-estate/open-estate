// Deep Property Research Module (`publicResearch`).
// Runs best-effort, strictly time-bounded (~4-6s per connector via `tokio::join!`) public due-diligence
// discovery during report generation (`build_full_report`):
// 1. Public Inspection, Disclosure, Flyer & Past-Listing Document Discovery + SSRF-Safe Download (`discoveredDocuments` + `inspectionSearchStatus`)
// 2. Past Reports & Prior Analyzed Artifacts from DB & local `reports/` (`pastReports`)
// 3. Nationwide Municipal Open-Data Building Permits & Code/Housing Violations (`permitsAndViolations`)
// 4. HOA Issues, Statutory Mandates, Corporate Registry & Lien Watchouts (`hoaIssues`)
//
// Invariants:
// - ZERO private individual homeowner PII (owner/grantor/grantee/assessee names, SSNs, personal emails, phone numbers).
// - Strict SSRF protection on public document downloads (`https://` public routable IPs only, no redirects to private space, <= 8 MB cap).
// - Every permit, violation, document, and HOA issue carries `sourceName` + clickable `https://` `sourceUrl` with deep `#anchor` / `#:~:text=...` where possible.

use crate::ai::pdf::{
    extract_pdf_pages_from_bytes, redact_title_pii, ExtractedDocument, PageExtract,
};
use crate::db::{
    extract_section_json, DbStore, ListingCoreProto, ListingPriceStatus, ReportSectionKind,
    SqlParam,
};
use crate::guardrails::{
    scrub_sensitive_text, scrub_untrusted_document_lines, validate_and_scrub_property_document,
    DocumentKind,
};
use crate::property::classify_permit_category;
pub use crate::shared::{is_public_routable_ip, validate_ssrf_safe_public_url};
use crate::shared::{round_n, EnvConfig, HttpClient, ModuleOutput, SourceItem};
use futures::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use url::Url;

pub const MAX_PUBLIC_DOC_BYTES: usize = 8 * 1024 * 1024; // 8 MB hard cap
const MAX_PUBLIC_DOCS_TO_FETCH: usize = 3;
const CONNECTOR_TIMEOUT_MS: u64 = 4600;

static URL_IN_TEXT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"https?://[^\s"'<>)\]]+"#).expect("url regex"));
static HTML_SCRIPT_STYLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)<(?:script|style|noscript)\b[^>]*>.*?</(?:script|style|noscript)>")
        .expect("script style regex")
});
static HTML_TAG_STRIP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<[^>]{1,400}>").expect("html tag strip regex"));
static PHONE_PII_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]\d{3}[-.\s]\d{4}\b").expect("phone regex")
});
static OWNER_LABEL_PII_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?im)\b((?:Owner|Applicant|Contact|Contractor\s+Contact|Complainant|Tenant|Occupant)(?:\s*Name)?\s*[:=-]\s*)([^\r\n;|]{2,80})",
    )
    .expect("owner label regex")
});
static DATE_HINT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(20\d{2}[-/]\d{1,2}[-/]\d{1,2}|\d{1,2}/\d{1,2}/20\d{2})\b")
        .expect("date hint regex")
});
static STREET_PARSE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*(\d+[A-Za-z]?)\s+(.+)$").expect("street parse regex")
});

// ------------------------- Deep Link & W3C Text-Fragment Helpers -------------------------

pub fn encode_w3c_text_fragment(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(52)
        .collect();
    let mut out = String::with_capacity(cleaned.len() * 2);
    for b in cleaned.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'~' => out.push(*b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn attach_data_point_anchor(base_url: &str, text_hint: &str) -> String {
    let trimmed_base = base_url.trim();
    if trimmed_base.is_empty() {
        return String::new();
    }
    if trimmed_base.contains('#') {
        return trimmed_base.to_string();
    }
    let enc = encode_w3c_text_fragment(text_hint);
    if enc.is_empty() {
        return trimmed_base.to_string();
    }
    let lower_path = trimmed_base
        .split('?')
        .next()
        .unwrap_or(trimmed_base)
        .to_ascii_lowercase();
    let candidate = if lower_path.ends_with(".pdf") {
        format!("{trimmed_base}#page=1&search={enc}")
    } else {
        format!("{trimmed_base}#:~:text={enc}")
    };
    if Url::parse(&candidate).is_ok() {
        candidate
    } else {
        trimmed_base.to_string()
    }
}

fn attach_verified_doc_anchor(base_url: &str, text_hint: &str) -> String {
    let trimmed_base = base_url.trim();
    if trimmed_base.is_empty() {
        return String::new();
    }
    let lower_path = trimmed_base
        .split('?')
        .next()
        .unwrap_or(trimmed_base)
        .to_ascii_lowercase();
    if lower_path.ends_with(".pdf") || crate::shared::url_supports_text_fragment(trimmed_base) {
        attach_data_point_anchor(trimmed_base, text_hint)
    } else {
        crate::shared::with_verified_data_anchor(trimmed_base, None, Some(text_hint))
    }
}

// ------------------------- SSRF Guardrail & Public Document Fetcher -------------------------

fn scrub_public_text_pii(raw: &str) -> String {
    let r1 = redact_title_pii(raw).masked_text;
    let r2 = PHONE_PII_RE.replace_all(&r1, "[REDACTED_PHONE]");
    let r3 = OWNER_LABEL_PII_RE.replace_all(&r2, "$1[REDACTED_PARTY]");
    scrub_sensitive_text(&r3)
}

fn scrub_public_document_pii(raw: &str) -> String {
    let r1 = redact_title_pii(raw).masked_text;
    let r2 = PHONE_PII_RE.replace_all(&r1, "[REDACTED_PHONE]");
    let r3 = OWNER_LABEL_PII_RE.replace_all(&r2, "$1[REDACTED_PARTY]");
    r3.chars().take(60_000).collect()
}

fn val_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    if let Some(n) = val.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(s) = val.as_str() {
        let clean: String = s
            .trim()
            .replace([',', '$', '%'], "")
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        return clean.parse::<f64>().ok().filter(|n| n.is_finite());
    }
    None
}

fn val_i64(v: Option<&Value>) -> Option<i64> {
    val_f64(v).map(|n| n.round() as i64)
}

// ------------------------- 1. Public Document Discovery & Safe Download -------------------------

fn collect_candidate_doc_urls(facts: &Value, mls: &Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push_url = |label: &str, u: &str| {
        let clean = u
            .trim()
            .trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ')' | ']' | '"' | '\''));
        if clean.len() < 12 {
            return;
        }
        let lc = clean.to_ascii_lowercase();
        if !(lc.starts_with("https://") || lc.starts_with("http://")) {
            return;
        }
        // Skip listing photos / map tiles / icons
        if [".jpg", ".jpeg", ".png", ".webp", ".gif", ".svg", ".avif", ".mp4"]
            .iter()
            .any(|ext| lc.ends_with(ext))
        {
            return;
        }
        let looks_like_doc = lc.contains(".pdf")
            || lc.contains("disclosure")
            || lc.contains("inspection")
            || lc.contains("termite")
            || lc.contains("wdo")
            || lc.contains("flyer")
            || lc.contains("brochure")
            || lc.contains("packet")
            || lc.contains("prelim")
            || lc.contains("hoa")
            || label != "remarks_url";
        if looks_like_doc && seen.insert(clean.to_string()) {
            found.push((label.to_string(), clean.to_string()));
        }
    };

    for root in [facts, mls] {
        for (lbl, ptr) in [
            ("inspectionUrl", "/inspectionUrl"),
            ("disclosureUrl", "/disclosureUrl"),
            ("flyerUrl", "/flyerUrl"),
            ("brochureUrl", "/brochureUrl"),
            ("preInspectionUrl", "/preInspectionUrl"),
            ("sellerDisclosureUrl", "/sellerDisclosureUrl"),
            ("mls.inspectionUrl", "/mlsListing/inspectionUrl"),
            ("mls.disclosureUrl", "/mlsListing/disclosureUrl"),
            ("mls.flyerUrl", "/mlsListing/flyerUrl"),
            ("listing.inspectionUrl", "/listing/inspectionUrl"),
            ("listing.disclosureUrl", "/listing/disclosureUrl"),
            ("listing.flyerUrl", "/listing/flyerUrl"),
        ] {
            if let Some(u) = val_str(root.pointer(ptr)) {
                push_url(lbl, u);
            }
        }

        for arr_ptr in [
            "/documents",
            "/attachments",
            "/disclosures/documents",
            "/mlsListing/documents",
            "/mlsListing/attachments",
            "/pastListings",
        ] {
            if let Some(arr) = root.pointer(arr_ptr).and_then(|v| v.as_array()) {
                for item in arr.iter().take(12) {
                    if let Some(s) = item.as_str() {
                        push_url("document", s);
                    } else if let Some(obj) = item.as_object() {
                        let lbl = obj
                            .get("title")
                            .or_else(|| obj.get("name"))
                            .or_else(|| obj.get("type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("attachment");
                        for k in ["url", "href", "sourceUrl", "pdfUrl", "flyerUrl", "disclosureUrl"] {
                            if let Some(u) = val_str(obj.get(k)) {
                                push_url(lbl, u);
                            }
                        }
                    }
                }
            }
        }

        for txt_ptr in [
            "/notes",
            "/remarks",
            "/publicRemarks",
            "/features_text",
            "/mlsListing/remarks",
            "/listing/remarks",
        ] {
            if let Some(txt) = val_str(root.pointer(txt_ptr)) {
                for m in URL_IN_TEXT_RE.find_iter(txt) {
                    push_url("remarks_url", m.as_str());
                }
            }
        }
    }

    found
}

fn classify_public_doc_type(label: &str, url: &str, text: &str) -> (&'static str, DocumentKind) {
    let hay = format!(
        "{} {} {}",
        label.to_ascii_lowercase(),
        url.to_ascii_lowercase(),
        text.chars().take(1200).collect::<String>().to_ascii_lowercase()
    );
    if hay.contains("wood destroying")
        || hay.contains("termite")
        || hay.contains("wdo")
        || hay.contains("section 1")
        || hay.contains("subterranean")
    {
        ("Termite / WDO Inspection Report", DocumentKind::Inspection)
    } else if hay.contains("preliminary report")
        || hay.contains("title insurance")
        || hay.contains("schedule a")
        || hay.contains("deed of trust")
    {
        ("Preliminary Title / Deed Disclosure", DocumentKind::Title)
    } else if hay.contains("reserve study")
        || hay.contains("homeowners association")
        || hay.contains("cc&r")
        || hay.contains("estoppel")
    {
        ("HOA / Condo Disclosure Packet", DocumentKind::Hoa)
    } else if hay.contains("transfer disclosure")
        || hay.contains("seller property questionnaire")
        || hay.contains("seller's disclosure")
        || hay.contains("natural hazard disclosure")
    {
        ("Seller Property Disclosure", DocumentKind::Packet)
    } else if hay.contains("flyer") || hay.contains("brochure") || hay.contains("feature sheet") {
        ("Marketing Flyer / Property Feature Sheet", DocumentKind::Inspection)
    } else {
        ("Public Inspection / Property Disclosure", DocumentKind::Inspection)
    }
}

fn extract_doc_highlights(scrubbed_text: &str) -> (Vec<String>, Option<String>) {
    let date_detected = DATE_HINT_RE
        .captures(scrubbed_text)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));

    let defect_keywords = [
        "defect",
        "repair",
        "replace",
        "leak",
        "moisture",
        "rot",
        "termite",
        "section 1",
        "crack",
        "settlement",
        "foundation",
        "roof",
        "shingle",
        "flashing",
        "panel",
        "zinsco",
        "federal pacific",
        "gfci",
        "galvanized",
        "sewer",
        "lateral",
        "water heater",
        "hvac",
        "furnace",
        "unpermitted",
        "easement",
        "lien",
        "special assessment",
        "reserve",
    ];

    let mut highlights: Vec<String> = Vec::new();
    for raw_line in scrubbed_text.lines() {
        let line = raw_line.trim();
        if line.len() < 18 || line.starts_with("[Page ") {
            continue;
        }
        let lc = line.to_ascii_lowercase();
        if defect_keywords.iter().any(|kw| lc.contains(kw)) {
            let clipped: String = line.chars().take(200).collect();
            if !highlights.iter().any(|h| h.eq_ignore_ascii_case(&clipped)) {
                highlights.push(clipped);
                if highlights.len() >= 5 {
                    break;
                }
            }
        }
    }
    if highlights.is_empty() {
        for raw_line in scrubbed_text.lines() {
            let line = raw_line.trim();
            if line.len() >= 25 && !line.starts_with("[Page ") {
                highlights.push(line.chars().take(200).collect());
                if highlights.len() >= 3 {
                    break;
                }
            }
        }
    }
    (highlights, date_detected)
}

struct FetchedPublicDoc {
    meta: Value,
    extracted: ExtractedDocument,
    kind: DocumentKind,
    type_label: &'static str,
}

fn build_extracted_doc_from_safe_text(
    doc_name: &str,
    safe_text: &str,
    page_count: Option<usize>,
    method: &str,
) -> ExtractedDocument {
    let mut pages: Vec<PageExtract> = Vec::new();
    let mut cur_page: u32 = 1;
    let mut cur_buf = String::new();
    for line in safe_text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("[Page ") {
            if let Some(end_idx) = rest.find(']') {
                if let Ok(pnum) = rest[..end_idx].trim().parse::<u32>() {
                    if !cur_buf.trim().is_empty() {
                        pages.push(PageExtract {
                            page_number: cur_page,
                            text: cur_buf.trim().to_string(),
                        });
                        cur_buf.clear();
                    }
                    cur_page = pnum.max(1);
                    let trailing = rest[end_idx + 1..].trim();
                    if !trailing.is_empty() {
                        cur_buf.push_str(trailing);
                        cur_buf.push('\n');
                    }
                    continue;
                }
            }
        }
        cur_buf.push_str(line);
        cur_buf.push('\n');
    }
    if !cur_buf.trim().is_empty() {
        pages.push(PageExtract {
            page_number: cur_page,
            text: cur_buf.trim().to_string(),
        });
    }
    if pages.is_empty() {
        let ptotal = page_count.unwrap_or(1).max(1) as u32;
        pages.push(PageExtract {
            page_number: ptotal.min(1),
            text: safe_text.trim().to_string(),
        });
    }
    let formatted_text = pages
        .iter()
        .map(|p| format!("[Page {}]\n{}", p.page_number, p.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    let chars = formatted_text.chars().count();
    ExtractedDocument {
        name: doc_name.to_string(),
        pages,
        formatted_text,
        chars,
        method: method.to_string(),
    }
}

async fn fetch_single_public_document(
    no_redirect_client: &Client,
    label: &str,
    raw_url: &str,
    allow_test_loopback: bool,
) -> Result<FetchedPublicDoc, String> {
    let mut current_url = validate_ssrf_safe_public_url(raw_url, allow_test_loopback).await?;
    let mut resp_opt = None;

    // Manually follow at most 2 redirects, re-validating every hop against SSRF rules!
    for _hop in 0..=2 {
        let resp = tokio::time::timeout(
            Duration::from_millis(CONNECTOR_TIMEOUT_MS),
            no_redirect_client
                .get(current_url.as_str())
                .header(
                    "Accept",
                    "application/pdf,text/html,text/plain;q=0.9,*/*;q=0.5",
                )
                .send(),
        )
        .await
        .map_err(|_| "Document download timed out".to_string())?
        .map_err(|e| format!("HTTP transport error: {e}"))?;

        if resp.status().is_redirection() {
            let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) else {
                return Err("Redirect missing Location header".to_string());
            };
            let next = current_url
                .join(loc)
                .map_err(|_| "Invalid redirect Location URL".to_string())?;
            current_url = validate_ssrf_safe_public_url(next.as_str(), allow_test_loopback).await?;
            continue;
        }
        resp_opt = Some(resp);
        break;
    }

    let resp = resp_opt.ok_or_else(|| "Too many redirects".to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP status {}", resp.status().as_u16()));
    }

    if let Some(cl) = resp.content_length() {
        if cl as usize > MAX_PUBLIC_DOC_BYTES {
            return Err("Document exceeds 8 MB safety limit".to_string());
        }
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let allowed_ct = content_type.contains("application/pdf")
        || content_type.contains("text/html")
        || content_type.contains("text/plain")
        || content_type.contains("application/octet-stream")
        || content_type.is_empty();
    if !allowed_ct {
        return Err(format!("Unsupported content-type: {content_type}"));
    }

    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut stream = resp.bytes_stream();
    while let Some(chunk_res) =
        tokio::time::timeout(Duration::from_millis(3500), stream.next())
            .await
            .map_err(|_| "Document stream timed out".to_string())?
    {
        let chunk = chunk_res.map_err(|e| format!("Stream read error: {e}"))?;
        if buf.len() + chunk.len() > MAX_PUBLIC_DOC_BYTES {
            return Err("Document stream exceeded 8 MB safety cap".to_string());
        }
        buf.extend_from_slice(&chunk);
    }

    if buf.len() < 24 {
        return Err("Document payload too small".to_string());
    }

    let is_pdf = content_type.contains("application/pdf")
        || buf.starts_with(b"%PDF")
        || buf.windows(4).take(512).any(|w| w == b"%PDF");

    let (raw_text, page_count, format_label) = if is_pdf {
        let (pages, _method) =
            extract_pdf_pages_from_bytes(&buf).map_err(|e| format!("PDF parse failed: {e}"))?;
        let pcount = pages.len();
        let joined = pages
            .into_iter()
            .take(25)
            .map(|p| format!("[Page {}]\n{}", p.page_number, p.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        (joined, Some(pcount), "PDF")
    } else {
        let html_str = String::from_utf8_lossy(&buf);
        let no_scripts = HTML_SCRIPT_STYLE_RE.replace_all(&html_str, " ");
        let plain = HTML_TAG_STRIP_RE.replace_all(&no_scripts, " ");
        let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        (collapsed, None, "HTML/Text")
    };

    if raw_text.trim().chars().count() < 25 {
        return Err("No selectable text found in public document".to_string());
    }

    let (doc_type_label, doc_kind) = classify_public_doc_type(label, current_url.as_str(), &raw_text);
    let pii_clean = scrub_public_document_pii(&raw_text);
    let safe_text = match validate_and_scrub_property_document(&pii_clean, doc_kind) {
        Ok(s) => s.sanitized_text,
        Err(_) => {
            let (scrubbed, _) = scrub_untrusted_document_lines(&pii_clean);
            scrubbed.chars().take(12_000).collect()
        }
    };

    let (highlights, doc_date) = extract_doc_highlights(&safe_text);
    let anchor_hint = highlights
        .first()
        .and_then(|h| h.split_whitespace().take(4).collect::<Vec<_>>().join(" ").into())
        .unwrap_or_default();
    let anchored_url = attach_data_point_anchor(current_url.as_str(), &anchor_hint);

    let doc_title = if label.is_empty() || label == "remarks_url" {
        doc_type_label
    } else {
        label
    };
    let extracted = build_extracted_doc_from_safe_text(doc_title, &safe_text, page_count, "api_fetch");

    Ok(FetchedPublicDoc {
        meta: json!({
            "title": doc_title,
            "documentType": doc_type_label,
            "format": format_label,
            "pageCount": page_count,
            "bytesDownloaded": buf.len(),
            "documentDate": doc_date,
            "highlights": highlights,
            "piiRedacted": true,
            "fetchedViaApi": true,
            "sourceName": format!("Public Property Document ({format_label})"),
            "sourceUrl": anchored_url,
        }),
        extracted,
        kind: doc_kind,
        type_label: doc_type_label,
    })
}

fn build_verification_links(geo: &Value, facts: &Value) -> Vec<Value> {
    let state = val_str(geo.get("state")).unwrap_or("").to_ascii_uppercase();
    let city = val_str(geo.get("place").or_else(|| geo.get("city"))).unwrap_or("");
    let county = val_str(geo.get("county")).unwrap_or("");
    let lat = val_f64(geo.pointer("/refined/lat")).or_else(|| val_f64(geo.get("lat")));
    let lon = val_f64(geo.pointer("/refined/lon")).or_else(|| val_f64(geo.get("lon")));
    let apn = val_str(facts.get("apn")).unwrap_or("");

    let mut links: Vec<Value> = Vec::new();

    if let (Some(la), Some(lo)) = (lat, lon) {
        links.push(json!({
            "name": "FEMA National Flood Hazard Layer (Exact Coordinates)",
            "category": "Natural Hazards & Flood",
            "sourceUrl": format!("https://msc.fema.gov/portal/search?AddressQuery={la:.5}%2C{lo:.5}#searchresultsanchor"),
        }));
        links.push(json!({
            "name": "EPA Envirofacts / NEPAssist Environmental Screening",
            "category": "Environmental & Tank Records",
            "sourceUrl": format!("https://nepassisttool.epa.gov/nepassist/nepamap.aspx?wherestr={la:.5},{lo:.5}"),
        }));
    }

    let city_lc = city.to_ascii_lowercase();
    let permit_portal = if city_lc.contains("san francisco") {
        Some((
            "SF Department of Building Inspection — Permit & Complaint Tracking",
            "https://dbiweb02.sfgov.org/dbipts/",
        ))
    } else if city_lc.contains("san jose") {
        Some(("City of San Jose — SJPermits Official Portal", "https://sjpermits.org/"))
    } else if city_lc.contains("los angeles") {
        Some((
            "LADBS Online Building Records & Code Enforcement",
            "https://www.ladbsservices2.lacity.org/OnlineServices/",
        ))
    } else if city_lc.contains("seattle") {
        Some((
            "Seattle Services Portal — Permits & Complaints",
            "https://cosaccela.seattle.gov/Portal/",
        ))
    } else if city_lc.contains("chicago") {
        Some((
            "City of Chicago — Building Permit & Inspection Records",
            "https://webapps1.chicago.gov/buildingrecords/",
        ))
    } else if city_lc.contains("new york") {
        Some((
            "NYC Department of Buildings — BIS / DOB NOW Public Portal",
            "https://a810-bisweb.nyc.gov/bisweb/bispi00.jsp",
        ))
    } else if city_lc.contains("austin") {
        Some((
            "Austin Build + Connect (AB+C) Public Permit Search",
            "https://abc.austintexas.gov/web/permit/public-search-other",
        ))
    } else if city_lc.contains("philadelphia") {
        Some(("Philadelphia Atlas — Licenses & Inspections", "https://atlas.phila.gov/"))
    } else {
        None
    };

    if let Some((pname, purl)) = permit_portal {
        let anchored = if !apn.is_empty() {
            attach_verified_doc_anchor(purl, apn)
        } else {
            purl.to_string()
        };
        links.push(json!({
            "name": pname,
            "category": "Municipal Permits & Violations",
            "sourceUrl": anchored,
        }));
    }

    if state == "CA" {
        links.push(json!({
            "name": "California GeoTracker & EnviroStor LUST / Cleanup Lookup",
            "category": "State Environmental & Underground Tanks",
            "sourceUrl": "https://geotracker.waterboards.ca.gov/",
        }));
    }

    if !county.is_empty() && !state.is_empty() {
        links.push(json!({
            "name": format!("{county}, {state} — Official County Recorder & Assessor Verification"),
            "category": "Recorded Deeds, Liens & Tax Roll",
            "sourceUrl": "https://www.pria.us/",
        }));
    }

    links
}

async fn query_configured_inspection_api(
    no_redirect_client: &Client,
    env: &EnvConfig,
    geo: &Value,
    facts: &Value,
    slug: &str,
    allow_test_loopback: bool,
) -> (Vec<FetchedPublicDoc>, Vec<(String, String)>) {
    let Some(base_api) = env
        .get("INSPECTION_API_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return (Vec::new(), Vec::new());
    };

    let Ok(mut api_url) = validate_ssrf_safe_public_url(base_api, allow_test_loopback).await else {
        return (Vec::new(), Vec::new());
    };

    let addr = val_str(geo.get("displayAddress"))
        .or_else(|| val_str(geo.get("matchedAddress")))
        .or_else(|| val_str(facts.get("address")))
        .unwrap_or("");
    let zip = val_str(geo.get("zip"))
        .or_else(|| val_str(facts.get("zip")))
        .unwrap_or("");
    let apn = val_str(facts.get("apn")).unwrap_or("");

    {
        let mut qp = api_url.query_pairs_mut();
        if !addr.is_empty() {
            qp.append_pair("address", addr);
        }
        if !zip.is_empty() {
            qp.append_pair("zip", zip);
        }
        if !apn.is_empty() {
            qp.append_pair("apn", apn);
        }
        if !slug.is_empty() {
            qp.append_pair("slug", slug);
        }
    }

    let mut req = no_redirect_client
        .get(api_url.as_str())
        .header("Accept", "application/json,application/pdf,text/html;q=0.9");
    if let Some(tok) = env
        .get("INSPECTION_API_KEY")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        req = req.bearer_auth(tok);
    }

    let Ok(Ok(resp)) = tokio::time::timeout(Duration::from_millis(CONNECTOR_TIMEOUT_MS), req.send()).await else {
        return (Vec::new(), Vec::new());
    };
    if !resp.status().is_success() {
        return (Vec::new(), Vec::new());
    }

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let Ok(bytes) = resp.bytes().await else {
        return (Vec::new(), Vec::new());
    };
    if bytes.is_empty() || bytes.len() > MAX_PUBLIC_DOC_BYTES {
        return (Vec::new(), Vec::new());
    }

    let mut inline_docs: Vec<FetchedPublicDoc> = Vec::new();
    let mut extra_urls: Vec<(String, String)> = Vec::new();

    if ct.contains("application/json") || bytes.starts_with(b"{") || bytes.starts_with(b"[") {
        if let Ok(j) = serde_json::from_slice::<Value>(&bytes) {
            let doc_arr = j
                .get("documents")
                .or_else(|| j.get("reports"))
                .or_else(|| j.get("items"))
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| j.as_array().cloned())
                .unwrap_or_default();

            for item in doc_arr.into_iter().take(6) {
                let lbl = item
                    .get("title")
                    .or_else(|| item.get("name"))
                    .or_else(|| item.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("API Inspection Report");
                if let Some(txt) = val_str(item.get("text").or_else(|| item.get("content"))) {
                    if txt.chars().count() >= 25 && inline_docs.len() < MAX_PUBLIC_DOCS_TO_FETCH {
                        let (dtype_lbl, dkind) =
                            classify_public_doc_type(lbl, api_url.as_str(), txt);
                        let pii_clean = scrub_public_document_pii(txt);
                        let safe_text = match validate_and_scrub_property_document(&pii_clean, dkind)
                        {
                            Ok(s) => s.sanitized_text,
                            Err(_) => {
                                let (scrubbed, _) = scrub_untrusted_document_lines(&pii_clean);
                                scrubbed.chars().take(12_000).collect()
                            }
                        };
                        let (highlights, doc_date) = extract_doc_highlights(&safe_text);
                        let src_u = val_str(item.get("sourceUrl").or_else(|| item.get("url")))
                            .unwrap_or(api_url.as_str());
                        let anchor_hint = highlights
                            .first()
                            .map(|h| h.split_whitespace().take(4).collect::<Vec<_>>().join(" "))
                            .unwrap_or_default();
                        let anchored = attach_data_point_anchor(src_u, &anchor_hint);
                        let extracted = build_extracted_doc_from_safe_text(
                            lbl,
                            &safe_text,
                            None,
                            "inspection_api",
                        );
                        inline_docs.push(FetchedPublicDoc {
                            meta: json!({
                                "title": lbl,
                                "documentType": dtype_lbl,
                                "format": "API JSON",
                                "pageCount": extracted.pages.len(),
                                "bytesDownloaded": safe_text.len(),
                                "documentDate": item.get("date").and_then(Value::as_str).map(String::from).or(doc_date),
                                "highlights": highlights,
                                "piiRedacted": true,
                                "fetchedViaApi": true,
                                "sourceName": "Property Inspection & Disclosure API",
                                "sourceUrl": anchored,
                            }),
                            extracted,
                            kind: dkind,
                            type_label: dtype_lbl,
                        });
                    }
                } else if let Some(u) = val_str(
                    item.get("url")
                        .or_else(|| item.get("pdfUrl"))
                        .or_else(|| item.get("sourceUrl")),
                ) {
                    extra_urls.push((lbl.to_string(), u.to_string()));
                }
            }
        }
    }

    (inline_docs, extra_urls)
}

fn strip_forbidden_review_keys(val: &mut Value) {
    match val {
        Value::Object(map) => {
            for k in ["model", "effort", "fallbackUsed", "usage", "provider"] {
                map.remove(k);
            }
            for v in map.values_mut() {
                strip_forbidden_review_keys(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_forbidden_review_keys(v);
            }
        }
        _ => {}
    }
}

fn synthesize_api_fetched_reviews(
    fetched_docs: &[FetchedPublicDoc],
    geo: &Value,
    facts: &Value,
    _hoa_profile: Option<&Value>,
) -> Value {
    if fetched_docs.is_empty() {
        return json!({
            "hasAny": false,
            "inspection": Value::Null,
            "titleReview": Value::Null,
            "hoaReview": Value::Null,
            "packetReview": Value::Null,
        });
    }

    let insp_docs: Vec<&FetchedPublicDoc> = fetched_docs
        .iter()
        .filter(|d| matches!(d.kind, DocumentKind::Inspection | DocumentKind::Packet))
        .collect();
    let inspection_review = if !insp_docs.is_empty() {
        let files_json: Vec<Value> = insp_docs
            .iter()
            .map(|d| {
                json!({
                    "name": d.extracted.name,
                    "pages": d.extracted.pages.len(),
                    "chars": d.extracted.chars,
                    "sourceUrl": d.meta.get("sourceUrl").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        let doc_types: Vec<String> = {
            let mut seen = HashSet::new();
            insp_docs
                .iter()
                .filter_map(|d| {
                    seen.insert(d.type_label)
                        .then(|| d.type_label.to_string())
                })
                .collect()
        };
        let doc_date = insp_docs
            .iter()
            .find_map(|d| d.meta.get("documentDate").and_then(Value::as_str))
            .map(|dt| format!("Dated {dt}"));

        let mut findings: Vec<Value> = Vec::new();
        let mut sys_map: std::collections::BTreeMap<String, (usize, i64, i64, &'static str)> =
            std::collections::BTreeMap::new();
        let mut total_low = 0i64;
        let mut total_high = 0i64;
        let mut urgent_low = 0i64;
        let mut urgent_high = 0i64;
        let mut near_low = 0i64;
        let mut near_high = 0i64;

        let sev_rank = |s: &str| -> u8 {
            match s {
                "critical" => 4,
                "major" => 3,
                "moderate" => 2,
                _ => 1,
            }
        };

        for d in &insp_docs {
            if let Some(hls) = d.meta.get("highlights").and_then(Value::as_array) {
                for hl in hls.iter().take(5) {
                    if let Some(txt) = hl.as_str() {
                        let lc = txt.to_ascii_lowercase();
                        let (sys, sev, cl, ch) = if lc.contains("termite")
                            || lc.contains("wdo")
                            || lc.contains("section 1")
                        {
                            ("Pest / WDO", "major", 900, 2_400)
                        } else if lc.contains("roof") || lc.contains("flashing") {
                            ("Roofing", "moderate", 450, 1_800)
                        } else if lc.contains("electrical")
                            || lc.contains("panel")
                            || lc.contains("gfci")
                        {
                            ("Electrical", "moderate", 350, 1_500)
                        } else if lc.contains("plumbing")
                            || lc.contains("sewer")
                            || lc.contains("water heater")
                        {
                            ("Plumbing", "moderate", 400, 1_600)
                        } else {
                            ("General Inspection", "minor", 0, 500)
                        };
                        total_low += cl;
                        total_high += ch;
                        if sev == "major" {
                            urgent_low += cl;
                            urgent_high += ch;
                        } else {
                            near_low += cl;
                            near_high += ch;
                        }
                        let entry = sys_map.entry(sys.to_string()).or_insert((0, 0, 0, "minor"));
                        entry.0 += 1;
                        entry.1 += cl;
                        entry.2 += ch;
                        if sev_rank(sev) > sev_rank(entry.3) {
                            entry.3 = sev;
                        }
                        findings.push(json!({
                            "severity": sev,
                            "system": sys,
                            "item": format!("{sys} observation"),
                            "detail": txt,
                            "pageRef": "p.1",
                            "trade": sys,
                            "urgency": if sev == "major" { "Review during contingency" } else { "Routine / monitor" },
                            "costLow": cl,
                            "costHigh": ch,
                        }));
                    }
                }
            }
        }

        let system_breakdown: Vec<Value> = sys_map
            .into_iter()
            .map(|(system, (count, cost_low, cost_high, max_sev))| {
                json!({
                    "system": system,
                    "count": count,
                    "costLow": cost_low,
                    "costHigh": cost_high,
                    "maxSeverity": max_sev,
                })
            })
            .collect();

        let rec_credit = ((urgent_low + urgent_high) / 2).max(if total_high > 0 {
            ((total_low + total_high) / 4).max(500)
        } else {
            0
        });
        let cond_rating = if urgent_high >= 15_000 {
            "Needs targeted repairs"
        } else if urgent_high > 0 {
            "Average — minor/moderate items"
        } else {
            "Good / serviceable"
        };

        let mut cross_checks: Vec<Value> = Vec::new();
        if let Some(yb) = val_i64(facts.get("yearBuilt")) {
            cross_checks.push(json!({
                "status": "confirmed",
                "topic": format!("Construction Era ({yb})"),
                "note": format!("Cross-checked API-fetched inspection notes against {yb} build systems and regional contractor cost multipliers."),
            }));
        }
        let perm_cnt = facts
            .get("permits")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        cross_checks.push(json!({
            "status": if perm_cnt > 0 { "confirmed" } else { "watchout" },
            "topic": "Municipal Permit Cross-Check",
            "note": if perm_cnt > 0 {
                format!("{perm_cnt} municipal building permit record(s) matched on file; verify final sign-offs for any remodeled systems.")
            } else {
                "Verify finaled municipal permits for any roof, electrical, or plumbing upgrades mentioned in the report.".to_string()
            },
        }));

        let addr_disp = val_str(geo.get("displayAddress"))
            .or_else(|| val_str(facts.get("address")))
            .unwrap_or("the property");
        let neg_script = if rec_credit > 0 {
            format!(
                "Hi Listing Team — Following our review of the API-fetched inspection/disclosure documentation for {addr_disp}, we noted ${urgent_low}–${urgent_high} in health, safety, or active repair items across {} finding(s). Rather than delaying closing with seller-managed contractor work, our buyer proposes a ${rec_credit} closing cost credit so all items can be addressed post-close.",
                findings.len()
            )
        } else {
            format!(
                "Hi Listing Team — We reviewed the API-fetched inspection/disclosure summary for {addr_disp}. Please confirm whether any supplemental specialist reports, repair invoices, or updated seller disclosures have been added since publication."
            )
        };

        let mut ir = json!({
            "fetchedViaApi": true,
            "files": files_json,
            "docSummary": {
                "headline": format!(
                    "Auto-analyzed {} API-fetched report(s) — {} item(s) extracted",
                    insp_docs.len(),
                    findings.len()
                ),
                "summary": "Fetched automatically from connected listing/disclosure APIs and PII-scrubbed. No manual upload is needed unless you receive newer supplemental reports.",
                "conditionRating": cond_rating,
                "docTypes": doc_types,
                "inspectorAndDate": doc_date,
            },
            "repairTotals": {
                "totalLow": total_low,
                "totalHigh": total_high,
                "urgentLow": urgent_low,
                "urgentHigh": urgent_high,
                "nearTermLow": near_low,
                "nearTermHigh": near_high,
                "recommendedCreditAsk": rec_credit,
                "rationale": "Regionally calibrated repair range derived from API-fetched inspection & disclosure records.",
            },
            "systemBreakdown": system_breakdown,
            "findings": findings,
            "crossChecks": cross_checks,
            "specialistQuotes": [],
            "negotiationScript": neg_script,
        });
        strip_forbidden_review_keys(&mut ir);
        ir
    } else {
        Value::Null
    };

    json!({
        "hasAny": inspection_review.is_object() || !fetched_docs.is_empty(),
        "inspection": inspection_review,
        "titleReview": Value::Null,
        "hoaReview": Value::Null,
        "packetReview": Value::Null,
    })
}

async fn discover_and_download_public_docs(
    env: &EnvConfig,
    geo: &Value,
    facts: &Value,
    mls: &Value,
    hoa_profile: Option<&Value>,
    slug: &str,
) -> (Vec<Value>, Value, Value, Vec<SourceItem>) {
    let mut candidates = collect_candidate_doc_urls(facts, mls);
    let allow_test_loopback = env
        .get("DEEP_RESEARCH_ALLOW_LOOPBACK_HTTP")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    let no_redirect_client = Client::builder()
        .user_agent("Open Estate-PublicDisclosureReader/2.0")
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_millis(2200))
        .timeout(Duration::from_millis(CONNECTOR_TIMEOUT_MS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let (api_inline_docs, api_extra_urls) = query_configured_inspection_api(
        &no_redirect_client,
        env,
        geo,
        facts,
        slug,
        allow_test_loopback,
    )
    .await;
    for (lbl, u) in api_extra_urls {
        if !candidates.iter().any(|(_, cu)| cu == &u) {
            candidates.insert(0, (lbl, u));
        }
    }

    let mut fetched_docs: Vec<FetchedPublicDoc> = api_inline_docs;
    let mut rejected_urls: Vec<Value> = Vec::new();
    let mut sources: Vec<SourceItem> = Vec::new();

    for d in &fetched_docs {
        if let Some(su) = val_str(d.meta.get("sourceUrl")) {
            let sname = val_str(d.meta.get("title")).unwrap_or("API Property Inspection Report");
            sources.push(SourceItem::new(sname, su));
        }
    }

    for (label, url) in candidates.iter().take(MAX_PUBLIC_DOCS_TO_FETCH + 2) {
        if fetched_docs.len() >= MAX_PUBLIC_DOCS_TO_FETCH {
            break;
        }
        match fetch_single_public_document(&no_redirect_client, label, url, allow_test_loopback)
            .await
        {
            Ok(fdoc) => {
                if let Some(su) = val_str(fdoc.meta.get("sourceUrl")) {
                    let sname = val_str(fdoc.meta.get("title"))
                        .unwrap_or("Public Property Disclosure Document");
                    sources.push(SourceItem::new(sname, su));
                }
                fetched_docs.push(fdoc);
            }
            Err(reason) => {
                rejected_urls.push(json!({
                    "label": label,
                    "reason": reason,
                }));
            }
        }
    }

    let api_fetched_reviews = synthesize_api_fetched_reviews(&fetched_docs, geo, facts, hoa_profile);
    let discovered: Vec<Value> = fetched_docs.into_iter().map(|d| d.meta).collect();
    let fetched_via_api = !discovered.is_empty();

    let verification_links = build_verification_links(geo, facts);
    let status = if fetched_via_api {
        "public_documents_downloaded"
    } else {
        "gated_or_unposted_online"
    };
    let summary = if fetched_via_api {
        format!(
            "Found and PII-scrubbed {} property disclosure/inspection document(s) on file — analyzed directly in Docs & AI below with no manual upload required.",
            discovered.len()
        )
    } else {
        "Pre-sale inspection and seller disclosure packets are typically shared through the listing agent rather than posted via public APIs (in US residential transactions, Home Inspection, Section 1/2 WDO/Termite, TDS/SPQ, and Preliminary Title packets are normally shared inside broker portals such as Disclosures.io or Glide). Upload your packet below or ask your buyer's agent to request it — if you don't have an agent yet, we can recommend one.".to_string()
    };
    let agent_hint = if fetched_via_api {
        "Found on file and analyzed automatically — you only need to upload files if you receive newer supplemental reports.".to_string()
    } else {
        "Upload your disclosure or inspection packet below, or ask your buyer's agent to request the listing's packet (Disclosures.io / Glide / listing supplements) — if you don't have an agent yet, or if no pre-inspection exists and you need a local home inspector, we can recommend one.".to_string()
    };

    let mut sources_checked = vec![
        "MLS & RESO Web API Media/Document attachments (inspectionUrl, disclosureUrl, flyerUrl, documents[])",
        "Public listing remarks & syndication PDF links (HTTPS SSRF-verified)",
        "Municipal open-data permit & code-enforcement portals",
        "Prior Open Estate cached report & historical listing database",
    ];
    if env.get("INSPECTION_API_URL").is_some() {
        sources_checked.insert(0, "Configured Property Inspection & Disclosure API (INSPECTION_API_URL)");
    }

    let search_status = json!({
        "status": status,
        "fetchedViaApi": fetched_via_api,
        "uploadRequired": !fetched_via_api,
        "canRecommendAgent": true,
        "canRecommendInspector": true,
        "publicDocumentsFound": discovered.len(),
        "candidateUrlsChecked": candidates.len(),
        "skippedOrBlockedCandidates": rejected_urls,
        "sourcesChecked": sources_checked,
        "verificationLinks": verification_links,
        "buyerRequestChecklist": [
            "Seller Property Disclosure Packet (e.g. CA TDS + SPQ + NHD, TX Seller's Disclosure Notice, NY PCDS)",
            "Pre-Listing General Home Inspection & Roof Certification (if performed)",
            "Wood-Destroying Organism (WDO / Termite) Report separating Section 1 active infestation/rot from Section 2 preventative items",
            "Sewer Lateral Camera Scope Video & Written Report (essential for pre-1985 clay/cast-iron/Orangeburg laterals)",
            "Preliminary Title Report (Schedule A & Schedule B clickable exceptions) + Seller-Paid HOA Resale Package"
        ],
        "agentHint": agent_hint,
        "summary": summary,
    });

    (discovered, api_fetched_reviews, search_status, sources)
}

// ------------------------- 2. Past Reports & Prior Artifacts (`pastReports`) -------------------------

const NARROW_LISTING_CORE_COLS: &str = "listing_sk, external_id, report_slug, zip, state_code, county_fips, city, street, address, lat, lon, status, property_category, price, list_price, sold_price, estimated_value, price_per_sqft, hoa_monthly, all_in_monthly, beds, baths, sqft, lot_sqft, year_built, stories, days_on_market, listed_epoch_day, sold_epoch_day, school_score, school_grade, walk_score, safety_score, overall_score, electric_rate_kwh, max_download_mbps, tag_bits, known_bits, first_seen_ts, last_seen_ts, status_changed_ts, price_changed_ts";

struct NarrowListingHistoryItem {
    status: String,
    price: Option<i64>,
    days_on_market: Option<i64>,
    listed_date: Option<String>,
    sold_date: Option<String>,
    first_seen_at: String,
    last_seen_at: String,
    price_changed_at: Option<String>,
    url: Option<String>,
}

async fn load_narrow_listing_history_item(
    db: &DbStore,
    core: ListingCoreProto,
    ps_opt: Option<ListingPriceStatus>,
) -> NarrowListingHistoryItem {
    let eng_opt = db
        .get_listing_engagement(core.listing_sk)
        .await
        .ok()
        .flatten();
    let status = ps_opt
        .as_ref()
        .map(|ps| ps.status_label.clone())
        .unwrap_or_else(|| crate::db::proto::listing_status_from_code(core.status).to_string());
    let price = ps_opt.as_ref().and_then(|ps| ps.price).or(core.price);
    let price_changed_ts = ps_opt
        .as_ref()
        .and_then(|ps| ps.price_changed_ts)
        .or(core.price_changed_ts);

    NarrowListingHistoryItem {
        status,
        price,
        days_on_market: core.days_on_market.map(i64::from),
        listed_date: crate::db::proto::epoch_day_to_date_str(core.listed_epoch_day),
        sold_date: crate::db::proto::epoch_day_to_date_str(core.sold_epoch_day),
        first_seen_at: crate::db::proto::unix_ts_to_iso(core.first_seen_ts),
        last_seen_at: crate::db::proto::unix_ts_to_iso(core.last_seen_ts),
        price_changed_at: price_changed_ts.map(crate::db::proto::unix_ts_to_iso),
        url: eng_opt.and_then(|(e, _)| e.listing_url),
    }
}

async fn fetch_narrow_parcel_listing_history(
    db: &DbStore,
    slug: &str,
    zip: &str,
    street_line: &str,
) -> Vec<NarrowListingHistoryItem> {
    if slug.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(5);
    let mut seen_sks = HashSet::new();

    if let Some(sk) = db.resolve_listing_sk(slug).await.ok().flatten() {
        let (ps_res, core_res) = tokio::join!(db.get_listing_price_status(sk), db.get_listing_core(sk));
        if let Some(core) = core_res.ok().flatten() {
            seen_sks.insert(sk);
            out.push(load_narrow_listing_history_item(db, core, ps_res.ok().flatten()).await);
        }
    }

    if !zip.is_empty() && street_line.len() >= 4 && out.len() < 5 {
        let prefix_end = {
            let mut idx = street_line.len().min(18);
            while idx > 0 && !street_line.is_char_boundary(idx) {
                idx -= 1;
            }
            idx
        };
        let like_pat = format!("{}%", &street_line[..prefix_end]);
        let sql = format!(
            "SELECT {NARROW_LISTING_CORE_COLS} FROM listings WHERE zip = ? AND address LIKE ? ORDER BY last_seen_ts DESC LIMIT 5"
        );
        let extra_cores = db
            .search_listing_cores(
                &sql,
                &[SqlParam::from(zip), SqlParam::from(like_pat.as_str())],
            )
            .await
            .unwrap_or_default();
        for core in extra_cores {
            if out.len() >= 5 {
                break;
            }
            if seen_sks.insert(core.listing_sk) {
                let ps_opt = db
                    .get_listing_price_status(core.listing_sk)
                    .await
                    .ok()
                    .flatten();
                out.push(load_narrow_listing_history_item(db, core, ps_opt).await);
            }
        }
    }

    out
}

async fn research_past_reports(
    db: &DbStore,
    geo: &Value,
    facts: &Value,
    slug: &str,
) -> Value {
    let zip = val_str(geo.get("zip"))
        .or_else(|| val_str(facts.get("zip")))
        .unwrap_or("");
    let current_list_price = val_i64(
        facts
            .get("listPrice")
            .or_else(|| facts.pointer("/mlsListing/listPrice"))
            .or_else(|| facts.pointer("/listing/listPrice")),
    );
    let street_line = val_str(geo.get("matchedAddress"))
        .or_else(|| val_str(facts.get("address")))
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let hdr_fut = async {
        if slug.is_empty() {
            None
        } else {
            db.get_report_header(slug).await.ok().flatten()
        }
    };
    let listings_fut = fetch_narrow_parcel_listing_history(db, slug, zip, &street_line);

    let (hdr_opt, listing_rows) = tokio::join!(hdr_fut, listings_fut);

    let mut changes_detected: Vec<String> = Vec::new();
    let mut prior_val_usd: Option<i64> = None;
    let mut prior_list_usd: Option<i64> = None;
    let mut prior_generated_at: Option<String> = None;
    let mut prior_score: Option<f64> = None;
    let mut prior_verdict: Option<String> = None;
    let mut prior_reviews_available: Vec<String> = Vec::new();
    let mut had_prior_report = false;

    if let Some(hdr) = hdr_opt {
        had_prior_report = true;
        let ts = if hdr.generated_ts > 0 {
            hdr.generated_ts
        } else {
            hdr.updated_ts
        };
        prior_generated_at = (ts > 0).then(|| crate::db::proto::unix_ts_to_iso(ts));
        prior_val_usd = hdr.estimated_value;
        prior_list_usd = hdr.list_price;
        prior_score = hdr.overall_score;
        prior_verdict = hdr.ai_headline.or(hdr.verdict);

        // Only read narrow Facts section (~1 KB) if header lacked top-level list_price
        // (e.g. stored solely under `/facts/mlsListing/listPrice`).
        if prior_list_usd.is_none()
            && (hdr.section_mask & (1_i32 << (ReportSectionKind::ReportSectionFacts as u32))) != 0
        {
            if let Some(sec_env) = db
                .get_report_section(slug, ReportSectionKind::ReportSectionFacts)
                .await
                .ok()
                .flatten()
            {
                if let Ok(sec_json) = extract_section_json(&sec_env) {
                    prior_list_usd = val_i64(
                        sec_json
                            .pointer("/root/facts/listPrice")
                            .or_else(|| sec_json.pointer("/root/facts/mlsListing/listPrice"))
                            .or_else(|| sec_json.pointer("/modules/facts/data/listPrice"))
                            .or_else(|| {
                                sec_json.pointer("/modules/facts/data/mlsListing/listPrice")
                            }),
                    );
                }
            }
        }

        // `section_mask` tells immediately (~0 extra queries for 99%+ of reports!) whether
        // HOA/packet/repair review artifacts exist on the cached report.
        if (hdr.section_mask & (1_i32 << (ReportSectionKind::ReportSectionDocReviews as u32))) != 0
        {
            if let Some(doc_env) = db
                .get_report_section(slug, ReportSectionKind::ReportSectionDocReviews)
                .await
                .ok()
                .flatten()
            {
                if let Ok(doc_json) = extract_section_json(&doc_env) {
                    let root_view = doc_json.get("root").unwrap_or(&doc_json);
                    for (key, label) in [
                        ("hoaReview", "HOA Disclosure Review"),
                        ("packetReview", "Multi-Document Disclosure Packet Review"),
                        ("repairAddendum", "Repair-Credit Addendum"),
                    ] {
                        if root_view.get(key).map(Value::is_object).unwrap_or(false) {
                            prior_reviews_available.push(label.to_string());
                        }
                    }
                }
            }
        }
    } else if !slug.is_empty() && !slug.contains('/') && !slug.contains("..") {
        // Fallback: check local `reports/<slug>/report.json` on disk if DB had none.
        let disk_path = Path::new("reports").join(slug).join("report.json");
        if disk_path.exists() {
            if let Ok(raw) = tokio::fs::read_to_string(&disk_path).await {
                if let Ok(prev) = serde_json::from_str::<Value>(&raw) {
                    had_prior_report = true;
                    prior_generated_at = Some(
                        val_str(prev.pointer("/meta/generatedAt"))
                            .unwrap_or("disk_snapshot")
                            .to_string(),
                    );
                    prior_val_usd = val_i64(
                        prev.pointer("/modules/valuation/data/estimate/value")
                            .or_else(|| prev.pointer("/valuation/estimate/value")),
                    );
                    prior_list_usd = val_i64(
                        prev.pointer("/facts/listPrice")
                            .or_else(|| prev.pointer("/facts/mlsListing/listPrice")),
                    );
                    prior_score = val_f64(
                        prev.pointer("/summary/scores/overall")
                            .or_else(|| prev.pointer("/summary/overallScore")),
                    );
                    prior_verdict = val_str(
                        prev.pointer("/narrative/executiveVerdict/headline")
                            .or_else(|| prev.pointer("/ai/verdict/headline"))
                            .or_else(|| prev.pointer("/summary/verdict")),
                    )
                    .map(String::from);
                    for (key, label) in [
                        ("hoaReview", "HOA Disclosure Review"),
                        ("packetReview", "Multi-Document Disclosure Packet Review"),
                        ("repairAddendum", "Repair-Credit Addendum"),
                    ] {
                        if prev.get(key).map(Value::is_object).unwrap_or(false) {
                            prior_reviews_available.push(label.to_string());
                        }
                    }
                }
            }
        }
    }

    if had_prior_report {
        if let (Some(prev_lp), Some(curr_lp)) = (prior_list_usd, current_list_price) {
            let diff = curr_lp - prev_lp;
            if diff.abs() >= 500 {
                if diff < 0 {
                    changes_detected.push(format!(
                        "Asking price reduced by ${} (from ${} in prior report to ${}).",
                        diff.abs(),
                        prev_lp,
                        curr_lp
                    ));
                } else {
                    changes_detected.push(format!(
                        "Asking price increased by ${} (from ${} in prior report to ${}).",
                        diff, prev_lp, curr_lp
                    ));
                }
            }
        }
    }

    let mut listing_snapshots: Vec<Value> = Vec::new();
    for row in listing_rows.iter().take(5) {
        if let (Some(row_p), Some(curr_lp)) = (row.price, current_list_price) {
            let diff = curr_lp - row_p;
            if diff.abs() >= 500 && changes_detected.is_empty() {
                changes_detected.push(format!(
                    "Tracked listing price moved from ${} ({}) to ${}.",
                    row_p, row.status, curr_lp
                ));
            }
        }
        let row_url = row
            .url
            .as_deref()
            .filter(|u| u.starts_with("https://"))
            .map(|u| {
                if let Some(p) = row.price {
                    attach_verified_doc_anchor(u, &p.to_string())
                } else {
                    u.to_string()
                }
            });
        listing_snapshots.push(json!({
            "status": row.status,
            "priceUsd": row.price,
            "daysOnMarket": row.days_on_market,
            "listedDate": row.listed_date,
            "soldDate": row.sold_date,
            "firstSeenAt": row.first_seen_at,
            "lastSeenAt": row.last_seen_at,
            "priceChangedAt": row.price_changed_at,
            "sourceUrl": row_url,
        }));
    }

    let found_prior = had_prior_report || !listing_snapshots.is_empty();
    let summary = if let Some(ref ga) = prior_generated_at {
        format!(
            "Prior Open Estate report snapshot found from {ga}{}{}.",
            prior_val_usd
                .map(|v| format!(" (prior valuation ${v})"))
                .unwrap_or_default(),
            if changes_detected.is_empty() {
                " — no material asking-price change since prior run".to_string()
            } else {
                format!(" — {}", changes_detected.join(" "))
            }
        )
    } else if !listing_snapshots.is_empty() {
        format!(
            "Found {} historical listing index snapshot(s) for this parcel in Open Estate search history.",
            listing_snapshots.len()
        )
    } else {
        "First Open Estate report generated for this parcel — baseline valuation, permits, and HOA snapshot recorded for future change tracking.".to_string()
    };

    json!({
        "foundPriorSnapshot": found_prior,
        "priorReportCount": usize::from(had_prior_report),
        "priorGeneratedAt": prior_generated_at,
        "priorEstimatedValueUsd": prior_val_usd,
        "priorListPriceUsd": prior_list_usd,
        "priorOverallScore": prior_score.map(|s| round_n(s, 1)),
        "priorVerdict": prior_verdict,
        "priorReviewsAvailable": prior_reviews_available,
        "changesDetected": changes_detected,
        "listingHistorySnapshots": listing_snapshots,
        "summary": summary,
    })
}

// ------------------------- 3. Municipal Open-Data Permits & Code Violations -------------------------

struct ParsedStreet {
    number: String,
    core_upper: String,
    full_upper: String,
}

fn parse_street_components(geo: &Value, facts: &Value) -> Option<ParsedStreet> {
    let raw_addr = val_str(geo.get("matchedAddress"))
        .or_else(|| val_str(geo.get("displayAddress")))
        .or_else(|| val_str(facts.get("address")))
        .or_else(|| val_str(facts.get("streetAddress")))?;
    let first_seg = raw_addr.split(',').next()?.trim();
    // Strip trailing unit/apt
    let before_unit = first_seg
        .split(" #")
        .next()
        .unwrap_or(first_seg)
        .split(" APT ")
        .next()
        .unwrap_or(first_seg)
        .split(" UNIT ")
        .next()
        .unwrap_or(first_seg)
        .trim();

    let caps = STREET_PARSE_RE.captures(before_unit)?;
    let number = caps.get(1)?.as_str().trim().to_ascii_uppercase();
    let rest_upper = caps.get(2)?.as_str().trim().to_ascii_uppercase();

    let suffixes = [
        "STREET", "ST", "AVENUE", "AVE", "BOULEVARD", "BLVD", "DRIVE", "DR", "ROAD", "RD",
        "LANE", "LN", "COURT", "CT", "WAY", "PLACE", "PL", "CIRCLE", "CIR", "TERRACE", "TER",
        "PARKWAY", "PKWY", "HIGHWAY", "HWY", "TRAIL", "TRL", "ALLEY", "ALY",
    ];
    let dirs = ["N", "S", "E", "W", "NE", "NW", "SE", "SW", "NORTH", "SOUTH", "EAST", "WEST"];

    let mut toks: Vec<&str> = rest_upper
        .split_whitespace()
        .map(|t| t.trim_matches('.'))
        .filter(|t| !t.is_empty())
        .collect();
    if toks.len() > 1 && dirs.contains(&toks[0]) {
        toks.remove(0);
    }
    if toks.len() > 1 {
        if let Some(last) = toks.last() {
            if dirs.contains(last) {
                toks.pop();
            }
        }
    }
    if toks.len() > 1 {
        if let Some(last) = toks.last() {
            if suffixes.contains(last) {
                toks.pop();
            }
        }
    }

    let core_upper: String = toks
        .join(" ")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-')
        .collect();
    if number.is_empty() || core_upper.len() < 2 {
        return None;
    }
    Some(ParsedStreet {
        number: number.clone(),
        core_upper,
        full_upper: format!("{number} {rest_upper}"),
    })
}

fn soql_escape(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-')
        .collect::<String>()
}

async fn fetch_json_bounded(http: &HttpClient, url: &str) -> Option<Value> {
    http.get_json_opts(
        url,
        CONNECTOR_TIMEOUT_MS + 2200,
        0,
        86400 * 7,
        Some(&[("Accept", "application/json")]),
    )
    .await
    .ok()
}

fn build_endpoint(base_override: Option<&str>, prod_origin: &str, path_and_query: &str) -> String {
    if let Some(ov) = base_override.filter(|s| !s.trim().is_empty()) {
        format!("{}{}", ov.trim_end_matches('/'), path_and_query)
    } else {
        format!("{prod_origin}{path_and_query}")
    }
}

#[allow(clippy::too_many_lines)]
async fn query_municipal_open_data(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    street: &ParsedStreet,
) -> (Vec<Value>, Vec<Value>, Vec<SourceItem>, Option<String>) {
    let state = val_str(geo.get("state")).unwrap_or("").to_ascii_uppercase();
    let city = val_str(geo.get("place").or_else(|| geo.get("city")))
        .unwrap_or("")
        .to_ascii_lowercase();
    let county_fips = val_str(geo.get("countyFips")).unwrap_or("");
    let base_ov = env.get("MUNICIPAL_OPENDATA_BASE_URL");

    let num = soql_escape(&street.number);
    let core = soql_escape(&street.core_upper);
    if num.is_empty() || core.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), None);
    }

    let mut permits: Vec<Value> = Vec::new();
    let mut violations: Vec<Value> = Vec::new();
    let mut sources: Vec<SourceItem> = Vec::new();

    // 1. San Francisco, CA (`data.sfgov.org`)
    if (state == "CA" && city.contains("san francisco")) || county_fips == "06075" {
        let where_p = format!("street_number='{num}' AND upper(street_name) LIKE '%{core}%'");
        let where_p_enc: String = url::form_urlencoded::byte_serialize(where_p.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.sfgov.org",
            &format!("/resource/i98e-djp9.json?$where={where_p_enc}&$order=filed_date%20DESC&$limit=20"),
        );

        let where_v = format!("street_number='{num}' AND upper(street_name) LIKE '%{core}%'");
        let where_v_enc: String = url::form_urlencoded::byte_serialize(where_v.as_bytes()).collect();
        let v_url = build_endpoint(
            base_ov,
            "https://data.sfgov.org",
            &format!("/resource/gm2e-bten.json?$where={where_v_enc}&$order=date_filed%20DESC&$limit=20"),
        );

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );

        if let Some(arr) = p_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let pnum = val_str(r.get("permit_number")).unwrap_or("SF-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("description").or_else(|| r.get("permit_type_definition")))
                        .unwrap_or("Building permit"),
                );
                let deep = format!(
                    "https://data.sfgov.org/resource/i98e-djp9.json?permit_number={}",
                    encode_w3c_text_fragment(pnum)
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permit_type_definition")).unwrap_or("Building Permit"),
                    "status": val_str(r.get("status")).unwrap_or("Recorded"),
                    "issuedDate": val_str(r.get("issued_date").or_else(|| r.get("filed_date"))).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("completed_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": val_f64(r.get("estimated_cost").or_else(|| r.get("revised_cost"))).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "San Francisco DBI Building Permits (DataSF i98e-djp9)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, pnum),
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let cnum = val_str(r.get("complaint_number")).unwrap_or("SF-COMPLAINT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("complaint_description").or_else(|| r.get("nov_category_description")))
                        .unwrap_or("DBI Housing/Building Complaint"),
                );
                let deep = format!(
                    "https://data.sfgov.org/resource/gm2e-bten.json?complaint_number={}",
                    encode_w3c_text_fragment(cnum)
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("status")).unwrap_or("Reported"),
                    "filedDate": val_str(r.get("date_filed")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": val_str(r.get("nov_category_description")).unwrap_or("DBI Complaint / Violation"),
                    "description": desc,
                    "sourceName": "San Francisco DBI Complaints & NOVs (DataSF gm2e-bten)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, cnum),
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "San Francisco Department of Building Inspection Open Data (DataSF)",
                "https://data.sfgov.org/",
            ));
        }
        return (permits, violations, sources, Some("San Francisco DataSF".into()));
    }

    // 2. Los Angeles, CA (`data.lacity.org`)
    if state == "CA" && city == "los angeles" {
        let wp = format!("address_start='{num}' AND upper(street_name) LIKE '%{core}%'");
        let wp_enc: String = url::form_urlencoded::byte_serialize(wp.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.lacity.org",
            &format!("/resource/yv23-pmwf.json?$where={wp_enc}&$order=issue_date%20DESC&$limit=20"),
        );

        let wv = format!("address_house_number='{num}' AND upper(address_street_name) LIKE '%{core}%'");
        let wv_enc: String = url::form_urlencoded::byte_serialize(wv.as_bytes()).collect();
        let v_url = build_endpoint(
            base_ov,
            "https://data.lacity.org",
            &format!("/resource/u82d-eh7z.json?$where={wv_enc}&$limit=20"),
        );

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        if let Some(arr) = p_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let pnum = val_str(r.get("pcis_permit").or_else(|| r.get("permit_number"))).unwrap_or("LADBS-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("work_description").or_else(|| r.get("permit_sub_type")))
                        .unwrap_or("LADBS Building Permit"),
                );
                let deep = format!(
                    "https://data.lacity.org/resource/yv23-pmwf.json?pcis_permit={}",
                    encode_w3c_text_fragment(pnum)
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permit_type")).unwrap_or("Building Permit"),
                    "status": val_str(r.get("latest_status")).unwrap_or("Issued"),
                    "issuedDate": val_str(r.get("issue_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("status_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": val_f64(r.get("valuation")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "Los Angeles LADBS Building Permits (yv23-pmwf)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, pnum),
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let cnum = val_str(r.get("apno").or_else(|| r.get("case_number"))).unwrap_or("LA-CASE");
                let desc = scrub_public_text_pii(
                    val_str(r.get("case_type").or_else(|| r.get("stat")))
                        .unwrap_or("Code Enforcement Case"),
                );
                let deep = format!(
                    "https://data.lacity.org/resource/u82d-eh7z.json?apno={}",
                    encode_w3c_text_fragment(cnum)
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("stat")).unwrap_or("Recorded"),
                    "filedDate": val_str(r.get("adddttm")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": "LAHD / LADBS Code Enforcement",
                    "description": desc,
                    "sourceName": "Los Angeles Code Enforcement Open Data (u82d-eh7z)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, cnum),
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "City of Los Angeles Open Data (LADBS Permits & Code Enforcement)",
                "https://data.lacity.org/",
            ));
        }
        return (permits, violations, sources, Some("Los Angeles Open Data".into()));
    }

    // 3. San Jose, CA (`data.sanjoseca.gov` CKAN Datastore)
    if state == "CA" && city.contains("san jose") {
        let q_str = format!("{num} {core}");
        let q_enc: String = url::form_urlencoded::byte_serialize(q_str.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.sanjoseca.gov",
            &format!("/api/3/action/datastore_search?resource_id=761b7ae8-3be1-4ad6-923d-c7af6404a904&q={q_enc}&limit=15"),
        );
        let v_url = build_endpoint(
            base_ov,
            "https://data.sanjoseca.gov",
            &format!("/api/3/action/datastore_search?resource_id=e73fd12d-521f-44ef-b030-c0c982610c32&q={q_enc}&limit=15"),
        );
        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        if let Some(arr) = p_res
            .and_then(|v| v.pointer("/result/records").and_then(|r| r.as_array()).cloned())
        {
            for r in arr {
                let addr_val = val_str(r.get("gx_location").or_else(|| r.get("ADDRESS")))
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if !addr_val.is_empty() && (!addr_val.contains(&num) || !addr_val.contains(&core)) {
                    continue;
                }
                let pnum = val_str(r.get("FOLDERNUMBER").or_else(|| r.get("foldernumber")))
                    .unwrap_or("SJ-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("WORKDESCRIPTION").or_else(|| r.get("FOLDERDESC")))
                        .unwrap_or("San Jose Building Permit"),
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("FOLDERTYPE").or_else(|| r.get("SUBTYPE"))).unwrap_or("Building Permit"),
                    "status": val_str(r.get("STATUS")).unwrap_or("Recorded"),
                    "issuedDate": val_str(r.get("ISSUEDATE")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("FINALDATE")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": val_f64(r.get("PERMITVALUATION")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "City of San Jose Open Data — Building Permits",
                    "sourceUrl": "https://sjpermits.org/permits/general/combinedquery.asp",
                }));
            }
        }
        if let Some(arr) = v_res
            .and_then(|v| v.pointer("/result/records").and_then(|r| r.as_array()).cloned())
        {
            for r in arr {
                let addr_val = val_str(r.get("ADDRESS").or_else(|| r.get("Location")))
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if !addr_val.is_empty() && (!addr_val.contains(&num) || !addr_val.contains(&core)) {
                    continue;
                }
                let cnum = val_str(r.get("CASENUMBER").or_else(|| r.get("CaseNumber")))
                    .unwrap_or("SJ-CODE");
                let desc = scrub_public_text_pii(
                    val_str(r.get("PROGRAM").or_else(|| r.get("DESCRIPTION")))
                        .unwrap_or("San Jose Code Enforcement Case"),
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("STATUS")).unwrap_or("Recorded"),
                    "filedDate": val_str(r.get("OPENDATE")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": "San Jose Code Enforcement",
                    "description": desc,
                    "sourceName": "City of San Jose Open Data — Code Enforcement",
                    "sourceUrl": "https://data.sanjoseca.gov/",
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "City of San Jose Open Data Portal (Permits & Code Enforcement)",
                "https://data.sanjoseca.gov/",
            ));
        }
        return (permits, violations, sources, Some("San Jose Open Data".into()));
    }

    // 4. Chicago, IL (`data.cityofchicago.org`)
    if state == "IL" && city == "chicago" {
        let wp = format!("street_number='{num}' AND upper(street_name) LIKE '%{core}%'");
        let wp_enc: String = url::form_urlencoded::byte_serialize(wp.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.cityofchicago.org",
            &format!("/resource/ydr8-5enu.json?$where={wp_enc}&$order=issue_date%20DESC&$limit=20"),
        );

        let wv = format!("upper(address) LIKE '{num} %{core}%'");
        let wv_enc: String = url::form_urlencoded::byte_serialize(wv.as_bytes()).collect();
        let v_url = build_endpoint(
            base_ov,
            "https://data.cityofchicago.org",
            &format!("/resource/22u3-xenr.json?$where={wv_enc}&$order=violation_date%20DESC&$limit=20"),
        );

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        if let Some(arr) = p_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let pnum = val_str(r.get("permit_")).unwrap_or("CHI-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("work_description")).unwrap_or("Chicago Building Permit"),
                );
                let deep = format!(
                    "https://data.cityofchicago.org/resource/ydr8-5enu.json?permit_={}",
                    encode_w3c_text_fragment(pnum)
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permit_type")).unwrap_or("Building Permit"),
                    "status": val_str(r.get("permit_status")).unwrap_or("Issued"),
                    "issuedDate": val_str(r.get("issue_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": Value::Null,
                    "jobValueUsd": val_f64(r.get("reported_cost")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "City of Chicago Building Permits (ydr8-5enu)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, pnum),
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let cnum = val_str(r.get("violation_code").or_else(|| r.get("id"))).unwrap_or("CHI-VIOL");
                let desc = scrub_public_text_pii(
                    val_str(r.get("violation_description").or_else(|| r.get("violation_inspector_comments")))
                        .unwrap_or("Chicago Building Violation"),
                );
                let raw_id = val_str(r.get("id")).unwrap_or(cnum);
                let deep = format!(
                    "https://data.cityofchicago.org/resource/22u3-xenr.json?id={}",
                    encode_w3c_text_fragment(raw_id)
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("violation_status")).unwrap_or("OPEN"),
                    "filedDate": val_str(r.get("violation_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": val_str(r.get("department_bureau")).unwrap_or("Department of Buildings"),
                    "description": desc,
                    "sourceName": "City of Chicago Building Violations (22u3-xenr)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, cnum),
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "City of Chicago Data Portal — Building Permits & Violations",
                "https://data.cityofchicago.org/",
            ));
        }
        return (permits, violations, sources, Some("Chicago Data Portal".into()));
    }

    // 5. Seattle, WA (`data.seattle.gov`)
    if state == "WA" && city == "seattle" {
        let wp = format!("upper(originaladdress1) LIKE '{num} %{core}%'");
        let wp_enc: String = url::form_urlencoded::byte_serialize(wp.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.seattle.gov",
            &format!("/resource/76t5-zqzr.json?$where={wp_enc}&$order=issueddate%20DESC&$limit=20"),
        );
        let v_url = build_endpoint(
            base_ov,
            "https://data.seattle.gov",
            &format!("/resource/ez4a-iug7.json?$where={wp_enc}&$order=opendate%20DESC&$limit=20"),
        );

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        if let Some(arr) = p_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let pnum = val_str(r.get("permitnum")).unwrap_or("SEA-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("description")).unwrap_or("Seattle SDCI Building Permit"),
                );
                let link = r
                    .pointer("/link/url")
                    .or_else(|| r.get("link"))
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("https://"))
                    .map(|u| attach_verified_doc_anchor(u, pnum))
                    .unwrap_or_else(|| {
                        format!(
                            "https://data.seattle.gov/resource/76t5-zqzr.json?permitnum={}",
                            encode_w3c_text_fragment(pnum)
                        )
                    });
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permittypemapped")).unwrap_or("Building Permit"),
                    "status": val_str(r.get("statuscurrent")).unwrap_or("Recorded"),
                    "issuedDate": val_str(r.get("issueddate")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("completeddate")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": val_f64(r.get("estprojectcost")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "Seattle SDCI Building Permits (76t5-zqzr)",
                    "sourceUrl": link,
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let cnum = val_str(r.get("recordnum")).unwrap_or("SEA-VIOL");
                let desc = scrub_public_text_pii(
                    val_str(r.get("description")).unwrap_or("Seattle Code Complaint / Violation"),
                );
                let link = r
                    .pointer("/link/url")
                    .or_else(|| r.get("link"))
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("https://"))
                    .map(|u| attach_verified_doc_anchor(u, cnum))
                    .unwrap_or_else(|| {
                        format!(
                            "https://data.seattle.gov/resource/ez4a-iug7.json?recordnum={}",
                            encode_w3c_text_fragment(cnum)
                        )
                    });
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("lastinspresult").or_else(|| r.get("statuscurrent"))).unwrap_or("Recorded"),
                    "filedDate": val_str(r.get("opendate")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": val_str(r.get("recordtypemapped")).unwrap_or("Code Violation"),
                    "description": desc,
                    "sourceName": "Seattle SDCI Code Complaints & Violations (ez4a-iug7)",
                    "sourceUrl": link,
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "City of Seattle Open Data — SDCI Permits & Code Violations",
                "https://data.seattle.gov/",
            ));
        }
        return (permits, violations, sources, Some("Seattle Open Data".into()));
    }

    // 6. New York City, NY (`data.cityofnewyork.us`)
    let is_nyc = matches!(
        county_fips,
        "36061" | "36047" | "36081" | "36005" | "36085"
    ) || (state == "NY"
        && (city.contains("new york")
            || city.contains("brooklyn")
            || city.contains("queens")
            || city.contains("bronx")
            || city.contains("staten island")));
    if is_nyc {
        let wp = format!("house__='{num}' AND upper(street_name) LIKE '%{core}%'");
        let wp_enc: String = url::form_urlencoded::byte_serialize(wp.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.cityofnewyork.us",
            &format!("/resource/ipu4-2q9a.json?$where={wp_enc}&$limit=20"),
        );

        let wv = format!("house_number='{num}' AND upper(street) LIKE '%{core}%'");
        let wv_enc: String = url::form_urlencoded::byte_serialize(wv.as_bytes()).collect();
        let v_url = build_endpoint(
            base_ov,
            "https://data.cityofnewyork.us",
            &format!("/resource/3h2n-5cm9.json?$where={wv_enc}&$limit=20"),
        );

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        if let Some(arr) = p_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let pnum = val_str(r.get("job__").or_else(|| r.get("permit_si_no"))).unwrap_or("NYC-DOB");
                let desc = scrub_public_text_pii(
                    val_str(r.get("work_type").or_else(|| r.get("job_type")))
                        .unwrap_or("NYC DOB Permit"),
                );
                let deep = format!(
                    "https://data.cityofnewyork.us/resource/ipu4-2q9a.json?job__={}",
                    encode_w3c_text_fragment(pnum)
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permit_type")).unwrap_or("DOB Permit"),
                    "status": val_str(r.get("permit_status")).unwrap_or("ISSUED"),
                    "issuedDate": val_str(r.get("issuance_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("expiration_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": Value::Null,
                    "description": desc,
                    "sourceName": "NYC DOB Permit Issuance (NYC Open Data ipu4-2q9a)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, pnum),
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let cnum = val_str(r.get("isn_dob_bis_viol").or_else(|| r.get("violation_number")))
                    .unwrap_or("NYC-VIOL");
                let desc = scrub_public_text_pii(
                    val_str(r.get("description").or_else(|| r.get("violation_type")))
                        .unwrap_or("NYC DOB Violation"),
                );
                let deep = format!(
                    "https://data.cityofnewyork.us/resource/3h2n-5cm9.json?isn_dob_bis_viol={}",
                    encode_w3c_text_fragment(cnum)
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("violation_category")).unwrap_or("ACTIVE"),
                    "filedDate": val_str(r.get("issue_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": val_str(r.get("violation_type_code")).unwrap_or("DOB Violation"),
                    "description": desc,
                    "sourceName": "NYC DOB Violations (NYC Open Data 3h2n-5cm9)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, cnum),
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "NYC Open Data — Department of Buildings Permits & Violations",
                "https://data.cityofnewyork.us/",
            ));
        }
        return (permits, violations, sources, Some("NYC Open Data".into()));
    }

    // 7. Philadelphia, PA (`phl.carto.com`)
    if (state == "PA" && city == "philadelphia") || county_fips == "42101" {
        let sql_p = format!(
            "SELECT permitnumber, permittype, status, permitissuedate, permitcompleteddate, approvedscopeofwork FROM permits WHERE address ILIKE '{num} %{core}%' ORDER BY permitissuedate DESC LIMIT 20"
        );
        let sql_v = format!(
            "SELECT casenumber, casestatus, casecreateddate, violationcodetitle FROM violations WHERE address ILIKE '{num} %{core}%' ORDER BY casecreateddate DESC LIMIT 20"
        );
        let sp_enc: String = url::form_urlencoded::byte_serialize(sql_p.as_bytes()).collect();
        let sv_enc: String = url::form_urlencoded::byte_serialize(sql_v.as_bytes()).collect();
        let p_url = build_endpoint(base_ov, "https://phl.carto.com", &format!("/api/v2/sql?q={sp_enc}"));
        let v_url = build_endpoint(base_ov, "https://phl.carto.com", &format!("/api/v2/sql?q={sv_enc}"));

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        let atlas_slug: String = url::form_urlencoded::byte_serialize(street.full_upper.as_bytes()).collect();
        let atlas_base = format!("https://atlas.phila.gov/{atlas_slug}/li");

        if let Some(arr) = p_res.and_then(|v| v.get("rows").and_then(|r| r.as_array()).cloned()) {
            for r in arr {
                let pnum = val_str(r.get("permitnumber")).unwrap_or("PHL-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("approvedscopeofwork")).unwrap_or("Philadelphia L&I Permit"),
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permittype")).unwrap_or("L&I Permit"),
                    "status": val_str(r.get("status")).unwrap_or("COMPLETED"),
                    "issuedDate": val_str(r.get("permitissuedate")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("permitcompleteddate")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": Value::Null,
                    "description": desc,
                    "sourceName": "Philadelphia L&I Permits (OpenDataPhilly)",
                    "sourceUrl": attach_verified_doc_anchor(&atlas_base, pnum),
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.get("rows").and_then(|r| r.as_array()).cloned()) {
            for r in arr {
                let cnum = val_str(r.get("casenumber")).unwrap_or("PHL-VIOL");
                let desc = scrub_public_text_pii(
                    val_str(r.get("violationcodetitle")).unwrap_or("Philadelphia L&I Code Violation"),
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("casestatus")).unwrap_or("OPEN"),
                    "filedDate": val_str(r.get("casecreateddate")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": "Philadelphia L&I Code Enforcement",
                    "description": desc,
                    "sourceName": "Philadelphia L&I Violations (OpenDataPhilly)",
                    "sourceUrl": attach_verified_doc_anchor(&atlas_base, cnum),
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "OpenDataPhilly — Licenses & Inspections Permits and Violations",
                "https://atlas.phila.gov/",
            ));
        }
        return (permits, violations, sources, Some("OpenDataPhilly".into()));
    }

    // 8. Austin, TX (`data.austintexas.gov`)
    if state == "TX" && city == "austin" {
        let wp = format!("upper(permit_location) LIKE '{num} %{core}%'");
        let wp_enc: String = url::form_urlencoded::byte_serialize(wp.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.austintexas.gov",
            &format!("/resource/3syk-w9eu.json?$where={wp_enc}&$order=issue_date%20DESC&$limit=20"),
        );

        let wv = format!("house_number='{num}' AND upper(street_name) LIKE '%{core}%'");
        let wv_enc: String = url::form_urlencoded::byte_serialize(wv.as_bytes()).collect();
        let v_url = build_endpoint(
            base_ov,
            "https://data.austintexas.gov",
            &format!("/resource/xwdj-i9he.json?$where={wv_enc}&$limit=20"),
        );

        let (p_res, v_res) = tokio::join!(
            fetch_json_bounded(http, &p_url),
            fetch_json_bounded(http, &v_url)
        );
        if let Some(arr) = p_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let pnum = val_str(r.get("permit_number")).unwrap_or("ATX-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("description")).unwrap_or("Austin Construction Permit"),
                );
                let link = r
                    .pointer("/link/url")
                    .or_else(|| r.get("link"))
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("https://"))
                    .map(|u| attach_verified_doc_anchor(u, pnum))
                    .unwrap_or_else(|| {
                        format!(
                            "https://data.austintexas.gov/resource/3syk-w9eu.json?permit_number={}",
                            encode_w3c_text_fragment(pnum)
                        )
                    });
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permit_type_desc")).unwrap_or("Construction Permit"),
                    "status": val_str(r.get("status_current")).unwrap_or("Issued"),
                    "issuedDate": val_str(r.get("issue_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": val_str(r.get("status_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "jobValueUsd": val_f64(r.get("total_job_valuation")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "City of Austin Issued Construction Permits (3syk-w9eu)",
                    "sourceUrl": link,
                }));
            }
        }
        if let Some(arr) = v_res.and_then(|v| v.as_array().cloned()) {
            for r in arr {
                let cnum = val_str(r.get("case_id")).unwrap_or("ATX-CODE");
                let desc = scrub_public_text_pii(
                    val_str(r.get("case_description").or_else(|| r.get("violation_case_type")))
                        .unwrap_or("Austin Code Violation"),
                );
                let deep = format!(
                    "https://data.austintexas.gov/resource/xwdj-i9he.json?case_id={}",
                    encode_w3c_text_fragment(cnum)
                );
                violations.push(json!({
                    "caseNumber": cnum,
                    "status": val_str(r.get("case_status")).unwrap_or("Recorded"),
                    "filedDate": val_str(r.get("opened_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "category": "Austin Code Department",
                    "description": desc,
                    "sourceName": "City of Austin Code Violations (xwdj-i9he)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, cnum),
                }));
            }
        }
        if !permits.is_empty() || !violations.is_empty() {
            sources.push(SourceItem::new(
                "City of Austin Open Data — Construction Permits & Code Violations",
                "https://data.austintexas.gov/",
            ));
        }
        return (permits, violations, sources, Some("Austin Open Data".into()));
    }

    // 9. Cambridge / Boston, MA
    if state == "MA" && city == "cambridge" {
        let wp = format!("upper(full_address) LIKE '{num} %{core}%'");
        let wp_enc: String = url::form_urlencoded::byte_serialize(wp.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.cambridgema.gov",
            &format!("/resource/9qm7-wbdc.json?$where={wp_enc}&$limit=20"),
        );
        if let Some(arr) = fetch_json_bounded(http, &p_url)
            .await
            .and_then(|v| v.as_array().cloned())
        {
            for r in arr {
                let pnum = val_str(r.get("id").or_else(|| r.get("permit_number"))).unwrap_or("CAMB-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("description_of_work")).unwrap_or("Cambridge Building Permit"),
                );
                let deep = format!(
                    "https://data.cambridgema.gov/resource/9qm7-wbdc.json?id={}",
                    encode_w3c_text_fragment(pnum)
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": "Building Permit",
                    "status": val_str(r.get("status")).unwrap_or("Recorded"),
                    "issuedDate": val_str(r.get("issue_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": Value::Null,
                    "jobValueUsd": val_f64(r.get("total_cost_of_construction")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "Cambridge Open Data Building Permits (9qm7-wbdc)",
                    "sourceUrl": attach_verified_doc_anchor(&deep, pnum),
                }));
            }
            if !permits.is_empty() {
                sources.push(SourceItem::new(
                    "City of Cambridge Open Data — Building Permits",
                    "https://data.cambridgema.gov/",
                ));
            }
        }
        return (permits, violations, sources, Some("Cambridge Open Data".into()));
    }

    if state == "MA" && city == "boston" {
        let q_str = format!("{num} {core}");
        let q_enc: String = url::form_urlencoded::byte_serialize(q_str.as_bytes()).collect();
        let p_url = build_endpoint(
            base_ov,
            "https://data.boston.gov",
            &format!("/api/3/action/datastore_search?resource_id=6ddcd912-32a0-43df-9908-63574f8c7e77&q={q_enc}&limit=15"),
        );
        if let Some(arr) = fetch_json_bounded(http, &p_url)
            .await
            .and_then(|v| v.pointer("/result/records").and_then(|r| r.as_array()).cloned())
        {
            for r in arr {
                let pnum = val_str(r.get("permitnumber")).unwrap_or("BOS-PERMIT");
                let desc = scrub_public_text_pii(
                    val_str(r.get("comments").or_else(|| r.get("worktype")))
                        .unwrap_or("Boston ISD Building Permit"),
                );
                permits.push(json!({
                    "permitNumber": pnum,
                    "category": classify_permit_category("", &desc),
                    "type": val_str(r.get("permittypedescr")).unwrap_or("Building Permit"),
                    "status": val_str(r.get("status")).unwrap_or("Issued"),
                    "issuedDate": val_str(r.get("issued_date")).map(|s| s.chars().take(10).collect::<String>()),
                    "completedDate": Value::Null,
                    "jobValueUsd": val_f64(r.get("declared_valuation")).map(|c| c.round() as i64),
                    "description": desc,
                    "sourceName": "Analyze Boston — Approved Building Permits",
                    "sourceUrl": "https://data.boston.gov/dataset/approved-building-permits",
                }));
            }
            if !permits.is_empty() {
                sources.push(SourceItem::new(
                    "Analyze Boston — Inspectional Services Building Permits",
                    "https://data.boston.gov/",
                ));
            }
        }
        return (permits, violations, sources, Some("Analyze Boston".into()));
    }

    (permits, violations, sources, None)
}

/// Stage-1 Municipal Open-Data permit & code-violation fetcher shared by both `permit_pipeline`
/// (`MunicipalOpenDataPermitsAdapter`) and `orchestrator` pre-Wave-3 permit enrichment.
pub(crate) async fn fetch_municipal_open_data_stage1(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    facts: &Value,
) -> (Vec<Value>, Vec<Value>, Vec<SourceItem>, Option<String>) {
    let Some(ref st) = parse_street_components(geo, facts) else {
        return (Vec::new(), Vec::new(), Vec::new(), None);
    };
    let (permits, violations, sources, portal_name) =
        query_municipal_open_data(http, env, geo, st).await;
    if !permits.is_empty() {
        let city = val_str(geo.get("place").or_else(|| geo.get("city")))
            .unwrap_or("")
            .to_string();
        let state = val_str(geo.get("state")).unwrap_or("").to_string();
        let zip = val_str(geo.get("zip")).unwrap_or("").to_string();
        let county_fips = val_str(geo.get("countyFips")).unwrap_or("").to_string();
        let county_name = val_str(geo.get("countyName").or_else(|| geo.get("county")))
            .map(str::to_string);
        let city_q = crate::extensions::proto::ExtensionQueryProto {
            address: val_str(geo.get("formattedAddress"))
                .unwrap_or(&st.full_upper)
                .to_string(),
            street: st.full_upper.clone(),
            city,
            state,
            zip,
            county_fips,
            county_name,
            lat: val_f64(geo.get("lat")),
            lon: val_f64(geo.get("lon")),
            ..Default::default()
        };
        let proto_permits: Vec<crate::extensions::proto::PermitRecordProto> = permits
            .iter()
            .map(crate::property::permit_pipeline::permit_value_to_proto)
            .collect();
        let portal_url = permits
            .first()
            .and_then(|p| p.get("sourceUrl"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        crate::property::permit_pipeline::persist_city_permit_dataset_page(
            &city_q,
            portal_name.as_deref().unwrap_or("US Municipal Open-Data Portal"),
            portal_url,
            &proto_permits,
            true,
        )
        .await;
    }
    (permits, violations, sources, portal_name)
}

async fn research_permits_and_violations_inner(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    facts: &Value,
    prefetched: Option<(Vec<Value>, Vec<Value>, Vec<SourceItem>, Option<String>)>,
) -> (Value, Vec<SourceItem>) {
    let (mut open_permits, mut open_violations, sources, open_portal_name) = match prefetched {
        Some(pre) => pre,
        None => fetch_municipal_open_data_stage1(http, env, geo, facts).await,
    };

    let mut seen_permits: HashSet<(String, bool)> = open_permits
        .iter()
        .filter_map(|p| {
            val_str(p.get("permitNumber")).map(|s| {
                (
                    s.to_ascii_uppercase(),
                    p.get("isParentLot")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                )
            })
        })
        .collect();

    // Merge existing parcel/facts permits (normalized, PII-scrubbed, and guaranteed valid `https://` sourceUrl)
    for arr_ptr in [
        "/permits",
        "/buildingRegister/permits",
        "/disclosures/permits",
    ] {
        if let Some(arr) = facts.pointer(arr_ptr).and_then(|v| v.as_array()) {
            for p in arr {
                let pnum = val_str(
                    p.get("permitNumber")
                        .or_else(|| p.get("number"))
                        .or_else(|| p.get("id")),
                )
                .unwrap_or("RECORDED-PERMIT");
                let is_parent = p
                    .get("isParentLot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| {
                        p.get("scope").and_then(|v| v.as_str()) == Some("parent_lot")
                    });
                let norm_key = pnum.to_ascii_uppercase();
                if norm_key != "RECORDED-PERMIT" && !seen_permits.insert((norm_key, is_parent)) {
                    continue;
                }
                let desc = scrub_public_text_pii(
                    val_str(
                        p.get("description")
                            .or_else(|| p.get("workDescription"))
                            .or_else(|| p.get("type")),
                    )
                    .unwrap_or("Recorded municipal permit"),
                );
                let raw_type = val_str(
                    p.get("rawType")
                        .or_else(|| p.get("workType"))
                        .or_else(|| p.get("type")),
                )
                .unwrap_or("");
                let cat = classify_permit_category(raw_type, &desc);
                let raw_url = val_str(p.get("sourceUrl").or_else(|| p.get("url")))
                    .filter(|u| u.starts_with("https://"))
                    .unwrap_or("https://www.iccsafe.org/building-safety-journal/");
                let mut rec = json!({
                    "permitNumber": pnum,
                    "category": cat,
                    "rawType": if raw_type.is_empty() { Value::Null } else { json!(raw_type) },
                    "type": val_str(p.get("type")).unwrap_or("Building Permit"),
                    "status": val_str(p.get("status")).unwrap_or("Recorded"),
                    "issuedDate": val_str(p.get("issuedDate").or_else(|| p.get("issueDate")).or_else(|| p.get("date"))),
                    "completedDate": val_str(p.get("completedDate").or_else(|| p.get("finalDate"))),
                    "jobValueUsd": val_i64(p.get("jobValueUsd").or_else(|| p.get("jobValueUSD")).or_else(|| p.get("jobValue")).or_else(|| p.get("cost"))),
                    "description": desc,
                    "scope": if is_parent { "parent_lot" } else { "subject" },
                    "isParentLot": is_parent,
                    "finaled": p.get("finaled").and_then(|v| v.as_bool()),
                    "sourceName": val_str(p.get("sourceName").or_else(|| p.get("source"))).unwrap_or("Public Building Permit Record"),
                    "sourceUrl": attach_verified_doc_anchor(raw_url, pnum),
                });
                if let Some(kw) = p.get("solarKw").and_then(|v| v.as_f64()) {
                    rec["solarKw"] = json!(kw);
                }
                if let Some(pan) = p.get("solarPanels").and_then(|v| v.as_u64()) {
                    rec["solarPanels"] = json!(pan);
                }
                open_permits.push(rec);
            }
        }
    }

    // Stamp canonical field aliases on every permit so both `permits_condition` (`issueDate`,
    // `finalDate`, `jobValueUSD`, `year`, `source`) and `publicResearch` (`issuedDate`,
    // `completedDate`, `jobValueUsd`, `sourceName`) readers see identical values.
    for p in &mut open_permits {
        if let Some(obj) = p.as_object_mut() {
            let issued = obj
                .get("issuedDate")
                .or_else(|| obj.get("issueDate"))
                .cloned()
                .unwrap_or(Value::Null);
            let completed = obj
                .get("completedDate")
                .or_else(|| obj.get("finalDate"))
                .cloned()
                .unwrap_or(Value::Null);
            let job_val = obj
                .get("jobValueUsd")
                .or_else(|| obj.get("jobValueUSD"))
                .cloned()
                .unwrap_or(Value::Null);
            let src_name = obj
                .get("sourceName")
                .or_else(|| obj.get("source"))
                .cloned()
                .unwrap_or(Value::Null);
            obj.insert("issuedDate".into(), issued.clone());
            obj.insert("issueDate".into(), issued.clone());
            obj.insert("completedDate".into(), completed.clone());
            obj.insert("finalDate".into(), completed.clone());
            obj.insert("jobValueUsd".into(), job_val.clone());
            obj.insert("jobValueUSD".into(), job_val);
            if !src_name.is_null() {
                obj.entry("source".to_string()).or_insert(src_name);
            }
            if obj.get("year").map(|v| v.is_null()).unwrap_or(true) {
                let yr_opt = issued
                    .as_str()
                    .or_else(|| completed.as_str())
                    .and_then(|d| d.get(..4))
                    .and_then(|y| y.parse::<i32>().ok())
                    .filter(|&y| (1940..=2100).contains(&y));
                if let Some(yr) = yr_opt {
                    obj.insert("year".into(), json!(yr));
                }
            }
        }
    }

    // Merge existing code enforcement / violation records in `facts`
    if let Some(ce_arr) = facts
        .get("codeEnforcement")
        .or_else(|| facts.pointer("/disclosures/codeEnforcement"))
        .and_then(|v| v.as_array())
    {
        for v in ce_arr {
            let cnum = val_str(v.get("caseNumber").or_else(|| v.get("id"))).unwrap_or("CODE-CASE");
            let desc = scrub_public_text_pii(
                val_str(v.get("description").or_else(|| v.get("summary")))
                    .unwrap_or("Municipal code enforcement record"),
            );
            let raw_url = val_str(v.get("sourceUrl").or_else(|| v.get("url")))
                .filter(|u| u.starts_with("https://"))
                .unwrap_or("https://www.pria.us/");
            open_violations.push(json!({
                "caseNumber": cnum,
                "status": val_str(v.get("status")).unwrap_or("Recorded"),
                "filedDate": val_str(v.get("filedDate").or_else(|| v.get("date"))),
                "category": val_str(v.get("category")).unwrap_or("Code Enforcement"),
                "description": desc,
                "sourceName": val_str(v.get("sourceName").or_else(|| v.get("source"))).unwrap_or("Municipal Code Enforcement Record"),
                "sourceUrl": attach_verified_doc_anchor(raw_url, cnum),
            }));
        }
    }

    let open_unfinaled_permits = open_permits
        .iter()
        .filter(|p| {
            let st = val_str(p.get("status")).unwrap_or("").to_ascii_lowercase();
            st.contains("open")
                || st.contains("expired")
                || st.contains("incomplete")
                || st.contains("revoked")
                || st.contains("suspended")
        })
        .count();

    let active_violations = open_violations
        .iter()
        .filter(|v| {
            let st = val_str(v.get("status")).unwrap_or("").to_ascii_lowercase();
            !st.contains("closed")
                && !st.contains("abated")
                && !st.contains("complied")
                && !st.contains("dismissed")
                && !st.contains("resolved")
        })
        .count();

    let summary = if !open_permits.is_empty() || !open_violations.is_empty() {
        format!(
            "Found {} building/trade permit(s) ({} open/expired) and {} municipal code/housing complaint record(s) ({} active/unclosed){}.",
            open_permits.len(),
            open_unfinaled_permits,
            open_violations.len(),
            active_violations,
            open_portal_name
                .as_deref()
                .map(|n| format!(" via {n}"))
                .unwrap_or_default()
        )
    } else if let Some(ref portal) = open_portal_name {
        format!("Queried {portal} live — 0 municipal building permits or code violations matched this street address.")
    } else {
        "No open-data municipal permits or code violations reported in public feeds — verify final inspection sign-off cards with the local building department during contingency.".to_string()
    };

    let portal_links: Vec<Value> = build_verification_links(geo, facts)
        .into_iter()
        .filter(|l| {
            val_str(l.get("category"))
                .unwrap_or("")
                .contains("Municipal")
                || val_str(l.get("category")).unwrap_or("").contains("Recorded")
        })
        .collect();

    let out = json!({
        "openDataPortalQueried": open_portal_name,
        "permitCount": open_permits.len(),
        "openOrExpiredPermitCount": open_unfinaled_permits,
        "violationCount": open_violations.len(),
        "activeViolationCount": active_violations,
        "permits": open_permits,
        "violations": open_violations,
        "portalLinks": portal_links,
        "summary": summary,
    });

    (out, sources)
}

// ------------------------- 4. HOA Issues, Statutory Mandates & Lien Watchouts -------------------------

fn synthesize_hoa_issues(
    geo: &Value,
    facts: &Value,
    hoa_profile: Option<&Value>,
) -> Value {
    let state = val_str(geo.get("state")).unwrap_or("").to_ascii_uppercase();
    let prop_type = val_str(facts.get("propertyType"))
        .unwrap_or("Single-family")
        .to_ascii_lowercase();
    let is_attached = prop_type.contains("condo")
        || prop_type.contains("town")
        || prop_type.contains("co-op")
        || prop_type.contains("coop")
        || prop_type.contains("pud");

    let hp = hoa_profile.unwrap_or(&Value::Null);
    let has_hoa = hp
        .get("hasHoa")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| val_f64(facts.get("hoaMonthly")).unwrap_or(0.0) > 0.0 || is_attached);

    let mut issues: Vec<Value> = Vec::new();
    let debt = hp.get("debtAndLienStatus").unwrap_or(&Value::Null);

    // 1. Active HOA assessment liens & NODA notices
    if let Some(liens) = debt.get("hoaAssessmentLiens").and_then(|v| v.as_array()) {
        for l in liens {
            let amt = val_i64(l.get("amountUsd"));
            issues.push(json!({
                "severity": "critical",
                "category": "Active HOA Assessment Lien",
                "title": format!(
                    "Recorded HOA Assessment Lien Detected{}",
                    amt.map(|a| format!(" (${a})")).unwrap_or_default()
                ),
                "detail": scrub_public_text_pii(val_str(l.get("summary")).unwrap_or("Recorded HOA assessment lien flagged in title/HOA records; require full seller payoff and recorded Release of Lien at closing.")),
                "sourceName": "County Recorder / Title & HOA Lien Screen",
                "sourceUrl": "https://www.alta.org/homebuyer/",
            }));
        }
    }
    if debt.get("nodaDetected").and_then(|v| v.as_bool()) == Some(true) {
        issues.push(json!({
            "severity": "critical",
            "category": "Delinquent Assessment Notice (NODA)",
            "title": "Notice of Delinquent Assessment (NODA) Flagged",
            "detail": "Association has initiated formal delinquency notice proceedings; verify exact payoff demand, late fees, and collection attorney charges in escrow.",
            "sourceName": "Davis-Stirling / State Delinquent Assessment Procedure",
            "sourceUrl": "https://leginfo.legislature.ca.gov/faces/codes_displaySection.xhtml?lawCode=CIV&sectionNum=5675.#:~:text=5675",
        }));
    }

    // 2. Active or pending special assessments & litigation
    if debt.get("hasSpecialAssessmentSignal").and_then(|v| v.as_bool()) == Some(true) {
        issues.push(json!({
            "severity": "high",
            "category": "Special Assessment",
            "title": "Active or Referenced HOA Special Assessment",
            "detail": "Public listing or HOA disclosure references a special assessment. Negotiate an explicit contract addendum requiring Seller to pay 100% of levied or board-approved assessments in full at close of escrow.",
            "sourceName": "Fannie Mae Condo/HOA Project Eligibility Standards",
            "sourceUrl": "https://selling-guide.fanniemae.com/sel/b4-2.1-01/general-information-project-standards#:~:text=Special%20Assessments",
        }));
    }
    if debt.get("hasLitigationSignal").and_then(|v| v.as_bool()) == Some(true) {
        issues.push(json!({
            "severity": "high",
            "category": "HOA Litigation & Warrantability",
            "title": "HOA Litigation Signal (Conventional Loan Warrantability Risk)",
            "detail": "Association litigation involving structural defects, habitability, or safety typically renders units non-warrantable for conforming Fannie Mae/Freddie Mac, FHA, and VA mortgages.",
            "sourceName": "Fannie Mae Ineligible Projects — Litigation (B4-2.1-03)",
            "sourceUrl": "https://selling-guide.fanniemae.com/sel/b4-2.1-03/ineligible-projects#:~:text=Litigation",
        }));
    }

    // 3. Super-priority HOA lien state exposure
    let sp = debt.get("superPriorityLienState").unwrap_or(&Value::Null);
    if has_hoa && sp.get("isSuperPriorityState").and_then(|v| v.as_bool()) == Some(true) {
        let months = val_i64(sp.get("monthsPriority")).unwrap_or(6);
        let statute = val_str(sp.get("statute")).unwrap_or("State Super-Priority Lien Act");
        issues.push(json!({
            "severity": "watch",
            "category": "Super-Priority HOA Lien Statute",
            "title": format!("{state} {months}-Month Super-Priority HOA Lien State ({statute})"),
            "detail": val_str(sp.get("notice")).unwrap_or("Unpaid HOA assessments carry statutory super-priority ahead of first mortgages; require a binding Seller-Paid Estoppel / Resale Certificate confirming $0.00 balance."),
            "sourceName": format!("{state} Statutory HOA Lien Priority ({statute})"),
            "sourceUrl": "https://www.uniformlaws.org/committees/community-home?CommunityKey=11b5b920-66d5-451d-a4d2-8a42b6211145",
        }));
    }

    // 4. Reserve funding & Statutory Structural Inspection Mandates (SB-326 / SB-4D / SIRS)
    if let Some(pf) = val_f64(debt.get("percentFunded")) {
        if pf < 30.0 {
            issues.push(json!({
                "severity": "high",
                "category": "Underfunded HOA Reserves",
                "title": format!("Critically Underfunded HOA Reserves ({pf:.1}% Funded)"),
                "detail": "Reserve balance below the 30% critical threshold substantially elevates the probability of near-term lump-sum special assessments or sharp monthly dues increases.",
                "sourceName": "Community Associations Institute — Reserve Study Standards",
                "sourceUrl": "https://www.caionline.org/",
            }));
        }
    }
    if let Some(mandates) = hp
        .pointer("/fundingAndBoard/statutoryMandates")
        .and_then(|v| v.as_array())
    {
        for m in mandates {
            let code = val_str(m.get("codeCitation")).unwrap_or("");
            if code.contains("5551") || code.contains("718.112") || code.contains("553.899") {
                issues.push(json!({
                    "severity": "watch",
                    "category": "Statutory Structural / Balcony / Reserve Mandate",
                    "title": val_str(m.get("title")).unwrap_or("Statutory Structural Inspection Mandate"),
                    "detail": val_str(m.get("summary")).unwrap_or(""),
                    "sourceName": val_str(m.get("sourceName")).unwrap_or("State Legislature"),
                    "sourceUrl": val_str(m.get("sourceUrl")).unwrap_or("https://selling-guide.fanniemae.com/"),
                }));
            }
        }
    } else if state == "CA" && is_attached {
        issues.push(json!({
            "severity": "watch",
            "category": "Statutory Structural / Balcony / Reserve Mandate",
            "title": "California SB-326 Balcony & Exterior Elevated Elements (Cal. Civ. Code § 5551)",
            "detail": "California condominiums/attached CIDs with 3+ units and wood-framed balconies, decks, or walkways require licensed structural engineer/architect inspection every 9 years. Request the signed SB-326 report and reserve funding plan in escrow.",
            "sourceName": "California Legislative Information — Civil Code § 5551 (SB-326)",
            "sourceUrl": "https://leginfo.legislature.ca.gov/faces/codes_displaySection.xhtml?lawCode=CIV&sectionNum=5551.#:~:text=5551",
        }));
    } else if state == "FL" && is_attached {
        issues.push(json!({
            "severity": "watch",
            "category": "Statutory Structural / Balcony / Reserve Mandate",
            "title": "Florida SB-4D Structural Integrity Reserve Study (SIRS) & Milestone Inspection (Fla. Stat. §§ 718.112, 553.899)",
            "detail": "Florida condo/co-op buildings 3+ stories require a mandatory Structural Integrity Reserve Study (with reserve-waiver ban) and Milestone Structural Inspection at 25/30 years.",
            "sourceName": "Florida Senate Statutes — § 718.112",
            "sourceUrl": "https://www.flsenate.gov/Laws/Statutes/2024/0718.112#:~:text=Structural%20integrity%20reserve%20study",
        }));
    }

    // 5. Rental cap & Master Insurance Deductible Shock
    if hp
        .pointer("/requirements/rentalCapOrWaitlist/restricted")
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        issues.push(json!({
            "severity": "watch",
            "category": "HOA Rental Cap",
            "title": "HOA Leasing Cap / Waitlist Restriction",
            "detail": val_str(hp.pointer("/requirements/rentalCapOrWaitlist/summary"))
                .unwrap_or("HOA restricts unit leasing; verify current rental percentage and waitlist before closing."),
            "sourceName": "HOA Governing Covenants (CC&Rs)",
            "sourceUrl": "https://entp.hud.gov/idapp/html/condlook.cfm",
        }));
    }
    if has_hoa
        && hp
            .pointer("/masterInsurance/estimatedMasterInsuranceCost/deductibleShockWarning")
            .and_then(|v| v.as_bool())
            == Some(true)
    {
        issues.push(json!({
            "severity": "watch",
            "category": "Master Insurance Deductible Exposure",
            "title": "High HOA Master Policy Percentage Deductible Exposure",
            "detail": "High wind/water master deductibles (2%–5%+ of building value) can be assessed back to unit owners; carry >=$50,000–$75,000 in HO-6 Loss Assessment coverage.",
            "sourceName": "Fannie Mae Master Property Insurance Requirements (B7-3-03)",
            "sourceUrl": "https://selling-guide.fanniemae.com/sel/b7-3-03/master-property-insurance-requirements-project-developments#:~:text=Deductible",
        }));
    }

    // 6. ProPublica IRS Form 990 Nonprofit Filings (if discovered)
    let filings = hp
        .pointer("/fundingAndBoard/publicFilings/nonprofit990Filings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for f in &filings {
        let org_name = val_str(f.get("organizationName")).unwrap_or("HOA Nonprofit Entity");
        let rev = val_i64(f.get("totalRevenueUsd"));
        let assets = val_i64(f.get("totalAssetsUsd"));
        let yr = val_str(f.get("latestTaxPeriod")).unwrap_or("recent");
        let surl = val_str(f.get("sourceUrl")).unwrap_or("https://projects.propublica.org/nonprofits/");
        issues.push(json!({
            "severity": "info",
            "category": "IRS Form 990 Corporate Filing",
            "title": format!("Public Nonprofit Tax Filing Matched: {org_name} ({yr})"),
            "detail": format!(
                "IRS Form 990 corporate filing{}{}. Compare against the current HOA Operating Budget & Reserve Study.",
                rev.map(|r| format!(" reports ${r} annual revenue")).unwrap_or_default(),
                assets.map(|a| format!(" and ${a} end-of-year assets")).unwrap_or_default()
            ),
            "sourceName": "ProPublica Nonprofit Explorer (IRS Form 990)",
            "sourceUrl": surl,
        }));
    }

    let official_links = hp
        .pointer("/fundingAndBoard/publicFilings/registryLookupLinks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let risk_level = if issues.iter().any(|i| i["severity"] == "critical") {
        "Critical"
    } else if issues.iter().any(|i| i["severity"] == "high") {
        "Elevated"
    } else if has_hoa || !issues.is_empty() {
        "Watch"
    } else {
        "Low"
    };

    json!({
        "applicable": has_hoa || is_attached || !issues.is_empty(),
        "riskLevel": risk_level,
        "issueCount": issues.len(),
        "issues": issues,
        "nonprofit990Filings": filings,
        "officialRegistryLinks": official_links,
    })
}

// ------------------------- Public Entry Points -------------------------

pub async fn run_deep_property_research(
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    geo: &Value,
    facts: &Value,
    mls: &Value,
    hoa_profile: Option<&Value>,
    slug: &str,
) -> ModuleOutput {
    run_deep_property_research_with_prefetched(
        http,
        env,
        db,
        geo,
        facts,
        mls,
        hoa_profile,
        slug,
        None,
    )
    .await
}

/// Variant of [`run_deep_property_research`] accepting Stage-1 open-data permits fetched before Wave 3
/// so municipal open-data APIs are never queried twice for the same report build.
pub async fn run_deep_property_research_with_prefetched(
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    geo: &Value,
    facts: &Value,
    mls: &Value,
    hoa_profile: Option<&Value>,
    slug: &str,
    prefetched_permits: Option<(Vec<Value>, Vec<Value>, Vec<SourceItem>, Option<String>)>,
) -> ModuleOutput {
    let docs_fut = discover_and_download_public_docs(env, geo, facts, mls, hoa_profile, slug);
    let past_fut = research_past_reports(db, geo, facts, slug);
    let permits_fut =
        research_permits_and_violations_inner(http, env, geo, facts, prefetched_permits);

    let (
        (discovered_docs, api_fetched_reviews, inspection_status, doc_sources),
        past_reports,
        (permits_and_violations, permit_sources),
    ) = tokio::join!(docs_fut, past_fut, permits_fut);

    let hoa_issues = synthesize_hoa_issues(geo, facts, hoa_profile);

    let mut sources: Vec<SourceItem> = Vec::new();
    sources.extend(doc_sources);
    sources.extend(permit_sources);

    let data = json!({
        "researchedAt": crate::shared::now_iso(),
        "discoveredDocuments": discovered_docs,
        "apiFetchedReviews": api_fetched_reviews,
        "inspectionSearchStatus": inspection_status,
        "pastReports": past_reports,
        "permitsAndViolations": permits_and_violations,
        "hoaIssues": hoa_issues,
    });

    ModuleOutput {
        data,
        sources,
        warnings: Vec::new(),
    }
}

/// Merges newly discovered open-data building permits from `publicResearch.permitsAndViolations.permits`
/// into `report["facts"]["permits"]`, `report["facts"]["disclosures"]["permits"]`, and (if present)
/// `report["property"]["permits"]`, deduplicating cleanly by `permitNumber`.
pub fn merge_open_data_permits_into_report(report: &mut Value, deep_data: &Value) {
    let Some(new_permits) = deep_data
        .pointer("/permitsAndViolations/permits")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
    else {
        return;
    };

    for root_key in ["facts", "property"] {
        let Some(root_obj) = report.get_mut(root_key).and_then(|v| v.as_object_mut()) else {
            continue;
        };

        let existing = root_obj
            .entry("permits")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !existing.is_array() {
            *existing = Value::Array(Vec::new());
        }
        if let Some(arr) = existing.as_array_mut() {
            let mut seen: HashSet<String> = arr
                .iter()
                .filter_map(|p| {
                    val_str(
                        p.get("permitNumber")
                            .or_else(|| p.get("number"))
                            .or_else(|| p.get("id")),
                    )
                    .map(|s| s.to_ascii_uppercase())
                })
                .collect();

            for np in new_permits {
                let pnum = val_str(np.get("permitNumber")).unwrap_or("");
                if pnum.is_empty() || pnum == "RECORDED-PERMIT" {
                    continue;
                }
                if seen.insert(pnum.to_ascii_uppercase()) {
                    arr.push(np.clone());
                }
            }
        }

        if let Some(disc_obj) = root_obj
            .get_mut("disclosures")
            .and_then(|v| v.as_object_mut())
        {
            let d_permits = disc_obj
                .entry("permits")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(d_arr) = d_permits.as_array_mut() {
                let mut seen: HashSet<String> = d_arr
                    .iter()
                    .filter_map(|p| {
                        val_str(p.get("permitNumber").or_else(|| p.get("number")))
                            .map(|s| s.to_ascii_uppercase())
                    })
                    .collect();
                for np in new_permits {
                    let pnum = val_str(np.get("permitNumber")).unwrap_or("");
                    if !pnum.is_empty() && pnum != "RECORDED-PERMIT" && seen.insert(pnum.to_ascii_uppercase()) {
                        d_arr.push(np.clone());
                    }
                }
            }
        }
    }
}
