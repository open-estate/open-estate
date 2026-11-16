// Local Buyer's Agent & Licensed Home Inspector Recommendation Engine + Match Request Handler.
// Used when inspection reports, seller disclosures, prelim title, or HOA packets cannot be fetched
// automatically via public/MLS APIs (e.g., broker-gated portals like Disclosures.io or Glide).
//
// Storage & Compliance Invariants:
// - Database-backed (`recommended_agents` table): starts completely empty (0 seeded rows), so
//   `recommendedAgents` and `recommendedInspectors` are `[]` until licensed specialists are added to the DB.
// - RESPA Section 8 (12 U.S.C. § 2607): Zero settlement-contingent referral kickbacks; any sponsored
//   placements are flat-fee sponsorship only, and buyers are free to choose any licensed agent or inspector.
// - Fair Housing Act: Recommendations filter strictly on geography (state/ZIP/city) and property price tier.
// - Unlicensed rows are skipped with a warning.

use crate::db::{AgentMatchRequestRow, DbStore, RecommendedAgentRow};
use crate::guardrails::GuardrailError;
use crate::search::{send_smtp_email, validate_email_input, EmailRequest};
use crate::shared::{now_iso, slugify, EnvConfig};
use serde_json::{json, Value};
use uuid::Uuid;

pub const AGENT_MATCH_COMPENSATION_NOTICE: &str =
    "Flat-fee sponsorship only; zero settlement-contingent referral fees (RESPA Section 8 compliant). Buyers are free to choose any licensed real estate agent or home inspector.";

pub const AGENT_MATCH_REQUEST_ALLOWED_KEYS: &[&str] = &[
    "slug",
    "address",
    "needType",
    "need_type",
    "docNeeded",
    "doc_needed",
    "name",
    "buyerName",
    "buyer_name",
    "email",
    "buyerEmail",
    "buyer_email",
    "phone",
    "buyerPhone",
    "buyer_phone",
    "preferredPartnerId",
    "preferred_partner_id",
    "notes",
];

fn classify_specialist_kind(raw_role: &str) -> Option<&'static str> {
    let v = raw_role.trim().to_ascii_lowercase();
    match v.as_str() {
        "agent" | "agents" | "buyer_agent" | "buyers_agent" | "buyeragent" | "realtor"
        | "broker" => Some("buyer_agent"),
        "inspector" | "inspectors" | "home_inspector" | "homeinspector" | "wdo_inspector" => {
            Some("home_inspector")
        }
        _ => None,
    }
}

fn sanitize_http_url(raw: Option<&str>) -> Option<String> {
    let u = raw?.trim();
    if u.starts_with("https://") || u.starts_with("http://") {
        Some(u.chars().take(512).collect())
    } else {
        None
    }
}

fn clean_plain_text(raw: Option<&str>, max_chars: usize) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(s.len().min(max_chars));
    let mut in_tag = false;
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            continue;
        }
        if in_tag || (ch.is_control() && ch != '\n' && ch != ' ') {
            continue;
        }
        out.push(ch);
        if out.chars().count() >= max_chars {
            break;
        }
    }
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn parse_json_str_list(raw: Option<&str>) -> Vec<String> {
    let Some(s) = raw.map(str::trim).filter(|x| !x.is_empty()) else {
        return Vec::new();
    };
    if let Ok(arr) = serde_json::from_str::<Vec<String>>(s) {
        return arr
            .into_iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
    }
    s.split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn extract_subject_price(report: &Value) -> Option<f64> {
    for ptr in [
        "/facts/listPrice",
        "/facts/price",
        "/valuation/estimate/value",
        "/modules/valuation/data/estimate/value",
        "/facts/lastSale/price",
    ] {
        if let Some(n) = report.pointer(ptr).and_then(|v| v.as_f64()) {
            if n.is_finite() && n > 10_000.0 {
                return Some(n);
            }
        }
    }
    None
}

fn row_matches_geo(
    row: &RecommendedAgentRow,
    state: &str,
    zip: &str,
    city: &str,
    price_opt: Option<f64>,
) -> bool {
    let states = parse_json_str_list(row.states_json.as_deref());
    if !states.is_empty()
        && !state.is_empty()
        && !states.iter().any(|s| s.eq_ignore_ascii_case(state))
    {
        return false;
    }
    let zips = parse_json_str_list(row.zips_json.as_deref());
    if !zips.is_empty() && !zip.is_empty() && !zips.iter().any(|z| z == zip) {
        return false;
    }
    let cities = parse_json_str_list(row.cities_json.as_deref());
    if !cities.is_empty()
        && !city.is_empty()
        && !cities.iter().any(|c| c.eq_ignore_ascii_case(city))
    {
        return false;
    }
    if let Some(p) = price_opt {
        if let Some(min_p) = row.min_price {
            if p < min_p as f64 {
                return false;
            }
        }
        if let Some(max_p) = row.max_price {
            if p > max_p as f64 {
                return false;
            }
        }
    }
    true
}

/// Builds `report["agentRecommendations"]` strictly from the `recommended_agents` database table.
/// Because no agents are seeded into the database by default, `recommendedAgents` and
/// `recommendedInspectors` are `[]` (`isEmpty: true`) until licensed rows are inserted into `recommended_agents`.
pub async fn build_agent_recommendations(db: &DbStore, report: &Value) -> Value {
    let mut warnings: Vec<String> = Vec::new();
    let db_rows = match db.list_active_recommended_agents().await {
        Ok(rows) => rows,
        Err(e) => {
            warnings.push(format!("Could not query recommended_agents table: {e}"));
            Vec::new()
        }
    };

    let state = report
        .pointer("/geo/state")
        .or_else(|| report.pointer("/facts/state"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let zip = report
        .pointer("/geo/zip")
        .or_else(|| report.pointer("/facts/zip"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let city = report
        .pointer("/geo/city")
        .or_else(|| report.pointer("/geo/place"))
        .or_else(|| report.pointer("/facts/city"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let price_opt = extract_subject_price(report);

    let mut agents: Vec<Value> = Vec::new();
    let mut inspectors: Vec<Value> = Vec::new();

    for (idx, row) in db_rows.iter().enumerate() {
        let Some(kind) = classify_specialist_kind(&row.role) else {
            continue;
        };
        let Some(name) = clean_plain_text(Some(&row.full_name), 140) else {
            continue;
        };
        let Some(lic) = clean_plain_text(Some(&row.license_number), 80) else {
            warnings.push(format!(
                "Skipped unlicensed {kind} row '{name}' (license_number required)."
            ));
            continue;
        };
        if !row_matches_geo(row, state, zip, city, price_opt) {
            continue;
        }
        let pid = clean_plain_text(Some(&row.id), 80)
            .unwrap_or_else(|| format!("{kind}-{}", idx + 1));
        let specialties: Vec<String> = parse_json_str_list(row.specialties_json.as_deref())
            .into_iter()
            .filter_map(|s| clean_plain_text(Some(&s), 64))
            .take(5)
            .collect();
        let languages: Vec<String> = parse_json_str_list(row.languages_json.as_deref())
            .into_iter()
            .filter_map(|s| clean_plain_text(Some(&s), 32))
            .take(4)
            .collect();

        let card = json!({
            "partnerId": pid,
            "role": kind,
            "name": name,
            "firmName": clean_plain_text(row.firm_name.as_deref(), 160),
            "licenseNumber": lic,
            "phone": clean_plain_text(row.phone.as_deref(), 40),
            "email": clean_plain_text(row.email.as_deref(), 200),
            "ctaUrl": sanitize_http_url(row.website_url.as_deref()),
            "specialties": specialties,
            "languages": languages,
            "notes": clean_plain_text(row.notes.as_deref(), 260),
            "isSponsored": row.is_sponsored,
            "badgeLabel": if row.is_sponsored { "Sponsored Partner" } else { "Verified License" },
            "disclosureText": AGENT_MATCH_COMPENSATION_NOTICE,
        });
        if kind == "buyer_agent" && agents.len() < 6 {
            agents.push(card);
        } else if kind == "home_inspector" && inspectors.len() < 6 {
            inspectors.push(card);
        }
    }

    let listing_office = report
        .pointer("/facts/mlsListing/officeName")
        .or_else(|| report.pointer("/modules/mls/data/mlsListing/officeName"))
        .and_then(Value::as_str)
        .and_then(|s| clean_plain_text(Some(s), 140));

    let is_empty = agents.is_empty() && inspectors.is_empty();

    json!({
        "city": if city.is_empty() { Value::Null } else { json!(city) },
        "state": if state.is_empty() { Value::Null } else { json!(state) },
        "zip": if zip.is_empty() { Value::Null } else { json!(zip) },
        "listingBrokerageContext": {
            "listingOfficeName": listing_office,
            "guidance": "In US residential sales, inspection & seller-disclosure packets (Disclosures.io, Glide, MLS supplements) are typically shared broker-to-broker. A buyer's agent can pull the full packet from the listing brokerage on your behalf — or help schedule an independent home, termite/WDO & sewer-scope inspection if none exists yet."
        },
        "recommendedAgents": agents,
        "recommendedInspectors": inspectors,
        "isEmpty": is_empty,
        "canRequestMatch": true,
        "requestEndpoint": "/api/agent-match/request",
        "complianceNotice": AGENT_MATCH_COMPENSATION_NOTICE,
        "warnings": warnings,
    })
}

fn resolve_notify_recipient(env: &EnvConfig) -> Option<String> {
    for key in ["AGENT_MATCH_NOTIFY_EMAIL", "SMTP_REPLY_TO", "SMTP_USER"] {
        if let Some(raw) = env.get(key) {
            if let Ok(clean) = validate_email_input(raw) {
                return Some(clean);
            }
        }
    }
    if let Some(from_raw) = env.get("SMTP_FROM") {
        let extracted = if let (Some(lt), Some(gt)) = (from_raw.find('<'), from_raw.find('>')) {
            if lt + 1 < gt {
                &from_raw[lt + 1..gt]
            } else {
                from_raw
            }
        } else {
            from_raw
        };
        if let Ok(clean) = validate_email_input(extracted) {
            return Some(clean);
        }
    }
    None
}

/// Validates, persists, and optionally dispatches an SMTP notification for `POST /api/agent-match/request`.
pub async fn submit_agent_match_request(
    db: &DbStore,
    env: &EnvConfig,
    report_opt: Option<&Value>,
    slug_fallback: Option<&str>,
    body: &Value,
    client_id: Option<String>,
) -> std::result::Result<Value, GuardrailError> {
    let raw_email = body
        .get("email")
        .or_else(|| body.get("buyerEmail"))
        .or_else(|| body.get("buyer_email"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let buyer_email = validate_email_input(raw_email)
        .map_err(|e| GuardrailError::new("INVALID_EMAIL", e.message, 400))?;

    let raw_need = body
        .get("needType")
        .or_else(|| body.get("need_type"))
        .and_then(Value::as_str)
        .unwrap_or("buyer_agent")
        .trim()
        .to_ascii_lowercase();
    let need_type = match raw_need.as_str() {
        "buyer_agent" | "agent" => "buyer_agent",
        "home_inspector" | "inspector" => "home_inspector",
        "both" | "agent_and_inspector" => "both",
        _ => {
            return Err(GuardrailError::new(
                "INVALID_NEED_TYPE",
                "needType must be 'buyer_agent', 'home_inspector', or 'both'.",
                400,
            ));
        }
    };

    let doc_needed = clean_plain_text(
        body.get("docNeeded")
            .or_else(|| body.get("doc_needed"))
            .and_then(Value::as_str),
        64,
    );
    let buyer_name = clean_plain_text(
        body.get("buyerName")
            .or_else(|| body.get("buyer_name"))
            .or_else(|| body.get("name"))
            .and_then(Value::as_str),
        120,
    );
    let buyer_phone = clean_plain_text(
        body.get("buyerPhone")
            .or_else(|| body.get("buyer_phone"))
            .or_else(|| body.get("phone"))
            .and_then(Value::as_str),
        40,
    );
    let preferred_partner_id = clean_plain_text(
        body.get("preferredPartnerId")
            .or_else(|| body.get("preferred_partner_id"))
            .and_then(Value::as_str),
        80,
    );
    let notes = clean_plain_text(body.get("notes").and_then(Value::as_str), 1_000);

    let display_address = report_opt
        .and_then(|r| {
            r.pointer("/meta/displayAddress")
                .or_else(|| r.pointer("/geo/displayAddress"))
                .and_then(Value::as_str)
        })
        .and_then(|s| clean_plain_text(Some(s), 240))
        .or_else(|| clean_plain_text(body.get("address").and_then(Value::as_str), 240))
        .unwrap_or_else(|| "Subject Property".to_string());

    let resolved_slug = report_opt
        .and_then(|r| r.pointer("/meta/slug").and_then(Value::as_str))
        .or(slug_fallback)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(180).collect::<String>())
        .unwrap_or_else(|| {
            let s = slugify(&display_address);
            if s.is_empty() {
                "subject-property".to_string()
            } else {
                s
            }
        });

    let city = report_opt
        .and_then(|r| {
            r.pointer("/geo/city")
                .or_else(|| r.pointer("/geo/place"))
                .or_else(|| r.pointer("/facts/city"))
                .and_then(Value::as_str)
        })
        .and_then(|s| clean_plain_text(Some(s), 100));
    let state = report_opt
        .and_then(|r| {
            r.pointer("/geo/state")
                .or_else(|| r.pointer("/facts/state"))
                .and_then(Value::as_str)
        })
        .and_then(|s| clean_plain_text(Some(s), 16));
    let zip = report_opt
        .and_then(|r| {
            r.pointer("/geo/zip")
                .or_else(|| r.pointer("/facts/zip"))
                .and_then(Value::as_str)
        })
        .and_then(|s| clean_plain_text(Some(s), 10));

    let req_id = format!("amr_{}", Uuid::new_v4().simple());
    let created_at = now_iso();

    let mut row = AgentMatchRequestRow {
        id: req_id.clone(),
        slug: resolved_slug.clone(),
        display_address: display_address.clone(),
        city: city.clone(),
        state: state.clone(),
        zip: zip.clone(),
        need_type: need_type.to_string(),
        doc_needed: doc_needed.clone(),
        buyer_name: buyer_name.clone(),
        buyer_email: buyer_email.clone(),
        buyer_phone: buyer_phone.clone(),
        preferred_partner_id: preferred_partner_id.clone(),
        notes: notes.clone(),
        client_id,
        smtp_status: "skipped".to_string(),
        created_at: created_at.clone(),
    };

    db.upsert_agent_match_request(&row).await.map_err(|_| {
        GuardrailError::new(
            "STORAGE_ERROR",
            "Could not save agent recommendation request.",
            500,
        )
    })?;

    // Dispatch optional SMTP notification when SMTP_HOST is configured
    let smtp_host_configured = env
        .get("SMTP_HOST")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if smtp_host_configured {
        let need_label = match need_type {
            "home_inspector" => "Licensed Local Home Inspector",
            "both" => "Local Buyer's Agent + Licensed Home Inspector",
            _ => "Local Buyer's Agent (MLS Disclosure/Inspection Packet & Representation)",
        };
        if let Some(notify_to) = resolve_notify_recipient(env) {
            let subject = format!(
                "[{}] {} Match Request — {}",
                env.brand_name(),
                need_label,
                display_address
            );
            let text_body = format!(
                "New {} recommendation request on {}\n\n\
                 Request ID: {}\n\
                 Property: {} (slug: {})\n\
                 Location: {}, {} {}\n\
                 Requested Role: {}\n\
                 Document Context: {}\n\
                 Preferred Partner ID: {}\n\
                 Buyer Name: {}\n\
                 Buyer Email: {}\n\
                 Buyer Phone: {}\n\
                 Notes: {}\n\
                 Submitted At: {}\n",
                env.brand_name(),
                env.brand_domain(),
                req_id,
                display_address,
                resolved_slug,
                city.as_deref().unwrap_or("—"),
                state.as_deref().unwrap_or("—"),
                zip.as_deref().unwrap_or(""),
                need_label,
                doc_needed.as_deref().unwrap_or("inspection_and_disclosures"),
                preferred_partner_id.as_deref().unwrap_or("any_local_specialist"),
                buyer_name.as_deref().unwrap_or("—"),
                buyer_email,
                buyer_phone.as_deref().unwrap_or("—"),
                notes.as_deref().unwrap_or("—"),
                created_at,
            );
            let mail_req = EmailRequest {
                to: vec![notify_to],
                subject,
                text: Some(text_body),
                html: None,
                reply_to: Some(buyer_email.clone()),
                unsubscribe_url: None,
            };
            row.smtp_status = match send_smtp_email(env, &mail_req).await {
                Ok(_) => "sent".to_string(),
                Err(_) => "failed".to_string(),
            };
            let _ = db.upsert_agent_match_request(&row).await;
        }
    }

    let empty_report = json!({
        "geo": { "city": city, "state": state, "zip": zip },
        "facts": {}
    });
    let recs = build_agent_recommendations(db, report_opt.unwrap_or(&empty_report)).await;

    let role_phrase = match need_type {
        "home_inspector" => "a licensed local home inspector",
        "both" => "a local buyer's agent and licensed home inspector",
        _ => "a local buyer's agent",
    };

    Ok(json!({
        "ok": true,
        "requestId": req_id,
        "slug": resolved_slug,
        "needType": need_type,
        "smtpStatus": row.smtp_status,
        "createdAt": created_at,
        "message": format!(
            "Request received! We'll follow up at {} with {} recommendation for {} so you can pull the disclosure/inspection packet or schedule an inspection.",
            buyer_email, role_phrase, display_address
        ),
        "recommendedAgents": recs["recommendedAgents"],
        "recommendedInspectors": recs["recommendedInspectors"],
        "isEmpty": recs["isEmpty"],
        "complianceNotice": AGENT_MATCH_COMPENSATION_NOTICE,
    }))
}
