//! Property Copilot Conversational Workflow Engine (`buyer_copilot`).
//!
//! Implements the role-aware Open Estate AI workflow:
//!   0. Address Validation, Honest Basic Info & Listing Status Gate (`for_sale` vs.
//!      "Not currently listed" soft notice; hard errors reserved for invalid addresses).
//!   1. Comps & Pricing (`Function 1`): Strict Appendix sold-comp table (`90%–110% sqft`,
//!      similar age, same property type, `<= 3 mo`, `<= 1 mi`) with 1-click sparse fallback
//!      (`80%–120% sqft`, `<= 6 mo`, `<= 2 mi`), session-pinned `compSetId`, and three
//!      independent non-linear follow-ups reusing the exact same comp set:
//!        - Comp-based Price Estimate (range + confidence level)
//!        - Similar Active Listings (`+-15% sqft`, `<= 2 mi`)
//!        - Recently Sold or Pending
//!   2. Disclosures Priority Reader (`Function 2`): Coupled with `crate::ai::packet_review`
//!      (`priorityReading` in strict `1..=6` order, explicit missing-document reporting,
//!      concise Red Flags list, standing non-advice disclaimer, and source-document audit log).
//!   3. Guided Offer Builder (`Function 3`): 6-group guided form with jurisdiction-backed
//!      city/county transfer tax & title/escrow custom lookup defaults (`rates_and_costs`),
//!      editable `2.5%` default buyer-agent commission, running inline-editable summary after
//!      every answer, and non-binding printable Offer Summary for buyer & agent review.

use crate::db::ListingRow;
use crate::property::facts::is_synthetic_comp_value;
use crate::property::mls::classify_asset_class;
use crate::registry::rates_and_costs::{estimate_transfer_tax_schedule, lookup_who_pays};
use crate::shared::{fmt_usd, haversine_mi, round_n};
use chrono::{NaiveDate, Utc};
use html_escape::encode_text;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const COPILOT_STANDING_DISCLAIMER: &str = "Informational research only — not legal advice, tax advice, real-estate brokerage advice, a home inspection, or a licensed real-estate appraisal.";

pub const OFFER_SUMMARY_DISCLAIMER: &str = "INFORMATIONAL OFFER SUMMARY FOR BUYER & AGENT REVIEW ONLY — THIS IS NOT A SIGNED OR BINDING PURCHASE CONTRACT, LEGAL ADVICE, OR A LICENSED APPRAISAL.";

pub const POST_NAR_BUYER_BROKER_NOTICE: &str = "Post-NAR Settlement Notice: Buyer-broker compensation is 100% negotiable, is never fixed by law or any MLS/association, and MLS-participating agents working with a buyer generally require a written Buyer-Broker Representation Agreement specifying compensation terms before touring a home. Any seller-paid buyer-broker compensation or concession must be explicitly negotiated in the purchase contract.";

pub const TRANSFER_TAX_CUSTOMARINESS_DISCLAIMER: &str = "State, County & City Transfer-Tax Customariness Disclaimer: Pre-filled transfer-tax schedules and buyer/seller closing-fee splits reflect general regional custom for informational modeling only, not statutory mandates. Actual documentary/conveyance tax rates (including tiered municipal measures and exemptions) and fee allocations are negotiable in the purchase contract and must be verified with your escrow officer, licensed broker, or real-estate/tax attorney.";

/// Normalizes an optional assistant role into `"buyer"`, `"seller"`, `"agent"`, or `"unset"`.
pub fn normalize_copilot_role(raw: Option<&str>) -> &'static str {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "buyer" => "buyer",
        "seller" => "seller",
        "agent" | "broker" | "realtor" => "agent",
        _ => "unset",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeTypeBucket {
    SingleFamily,
    Condo,
    Townhouse,
    MultiFamily,
    Land,
    Commercial,
    Unknown,
}

impl HomeTypeBucket {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::SingleFamily => "Single-Family",
            Self::Condo => "Condo / Co-op",
            Self::Townhouse => "Townhouse",
            Self::MultiFamily => "Multi-Family (2–4 Unit)",
            Self::Land => "Land / Lot",
            Self::Commercial => "Commercial",
            Self::Unknown => "Residential",
        }
    }
}

pub fn normalize_home_type_bucket(raw: Option<&str>) -> HomeTypeBucket {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return HomeTypeBucket::Unknown;
    };
    let lc = s.to_ascii_lowercase();
    if lc.contains("town") || lc.contains("row") {
        HomeTypeBucket::Townhouse
    } else if lc.contains("condo")
        || lc.contains("co-op")
        || lc.contains("coop")
        || lc.contains("apart")
        || lc.contains("flat")
    {
        HomeTypeBucket::Condo
    } else if lc.contains("multi")
        || lc.contains("duplex")
        || lc.contains("triplex")
        || lc.contains("fourplex")
        || lc.contains("quad")
        || lc.contains("2-4")
    {
        HomeTypeBucket::MultiFamily
    } else if lc == "land" || lc.contains("vacant") || lc.contains("lot") {
        HomeTypeBucket::Land
    } else if lc.contains("commercial")
        || lc.contains("retail")
        || lc.contains("office")
        || lc.contains("industrial")
    {
        HomeTypeBucket::Commercial
    } else if lc.contains("single")
        || lc.contains("sfr")
        || lc.contains("detached")
        || lc == "house"
        || lc == "residential"
    {
        HomeTypeBucket::SingleFamily
    } else if let Some(ac) = classify_asset_class(Some(s)) {
        match ac.as_str() {
            "sfr" => HomeTypeBucket::SingleFamily,
            "condo" => HomeTypeBucket::Condo,
            "townhouse" => HomeTypeBucket::Townhouse,
            "multifamily" => HomeTypeBucket::MultiFamily,
            "commercial_land" | "agricultural_farm" => HomeTypeBucket::Land,
            "cre" => HomeTypeBucket::Commercial,
            _ => HomeTypeBucket::Unknown,
        }
    } else {
        HomeTypeBucket::Unknown
    }
}

fn types_compatible(subj: HomeTypeBucket, cand: HomeTypeBucket) -> bool {
    if subj == HomeTypeBucket::Unknown || cand == HomeTypeBucket::Unknown {
        return true;
    }
    subj == cand
}

fn norm_addr_dedup_key(addr: &str) -> String {
    let head = addr.split(',').next().unwrap_or(addr).trim();
    head.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn parse_iso_date_prefix(raw: &str) -> Option<NaiveDate> {
    let t = raw.trim();
    if t.len() < 10 {
        return None;
    }
    NaiveDate::parse_from_str(&t[..10], "%Y-%m-%d").ok()
}

fn resolve_reference_date(report: Option<&Value>) -> NaiveDate {
    if let Some(rep) = report {
        for ptr in ["/facts/asOf", "/facts/valuationAsOf", "/meta/generatedAt"] {
            if let Some(ds) = rep.pointer(ptr).and_then(|v| v.as_str()) {
                if let Some(dt) = parse_iso_date_prefix(ds) {
                    return dt;
                }
            }
        }
    }
    Utc::now().date_naive()
}

fn opt_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    val.as_f64()
        .or_else(|| val.as_i64().map(|i| i as f64))
        .or_else(|| {
            val.as_str()
                .and_then(|s| s.trim().replace(',', "").parse::<f64>().ok())
        })
        .filter(|x| x.is_finite())
}

fn opt_i64(v: Option<&Value>) -> Option<i64> {
    opt_f64(v).map(|x| x.round() as i64)
}

fn opt_pos_i64(v: Option<&Value>) -> Option<i64> {
    opt_i64(v).filter(|&x| x > 0)
}

/// Resolves normalized listing status (`"for_sale" | "pending" | "sold" | "off_market"`)
/// from a merged report / facts / DB listing row.
pub fn resolve_subject_listing_status(
    facts: &Value,
    valuation: Option<&Value>,
    db_row: Option<&ListingRow>,
) -> (&'static str, &'static str) {
    let raw_mls_status = facts
        .pointer("/mlsListing/status")
        .or_else(|| facts.get("listingStatus"))
        .or_else(|| facts.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if !raw_mls_status.is_empty() {
        if raw_mls_status.contains("pending")
            || raw_mls_status.contains("under contract")
            || raw_mls_status.contains("active under contract")
            || raw_mls_status.contains("contingent")
        {
            return ("pending", "Pending / Under Contract");
        }
        if raw_mls_status == "active"
            || raw_mls_status == "for sale"
            || raw_mls_status == "for_sale"
            || raw_mls_status.contains("coming soon")
            || (raw_mls_status.contains("active") && !raw_mls_status.contains("inactive"))
        {
            return ("for_sale", "Actively For Sale");
        }
        if raw_mls_status.contains("sold") || raw_mls_status.contains("closed") {
            return ("sold", "Recently Sold — Not Active");
        }
        if raw_mls_status.contains("off")
            || raw_mls_status.contains("inactive")
            || raw_mls_status.contains("expired")
            || raw_mls_status.contains("withdrawn")
            || raw_mls_status.contains("canceled")
        {
            return ("off_market", "Not Currently Listed");
        }
    }

    if let Some(row) = db_row {
        match row.status.as_str() {
            "for_sale" => return ("for_sale", "Actively For Sale"),
            "pending" => return ("pending", "Pending / Under Contract"),
            "sold" => return ("sold", "Recently Sold — Not Active"),
            "off_market" => return ("off_market", "Not Currently Listed"),
            _ => {}
        }
    }

    let explicit_for_sale = facts
        .get("forSale")
        .and_then(|v| v.as_bool())
        .or_else(|| valuation.and_then(|v| v.get("forSale")).and_then(|x| x.as_bool()));
    if explicit_for_sale == Some(true) {
        return ("for_sale", "Actively For Sale");
    }
    if opt_pos_i64(facts.get("listPrice").or_else(|| facts.pointer("/mlsListing/listPrice")))
        .is_some()
        && explicit_for_sale != Some(false)
    {
        return ("for_sale", "Actively For Sale");
    }

    ("off_market", "Not Currently Listed")
}

/// Builds the Stage-0 Property Status Card (`D -> E` or `D -> F`).
pub fn build_property_status_card(
    slug: &str,
    geo: &Value,
    facts: &Value,
    valuation: Option<&Value>,
    db_row: Option<&ListingRow>,
) -> Value {
    let (status_code, status_label) = resolve_subject_listing_status(facts, valuation, db_row);
    let on_market = status_code == "for_sale";

    let matched_address = geo
        .get("matchedAddress")
        .or_else(|| facts.get("address"))
        .and_then(|v| v.as_str())
        .or_else(|| db_row.map(|r| r.address.as_str()))
        .unwrap_or("")
        .to_string();

    let raw_ptype = facts
        .get("propertyType")
        .and_then(|v| v.as_str())
        .or_else(|| db_row.and_then(|r| r.property_type.as_deref()));
    let bucket = normalize_home_type_bucket(raw_ptype);
    let property_type_label = raw_ptype
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| bucket.as_label());

    let sqft = opt_pos_i64(facts.get("sqft")).or_else(|| db_row.and_then(|r| r.sqft).filter(|&x| x > 0));
    let lot_sqft = opt_pos_i64(facts.get("lotSqft"))
        .or_else(|| db_row.and_then(|r| r.lot_sqft).filter(|&x| x > 0));
    let beds = opt_f64(facts.get("beds"))
        .filter(|&x| x > 0.0)
        .or_else(|| db_row.and_then(|r| r.beds).filter(|&x| x > 0.0));
    let baths = opt_f64(facts.get("baths"))
        .filter(|&x| x > 0.0)
        .or_else(|| db_row.and_then(|r| r.baths).filter(|&x| x > 0.0));
    let year_built = opt_pos_i64(facts.get("yearBuilt"))
        .filter(|&y| y > 1700)
        .or_else(|| db_row.and_then(|r| r.year_built).filter(|&y| y > 1700));
    let hoa_monthly = opt_i64(facts.get("hoaMonthly"))
        .filter(|&h| h >= 0)
        .or_else(|| db_row.and_then(|r| r.hoa_monthly).filter(|&h| h >= 0));

    let list_price = opt_pos_i64(
        facts
            .get("listPrice")
            .or_else(|| facts.pointer("/mlsListing/listPrice")),
    )
    .or_else(|| {
        db_row.and_then(|r| {
            r.list_price
                .filter(|&p| p > 0)
                .or_else(|| if r.status == "for_sale" { r.price.filter(|&p| p > 0) } else { None })
        })
    });

    let last_sale_price = opt_pos_i64(facts.get("lastSalePrice"))
        .or_else(|| db_row.and_then(|r| r.sold_price).filter(|&p| p > 0));
    let last_sale_date = facts
        .get("lastSaleDate")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| db_row.and_then(|r| r.sold_date.clone()));
    let days_on_market = opt_i64(
        facts
            .pointer("/mlsListing/daysOnMarket")
            .or_else(|| facts.get("daysOnMarket")),
    )
    .filter(|&d| d >= 0)
    .or_else(|| db_row.and_then(|r| r.days_on_market).filter(|&d| d >= 0));

    let status_notice = if on_market {
        Value::Null
    } else if status_code == "pending" {
        json!("Not currently available as an active listing — this property is Pending / Under Contract. Basic property details from public & MLS records are shown below.")
    } else {
        json!("Not currently listed for sale on the active market. Basic property information from public records is shown below.")
    };

    let action_menu = if on_market {
        json!([
            {
                "id": "comps_pricing",
                "number": 1,
                "title": "Comps & Pricing",
                "subtitle": "Market comps table, comp-based price range, similar active listings & pending sales"
            },
            {
                "id": "disclosures",
                "number": 2,
                "title": "Disclosures Reader",
                "subtitle": "Upload .zip / PDFs (Inspection, Termite, Prelim Title, NHD, HOA, Permits) by priority"
            },
            {
                "id": "offer_builder",
                "number": 3,
                "title": "Offer Builder",
                "subtitle": "6-step guided offer form with editable commission, local tax/fee splits & live summary"
            }
        ])
    } else {
        json!([])
    };

    let material_request_mailto = build_copilot_mailto_request(geo, facts);

    json!({
        "slug": slug,
        "matchedAddress": matched_address,
        "city": geo.get("city").cloned().or_else(|| db_row.and_then(|r| r.city.as_deref()).map(|s| json!(s))),
        "state": geo.get("state").cloned().or_else(|| db_row.and_then(|r| r.state.as_deref()).map(|s| json!(s))),
        "zip": geo.get("zip").cloned().or_else(|| db_row.map(|r| json!(r.zip))),
        "county": geo.get("county"),
        "countyFips": geo.get("countyFips"),
        "lat": geo.get("lat").cloned().or_else(|| db_row.and_then(|r| r.lat).map(|v| json!(v))),
        "lon": geo.get("lon").cloned().or_else(|| db_row.and_then(|r| r.lon).map(|v| json!(v))),
        "listingStatus": status_code,
        "listingStatusLabel": status_label,
        "onMarket": on_market,
        "statusNotice": status_notice,
        "basicInfo": {
            "propertyType": property_type_label,
            "propertyTypeBucket": bucket.as_label(),
            "beds": beds,
            "baths": baths,
            "sqft": sqft,
            "lotSqft": lot_sqft,
            "yearBuilt": year_built,
            "hoaMonthly": hoa_monthly,
            "listPrice": list_price,
            "daysOnMarket": days_on_market,
            "lastSalePrice": last_sale_price,
            "lastSaleDate": last_sale_date,
        },
        "actionMenu": action_menu,
        "materialRequestMailto": material_request_mailto,
        "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
    })
}

/// Builds a prefilled `mailto:` material-request payload for requesting the 6 priority disclosure
/// documents from the listing agent or buyer's agent (zero server SMTP required).
pub fn build_copilot_mailto_request(geo: &Value, facts: &Value) -> Value {
    let display_address = geo
        .get("displayAddress")
        .or_else(|| geo.get("matchedAddress"))
        .or_else(|| facts.get("address"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Subject Property");
    let ctx = json!({
        "geo": geo,
        "facts": facts,
    });
    build_material_request_mailto(Some(&ctx), display_address)
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

fn sanitize_mailto_email(raw: Option<&str>) -> String {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return String::new();
    };
    crate::search::validate_email_input(s).unwrap_or_default()
}

fn extract_report_str(
    report_opt: Option<&Value>,
    ptrs: &[&str],
    max_chars: usize,
) -> Option<String> {
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

fn build_material_request_mailto(report_opt: Option<&Value>, display_address: &str) -> Value {
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

fn non_empty_str_ptr<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn insert_if_some(map: &mut serde_json::Map<String, Value>, key: &str, val: Option<Value>) {
    if let Some(v) = val.filter(|x| !x.is_null()) {
        map.insert(key.to_string(), v);
    }
}

/// Mode 3 — Always computes open-government-data disclosure equivalents for every property
/// without ever inventing missing figures (`AGENTS.md` Data Honesty).
pub fn build_public_record_equivalents(
    slug: &str,
    geo: &Value,
    facts: &Value,
    full_report_opt: Option<&Value>,
) -> Value {
    // 1. Natural Hazards & Environmental Baseline (FEMA / USGS / CAL FIRE / EPA)
    let hazards = full_report_opt
        .and_then(|r| r.pointer("/modules/hazards/data").or_else(|| r.get("hazards")));
    let env_mod = full_report_opt.and_then(|r| {
        r.pointer("/modules/environment/data")
            .or_else(|| r.get("environment"))
    });
    let mut hz_metrics = serde_json::Map::new();
    let mut hz_parts: Vec<String> = Vec::new();
    if let Some(hz) = hazards {
        if let Some(zone) = non_empty_str_ptr(hz, "/flood/zone") {
            let sfha = hz.pointer("/flood/sfha").and_then(Value::as_bool);
            hz_metrics.insert("floodZone".into(), json!(zone));
            if let Some(in_sfha) = sfha {
                hz_metrics.insert("inSfha".into(), json!(in_sfha));
                hz_parts.push(format!(
                    "FEMA Flood Zone {zone} ({})",
                    if in_sfha { "High-Risk SFHA" } else { "Non-SFHA" }
                ));
            } else {
                hz_parts.push(format!("FEMA Flood Zone {zone}"));
            }
        }
        if let Some(sdc) = non_empty_str_ptr(hz, "/earthquake/sdc") {
            hz_metrics.insert("seismicDesignCategory".into(), json!(sdc));
            let pga = hz.pointer("/earthquake/pga").and_then(Value::as_f64);
            if let Some(p) = pga {
                hz_metrics.insert("seismicPga".into(), json!(round_n(p, 2)));
                hz_parts.push(format!("USGS Seismic SDC {sdc} (PGA {:.2}g)", p));
            } else {
                hz_parts.push(format!("USGS Seismic SDC {sdc}"));
            }
        }
        if let Some(fname) = non_empty_str_ptr(hz, "/earthquake/nearestFaultName") {
            hz_metrics.insert("nearestFaultName".into(), json!(fname));
            if let Some(fkm) = hz.pointer("/earthquake/nearestFaultKm").and_then(Value::as_f64) {
                hz_metrics.insert("nearestFaultKm".into(), json!(round_n(fkm, 1)));
                hz_parts.push(format!("Nearest fault: {fname} ({:.1} km)", fkm));
            }
        }
        if let Some(wf) = non_empty_str_ptr(hz, "/wildfire/zone")
            .or_else(|| non_empty_str_ptr(hz, "/wildfire/whpClass"))
        {
            hz_metrics.insert("wildfireZone".into(), json!(wf));
            hz_parts.push(format!("Wildfire class: {wf}"));
        }
        if let Some(rz) = hz.pointer("/radon/epaZone").and_then(Value::as_i64) {
            hz_metrics.insert("radonEpaZone".into(), json!(rz));
            hz_parts.push(format!("EPA Radon Zone {rz}"));
        }
        for (label, ptr) in [
            ("alquistPriolo", "/other/alquistPriolo/inZone"),
            ("liquefaction", "/other/liquefaction/inZone"),
            ("landslide", "/other/landslide/inZone"),
            ("damInundation", "/other/damInundation/inZone"),
        ] {
            if let Some(b) = hz.pointer(ptr).and_then(Value::as_bool) {
                hz_metrics.insert(label.into(), json!(b));
            }
        }
    }
    if let Some(em) = env_mod {
        insert_if_some(
            &mut hz_metrics,
            "superfundWithin3Mi",
            em.pointer("/sites/superfundWithin3Mi").cloned(),
        );
        insert_if_some(
            &mut hz_metrics,
            "openLustWithinHalfMi",
            em.pointer("/sites/openLustWithinHalfMi").cloned(),
        );
        if let Some(aqi) = em.pointer("/air/meanAqi").and_then(Value::as_f64) {
            hz_metrics.insert("meanAqi30d".into(), json!(aqi.round() as i64));
            hz_parts.push(format!("30-day mean US AQI: {}", aqi.round() as i64));
        }
        insert_if_some(
            &mut hz_metrics,
            "waterHealthViolations5yr",
            em.pointer("/water/healthViolations5yr").cloned(),
        );
    }
    let state_str = geo.get("state").and_then(Value::as_str).unwrap_or("").trim();
    let county_str = geo.get("county").and_then(Value::as_str).unwrap_or("").trim();
    let hz_summary = if !hz_parts.is_empty() {
        hz_parts.join(" · ")
    } else {
        let loc = [county_str, state_str]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if loc.is_empty() {
            "FEMA flood, USGS seismic, wildfire, and EPA environmental layers have not been run for this session yet — generate the full property report to load public GIS hazard data.".to_string()
        } else {
            format!("Public GIS hazard screening for {loc} has not been fetched in this lightweight lookup — open the full property report to run FEMA NFHL, USGS seismic, wildfire, and EPA checks.")
        }
    };

    // 2. County Assessor, Tax & Deed Sale History
    let mut tax_metrics = serde_json::Map::new();
    let mut tax_parts: Vec<String> = Vec::new();
    if let Some(apn) = facts
        .get("apn")
        .or_else(|| facts.get("parcelNumber"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        tax_metrics.insert("apn".into(), json!(apn));
        tax_parts.push(format!("APN {apn}"));
    }
    if let Some(rate) = opt_f64(facts.get("taxRate")).filter(|&r| r > 0.0) {
        tax_metrics.insert("taxRate".into(), json!(round_n(rate, 5)));
    }
    let assessed = opt_pos_i64(facts.pointer("/officialAssessment/totalAssessedValue"))
        .or_else(|| {
            facts
                .pointer("/taxHistory/0/assessedValue")
                .or_else(|| facts.pointer("/taxHistory/0/assessed"))
                .and_then(|v| opt_pos_i64(Some(v)))
        });
    if let Some(av) = assessed {
        tax_metrics.insert("assessedValueUsd".into(), json!(av));
        tax_parts.push(format!("Assessed value {}", fmt_usd(av as f64)));
    }
    let annual_tax = opt_pos_i64(facts.pointer("/officialAssessment/annualTax")).or_else(|| {
        facts
            .pointer("/taxHistory/0/taxAmount")
            .or_else(|| facts.pointer("/taxHistory/0/tax"))
            .and_then(|v| opt_pos_i64(Some(v)))
    });
    if let Some(tx) = annual_tax {
        tax_metrics.insert("annualTaxUsd".into(), json!(tx));
        tax_parts.push(format!("Annual tax {}", fmt_usd(tx as f64)));
    }
    let last_sale_price = opt_pos_i64(facts.pointer("/lastSale/price"))
        .or_else(|| opt_pos_i64(facts.get("lastSalePrice")));
    let last_sale_date = non_empty_str_ptr(facts, "/lastSale/date")
        .or_else(|| non_empty_str_ptr(facts, "/lastSaleDate"));
    if let Some(sp) = last_sale_price {
        tax_metrics.insert("lastSalePriceUsd".into(), json!(sp));
        if let Some(sd) = last_sale_date {
            tax_metrics.insert("lastSaleDate".into(), json!(sd));
            tax_parts.push(format!("Last recorded sale {} on {}", fmt_usd(sp as f64), sd));
        } else {
            tax_parts.push(format!("Last recorded sale {}", fmt_usd(sp as f64)));
        }
    }
    if let Some(sh) = facts.get("saleHistory").and_then(Value::as_array) {
        if !sh.is_empty() {
            tax_metrics.insert("saleHistoryCount".into(), json!(sh.len()));
            tax_metrics.insert(
                "recentTransfers".into(),
                Value::Array(sh.iter().take(3).cloned().collect()),
            );
        }
    }
    let tax_summary = if !tax_parts.is_empty() {
        tax_parts.join(" · ")
    } else {
        "County assessor tax roll & deed transfer records were not present in this lightweight lookup; verify parcel APN, tax bill, and recorded deeds directly with the county assessor/recorder.".to_string()
    };

    // 3. Building Permits, Year Built & Zoning Context
    let mut perm_metrics = serde_json::Map::new();
    let mut perm_parts: Vec<String> = Vec::new();
    if let Some(yb) = opt_pos_i64(facts.get("yearBuilt")).filter(|&y| y > 1700) {
        perm_metrics.insert("yearBuilt".into(), json!(yb));
        perm_parts.push(format!("Built {yb}"));
    }
    if let Some(sq) = opt_pos_i64(facts.get("sqft")) {
        perm_metrics.insert("livingSqft".into(), json!(sq));
        perm_parts.push(format!("{sq} sq ft living area"));
    }
    if let Some(lsq) = opt_pos_i64(facts.get("lotSqft")) {
        perm_metrics.insert("lotSqft".into(), json!(lsq));
    }
    if let Some(zn) = non_empty_str_ptr(facts, "/zoning") {
        perm_metrics.insert("zoning".into(), json!(zn));
        perm_parts.push(format!("Zoning {zn}"));
    }
    let permits_arr = facts
        .get("permits")
        .and_then(Value::as_array)
        .or_else(|| {
            full_report_opt
                .and_then(|r| r.pointer("/condition/permits"))
                .and_then(Value::as_array)
        });
    if let Some(arr) = permits_arr {
        perm_metrics.insert("permitRecordCount".into(), json!(arr.len()));
        if !arr.is_empty() {
            perm_metrics.insert(
                "recentPermits".into(),
                Value::Array(arr.iter().take(5).cloned().collect()),
            );
            perm_parts.push(format!("{} public building permit record(s) found", arr.len()));
        } else {
            perm_parts.push("0 public permit records returned in parcel feed".to_string());
        }
    }
    let perm_summary = if !perm_parts.is_empty() {
        perm_parts.join(" · ")
    } else {
        "Public permit and zoning records were not present for this parcel; verify finaled permits directly with the municipal building department.".to_string()
    };

    // 4. HOA & Special Tax Districts
    let mut hoa_metrics = serde_json::Map::new();
    let mut hoa_parts: Vec<String> = Vec::new();
    let hoa_monthly = opt_i64(
        facts
            .get("hoaMonthly")
            .or_else(|| full_report_opt.and_then(|r| r.pointer("/hoaProfile/monthlyFeeUsd"))),
    )
    .filter(|&h| h >= 0);
    if let Some(hm) = hoa_monthly {
        hoa_metrics.insert("hoaMonthlyUsd".into(), json!(hm));
        hoa_parts.push(format!("Public/listing HOA fee: {}/mo", fmt_usd(hm as f64)));
    }
    if let Some(has_hoa) = full_report_opt
        .and_then(|r| r.pointer("/hoaProfile/hasHoa"))
        .and_then(Value::as_bool)
    {
        hoa_metrics.insert("hasHoa".into(), json!(has_hoa));
    }
    let mello_annual = opt_f64(facts.get("melloRoosAnnual")).or_else(|| {
        full_report_opt.and_then(|r| {
            opt_f64(r.pointer("/buyerDiligence/specialDistricts/melloRoosCfd/annualUsd"))
        })
    });
    if let Some(mr) = mello_annual.filter(|&x| x >= 0.0) {
        hoa_metrics.insert("melloRoosAnnualUsd".into(), json!(mr.round() as i64));
        hoa_parts.push(format!("Mello-Roos / CFD: {}/yr", fmt_usd(mr)));
    }
    if let Some(sd_mo) = full_report_opt
        .and_then(|r| opt_f64(r.pointer("/buyerDiligence/ongoingCosts/specialDistrictsMonthlyUsd")))
        .filter(|&x| x > 0.0)
    {
        hoa_metrics.insert(
            "specialDistrictsMonthlyUsd".into(),
            json!(sd_mo.round() as i64),
        );
    }
    if !state_str.is_empty() {
        let tax_info = decompose_jurisdiction_transfer_taxes(geo, 0.0, 0.0);
        hoa_metrics.insert(
            "transferTaxJurisdiction".into(),
            json!(tax_info.jurisdiction_label),
        );
        if !tax_info.custom_note.is_empty() {
            hoa_metrics.insert("transferTaxCustomNote".into(), json!(tax_info.custom_note));
        }
        hoa_parts.push(format!("Transfer tax schedule: {}", tax_info.jurisdiction_label));
    }
    let hoa_summary = if !hoa_parts.is_empty() {
        hoa_parts.join(" · ")
    } else {
        "Public HOA & special-assessment data not yet populated; request HOA resale package & county tax bill breakdown.".to_string()
    };

    let items = vec![
        json!({
            "id": "natural_hazards",
            "priorityCategory": 4,
            "title": "Public GIS Natural Hazard & Environmental Baseline",
            "source": "FEMA NFHL / USGS / CAL FIRE / EPA",
            "status": if hazards.is_some() { "available" } else { "pending_full_report" },
            "summary": hz_summary,
            "highlights": hz_parts,
            "metrics": hz_metrics,
            "limitations": "Public GIS layers do not replace statutory third-party NHD disclosures or site-specific geotechnical/environmental Phase I reports.",
        }),
        json!({
            "id": "county_tax_deed",
            "priorityCategory": 3,
            "title": "County Assessor, Property Tax & Deed History",
            "source": "County Assessor & Recorder Public Roll",
            "status": if !tax_parts.is_empty() { "available" } else { "partial_facts_only" },
            "summary": tax_summary,
            "highlights": tax_parts,
            "metrics": tax_metrics,
            "limitations": "Assessor rolls exclude unrecorded easements, Schedule B title exceptions, mechanics liens, and solar UCC-1 filings — review a Preliminary Title Report.",
        }),
        json!({
            "id": "permits_zoning",
            "priorityCategory": 6,
            "title": "Building Permits, Year Built & Zoning Context",
            "source": "Municipal Building Dept / County Parcel Roll",
            "status": if !perm_parts.is_empty() { "available" } else { "partial_facts_only" },
            "summary": perm_summary,
            "highlights": perm_parts,
            "metrics": perm_metrics,
            "limitations": "Open-data permit feeds may lag or omit historic permits; confirm finaled sign-offs directly with the local building department.",
        }),
        json!({
            "id": "hoa_special_tax",
            "priorityCategory": 5,
            "title": "HOA & Special Tax District Baseline",
            "source": "Parcel Tax Roll / Jurisdiction Fee Schedule",
            "status": if !hoa_parts.is_empty() { "available" } else { "partial_facts_only" },
            "summary": hoa_summary,
            "highlights": hoa_parts,
            "metrics": hoa_metrics,
            "limitations": "HOA reserve funding %, CC&Rs, minutes, and pending special assessments require the association's private financial & governance packet.",
        }),
    ];

    json!({
        "available": true,
        "slug": slug,
        "hasFullReportData": full_report_opt.is_some(),
        "disclaimer": "Open-government and public-record context only — provides baseline diligence when private seller disclosures are unavailable, and is NOT a substitute for a licensed home inspection, Preliminary Title Report, statutory NHD, or HOA resale packet.",
        "items": items,
    })
}

// =================================================================================================
// Function 1: Market Comps & Non-Linear Follow-Ups (Price Estimate, Similar Active, Sold/Pending)
// =================================================================================================

fn is_distressed_transfer(c: &Value) -> bool {
    let combined = format!(
        "{} {} {} {}",
        c.get("saleType").and_then(|v| v.as_str()).unwrap_or(""),
        c.get("dealType").and_then(|v| v.as_str()).unwrap_or(""),
        c.get("status").and_then(|v| v.as_str()).unwrap_or(""),
        c.get("remarks").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .to_ascii_lowercase();
    combined.contains("foreclos")
        || combined.contains("short sale")
        || combined.contains("reo")
        || combined.contains("auction")
        || combined.contains("quitclaim")
        || combined.contains("probate")
}

fn listing_row_to_candidate_json(row: &ListingRow, subj_lat: Option<f64>, subj_lon: Option<f64>) -> Value {
    let dist_mi = match (subj_lat, subj_lon, row.lat, row.lon) {
        (Some(sla), Some(slo), Some(cla), Some(clo)) => Some(round_n(haversine_mi(sla, slo, cla, clo), 2)),
        _ => None,
    };
    let price = if row.status == "sold" {
        row.sold_price.or(row.price)
    } else {
        row.price.or(row.list_price)
    };
    let ppsf = row.price_per_sqft.or_else(|| match (price, row.sqft) {
        (Some(p), Some(s)) if p > 0 && s > 0 => Some(((p as f64) / (s as f64)).round() as i64),
        _ => None,
    });
    json!({
        "address": row.address,
        "lat": row.lat,
        "lon": row.lon,
        "status": row.status,
        "price": price,
        "soldPrice": row.sold_price.or_else(|| if row.status == "sold" { row.price } else { None }),
        "listPrice": row.list_price.or_else(|| if row.status != "sold" { row.price } else { None }),
        "soldDate": row.sold_date,
        "listedDate": row.listed_date,
        "beds": row.beds,
        "baths": row.baths,
        "sqft": row.sqft,
        "lotSqft": row.lot_sqft,
        "yearBuilt": row.year_built,
        "propertyType": row.property_type,
        "pricePerSqft": ppsf,
        "distanceMi": dist_mi,
        "daysOnMarket": row.days_on_market,
        "source": "Licensed / Synced MLS Index",
    })
}

/// Computes Function 1's Sold Comps table (`H1`) under either `"strict"` (Appendix exact rules:
/// `90%–110% sqft`, similar age `+-15 yrs`, same property type, `<= 3 mo`, `<= 1.0 mi`) or
/// `"expanded"` (`80%–120% sqft`, `+-25 yrs`, `<= 6 mo`, `<= 2.0 mi`) mode, producing a
/// deterministic `compSetId` that all follow-ups reuse.
pub fn compute_copilot_comps(
    slug: &str,
    geo: &Value,
    facts: &Value,
    report: Option<&Value>,
    db_nearby: &[ListingRow],
    mode: &str,
) -> Value {
    let expand = mode.eq_ignore_ascii_case("expanded");
    let eff_mode = if expand { "expanded" } else { "strict" };

    let subj_addr_key = norm_addr_dedup_key(
        geo.get("matchedAddress")
            .or_else(|| facts.get("address"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let subj_lat = opt_f64(geo.get("lat"));
    let subj_lon = opt_f64(geo.get("lon"));
    let subj_sqft = opt_f64(facts.get("sqft")).filter(|&x| x > 0.0);
    let subj_yb = opt_pos_i64(facts.get("yearBuilt")).filter(|&y| y > 1700);
    let subj_bucket = normalize_home_type_bucket(facts.get("propertyType").and_then(|v| v.as_str()));
    let ref_date = resolve_reference_date(report);

    // Gather deduplicated arm's-length sold candidates
    let mut raw_candidates: Vec<Value> = Vec::new();
    let mut seen_addrs: HashSet<String> = HashSet::new();
    if !subj_addr_key.is_empty() {
        seen_addrs.insert(subj_addr_key.clone());
    }

    let mut push_candidate = |c: Value| {
        if is_synthetic_comp_value(&c) || is_distressed_transfer(&c) {
            return;
        }
        let Some(addr) = c.get("address").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
            return;
        };
        let key = norm_addr_dedup_key(addr);
        if key.is_empty() || !seen_addrs.insert(key) {
            return;
        }
        raw_candidates.push(c);
    };

    if let Some(arr) = facts.get("comps").and_then(|v| v.as_array()) {
        for c in arr {
            push_candidate(c.clone());
        }
    }
    if let Some(rep) = report {
        if let Some(arr) = rep.pointer("/valuation/comps").and_then(|v| v.as_array()) {
            for c in arr {
                push_candidate(c.clone());
            }
        }
    }
    for row in db_nearby.iter().filter(|r| r.status == "sold") {
        push_candidate(listing_row_to_candidate_json(row, subj_lat, subj_lon));
    }

    let eval_candidate = |c: &Value, is_expanded_pass: bool| -> Option<(Value, bool)> {
        let sold_price = opt_pos_i64(c.get("soldPrice").or_else(|| c.get("price")))?;
        let c_sqft = opt_f64(c.get("sqft")).filter(|&s| s > 0.0)?;
        let sold_dt_str = c.get("soldDate").and_then(|v| v.as_str()).unwrap_or("").trim();
        let sold_dt = parse_iso_date_prefix(sold_dt_str)?;
        let days_ago = (ref_date - sold_dt).num_days();
        if days_ago < 0 {
            return None;
        }

        let dist_mi = opt_f64(c.get("distanceMi")).or_else(|| {
            let cla = opt_f64(c.get("lat"))?;
            let clo = opt_f64(c.get("lon"))?;
            Some(round_n(haversine_mi(subj_lat?, subj_lon?, cla, clo), 2))
        })?;

        let c_bucket = normalize_home_type_bucket(
            c.get("propertyType")
                .or_else(|| c.get("assetClass"))
                .and_then(|v| v.as_str()),
        );
        if !types_compatible(subj_bucket, c_bucket) {
            return None;
        }

        let c_yb = opt_pos_i64(c.get("yearBuilt")).filter(|&y| y > 1700);

        let strict_sqft_ok = match subj_sqft {
            Some(ssq) => c_sqft >= ssq * 0.90 && c_sqft <= ssq * 1.10,
            None => true,
        };
        let strict_age_ok = match (subj_yb, c_yb) {
            (Some(sy), Some(cy)) => (sy - cy).abs() <= 15,
            _ => true,
        };
        let strict_ok = strict_sqft_ok && strict_age_ok && days_ago <= 92 && dist_mi <= 1.0;

        if !is_expanded_pass && !strict_ok {
            return None;
        }
        if is_expanded_pass && !strict_ok {
            let exp_sqft_ok = match subj_sqft {
                Some(ssq) => c_sqft >= ssq * 0.80 && c_sqft <= ssq * 1.20,
                None => true,
            };
            let exp_age_ok = match (subj_yb, c_yb) {
                (Some(sy), Some(cy)) => (sy - cy).abs() <= 25,
                _ => true,
            };
            if !(exp_sqft_ok && exp_age_ok && days_ago <= 183 && dist_mi <= 2.0) {
                return None;
            }
        }

        let ppsf = ((sold_price as f64) / c_sqft).round() as i64;
        let sqft_pct_of_subj = subj_sqft.map(|ssq| round_n((c_sqft / ssq) * 100.0, 1));
        Some((
            json!({
                "address": c.get("address"),
                "soldPrice": sold_price,
                "soldDate": &sold_dt_str[..sold_dt_str.len().min(10)],
                "daysAgo": days_ago,
                "sqft": c_sqft.round() as i64,
                "sqftPctOfSubject": sqft_pct_of_subj,
                "pricePerSqft": ppsf,
                "beds": opt_f64(c.get("beds")).filter(|&x| x > 0.0),
                "baths": opt_f64(c.get("baths")).filter(|&x| x > 0.0),
                "yearBuilt": c_yb,
                "propertyType": c.get("propertyType").and_then(|v| v.as_str()).unwrap_or_else(|| c_bucket.as_label()),
                "distanceMi": round_n(dist_mi, 2),
                "matchTier": if strict_ok { "strict" } else { "expanded" },
                "source": c.get("source").and_then(|v| v.as_str()).unwrap_or("Recorded / MLS Sale"),
            }),
            strict_ok,
        ))
    };

    let mut strict_rows: Vec<Value> = raw_candidates
        .iter()
        .filter_map(|c| eval_candidate(c, false).map(|(v, _)| v))
        .collect();
    strict_rows.sort_by(|a, b| {
        let da = opt_f64(a.get("distanceMi")).unwrap_or(99.0);
        let db = opt_f64(b.get("distanceMi")).unwrap_or(99.0);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    let strict_count = strict_rows.len();

    let mut selected_rows = if expand {
        let mut rows: Vec<Value> = raw_candidates
            .iter()
            .filter_map(|c| eval_candidate(c, true).map(|(v, _)| v))
            .collect();
        rows.sort_by(|a, b| {
            let ta = if a.get("matchTier").and_then(|v| v.as_str()) == Some("strict") { 0 } else { 1 };
            let tb = if b.get("matchTier").and_then(|v| v.as_str()) == Some("strict") { 0 } else { 1 };
            ta.cmp(&tb).then_with(|| {
                let da = opt_f64(a.get("distanceMi")).unwrap_or(99.0);
                let db = opt_f64(b.get("distanceMi")).unwrap_or(99.0);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        rows
    } else {
        strict_rows
    };
    selected_rows.truncate(15);

    let mut ppsf_vals: Vec<f64> = selected_rows
        .iter()
        .filter_map(|r| opt_f64(r.get("pricePerSqft")).filter(|&p| p > 0.0))
        .collect();
    ppsf_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let avg_ppsf = if ppsf_vals.is_empty() {
        None
    } else {
        Some((ppsf_vals.iter().sum::<f64>() / (ppsf_vals.len() as f64)).round() as i64)
    };
    let median_ppsf = if ppsf_vals.is_empty() {
        None
    } else {
        let mid = ppsf_vals.len() / 2;
        let m = if ppsf_vals.len() % 2 == 1 {
            ppsf_vals[mid]
        } else {
            (ppsf_vals[mid - 1] + ppsf_vals[mid]) / 2.0
        };
        Some(m.round() as i64)
    };

    // Deterministic compSetId from slug + mode + selected comp addresses & prices
    let mut hasher = Sha256::new();
    hasher.update(format!("cset_v1|{slug}|{eff_mode}|").as_bytes());
    for r in &selected_rows {
        hasher.update(
            format!(
                "{}:{};",
                r.get("address").and_then(|v| v.as_str()).unwrap_or(""),
                r.get("soldPrice").and_then(|v| v.as_i64()).unwrap_or(0)
            )
            .as_bytes(),
        );
    }
    let hex_digest = hex::encode(hasher.finalize());
    let comp_set_id = format!("cset_{}", &hex_digest[..12]);

    let can_expand = !expand && strict_count < 3;
    let filter_summary = if expand {
        "Expanded filter: 80%–120% sq ft, +-25 yrs age, same property type, sold within past 6 months, within 2.0 miles."
    } else {
        "Strict filter: 90%–110% sq ft, +-15 yrs age, same property type, sold within past 3 months, within 1.0 mile."
    };

    json!({
        "compSetId": comp_set_id,
        "slug": slug,
        "mode": eff_mode,
        "referenceDate": ref_date.to_string(),
        "filterSummary": filter_summary,
        "subjectBasis": {
            "sqft": subj_sqft.map(|s| s.round() as i64),
            "yearBuilt": subj_yb,
            "propertyType": subj_bucket.as_label(),
        },
        "strictMatchCount": strict_count,
        "totalCandidateCount": raw_candidates.len(),
        "canExpandSearch": can_expand,
        "expandPrompt": if can_expand {
            json!(format!(
                "Found {} strict comp(s) within 90%–110% sq ft, <= 3 months, and <= 1 mile. You can expand to 6 months / 2 miles for broader sample size.",
                strict_count
            ))
        } else {
            Value::Null
        },
        "avgPricePerSqft": avg_ppsf,
        "medianPricePerSqft": median_ppsf,
        "comps": selected_rows,
        "followUpOptions": [
            {
                "id": "estimate",
                "label": "Generate Estimate Price",
                "description": "Comp-based value range & confidence level reusing this exact comp set"
            },
            {
                "id": "similar_active",
                "label": "Similar Properties For Sale",
                "description": "Active listings of the same property type within +-15% sq ft and 1–2 miles"
            },
            {
                "id": "sold_or_pending",
                "label": "Recently Sold or Pending",
                "description": "Pinned sold comps alongside nearby Pending / Under-Contract listings"
            }
        ],
        "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
    })
}

/// Executes any of the three optional Function 1 follow-ups (`"estimate"`, `"similar_active"`,
/// `"sold_or_pending"`) in any order, strictly reusing `pinned_comp_set` so numbers stay consistent.
pub fn compute_copilot_followup(
    follow_up: &str,
    pinned_comp_set: &Value,
    geo: &Value,
    facts: &Value,
    db_nearby: &[ListingRow],
) -> Result<Value, String> {
    let comp_set_id = pinned_comp_set
        .get("compSetId")
        .and_then(|v| v.as_str())
        .unwrap_or("cset_session");
    let pinned_comps = pinned_comp_set
        .get("comps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let avg_ppsf = opt_f64(pinned_comp_set.get("avgPricePerSqft")).filter(|&x| x > 0.0);
    let subj_sqft = opt_f64(pinned_comp_set.pointer("/subjectBasis/sqft"))
        .or_else(|| opt_f64(facts.get("sqft")))
        .filter(|&x| x > 0.0);
    let subj_lat = opt_f64(geo.get("lat"));
    let subj_lon = opt_f64(geo.get("lon"));
    let subj_bucket = normalize_home_type_bucket(facts.get("propertyType").and_then(|v| v.as_str()));
    let subj_addr_key = norm_addr_dedup_key(
        geo.get("matchedAddress")
            .or_else(|| facts.get("address"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );

    match follow_up.trim().to_ascii_lowercase().as_str() {
        "estimate" => {
            if pinned_comps.is_empty() || subj_sqft.is_none() || avg_ppsf.is_none() {
                return Ok(json!({
                    "followUp": "estimate",
                    "compSetId": comp_set_id,
                    "reusedCompCount": pinned_comps.len(),
                    "comps": pinned_comps,
                    "avgPricePerSqft": avg_ppsf.map(|p| p.round() as i64),
                    "subjectSqft": subj_sqft.map(|s| s.round() as i64),
                    "estimateRangeUsd": Value::Null,
                    "confidence": {
                        "level": "Insufficient Data",
                        "score": 0,
                        "rationale": "At least 1 arm's-length comparable sale and verified subject sq ft are required to compute a comp-based price range."
                    },
                    "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
                }));
            }

            let ssq = subj_sqft.unwrap();
            let mean_ppsf = avg_ppsf.unwrap();
            let ppsf_list: Vec<f64> = pinned_comps
                .iter()
                .filter_map(|c| opt_f64(c.get("pricePerSqft")).filter(|&p| p > 0.0))
                .collect();
            let n = ppsf_list.len();
            let variance = if n > 1 {
                ppsf_list
                    .iter()
                    .map(|p| (p - mean_ppsf).powi(2))
                    .sum::<f64>()
                    / ((n - 1) as f64)
            } else {
                0.0
            };
            let std_dev = variance.sqrt();
            let cv = if mean_ppsf > 0.0 { std_dev / mean_ppsf } else { 0.10 };

            let strict_n = pinned_comps
                .iter()
                .filter(|c| c.get("matchTier").and_then(|v| v.as_str()) == Some("strict"))
                .count();

            let (conf_level, conf_score, min_band_pct) = if strict_n >= 4 && cv <= 0.12 {
                ("High", 86, 0.045)
            } else if strict_n >= 2 || (n >= 3 && cv <= 0.18) {
                ("Medium", 68, 0.070)
            } else {
                ("Low", 42, 0.110)
            };

            let half_spread_pct = cv.clamp(min_band_pct, 0.16);
            let mid_usd = (((mean_ppsf * ssq) / 1000.0).round() as i64) * 1000;
            let low_usd = ((((mean_ppsf * ssq) * (1.0 - half_spread_pct)) / 1000.0).round() as i64) * 1000;
            let high_usd = ((((mean_ppsf * ssq) * (1.0 + half_spread_pct)) / 1000.0).round() as i64) * 1000;

            let list_price = opt_pos_i64(
                facts
                    .get("listPrice")
                    .or_else(|| facts.pointer("/mlsListing/listPrice")),
            );
            let vs_list_pct = list_price.map(|lp| round_n(((mid_usd as f64 / lp as f64) - 1.0) * 100.0, 1));

            Ok(json!({
                "followUp": "estimate",
                "compSetId": comp_set_id,
                "reusedCompCount": n,
                "subjectSqft": ssq.round() as i64,
                "avgPricePerSqft": mean_ppsf.round() as i64,
                "medianPricePerSqft": pinned_comp_set.get("medianPricePerSqft"),
                "estimateRangeUsd": {
                    "low": low_usd,
                    "mid": mid_usd,
                    "high": high_usd,
                },
                "listPriceUsd": list_price,
                "midpointVsListPct": vs_list_pct,
                "confidence": {
                    "level": conf_level,
                    "score": conf_score,
                    "rationale": format!(
                        "Derived from {} pinned comp(s) ({} strict match) at ${}/sq ft average ({:.1}% $/sq ft dispersion) × {} sq ft.",
                        n,
                        strict_n,
                        mean_ppsf.round() as i64,
                        cv * 100.0,
                        ssq.round() as i64
                    ),
                },
                "comps": pinned_comps,
                "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
            }))
        }

        "similar_active" => {
            let mut seen: HashSet<String> = HashSet::new();
            if !subj_addr_key.is_empty() {
                seen.insert(subj_addr_key);
            }
            let mut active_candidates: Vec<Value> = Vec::new();
            if let Some(arr) = facts.get("nearbyListings").and_then(|v| v.as_array()) {
                for item in arr {
                    active_candidates.push(item.clone());
                }
            }
            for row in db_nearby.iter().filter(|r| r.status == "for_sale") {
                active_candidates.push(listing_row_to_candidate_json(row, subj_lat, subj_lon));
            }

            let mut filtered: Vec<Value> = Vec::new();
            for item in active_candidates {
                let Some(addr) = item.get("address").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let key = norm_addr_dedup_key(addr);
                if key.is_empty() || !seen.insert(key) {
                    continue;
                }
                let Some(price) = opt_pos_i64(item.get("price").or_else(|| item.get("listPrice"))) else {
                    continue;
                };
                let Some(lsq) = opt_f64(item.get("sqft")).filter(|&s| s > 0.0) else {
                    continue;
                };
                if let Some(ssq) = subj_sqft {
                    if lsq < ssq * 0.85 || lsq > ssq * 1.15 {
                        continue;
                    }
                }
                let l_bucket = normalize_home_type_bucket(
                    item.get("propertyType")
                        .or_else(|| item.get("assetClass"))
                        .and_then(|v| v.as_str()),
                );
                if !types_compatible(subj_bucket, l_bucket) {
                    continue;
                }
                let dist_mi = opt_f64(item.get("distanceMi")).or_else(|| {
                    let la = opt_f64(item.get("lat"))?;
                    let lo = opt_f64(item.get("lon"))?;
                    Some(round_n(haversine_mi(subj_lat?, subj_lon?, la, lo), 2))
                });
                let Some(d_mi) = dist_mi else {
                    continue;
                };
                if d_mi > 2.0 {
                    continue;
                }
                let ppsf = ((price as f64) / lsq).round() as i64;
                let vs_comp_ppsf_pct = avg_ppsf.map(|cp| round_n(((ppsf as f64 / cp) - 1.0) * 100.0, 1));
                filtered.push(json!({
                    "address": addr,
                    "sqft": lsq.round() as i64,
                    "beds": opt_f64(item.get("beds")).filter(|&b| b > 0.0),
                    "baths": opt_f64(item.get("baths")).filter(|&b| b > 0.0),
                    "yearBuilt": opt_pos_i64(item.get("yearBuilt")).filter(|&y| y > 1700),
                    "price": price,
                    "pricePerSqft": ppsf,
                    "distanceMi": round_n(d_mi, 2),
                    "inOneToTwoMileBand": (1.0..=2.0).contains(&d_mi),
                    "vsCompSetAvgPpsfPct": vs_comp_ppsf_pct,
                    "daysOnMarket": opt_i64(item.get("daysOnMarket")).filter(|&d| d >= 0),
                }));
            }

            filtered.sort_by(|a, b| {
                let band_a = if a.get("inOneToTwoMileBand").and_then(|v| v.as_bool()) == Some(true) { 0 } else { 1 };
                let band_b = if b.get("inOneToTwoMileBand").and_then(|v| v.as_bool()) == Some(true) { 0 } else { 1 };
                band_a.cmp(&band_b).then_with(|| {
                    let da = opt_f64(a.get("distanceMi")).unwrap_or(99.0);
                    let db = opt_f64(b.get("distanceMi")).unwrap_or(99.0);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
            });
            filtered.truncate(15);

            Ok(json!({
                "followUp": "similar_active",
                "compSetId": comp_set_id,
                "compSetAvgPricePerSqft": avg_ppsf.map(|p| p.round() as i64),
                "filterSummary": "Same property type, +-15% sq ft range (85%–115%), actively listed within 2.0 miles (1–2 mile band prioritized).",
                "activeListings": filtered,
                "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
            }))
        }

        "sold_or_pending" => {
            let mut seen: HashSet<String> = HashSet::new();
            if !subj_addr_key.is_empty() {
                seen.insert(subj_addr_key);
            }
            for sc in &pinned_comps {
                if let Some(a) = sc.get("address").and_then(|v| v.as_str()) {
                    seen.insert(norm_addr_dedup_key(a));
                }
            }

            let mut pending_raw: Vec<Value> = Vec::new();
            if let Some(arr) = facts.get("nearbyPending").and_then(|v| v.as_array()) {
                for item in arr {
                    pending_raw.push(item.clone());
                }
            }
            for row in db_nearby.iter().filter(|r| r.status == "pending") {
                pending_raw.push(listing_row_to_candidate_json(row, subj_lat, subj_lon));
            }

            let mut pending_rows: Vec<Value> = Vec::new();
            for item in pending_raw {
                let Some(addr) = item.get("address").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let key = norm_addr_dedup_key(addr);
                if key.is_empty() || !seen.insert(key) {
                    continue;
                }
                let p_bucket = normalize_home_type_bucket(
                    item.get("propertyType")
                        .or_else(|| item.get("assetClass"))
                        .and_then(|v| v.as_str()),
                );
                if !types_compatible(subj_bucket, p_bucket) {
                    continue;
                }
                let dist_mi = opt_f64(item.get("distanceMi")).or_else(|| {
                    let la = opt_f64(item.get("lat"))?;
                    let lo = opt_f64(item.get("lon"))?;
                    Some(round_n(haversine_mi(subj_lat?, subj_lon?, la, lo), 2))
                });
                if let Some(d) = dist_mi {
                    if d > 2.0 {
                        continue;
                    }
                }
                let sq = opt_f64(item.get("sqft")).filter(|&s| s > 0.0);
                if let (Some(ssq), Some(psq)) = (subj_sqft, sq) {
                    if psq < ssq * 0.80 || psq > ssq * 1.20 {
                        continue;
                    }
                }
                let price = opt_pos_i64(item.get("price").or_else(|| item.get("listPrice")));
                let ppsf = match (price, sq) {
                    (Some(p), Some(s)) if s > 0.0 => Some(((p as f64) / s).round() as i64),
                    _ => None,
                };
                pending_rows.push(json!({
                    "address": addr,
                    "status": "Pending",
                    "listPrice": price,
                    "sqft": sq.map(|s| s.round() as i64),
                    "pricePerSqft": ppsf,
                    "beds": opt_f64(item.get("beds")).filter(|&b| b > 0.0),
                    "baths": opt_f64(item.get("baths")).filter(|&b| b > 0.0),
                    "yearBuilt": opt_pos_i64(item.get("yearBuilt")).filter(|&y| y > 1700),
                    "distanceMi": dist_mi.map(|d| round_n(d, 2)),
                }));
            }
            pending_rows.sort_by(|a, b| {
                let da = opt_f64(a.get("distanceMi")).unwrap_or(99.0);
                let db = opt_f64(b.get("distanceMi")).unwrap_or(99.0);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            pending_rows.truncate(12);

            Ok(json!({
                "followUp": "sold_or_pending",
                "compSetId": comp_set_id,
                "compSetAvgPricePerSqft": avg_ppsf.map(|p| p.round() as i64),
                "recentlySold": pinned_comps,
                "pendingListings": pending_rows,
                "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
            }))
        }

        other => Err(format!(
            "Unsupported followUp '{other}'. Expected 'estimate', 'similar_active', or 'sold_or_pending'."
        )),
    }
}

// =================================================================================================
// Function 3: 6-Group Guided Offer Builder + Jurisdiction Tax/Fee Lookup + Running Editable Summary
// =================================================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferDraft {
    pub slug: String,
    pub property_address: String,
    pub current_step: u8,
    pub completed_steps: Vec<u8>,

    // Group 1: Offer Price
    pub offer_price_usd: Option<i64>,

    // Group 2: Downpayment & Loan Split
    pub financing_type: String, // "conventional" | "fha" | "va" | "cash"
    pub down_payment_pct: f64,
    pub loan_pct: f64,
    pub earnest_money_pct: f64,

    // Group 3: Close of Escrow (COE)
    pub coe_days: u16,
    pub target_close_date: Option<String>,

    // Group 4: Contingencies
    pub loan_contingency_enabled: bool,
    pub loan_contingency_days: u16,
    pub inspection_contingency_enabled: bool,
    pub inspection_contingency_days: u16,
    pub appraisal_contingency_enabled: bool,
    pub appraisal_contingency_days: u16,
    pub rent_back_enabled: bool,
    pub rent_back_days: u16,
    pub rent_back_deposit_usd: i64,
    pub rent_back_daily_rate_usd: i64,
    pub sale_of_property_contingency_enabled: bool,
    pub sale_of_property_address: String,
    pub sale_of_property_days: u16,

    // Group 5: Who Pays What (editable commission + lookup-table defaults)
    pub buyer_agent_commission_pct: f64,
    pub buyer_agent_commission_payer: String, // "seller" | "buyer" | "split_50_50"
    pub city_transfer_tax_payer: String,      // "seller" | "buyer" | "split_50_50"
    pub county_transfer_tax_payer: String,    // "seller" | "buyer" | "split_50_50"
    pub escrow_fee_payer: String,             // "seller" | "buyer" | "split_50_50"
    pub owners_title_payer: String,           // "seller" | "buyer" | "split_50_50"
    pub lenders_title_payer: String,          // "seller" | "buyer" | "split_50_50"
    pub home_warranty_enabled: bool,
    pub home_warranty_months: u16,
    pub home_warranty_max_usd: i64,
    pub home_warranty_payer: String, // "seller" | "buyer"

    // Group 6: Other Input (Seller Repairs, Credits, Included Items)
    pub seller_repairs: String,
    pub seller_credit_usd: i64,
    pub included_items: String,
    pub other_terms: String,
}

fn clean_payer(raw: Option<&str>, fallback: &str) -> String {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "buyer" => "buyer".to_string(),
        "seller" => "seller".to_string(),
        "split" | "split_50_50" | "50/50" | "50_50" | "buyer_or_split" | "negotiable" => {
            "split_50_50".to_string()
        }
        _ => fallback.to_string(),
    }
}

fn payer_label(p: &str) -> &'static str {
    match p {
        "buyer" => "Buyer pays 100%",
        "seller" => "Seller pays 100%",
        "split_50_50" => "Split 50% / 50%",
        _ => "Negotiable / Per Custom",
    }
}

fn split_amount_by_payer(total_usd: f64, payer: &str) -> (i64, i64) {
    let tot = total_usd.max(0.0);
    match payer {
        "buyer" => (tot.round() as i64, 0),
        "split_50_50" => {
            let b = (tot * 0.5).round() as i64;
            (b, (tot.round() as i64 - b).max(0))
        }
        _ => (0, tot.round() as i64),
    }
}

struct JurisdictionTaxDecomposition {
    jurisdiction_label: String,
    city_tax_total_usd: f64,
    city_default_payer: &'static str,
    county_state_tax_total_usd: f64,
    county_default_payer: &'static str,
    buyer_mortgage_recording_tax_usd: i64,
    custom_note: String,
    breakdown: Vec<String>,
}

fn decompose_jurisdiction_transfer_taxes(
    geo: &Value,
    offer_price: f64,
    loan_amount: f64,
) -> JurisdictionTaxDecomposition {
    let state = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let county = geo.get("county").and_then(|v| v.as_str()).unwrap_or("");
    let county_fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let city = geo.get("city").and_then(|v| v.as_str()).unwrap_or("");

    if offer_price <= 0.0 {
        // Probe with $1,000,000 solely to discover customary payer split, emitting $0 amounts
        let probe = estimate_transfer_tax_schedule(state, county, county_fips, city, 1_000_000.0, 800_000.0);
        let city_lc = city.to_ascii_lowercase();
        let default_city_payer = if city_lc.contains("oakland")
            || city_lc.contains("berkeley")
            || city_lc.contains("san jose")
        {
            "split_50_50"
        } else if probe.buyer_share_usd > 0.0 && probe.seller_share_usd > 0.0 {
            "split_50_50"
        } else if probe.buyer_share_usd > 0.0 {
            "buyer"
        } else {
            "seller"
        };
        let default_county_payer = if probe.seller_share_usd == 0.0 && probe.buyer_share_usd > 0.0 {
            "buyer"
        } else if state.eq_ignore_ascii_case("PA")
            || state.eq_ignore_ascii_case("MD")
            || state.eq_ignore_ascii_case("DC")
        {
            "split_50_50"
        } else {
            "seller"
        };
        return JurisdictionTaxDecomposition {
            jurisdiction_label: probe.jurisdiction,
            city_tax_total_usd: 0.0,
            city_default_payer: default_city_payer,
            county_state_tax_total_usd: 0.0,
            county_default_payer: default_county_payer,
            buyer_mortgage_recording_tax_usd: 0,
            custom_note: probe.payer_custom_note,
            breakdown: Vec::new(),
        };
    }

    let sched = estimate_transfer_tax_schedule(state, county, county_fips, city, offer_price, loan_amount);
    let mut city_total = 0.0;
    let mut county_total = 0.0;

    for line in &sched.breakdown {
        let ll = line.to_ascii_lowercase();
        let dollar_val = line
            .rsplit('$')
            .next()
            .map(|s| {
                s.chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<f64>()
                    .unwrap_or(0.0)
            })
            .unwrap_or(0.0);
        if ll.contains("mortgage recording") {
            continue;
        }
        if ll.contains("city")
            || ll.contains("conveyance")
            || ll.contains("measure e")
            || ll.contains("measure ula")
            || ll.contains("nyc rptt")
            || ll.contains("chicago")
            || ll.contains("philadelphia")
        {
            city_total += dollar_val;
        } else {
            county_total += dollar_val;
        }
    }

    if city_total == 0.0 && county_total == 0.0 && sched.total_tax_usd > 0.0 {
        county_total = sched.total_tax_usd;
    }

    let city_lc = city.to_ascii_lowercase();
    let city_default_payer = if city_total <= 0.0 {
        "seller"
    } else if city_lc.contains("oakland")
        || city_lc.contains("berkeley")
        || city_lc.contains("san jose")
    {
        "split_50_50"
    } else if city_lc.contains("chicago") && sched.buyer_share_usd > 0.0 {
        "buyer"
    } else {
        "seller"
    };

    let county_default_payer = if state.eq_ignore_ascii_case("PA")
        || state.eq_ignore_ascii_case("MD")
        || state.eq_ignore_ascii_case("DC")
    {
        "split_50_50"
    } else if sched.seller_share_usd == 0.0 && sched.buyer_share_usd > 0.0 {
        "buyer"
    } else {
        "seller"
    };

    JurisdictionTaxDecomposition {
        jurisdiction_label: sched.jurisdiction,
        city_tax_total_usd: city_total,
        city_default_payer,
        county_state_tax_total_usd: county_total,
        county_default_payer,
        buyer_mortgage_recording_tax_usd: sched.buyer_mortgage_recording_tax_usd.round() as i64,
        custom_note: sched.payer_custom_note,
        breakdown: sched.breakdown,
    }
}

/// Initializes a jurisdiction-calibrated `OfferDraft` for a property (`geo` + `facts`).
pub fn init_offer_draft(slug: &str, geo: &Value, facts: &Value) -> OfferDraft {
    let matched_address = geo
        .get("matchedAddress")
        .or_else(|| facts.get("address"))
        .and_then(|v| v.as_str())
        .unwrap_or(slug)
        .to_string();

    let list_price = opt_pos_i64(
        facts
            .get("listPrice")
            .or_else(|| facts.pointer("/mlsListing/listPrice")),
    );

    let who_pays = lookup_who_pays(geo);
    let tax_probe = decompose_jurisdiction_transfer_taxes(
        geo,
        list_price.unwrap_or(0) as f64,
        (list_price.unwrap_or(0) as f64) * 0.80,
    );

    OfferDraft {
        slug: slug.to_string(),
        property_address: matched_address,
        current_step: 1,
        completed_steps: Vec::new(),
        offer_price_usd: list_price,
        financing_type: "conventional".to_string(),
        down_payment_pct: 20.0,
        loan_pct: 80.0,
        earnest_money_pct: 3.0,
        coe_days: 30,
        target_close_date: None,
        loan_contingency_enabled: true,
        loan_contingency_days: 17,
        inspection_contingency_enabled: true,
        inspection_contingency_days: 12,
        appraisal_contingency_enabled: true,
        appraisal_contingency_days: 17,
        rent_back_enabled: false,
        rent_back_days: 0,
        rent_back_deposit_usd: 0,
        rent_back_daily_rate_usd: 0,
        sale_of_property_contingency_enabled: false,
        sale_of_property_address: String::new(),
        sale_of_property_days: 30,
        buyer_agent_commission_pct: 2.5,
        buyer_agent_commission_payer: "seller".to_string(),
        city_transfer_tax_payer: tax_probe.city_default_payer.to_string(),
        county_transfer_tax_payer: tax_probe.county_default_payer.to_string(),
        escrow_fee_payer: clean_payer(Some(who_pays.escrow_fee_payer), "split_50_50"),
        owners_title_payer: clean_payer(Some(who_pays.owners_policy_payer), "seller"),
        lenders_title_payer: clean_payer(Some(who_pays.lenders_policy_payer), "buyer"),
        home_warranty_enabled: true,
        home_warranty_months: 12,
        home_warranty_max_usd: 750,
        home_warranty_payer: "seller".to_string(),
        seller_repairs: String::new(),
        seller_credit_usd: 0,
        included_items: String::new(),
        other_terms: String::new(),
    }
}

/// Applies user step answers or inline edits onto `OfferDraft` cleanly.
pub fn apply_offer_patch(draft: &mut OfferDraft, patch: &Value) {
    if let Some(step) = patch.get("step").and_then(|v| v.as_u64()) {
        let st = (step as u8).clamp(1, 6);
        if !draft.completed_steps.contains(&st) {
            draft.completed_steps.push(st);
            draft.completed_steps.sort_unstable();
        }
        draft.current_step = (st + 1).min(6);
    }
    if let Some(cs) = patch.get("currentStep").and_then(|v| v.as_u64()) {
        draft.current_step = (cs as u8).clamp(1, 6);
    }

    // Group 1: Offer Price
    if let Some(op_val) = patch.get("offerPriceUsd") {
        if op_val.is_null() {
            draft.offer_price_usd = None;
        } else if let Some(p) = opt_pos_i64(Some(op_val)) {
            draft.offer_price_usd = Some(p.clamp(1_000, 500_000_000));
        }
    }

    // Group 2: Downpayment & Loan
    if let Some(ft) = patch.get("financingType").and_then(|v| v.as_str()) {
        let low = ft.trim().to_ascii_lowercase();
        if matches!(low.as_str(), "conventional" | "fha" | "va" | "cash") {
            draft.financing_type = low.clone();
            if low == "cash" {
                draft.down_payment_pct = 100.0;
                draft.loan_pct = 0.0;
                draft.loan_contingency_enabled = false;
                draft.appraisal_contingency_enabled = false;
            }
        }
    }
    if let Some(dp) = opt_f64(patch.get("downPaymentPct")) {
        let clamped = if draft.financing_type == "cash" {
            100.0
        } else {
            round_n(dp.clamp(0.0, 100.0), 2)
        };
        draft.down_payment_pct = clamped;
        draft.loan_pct = round_n((100.0 - clamped).max(0.0), 2);
    } else if let Some(lp) = opt_f64(patch.get("loanPct")) {
        let clamped = if draft.financing_type == "cash" {
            0.0
        } else {
            round_n(lp.clamp(0.0, 100.0), 2)
        };
        draft.loan_pct = clamped;
        draft.down_payment_pct = round_n((100.0 - clamped).max(0.0), 2);
    }
    if let Some(emd) = opt_f64(patch.get("earnestMoneyPct")) {
        draft.earnest_money_pct = round_n(emd.clamp(0.0, 25.0), 2);
    }

    // Group 3: Close of Escrow
    if let Some(days) = opt_i64(patch.get("coeDays")) {
        draft.coe_days = (days.clamp(3, 365)) as u16;
    }
    if let Some(tcd) = patch.get("targetCloseDate") {
        if tcd.is_null() {
            draft.target_close_date = None;
        } else if let Some(s) = tcd.as_str() {
            let trimmed: String = s.trim().chars().take(32).collect();
            draft.target_close_date = if trimmed.is_empty() { None } else { Some(trimmed) };
        }
    }

    // Group 4: Contingencies
    if let Some(b) = patch.get("loanContingencyEnabled").and_then(|v| v.as_bool()) {
        draft.loan_contingency_enabled = b;
    }
    if let Some(d) = opt_i64(patch.get("loanContingencyDays")) {
        draft.loan_contingency_days = d.clamp(0, 120) as u16;
    }
    if let Some(b) = patch.get("inspectionContingencyEnabled").and_then(|v| v.as_bool()) {
        draft.inspection_contingency_enabled = b;
    }
    if let Some(d) = opt_i64(patch.get("inspectionContingencyDays")) {
        draft.inspection_contingency_days = d.clamp(0, 90) as u16;
    }
    if let Some(b) = patch.get("appraisalContingencyEnabled").and_then(|v| v.as_bool()) {
        draft.appraisal_contingency_enabled = b;
    }
    if let Some(d) = opt_i64(patch.get("appraisalContingencyDays")) {
        draft.appraisal_contingency_days = d.clamp(0, 90) as u16;
    }
    if let Some(b) = patch.get("rentBackEnabled").and_then(|v| v.as_bool()) {
        draft.rent_back_enabled = b;
    }
    if let Some(d) = opt_i64(patch.get("rentBackDays")) {
        draft.rent_back_days = d.clamp(0, 180) as u16;
    }
    if let Some(dep) = opt_i64(patch.get("rentBackDepositUsd")) {
        draft.rent_back_deposit_usd = dep.clamp(0, 500_000);
    }
    if let Some(rate) = opt_i64(patch.get("rentBackDailyRateUsd")) {
        draft.rent_back_daily_rate_usd = rate.clamp(0, 25_000);
    }
    if let Some(b) = patch.get("saleOfPropertyContingencyEnabled").and_then(|v| v.as_bool()) {
        draft.sale_of_property_contingency_enabled = b;
    }
    if let Some(s) = patch.get("saleOfPropertyAddress").and_then(|v| v.as_str()) {
        draft.sale_of_property_address = s.trim().chars().take(200).collect();
    }
    if let Some(d) = opt_i64(patch.get("saleOfPropertyDays")) {
        draft.sale_of_property_days = d.clamp(0, 180) as u16;
    }

    // Group 5: Who Pays What
    if let Some(comm) = opt_f64(patch.get("buyerAgentCommissionPct")) {
        draft.buyer_agent_commission_pct = round_n(comm.clamp(0.0, 10.0), 2);
    }
    if let Some(p) = patch.get("buyerAgentCommissionPayer").and_then(|v| v.as_str()) {
        draft.buyer_agent_commission_payer = clean_payer(Some(p), &draft.buyer_agent_commission_payer);
    }
    if let Some(p) = patch.get("cityTransferTaxPayer").and_then(|v| v.as_str()) {
        draft.city_transfer_tax_payer = clean_payer(Some(p), &draft.city_transfer_tax_payer);
    }
    if let Some(p) = patch.get("countyTransferTaxPayer").and_then(|v| v.as_str()) {
        draft.county_transfer_tax_payer = clean_payer(Some(p), &draft.county_transfer_tax_payer);
    }
    if let Some(p) = patch.get("escrowFeePayer").and_then(|v| v.as_str()) {
        draft.escrow_fee_payer = clean_payer(Some(p), &draft.escrow_fee_payer);
    }
    if let Some(p) = patch.get("ownersTitlePayer").and_then(|v| v.as_str()) {
        draft.owners_title_payer = clean_payer(Some(p), &draft.owners_title_payer);
    }
    if let Some(p) = patch.get("lendersTitlePayer").and_then(|v| v.as_str()) {
        draft.lenders_title_payer = clean_payer(Some(p), &draft.lenders_title_payer);
    }
    if let Some(b) = patch.get("homeWarrantyEnabled").and_then(|v| v.as_bool()) {
        draft.home_warranty_enabled = b;
    }
    if let Some(m) = opt_i64(patch.get("homeWarrantyMonths")) {
        draft.home_warranty_months = m.clamp(0, 60) as u16;
    }
    if let Some(w) = opt_i64(patch.get("homeWarrantyMaxUsd")) {
        draft.home_warranty_max_usd = w.clamp(0, 25_000);
    }
    if let Some(p) = patch.get("homeWarrantyPayer").and_then(|v| v.as_str()) {
        let low = p.trim().to_ascii_lowercase();
        if matches!(low.as_str(), "seller" | "buyer") {
            draft.home_warranty_payer = low;
        }
    }

    // Group 6: Other Input
    if let Some(s) = patch.get("sellerRepairs").and_then(|v| v.as_str()) {
        draft.seller_repairs = s.trim().chars().take(1_500).collect();
    }
    if let Some(c) = opt_i64(patch.get("sellerCreditUsd")) {
        draft.seller_credit_usd = c.clamp(0, 5_000_000);
    }
    if let Some(s) = patch.get("includedItems").and_then(|v| v.as_str()) {
        draft.included_items = s.trim().chars().take(600).collect();
    }
    if let Some(s) = patch.get("otherTerms").and_then(|v| v.as_str()) {
        draft.other_terms = s.trim().chars().take(1_500).collect();
    }
}

/// Evaluates an `OfferDraft` against local county/city fee lookup tables and builds:
/// - `runningSummary` (live editable summary updated after every answer)
/// - `markdownSummary` & `printableHtml` (final buyer + agent review document, non-contract)
pub fn build_offer_evaluation(
    draft: &OfferDraft,
    geo: &Value,
    facts: &Value,
    pinned_estimate: Option<&Value>,
    disclosure_red_flags: Option<&[Value]>,
) -> Value {
    let offer_price_f = draft.offer_price_usd.unwrap_or(0) as f64;
    let down_payment_usd = if offer_price_f > 0.0 {
        Some(((offer_price_f * (draft.down_payment_pct / 100.0)).round()) as i64)
    } else {
        None
    };
    let loan_amount_usd = match (draft.offer_price_usd, down_payment_usd) {
        (Some(op), Some(dp)) => Some((op - dp).max(0)),
        _ => None,
    };
    let earnest_money_usd = if offer_price_f > 0.0 {
        Some(((offer_price_f * (draft.earnest_money_pct / 100.0)).round()) as i64)
    } else {
        None
    };

    let commission_total_usd = if offer_price_f > 0.0 {
        ((offer_price_f * (draft.buyer_agent_commission_pct / 100.0)).round()) as i64
    } else {
        0
    };
    let (comm_buyer_usd, comm_seller_usd) = split_amount_by_payer(
        commission_total_usd as f64,
        &draft.buyer_agent_commission_payer,
    );

    let who_pays = lookup_who_pays(geo);
    let tax_decomp = decompose_jurisdiction_transfer_taxes(
        geo,
        offer_price_f,
        loan_amount_usd.unwrap_or(0) as f64,
    );

    let (city_tax_buyer_usd, city_tax_seller_usd) =
        split_amount_by_payer(tax_decomp.city_tax_total_usd, &draft.city_transfer_tax_payer);
    let (county_tax_buyer_usd, county_tax_seller_usd) = split_amount_by_payer(
        tax_decomp.county_state_tax_total_usd,
        &draft.county_transfer_tax_payer,
    );

    let warranty_total_usd = if draft.home_warranty_enabled {
        draft.home_warranty_max_usd.max(0)
    } else {
        0
    };
    let warranty_buyer_usd = if draft.home_warranty_enabled && draft.home_warranty_payer == "buyer" {
        warranty_total_usd
    } else {
        0
    };
    let warranty_seller_usd = if draft.home_warranty_enabled && draft.home_warranty_payer == "seller" {
        warranty_total_usd
    } else {
        0
    };

    let estimated_buyer_cash_before_lender_escrow = down_payment_usd.map(|dp| {
        (dp + comm_buyer_usd
            + city_tax_buyer_usd
            + county_tax_buyer_usd
            + tax_decomp.buyer_mortgage_recording_tax_usd
            + warranty_buyer_usd
            - draft.seller_credit_usd)
            .max(0)
    });

    let mut active_contingencies: Vec<String> = Vec::new();
    if draft.loan_contingency_enabled {
        active_contingencies.push(format!("Loan ({} days)", draft.loan_contingency_days));
    } else {
        active_contingencies.push("Loan Contingency Waived".to_string());
    }
    if draft.inspection_contingency_enabled {
        active_contingencies.push(format!(
            "Inspection ({} days)",
            draft.inspection_contingency_days
        ));
    } else {
        active_contingencies.push("Inspection Contingency Waived".to_string());
    }
    if draft.appraisal_contingency_enabled {
        active_contingencies.push(format!(
            "Appraisal ({} days)",
            draft.appraisal_contingency_days
        ));
    } else {
        active_contingencies.push("Appraisal Contingency Waived".to_string());
    }
    if draft.rent_back_enabled && draft.rent_back_days > 0 {
        active_contingencies.push(format!(
            "Seller Rent-Back ({} days, ${} deposit, ${}/day)",
            draft.rent_back_days, draft.rent_back_deposit_usd, draft.rent_back_daily_rate_usd
        ));
    }
    if draft.sale_of_property_contingency_enabled {
        let addr_part = if draft.sale_of_property_address.is_empty() {
            "Buyer's existing property".to_string()
        } else {
            draft.sale_of_property_address.clone()
        };
        active_contingencies.push(format!(
            "Contingent on Sale of {} ({} days)",
            addr_part, draft.sale_of_property_days
        ));
    }

    let list_price = opt_pos_i64(
        facts
            .get("listPrice")
            .or_else(|| facts.pointer("/mlsListing/listPrice")),
    );

    let price_str = draft
        .offer_price_usd
        .map(|p| fmt_usd(p as f64))
        .unwrap_or_else(|| "Not entered yet".to_string());
    let dp_str = match down_payment_usd {
        Some(dp) => format!("{} ({:.1}%)", fmt_usd(dp as f64), draft.down_payment_pct),
        None => format!("{:.1}%", draft.down_payment_pct),
    };
    let loan_str = match loan_amount_usd {
        Some(ln) => format!(
            "{} ({:.1}% · {})",
            fmt_usd(ln as f64),
            draft.loan_pct,
            draft.financing_type.to_uppercase()
        ),
        None => format!("{:.1}% ({})", draft.loan_pct, draft.financing_type.to_uppercase()),
    };
    let coe_str = match &draft.target_close_date {
        Some(dt) => format!("{} days (Target: {})", draft.coe_days, dt),
        None => format!("{} days from acceptance", draft.coe_days),
    };

    let md = format!(
        "# Buyer Offer Review Summary (Non-Binding)\n\
         > **{disclaimer}**\n\n\
         **Property:** {addr}\n\
         - **1. Offer Price:** {price}\n\
         - **2. Financing Split:** Downpayment {dp} | Loan {loan} | Earnest Deposit {emd:.1}%\n\
         - **3. Close of Escrow (COE):** {coe}\n\
         - **4. Contingencies:** {cont}\n\
         - **5. Who Pays What ({region}):**\n\
           - Buyer Agent Commission ({comm_pct:.2}% = {comm_tot} — negotiable): {comm_payer}\n\
           - City Transfer Tax ({city_tot}): {city_payer}\n\
           - County/State Transfer Tax ({cnty_tot}): {cnty_payer}\n\
           - Escrow / Settlement Fee: {escrow_payer}\n\
           - Owner's Title Insurance: {own_title_payer} | Lender's Title Insurance: {lnd_title_payer}\n\
           - Home Warranty: {warranty_line}\n\
         - **6. Repairs, Credits & Other Terms:**\n\
           - Seller Closing Credit Ask: {credit}\n\
           - Repair Requests: {repairs}\n\
           - Included Items / Other Terms: {other}\n\n\
         > {post_nar}\n\n\
         > {transfer_tax_disc}\n",
        disclaimer = OFFER_SUMMARY_DISCLAIMER,
        post_nar = POST_NAR_BUYER_BROKER_NOTICE,
        transfer_tax_disc = TRANSFER_TAX_CUSTOMARINESS_DISCLAIMER,
        addr = draft.property_address,
        price = price_str,
        dp = dp_str,
        loan = loan_str,
        emd = draft.earnest_money_pct,
        coe = coe_str,
        cont = active_contingencies.join("; "),
        region = who_pays.region_name,
        comm_pct = draft.buyer_agent_commission_pct,
        comm_tot = fmt_usd(commission_total_usd as f64),
        comm_payer = payer_label(&draft.buyer_agent_commission_payer),
        city_tot = fmt_usd(tax_decomp.city_tax_total_usd),
        city_payer = payer_label(&draft.city_transfer_tax_payer),
        cnty_tot = fmt_usd(tax_decomp.county_state_tax_total_usd),
        cnty_payer = payer_label(&draft.county_transfer_tax_payer),
        escrow_payer = payer_label(&draft.escrow_fee_payer),
        own_title_payer = payer_label(&draft.owners_title_payer),
        lnd_title_payer = payer_label(&draft.lenders_title_payer),
        warranty_line = if draft.home_warranty_enabled {
            format!(
                "{} mos up to {} ({})",
                draft.home_warranty_months,
                fmt_usd(warranty_total_usd as f64),
                payer_label(&draft.home_warranty_payer)
            )
        } else {
            "Waived / None".to_string()
        },
        credit = fmt_usd(draft.seller_credit_usd as f64),
        repairs = if draft.seller_repairs.is_empty() {
            "None specified"
        } else {
            draft.seller_repairs.as_str()
        },
        other = if draft.included_items.is_empty() && draft.other_terms.is_empty() {
            "Standard contract terms".to_string()
        } else {
            format!("{} {}", draft.included_items, draft.other_terms)
                .trim()
                .to_string()
        },
    );

    let printable_html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Offer Summary — {addr}</title>
<style>
  body{{font:14px/1.55 Inter,system-ui,sans-serif;color:#111827;max-width:820px;margin:28px auto;padding:0 20px}}
  .banner{{background:#fef3c7;border:1px solid #f59e0b;color:#78350f;padding:12px 16px;border-radius:10px;font-weight:700;font-size:12.5px;margin-bottom:18px}}
  h1{{font-size:22px;margin:0 0 4px}}
  .sub{{color:#4b5563;font-size:13px;margin-bottom:18px}}
  table{{width:100%;border-collapse:collapse;margin:12px 0 20px}}
  th,td{{text-align:left;padding:9px 11px;border-bottom:1px solid #e5e7eb;vertical-align:top}}
  th{{width:34%;color:#374151;font-weight:700;background:#f9fafb}}
  @media print{{body{{margin:0}}}}
</style></head><body>
<div class="banner">{disclaimer}</div>
<h1>Buyer &amp; Agent Offer Review Summary</h1>
<div class="sub">Subject Property: <strong>{addr}</strong> · Local Custom Schedule: {region}</div>
<table>
  <tr><th>1. Offer Price</th><td><strong>{price}</strong></td></tr>
  <tr><th>2. Downpayment &amp; Loan</th><td>Downpayment: {dp}<br>Loan: {loan}<br>Earnest Money Deposit: {emd:.1}%</td></tr>
  <tr><th>3. Close of Escrow (COE)</th><td>{coe}</td></tr>
  <tr><th>4. Contingencies</th><td>{cont}</td></tr>
  <tr><th>5. Who Pays What</th><td>
    • Buyer Agent Commission ({comm_pct:.2}% = {comm_tot}, negotiable): <strong>{comm_payer}</strong><br>
    • City Transfer Tax ({city_tot}): <strong>{city_payer}</strong><br>
    • County/State Transfer Tax ({cnty_tot}): <strong>{cnty_payer}</strong><br>
    • Escrow / Settlement Fee: <strong>{escrow_payer}</strong><br>
    • Title Insurance: Owner's Policy — <strong>{own_title_payer}</strong> · Lender's Policy — <strong>{lnd_title_payer}</strong><br>
    • Home Warranty: <strong>{warranty_line}</strong>
  </td></tr>
  <tr><th>6. Seller Repairs &amp; Other Terms</th><td>
    • Seller Closing Credit Ask: <strong>{credit}</strong><br>
    • Seller Repairs: {repairs}<br>
    • Included Items / Notes: {other}
  </td></tr>
</table>
<p style="font-size:12px;color:#374151;margin:0 0 8px"><strong>Post-NAR Buyer-Broker Notice:</strong> {post_nar}</p>
<p style="font-size:12px;color:#374151;margin:0 0 8px"><strong>Transfer-Tax &amp; Local Custom Disclaimer:</strong> {transfer_tax_disc}</p>
<p style="font-size:12px;color:#6b7280">{standing}</p>
</body></html>"#,
        disclaimer = encode_text(OFFER_SUMMARY_DISCLAIMER),
        post_nar = encode_text(POST_NAR_BUYER_BROKER_NOTICE),
        transfer_tax_disc = encode_text(TRANSFER_TAX_CUSTOMARINESS_DISCLAIMER),
        addr = encode_text(&draft.property_address),
        region = encode_text(who_pays.region_name),
        price = encode_text(&price_str),
        dp = encode_text(&dp_str),
        loan = encode_text(&loan_str),
        emd = draft.earnest_money_pct,
        coe = encode_text(&coe_str),
        cont = encode_text(&active_contingencies.join(" · ")),
        comm_pct = draft.buyer_agent_commission_pct,
        comm_tot = encode_text(&fmt_usd(commission_total_usd as f64)),
        comm_payer = encode_text(payer_label(&draft.buyer_agent_commission_payer)),
        city_tot = encode_text(&fmt_usd(tax_decomp.city_tax_total_usd)),
        city_payer = encode_text(payer_label(&draft.city_transfer_tax_payer)),
        cnty_tot = encode_text(&fmt_usd(tax_decomp.county_state_tax_total_usd)),
        cnty_payer = encode_text(payer_label(&draft.county_transfer_tax_payer)),
        escrow_payer = encode_text(payer_label(&draft.escrow_fee_payer)),
        own_title_payer = encode_text(payer_label(&draft.owners_title_payer)),
        lnd_title_payer = encode_text(payer_label(&draft.lenders_title_payer)),
        warranty_line = encode_text(&if draft.home_warranty_enabled {
            format!(
                "{} months up to {} ({})",
                draft.home_warranty_months,
                fmt_usd(warranty_total_usd as f64),
                payer_label(&draft.home_warranty_payer)
            )
        } else {
            "Waived / None".to_string()
        }),
        credit = encode_text(&fmt_usd(draft.seller_credit_usd as f64)),
        repairs = encode_text(if draft.seller_repairs.is_empty() {
            "None specified"
        } else {
            draft.seller_repairs.as_str()
        }),
        other = encode_text(&if draft.included_items.is_empty() && draft.other_terms.is_empty() {
            "Standard contract terms".to_string()
        } else {
            format!("{} {}", draft.included_items, draft.other_terms)
                .trim()
                .to_string()
        }),
        standing = encode_text(COPILOT_STANDING_DISCLAIMER),
    );

    let suggested_repair_items: Vec<Value> = disclosure_red_flags
        .unwrap_or(&[])
        .iter()
        .take(6)
        .map(|rf| {
            json!({
                "title": rf.get("title"),
                "severity": rf.get("severity"),
                "sourceDoc": rf.get("sourceDoc"),
                "sourcePage": rf.get("sourcePage"),
            })
        })
        .collect();

    json!({
        "draft": draft,
        "context": {
            "listPriceUsd": list_price,
            "compEstimateRangeUsd": pinned_estimate.and_then(|e| e.get("estimateRangeUsd")).cloned().unwrap_or(Value::Null),
            "SuggestedDisclosureRedFlags": suggested_repair_items,
            "jurisdictionDefaults": {
                "regionName": who_pays.region_name,
                "closingModel": who_pays.closing_model,
                "titleEscrowCustomNote": who_pays.custom_note,
                "transferTaxJurisdiction": tax_decomp.jurisdiction_label,
                "transferTaxCustomNote": tax_decomp.custom_note,
                "transferTaxBreakdown": tax_decomp.breakdown,
                "commissionNegotiabilityNote": POST_NAR_BUYER_BROKER_NOTICE,
                "postNarSettlementNotice": POST_NAR_BUYER_BROKER_NOTICE,
                "transferTaxCustomarinessDisclaimer": TRANSFER_TAX_CUSTOMARINESS_DISCLAIMER,
            }
        },
        "runningSummary": {
            "offerPriceUsd": draft.offer_price_usd,
            "downPaymentUsd": down_payment_usd,
            "downPaymentPct": draft.down_payment_pct,
            "loanAmountUsd": loan_amount_usd,
            "loanPct": draft.loan_pct,
            "financingType": draft.financing_type,
            "earnestMoneyUsd": earnest_money_usd,
            "earnestMoneyPct": draft.earnest_money_pct,
            "coeDays": draft.coe_days,
            "targetCloseDate": draft.target_close_date,
            "activeContingencies": active_contingencies,
            "whoPaysWhat": {
                "buyerAgentCommission": {
                    "pct": draft.buyer_agent_commission_pct,
                    "totalUsd": commission_total_usd,
                    "payer": draft.buyer_agent_commission_payer,
                    "buyerShareUsd": comm_buyer_usd,
                    "sellerShareUsd": comm_seller_usd,
                },
                "cityTransferTax": {
                    "totalUsd": tax_decomp.city_tax_total_usd.round() as i64,
                    "payer": draft.city_transfer_tax_payer,
                    "buyerShareUsd": city_tax_buyer_usd,
                    "sellerShareUsd": city_tax_seller_usd,
                },
                "countyTransferTax": {
                    "totalUsd": tax_decomp.county_state_tax_total_usd.round() as i64,
                    "payer": draft.county_transfer_tax_payer,
                    "buyerShareUsd": county_tax_buyer_usd,
                    "sellerShareUsd": county_tax_seller_usd,
                },
                "buyerMortgageRecordingTaxUsd": tax_decomp.buyer_mortgage_recording_tax_usd,
                "escrowFeePayer": draft.escrow_fee_payer,
                "ownersTitlePayer": draft.owners_title_payer,
                "lendersTitlePayer": draft.lenders_title_payer,
                "homeWarranty": {
                    "enabled": draft.home_warranty_enabled,
                    "months": draft.home_warranty_months,
                    "maxUsd": warranty_total_usd,
                    "payer": draft.home_warranty_payer,
                    "buyerShareUsd": warranty_buyer_usd,
                    "sellerShareUsd": warranty_seller_usd,
                }
            },
            "sellerCreditUsd": draft.seller_credit_usd,
            "sellerRepairs": draft.seller_repairs,
            "estimatedBuyerCashBeforeLenderFeesUsd": estimated_buyer_cash_before_lender_escrow,
        },
        "finalReview": {
            "isContract": false,
            "disclaimer": OFFER_SUMMARY_DISCLAIMER,
            "standingDisclaimer": COPILOT_STANDING_DISCLAIMER,
            "postNarSettlementNotice": POST_NAR_BUYER_BROKER_NOTICE,
            "transferTaxDisclaimer": TRANSFER_TAX_CUSTOMARINESS_DISCLAIMER,
            "markdownSummary": md,
            "printableHtml": printable_html,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_card_handles_off_market_with_soft_notice() {
        let geo = json!({
            "matchedAddress": "742 Synth Evergreen Ter, Springfield, IL 62704",
            "city": "Springfield",
            "state": "IL",
            "zip": "62704",
            "lat": 39.78,
            "lon": -89.65
        });
        let facts = json!({
            "propertyType": "Single Family",
            "beds": 4,
            "baths": 2.5,
            "sqft": 2100,
            "yearBuilt": 1989,
            "forSale": false
        });
        let card = build_property_status_card("742-synth-evergreen", &geo, &facts, None, None);
        assert_eq!(card["onMarket"], false);
        assert_eq!(card["listingStatus"], "off_market");
        assert!(card["statusNotice"]
            .as_str()
            .unwrap()
            .contains("Not currently listed"));
        assert_eq!(card["basicInfo"]["sqft"], 2100);
        assert_eq!(card["basicInfo"]["listPrice"], Value::Null);
    }

    #[test]
    fn comps_strict_vs_expanded_and_consistent_followups() {
        let geo = json!({
            "matchedAddress": "100 Synth Main St, San Jose, CA 95112",
            "city": "San Jose",
            "state": "CA",
            "county": "Santa Clara",
            "countyFips": "06085",
            "lat": 37.3382,
            "lon": -121.8863
        });
        let facts = json!({
            "asOf": "2026-11-15",
            "propertyType": "Single Family",
            "sqft": 2000,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1998,
            "listPrice": 1_520_000,
            "forSale": true,
            "comps": [
                {
                    "address": "108 Synth Main St, San Jose, CA 95112",
                    "soldPrice": 1_500_000,
                    "soldDate": "2026-10-10",
                    "sqft": 2000,
                    "beds": 4,
                    "baths": 2.5,
                    "yearBuilt": 2001,
                    "propertyType": "Single Family",
                    "distanceMi": 0.25
                },
                {
                    "address": "240 Synth Oak Ave, San Jose, CA 95112",
                    "soldPrice": 1_584_000,
                    "soldDate": "2026-09-20",
                    "sqft": 2100,
                    "beds": 4,
                    "baths": 3.0,
                    "yearBuilt": 1995,
                    "propertyType": "Single Family",
                    "distanceMi": 0.60
                },
                {
                    "address": "900 Synth Farther Rd, San Jose, CA 95125",
                    "soldPrice": 1_650_000,
                    "soldDate": "2026-06-15",
                    "sqft": 2250,
                    "beds": 4,
                    "baths": 2.5,
                    "yearBuilt": 1982,
                    "propertyType": "Single Family",
                    "distanceMi": 1.65
                }
            ],
            "nearbyListings": [
                {
                    "address": "1500 Synth Willow St, San Jose, CA 95125",
                    "price": 1_595_000,
                    "sqft": 2150,
                    "beds": 4,
                    "baths": 2.5,
                    "yearBuilt": 1996,
                    "propertyType": "Single Family",
                    "distanceMi": 1.35
                },
                {
                    "address": "999 Synth Mansion Way, San Jose, CA 95125",
                    "price": 3_400_000,
                    "sqft": 4200,
                    "beds": 6,
                    "baths": 5.0,
                    "yearBuilt": 2015,
                    "propertyType": "Single Family",
                    "distanceMi": 1.20
                }
            ],
            "nearbyPending": [
                {
                    "address": "310 Synth Pine St, San Jose, CA 95112",
                    "price": 1_540_000,
                    "sqft": 1980,
                    "beds": 3,
                    "baths": 2.0,
                    "yearBuilt": 1994,
                    "propertyType": "Single Family",
                    "distanceMi": 0.85
                }
            ]
        });

        let strict_set = compute_copilot_comps("100-synth-main-st", &geo, &facts, None, &[], "strict");
        assert_eq!(strict_set["comps"].as_array().unwrap().len(), 2);
        assert_eq!(strict_set["canExpandSearch"], true);

        let exp_set = compute_copilot_comps("100-synth-main-st", &geo, &facts, None, &[], "expanded");
        assert_eq!(exp_set["comps"].as_array().unwrap().len(), 3);

        // Pick follow-ups in reverse/arbitrary order reusing `strict_set`:
        let f_sp = compute_copilot_followup("sold_or_pending", &strict_set, &geo, &facts, &[]).unwrap();
        assert_eq!(f_sp["compSetId"], strict_set["compSetId"]);
        assert_eq!(f_sp["recentlySold"].as_array().unwrap().len(), 2);
        assert_eq!(f_sp["pendingListings"].as_array().unwrap().len(), 1);

        let f_sim = compute_copilot_followup("similar_active", &strict_set, &geo, &facts, &[]).unwrap();
        assert_eq!(f_sim["activeListings"].as_array().unwrap().len(), 1);
        assert_eq!(
            f_sim["activeListings"][0]["address"],
            "1500 Synth Willow St, San Jose, CA 95125"
        );

        let f_est = compute_copilot_followup("estimate", &strict_set, &geo, &facts, &[]).unwrap();
        assert_eq!(f_est["compSetId"], strict_set["compSetId"]);
        assert!(f_est["estimateRangeUsd"]["low"].as_i64().unwrap() > 1_200_000);
        assert!(f_est["estimateRangeUsd"]["high"].as_i64().unwrap() > f_est["estimateRangeUsd"]["low"].as_i64().unwrap());
        assert_eq!(f_est["confidence"]["level"], "Medium");
    }

    #[test]
    fn offer_builder_prefills_jurisdiction_splits_editable_commission_and_post_nar_disclaimers() {
        let geo = json!({
            "matchedAddress": "500 Synth Grand Ave, Oakland, CA 94610",
            "city": "Oakland",
            "state": "CA",
            "county": "Alameda",
            "countyFips": "06001"
        });
        let facts = json!({
            "listPrice": 1_200_000,
            "forSale": true
        });
        let mut draft = init_offer_draft("500-synth-grand-ave", &geo, &facts);
        // Oakland city transfer tax is customarily split 50/50, Alameda county seller-paid,
        // Northern CA Alameda title/escrow is buyer-paid!
        assert_eq!(draft.buyer_agent_commission_pct, 2.5);
        assert_eq!(draft.city_transfer_tax_payer, "split_50_50");
        assert_eq!(draft.county_transfer_tax_payer, "seller");
        assert_eq!(draft.owners_title_payer, "buyer");
        assert_eq!(draft.escrow_fee_payer, "buyer");

        // User edits commission to 2.0% and sets offer price to $1,250,000
        apply_offer_patch(
            &mut draft,
            &json!({
                "step": 1,
                "offerPriceUsd": 1_250_000,
                "buyerAgentCommissionPct": 2.0,
                "rentBackEnabled": true,
                "rentBackDays": 15,
                "rentBackDepositUsd": 5000
            }),
        );
        let eval = build_offer_evaluation(&draft, &geo, &facts, None, None);
        assert_eq!(eval["runningSummary"]["offerPriceUsd"], 1_250_000);
        assert_eq!(
            eval["runningSummary"]["whoPaysWhat"]["buyerAgentCommission"]["totalUsd"],
            25_000
        );
        assert_eq!(eval["finalReview"]["isContract"], false);
        assert!(eval["finalReview"]["postNarSettlementNotice"]
            .as_str()
            .unwrap()
            .contains("Buyer-Broker Representation Agreement"));
        assert!(eval["finalReview"]["transferTaxDisclaimer"]
            .as_str()
            .unwrap()
            .contains("regional custom"));
        assert!(eval["finalReview"]["markdownSummary"]
            .as_str()
            .unwrap()
            .contains("Post-NAR Settlement Notice"));
        assert!(eval["finalReview"]["printableHtml"]
            .as_str()
            .unwrap()
            .contains("Post-NAR Buyer-Broker Notice"));
    }

    #[test]
    fn mailto_request_and_public_record_equivalents_follow_data_honesty() {
        let geo = json!({
            "matchedAddress": "100 Synth Main St, San Jose, CA 95112",
            "displayAddress": "100 Synth Main St, San Jose, CA 95112",
            "city": "San Jose",
            "state": "CA",
            "county": "Santa Clara",
            "countyFips": "06085",
        });
        let facts = json!({
            "apn": "123-45-001",
            "yearBuilt": 1998,
            "sqft": 2000,
            "mlsListing": {
                "agentEmail": "listing.agent@example.com",
                "agentName": "Jane Agent",
                "officeName": "Valley Realty"
            }
        });
        let mailto = build_copilot_mailto_request(&geo, &facts);
        assert_eq!(mailto["recipientEmail"], "listing.agent@example.com");
        assert!(mailto["mailtoHref"]
            .as_str()
            .unwrap()
            .starts_with("mailto:listing.agent@example.com?subject="));
        assert!(mailto["body"].as_str().unwrap().contains("123-45-001"));
        assert!(mailto["body"].as_str().unwrap().contains("Termite"));
        assert!(mailto["body"].as_str().unwrap().contains("Preliminary Title"));
        assert_eq!(mailto["smtpDisabled"], true);

        let equiv = build_public_record_equivalents("100-synth-main-st", &geo, &facts, None);
        assert_eq!(equiv["available"], true);
        assert_eq!(equiv["hasFullReportData"], false);
        assert_eq!(equiv["items"].as_array().unwrap().len(), 4);
        // Never invent missing flood zone or assessed value when full report & tax facts are absent
        assert!(equiv["items"][0]["metrics"].get("floodZone").is_none());
        assert!(equiv["items"][1]["metrics"].get("assessedValueUsd").is_none());
        assert_eq!(equiv["items"][1]["metrics"]["apn"], "123-45-001");
        assert_eq!(equiv["items"][2]["metrics"]["yearBuilt"], 1998);
    }

    #[test]
    fn normalize_copilot_role_maps_expected_values() {
        assert_eq!(normalize_copilot_role(Some("buyer")), "buyer");
        assert_eq!(normalize_copilot_role(Some("Seller")), "seller");
        assert_eq!(normalize_copilot_role(Some("agent")), "agent");
        assert_eq!(normalize_copilot_role(Some("all")), "unset");
        assert_eq!(normalize_copilot_role(None), "unset");
    }
}
