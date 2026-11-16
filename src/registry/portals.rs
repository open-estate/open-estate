//! Shared portal domain lists and deduplicated official-portal card builders.

use serde_json::{json, Value};

/// Portal / brokerage domains excluded from user-facing gallery chips, map links, and inline bars.
pub const COMPETITOR_PORTAL_DOMAINS: &[&str] = &[
    "zillow.com",
    "zillowstatic.com",
    "redfin.com",
    "cdn-redfin.com",
    "realtor.com",
    "rdcpix.com",
    "trulia.com",
    "homes.com",
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
];

/// Appends an official portal entry `{ category, name, url, hint }` only if `url` is non-empty
/// and not already present (ignoring `#fragment` differences so anchored and unanchored duplicates never double-push).
pub fn push_unique_official_portal(
    portals: &mut Vec<Value>,
    category: &str,
    name: impl Into<String>,
    url: impl Into<String>,
    hint: impl Into<String>,
) {
    let u = url.into();
    let trimmed = u.trim();
    if trimmed.is_empty() {
        return;
    }
    let base_new = trimmed.split('#').next().unwrap_or(trimmed);
    let already = portals.iter().any(|p| {
        p.get("url")
            .and_then(|v| v.as_str())
            .is_some_and(|existing| existing.split('#').next().unwrap_or(existing) == base_new)
    });
    if !already {
        portals.push(json!({
            "category": category,
            "name": name.into(),
            "url": trimmed,
            "hint": hint.into(),
        }));
    }
}
