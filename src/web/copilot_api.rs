//! Axum HTTP handlers for Property Copilot (`/api/copilot/*`):
//!   - `POST /api/copilot/lookup`: Validate address -> Basic info + Listing status gate + role selection
//!   - `POST /api/copilot/comps`: Function 1 (Comps table + 3 non-linear follow-ups reusing pinned comp set)
//!   - `GET|POST /api/copilot/disclosures`: Function 2 (.zip / multi-PDF priority reader + file discard policy + audit log)
//!   - `GET|POST /api/copilot/offer`: Function 3 (6-step Offer Builder + lookup defaults + running editable summary)

use crate::ai::packet_review::{analyze_disclosure_packet, build_file_retention_policy};
use crate::ai::pdf::{expand_uploaded_documents_with_zip, MAX_PACKET_DOCUMENTS};
use crate::guardrails::{
    enforce_allowed_body_keys, extract_browser_client_id, validate_address_input,
    validate_slug_param, verify_same_origin, GuardrailError,
};
use crate::modules::buyer_copilot::{
    apply_offer_patch, build_copilot_mailto_request, build_offer_evaluation,
    build_property_status_card, build_public_record_equivalents, compute_copilot_comps,
    compute_copilot_followup, init_offer_draft, normalize_copilot_role, OfferDraft,
    COPILOT_STANDING_DISCLAIMER,
};
use crate::property::{build_facts_and_mls_with_given, geocode_address};
use crate::shared::{now_iso, slugify, EnvConfig};
use crate::web::http_util::{
    json_response, map_anyhow, map_guardrail, parse_json_body, req_host_str, ApiResult,
};
use crate::web::orchestrator::DbLlmCache;
use crate::web::server::{
    get_cached_or_disk_report, parse_uploaded_docs, resolve_client_ip_from_req, AppState,
};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Query, State},
    http::{Extensions, HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::{json, Value};

const BASIS_SHARED_CLIENT: &str = "_shared";
pub const DEFAULT_SESSION_TTL_SECS: i64 = 86_400 * 14;
pub const DEFAULT_COPILOT_DISCLOSURE_TTL_DAYS: i64 = 30;

pub fn resolve_copilot_disclosure_ttl_secs(env: &EnvConfig) -> i64 {
    let days = env
        .get("COPILOT_DISCLOSURE_TTL_DAYS")
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&d| (1..=3650).contains(&d))
        .unwrap_or(DEFAULT_COPILOT_DISCLOSURE_TTL_DAYS);
    days.saturating_mul(86_400)
}

#[derive(Debug, Deserialize)]
pub struct SlugQuery {
    pub slug: Option<String>,
    #[serde(rename = "recipientView", alias = "requireAuth", default)]
    pub recipient_view: Option<bool>,
}

async fn resolve_disclosure_owner_key(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    reject_invalid_session: bool,
) -> std::result::Result<String, Response> {
    if let Some(owner_key) = state
        .hooks
        .resolve_shared_disclosure_owner(state, headers, reject_invalid_session)
        .await?
    {
        return Ok(owner_key);
    }
    Ok(client_key_or_fallback(headers, state, extensions))
}

/// Resolves any saved disclosure/packet review for `slug` belonging to the caller's verified
/// session, browser client id, or host-extension shared/authorized disclosure record so
/// `/api/ask` and PDF Studio Q&A stay grounded in uploaded packets.
pub(crate) async fn resolve_disclosure_packet_for_ask(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    cid_opt: Option<&str>,
) -> Option<Value> {
    if let Ok(Some(owner_key)) = state
        .hooks
        .resolve_shared_disclosure_owner(state, headers, false)
        .await
    {
        if let Ok(Some(doc)) = state
            .db
            .get_client_doc_review("copilot_disclosures", slug, &owner_key)
            .await
        {
            return Some(doc);
        }
    }
    if let Some(cid) = cid_opt {
        for kind in ["copilot_disclosures", "packet"] {
            if let Ok(Some(doc)) = state.db.get_client_doc_review(kind, slug, cid).await {
                return Some(doc);
            }
        }
    }
    if let Ok((_, Some(auth_rec))) = state
        .hooks
        .load_shared_disclosures_for_viewer(state, headers, slug, false)
        .await
    {
        return Some(auth_rec);
    }
    None
}

pub fn clean_plain_text_field(raw: Option<&str>, max_chars: usize) -> Option<String> {
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
        if in_tag || ch.is_control() {
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

pub fn attach_smtp_disabled(mut mailto: Value, state: &AppState) -> Value {
    if let Some(obj) = mailto.as_object_mut() {
        obj.insert(
            "smtpDisabled".to_string(),
            json!(!state.env.smtp_feature_enabled()),
        );
    }
    mailto
}

pub fn client_key_or_fallback(
    headers: &HeaderMap,
    state: &AppState,
    extensions: &Extensions,
) -> String {
    if let Some(cid) = extract_browser_client_id(headers) {
        return cid;
    }
    let ip = resolve_client_ip_from_req(state, headers, extensions);
    format!("ip_{}", ip.chars().take(48).collect::<String>())
}

pub struct ResolvedPropertyBasis {
    pub slug: String,
    pub geo: Value,
    pub facts: Value,
    pub valuation: Option<Value>,
    pub full_report: Option<Value>,
}

pub async fn resolve_property_basis_by_slug(
    state: &AppState,
    slug: &str,
) -> std::result::Result<ResolvedPropertyBasis, Response> {
    if let Some(rec) = get_cached_or_disk_report(state, slug).await {
        let geo = rec.report.get("geo").cloned().unwrap_or_else(|| json!({}));
        let facts = rec.report.get("facts").cloned().unwrap_or_else(|| json!({}));
        let valuation = rec.report.get("valuation").cloned();
        return Ok(ResolvedPropertyBasis {
            slug: rec.slug,
            geo,
            facts,
            valuation,
            full_report: Some(rec.report),
        });
    }

    if let Ok(Some(basis)) = state
        .db
        .get_client_doc_review("copilot_basis", slug, BASIS_SHARED_CLIENT)
        .await
    {
        let geo = basis.get("geo").cloned().unwrap_or_else(|| json!({}));
        let facts = basis.get("facts").cloned().unwrap_or_else(|| json!({}));
        return Ok(ResolvedPropertyBasis {
            slug: slug.to_string(),
            geo,
            facts,
            valuation: None,
            full_report: None,
        });
    }

    // Check if local `listings` DB has a row for this slug
    if let Ok(rows) = state.db.list_listings_for_engagement(slug, None).await {
        if let Some(first) = rows.first() {
            let geo = json!({
                "matchedAddress": first.address,
                "city": first.city,
                "state": first.state,
                "zip": first.zip,
                "lat": first.lat,
                "lon": first.lon,
            });
            let facts = json!({
                "address": first.address,
                "propertyType": first.property_type,
                "beds": first.beds,
                "baths": first.baths,
                "sqft": first.sqft,
                "lotSqft": first.lot_sqft,
                "yearBuilt": first.year_built,
                "hoaMonthly": first.hoa_monthly,
                "listPrice": first.list_price.or(first.price),
                "daysOnMarket": first.days_on_market,
                "forSale": first.status == "for_sale",
                "listingStatus": first.status,
            });
            return Ok(ResolvedPropertyBasis {
                slug: slug.to_string(),
                geo,
                facts,
                valuation: None,
                full_report: None,
            });
        }
    }

    Err(json_response(
        StatusCode::NOT_FOUND,
        json!({
            "ok": false,
            "code": "PROPERTY_NOT_LOADED",
            "error": "Property session expired or not yet looked up. Please search the address first."
        }),
    ))
}

/// `POST /api/copilot/lookup`
/// Validates a US street address (or loads an existing report `slug`), returning basic
/// property info, listing status (`for_sale` vs. "Not currently listed" notice), active role,
/// and any saved session state for Functions 1–3.
pub async fn handle_copilot_lookup_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 6, 64)?;
    enforce_allowed_body_keys(&body, &["address", "slug", "role"]).map_err(map_guardrail)?;

    let active_role = normalize_copilot_role(body.get("role").and_then(Value::as_str));
    let needs_role_selection = active_role == "unset";

    let explicit_slug = body
        .get("slug")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let raw_address = body
        .get("address")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let basis = if let Some(sl_raw) = explicit_slug {
        let clean_slug = validate_slug_param(sl_raw).map_err(map_guardrail)?;
        resolve_property_basis_by_slug(&state, &clean_slug).await?
    } else if let Some(addr_raw) = raw_address {
        let clean_addr = validate_address_input(addr_raw).map_err(|_| {
            map_guardrail(GuardrailError::new(
                "INVALID_ADDRESS",
                "Please enter a valid US street address (street number, street name, and city/state or ZIP).",
                400,
            ))
        })?;
        // Require a street number so bare city/state strings trigger an invalid-address error per PRD
        if !clean_addr
            .split_whitespace()
            .next()
            .map(|tok| tok.chars().any(|c| c.is_ascii_digit()))
            .unwrap_or(false)
        {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_ADDRESS",
                "Please include a street number and street name (for example: 123 Synth Main St, City, ST).",
                400,
            )));
        }

        let guess_slug = slugify(&clean_addr);
        if !guess_slug.is_empty() {
            if let Ok(existing) = resolve_property_basis_by_slug(&state, &guess_slug).await {
                existing
            } else {
                lookup_live_address_basis(&state, &clean_addr).await?
            }
        } else {
            lookup_live_address_basis(&state, &clean_addr).await?
        }
    } else {
        return Err(map_guardrail(GuardrailError::new(
            "INVALID_ADDRESS",
            "Address is required.",
            400,
        )));
    };

    let zip_opt = basis.geo.get("zip").and_then(|v| v.as_str());
    let db_rows = state
        .db
        .list_listings_for_engagement(&basis.slug, zip_opt)
        .await
        .unwrap_or_default();
    let db_row = db_rows.first();

    let mut status_card = build_property_status_card(
        &basis.slug,
        &basis.geo,
        &basis.facts,
        basis.valuation.as_ref(),
        db_row,
    );
    if let Some(mr) = status_card
        .get_mut("materialRequestMailto")
        .and_then(Value::as_object_mut)
    {
        mr.insert(
            "smtpDisabled".to_string(),
            json!(!state.env.smtp_feature_enabled()),
        );
    }

    let cid = client_key_or_fallback(&headers, &state, &extensions);
    let disc_owner_key = resolve_disclosure_owner_key(&state, &headers, &extensions, false).await?;
    let saved_comps = state
        .db
        .get_client_doc_review("copilot_comps", &basis.slug, &cid)
        .await
        .ok()
        .flatten();
    let saved_disclosures = state
        .db
        .get_client_doc_review("copilot_disclosures", &basis.slug, &disc_owner_key)
        .await
        .ok()
        .flatten();
    let saved_offer = state
        .db
        .get_client_doc_review("copilot_offer", &basis.slug, &cid)
        .await
        .ok()
        .flatten();

    let public_equiv = build_public_record_equivalents(
        &basis.slug,
        &basis.geo,
        &basis.facts,
        basis.full_report.as_ref(),
    );
    let (_shared_deal_team, authorized_disclosures) = state
        .hooks
        .load_shared_disclosures_for_viewer(&state, &headers, &basis.slug, false)
        .await?;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "sharingEnabled": state.hooks.sharing_enabled(),
            "activeRole": active_role,
            "needsRoleSelection": needs_role_selection,
            "property": status_card,
            "hasFullReport": basis.full_report.is_some(),
            "publicRecordEquivalents": public_equiv,
            "authorizedDisclosures": authorized_disclosures,
            "fileRetentionPolicy": build_file_retention_policy(),
            "savedState": {
                "comps": saved_comps,
                "disclosures": saved_disclosures,
                "offer": saved_offer,
            }
        }),
    ))
}

async fn lookup_live_address_basis(
    state: &AppState,
    clean_addr: &str,
) -> std::result::Result<ResolvedPropertyBasis, Response> {
    let geo_out = geocode_address(&state.http, clean_addr).await.map_err(|_| {
        map_guardrail(GuardrailError::new(
            "INVALID_ADDRESS",
            "Could not validate this US street address. Please check the street number, city, state, or ZIP code.",
            400,
        ))
    })?;

    let lat_ok = geo_out
        .data
        .get("lat")
        .and_then(|v| v.as_f64())
        .map(|x| x.is_finite())
        .unwrap_or(false);
    let lon_ok = geo_out
        .data
        .get("lon")
        .and_then(|v| v.as_f64())
        .map(|x| x.is_finite())
        .unwrap_or(false);
    if !lat_ok || !lon_ok {
        return Err(map_guardrail(GuardrailError::new(
            "INVALID_ADDRESS",
            "Address could not be matched to a valid US street location.",
            400,
        )));
    }

    let matched = geo_out
        .data
        .get("matchedAddress")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(clean_addr);
    let slug = slugify(matched);

    // Check if geocoded canonical slug already has a full cached report!
    if let Some(rec) = get_cached_or_disk_report(state, &slug).await {
        let geo = rec.report.get("geo").cloned().unwrap_or(geo_out.data);
        let facts = rec.report.get("facts").cloned().unwrap_or_else(|| json!({}));
        let valuation = rec.report.get("valuation").cloned();
        return Ok(ResolvedPropertyBasis {
            slug: rec.slug,
            geo,
            facts,
            valuation,
            full_report: Some(rec.report),
        });
    }

    let facts_val = match build_facts_and_mls_with_given(
        &state.http,
        &state.env,
        matched,
        &geo_out.data,
        None,
        None,
    )
    .await
    {
        Ok(f_out) => f_out.data,
        Err(_) => json!({ "address": matched }),
    };

    let basis_blob = json!({
        "geo": geo_out.data,
        "facts": facts_val,
    });
    let _ = state
        .db
        .put_client_doc_review(
            "copilot_basis",
            &slug,
            BASIS_SHARED_CLIENT,
            &basis_blob,
            86_400 * 7,
        )
        .await;

    Ok(ResolvedPropertyBasis {
        slug,
        geo: geo_out.data,
        facts: facts_val,
        valuation: None,
        full_report: None,
    })
}

/// `POST /api/copilot/comps`
/// Function 1: Generates or expands the sold comps table first, or runs any of the 3 optional
/// follow-ups (`"estimate"`, `"similar_active"`, `"sold_or_pending"`) reusing the pinned comp set.
pub async fn handle_copilot_comps_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 6, 64)?;
    enforce_allowed_body_keys(&body, &["slug", "mode", "followUp", "compSetId"])
        .map_err(map_guardrail)?;

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;
    let basis = resolve_property_basis_by_slug(&state, &slug).await?;

    let lat = basis.geo.get("lat").and_then(|v| v.as_f64());
    let lon = basis.geo.get("lon").and_then(|v| v.as_f64());
    let db_nearby = match (lat, lon) {
        (Some(la), Some(lo)) => state
            .db
            .query_listings_within_radius(la, lo, 2.05, None, 200)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    let cid = client_key_or_fallback(&headers, &state, &extensions);
    let follow_up_opt = body
        .get("followUp")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let requested_mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("strict");

    if let Some(follow_up) = follow_up_opt {
        let mut saved_env = state
            .db
            .get_client_doc_review("copilot_comps", &basis.slug, &cid)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                let cs = compute_copilot_comps(
                    &basis.slug,
                    &basis.geo,
                    &basis.facts,
                    basis.full_report.as_ref(),
                    &db_nearby,
                    requested_mode,
                );
                json!({ "compSet": cs, "followUps": {} })
            });

        let pinned_comp_set = saved_env
            .get("compSet")
            .cloned()
            .unwrap_or_else(|| saved_env.clone());

        let follow_res = compute_copilot_followup(
            follow_up,
            &pinned_comp_set,
            &basis.geo,
            &basis.facts,
            &db_nearby,
        )
        .map_err(|msg| map_guardrail(GuardrailError::new("INVALID_FOLLOW_UP", &msg, 400)))?;

        if let Some(obj) = saved_env.as_object_mut() {
            if !obj.contains_key("compSet") {
                obj.insert("compSet".to_string(), pinned_comp_set.clone());
            }
            let f_map = obj
                .entry("followUps".to_string())
                .or_insert_with(|| json!({}));
            if let Some(fm) = f_map.as_object_mut() {
                fm.insert(follow_up.to_string(), follow_res.clone());
            }
        }
        let _ = state
            .db
            .put_client_doc_review(
                "copilot_comps",
                &basis.slug,
                &cid,
                &saved_env,
                DEFAULT_SESSION_TTL_SECS,
            )
            .await;

        return Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "slug": basis.slug,
                "compSet": pinned_comp_set,
                "followUpResult": follow_res,
            }),
        ));
    }

    let comp_set = compute_copilot_comps(
        &basis.slug,
        &basis.geo,
        &basis.facts,
        basis.full_report.as_ref(),
        &db_nearby,
        requested_mode,
    );
    let envelope = json!({
        "compSet": comp_set,
        "followUps": {},
    });
    let _ = state
        .db
        .put_client_doc_review(
            "copilot_comps",
            &basis.slug,
            &cid,
            &envelope,
            DEFAULT_SESSION_TTL_SECS,
        )
        .await;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": basis.slug,
            "compSet": comp_set,
        }),
    ))
}

/// `GET /api/copilot/disclosures?slug=...`
pub async fn handle_copilot_disclosures_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<SlugQuery>,
) -> Response {
    let recipient_view_only = q.recipient_view == Some(true);
    let sharing_enabled = state.hooks.sharing_enabled();
    if recipient_view_only && !sharing_enabled {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "code": "FEATURE_DISABLED",
                "error": "Shared disclosure recipient view is not enabled on this standalone server."
            }),
        );
    }

    let owner_key = match resolve_disclosure_owner_key(&state, &headers, &extensions, true).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };

    let policy = build_file_retention_policy();
    let Ok(slug) = validate_slug_param(q.slug.as_deref().unwrap_or("")) else {
        if recipient_view_only {
            if let Err(resp) = state
                .hooks
                .load_shared_disclosures_for_viewer(&state, &headers, "", true)
                .await
            {
                return resp;
            }
        }
        return json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "sharingEnabled": sharing_enabled,
                "oneTimeCompleted": false,
                "data": Value::Null,
                "privateUserAnalysis": Value::Null,
                "sharedWithMeAnalysis": Value::Null,
                "sharedDealTeamAnalysis": Value::Null,
                "authorizedDisclosures": Value::Null,
                "publicRecordEquivalents": Value::Null,
                "fileRetentionPolicy": policy,
                "mailtoRequest": Value::Null,
                "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
            }),
        );
    };

    let (eff_slug, geo, facts, full_report) = match resolve_property_basis_by_slug(&state, &slug).await {
        Ok(b) => (b.slug, b.geo, b.facts, b.full_report),
        Err(_) => (slug.clone(), json!({}), json!({}), None),
    };

    let (shared_with_me_analysis, authorized_disclosures) = match state
        .hooks
        .load_shared_disclosures_for_viewer(&state, &headers, &eff_slug, recipient_view_only)
        .await
    {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let private_user_analysis = state
        .db
        .get_client_doc_review("copilot_disclosures", &eff_slug, &owner_key)
        .await
        .ok()
        .flatten();

    let one_time_completed = private_user_analysis
        .as_ref()
        .and_then(|v| v.get("oneTimeCompleted"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let public_equiv =
        build_public_record_equivalents(&eff_slug, &geo, &facts, full_report.as_ref());
    let mailto_req = attach_smtp_disabled(build_copilot_mailto_request(&geo, &facts), &state);

    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": eff_slug,
            "sharingEnabled": sharing_enabled,
            "oneTimeCompleted": one_time_completed,
            "data": private_user_analysis,
            "privateUserAnalysis": private_user_analysis,
            "sharedWithMeAnalysis": shared_with_me_analysis,
            "sharedDealTeamAnalysis": shared_with_me_analysis,
            "authorizedDisclosures": authorized_disclosures,
            "publicRecordEquivalents": public_equiv,
            "fileRetentionPolicy": policy,
            "mailtoRequest": mailto_req,
            "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
        }),
    )
}

/// `POST /api/copilot/disclosures`
/// Function 2: Per-user one-time comprehensive `.zip` / multi-PDF disclosure analysis.
/// Uses verified collaborator owner key when hosted sharing extension is active, otherwise
/// browser `cid`. Enforces strict `.zip`-bomb guards, immediate raw-file discard, and 409 on
/// second attempt.
pub async fn handle_copilot_disclosures_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let owner_key = resolve_disclosure_owner_key(&state, &headers, &extensions, true).await?;

    let body = parse_json_body(raw_body, 16, 25_000)?;
    enforce_allowed_body_keys(&body, &["slug", "documents", "regionalMultiplier"])
        .map_err(map_guardrail)?;

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;
    let basis = resolve_property_basis_by_slug(&state, &slug).await?;

    // Enforce strict one-time per-user analysis BEFORE keeping or decoding files.
    if let Ok(Some(existing)) = state
        .db
        .get_client_doc_review("copilot_disclosures", &basis.slug, &owner_key)
        .await
    {
        if existing.get("oneTimeCompleted").and_then(|v| v.as_bool()) == Some(true) {
            drop(body);
            return Err(map_guardrail(GuardrailError::new(
                "ONE_TIME_ANALYSIS_ALREADY_USED",
                "One-time disclosure analysis has already been completed for your session on this property. Your comprehensive analysis is saved privately below, and original files were discarded.",
                409,
            )));
        }
    }

    let raw_docs = parse_uploaded_docs(&body).map_err(map_guardrail)?;
    let regional_mult = body.get("regionalMultiplier").and_then(|v| v.as_f64());
    // Discard raw JSON request payload containing base64 bytes immediately.
    drop(body);

    // Pre-validate and unpack `.zip` archives with strict zip-bomb, traversal, and nested-archive guards
    // BEFORE consuming AI rate-limit budget.
    let expanded_docs =
        expand_uploaded_documents_with_zip(&raw_docs, MAX_PACKET_DOCUMENTS).map_err(map_guardrail)?;
    drop(raw_docs);

    let report_ctx = basis.full_report.as_ref().cloned().unwrap_or_else(|| {
        json!({
            "meta": { "slug": basis.slug, "displayAddress": basis.geo.get("matchedAddress") },
            "geo": basis.geo,
            "facts": basis.facts,
        })
    });

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "packet")
        .map_err(map_guardrail)?;

    let llm_cache = DbLlmCache(state.db.clone());
    // `expanded_docs` is moved in and explicitly dropped inside `analyze_disclosure_packet` immediately after extraction.
    let mut out = analyze_disclosure_packet(
        &state.http,
        &state.env,
        &report_ctx,
        expanded_docs,
        regional_mult,
        Some(&llm_cache),
    )
    .await
    .map_err(map_anyhow)?;

    let policy = build_file_retention_policy();
    let analyzed_at = now_iso();
    if let Some(obj) = out.data.as_object_mut() {
        obj.insert("oneTimeCompleted".to_string(), json!(true));
        obj.insert("analyzedAt".to_string(), json!(analyzed_at));
        obj.insert("fileRetentionPolicy".to_string(), policy.clone());
    }

    // Part 3 Guardrail: Log every disclosure finding's source document & page for verification
    let audit_entries = out
        .data
        .pointer("/priorityReading/auditEntries")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let missing_docs = out
        .data
        .pointer("/priorityReading/missingDocuments")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let audit_payload = json!({
        "slug": basis.slug,
        "packetDocumentCount": out.data.get("packetDocumentCount"),
        "presentCategoryCount": out.data.pointer("/priorityReading/presentCategoryCount"),
        "missingDocuments": missing_docs,
        "loggedFindings": audit_entries,
        "rawFilesDiscarded": true,
    });
    let _ = state
        .db
        .append_admin_audit_log(
            &owner_key,
            "disclosure_findings_logged",
            "disclosure_packet",
            &basis.slug,
            &audit_payload,
        )
        .await;

    let ttl_secs = resolve_copilot_disclosure_ttl_secs(&state.env);
    let _ = state
        .db
        .put_client_doc_review(
            "copilot_disclosures",
            &basis.slug,
            &owner_key,
            &out.data,
            ttl_secs,
        )
        .await;

    let public_equiv = build_public_record_equivalents(
        &basis.slug,
        &basis.geo,
        &basis.facts,
        basis.full_report.as_ref(),
    );
    let mailto_req = attach_smtp_disabled(
        build_copilot_mailto_request(&basis.geo, &basis.facts),
        &state,
    );
    let sharing_enabled = state.hooks.sharing_enabled();
    let (_, authorized_disclosures) = state
        .hooks
        .load_shared_disclosures_for_viewer(&state, &headers, &basis.slug, false)
        .await
        .unwrap_or((None, None));

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": basis.slug,
            "sharingEnabled": sharing_enabled,
            "oneTimeCompleted": true,
            "analyzedAt": analyzed_at,
            "data": out.data,
            "privateUserAnalysis": out.data,
            "authorizedDisclosures": authorized_disclosures,
            "publicRecordEquivalents": public_equiv,
            "fileRetentionPolicy": policy,
            "mailtoRequest": mailto_req,
            "warnings": out.warnings,
            "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
        }),
    ))
}

async fn load_offer_context_parts(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    cid: &str,
    disc_owner_key: &str,
) -> (Option<Value>, Option<Vec<Value>>) {
    let saved_comps = state
        .db
        .get_client_doc_review("copilot_comps", slug, cid)
        .await
        .ok()
        .flatten();
    let pinned_est = saved_comps
        .as_ref()
        .and_then(|v| v.pointer("/followUps/estimate"))
        .cloned();

    let saved_disc = match state
        .db
        .get_client_doc_review("copilot_disclosures", slug, disc_owner_key)
        .await
        .ok()
        .flatten()
    {
        Some(v) => Some(v),
        None => match state
            .db
            .get_client_doc_review("packet", slug, cid)
            .await
            .ok()
            .flatten()
        {
            Some(v) => Some(v),
            None => state
                .hooks
                .load_shared_disclosures_for_viewer(state, headers, slug, false)
                .await
                .ok()
                .and_then(|(_, auth)| auth)
                .and_then(|rec| rec.get("data").cloned()),
        },
    };
    let red_flags = saved_disc
        .as_ref()
        .and_then(|d| d.pointer("/priorityReading/redFlags"))
        .and_then(|v| v.as_array())
        .cloned();

    (pinned_est, red_flags)
}

/// `GET /api/copilot/offer?slug=...`
pub async fn handle_copilot_offer_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<SlugQuery>,
) -> ApiResult {
    let slug = validate_slug_param(q.slug.as_deref().unwrap_or("")).map_err(map_guardrail)?;
    let basis = resolve_property_basis_by_slug(&state, &slug).await?;
    let cid = client_key_or_fallback(&headers, &state, &extensions);
    let disc_owner_key = resolve_disclosure_owner_key(&state, &headers, &extensions, false).await?;

    let draft: OfferDraft = state
        .db
        .get_client_doc_review("copilot_offer", &basis.slug, &cid)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.get("draft").cloned().or(Some(v)))
        .and_then(|v| serde_json::from_value::<OfferDraft>(v).ok())
        .unwrap_or_else(|| init_offer_draft(&basis.slug, &basis.geo, &basis.facts));

    let (pinned_est, red_flags) =
        load_offer_context_parts(&state, &headers, &basis.slug, &cid, &disc_owner_key).await;
    let evaluated = build_offer_evaluation(
        &draft,
        &basis.geo,
        &basis.facts,
        pinned_est.as_ref(),
        red_flags.as_deref(),
    );

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": basis.slug,
            "offer": evaluated,
        }),
    ))
}

const ALLOWED_OFFER_KEYS: &[&str] = &[
    "slug",
    "step",
    "currentStep",
    "reset",
    "importDisclosures",
    "offerPriceUsd",
    "financingType",
    "downPaymentPct",
    "loanPct",
    "earnestMoneyPct",
    "coeDays",
    "targetCloseDate",
    "loanContingencyEnabled",
    "loanContingencyDays",
    "inspectionContingencyEnabled",
    "inspectionContingencyDays",
    "appraisalContingencyEnabled",
    "appraisalContingencyDays",
    "rentBackEnabled",
    "rentBackDays",
    "rentBackDepositUsd",
    "rentBackDailyRateUsd",
    "saleOfPropertyContingencyEnabled",
    "saleOfPropertyAddress",
    "saleOfPropertyDays",
    "buyerAgentCommissionPct",
    "buyerAgentCommissionPayer",
    "cityTransferTaxPayer",
    "countyTransferTaxPayer",
    "escrowFeePayer",
    "ownersTitlePayer",
    "lendersTitlePayer",
    "homeWarrantyEnabled",
    "homeWarrantyMonths",
    "homeWarrantyMaxUsd",
    "homeWarrantyPayer",
    "sellerRepairs",
    "sellerCreditUsd",
    "includedItems",
    "otherTerms",
];

/// `POST /api/copilot/offer`
/// Function 3: Updates any of the 6 Offer Builder input groups (or inline running-summary edits)
/// and returns the live editable summary + final non-binding Offer Review Summary.
pub async fn handle_copilot_offer_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 8, 128)?;
    enforce_allowed_body_keys(&body, ALLOWED_OFFER_KEYS).map_err(map_guardrail)?;

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;
    let basis = resolve_property_basis_by_slug(&state, &slug).await?;
    let cid = client_key_or_fallback(&headers, &state, &extensions);
    let disc_owner_key = resolve_disclosure_owner_key(&state, &headers, &extensions, false).await?;

    let reset = body.get("reset").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut draft: OfferDraft = if reset {
        init_offer_draft(&basis.slug, &basis.geo, &basis.facts)
    } else {
        state
            .db
            .get_client_doc_review("copilot_offer", &basis.slug, &cid)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.get("draft").cloned().or(Some(v)))
            .and_then(|v| serde_json::from_value::<OfferDraft>(v).ok())
            .unwrap_or_else(|| init_offer_draft(&basis.slug, &basis.geo, &basis.facts))
    };

    let (pinned_est, red_flags) =
        load_offer_context_parts(&state, &headers, &basis.slug, &cid, &disc_owner_key).await;

    if body
        .get("importDisclosures")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        if let Some(ref rfs) = red_flags {
            let lines: Vec<String> = rfs
                .iter()
                .take(6)
                .filter_map(|rf| {
                    let t = rf.get("title")?.as_str()?;
                    let d = rf.get("sourceDoc").and_then(|v| v.as_str()).unwrap_or("Report");
                    let p = rf.get("sourcePage").and_then(|v| v.as_u64()).unwrap_or(1);
                    Some(format!("• {t} ({d}, p.{p})"))
                })
                .collect();
            if !lines.is_empty() {
                draft.seller_repairs = lines.join("\n");
            }
        }
    }

    apply_offer_patch(&mut draft, &body);

    let evaluated = build_offer_evaluation(
        &draft,
        &basis.geo,
        &basis.facts,
        pinned_est.as_ref(),
        red_flags.as_deref(),
    );

    let _ = state
        .db
        .put_client_doc_review(
            "copilot_offer",
            &basis.slug,
            &cid,
            &evaluated,
            DEFAULT_SESSION_TTL_SECS,
        )
        .await;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": basis.slug,
            "offer": evaluated,
        }),
    ))
}
