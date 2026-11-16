//! Format-agnostic proxy extraction (plain text, JSON, HTML) and source-registry parsing.
//!
//! No site-specific adapters: any document that keeps `ip:port` together (optionally with a
//! `scheme://` and `user:pass@`) is understood, JSON objects with separate `ip`/`port` fields are
//! walked generically, and HTML tables are read by header names (`IP`, `Port`, `Protocol`, `Code`).

use super::{infer_protocol_from_url, ProxyProtocol};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};

/// One proxy source: a URL plus the protocol assumed for entries without a scheme.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpec {
    pub url: String,
    pub protocol: ProxyProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtractedProxy {
    pub protocol: ProxyProtocol,
    pub ip: IpAddr,
    pub port: u16,
    pub auth: Option<(String, String)>,
    pub country: Option<[u8; 2]>,
}

static PROXY_TOKEN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:(https?|socks4a?|socks5h?)://)?(?:([A-Za-z0-9._~%!$&'*+=-]{1,64}):([^\s:@/]{1,64})@)?((?:\d{1,3}\.){3}\d{1,3}):(\d{1,5})\b",
    )
    .expect("proxy token regex")
});
static HTML_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>").expect("tag regex"));
static HTML_CELL_BREAK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)</t[dh]\s*>|<br\s*/?>|</tr\s*>|</li\s*>|</p\s*>|</div\s*>").expect("cell regex")
});
static HTML_ROW_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<tr[^>]*>(.*?)</tr\s*>").expect("row regex"));
static HTML_CELL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<t([hd])\b[^>]*>(.*?)</t[hd]\s*>").expect("td regex"));

fn country_code(raw: &str) -> Option<[u8; 2]> {
    let b = raw.trim().as_bytes();
    (b.len() == 2 && b.iter().all(u8::is_ascii_alphabetic))
        .then(|| [b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
}

fn valid_port(raw: &str) -> Option<u16> {
    raw.parse::<u16>().ok().filter(|p| *p > 0)
}

/// Extracts every proxy found in `body`; entries without a scheme get `default_protocol`.
/// Results are de-duplicated (first occurrence wins, later ones only fill a missing country).
pub fn extract_proxies(body: &str, default_protocol: ProxyProtocol) -> Vec<ExtractedProxy> {
    let mut out: Vec<ExtractedProxy> = Vec::new();
    let trimmed = body.trim_start();
    let first = trimmed.as_bytes().first().copied();

    if matches!(first, Some(b'{') | Some(b'[')) {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            walk_json(&v, default_protocol, &mut out, 0);
            return dedupe(out);
        }
    }

    let head: String = trimmed.chars().take(4096).collect::<String>().to_ascii_lowercase();
    let looks_html = head.starts_with('<')
        || head.contains("<html")
        || head.contains("<table")
        || head.contains("<tr")
        || body.contains("<td");
    if looks_html {
        // Tables carry the protocol/country columns; fall back to plain-text matching (tags
        // stripped, cells split) only for pages without a recognisable proxy table.
        let before = out.len();
        extract_html_tables(body, default_protocol, &mut out);
        if out.len() == before {
            let broken = HTML_CELL_BREAK_RE.replace_all(body, "\n");
            let text = HTML_TAG_RE.replace_all(&broken, "");
            extract_text(&text, default_protocol, None, &mut out);
        }
    } else {
        extract_text(body, default_protocol, None, &mut out);
    }
    dedupe(out)
}

fn dedupe(items: Vec<ExtractedProxy>) -> Vec<ExtractedProxy> {
    let mut seen: HashSet<(ProxyProtocol, IpAddr, u16)> = HashSet::with_capacity(items.len());
    let mut out: Vec<ExtractedProxy> = Vec::with_capacity(items.len());
    for it in items {
        if seen.insert((it.protocol, it.ip, it.port)) {
            out.push(it);
        } else if it.country.is_some() {
            if let Some(prev) = out
                .iter_mut()
                .find(|p| p.protocol == it.protocol && p.ip == it.ip && p.port == it.port)
            {
                if prev.country.is_none() {
                    prev.country = it.country;
                }
            }
        }
    }
    out
}

fn extract_text(
    text: &str,
    default_protocol: ProxyProtocol,
    country: Option<[u8; 2]>,
    out: &mut Vec<ExtractedProxy>,
) {
    let bytes = text.as_bytes();
    for caps in PROXY_TOKEN_RE.captures_iter(text) {
        let whole = caps.get(0).expect("match");
        // `1.2.3.4.5:80` or `v1.2.3.4:80` are not addresses.
        if whole.start() > 0 {
            let prev = bytes[whole.start() - 1];
            if prev == b'.' || prev.is_ascii_alphanumeric() {
                continue;
            }
        }
        let Some(ip) = caps.get(4).and_then(|m| m.as_str().parse::<Ipv4Addr>().ok()) else {
            continue;
        };
        let Some(port) = caps.get(5).and_then(|m| valid_port(m.as_str())) else {
            continue;
        };
        let protocol = caps
            .get(1)
            .and_then(|m| ProxyProtocol::from_list_label(m.as_str()))
            .unwrap_or(default_protocol);
        let auth = match (caps.get(2), caps.get(3)) {
            (Some(u), Some(p)) => Some((
                urlencoding::decode(u.as_str())
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| u.as_str().to_string()),
                urlencoding::decode(p.as_str())
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| p.as_str().to_string()),
            )),
            _ => None,
        };
        out.push(ExtractedProxy {
            protocol,
            ip: IpAddr::V4(ip),
            port,
            auth,
            country,
        });
    }
}

const IP_KEYS: &[&str] = &["ip", "host", "address", "ipAddress", "ip_address", "addr", "server"];
const COUNTRY_KEYS: &[&str] = &["countryCode", "country_code", "country", "cc"];

fn json_country(obj: &serde_json::Map<String, Value>) -> Option<[u8; 2]> {
    for k in COUNTRY_KEYS {
        if let Some(cc) = obj.get(*k).and_then(Value::as_str).and_then(country_code) {
            return Some(cc);
        }
    }
    // One level of nesting covers `ip_data`, `geolocation`, `location` style payloads.
    for v in obj.values() {
        if let Some(inner) = v.as_object() {
            for k in ["countryCode", "country_code", "country"] {
                if let Some(cc) = inner.get(k).and_then(Value::as_str).and_then(country_code) {
                    return Some(cc);
                }
            }
        }
    }
    None
}

fn json_port(v: &Value) -> Option<u16> {
    v.as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| valid_port(s.trim())))
        .filter(|p| *p > 0)
}

fn json_protocols(obj: &serde_json::Map<String, Value>) -> Vec<ProxyProtocol> {
    let mut protos = Vec::new();
    for k in ["protocols", "protocol", "type", "types"] {
        match obj.get(k) {
            Some(Value::String(s)) => {
                for part in s.split([',', '/', ' ']) {
                    if let Some(p) = ProxyProtocol::from_list_label(part) {
                        protos.push(p);
                    }
                }
            }
            Some(Value::Array(arr)) => {
                protos.extend(
                    arr.iter()
                        .filter_map(Value::as_str)
                        .filter_map(ProxyProtocol::from_list_label),
                );
            }
            _ => {}
        }
        if !protos.is_empty() {
            break;
        }
    }
    protos.sort();
    protos.dedup();
    protos
}

fn walk_json(v: &Value, default_protocol: ProxyProtocol, out: &mut Vec<ExtractedProxy>, depth: u8) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Object(obj) => {
            let country = json_country(obj);
            let before = out.len();
            for k in ["proxy", "url", "uri"] {
                if let Some(s) = obj.get(k).and_then(Value::as_str) {
                    extract_text(s, default_protocol, country, out);
                }
            }
            if out.len() == before {
                let ip = IP_KEYS
                    .iter()
                    .find_map(|k| obj.get(*k).and_then(Value::as_str))
                    .and_then(|s| s.trim().parse::<IpAddr>().ok());
                let port = obj.get("port").and_then(json_port);
                if let (Some(ip), Some(port)) = (ip, port) {
                    let protos = json_protocols(obj);
                    let protos = if protos.is_empty() { vec![default_protocol] } else { protos };
                    for protocol in protos {
                        out.push(ExtractedProxy { protocol, ip, port, auth: None, country });
                    }
                    return;
                }
            }
            for child in obj.values() {
                if child.is_object() || child.is_array() {
                    walk_json(child, default_protocol, out, depth + 1);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                match child {
                    Value::String(s) => extract_text(s, default_protocol, None, out),
                    Value::Object(_) | Value::Array(_) => {
                        walk_json(child, default_protocol, out, depth + 1)
                    }
                    _ => {}
                }
            }
        }
        Value::String(s) => extract_text(s, default_protocol, None, out),
        _ => {}
    }
}

fn clean_cell(raw: &str) -> String {
    HTML_TAG_RE
        .replace_all(raw, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Header-aware HTML table reader (`IP Address | Port | Code | ... | Https` and `IP:Port` layouts).
fn extract_html_tables(html: &str, default_protocol: ProxyProtocol, out: &mut Vec<ExtractedProxy>) {
    let mut headers: Vec<String> = Vec::new();
    for row in HTML_ROW_RE.captures_iter(html) {
        let mut cells = Vec::new();
        let (mut has_th, mut has_td) = (false, false);
        for cell in HTML_CELL_RE.captures_iter(&row[1]) {
            if cell[1].eq_ignore_ascii_case("h") {
                has_th = true;
            } else {
                has_td = true;
            }
            cells.push(clean_cell(&cell[2]));
        }
        if cells.is_empty() {
            continue;
        }
        if has_th && !has_td {
            headers = cells.iter().map(|h| h.to_ascii_lowercase()).collect();
            continue;
        }
        if headers.is_empty() {
            continue;
        }
        let col = |pred: &dyn Fn(&str) -> bool| headers.iter().position(|h| pred(h));
        let ip_col = col(&|h| h.starts_with("ip") || h == "host" || h.contains("address"));
        let port_col = col(&|h| h == "port");
        let proto_col = col(&|h| h.contains("protocol") || h == "type");
        let cc_col = col(&|h| h == "code" || h == "country code" || h == "cc");

        let Some(ip_i) = ip_col else {
            continue;
        };
        let Some(ip_cell) = cells.get(ip_i) else {
            continue;
        };
        let protocol = proto_col
            .and_then(|i| cells.get(i))
            .and_then(|p| ProxyProtocol::from_list_label(p))
            .unwrap_or(default_protocol);
        let country = cc_col.and_then(|i| cells.get(i)).and_then(|c| country_code(c));

        let joined = match port_col.and_then(|i| cells.get(i)) {
            Some(port) if !ip_cell.contains(':') => format!("{ip_cell}:{port}"),
            _ => ip_cell.clone(),
        };
        extract_text(&joined, protocol, country, out);
    }
}

// ------------------------------------------------------------------------------------------------
// Source registries
// ------------------------------------------------------------------------------------------------

fn section_protocol(section: &str) -> Option<ProxyProtocol> {
    let last = section.rsplit('.').next().unwrap_or(section).trim();
    ProxyProtocol::from_list_label(last)
}

fn push_source(out: &mut Vec<SourceSpec>, url: &str, protocol: ProxyProtocol) {
    let u = url.trim();
    if (u.starts_with("http://") || u.starts_with("https://")) && url::Url::parse(u).is_ok() {
        out.push(SourceSpec { url: u.to_string(), protocol });
    }
}

static TOML_URL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""(https?://[^"\s]+)""#).expect("toml url regex"));

/// Parses a registry of proxy sources. Understood shapes:
/// * TOML with `[scraping.http]` / `[scraping.socks4]` / `[scraping.socks5]` sections holding
///   `urls = [ "...", { url = "..." } ]`;
///   sections with `enabled = false` and entries needing credentials/headers are skipped;
/// * JSON: `["url", ...]`, `[{"url": .., "protocol": ..}]` or `{"socks5": ["url", ...]}`;
/// * plain text: one URL per line, optionally prefixed by a protocol (`socks5 https://...`).
pub fn parse_registry(body: &str) -> Vec<SourceSpec> {
    let trimmed = body.trim_start();
    let mut out = Vec::new();
    if matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'[')) {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            parse_registry_json(&v, None, &mut out);
            return dedupe_sources(out);
        }
    }
    if trimmed.lines().any(|l| {
        let t = l.trim();
        t.starts_with('[') && t.ends_with(']') && !t.contains("://")
    }) {
        parse_registry_toml(trimmed, &mut out);
    } else {
        for line in trimmed.lines() {
            let t = line.split('#').next().unwrap_or("").trim();
            if t.is_empty() {
                continue;
            }
            let mut parts = t.split_whitespace();
            let first = parts.next().unwrap_or("");
            match (ProxyProtocol::from_list_label(first), parts.next()) {
                (Some(p), Some(url)) => push_source(&mut out, url, p),
                _ => push_source(&mut out, first, infer_protocol_from_url(first)),
            }
        }
    }
    dedupe_sources(out)
}

fn dedupe_sources(items: Vec<SourceSpec>) -> Vec<SourceSpec> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert((s.url.clone(), s.protocol)))
        .collect()
}

fn parse_registry_json(v: &Value, proto_hint: Option<ProxyProtocol>, out: &mut Vec<SourceSpec>) {
    match v {
        Value::String(s) => {
            push_source(out, s, proto_hint.unwrap_or_else(|| infer_protocol_from_url(s)))
        }
        Value::Array(arr) => {
            for item in arr {
                parse_registry_json(item, proto_hint, out);
            }
        }
        Value::Object(obj) => {
            if obj.get("enabled").and_then(Value::as_bool) == Some(false) {
                return;
            }
            if let Some(url) = obj.get("url").and_then(Value::as_str) {
                if obj.contains_key("headers") || obj.contains_key("basic_auth") {
                    return;
                }
                let proto = ["protocol", "type"]
                    .iter()
                    .find_map(|k| obj.get(*k).and_then(Value::as_str))
                    .and_then(ProxyProtocol::from_list_label)
                    .or(proto_hint)
                    .unwrap_or_else(|| infer_protocol_from_url(url));
                push_source(out, url, proto);
                return;
            }
            for (k, child) in obj {
                let key_proto = ProxyProtocol::from_list_label(k);
                let listy = matches!(k.as_str(), "urls" | "sources" | "lists" | "proxies");
                // Plain strings only count under protocol/list keys, so metadata such as
                // `"homepage": "https://..."` is never mistaken for a proxy source.
                if child.is_string() && key_proto.is_none() && !listy {
                    continue;
                }
                parse_registry_json(child, key_proto.or(proto_hint), out);
            }
        }
        _ => {}
    }
}

fn parse_registry_toml(body: &str, out: &mut Vec<SourceSpec>) {
    // Collect each section's lines first so `enabled = false` anywhere in a section applies.
    let mut sections: Vec<(String, Vec<&str>)> = vec![(String::new(), Vec::new())];
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') && !t.starts_with("[[") && !t.contains('"') {
            sections.push((t.trim_matches(['[', ']']).trim().to_string(), Vec::new()));
        } else if let Some(last) = sections.last_mut() {
            last.1.push(line);
        }
    }
    for (name, lines) in sections {
        let Some(proto) = section_protocol(&name) else {
            continue;
        };
        let disabled = lines.iter().any(|l| {
            let t = l.trim();
            !t.starts_with('#')
                && t.replace(' ', "").to_ascii_lowercase().starts_with("enabled=false")
        });
        if disabled {
            continue;
        }
        for line in lines {
            let t = line.trim();
            if t.starts_with('#') {
                continue;
            }
            let code = strip_toml_comment(t);
            if code.contains("basic_auth") || code.contains("headers") {
                continue;
            }
            for caps in TOML_URL_RE.captures_iter(code) {
                push_source(out, &caps[1], proto);
            }
        }
    }
}

/// Drops a trailing `# comment` that is outside of double quotes.
fn strip_toml_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}
