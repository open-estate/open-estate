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

use crate::db::proto::{RecommendedAgentAreaProto, RecommendedAgentTagProto};
use crate::db::{
    deterministic_sk, AgentMatchRequestRow, DbStore, RecommendedAgentProto, SpecialistRole,
    SqlParam,
};
use crate::guardrails::GuardrailError;
use crate::search::{send_smtp_email, validate_email_input, EmailRequest};
use crate::shared::{now_iso, now_unix_secs, slugify, EnvConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
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

fn classify_specialist_proto_kind(row: &RecommendedAgentProto) -> Option<&'static str> {
    match SpecialistRole::try_from(row.role).unwrap_or(SpecialistRole::Unspecified) {
        SpecialistRole::BuyerAgent => Some("buyer_agent"),
        SpecialistRole::HomeInspector => Some("home_inspector"),
        _ => classify_specialist_kind(&row.role_label),
    }
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

async fn load_active_recommended_agents_normalized(
    db: &DbStore,
    price_opt: Option<f64>,
) -> anyhow::Result<Vec<RecommendedAgentProto>> {
    db.ensure_schema().await?;
    let mut sql = String::from(
        "SELECT agent_sk, partner_id, role, role_label, full_name, firm_name, license_number, \
                phone, email, website_url, min_price, max_price, notes, is_sponsored, \
                priority, active, created_ts, updated_ts \
         FROM recommended_agents \
         WHERE active = 1",
    );
    let mut params: Vec<SqlParam> = Vec::new();
    if let Some(p) = price_opt {
        let p_int = p.round() as i64;
        sql.push_str(" AND (min_price IS NULL OR min_price <= ?) AND (max_price IS NULL OR max_price >= ?)");
        params.push(SqlParam::I64(p_int));
        params.push(SqlParam::I64(p_int));
    }
    sql.push_str(" ORDER BY is_sponsored DESC, priority DESC, updated_ts DESC LIMIT 200");

    let mut protos = db
        .fetch_all_mapped_raw(&sql, &params, |r| RecommendedAgentProto {
            agent_sk: r.opt_i64("agent_sk").unwrap_or(0),
            partner_id: r.opt_str("partner_id").unwrap_or_default(),
            role: r.opt_i64("role").unwrap_or(0) as i32,
            role_label: r.opt_str("role_label").unwrap_or_default(),
            full_name: r.opt_str("full_name").unwrap_or_default(),
            firm_name: r.opt_str("firm_name"),
            license_number: r.opt_str("license_number").unwrap_or_default(),
            phone: r.opt_str("phone"),
            email: r.opt_str("email"),
            website_url: r.opt_str("website_url"),
            min_price: r.opt_i64("min_price"),
            max_price: r.opt_i64("max_price"),
            notes: r.opt_str("notes"),
            is_sponsored: r.opt_i64("is_sponsored").unwrap_or(0) != 0,
            priority: r.opt_i64("priority").unwrap_or(100) as i32,
            active: r.opt_i64("active").unwrap_or(1) != 0,
            areas: Vec::new(),
            tags: Vec::new(),
            created_ts: r.opt_i64("created_ts").unwrap_or(0),
            updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
        })
        .await?;

    if protos.is_empty() {
        return Ok(protos);
    }

    let sk_params: Vec<SqlParam> = protos.iter().map(|p| SqlParam::I64(p.agent_sk)).collect();
    let ph = vec!["?"; sk_params.len()].join(", ");
    let area_sql = format!(
        "SELECT agent_sk, area_kind, area_value FROM recommended_agent_areas WHERE agent_sk IN ({ph}) ORDER BY area_value ASC"
    );
    let tag_sql = format!(
        "SELECT agent_sk, tag_kind, tag_value FROM recommended_agent_tags WHERE agent_sk IN ({ph}) ORDER BY tag_value ASC"
    );
    let (area_rows, tag_rows) = tokio::try_join!(
        db.fetch_all_mapped_raw(&area_sql, &sk_params, |r| {
            (
                r.opt_i64("agent_sk").unwrap_or(0),
                RecommendedAgentAreaProto {
                    area_kind: r.opt_i64("area_kind").unwrap_or(0) as i32,
                    area_value: r.opt_str("area_value").unwrap_or_default(),
                },
            )
        }),
        db.fetch_all_mapped_raw(&tag_sql, &sk_params, |r| {
            (
                r.opt_i64("agent_sk").unwrap_or(0),
                RecommendedAgentTagProto {
                    tag_kind: r.opt_i64("tag_kind").unwrap_or(0) as i32,
                    tag_value: r.opt_str("tag_value").unwrap_or_default(),
                },
            )
        }),
    )?;

    let mut areas_by_sk: HashMap<i64, Vec<RecommendedAgentAreaProto>> = HashMap::new();
    for (sk, area) in area_rows {
        if !area.area_value.trim().is_empty() {
            areas_by_sk.entry(sk).or_default().push(area);
        }
    }
    let mut tags_by_sk: HashMap<i64, Vec<RecommendedAgentTagProto>> = HashMap::new();
    for (sk, tag) in tag_rows {
        if !tag.tag_value.trim().is_empty() {
            tags_by_sk.entry(sk).or_default().push(tag);
        }
    }
    for p in &mut protos {
        if let Some(a) = areas_by_sk.remove(&p.agent_sk) {
            p.areas = a;
        }
        if let Some(t) = tags_by_sk.remove(&p.agent_sk) {
            p.tags = t;
        }
    }
    Ok(protos)
}

fn proto_matches_geo(
    row: &RecommendedAgentProto,
    state: &str,
    zip: &str,
    city: &str,
    price_opt: Option<f64>,
) -> bool {
    let mut has_states = false;
    let mut state_ok = false;
    let mut has_zips = false;
    let mut zip_ok = false;
    let mut has_cities = false;
    let mut city_ok = false;

    for area in &row.areas {
        let v = area.area_value.trim();
        if v.is_empty() {
            continue;
        }
        match area.area_kind {
            1 => {
                has_states = true;
                if !state.is_empty() && v.eq_ignore_ascii_case(state) {
                    state_ok = true;
                }
            }
            2 => {
                has_zips = true;
                if !zip.is_empty() && v == zip {
                    zip_ok = true;
                }
            }
            3 => {
                has_cities = true;
                if !city.is_empty() && v.eq_ignore_ascii_case(city) {
                    city_ok = true;
                }
            }
            _ => {}
        }
    }
    if has_states && !state.is_empty() && !state_ok {
        return false;
    }
    if has_zips && !zip.is_empty() && !zip_ok {
        return false;
    }
    if has_cities && !city.is_empty() && !city_ok {
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

fn sanitize_mailto_email(raw: Option<&str>) -> String {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return String::new();
    };
    validate_email_input(s).unwrap_or_default()
}

fn extract_report_str(report_opt: Option<&Value>, ptrs: &[&str], max_chars: usize) -> Option<String> {
    let report = report_opt?;
    for ptr in ptrs {
        if let Some(val) = report.pointer(ptr).and_then(Value::as_str) {
            if let Some(clean) = clean_plain_text(Some(val), max_chars) {
                return Some(clean);
            }
        }
    }
    None
}

/// Builds a prefilled `mailto:` material-request payload for requesting the 6 priority
/// disclosure/inspection documents from a listing agent or buyer's agent without needing server SMTP.
pub fn build_material_request_mailto(report_opt: Option<&Value>, display_address: &str) -> Value {
    let addr_clean = clean_plain_text(Some(display_address), 240)
        .filter(|s| s != "Subject Property")
        .or_else(|| {
            extract_report_str(
                report_opt,
                &[
                    "/meta/displayAddress",
                    "/geo/displayAddress",
                    "/geo/matchedAddress",
                    "/facts/address",
                ],
                240,
            )
        })
        .unwrap_or_else(|| "Subject Property".to_string());

    let recipient_email = extract_report_str(
        report_opt,
        &[
            "/facts/mlsListing/agentEmail",
            "/facts/mlsListing/listingAgent/email",
            "/facts/listingAgent/email",
            "/modules/mls/data/mlsListing/agentEmail",
            "/modules/mls/data/mlsListing/listingAgent/email",
        ],
        200,
    )
    .map(|e| sanitize_mailto_email(Some(&e)))
    .unwrap_or_default();

    let agent_name = extract_report_str(
        report_opt,
        &[
            "/facts/mlsListing/agentName",
            "/facts/mlsListing/listingAgent/name",
            "/facts/listingAgent/name",
            "/modules/mls/data/mlsListing/agentName",
            "/modules/mls/data/mlsListing/listingAgent/name",
        ],
        120,
    );
    let office_name = extract_report_str(
        report_opt,
        &[
            "/facts/mlsListing/officeName",
            "/facts/mlsListing/listingOffice/name",
            "/modules/mls/data/mlsListing/officeName",
        ],
        140,
    );

    let recipient_label = match (agent_name.as_deref(), office_name.as_deref()) {
        (Some(n), Some(o)) => format!("Listing Agent — {n} ({o})"),
        (Some(n), None) => format!("Listing Agent — {n}"),
        (None, Some(o)) => format!("Listing Brokerage — {o}"),
        (None, None) => "Listing Agent or Buyer's Agent".to_string(),
    };

    let apn = extract_report_str(report_opt, &["/facts/apn", "/facts/parcelNumber"], 64);
    let mls_num = extract_report_str(
        report_opt,
        &[
            "/facts/mlsListing/mlsNumber",
            "/modules/mls/data/mlsListing/mlsNumber",
        ],
        64,
    );

    let mut meta_lines = vec![format!("Property Address: {addr_clean}")];
    if let Some(ref a) = apn {
        meta_lines.push(format!("APN: {a}"));
    }
    if let Some(ref m) = mls_num {
        meta_lines.push(format!("MLS #: {m}"));
    }

    let subject = format!("Disclosure & Inspection Packet Request — {addr_clean}");
    let body = format!(
        "Hello,\n\n\
         I am evaluating the property below and would appreciate copies of any available disclosure and inspection documents (PDFs or .zip packet):\n\
         {}\n\n\
         Priority Documents Requested:\n\
         1. Home Inspection Report (including Roof, Sewer Lateral & Foundation/Structural)\n\
         2. Termite / Wood-Destroying Organism (WDO) Inspection Report\n\
         3. Preliminary Title Report (Schedule A/B, easements & liens)\n\
         4. Natural Hazard Disclosure (NHD) Report\n\
         5. HOA Financials, CC&Rs & Reserve Study (if applicable)\n\
         6. Building Permits & Municipal Records\n\n\
         Please reply with the .zip packet or PDF files when available. Thank you!",
        meta_lines.join("\n")
    );

    let mailto_href = format!(
        "mailto:{}?subject={}&body={}",
        recipient_email,
        urlencoding::encode(&subject),
        urlencoding::encode(&body)
    );

    json!({
        "recipientEmail": recipient_email,
        "recipientLabel": recipient_label,
        "apn": apn,
        "subject": subject,
        "body": body,
        "mailtoHref": mailto_href,
        "uploadFollowUpPrompt": "Once you receive the .zip archive or PDF reports, upload them here in Docs & AI for your one-time private analysis — original files are discarded immediately after analysis.",
        "smtpDisabled": true,
    })
}

/// Builds `report["agentRecommendations"]` strictly from the normalized `recommended_agents`,
/// `recommended_agent_areas`, and `recommended_agent_tags` database tables.
/// Because no agents are seeded into the database by default, `recommendedAgents` and
/// `recommendedInspectors` are `[]` (`isEmpty: true`) until licensed rows are inserted into `recommended_agents`.
pub async fn build_agent_recommendations(db: &DbStore, report: &Value) -> Value {
    let mut warnings: Vec<String> = Vec::new();
    let price_opt = extract_subject_price(report);
    let db_rows = match load_active_recommended_agents_normalized(db, price_opt).await {
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

    let mut agents: Vec<Value> = Vec::new();
    let mut inspectors: Vec<Value> = Vec::new();

    for (idx, row) in db_rows.iter().enumerate() {
        let Some(kind) = classify_specialist_proto_kind(row) else {
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
        if !proto_matches_geo(row, state, zip, city, price_opt) {
            continue;
        }
        let pid = clean_plain_text(Some(&row.partner_id), 80)
            .unwrap_or_else(|| format!("{kind}-{}", idx + 1));
        let specialties: Vec<String> = row
            .tags
            .iter()
            .filter(|t| t.tag_kind == 1)
            .filter_map(|t| clean_plain_text(Some(&t.tag_value), 64))
            .take(5)
            .collect();
        let languages: Vec<String> = row
            .tags
            .iter()
            .filter(|t| t.tag_kind == 2)
            .filter_map(|t| clean_plain_text(Some(&t.tag_value), 32))
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
    let display_address = extract_report_str(
        Some(report),
        &[
            "/meta/displayAddress",
            "/geo/displayAddress",
            "/geo/matchedAddress",
            "/facts/address",
        ],
        240,
    )
    .unwrap_or_else(|| "Subject Property".to_string());
    let mailto_request = build_material_request_mailto(Some(report), &display_address);

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
        "mailtoRequest": mailto_request,
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

    // Dispatch optional SMTP notification ONLY when `ENABLE_SMTP_FEATURES=true` and `SMTP_HOST` is configured
    let smtp_host_configured = env.smtp_feature_enabled()
        && env
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
            let _ = db
                .execute_params_raw(
                    "UPDATE agent_match_requests SET smtp_status = ?, updated_ts = ? WHERE request_sk = ?",
                    &[
                        SqlParam::from(row.smtp_status.as_str()),
                        SqlParam::I64(now_unix_secs()),
                        SqlParam::I64(deterministic_sk(&req_id)),
                    ],
                )
                .await;
        }
    }

    let empty_report = json!({
        "geo": { "city": city, "state": state, "zip": zip },
        "facts": {}
    });
    let recs = build_agent_recommendations(db, report_opt.unwrap_or(&empty_report)).await;
    let mut mailto_req = build_material_request_mailto(
        report_opt.or(Some(&empty_report)),
        &display_address,
    );
    if let Some(obj) = mailto_req.as_object_mut() {
        obj.insert("smtpDisabled".into(), json!(!env.smtp_feature_enabled()));
    }

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
        "mailtoRequest": mailto_req,
        "recommendedAgents": recs["recommendedAgents"],
        "recommendedInspectors": recs["recommendedInspectors"],
        "isEmpty": recs["isEmpty"],
        "complianceNotice": AGENT_MATCH_COMPENSATION_NOTICE,
    }))
}
