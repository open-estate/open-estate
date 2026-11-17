// Area alert subscriptions, `minijinja` + `html_escape` email rendering, background Cron ZIP sync loop,
// LLM match verification, deduplicated SMTP dispatch, and `/api/listings` + `/api/watched-zips` queries.

use crate::db::{
    compress_bytes_zstd_dict, decompress_bytes_zstd_any, deterministic_sk, extract_section_json,
    format_listing_row, proto, DbStore, FeatureTag, ListingFullView, ListingRow, ReportHeaderProto,
    ReportSectionKind, SqlParam, SubscriptionCoreProto, SubscriptionDigestItemProto,
    SubscriptionNotificationProto, SubscriptionRow,
};
use crate::modules::commute::{LatLon, RoutingClient, RoutingConfig};
use crate::scoring::catalog::{Dimension, Feature, Lang, MatchBand, PropertyKind, Screen};
use crate::scoring::engine::{rank_with, CommuteMatrix, NoticeCode, RankOptions, ScoredHome};
use crate::scoring::explain::localize;
use crate::scoring::factors::{Factor, Grade, HomeFactors};
use crate::scoring::presets::PRESETS;
use crate::scoring::profile::Profile;
use crate::search::mailer::{send_smtp_email, smtp_status_json, EmailRequest};
use crate::search::matcher::{
    clean_untrusted_text, fmt_clean_num, format_int_commas, parse_requirements_to_query,
    rank_candidates_with_llm, score_listing_deterministic, search_listings_narrow, SearchFilters,
    SearchValidationError, CURRENT_FILTER_SCHEMA_VERSION,
};
pub use crate::search::matcher::{
    fair_housing_audit, fair_housing_audit_reports, poison_report_with_demographics,
    suggest_weight_adjustments, FairHousingAuditReport, FeedbackAction, FeedbackSample,
    PresetAuditResult, WeightSuggestion,
};
use crate::search::sync::{
    parse_listing_csv, seed_reports_from_dir, str_from_val, sync_connector_status,
    sync_zip_listings_with_provider, upsert_listings_batch, ListingChangeEvent, ListingInput,
    LiveZipListingProvider,
};
use crate::shared::{
    format_brand_wordmark_styled_html, now_iso, now_unix_ms, now_unix_secs, EnvConfig, HttpClient,
};
use crate::web::i18n::{bcp47_code, resolve_lang_str, t, t_args, t_brand, t_brand_args};
use anyhow::Result;
use html_escape::encode_safe;
use minijinja::{context, AutoEscape, Environment};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use unic_langid::LanguageIdentifier;
use uuid::Uuid;

static EMAIL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$").unwrap()
});
static EXACT_ZIP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\d{5})(?:-\d{4})?$").unwrap());
static SPLIT_ZIPS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\s,;]+").unwrap());

pub fn validate_email_input(raw_email: &str) -> std::result::Result<String, SearchValidationError> {
    let email = clean_untrusted_text(raw_email, 180).to_ascii_lowercase();
    if email.is_empty() || !EMAIL_RE.is_match(&email) {
        return Err(SearchValidationError::new(
            "INVALID_EMAIL",
            "Please enter a valid email address.",
            400,
        ));
    }
    Ok(email)
}

pub fn validate_zip_list(
    raw_zips: &[String],
    max_zips: usize,
    allow_empty: bool,
) -> std::result::Result<Vec<String>, SearchValidationError> {
    let mut out = Vec::new();
    for item in raw_zips {
        for tok in SPLIT_ZIPS_RE.split(item) {
            let clean = tok.trim();
            if clean.is_empty() {
                continue;
            }
            let Some(cap) = EXACT_ZIP_RE.captures(clean) else {
                return Err(SearchValidationError::new(
                    "INVALID_ZIP",
                    format!(
                        "Invalid 5-digit US ZIP code: \"{}\".",
                        &clean[..clean.len().min(16)]
                    ),
                    400,
                ));
            };
            let z = cap[1].to_string();
            if !out.contains(&z) {
                out.push(z);
            }
        }
    }
    if !allow_empty && out.is_empty() {
        return Err(SearchValidationError::new(
            "MISSING_ZIP",
            "Please specify at least one 5-digit US ZIP code to watch.",
            400,
        ));
    }
    if out.len() > max_zips {
        return Err(SearchValidationError::new(
            "TOO_MANY_ZIPS",
            format!("You can subscribe to up to {max_zips} ZIP codes per alert."),
            400,
        ));
    }
    Ok(out)
}

pub fn validate_zip_csv(
    raw_csv: &str,
    max_zips: usize,
    allow_empty: bool,
) -> std::result::Result<Vec<String>, SearchValidationError> {
    validate_zip_list(&[raw_csv.to_string()], max_zips, allow_empty)
}

fn generate_unsubscribe_token() -> String {
    let mut h = Sha256::new();
    h.update(Uuid::new_v4().as_bytes());
    h.update(Uuid::new_v4().as_bytes());
    h.update(now_unix_ms().to_le_bytes());
    let hex_str = hex::encode(h.finalize());
    hex_str[..48].to_string()
}

async fn refresh_watched_zip_subscriber_counts(db: &DbStore, extra_zips: &[String]) -> Result<()> {
    db.ensure_schema().await?;
    let zip_counts = db
        .fetch_all_mapped_raw(
            "SELECT sz.zip AS zip, COUNT(*) AS cnt \
             FROM subscription_zips sz \
             JOIN subscriptions s ON s.subscription_sk = sz.subscription_sk \
             WHERE s.active = 1 \
             GROUP BY sz.zip",
            &[],
            |r| {
                (
                    r.opt_str("zip").unwrap_or_default(),
                    r.opt_i64("cnt").unwrap_or(0),
                )
            },
        )
        .await?;

    let mut count_map: HashMap<String, i64> = HashMap::with_capacity(zip_counts.len());
    for (z, cnt) in zip_counts {
        if !z.is_empty() {
            count_map.insert(z, cnt);
        }
    }

    let existing_watched = db
        .fetch_all_mapped_raw(
            "SELECT zip, active FROM watched_zips",
            &[],
            |r| {
                (
                    r.opt_str("zip").unwrap_or_default(),
                    r.opt_i64("active").unwrap_or(1),
                )
            },
        )
        .await?;
    let existing_map: HashMap<String, i64> = existing_watched
        .into_iter()
        .filter(|(z, _)| !z.is_empty())
        .collect();

    let mut all_zips: BTreeSet<String> = extra_zips
        .iter()
        .map(|z| z.trim().to_string())
        .filter(|z| !z.is_empty())
        .collect();
    for k in count_map.keys() {
        all_zips.insert(k.clone());
    }
    for (k, _) in &existing_map {
        if !count_map.contains_key(k) {
            all_zips.insert(k.clone());
        }
    }

    let now_ts = now_unix_secs();
    for z in all_zips {
        let sub_cnt = *count_map.get(&z).unwrap_or(&0);
        if let Some(&prev_active) = existing_map.get(&z) {
            let active = if sub_cnt > 0 { 1_i64 } else { prev_active };
            db.execute_params_raw(
                "UPDATE watched_zips SET subscriber_count = ?, active = ? WHERE zip = ?",
                &[
                    SqlParam::I64(sub_cnt),
                    SqlParam::I64(active),
                    SqlParam::from(z.as_str()),
                ],
            )
            .await?;
        } else {
            db.upsert_row_raw(
                "watched_zips",
                &[
                    ("zip", SqlParam::from(z.as_str())),
                    ("active", SqlParam::I64(1)),
                    ("subscriber_count", SqlParam::I64(sub_cnt)),
                    ("sync_interval_min", SqlParam::I64(15)),
                    ("last_sync_status", SqlParam::I64(0)),
                    ("last_new_count", SqlParam::I64(0)),
                    ("last_sold_count", SqlParam::I64(0)),
                    ("total_active_listings", SqlParam::I64(0)),
                    ("total_sold_listings", SqlParam::I64(0)),
                    ("created_ts", SqlParam::I64(now_ts)),
                ],
                &["zip"],
            )
            .await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertMatchedItem {
    pub event_type: String,
    pub old_price: Option<i64>,
    pub event_key: String,
    pub listing: Value,
    pub match_score: i64,
    pub match_tier: String,
    pub why_it_matches: String,
    pub tradeoffs: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertEmailPayload {
    pub subject: String,
    pub text: String,
    pub html: String,
    pub unsubscribe_url: String,
}

const MATCH_ALERT_HTML_TPL: &str = r#"<!doctype html><html lang="{{ html_lang }}"><body style="margin:0;padding:20px;background:#f5f6f8;font-family:Inter,-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#111827">
  <div style="max-width:600px;margin:0 auto">
    <table width="100%" cellpadding="0" cellspacing="0" role="presentation" style="margin-bottom:14px">
      <tr>
        <td style="font-weight:800;font-size:20px;color:#111827">{{ brand_wordmark | safe }}</td>
        <td align="right" style="vertical-align:middle"><a href="{{ manage_url }}" style="font-size:13px;font-weight:600;color:#0f766e;text-decoration:none">{{ manage_cta_label }} →</a></td>
      </tr>
    </table>
    <div style="font-size:17px;font-weight:700;color:#111827;margin:0 0 4px">{{ intro_html_text }}</div>
    {% if search_caption %}<div style="font-size:13px;color:#6b7280;margin:0 0 16px">{{ search_caption }}</div>{% else %}<div style="margin-bottom:14px"></div>{% endif %}
    {% for card in cards %}
    <div style="border:1px solid #e5e7eb;border-radius:12px;overflow:hidden;margin-bottom:14px;background:#ffffff">
      {% if card.photo_url %}
      <a href="{{ card.report_link }}" style="display:block;text-decoration:none">
        <img src="{{ card.photo_url }}" alt="{{ card.address }}" style="width:100%;max-height:220px;object-fit:cover;display:block;background:#f3f4f6">
      </a>
      {% endif %}
      <div style="padding:14px 16px">
        <div style="margin-bottom:6px">
          <span style="display:inline-block;background:{{ card.badge_bg }};color:{{ card.badge_fg }};font-size:11px;font-weight:700;padding:2px 8px;border-radius:999px;letter-spacing:.3px">{{ card.badge_text }}</span>
        </div>
        <div style="font-size:21px;font-weight:800;color:#111827;line-height:1.25">{{ card.price_label }}{% if card.old_price_label %} <span style="font-size:13px;color:#6b7280;font-weight:500;text-decoration:line-through;margin-left:4px">{{ card.old_price_label }}</span>{% endif %}</div>
        {% if card.specs %}<div style="font-size:13.5px;color:#374151;margin:4px 0 4px">{{ card.specs }}</div>{% endif %}
        <div style="font-size:14px;font-weight:600;color:#111827;margin-bottom:11px">{{ card.address }}</div>
        <a href="{{ card.report_link }}" style="display:inline-block;background:#0f766e;color:#ffffff;text-decoration:none;font-size:13px;font-weight:700;padding:8px 14px;border-radius:8px">{{ view_home_label }}</a>
      </div>
    </div>
    {% endfor %}
    {% if more_homes_label %}
    <div style="text-align:center;margin:10px 0 16px">
      <a href="{{ manage_url }}" style="display:inline-block;background:#ffffff;border:1px solid #d1d5db;color:#111827;text-decoration:none;font-size:13px;font-weight:700;padding:9px 16px;border-radius:999px">{{ more_homes_label }}</a>
    </div>
    {% endif %}
    <div style="font-size:12px;color:#6b7280;margin-top:18px;border-top:1px solid #e5e7eb;padding-top:12px;line-height:1.6">
      <a href="{{ manage_url }}" style="color:#0f766e;font-weight:600;text-decoration:none">{{ manage_cta_label }}</a>
      &nbsp;·&nbsp;
      <a href="{{ unsub_url }}" style="color:#6b7280;text-decoration:underline">{{ unsub_cta_label }}</a><br>
      Compiled from public &amp; MLS listing feeds — verify all facts independently.
    </div>
  </div>
</body></html>"#;

const WELCOME_CONFIRM_HTML_TPL: &str = r#"<!doctype html><html lang="{{ html_lang }}"><body style="margin:0;padding:20px;background:#f5f6f8;font-family:Inter,-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#111827">
  <div style="max-width:580px;margin:0 auto">
    <div style="background:#ffffff;border:1px solid #e5e7eb;border-radius:14px;padding:22px 24px">
      <div style="font-weight:800;font-size:20px;color:#111827;margin-bottom:14px">{{ brand_wordmark | safe }}</div>
      <h2 style="margin:0 0 8px;font-size:18px;font-weight:800;color:#111827">{{ welcome_heading }}</h2>
      <p style="margin:0 0 16px;font-size:14.5px;color:#374151;line-height:1.55">{{ welcome_watching }}</p>
      <div style="margin-bottom:12px">
        <div style="font-size:11.5px;font-weight:700;text-transform:uppercase;letter-spacing:.4px;color:#6b7280;margin-bottom:6px">{{ zips_label }}</div>
        {% for z in zip_chips %}<span style="display:inline-block;background:#f0fdfa;border:1px solid #99f6e4;color:#0f766e;font-size:12.5px;font-weight:700;padding:4px 10px;border-radius:999px;margin:0 6px 6px 0">{{ z }}</span>{% endfor %}
      </div>
      <div style="margin-bottom:18px">
        <span style="font-size:12.5px;color:#6b7280;margin-right:6px">{{ cadence_label_title }}:</span>
        <span style="display:inline-block;background:#eef2ff;color:#3730a3;font-size:12px;font-weight:700;padding:4px 10px;border-radius:999px">{{ cadence_pill }}</span>
      </div>
      <div>
        <a href="{{ manage_url }}" style="display:inline-block;background:#0f766e;color:#ffffff;text-decoration:none;font-size:13.5px;font-weight:700;padding:10px 18px;border-radius:9px">{{ manage_cta_label }} →</a>
      </div>
    </div>
    <div style="font-size:12px;color:#6b7280;margin-top:14px;text-align:center">
      <a href="{{ manage_url }}" style="color:#0f766e;font-weight:600;text-decoration:none">{{ manage_cta_label }}</a>
      &nbsp;·&nbsp;
      <a href="{{ unsub_url }}" style="color:#6b7280;text-decoration:underline">{{ unsub_cta_label }}</a>
    </div>
  </div>
</body></html>"#;

const MANAGE_LINK_HTML_TPL: &str = r#"<!doctype html><html lang="{{ html_lang }}"><body style="margin:0;padding:20px;background:#f5f6f8;font-family:Inter,-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#111827">
  <div style="max-width:560px;margin:0 auto">
    <div style="background:#ffffff;border:1px solid #e5e7eb;border-radius:14px;padding:22px 24px">
      <div style="font-weight:800;font-size:20px;color:#111827;margin-bottom:14px">{{ brand_wordmark | safe }}</div>
      <h2 style="margin:0 0 10px;font-size:18px;font-weight:800;color:#111827">{{ heading }}</h2>
      <p style="margin:0 0 18px;font-size:14.5px;color:#374151;line-height:1.55">{{ intro }}</p>
      <a href="{{ manage_url }}" style="display:inline-block;background:#0f766e;color:#ffffff;text-decoration:none;font-size:14px;font-weight:700;padding:10px 18px;border-radius:9px">{{ cta_label }}</a>
    </div>
  </div>
</body></html>"#;

static ALERT_EMAIL_JINJA_ENV: Lazy<Environment<'static>> = Lazy::new(|| {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|name| {
        if name.ends_with(".html") {
            AutoEscape::Html
        } else {
            minijinja::default_auto_escape_callback(name)
        }
    });
    env.add_template("match_alert.html", MATCH_ALERT_HTML_TPL)
        .unwrap();
    env.add_template("welcome_confirm.html", WELCOME_CONFIRM_HTML_TPL)
        .unwrap();
    env.add_template("manage_link.html", MANAGE_LINK_HTML_TPL)
        .unwrap();
    env
});

fn resolve_origin_and_brand(
    base_origin: &str,
    brand_name: &str,
    brand_domain: &str,
) -> (String, String, String) {
    let eff_brand = if brand_name.trim().is_empty() {
        "Open Estate".to_string()
    } else {
        brand_name.trim().to_string()
    };
    let eff_domain = if brand_domain.trim().is_empty() {
        "open-estate.local".to_string()
    } else {
        brand_domain.trim().to_string()
    };
    let origin = if base_origin.trim().is_empty() {
        format!("https://{eff_domain}")
    } else {
        base_origin.trim().trim_end_matches('/').to_string()
    };
    (origin, eff_brand, eff_domain)
}

pub fn normalize_frequency(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "daily" | "day" => "daily",
        "weekly" | "week" => "weekly",
        _ => "instant",
    }
}

fn cadence_label_for_lang(lang: &LanguageIdentifier, freq: &str) -> String {
    match normalize_frequency(freq) {
        "daily" => t(lang, "alerts-freq-daily"),
        "weekly" => t(lang, "alerts-freq-weekly"),
        _ => t(lang, "alerts-freq-instant"),
    }
}

fn valid_http_photo_url(raw: Option<String>) -> Option<String> {
    let trimmed = raw?.trim().to_string();
    let lc = trimmed.to_ascii_lowercase();
    if (lc.starts_with("https://") || lc.starts_with("http://"))
        && !trimmed.chars().any(char::is_whitespace)
        && !trimmed.chars().any(char::is_control)
    {
        Some(trimmed)
    } else {
        None
    }
}

pub(crate) fn safe_html_url_val(raw_url: &str) -> minijinja::Value {
    let escaped = raw_url
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    minijinja::Value::from_safe_string(escaped)
}

pub fn build_match_alert_email(
    subscriber_name: Option<&str>,
    requirements_text: &str,
    zips: &[String],
    matched_items: &[AlertMatchedItem],
    base_origin: &str,
    unsubscribe_token: &str,
) -> AlertEmailPayload {
    build_match_alert_email_for_lang(
        subscriber_name,
        requirements_text,
        zips,
        matched_items,
        base_origin,
        unsubscribe_token,
        "en-US",
    )
}

pub fn build_match_alert_email_for_lang(
    subscriber_name: Option<&str>,
    requirements_text: &str,
    zips: &[String],
    matched_items: &[AlertMatchedItem],
    base_origin: &str,
    unsubscribe_token: &str,
    lang: &str,
) -> AlertEmailPayload {
    build_match_alert_email_with_brand(
        subscriber_name,
        requirements_text,
        zips,
        matched_items,
        base_origin,
        unsubscribe_token,
        lang,
        "Open Estate",
        "open-estate.local",
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_match_alert_email_with_brand(
    subscriber_name: Option<&str>,
    requirements_text: &str,
    zips: &[String],
    matched_items: &[AlertMatchedItem],
    base_origin: &str,
    unsubscribe_token: &str,
    lang: &str,
    brand_name: &str,
    brand_domain: &str,
) -> AlertEmailPayload {
    render_home_digest_email(
        subscriber_name,
        requirements_text,
        zips,
        matched_items,
        base_origin,
        unsubscribe_token,
        lang,
        brand_name,
        brand_domain,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_welcome_confirm_email_with_brand(
    subscriber_name: Option<&str>,
    zips: &[String],
    frequency: &str,
    base_origin: &str,
    unsubscribe_token: &str,
    lang: &str,
    brand_name: &str,
    brand_domain: &str,
) -> AlertEmailPayload {
    let (origin, eff_brand, _eff_domain) =
        resolve_origin_and_brand(base_origin, brand_name, brand_domain);
    let encoded_tok = urlencoding::encode(unsubscribe_token);
    let unsub_url = format!("{origin}/api/subscriptions/unsubscribe?token={encoded_tok}");
    let manage_url = format!("{origin}/alerts?token={encoded_tok}");
    let zips_str = zips.join(", ");
    let lang_id = resolve_lang_str(lang);

    let welcome_subject =
        t_brand_args(&lang_id, "email-welcome-subject", &eff_brand, &[("zips", &zips_str)]);
    let welcome_heading = t_brand(&lang_id, "email-welcome-heading", &eff_brand);
    let welcome_intro =
        t_brand_args(&lang_id, "email-welcome-intro", &eff_brand, &[("zips", &zips_str)]);
    let welcome_watching =
        t_args(&lang_id, "email-welcome-watching", &[("zips", &zips_str)]);
    let manage_cta = t(&lang_id, "email-alert-manage-cta");
    let unsub_cta = t(&lang_id, "email-welcome-unsub");
    let zips_label = t(&lang_id, "email-welcome-zips-label");
    let cadence_label_title = t(&lang_id, "email-welcome-cadence-label");
    let cadence_pill = cadence_label_for_lang(&lang_id, frequency);
    let greeting = match subscriber_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(nm) => t_args(&lang_id, "email-alert-greeting-named", &[("name", nm)]),
        None => t(&lang_id, "email-alert-greeting-anon"),
    };

    let text_body = [
        greeting,
        String::new(),
        welcome_intro,
        format!("{zips_label}: {zips_str}"),
        format!("{cadence_label_title}: {cadence_pill}"),
        String::new(),
        format!("{manage_cta}: {manage_url}"),
        format!("{unsub_cta}: {unsub_url}"),
    ]
    .join("\n");

    let brand_wordmark = format_brand_wordmark_styled_html(&eff_brand, Some("color:#0f766e"));
    let html_body = ALERT_EMAIL_JINJA_ENV
        .get_template("welcome_confirm.html")
        .and_then(|t_tpl| {
            t_tpl.render(context! {
                html_lang => bcp47_code(&lang_id),
                brand_wordmark => &brand_wordmark,
                welcome_heading => &welcome_heading,
                welcome_watching => &welcome_watching,
                zips_label => &zips_label,
                zip_chips => zips.to_vec(),
                cadence_label_title => &cadence_label_title,
                cadence_pill => &cadence_pill,
                manage_url => safe_html_url_val(&manage_url),
                manage_cta_label => &manage_cta,
                unsub_url => safe_html_url_val(&unsub_url),
                unsub_cta_label => &unsub_cta,
            })
        })
        .unwrap_or_else(|_| {
            format!(
                "<p>{}</p><p><a href=\"{}\">{}</a></p>",
                encode_safe(&welcome_watching),
                encode_safe(&unsub_url),
                encode_safe(&unsub_cta),
            )
        });

    AlertEmailPayload {
        subject: welcome_subject,
        text: text_body,
        html: html_body,
        unsubscribe_url: unsub_url,
    }
}

pub fn build_manage_link_email_with_brand(
    base_origin: &str,
    manage_token: &str,
    lang: &str,
    brand_name: &str,
    brand_domain: &str,
) -> AlertEmailPayload {
    let (origin, eff_brand, _eff_domain) =
        resolve_origin_and_brand(base_origin, brand_name, brand_domain);
    let manage_url = format!("{origin}/alerts?manage={}", urlencoding::encode(manage_token));

    let lang_id = resolve_lang_str(lang);
    let subject = t_brand(&lang_id, "email-manage-subject", &eff_brand);
    let heading = t_brand(&lang_id, "email-manage-heading", &eff_brand);
    let intro = t(&lang_id, "email-manage-intro");
    let cta_label = t(&lang_id, "email-manage-cta");

    let text_body = [
        heading.clone(),
        String::new(),
        intro.clone(),
        String::new(),
        format!("{cta_label}: {manage_url}"),
    ]
    .join("\n");

    let brand_wordmark = format_brand_wordmark_styled_html(&eff_brand, Some("color:#0f766e"));
    let html_body = ALERT_EMAIL_JINJA_ENV
        .get_template("manage_link.html")
        .and_then(|tpl| {
            tpl.render(context! {
                html_lang => bcp47_code(&lang_id),
                brand_wordmark => &brand_wordmark,
                heading => &heading,
                intro => &intro,
                manage_url => safe_html_url_val(&manage_url),
                cta_label => &cta_label,
            })
        })
        .unwrap_or_else(|_| {
            format!(
                "<p>{}</p><p><a href=\"{}\">{}</a></p>",
                encode_safe(&intro),
                encode_safe(&manage_url),
                encode_safe(&cta_label),
            )
        });

    AlertEmailPayload {
        subject,
        text: text_body,
        html: html_body,
        unsubscribe_url: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_home_digest_email(
    subscriber_name: Option<&str>,
    requirements_text: &str,
    zips: &[String],
    matched_items: &[AlertMatchedItem],
    base_origin: &str,
    unsubscribe_token: &str,
    lang: &str,
    brand_name: &str,
    brand_domain: &str,
    subject_override: Option<&str>,
    heading_override: Option<&str>,
    frequency: Option<&str>,
) -> AlertEmailPayload {
    let (origin, eff_brand_owned, _eff_domain) =
        resolve_origin_and_brand(base_origin, brand_name, brand_domain);
    let eff_brand = eff_brand_owned.as_str();
    let lang = resolve_lang_str(lang);
    let encoded_tok = urlencoding::encode(unsubscribe_token);
    let unsub_url = format!("{origin}/api/subscriptions/unsubscribe?token={encoded_tok}");
    let manage_url = format!("{origin}/alerts?token={encoded_tok}");

    let mut sorted_items: Vec<&AlertMatchedItem> = matched_items.iter().collect();
    sorted_items.sort_by(|a, b| {
        b.match_score.cmp(&a.match_score).then_with(|| {
            let ia = str_from_val(a.listing.get("id")).unwrap_or_default();
            let ib = str_from_val(b.listing.get("id")).unwrap_or_default();
            ia.cmp(&ib)
        })
    });

    let count = sorted_items.len();
    let has_sold = sorted_items
        .iter()
        .any(|m| str_from_val(m.listing.get("status")).as_deref() == Some("sold"));
    let has_new = sorted_items
        .iter()
        .any(|m| str_from_val(m.listing.get("status")).as_deref() != Some("sold"));
    let kind_key = if has_new && has_sold {
        "email-alert-kind-both"
    } else if has_sold {
        "email-alert-kind-sold"
    } else {
        "email-alert-kind-new"
    };
    let kind_label = t(&lang, kind_key);
    let noun = t(
        &lang,
        if count == 1 {
            "email-alert-noun-one"
        } else {
            "email-alert-noun-many"
        },
    );

    let zips_joined = zips.join(", ");
    let count_str = count.to_string();
    let norm_freq = frequency.map(normalize_frequency);
    let subject = if let Some(ov) = subject_override.map(str::trim).filter(|s| !s.is_empty()) {
        ov.to_string()
    } else if matches!(norm_freq, Some("daily") | Some("weekly")) {
        let cadence = cadence_label_for_lang(&lang, norm_freq.unwrap_or("daily"));
        t_brand_args(
            &lang,
            "email-digest-subject",
            eff_brand,
            &[
                ("cadence", cadence.as_str()),
                ("count", &count_str),
                ("zips", &zips_joined),
            ],
        )
    } else if count == 1 {
        let first_l = &sorted_items[0].listing;
        let street_or_addr = str_from_val(first_l.get("street"))
            .or_else(|| str_from_val(first_l.get("address")))
            .unwrap_or_else(|| "Property".to_string());
        let p = first_l.get("price").and_then(Value::as_i64).unwrap_or(0);
        let price_str = format!("${}", format_int_commas(p));
        t_brand_args(
            &lang,
            "email-alert-subject-single",
            eff_brand,
            &[
                ("kind", kind_label.as_str()),
                ("street", &street_or_addr),
                ("price", &price_str),
            ],
        )
    } else {
        t_brand_args(
            &lang,
            "email-alert-subject-multi",
            eff_brand,
            &[("count", &count_str), ("zips", &zips_joined)],
        )
    };

    let intro_sentence = heading_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            t_brand_args(
                &lang,
                "email-alert-intro",
                eff_brand,
                &[
                    ("count", &count_str),
                    ("noun", noun.as_str()),
                    ("zips", &zips_joined),
                ],
            )
        });

    let clean_reqs = crate::shared::scrub_competitor_brands(requirements_text);
    let trimmed_reqs = clean_reqs.trim();
    let search_caption = if !trimmed_reqs.is_empty()
        && !trimmed_reqs.starts_with('{')
        && !trimmed_reqs.starts_with("Saved Your Match profile")
    {
        let mut short_req: String = trimmed_reqs.chars().take(140).collect();
        if trimmed_reqs.chars().count() > 140 {
            short_req.push('…');
        }
        if let Some(freq) = norm_freq {
            format!("{} · {short_req}", cadence_label_for_lang(&lang, freq))
        } else {
            short_req
        }
    } else if let Some(freq) = norm_freq {
        if zips_joined.is_empty() {
            cadence_label_for_lang(&lang, freq)
        } else {
            format!("{} · ZIP {zips_joined}", cadence_label_for_lang(&lang, freq))
        }
    } else {
        String::new()
    };

    let manage_cta = t(&lang, "email-alert-manage-cta");
    let view_home_label = t(&lang, "email-alert-view-home");
    let unsub_cta = t(&lang, "email-welcome-unsub");

    let greeting = match subscriber_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(nm) => t_args(&lang, "email-alert-greeting-named", &[("name", nm)]),
        None => t(&lang, "email-alert-greeting-anon"),
    };
    let mut text_lines = vec![greeting, String::new(), intro_sentence.clone()];
    if !search_caption.is_empty() {
        text_lines.push(search_caption.clone());
    }
    text_lines.push(String::new());

    let badge_sold_txt = t(&lang, "email-alert-badge-sold");
    let badge_drop_txt = t(&lang, "email-alert-badge-drop");
    let badge_new_txt = t(&lang, "email-alert-badge-new");
    let badge_drop_html = t(&lang, "email-alert-badge-drop-html");
    let badge_new_html = t(&lang, "email-alert-badge-new-html");
    let price_tbd = t(&lang, "email-alert-price-tbd");
    let bd_unit = t(&lang, "email-alert-bd");
    let ba_unit = t(&lang, "email-alert-ba");

    let max_cards = 10_usize;
    let display_items = &sorted_items[..count.min(max_cards)];
    let remaining = count.saturating_sub(max_cards);
    let more_homes_label = if remaining > 0 {
        Some(t_args(
            &lang,
            "email-alert-more-homes",
            &[("remaining", &remaining.to_string())],
        ))
    } else {
        None
    };

    let mut cards = Vec::with_capacity(display_items.len());
    for item in display_items {
        let l = &item.listing;
        let addr = str_from_val(l.get("address")).unwrap_or_default();
        let st = str_from_val(l.get("status")).unwrap_or_default();
        let is_sold = item.event_type == "sold" || st == "sold";
        let is_drop = item.event_type == "price_change";
        let badge_txt = if is_sold {
            badge_sold_txt.as_str()
        } else if is_drop {
            badge_drop_txt.as_str()
        } else {
            badge_new_txt.as_str()
        };
        let report_link = if let Some(rs) = str_from_val(l.get("reportSlug")) {
            format!("{origin}/r/{}/", urlencoding::encode(&rs))
        } else {
            format!("{origin}/report?address={}", urlencoding::encode(&addr))
        };
        let price_num = l.get("price").and_then(Value::as_i64);
        let price_txt = price_num
            .map(|p| format!("${}", format_int_commas(p)))
            .unwrap_or_else(|| price_tbd.clone());
        let old_price_label = match (item.old_price, price_num) {
            (Some(op), Some(cp)) if op != cp => Some(format!("${}", format_int_commas(op))),
            _ => None,
        };

        let beds = l.get("beds").and_then(Value::as_f64);
        let baths = l.get("baths").and_then(Value::as_f64);
        let sqft = l.get("sqft").and_then(Value::as_i64);
        let specs = [
            beds.map(|b| format!("{} {bd_unit}", fmt_clean_num(b))),
            baths.map(|b| format!("{} {ba_unit}", fmt_clean_num(b))),
            sqft.map(|s| format!("{} sq ft", format_int_commas(s))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        let price_line = if let Some(ref op) = old_price_label {
            format!("{price_txt} (was {op})")
        } else {
            price_txt.clone()
        };
        if specs.is_empty() {
            text_lines.push(format!("• [{badge_txt}] {price_line}"));
        } else {
            text_lines.push(format!("• [{badge_txt}] {price_line} — {specs}"));
        }
        text_lines.push(format!("  {addr}"));
        text_lines.push(format!("  {view_home_label} {report_link}"));
        text_lines.push(String::new());

        let photo_url = valid_http_photo_url(str_from_val(l.get("photoUrl")))
            .map(|u| safe_html_url_val(&u));
        let report_link_val = safe_html_url_val(&report_link);
        cards.push(context! {
            photo_url => photo_url,
            badge_text => if is_sold { &badge_sold_txt } else if is_drop { &badge_drop_html } else { &badge_new_html },
            badge_bg => if is_sold { "#ede9fe" } else if is_drop { "#fef3c7" } else { "#d1fae5" },
            badge_fg => if is_sold { "#5b21b6" } else if is_drop { "#92400e" } else { "#065f46" },
            address => addr,
            price_label => price_txt,
            old_price_label => old_price_label,
            specs => specs,
            report_link => report_link_val,
        });
    }

    if let Some(ref more_lbl) = more_homes_label {
        text_lines.push(format!("{more_lbl}: {manage_url}"));
        text_lines.push(String::new());
    }
    text_lines.push("---".to_string());
    text_lines.push(format!("{manage_cta}: {manage_url}"));
    text_lines.push(format!("{unsub_cta}: {unsub_url}"));

    // Render HTML safely via pre-compiled `minijinja` `.html` auto-escaping template
    let brand_wordmark = format_brand_wordmark_styled_html(eff_brand, Some("color:#0f766e"));
    let manage_url_val = safe_html_url_val(&manage_url);
    let unsub_url_val = safe_html_url_val(&unsub_url);
    let html = ALERT_EMAIL_JINJA_ENV
        .get_template("match_alert.html")
        .and_then(|t_tpl| {
            t_tpl.render(context! {
                html_lang => bcp47_code(&lang),
                brand_wordmark => &brand_wordmark,
                manage_url => manage_url_val,
                manage_cta_label => &manage_cta,
                intro_html_text => &intro_sentence,
                search_caption => &search_caption,
                cards => cards,
                view_home_label => &view_home_label,
                more_homes_label => &more_homes_label,
                unsub_cta_label => &unsub_cta,
                unsub_url => unsub_url_val,
            })
        })
        .unwrap_or_else(|_| {
            format!(
                "<p>{}</p><p><a href=\"{}\">{}</a></p>",
                encode_safe(&intro_sentence),
                encode_safe(&unsub_url),
                encode_safe(&unsub_cta),
            )
        });

    AlertEmailPayload {
        subject,
        text: text_lines.join("\n"),
        html,
        unsubscribe_url: unsub_url,
    }
}

fn default_true() -> bool {
    true
}

fn default_frequency() -> String {
    "instant".to_string()
}

fn default_origin() -> String {
    String::new()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSubscriptionRequest {
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub zips: Vec<String>,
    #[serde(alias = "requirementsText")]
    pub requirements: String,
    #[serde(default = "default_true")]
    pub notify_on_new: bool,
    #[serde(default)]
    pub notify_on_sold: bool,
    #[serde(default = "default_frequency")]
    pub frequency: String,
    #[serde(default = "default_true")]
    pub send_welcome_email: bool,
    #[serde(default)]
    pub sync_now: bool,
    #[serde(default = "default_origin")]
    pub base_origin: String,
}

impl Default for CreateSubscriptionRequest {
    fn default() -> Self {
        Self {
            email: String::new(),
            name: String::new(),
            zips: Vec::new(),
            requirements: String::new(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: default_frequency(),
            send_welcome_email: true,
            sync_now: false,
            base_origin: default_origin(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSubscriptionPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub zips: Option<Vec<String>>,
    #[serde(default, alias = "requirementsText")]
    pub requirements: Option<String>,
    #[serde(default)]
    pub notify_on_new: Option<bool>,
    #[serde(default)]
    pub notify_on_sold: Option<bool>,
    #[serde(default)]
    pub frequency: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
}

pub fn subscription_to_json(r: &SubscriptionRow, include_token: bool) -> Value {
    let zips: Value = serde_json::from_str(&r.zips_json).unwrap_or_else(|_| json!([]));
    let parsed_filters: Value =
        serde_json::from_str(&r.parsed_filters_json).unwrap_or_else(|_| json!({}));
    let pending_digest_count = parse_pending_digest_items(r.digest_pending_json.as_deref()).len();
    let mut obj = json!({
        "id": r.id,
        "email": r.email,
        "name": r.name,
        "zips": zips,
        "requirementsText": r.requirements_text,
        "intentSummary": parsed_filters.get("intentSummary").cloned().unwrap_or(Value::Null),
        "parsedFilters": parsed_filters,
        "notifyOnNew": r.notify_on_new != 0,
        "notifyOnSold": r.notify_on_sold != 0,
        "active": r.active != 0,
        "frequency": normalize_frequency(&r.frequency),
        "pendingDigestCount": pending_digest_count,
        "matchCount": r.match_count,
        "lastNotifiedAt": r.last_notified_at,
        "filterSchemaVersion": r.filter_schema_version,
        "filtersRefreshedAt": r.filters_refreshed_at,
        "createdAt": r.created_at,
        "updatedAt": r.updated_at,
    });
    if include_token {
        obj["unsubscribeToken"] = json!(r.unsubscribe_token);
    }
    let (prof_opt, is_explicit, _) = extract_subscription_profile(
        &r.parsed_filters_json,
        &r.requirements_text,
        &serde_json::from_value::<SearchFilters>(parsed_filters.clone()).unwrap_or_default(),
    );
    if is_explicit {
        if let Some(p) = prof_opt {
            obj["hasProfile"] = json!(true);
            if let Ok(pv) = serde_json::to_value(p) {
                obj["profile"] = pv;
            }
        }
    }
    obj
}

pub async fn create_subscription(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    req: CreateSubscriptionRequest,
) -> Result<Value> {
    create_subscription_with_profile_and_provider(db, http, env, req, None, None).await
}

pub async fn create_subscription_with_profile(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    req: CreateSubscriptionRequest,
    profile: Option<&Profile>,
) -> Result<Value> {
    create_subscription_with_profile_and_provider(db, http, env, req, profile, None).await
}

pub async fn create_subscription_with_profile_and_provider(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    mut req: CreateSubscriptionRequest,
    profile: Option<&Profile>,
    provider: Option<&dyn LiveZipListingProvider>,
) -> Result<Value> {
    db.ensure_schema().await?;
    if let Some(p) = profile {
        if let Err(errs) = p.validate() {
            let msg = errs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SearchValidationError::new("INVALID_PROFILE", msg, 400).into());
        }
    }

    let clean_email = validate_email_input(&req.email)?;
    let clean_name_str = clean_untrusted_text(&req.name, 80);
    let clean_name = if clean_name_str.is_empty() {
        None
    } else {
        Some(clean_name_str)
    };
    let explicit_zips = validate_zip_list(&req.zips, 5, true)?;
    let frequency = normalize_frequency(&req.frequency);

    if req.requirements.trim().is_empty() && profile.is_some() && !explicit_zips.is_empty() {
        req.requirements = format!("Saved Your Match profile in ZIP {}", explicit_zips.join(", "));
    }

    let mut parsed = parse_requirements_to_query(
        db,
        http,
        env,
        &req.requirements,
        &explicit_zips,
        None,
    )
    .await?;

    let final_zips = validate_zip_list(&parsed.filters.zips, 5, false)?;
    parsed.filters.zips = final_zips.clone();
    parsed.filters.schema_version = Some(CURRENT_FILTER_SCHEMA_VERSION);

    let id = Uuid::new_v4().to_string();
    let token = generate_unsubscribe_token();
    let now = now_iso();

    let zips_json = serde_json::to_string(&final_zips)?;
    let cities_json = serde_json::to_string(&parsed.filters.cities)?;
    let mut filters_val = serde_json::to_value(&parsed.filters)?;
    if let Some(p) = profile {
        if let Some(obj) = filters_val.as_object_mut() {
            obj.insert("profileJson".to_string(), serde_json::to_value(p)?);
        }
    }
    let parsed_filters_json = serde_json::to_string(&filters_val)?;

    let new_sub_row = SubscriptionRow {
        id: id.clone(),
        email: clean_email.clone(),
        name: clean_name.clone(),
        zips_json,
        cities_json: Some(cities_json),
        requirements_text: parsed.sanitized_query.clone(),
        parsed_filters_json: parsed_filters_json.clone(),
        notify_on_new: i64::from(req.notify_on_new),
        notify_on_sold: i64::from(req.notify_on_sold),
        active: 1,
        unsubscribe_token: token.clone(),
        last_notified_at: None,
        match_count: 0,
        filter_schema_version: CURRENT_FILTER_SCHEMA_VERSION,
        plan_tier: None,
        filters_refreshed_at: Some(now.clone()),
        frequency: frequency.to_string(),
        digest_pending_json: None,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    db.upsert_subscription_normalized(&new_sub_row).await?;

    refresh_watched_zip_subscriber_counts(db, &final_zips).await?;

    let mut initial_sync_results = Vec::new();
    if req.sync_now {
        for z in final_zips.iter().take(2) {
            let never_synced = db
                .get_watched_zip(z)
                .await?
                .and_then(|w| w.last_synced_at)
                .is_none();
            if never_synced {
                if let Ok(sr) =
                    sync_zip_listings_with_provider(db, http, env, z, false, provider).await
                {
                    initial_sync_results.push(json!({
                        "zip": z,
                        "activeCount": sr.active_count,
                        "soldCount": sr.sold_count,
                    }));
                }
            }
        }
    }

    let (prof_opt, is_explicit_profile, sub_lang) = extract_subscription_profile(
        &parsed_filters_json,
        &parsed.sanitized_query,
        &parsed.filters,
    );
    let initial_matches = if req.send_welcome_email {
        query_matching_listings_for_subscription(
            db,
            http,
            env,
            &id,
            &final_zips,
            &parsed.filters,
            req.notify_on_new,
            req.notify_on_sold,
            prof_opt.as_ref(),
            is_explicit_profile,
            sub_lang,
            6,
        )
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    let welcome_delivery = if req.send_welcome_email && env.smtp_configured() {
        let brand_name = env.brand_name();
        let brand_dom = env.brand_domain();
        let lang_code = sub_lang.code();

        let payload = if !initial_matches.is_empty() {
            let (origin, eff_brand, _eff_domain) =
                resolve_origin_and_brand(&req.base_origin, &brand_name, &brand_dom);
            let zips_str = final_zips.join(", ");
            let lang_id = resolve_lang_str(lang_code);
            let welcome_subject =
                t_brand_args(&lang_id, "email-welcome-subject", &eff_brand, &[("zips", &zips_str)]);
            let homes_heading = t_args(
                &lang_id,
                "email-welcome-homes-heading",
                &[
                    ("count", &initial_matches.len().to_string()),
                    ("zips", &zips_str),
                ],
            );
            render_home_digest_email(
                clean_name.as_deref(),
                &parsed.sanitized_query,
                &final_zips,
                &initial_matches,
                &origin,
                &token,
                lang_code,
                &brand_name,
                &brand_dom,
                Some(&welcome_subject),
                Some(&homes_heading),
                Some(frequency),
            )
        } else {
            build_welcome_confirm_email_with_brand(
                clean_name.as_deref(),
                &final_zips,
                frequency,
                &req.base_origin,
                &token,
                lang_code,
                &brand_name,
                &brand_dom,
            )
        };

        let email_req = EmailRequest {
            to: vec![clean_email.clone()],
            subject: payload.subject,
            text: Some(payload.text),
            html: Some(payload.html),
            reply_to: None,
            unsubscribe_url: Some(payload.unsubscribe_url),
        };

        match send_smtp_email(env, &email_req).await {
            Ok(_) => json!({
                "sent": true,
                "status": "sent",
                "initialMatchCount": initial_matches.len(),
            }),
            Err(e) => {
                let msg: String = e.to_string().chars().take(180).collect();
                json!({
                    "sent": false,
                    "status": "failed",
                    "error": msg,
                    "initialMatchCount": initial_matches.len(),
                })
            }
        }
    } else if req.send_welcome_email {
        json!({
            "sent": false,
            "status": "smtp_not_configured",
            "initialMatchCount": initial_matches.len(),
        })
    } else {
        json!({
            "sent": false,
            "status": "skipped",
            "initialMatchCount": 0,
        })
    };

    let mut sub_obj = json!({
        "id": id,
        "email": clean_email,
        "name": clean_name,
        "zips": final_zips,
        "requirementsText": parsed.sanitized_query,
        "intentSummary": parsed.filters.intent_summary,
        "parsedFilters": filters_val,
        "notifyOnNew": req.notify_on_new,
        "notifyOnSold": req.notify_on_sold,
        "active": true,
        "frequency": frequency,
        "unsubscribeToken": token,
        "filterSchemaVersion": CURRENT_FILTER_SCHEMA_VERSION,
        "createdAt": now,
    });
    if let Some(p) = profile {
        sub_obj["hasProfile"] = json!(true);
        sub_obj["profile"] = serde_json::to_value(p)?;
    }

    Ok(json!({
        "ok": true,
        "subscription": sub_obj,
        "model": parsed.model,
        "effort": parsed.effort,
        "initialSyncResults": initial_sync_results,
        "welcomeDelivery": welcome_delivery,
    }))
}

pub async fn list_subscriptions(
    db: &DbStore,
    email: Option<&str>,
    token: Option<&str>,
) -> Result<Vec<Value>> {
    db.ensure_schema().await?;
    let include_token = token.map(|t| !t.trim().is_empty()).unwrap_or(false);
    let rows = if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        db.fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE unsubscribe_token = ?",
            &[tok.into()],
        )
        .await?
    } else if let Some(em) = email.map(str::trim).filter(|s| !s.is_empty()) {
        let clean_email = validate_email_input(em)?;
        db.fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE LOWER(email) = LOWER(?) AND active = 1 ORDER BY created_at DESC",
            &[clean_email.into()],
        )
        .await?
    } else {
        return Ok(Vec::new());
    };

    Ok(rows
        .into_iter()
        .map(|r| subscription_to_json(&r, include_token))
        .collect())
}

pub async fn list_subscriptions_by_manage_token(
    db: &DbStore,
    manage_token: &str,
) -> Result<Option<(String, Vec<Value>)>> {
    db.ensure_schema().await?;
    let Some(email) = db.resolve_alert_manage_token(manage_token).await? else {
        return Ok(None);
    };
    let rows = db.list_subscriptions_for_email_all(&email).await?;
    let subs = rows
        .into_iter()
        .map(|r| subscription_to_json(&r, true))
        .collect();
    Ok(Some((email, subs)))
}

pub async fn create_and_send_manage_link(
    db: &DbStore,
    env: &EnvConfig,
    raw_email: &str,
    base_origin: &str,
    lang: &str,
) -> Result<Value> {
    db.ensure_schema().await?;
    let clean_email = validate_email_input(raw_email)?;
    let subs = db.list_subscriptions_for_email_all(&clean_email).await?;
    if subs.is_empty() {
        return Ok(json!({
            "ok": true,
            "sent": false,
            "status": "no_subscriptions",
            "subscriptionCount": 0,
        }));
    }

    let manage_token = generate_unsubscribe_token();
    let now_dt = chrono::Utc::now();
    let created_at = now_dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let expires_at =
        (now_dt + chrono::Duration::hours(24)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    db.create_alert_manage_token(&manage_token, &clean_email, &expires_at, &created_at)
        .await?;

    let brand_name = env.brand_name();
    let brand_dom = env.brand_domain();
    let email_payload = build_manage_link_email_with_brand(
        base_origin,
        &manage_token,
        lang,
        &brand_name,
        &brand_dom,
    );

    if env.smtp_configured() {
        let email_req = EmailRequest {
            to: vec![clean_email],
            subject: email_payload.subject,
            text: Some(email_payload.text),
            html: Some(email_payload.html),
            reply_to: None,
            unsubscribe_url: None,
        };
        match send_smtp_email(env, &email_req).await {
            Ok(_) => Ok(json!({
                "ok": true,
                "sent": true,
                "status": "sent",
                "subscriptionCount": subs.len(),
                "expiresAt": expires_at,
            })),
            Err(e) => {
                let msg: String = e.to_string().chars().take(180).collect();
                Ok(json!({
                    "ok": true,
                    "sent": false,
                    "status": "failed",
                    "error": msg,
                    "subscriptionCount": subs.len(),
                    "expiresAt": expires_at,
                }))
            }
        }
    } else {
        Ok(json!({
            "ok": true,
            "sent": false,
            "status": "smtp_not_configured",
            "subscriptionCount": subs.len(),
            "expiresAt": expires_at,
        }))
    }
}

pub async fn authorize_subscription_access(
    db: &DbStore,
    id: &str,
    unsubscribe_token: Option<&str>,
    manage_token: Option<&str>,
) -> Result<SubscriptionRow> {
    db.ensure_schema().await?;
    let clean_id = id.trim();
    if clean_id.is_empty() {
        return Err(SearchValidationError::new(
            "SUBSCRIPTION_NOT_FOUND",
            "Subscription not found.",
            404,
        )
        .into());
    }
    let Some(sub) = db.fetch_subscription_by_id(clean_id).await? else {
        return Err(SearchValidationError::new(
            "SUBSCRIPTION_NOT_FOUND",
            "Subscription not found.",
            404,
        )
        .into());
    };

    let unsub_tok = unsubscribe_token
        .map(str::trim)
        .filter(|s| s.len() >= 12);
    let manage_tok = manage_token.map(str::trim).filter(|s| s.len() >= 12);
    if unsub_tok.is_none() && manage_tok.is_none() {
        return Err(SearchValidationError::new(
            "UNAUTHORIZED",
            "A valid alert token or manage link token is required.",
            401,
        )
        .into());
    }

    if let Some(tok) = unsub_tok {
        if tok == sub.unsubscribe_token {
            return Ok(sub);
        }
    }
    if let Some(mtok) = manage_tok {
        let Some(token_email) = db.resolve_alert_manage_token(mtok).await? else {
            return Err(SearchValidationError::new(
                "INVALID_OR_EXPIRED_TOKEN",
                "This alert management link is invalid or has expired.",
                401,
            )
            .into());
        };
        if token_email.eq_ignore_ascii_case(&sub.email) {
            return Ok(sub);
        }
    }

    Err(SearchValidationError::new(
        "FORBIDDEN",
        "You are not authorized to access this alert subscription.",
        403,
    )
    .into())
}

async fn check_free_active_alert_quota(
    db: &DbStore,
    env: &EnvConfig,
    email: &str,
) -> Result<Option<Value>> {
    let Some(max_free_alerts) = env
        .get("FREE_MAX_ACTIVE_NL_ALERTS")
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return Ok(None);
    };
    let email_norm = email.trim().to_ascii_lowercase();
    if !email_norm.contains('@') {
        return Ok(None);
    }
    let active_count = db
        .fetch_scalar_i64(
            "SELECT COUNT(*) AS cnt FROM subscriptions WHERE active = 1 AND LOWER(email) = LOWER(?)",
            &[SqlParam::from(email_norm.as_str())],
            "cnt",
        )
        .await?
        .max(0) as u32;
    if active_count >= max_free_alerts {
        let upgrade_url = env
            .get("UPGRADE_URL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| "/?upgrade=alerts#billing".to_string());
        return Ok(Some(json!({
            "ok": false,
            "quotaExceeded": true,
            "status": 402,
            "code": "PAYWALL_ALERT_LIMIT_REACHED",
            "error": format!(
                "Active natural-language alert limit reached ({active_count}/{max_free_alerts}). Upgrade your plan to add more active property alerts."
            ),
            "activeCount": active_count,
            "maxActiveNlAlerts": max_free_alerts,
            "upgradeUrl": upgrade_url,
        })));
    }
    Ok(None)
}

async fn apply_subscription_patch(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    sub: SubscriptionRow,
    patch: UpdateSubscriptionPatch,
    include_token: bool,
) -> Result<Value> {
    let old_zips: Vec<String> = serde_json::from_str(&sub.zips_json).unwrap_or_default();
    let old_filters_val: Value =
        serde_json::from_str(&sub.parsed_filters_json).unwrap_or_else(|_| json!({}));

    let next_name = match patch.name {
        Some(n) => {
            let cleaned = clean_untrusted_text(&n, 80);
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        }
        None => sub.name.clone(),
    };

    let explicit_zips_opt = match patch.zips {
        Some(raw_zips) => Some(validate_zip_list(&raw_zips, 5, false)?),
        None => None,
    };

    let mut next_zips = explicit_zips_opt.clone().unwrap_or_else(|| old_zips.clone());
    let mut next_cities_json = sub.cities_json.clone().unwrap_or_else(|| "[]".to_string());
    let mut next_requirements = sub.requirements_text.clone();
    let mut next_filters_json = sub.parsed_filters_json.clone();
    let mut next_schema_version = sub.filter_schema_version;
    let mut next_refreshed_at = sub.filters_refreshed_at.clone();
    let mut clear_pending_digest = false;
    let now = now_iso();

    if let Some(new_req_raw) = patch.requirements {
        let hint_zips = explicit_zips_opt.clone().unwrap_or_else(|| old_zips.clone());
        let mut parsed = parse_requirements_to_query(
            db,
            http,
            env,
            &new_req_raw,
            &hint_zips,
            None,
        )
        .await?;
        let final_zips = if let Some(ez) = explicit_zips_opt {
            ez
        } else {
            validate_zip_list(&parsed.filters.zips, 5, false)?
        };
        parsed.filters.zips = final_zips.clone();
        parsed.filters.schema_version = Some(CURRENT_FILTER_SCHEMA_VERSION);
        let mut next_val = serde_json::to_value(&parsed.filters)?;
        if let (Some(old_obj), Some(next_obj)) =
            (old_filters_val.as_object(), next_val.as_object_mut())
        {
            for keep_key in ["profileJson", "profile_json", "profile", "lang"] {
                if let Some(v) = old_obj.get(keep_key) {
                    next_obj.insert(keep_key.to_string(), v.clone());
                }
            }
        }
        next_zips = final_zips;
        next_cities_json = serde_json::to_string(&parsed.filters.cities)?;
        next_requirements = parsed.sanitized_query;
        next_filters_json = serde_json::to_string(&next_val)?;
        next_schema_version = CURRENT_FILTER_SCHEMA_VERSION;
        next_refreshed_at = Some(now.clone());
        clear_pending_digest = true;
    } else if let Some(ez) = explicit_zips_opt {
        let mut updated_val = old_filters_val.clone();
        if let Some(obj) = updated_val.as_object_mut() {
            obj.insert("zips".to_string(), json!(ez));
        }
        next_zips = ez;
        next_filters_json = serde_json::to_string(&updated_val)?;
        next_refreshed_at = Some(now.clone());
        clear_pending_digest = true;
    }

    let next_notify_new = patch.notify_on_new.unwrap_or(sub.notify_on_new != 0);
    let next_notify_sold = patch.notify_on_sold.unwrap_or(sub.notify_on_sold != 0);
    let next_frequency = patch
        .frequency
        .as_deref()
        .map(normalize_frequency)
        .unwrap_or_else(|| normalize_frequency(&sub.frequency))
        .to_string();
    if next_frequency == "instant" && normalize_frequency(&sub.frequency) != "instant" {
        clear_pending_digest = true;
    }
    let next_active = patch.active.unwrap_or(sub.active != 0);
    let next_zips_json = serde_json::to_string(&next_zips)?;
    let next_digest_pending = if clear_pending_digest {
        None
    } else {
        sub.digest_pending_json.clone()
    };

    let updated_sub = SubscriptionRow {
        id: sub.id.clone(),
        email: sub.email.clone(),
        name: next_name,
        zips_json: next_zips_json,
        cities_json: Some(next_cities_json),
        requirements_text: next_requirements,
        parsed_filters_json: next_filters_json,
        notify_on_new: i64::from(next_notify_new),
        notify_on_sold: i64::from(next_notify_sold),
        active: i64::from(next_active),
        unsubscribe_token: sub.unsubscribe_token.clone(),
        last_notified_at: sub.last_notified_at.clone(),
        match_count: sub.match_count,
        filter_schema_version: next_schema_version,
        plan_tier: sub.plan_tier.clone(),
        filters_refreshed_at: next_refreshed_at,
        frequency: next_frequency,
        digest_pending_json: next_digest_pending,
        created_at: sub.created_at.clone(),
        updated_at: now,
    };
    db.upsert_subscription_normalized(&updated_sub).await?;

    let mut touched_zips = old_zips;
    for z in &next_zips {
        if !touched_zips.contains(z) {
            touched_zips.push(z.clone());
        }
    }
    refresh_watched_zip_subscriber_counts(db, &touched_zips).await?;

    let updated_row = db
        .fetch_subscription_by_id(&sub.id)
        .await?
        .ok_or_else(|| SearchValidationError::new("SUBSCRIPTION_NOT_FOUND", "Subscription not found.", 404))?;
    Ok(json!({
        "ok": true,
        "subscription": subscription_to_json(&updated_row, include_token),
    }))
}

pub async fn update_subscription_authorized(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    id: &str,
    unsubscribe_token: Option<&str>,
    manage_token: Option<&str>,
    patch: UpdateSubscriptionPatch,
) -> Result<Value> {
    let sub = authorize_subscription_access(db, id, unsubscribe_token, manage_token).await?;
    let resuming = patch.active == Some(true) && sub.active == 0;
    if resuming {
        if let Some(paywall) = check_free_active_alert_quota(db, env, &sub.email).await? {
            return Ok(paywall);
        }
    }
    apply_subscription_patch(db, http, env, sub, patch, true).await
}

#[allow(clippy::too_many_arguments)]
pub async fn update_subscription_authorized_with_quota<F, Fut>(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    id: &str,
    unsubscribe_token: Option<&str>,
    manage_token: Option<&str>,
    patch: UpdateSubscriptionPatch,
    resume_quota_check: F,
) -> Result<Value>
where
    F: FnOnce(SubscriptionRow) -> Fut,
    Fut: Future<Output = std::result::Result<(), Value>>,
{
    let sub = authorize_subscription_access(db, id, unsubscribe_token, manage_token).await?;
    let resuming = patch.active == Some(true) && sub.active == 0;
    if resuming {
        if let Err(mut paywall) = resume_quota_check(sub.clone()).await {
            if let Some(obj) = paywall.as_object_mut() {
                obj.entry("ok".to_string()).or_insert_with(|| json!(false));
                obj.entry("quotaExceeded".to_string()).or_insert_with(|| json!(true));
                obj.entry("status".to_string()).or_insert_with(|| json!(402));
                obj.entry("code".to_string())
                    .or_insert_with(|| json!("PAYWALL_ALERT_LIMIT_REACHED"));
            }
            return Ok(paywall);
        }
        if let Some(paywall) = check_free_active_alert_quota(db, env, &sub.email).await? {
            return Ok(paywall);
        }
    }
    apply_subscription_patch(db, http, env, sub, patch, true).await
}

pub async fn delete_subscription_authorized(
    db: &DbStore,
    id: &str,
    unsubscribe_token: Option<&str>,
    manage_token: Option<&str>,
) -> Result<Value> {
    let sub = authorize_subscription_access(db, id, unsubscribe_token, manage_token).await?;
    let old_zips: Vec<String> = serde_json::from_str(&sub.zips_json).unwrap_or_default();
    let deleted = db.delete_subscription_by_id(&sub.id).await?;
    refresh_watched_zip_subscriber_counts(db, &old_zips).await?;
    Ok(json!({
        "ok": true,
        "deleted": deleted,
        "id": sub.id,
    }))
}

pub async fn preview_subscription_matches_authorized(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    id: &str,
    unsubscribe_token: Option<&str>,
    manage_token: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let sub = authorize_subscription_access(db, id, unsubscribe_token, manage_token).await?;
    let sub_zips: Vec<String> = serde_json::from_str(&sub.zips_json).unwrap_or_default();
    let filters: SearchFilters =
        serde_json::from_str(&sub.parsed_filters_json).unwrap_or_default();
    let (prof_opt, is_explicit_profile, lang) = extract_subscription_profile(
        &sub.parsed_filters_json,
        &sub.requirements_text,
        &filters,
    );
    let safe_limit = limit.clamp(1, 50);
    let items = query_matching_listings_for_subscription(
        db,
        http,
        env,
        &sub.id,
        &sub_zips,
        &filters,
        sub.notify_on_new != 0,
        sub.notify_on_sold != 0,
        prof_opt.as_ref(),
        is_explicit_profile,
        lang,
        safe_limit,
    )
    .await?;

    let matches: Vec<Value> = items
        .into_iter()
        .map(|item| {
            let mut obj = item.listing.clone();
            if let Some(m) = obj.as_object_mut() {
                m.insert("eventType".to_string(), json!(item.event_type));
                m.insert("oldPrice".to_string(), json!(item.old_price));
                let addr = str_from_val(m.get("address")).unwrap_or_default();
                let report_link = if let Some(rs) = str_from_val(m.get("reportSlug")) {
                    format!("/r/{}/", urlencoding::encode(&rs))
                } else {
                    format!("/report?address={}", urlencoding::encode(&addr))
                };
                m.insert("reportLink".to_string(), json!(report_link));
            }
            obj
        })
        .collect();

    Ok(json!({
        "ok": true,
        "subscriptionId": sub.id,
        "count": matches.len(),
        "matches": matches,
    }))
}

pub async fn admin_update_subscription_by_id(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    id: &str,
    patch: UpdateSubscriptionPatch,
) -> Result<Value> {
    db.ensure_schema().await?;
    let Some(sub) = db.fetch_subscription_by_id(id.trim()).await? else {
        return Err(SearchValidationError::new(
            "SUBSCRIPTION_NOT_FOUND",
            "Subscription not found.",
            404,
        )
        .into());
    };
    apply_subscription_patch(db, http, env, sub, patch, false).await
}

pub async fn admin_retranslate_subscription_by_id(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    id: &str,
) -> Result<Value> {
    db.ensure_schema().await?;
    let Some(sub) = db.fetch_subscription_by_id(id.trim()).await? else {
        return Err(SearchValidationError::new(
            "SUBSCRIPTION_NOT_FOUND",
            "Subscription not found.",
            404,
        )
        .into());
    };
    let req_txt = sub.requirements_text.clone();
    apply_subscription_patch(
        db,
        http,
        env,
        sub,
        UpdateSubscriptionPatch {
            requirements: Some(req_txt),
            ..Default::default()
        },
        false,
    )
    .await
}

pub async fn admin_delete_subscription_by_id(db: &DbStore, id: &str) -> Result<bool> {
    db.ensure_schema().await?;
    let clean_id = id.trim();
    if clean_id.is_empty() {
        return Ok(false);
    }
    let old_zips: Vec<String> = match db.fetch_subscription_by_id(clean_id).await? {
        Some(sub) => serde_json::from_str(&sub.zips_json).unwrap_or_default(),
        None => return Ok(false),
    };
    let deleted = db.delete_subscription_by_id(clean_id).await?;
    refresh_watched_zip_subscriber_counts(db, &old_zips).await?;
    Ok(deleted)
}

pub async fn list_subscription_notifications(
    db: &DbStore,
    subscription_id: &str,
    limit: usize,
) -> Result<Vec<Value>> {
    db.list_subscription_notifications(subscription_id, limit).await
}

pub async fn unsubscribe_by_token(db: &DbStore, raw_token: &str) -> Result<Value> {
    db.ensure_schema().await?;
    let token = raw_token.trim();
    if token.len() < 12 {
        return Ok(json!({ "ok": false, "found": false }));
    }
    let rows = db
        .fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE unsubscribe_token = ?",
            &[token.into()],
        )
        .await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(json!({ "ok": false, "found": false }));
    };

    db.set_subscription_active(&row.id, false).await?;

    let zips: Vec<String> = serde_json::from_str(&row.zips_json).unwrap_or_default();
    refresh_watched_zip_subscriber_counts(db, &zips).await?;

    Ok(json!({
        "ok": true,
        "found": true,
        "email": row.email,
        "zips": zips,
        "requirementsText": row.requirements_text,
    }))
}

fn parse_profile_value(v: &Value) -> Option<Profile> {
    if let Some(s) = v.as_str() {
        Profile::from_json(s).ok()
    } else if v.is_object() {
        serde_json::to_string(v)
            .ok()
            .and_then(|s| Profile::from_json(&s).ok())
    } else {
        None
    }
}

/// Maps structured [`SearchFilters`] criteria into a deterministic [`Profile`] when the filter
/// set contains concrete numeric/feature preferences.
pub fn profile_from_search_filters(filters: &SearchFilters) -> Option<Profile> {
    let has_any = filters.max_price.is_some()
        || filters.min_beds.is_some()
        || filters.min_baths.is_some()
        || filters.min_sqft.is_some()
        || filters.min_lot_sqft.is_some()
        || filters.min_school_score.is_some()
        || filters.min_school_grade.is_some()
        || filters.max_hoa_monthly.is_some()
        || filters.exclude_high_flood_risk
        || filters.exclude_high_wildfire_risk
        || !filters.property_types.is_empty()
        || !filters.must_have_keywords.is_empty()
        || !filters.nice_to_have_keywords.is_empty();
    if !has_any {
        return None;
    }

    let mut profile = PRESETS[0].to_profile();
    profile.preset = None;
    if let Some(mp) = filters.max_price.filter(|&p| p > 0) {
        profile.budget.max_price = Some(mp as f64);
        profile.weights.insert(Dimension::Cost, 28.0);
    }
    if let Some(b) = filters.min_beds.filter(|&b| b >= 0.0 && b <= 20.0) {
        profile.min_beds = Some(b);
    }
    if let Some(ba) = filters.min_baths.filter(|&b| b >= 0.0 && b <= 20.0) {
        profile.min_baths = Some(ba);
    }
    if let Some(sq) = filters.min_sqft.filter(|&s| s > 0 && s < 100_000) {
        profile.min_sqft = Some(sq as f64);
    }
    if let Some(lot) = filters.min_lot_sqft.filter(|&s| s > 0 && s < 50_000_000) {
        profile.min_lot_sqft = Some(lot as f64);
    }
    if profile.min_beds.is_some()
        || profile.min_baths.is_some()
        || profile.min_sqft.is_some()
        || profile.min_lot_sqft.is_some()
    {
        profile.weights.insert(Dimension::Homefit, 16.0);
    }
    if filters.min_school_score.is_some() || filters.min_school_grade.is_some() {
        profile.weights.insert(Dimension::Schools, 20.0);
    }
    for raw_pt in &filters.property_types {
        if let Some(kind) = PropertyKind::classify(raw_pt) {
            if kind != PropertyKind::Other && !profile.property_types.contains(&kind) {
                profile.property_types.push(kind);
            }
        }
    }
    if let Some(hoa) = filters.max_hoa_monthly.filter(|&h| h >= 0) {
        profile.hoa_max_monthly = Some(hoa as f64);
        if !profile.deal_breakers.contains(&Screen::HoaOver) {
            profile.deal_breakers.push(Screen::HoaOver);
        }
    }
    if filters.exclude_high_flood_risk && !profile.deal_breakers.contains(&Screen::SfhaFlood) {
        profile.deal_breakers.push(Screen::SfhaFlood);
    }
    if filters.exclude_high_wildfire_risk && !profile.deal_breakers.contains(&Screen::VeryHighFire)
    {
        profile.deal_breakers.push(Screen::VeryHighFire);
    }
    for kw in filters
        .must_have_keywords
        .iter()
        .chain(filters.nice_to_have_keywords.iter())
    {
        let low = kw.trim().to_ascii_lowercase();
        let feat = if low.contains("pool") {
            Some(Feature::Pool)
        } else if low.contains("fireplace") {
            Some(Feature::Fireplace)
        } else if low.contains("hardwood") {
            Some(Feature::Hardwood)
        } else if low.contains("office") || low.contains("study") {
            Some(Feature::Office)
        } else if low.contains("view") {
            Some(Feature::View)
        } else if low.contains("open") {
            Some(Feature::OpenPlan)
        } else if low.contains("ceiling") {
            Some(Feature::HighCeilings)
        } else if low.contains("light") || low.contains("sunroom") {
            Some(Feature::NaturalLight)
        } else if low.contains("ev") {
            Some(Feature::EvReady)
        } else if low.contains("workshop") {
            Some(Feature::WorkshopGarage)
        } else {
            None
        };
        if let Some(f) = feat {
            if !profile.feature_wishes.contains(&f) {
                profile.feature_wishes.push(f);
            }
        }
        if low.contains("adu") || low.contains("in-law") {
            profile.multigenerational = true;
        }
    }
    profile.validate().ok().map(|()| profile)
}

/// Extracts an explicit `Profile` from subscription `parsed_filters_json` (`profileJson` /
/// `profile_json` / `profile`) or JSON `requirements_text`, falling back to mapping `SearchFilters`.
pub fn extract_subscription_profile(
    parsed_filters_json: &str,
    requirements_text: &str,
    filters: &SearchFilters,
) -> (Option<Profile>, bool, Lang) {
    let raw_filters: Value = serde_json::from_str(parsed_filters_json).unwrap_or_else(|_| json!({}));
    let lang = raw_filters
        .get("lang")
        .and_then(Value::as_str)
        .map(Lang::parse)
        .unwrap_or(Lang::En);

    if let Some(pv) = raw_filters
        .get("profileJson")
        .or_else(|| raw_filters.get("profile_json"))
        .or_else(|| raw_filters.get("profile"))
    {
        if let Some(p) = parse_profile_value(pv) {
            return (Some(p), true, lang);
        }
    }

    let req_trim = requirements_text.trim();
    if req_trim.starts_with('{') {
        if let Ok(rv) = serde_json::from_str::<Value>(req_trim) {
            let inner = rv
                .get("profileJson")
                .or_else(|| rv.get("profile_json"))
                .or_else(|| rv.get("profile"))
                .unwrap_or(&rv);
            if let Some(p) = parse_profile_value(inner) {
                return (Some(p), true, lang);
            }
        }
    }

    (profile_from_search_filters(filters), false, lang)
}

fn est_monthly_ownership(price: i64, hoa: i64, state: Option<&str>) -> (f64, f64) {
    crate::scoring::factors::estimate_listing_monthly_ownership(price, hoa, state)
}

/// Converts a [`ListingRow`] into [`HomeFactors`] for deterministic `open_estate::scoring` evaluation.
pub fn home_factors_from_listing(listing: &ListingRow) -> HomeFactors {
    HomeFactors::from_listing_row(listing)
}

fn apply_listing_price_override(factors: &mut HomeFactors, listing: &ListingRow) {
    factors.id = listing.id.clone();
    if let Some(lp) = listing.list_price.or(listing.price).filter(|&p| p > 0) {
        let prev = factors.cost.price_for_budget();
        factors.cost.list_price = Some(Factor::new(lp as f64, "Listing price", Grade::Listing));
        if let Some(p) = listing.price.filter(|&p| p > 0) {
            factors.cost.price = Some(Factor::new(p as f64, "Listing price", Grade::Listing));
        }
        let price_changed = prev.map_or(true, |old| (old - lp as f64).abs() > 1.0);
        if factors.cost.monthly_total.is_none() || price_changed {
            let (total, pi) = est_monthly_ownership(
                lp,
                listing.hoa_monthly.unwrap_or(0),
                listing.state.as_deref(),
            );
            factors.cost.monthly_total =
                Some(Factor::new(total, "Estimated monthly cost model", Grade::Area).mark_estimated());
            factors.cost.monthly_principal_interest = Some(pi);
            factors.cost.assumed_down_payment_pct = Some(20.0);
        }
    }
}

fn format_scored_reasons_and_tradeoffs(scored: &ScoredHome, lang: Lang) -> (String, Option<String>) {
    let mut why_parts: Vec<String> = scored
        .reasons
        .iter()
        .filter_map(|r| r.text.clone())
        .collect();

    if why_parts.is_empty() {
        let mut sorted_dims: Vec<_> = scored
            .dimensions
            .iter()
            .filter(|d| !d.estimated && d.utility >= 55.0)
            .collect();
        sorted_dims.sort_by(|a, b| {
            b.points
                .total_cmp(&a.points)
                .then_with(|| a.dimension.cmp(&b.dimension))
        });
        for d in sorted_dims.into_iter().take(3) {
            why_parts.push(format!(
                "{}: {:.0}/100 ({:.1} pts)",
                d.dimension.label().get(lang),
                d.utility.round(),
                d.points
            ));
        }
        if why_parts.is_empty() {
            let mut any_dims: Vec<_> = scored.dimensions.iter().collect();
            any_dims.sort_by(|a, b| {
                b.points
                    .total_cmp(&a.points)
                    .then_with(|| a.dimension.cmp(&b.dimension))
            });
            for d in any_dims.into_iter().take(2) {
                why_parts.push(format!(
                    "{}: {:.0}/100",
                    d.dimension.label().get(lang),
                    d.utility.round()
                ));
            }
        }
    }

    if why_parts.is_empty() {
        why_parts.push(format!(
            "{} ({:.0}/100)",
            scored.band.label().get(lang),
            scored.score.round()
        ));
    }

    let mut tradeoff_parts: Vec<String> = scored
        .trade_offs
        .iter()
        .filter_map(|t| t.text.clone())
        .collect();

    if tradeoff_parts.is_empty() {
        let mut weak_dims: Vec<_> = scored
            .dimensions
            .iter()
            .filter(|d| !d.estimated && d.utility < 60.0)
            .collect();
        weak_dims.sort_by(|a, b| {
            a.utility
                .total_cmp(&b.utility)
                .then_with(|| a.dimension.cmp(&b.dimension))
        });
        for d in weak_dims.into_iter().take(2) {
            tradeoff_parts.push(format!(
                "{}: {:.0}/100",
                d.dimension.label().get(lang),
                d.utility.round()
            ));
        }
    }
    if !scored.capped_by.is_empty() {
        let names = scored
            .capped_by
            .iter()
            .map(|d| d.label().get(lang))
            .collect::<Vec<_>>()
            .join(", ");
        tradeoff_parts.push(format!("Capped below Strong match on {names}"));
    }

    let why_it_matches = why_parts.join("; ");
    let tradeoffs = if tradeoff_parts.is_empty() {
        None
    } else {
        Some(tradeoff_parts.join("; "))
    };
    (why_it_matches, tradeoffs)
}

fn listing_matches_structured_criteria(l: &ListingRow, f: &SearchFilters) -> bool {
    if let Some(min_safe) = f.min_safety_score {
        if !l.safety_score.map(|s| s >= min_safe).unwrap_or(false) {
            return false;
        }
    }
    if f.has_hardwood_floors && l.has_hardwood_floors != Some(1) {
        return false;
    }
    if f.has_updated_kitchen && l.has_updated_kitchen != Some(1) {
        return false;
    }
    if f.remodeled_only && l.has_remodeled != Some(1) {
        return false;
    }
    if f.quiet_street_only && l.quiet_street != Some(1) {
        return false;
    }
    if let Some(max_db) = f.max_noise_db {
        if !l.noise_max_db.map(|d| d <= max_db).unwrap_or(false) {
            return false;
        }
    }
    if let Some(max_pga) = f.max_seismic_pga {
        if !l.seismic_pga.map(|p| p <= max_pga).unwrap_or(false) {
            return false;
        }
    }
    if !f.required_tags.is_empty() {
        let (l_tag_bits, _, _) = proto::compute_listing_tag_bits(l);
        let tags_lc = l.tags_json.as_deref().unwrap_or("").to_ascii_lowercase();
        let feat_lc = l.features_text.as_deref().unwrap_or("").to_ascii_lowercase();
        for tag in &f.required_tags {
            let clean = tag.trim().to_ascii_lowercase().replace(' ', "_");
            if clean.is_empty() {
                continue;
            }
            if let Some(ft) = proto::feature_tag_from_slug(&clean) {
                if (l_tag_bits & proto::feature_tag_bit(ft)) != 0 {
                    continue;
                }
            }
            let words = clean.replace('_', " ");
            if !tags_lc.contains(&clean) && !feat_lc.contains(&words) {
                return false;
            }
        }
    }
    true
}

fn parse_pending_digest_items(raw: Option<&str>) -> Vec<AlertMatchedItem> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && *s != "[]")
        .and_then(|s| serde_json::from_str::<Vec<AlertMatchedItem>>(s).ok())
        .unwrap_or_default()
}

fn merge_digest_items(
    existing_raw: Option<&str>,
    incoming: Vec<AlertMatchedItem>,
) -> Vec<AlertMatchedItem> {
    let mut merged = parse_pending_digest_items(existing_raw);
    for item in incoming {
        if merged.iter().any(|m| m.event_key == item.event_key) {
            continue;
        }
        if let Some(lid) = str_from_val(item.listing.get("id")) {
            if let Some(pos) = merged
                .iter()
                .position(|m| str_from_val(m.listing.get("id")).as_deref() == Some(lid.as_str()))
            {
                merged[pos] = item;
                continue;
            }
        }
        merged.push(item);
    }
    merged.sort_by(|a, b| {
        b.match_score.cmp(&a.match_score).then_with(|| {
            let ia = str_from_val(a.listing.get("id")).unwrap_or_default();
            let ib = str_from_val(b.listing.get("id")).unwrap_or_default();
            ia.cmp(&ib)
        })
    });
    merged.truncate(200);
    merged
}

fn is_subscription_digest_due_ts(
    freq_code: i32,
    last_notified_ts: Option<i64>,
    created_ts: i64,
    now_ts: i64,
) -> bool {
    let interval = match freq_code {
        2 => 86_400_i64,
        3 => 604_800_i64,
        _ => 0_i64,
    };
    if interval <= 0 {
        return true;
    }
    let anchor_ts = last_notified_ts.unwrap_or(created_ts);
    if anchor_ts <= 0 {
        return true;
    }
    now_ts.saturating_sub(anchor_ts) >= interval
}

/// Reconstructs `(SearchFilters, Option<Profile>, is_explicit_profile, Lang)` directly from
/// a normalized `SubscriptionCoreProto` with zero JSON round-tripping on standard rows.
fn search_filters_and_profile_from_proto(
    core: &SubscriptionCoreProto,
) -> (SearchFilters, Option<Profile>, bool, Lang) {
    let mut statuses = Vec::new();
    if (core.status_mask & 1) != 0 {
        statuses.push("for_sale".to_string());
    }
    if (core.status_mask & 2) != 0 {
        statuses.push("pending".to_string());
    }
    if (core.status_mask & 4) != 0 {
        statuses.push("sold".to_string());
    }
    if (core.status_mask & 8) != 0 {
        statuses.push("off_market".to_string());
    }
    if statuses.is_empty() {
        statuses.push("for_sale".to_string());
    }

    let mut prop_types = Vec::new();
    for code in 1..=6 {
        if (core.property_cat_mask & (1 << code)) != 0 {
            if let Some(lbl) = proto::property_category_from_code(code) {
                prop_types.push(lbl.to_string());
            }
        }
    }

    let mut must_have_keywords = Vec::new();
    let mut nice_to_have_keywords = Vec::new();
    let mut deal_breakers = Vec::new();
    let mut internet_carriers = Vec::new();
    let mut electric_carriers = Vec::new();
    let mut states = Vec::new();
    let mut flags: HashSet<&str> = HashSet::new();
    let mut architectural_styles = Vec::new();
    let mut required_tags = Vec::new();

    for kw in &core.keywords {
        match kw.term_kind {
            1 => must_have_keywords.push(kw.term_value.clone()),
            2 => nice_to_have_keywords.push(kw.term_value.clone()),
            3 => deal_breakers.push(kw.term_value.clone()),
            4 => internet_carriers.push(kw.term_value.clone()),
            5 => electric_carriers.push(kw.term_value.clone()),
            6 => {
                flags.insert(kw.term_value.as_str());
            }
            7 => states.push(kw.term_value.clone()),
            8 => architectural_styles.push(kw.term_value.clone()),
            9 => required_tags.push(kw.term_value.clone()),
            _ => {}
        }
    }

    let filters = SearchFilters {
        on_topic: !flags.contains("off_topic"),
        intent_summary: core
            .intent_summary
            .clone()
            .unwrap_or_else(|| "Parsed criteria".to_string()),
        schema_version: Some(core.filter_schema_version as i64),
        zips: core.zips.clone(),
        cities: core.cities.clone(),
        states,
        statuses,
        min_price: core.min_price,
        max_price: core.max_price,
        min_beds: core.min_beds,
        max_beds: core.max_beds,
        min_baths: core.min_baths,
        min_sqft: core.min_sqft.map(i64::from),
        max_sqft: core.max_sqft.map(i64::from),
        min_lot_sqft: core.min_lot_sqft.map(i64::from),
        min_year_built: core.min_year_built.map(i64::from),
        max_year_built: core.max_year_built.map(i64::from),
        stories: core.stories.map(i64::from),
        property_types: prop_types,
        max_hoa_monthly: core.max_hoa_monthly.map(i64::from),
        min_school_score: core.min_school_score,
        min_school_grade: proto::school_grade_from_code(core.min_school_grade),
        must_have_keywords,
        nice_to_have_keywords,
        deal_breakers,
        fair_housing_notice: core.fair_housing_notice.clone(),
        exclude_high_flood_risk: flags.contains("exclude_high_flood_risk")
            || (core.excluded_tag_bits & proto::feature_tag_bit(FeatureTag::InSfhaFlood)) != 0,
        exclude_high_wildfire_risk: flags.contains("exclude_high_wildfire_risk")
            || (core.excluded_tag_bits & proto::feature_tag_bit(FeatureTag::HighWildfire)) != 0,
        fiber_only: flags.contains("fiber_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::FiberAvailable)) != 0,
        high_speed_internet_only: flags.contains("high_speed_internet_only"),
        min_download_mbps: core.min_download_mbps.map(i64::from),
        internet_carriers,
        electric_carriers,
        municipal_electric_only: flags.contains("municipal_electric_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::MunicipalElectric))
                != 0,
        max_electric_rate_kwh: core.max_electric_rate_kwh,
        municipal_water_only: flags.contains("municipal_water_only"),
        municipal_sewer_only: flags.contains("municipal_sewer_only"),
        max_monthly_utilities: core.max_monthly_utilities.map(i64::from),
        max_all_in_monthly: core.max_all_in_monthly.map(i64::from),
        exclude_special_tax: flags.contains("exclude_special_tax")
            || (core.excluded_tag_bits & proto::feature_tag_bit(FeatureTag::HasSpecialTax)) != 0,
        has_solar: flags.contains("has_solar")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Solar)) != 0,
        has_ev_charger: flags.contains("has_ev_charger")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::EvCharger)) != 0,
        has_heat_pump: flags.contains("has_heat_pump")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::HeatPump)) != 0,
        has_adu: flags.contains("has_adu")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Adu)) != 0,
        adu_feasible: flags.contains("adu_feasible")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::AduFeasible)) != 0,
        has_pool: flags.contains("has_pool")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Pool)) != 0,
        min_garage_spaces: core.min_garage_spaces,
        architectural_styles,
        min_safety_score: core.min_safety_score,
        has_hardwood_floors: flags.contains("has_hardwood_floors")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::HardwoodFloors)) != 0,
        has_updated_kitchen: flags.contains("has_updated_kitchen")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::UpdatedKitchen)) != 0,
        remodeled_only: flags.contains("remodeled_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Remodeled)) != 0,
        quiet_street_only: flags.contains("quiet_street_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::QuietStreet)) != 0,
        max_noise_db: core.max_noise_db,
        max_seismic_pga: core.max_seismic_pga,
        required_tags,
        sort_by: core
            .sort_by
            .clone()
            .unwrap_or_else(|| "best_match".to_string()),
    };

    let lang = match core.lang_code {
        2 => Lang::parse("zh-CN"),
        3 => Lang::parse("es"),
        _ => Lang::En,
    };

    if core.has_explicit_profile {
        if let Some(ref up) = core.profile {
            let prof = proto::buyer_profile_from_proto(up);
            return (filters, Some(prof), true, lang);
        }
    }

    let req_trim = core.requirements_text.trim();
    if req_trim.starts_with('{') {
        if let Ok(rv) = serde_json::from_str::<Value>(req_trim) {
            let inner = rv
                .get("profileJson")
                .or_else(|| rv.get("profile_json"))
                .or_else(|| rv.get("profile"))
                .unwrap_or(&rv);
            if let Some(p) = parse_profile_value(inner) {
                return (filters, Some(p), true, lang);
            }
        }
    }

    let fallback = profile_from_search_filters(&filters);
    (filters, fallback, false, lang)
}

fn enrich_factors_from_report_header(hf: &mut HomeFactors, hdr: &ReportHeaderProto) {
    if hf.cost.monthly_total.is_none() {
        if let Some(mt) = hdr.all_in_monthly.filter(|&m| m > 0) {
            hf.cost.monthly_total = Some(Factor::new(
                mt as f64,
                "Open Estate cached report header",
                Grade::Report,
            ));
        }
    }
    if hf.cost.price.is_none() {
        if let Some(ev) = hdr.estimated_value.or(hdr.list_price).filter(|&p| p > 0) {
            hf.cost.price = Some(Factor::new(
                ev as f64,
                "Open Estate cached report header",
                Grade::Report,
            ));
        }
    }
}

/// Batches `listing_sk` resolution + typed `get_home_factors_by_sk_batch` + narrow
/// `get_report_headers_batch` fallback across all candidate listings (ZERO N+1 `get_cached_report`).
async fn prefetch_factors_for_listings(
    db: &DbStore,
    listings: &[&ListingRow],
) -> Result<(HashMap<String, i64>, HashMap<String, HomeFactors>)> {
    if listings.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }

    let mut seen_ids = HashSet::with_capacity(listings.len());
    let mut unique_listings: Vec<&ListingRow> = Vec::with_capacity(listings.len());
    for &l in listings {
        let id = l.id.trim();
        if !id.is_empty() && seen_ids.insert(id.to_string()) {
            unique_listings.push(l);
        }
    }
    if unique_listings.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }

    let mut sk_by_id: HashMap<String, i64> = HashMap::with_capacity(unique_listings.len());
    let ext_ids: Vec<&str> = unique_listings.iter().map(|l| l.id.trim()).collect();
    for chunk in ext_ids.chunks(200) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT external_id, listing_sk FROM listings WHERE external_id IN ({ph})"
        );
        let params: Vec<SqlParam> = chunk.iter().map(|&id| SqlParam::from(id)).collect();
        let pairs = db
            .fetch_all_mapped_raw(&sql, &params, |r| {
                (
                    r.opt_str("external_id").unwrap_or_default(),
                    r.opt_i64("listing_sk").unwrap_or(0),
                )
            })
            .await
            .unwrap_or_default();
        for (ext, sk) in pairs {
            if !ext.is_empty() && sk > 0 {
                sk_by_id.insert(ext, sk);
            }
        }
    }

    let mut all_sks: Vec<i64> = Vec::with_capacity(unique_listings.len());
    for l in &unique_listings {
        let sk = *sk_by_id
            .entry(l.id.clone())
            .or_insert_with(|| deterministic_sk(l.id.trim()));
        if sk > 0 {
            all_sks.push(sk);
        }
    }

    let sk_factors = db
        .get_home_factors_by_sk_batch(&all_sks)
        .await
        .unwrap_or_default();

    let mut missing_slugs: Vec<String> = Vec::new();
    let mut seen_slugs: HashSet<String> = HashSet::new();
    for l in &unique_listings {
        let sk = sk_by_id.get(&l.id).copied().unwrap_or(0);
        if !sk_factors.contains_key(&sk) {
            if let Some(slug) = l
                .report_slug
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if seen_slugs.insert(slug.to_string()) {
                    missing_slugs.push(slug.to_string());
                }
            }
        }
    }

    let headers_by_slug = if missing_slugs.is_empty() {
        HashMap::new()
    } else {
        db.get_report_headers_batch(&missing_slugs)
            .await
            .unwrap_or_default()
    };

    let factors_sec_bit = 1_i32 << (ReportSectionKind::ReportSectionFactors as u32);
    let mut factors_by_id: HashMap<String, HomeFactors> =
        HashMap::with_capacity(unique_listings.len());

    for l in unique_listings {
        let sk = sk_by_id.get(&l.id).copied().unwrap_or(0);
        let mut hf = if let Some(cached) = sk_factors.get(&sk) {
            cached.clone()
        } else if let Some(hdr) = l
            .report_slug
            .as_deref()
            .map(str::trim)
            .and_then(|s| headers_by_slug.get(s))
        {
            let mut from_sec: Option<HomeFactors> = None;
            if (hdr.section_mask & factors_sec_bit) != 0 {
                if let Ok(Some(sec_env)) = db
                    .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionFactors)
                    .await
                {
                    if let Ok(val) = extract_section_json(&sec_env) {
                        from_sec = serde_json::from_value::<HomeFactors>(val).ok();
                    }
                }
            }
            if let Some(parsed_hf) = from_sec {
                parsed_hf
            } else {
                let mut fallback_hf = home_factors_from_listing(l);
                enrich_factors_from_report_header(&mut fallback_hf, hdr);
                fallback_hf
            }
        } else {
            home_factors_from_listing(l)
        };

        apply_listing_price_override(&mut hf, l);
        factors_by_id.insert(l.id.clone(), hf);
    }

    Ok((sk_by_id, factors_by_id))
}

#[allow(clippy::too_many_arguments)]
async fn score_listing_events_for_subscription(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    sub_id: &str,
    sub_requirements: &str,
    sub_zips: &[String],
    sub_cities: &[String],
    filters: &SearchFilters,
    notify_on_new: bool,
    notify_on_sold: bool,
    profile_opt: Option<&Profile>,
    is_explicit_profile: bool,
    lang: Lang,
    events: &[ListingChangeEvent],
    skip_event_key_dedup: bool,
    already_sent_keys: Option<&HashSet<String>>,
    prefetched_factors: Option<&HashMap<String, HomeFactors>>,
    allow_llm_explain: bool,
) -> Result<(Vec<AlertMatchedItem>, bool)> {
    let mut eligible: Vec<(&ListingChangeEvent, String)> = Vec::new();
    for ch in events {
        let l = &ch.listing;
        let zip_match = sub_zips.contains(&l.zip);
        let city_match = !sub_cities.is_empty()
            && l.city
                .as_deref()
                .map(|c| {
                    let cn = c.trim();
                    sub_cities.iter().any(|sc| sc.eq_ignore_ascii_case(cn))
                })
                .unwrap_or(false);
        if !zip_match && !city_match {
            continue;
        }
        let is_sold_ev = ch.event_type == "sold" || l.status == "sold";
        if is_sold_ev && !notify_on_sold {
            continue;
        }
        if !is_sold_ev && !notify_on_new {
            continue;
        }

        let event_key = format!(
            "{}:{}:{}:{}",
            sub_id,
            l.id,
            l.status,
            l.price.unwrap_or(0)
        );
        if !skip_event_key_dedup {
            let exists = if let Some(sent_set) = already_sent_keys {
                sent_set.contains(&event_key)
            } else {
                db.has_notification_event_key(&event_key).await?
            };
            if exists {
                continue;
            }
        }

        if !is_explicit_profile && !listing_matches_structured_criteria(l, filters) {
            continue;
        }
        if profile_opt.is_none() {
            let det = score_listing_deterministic(l, filters);
            if det.match_score < 55 {
                continue;
            }
        }
        eligible.push((ch, event_key));
    }

    if eligible.is_empty() {
        return Ok((Vec::new(), false));
    }

    let alert_llm_explain = allow_llm_explain
        && env
            .get("ALERT_LLM_EXPLAIN")
            .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

    let mut matched_items: Vec<AlertMatchedItem> = Vec::new();
    let mut used_profile_scoring = false;

    if let Some(profile) = profile_opt {
        let owned_factors_map = if prefetched_factors.is_none() {
            let refs: Vec<&ListingRow> = eligible.iter().map(|(ch, _)| &ch.listing).collect();
            let (_, fmap) = prefetch_factors_for_listings(db, &refs).await?;
            Some(fmap)
        } else {
            None
        };
        let factor_lookup = prefetched_factors.or(owned_factors_map.as_ref());

        let mut factors_vec = Vec::with_capacity(eligible.len());
        let mut by_id: HashMap<String, (&ListingChangeEvent, String)> =
            HashMap::with_capacity(eligible.len());
        let mut coord_entries: Vec<(String, LatLon)> = Vec::new();

        for (ch, event_key) in &eligible {
            let l = &ch.listing;
            let mut hf = factor_lookup
                .and_then(|m| m.get(&l.id).cloned())
                .unwrap_or_else(|| home_factors_from_listing(l));
            apply_listing_price_override(&mut hf, l);
            if let (Some(la), Some(lo)) = (l.lat, l.lon) {
                let pt = LatLon::new(la, lo);
                if pt.is_valid() {
                    coord_entries.push((l.id.clone(), pt));
                }
            }
            factors_vec.push(hf);
            by_id.insert(l.id.clone(), (*ch, event_key.clone()));
        }

        let mut commute = CommuteMatrix::new();
        if !profile.places.is_empty() && !coord_entries.is_empty() {
            let home_coords: Vec<LatLon> = coord_entries.iter().map(|(_, pt)| *pt).collect();
            let place_coords: Vec<LatLon> = profile
                .places
                .iter()
                .map(|p| LatLon::new(p.lat, p.lon))
                .collect();
            let routing = RoutingClient::new(http.clone(), RoutingConfig::from_env(env))
                .with_traffic_buffer(profile.traffic_buffer);
            let mat = routing.commute_times(&home_coords, &place_coords).await;
            for ((lid, _), row_times) in coord_entries.iter().zip(mat.times.iter()) {
                for (p_idx, cell) in row_times.iter().enumerate() {
                    if let Some(dt) = cell {
                        if dt.estimated {
                            commute.set_estimated(lid, p_idx, dt.free_flow_min);
                        } else {
                            commute.set(lid, p_idx, dt.free_flow_min);
                        }
                    }
                }
            }
        }

        let mut rank_res = rank_with(profile, &factors_vec, &commute, &RankOptions::default());
        localize(&mut rank_res, lang);

        let no_scorable = rank_res
            .notices
            .iter()
            .any(|n| n.code == NoticeCode::NoScorableDimensions);
        if !no_scorable || is_explicit_profile {
            used_profile_scoring = true;
            let mut scored_with_float: Vec<(f64, AlertMatchedItem)> = Vec::new();
            for scored in &rank_res.ranked {
                if scored.score < MatchBand::GOOD_MIN {
                    continue;
                }
                let Some((ch, event_key)) = by_id.get(&scored.id) else {
                    continue;
                };
                let match_score = scored.score.round() as i64;
                let match_tier = scored.band.label().get(lang).to_string();
                let (why_it_matches, tradeoffs) =
                    format_scored_reasons_and_tradeoffs(scored, lang);

                let mut listing_val = format_listing_row(&ch.listing);
                if let Some(obj) = listing_val.as_object_mut() {
                    obj.insert("matchScore".to_string(), json!(match_score));
                    obj.insert("exactMatchScore".to_string(), json!(scored.score));
                    obj.insert("matchTier".to_string(), json!(match_tier));
                    obj.insert("bandKey".to_string(), json!(scored.band.key()));
                    obj.insert("coverage".to_string(), json!(scored.coverage));
                    obj.insert("whyItMatches".to_string(), json!(why_it_matches));
                    obj.insert("tradeoffs".to_string(), json!(tradeoffs));
                    obj.insert(
                        "reasons".to_string(),
                        serde_json::to_value(&scored.reasons).unwrap_or_else(|_| json!([])),
                    );
                    obj.insert(
                        "tradeOffs".to_string(),
                        serde_json::to_value(&scored.trade_offs).unwrap_or_else(|_| json!([])),
                    );
                }

                scored_with_float.push((
                    scored.score,
                    AlertMatchedItem {
                        event_type: ch.event_type.clone(),
                        old_price: ch.old_price,
                        event_key: event_key.clone(),
                        listing: listing_val,
                        match_score,
                        match_tier,
                        why_it_matches,
                        tradeoffs,
                    },
                ));
            }
            scored_with_float.sort_by(|a, b| {
                b.0.total_cmp(&a.0).then_with(|| {
                    let ia = str_from_val(a.1.listing.get("id")).unwrap_or_default();
                    let ib = str_from_val(b.1.listing.get("id")).unwrap_or_default();
                    ia.cmp(&ib)
                })
            });
            matched_items = scored_with_float.into_iter().map(|(_, item)| item).collect();
        }
    }

    let req_trimmed = sub_requirements.trim();
    let has_qualitative_req = (!req_trimmed.is_empty()
        && !req_trimmed.starts_with("Saved Your Match profile")
        && !req_trimmed.starts_with('{'))
        || !filters.must_have_keywords.is_empty()
        || !filters.nice_to_have_keywords.is_empty();
    if used_profile_scoring
        && !matched_items.is_empty()
        && alert_llm_explain
        && env.llm_configured()
        && has_qualitative_req
    {
        let top_n = matched_items.len().min(8);
        let finalist_rows: Vec<ListingRow> = matched_items[..top_n]
            .iter()
            .filter_map(|m| {
                let id = str_from_val(m.listing.get("id"))?;
                eligible
                    .iter()
                    .find(|(ch, _)| ch.listing.id == id)
                    .map(|(ch, _)| ch.listing.clone())
            })
            .collect();
        if !finalist_rows.is_empty() {
            let llm_res = rank_candidates_with_llm(
                db,
                http,
                env,
                &finalist_rows,
                sub_requirements,
                filters,
            )
            .await;
            let mut prose_by_id: HashMap<String, (Option<String>, Option<String>)> =
                HashMap::new();
            for rv in llm_res.ranked {
                if let Some(id) = str_from_val(rv.get("id")) {
                    prose_by_id.insert(
                        id,
                        (
                            str_from_val(rv.get("whyItMatches")),
                            str_from_val(rv.get("tradeoffs")),
                        ),
                    );
                }
            }
            for item in &mut matched_items[..top_n] {
                if let Some(id) = str_from_val(item.listing.get("id")) {
                    if let Some((why_opt, trade_opt)) = prose_by_id.get(&id) {
                        if let Some(ref w) = why_opt {
                            if !w.trim().is_empty() {
                                item.why_it_matches = w.clone();
                                if let Some(obj) = item.listing.as_object_mut() {
                                    obj.insert("whyItMatches".to_string(), json!(w));
                                }
                            }
                        }
                        if let Some(ref tr) = trade_opt {
                            if !tr.trim().is_empty() {
                                item.tradeoffs = Some(tr.clone());
                                if let Some(obj) = item.listing.as_object_mut() {
                                    obj.insert("tradeoffs".to_string(), json!(tr));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if !used_profile_scoring {
        for (ch, event_key) in &eligible {
            if !listing_matches_structured_criteria(&ch.listing, filters) {
                continue;
            }
            let det = score_listing_deterministic(&ch.listing, filters);
            if det.match_score < 68 {
                continue;
            }
            let why = crate::shared::scrub_competitor_brands(&det.why_it_matches);
            let tradeoffs = det
                .tradeoffs
                .as_deref()
                .map(crate::shared::scrub_competitor_brands)
                .filter(|s| !s.trim().is_empty());
            let mut listing_val = format_listing_row(&ch.listing);
            if let Some(obj) = listing_val.as_object_mut() {
                obj.insert("matchScore".to_string(), json!(det.match_score));
                obj.insert("matchTier".to_string(), json!(det.match_tier));
                obj.insert("whyItMatches".to_string(), json!(why));
                obj.insert("tradeoffs".to_string(), json!(tradeoffs));
            }

            matched_items.push(AlertMatchedItem {
                event_type: ch.event_type.clone(),
                old_price: ch.old_price,
                event_key: event_key.clone(),
                listing: listing_val,
                match_score: det.match_score,
                match_tier: det.match_tier,
                why_it_matches: why,
                tradeoffs,
            });
        }
        matched_items.sort_by(|a, b| {
            b.match_score.cmp(&a.match_score).then_with(|| {
                let ia = str_from_val(a.listing.get("id")).unwrap_or_default();
                let ib = str_from_val(b.listing.get("id")).unwrap_or_default();
                ia.cmp(&ib)
            })
        });

        if !matched_items.is_empty() && alert_llm_explain && env.llm_configured() {
            let top_n = matched_items.len().min(8);
            let finalist_rows: Vec<ListingRow> = matched_items[..top_n]
                .iter()
                .filter_map(|m| {
                    let id = str_from_val(m.listing.get("id"))?;
                    eligible
                        .iter()
                        .find(|(ch, _)| ch.listing.id == id)
                        .map(|(ch, _)| ch.listing.clone())
                })
                .collect();
            if !finalist_rows.is_empty() {
                let llm_res = rank_candidates_with_llm(
                    db,
                    http,
                    env,
                    &finalist_rows,
                    sub_requirements,
                    filters,
                )
                .await;
                let mut prose_by_id: HashMap<String, (Option<String>, Option<String>)> =
                    HashMap::new();
                for rv in llm_res.ranked {
                    if let Some(id) = str_from_val(rv.get("id")) {
                        prose_by_id.insert(
                            id,
                            (
                                str_from_val(rv.get("whyItMatches")),
                                str_from_val(rv.get("tradeoffs")),
                            ),
                        );
                    }
                }
                for item in &mut matched_items[..top_n] {
                    if let Some(id) = str_from_val(item.listing.get("id")) {
                        if let Some((why_opt, trade_opt)) = prose_by_id.get(&id) {
                            if let Some(ref w) = why_opt {
                                if !w.trim().is_empty() {
                                    item.why_it_matches = w.clone();
                                    if let Some(obj) = item.listing.as_object_mut() {
                                        obj.insert("whyItMatches".to_string(), json!(w));
                                    }
                                }
                            }
                            if let Some(ref tr) = trade_opt {
                                if !tr.trim().is_empty() {
                                    item.tradeoffs = Some(tr.clone());
                                    if let Some(obj) = item.listing.as_object_mut() {
                                        obj.insert("tradeoffs".to_string(), json!(tr));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok((matched_items, used_profile_scoring))
}

#[allow(clippy::too_many_arguments)]
async fn query_matching_listings_for_subscription(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    sub_id: &str,
    sub_zips: &[String],
    filters: &SearchFilters,
    notify_on_new: bool,
    notify_on_sold: bool,
    profile_opt: Option<&Profile>,
    is_explicit_profile: bool,
    lang: Lang,
    limit: usize,
) -> Result<Vec<AlertMatchedItem>> {
    if sub_zips.is_empty() || (!notify_on_new && !notify_on_sold) {
        return Ok(Vec::new());
    }
    let mut statuses = Vec::new();
    if notify_on_new {
        statuses.push("for_sale".to_string());
    }
    if notify_on_sold {
        statuses.push("sold".to_string());
    }

    let sql_filters = if is_explicit_profile {
        SearchFilters {
            zips: sub_zips.to_vec(),
            ..Default::default()
        }
    } else {
        let mut f = filters.clone();
        f.zips = sub_zips.to_vec();
        f.cities.clear();
        f.states.clear();
        f
    };

    let (_, rows) = search_listings_narrow(db, &sql_filters, false, Some(&statuses), 100).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let events: Vec<ListingChangeEvent> = rows
        .into_iter()
        .filter(|r| sub_zips.contains(&r.zip))
        .map(|r| {
            let ev_type = if r.status == "sold" {
                "sold"
            } else {
                "new_listing"
            };
            ListingChangeEvent {
                event_type: ev_type.to_string(),
                old_status: None,
                old_price: None,
                listing_json: format_listing_row(&r),
                listing: r,
            }
        })
        .collect();
    if events.is_empty() {
        return Ok(Vec::new());
    }

    let (mut items, _) = score_listing_events_for_subscription(
        db,
        http,
        env,
        sub_id,
        "",
        sub_zips,
        &[],
        filters,
        notify_on_new,
        notify_on_sold,
        profile_opt,
        is_explicit_profile,
        lang,
        &events,
        true,
        None,
        None,
        false,
    )
    .await?;
    items.truncate(limit);
    Ok(items)
}

async fn render_and_deliver_subscription_email(
    env: &EnvConfig,
    core: &SubscriptionCoreProto,
    lang: Lang,
    matched_items: &[AlertMatchedItem],
    base_origin: &str,
) -> (String, Option<String>, bool, String) {
    let freq = proto::subscription_freq_from_code(core.frequency);
    let freq_opt = if freq == "instant" { None } else { Some(freq) };

    let brand_name = env.brand_name();
    let brand_dom = env.brand_domain();
    let email_payload = render_home_digest_email(
        core.subscriber_name.as_deref(),
        &core.requirements_text,
        &core.zips,
        matched_items,
        base_origin,
        &core.unsubscribe_token,
        lang.code(),
        &brand_name,
        &brand_dom,
        None,
        None,
        freq_opt,
    );

    let mut delivery_status = "queued_no_smtp".to_string();
    let mut delivery_error: Option<String> = None;
    let mut email_sent = false;
    if env.smtp_configured() {
        let mail_req = EmailRequest {
            to: vec![core.email.clone()],
            subject: email_payload.subject.clone(),
            text: Some(email_payload.text.clone()),
            html: Some(email_payload.html.clone()),
            reply_to: None,
            unsubscribe_url: Some(email_payload.unsubscribe_url.clone()),
        };
        match send_smtp_email(env, &mail_req).await {
            Ok(_) => {
                delivery_status = "sent".to_string();
                email_sent = true;
            }
            Err(e) => {
                delivery_status = "failed".to_string();
                delivery_error = Some(e.to_string().chars().take(250).collect());
            }
        }
    }

    let subj_short: String = email_payload.subject.chars().take(250).collect();
    (delivery_status, delivery_error, email_sent, subj_short)
}

fn build_notification_protos_for_items(
    core: &SubscriptionCoreProto,
    matched_items: &[AlertMatchedItem],
    sk_by_id: &HashMap<String, i64>,
    subj_short: &str,
    delivery_status: &str,
    delivery_error: Option<&str>,
    sent_ts: i64,
    out: &mut Vec<SubscriptionNotificationProto>,
) {
    let deliv_code = proto::delivery_status_to_code(delivery_status);
    let err_str = delivery_error.map(ToOwned::to_owned);
    for item in matched_items {
        let lid = str_from_val(item.listing.get("id")).unwrap_or_default();
        let lsk = sk_by_id
            .get(&lid)
            .copied()
            .unwrap_or_else(|| deterministic_sk(&lid));
        let pub_id = Uuid::new_v4().to_string();
        let notif_sk = deterministic_sk(&pub_id);
        let reason_short: String = item.why_it_matches.chars().take(512).collect();
        out.push(SubscriptionNotificationProto {
            notification_sk: notif_sk,
            public_id: pub_id,
            subscription_sk: core.subscription_sk,
            subscription_public_id: core.public_id.clone(),
            listing_sk: lsk,
            listing_external_id: lid,
            email_to: core.email.clone(),
            subject: subj_short.to_string(),
            event_kind: proto::listing_event_to_code(&item.event_type),
            event_key: item.event_key.chars().take(200).collect(),
            match_score: Some(item.match_score as i32),
            match_reason: Some(reason_short),
            delivery_status: deliv_code,
            delivery_error: err_str.clone(),
            sent_ts,
        });
    }
}

pub async fn evaluate_alerts_for_listings(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    changed_listings: &[ListingChangeEvent],
    base_origin: &str,
) -> Result<Value> {
    db.ensure_schema().await?;
    if changed_listings.is_empty() {
        return Ok(json!({
            "subscriptionsChecked": 0,
            "notificationsTriggered": 0,
            "emailsSent": 0,
            "queuedDigests": 0,
            "details": [],
        }));
    }

    // Q4_subs_scan: indexed SQL join over `subscription_zips` + `subscription_cities` + scalar bounds
    let candidates = db.match_subscriptions_for_events(changed_listings).await?;
    if candidates.is_empty() {
        return Ok(json!({
            "subscriptionsChecked": 0,
            "notificationsTriggered": 0,
            "emailsSent": 0,
            "queuedDigests": 0,
            "details": [],
        }));
    }

    // Q4_fanout step 1: batch-dedup all candidate `event_key`s in ONE SQL query
    let mut all_candidate_keys: Vec<String> = Vec::new();
    let mut seen_cand_keys: HashSet<String> = HashSet::new();
    let mut matched_id_set_all: HashSet<String> = HashSet::new();

    for cand in &candidates {
        let sub_id = &cand.subscription.public_id;
        let cand_ids: HashSet<&str> = cand.matched_listing_ids.iter().map(String::as_str).collect();
        for ev in changed_listings {
            if cand_ids.contains(ev.listing.id.as_str()) {
                matched_id_set_all.insert(ev.listing.id.clone());
                let ek = format!(
                    "{}:{}:{}:{}",
                    sub_id,
                    ev.listing.id,
                    ev.listing.status,
                    ev.listing.price.unwrap_or(0)
                );
                if seen_cand_keys.insert(ek.clone()) {
                    all_candidate_keys.push(ek);
                }
            }
        }
    }

    let already_sent = db
        .filter_existing_notification_event_keys(&all_candidate_keys)
        .await?;

    // Q4_fanout step 2: batch-prefetch typed `HomeFactors` + `ReportHeaderProto` ONCE across all candidate listings
    let candidate_listing_refs: Vec<&ListingRow> = changed_listings
        .iter()
        .filter(|ev| matched_id_set_all.contains(&ev.listing.id))
        .map(|ev| &ev.listing)
        .collect();
    let (sk_by_id, prefetched_factors) =
        prefetch_factors_for_listings(db, &candidate_listing_refs).await?;

    let mut notifications_triggered = 0_usize;
    let mut emails_sent = 0_usize;
    let mut queued_digests = 0_usize;
    let mut details = Vec::new();
    let now_ts = now_unix_secs();

    let mut all_notif_rows: Vec<SubscriptionNotificationProto> = Vec::new();
    let mut flushed_digest_sks: Vec<i64> = Vec::new();
    let mut notified_updates_by_delta: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut digest_updated_sks: Vec<i64> = Vec::new();

    for cand in &candidates {
        let core = &cand.subscription;
        let (filters, profile_opt, is_explicit_profile, lang) =
            search_filters_and_profile_from_proto(core);

        let cand_ids: HashSet<&str> = cand.matched_listing_ids.iter().map(String::as_str).collect();
        let sub_events: Vec<ListingChangeEvent> = changed_listings
            .iter()
            .filter(|ev| cand_ids.contains(ev.listing.id.as_str()))
            .cloned()
            .collect();
        if sub_events.is_empty() {
            continue;
        }

        let (matched_items, used_profile_scoring) = score_listing_events_for_subscription(
            db,
            http,
            env,
            &core.public_id,
            &core.requirements_text,
            &core.zips,
            &core.cities,
            &filters,
            core.notify_on_new,
            core.notify_on_sold,
            profile_opt.as_ref(),
            is_explicit_profile,
            lang,
            &sub_events,
            false,
            Some(&already_sent),
            Some(&prefetched_factors),
            true,
        )
        .await?;

        if matched_items.is_empty() {
            continue;
        }

        let freq = proto::subscription_freq_from_code(core.frequency);
        let had_pending = cand
            .subscription_row
            .digest_pending_json
            .as_deref()
            .map(|s| {
                let t = s.trim();
                !t.is_empty() && t != "[]"
            })
            .unwrap_or(false);
        let merged = merge_digest_items(
            cand.subscription_row.digest_pending_json.as_deref(),
            matched_items,
        );

        if freq != "instant"
            && !is_subscription_digest_due_ts(
                core.frequency,
                core.last_notified_ts,
                core.created_ts,
                now_ts,
            )
        {
            let mut digest_protos = Vec::with_capacity(merged.len());
            for m in &merged {
                let ext_id = str_from_val(m.listing.get("id"))
                    .unwrap_or_else(|| "unknown".to_string())
                    .chars()
                    .take(180)
                    .collect::<String>();
                let lsk = sk_by_id
                    .get(&ext_id)
                    .copied()
                    .unwrap_or_else(|| deterministic_sk(&ext_id));
                let snap_zstd = compress_bytes_zstd_dict(&serde_json::to_vec(&m.listing)?)?;
                digest_protos.push(SubscriptionDigestItemProto {
                    subscription_sk: core.subscription_sk,
                    listing_sk: lsk,
                    listing_external_id: ext_id,
                    event_kind: proto::listing_event_to_code(&m.event_type),
                    old_price: m.old_price,
                    event_key: m.event_key.chars().take(200).collect(),
                    match_score: m.match_score as i32,
                    match_tier: m.match_tier.chars().take(32).collect(),
                    why_it_matches: m.why_it_matches.chars().take(512).collect(),
                    tradeoffs: m
                        .tradeoffs
                        .as_ref()
                        .map(|s| s.chars().take(255).collect::<String>()),
                    queued_ts: now_ts,
                    listing_snapshot_zstd: snap_zstd,
                });
            }
            db.upsert_digest_queue_items(core.subscription_sk, &digest_protos)
                .await?;
            digest_updated_sks.push(core.subscription_sk);
            queued_digests += 1;

            let matched_scores: Vec<i64> = merged.iter().map(|m| m.match_score).collect();
            let matched_ids: Vec<String> = merged
                .iter()
                .filter_map(|m| str_from_val(m.listing.get("id")))
                .collect();
            details.push(json!({
                "subscriptionId": core.public_id,
                "email": core.email,
                "frequency": freq,
                "matchedCount": merged.len(),
                "queuedCount": merged.len(),
                "topMatchScore": matched_scores.first().copied(),
                "matchedScores": matched_scores,
                "matchedListingIds": matched_ids,
                "usedProfileScoring": used_profile_scoring,
                "deliveryStatus": "queued_digest",
                "deliveryError": Value::Null,
            }));
            continue;
        }

        let (delivery_status, delivery_error, sent_ok, subj_short) =
            render_and_deliver_subscription_email(env, core, lang, &merged, base_origin).await;
        if sent_ok {
            emails_sent += 1;
        }
        let recorded = merged.len();
        notifications_triggered += recorded;

        build_notification_protos_for_items(
            core,
            &merged,
            &sk_by_id,
            &subj_short,
            &delivery_status,
            delivery_error.as_deref(),
            now_ts,
            &mut all_notif_rows,
        );
        if had_pending {
            flushed_digest_sks.push(core.subscription_sk);
        }
        notified_updates_by_delta
            .entry(recorded as i64)
            .or_default()
            .push(core.subscription_sk);

        let matched_scores: Vec<i64> = merged.iter().map(|m| m.match_score).collect();
        let matched_ids: Vec<String> = merged
            .iter()
            .filter_map(|m| str_from_val(m.listing.get("id")))
            .collect();
        details.push(json!({
            "subscriptionId": core.public_id,
            "email": core.email,
            "frequency": freq,
            "matchedCount": merged.len(),
            "topMatchScore": matched_scores.first().copied(),
            "matchedScores": matched_scores,
            "matchedListingIds": matched_ids,
            "usedProfileScoring": used_profile_scoring,
            "deliveryStatus": delivery_status,
            "deliveryError": delivery_error,
        }));
    }

    // Q4_fanout step 3: persist notifications & subscription state updates in batched multi-row SQL
    if !all_notif_rows.is_empty() {
        db.insert_subscription_notifications_batch(&all_notif_rows)
            .await?;
    }
    for chunk in flushed_digest_sks.chunks(200) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let sql =
            format!("DELETE FROM subscription_digest_queue WHERE subscription_sk IN ({ph})");
        let params: Vec<SqlParam> = chunk.iter().copied().map(SqlParam::I64).collect();
        db.execute_params_raw(&sql, &params).await?;
    }
    for (delta, sks) in notified_updates_by_delta {
        for chunk in sks.chunks(200) {
            let ph = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "UPDATE subscriptions
                 SET last_notified_ts = ?,
                     match_count = match_count + ?,
                     updated_ts = ?
                 WHERE subscription_sk IN ({ph})"
            );
            let mut params = Vec::with_capacity(3 + chunk.len());
            params.push(SqlParam::I64(now_ts));
            params.push(SqlParam::I64(delta));
            params.push(SqlParam::I64(now_ts));
            for &sk in chunk {
                params.push(SqlParam::I64(sk));
            }
            db.execute_params_raw(&sql, &params).await?;
        }
    }
    for chunk in digest_updated_sks.chunks(200) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let sql = format!("UPDATE subscriptions SET updated_ts = ? WHERE subscription_sk IN ({ph})");
        let mut params = Vec::with_capacity(1 + chunk.len());
        params.push(SqlParam::I64(now_ts));
        for &sk in chunk {
            params.push(SqlParam::I64(sk));
        }
        db.execute_params_raw(&sql, &params).await?;
    }

    Ok(json!({
        "subscriptionsChecked": candidates.len(),
        "notificationsTriggered": notifications_triggered,
        "emailsSent": emails_sent,
        "queuedDigests": queued_digests,
        "details": details,
    }))
}

pub async fn flush_due_subscription_digests(
    db: &DbStore,
    env: &EnvConfig,
    base_origin: &str,
) -> Result<Value> {
    db.ensure_schema().await?;
    let checked_cnt = db
        .fetch_scalar_i64(
            "SELECT COUNT(DISTINCT dq.subscription_sk) AS cnt
             FROM subscription_digest_queue dq
             JOIN subscriptions s ON s.subscription_sk = dq.subscription_sk
             WHERE s.active = 1",
            &[],
            "cnt",
        )
        .await? as usize;
    if checked_cnt == 0 {
        return Ok(json!({
            "ok": true,
            "checked": 0,
            "flushedSubscriptions": 0,
            "notificationsTriggered": 0,
            "emailsSent": 0,
            "details": [],
        }));
    }

    let now_ts = now_unix_secs();
    let popped = db.pop_due_digest_queues(now_ts).await?;
    if popped.is_empty() {
        return Ok(json!({
            "ok": true,
            "checked": checked_cnt,
            "flushedSubscriptions": 0,
            "notificationsTriggered": 0,
            "emailsSent": 0,
            "details": [],
        }));
    }

    let mut ready_queues: Vec<(SubscriptionCoreProto, Vec<SubscriptionDigestItemProto>)> =
        Vec::new();
    for (core, dq_items) in popped {
        if !is_subscription_digest_due_ts(
            core.frequency,
            core.last_notified_ts,
            core.created_ts,
            now_ts,
        ) {
            if !dq_items.is_empty() {
                db.upsert_digest_queue_items(core.subscription_sk, &dq_items)
                    .await?;
            }
            continue;
        }
        ready_queues.push((core, dq_items));
    }

    if ready_queues.is_empty() {
        return Ok(json!({
            "ok": true,
            "checked": checked_cnt,
            "flushedSubscriptions": 0,
            "notificationsTriggered": 0,
            "emailsSent": 0,
            "details": [],
        }));
    }

    let mut all_keys: Vec<String> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    for (_, dq_items) in &ready_queues {
        for it in dq_items {
            if !it.event_key.is_empty() && seen_keys.insert(it.event_key.clone()) {
                all_keys.push(it.event_key.clone());
            }
        }
    }
    let already_sent = db.filter_existing_notification_event_keys(&all_keys).await?;

    let mut flushed_subs = 0_usize;
    let mut notifications_triggered = 0_usize;
    let mut emails_sent = 0_usize;
    let mut details = Vec::new();
    let mut all_notif_rows: Vec<SubscriptionNotificationProto> = Vec::new();
    let mut notified_updates_by_delta: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut empty_updated_sks: Vec<i64> = Vec::new();

    for (core, dq_items) in &ready_queues {
        let mut unsent: Vec<AlertMatchedItem> = Vec::with_capacity(dq_items.len());
        let mut sk_by_id: HashMap<String, i64> = HashMap::with_capacity(dq_items.len());
        for it in dq_items {
            if already_sent.contains(&it.event_key) {
                continue;
            }
            if !it.listing_external_id.is_empty() && it.listing_sk > 0 {
                sk_by_id.insert(it.listing_external_id.clone(), it.listing_sk);
            }
            let listing_val = if !it.listing_snapshot_zstd.is_empty() {
                decompress_bytes_zstd_any(&it.listing_snapshot_zstd)
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                    .unwrap_or_else(|| json!({ "id": it.listing_external_id }))
            } else {
                json!({ "id": it.listing_external_id })
            };
            unsent.push(AlertMatchedItem {
                event_type: proto::listing_event_from_code(it.event_kind).to_string(),
                old_price: it.old_price,
                event_key: it.event_key.clone(),
                listing: listing_val,
                match_score: it.match_score as i64,
                match_tier: it.match_tier.clone(),
                why_it_matches: it.why_it_matches.clone(),
                tradeoffs: it.tradeoffs.clone(),
            });
        }

        if unsent.is_empty() {
            empty_updated_sks.push(core.subscription_sk);
            continue;
        }

        let lang = match core.lang_code {
            2 => Lang::parse("zh-CN"),
            3 => Lang::parse("es"),
            _ => Lang::En,
        };
        let (delivery_status, delivery_error, sent_ok, subj_short) =
            render_and_deliver_subscription_email(env, core, lang, &unsent, base_origin).await;
        flushed_subs += 1;
        if sent_ok {
            emails_sent += 1;
        }
        let recorded = unsent.len();
        notifications_triggered += recorded;

        build_notification_protos_for_items(
            core,
            &unsent,
            &sk_by_id,
            &subj_short,
            &delivery_status,
            delivery_error.as_deref(),
            now_ts,
            &mut all_notif_rows,
        );
        notified_updates_by_delta
            .entry(recorded as i64)
            .or_default()
            .push(core.subscription_sk);

        details.push(json!({
            "subscriptionId": core.public_id,
            "email": core.email,
            "frequency": proto::subscription_freq_from_code(core.frequency),
            "matchedCount": unsent.len(),
            "deliveryStatus": delivery_status,
            "deliveryError": delivery_error,
        }));
    }

    if !all_notif_rows.is_empty() {
        db.insert_subscription_notifications_batch(&all_notif_rows)
            .await?;
    }
    for (delta, sks) in notified_updates_by_delta {
        for chunk in sks.chunks(200) {
            let ph = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "UPDATE subscriptions
                 SET last_notified_ts = ?,
                     match_count = match_count + ?,
                     updated_ts = ?
                 WHERE subscription_sk IN ({ph})"
            );
            let mut params = Vec::with_capacity(3 + chunk.len());
            params.push(SqlParam::I64(now_ts));
            params.push(SqlParam::I64(delta));
            params.push(SqlParam::I64(now_ts));
            for &sk in chunk {
                params.push(SqlParam::I64(sk));
            }
            db.execute_params_raw(&sql, &params).await?;
        }
    }
    for chunk in empty_updated_sks.chunks(200) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let sql = format!("UPDATE subscriptions SET updated_ts = ? WHERE subscription_sk IN ({ph})");
        let mut params = Vec::with_capacity(1 + chunk.len());
        params.push(SqlParam::I64(now_ts));
        for &sk in chunk {
            params.push(SqlParam::I64(sk));
        }
        db.execute_params_raw(&sql, &params).await?;
    }

    Ok(json!({
        "ok": true,
        "checked": checked_cnt,
        "flushedSubscriptions": flushed_subs,
        "notificationsTriggered": notifications_triggered,
        "emailsSent": emails_sent,
        "details": details,
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestListingsRequest {
    pub zip: Option<String>,
    #[serde(default)]
    pub listings: Vec<Value>,
    pub csv: Option<String>,
    #[serde(default = "default_true")]
    pub trigger_alerts: bool,
    #[serde(default = "default_origin")]
    pub base_origin: String,
}

impl Default for IngestListingsRequest {
    fn default() -> Self {
        Self {
            zip: None,
            listings: Vec::new(),
            csv: None,
            trigger_alerts: true,
            base_origin: default_origin(),
        }
    }
}

pub async fn ingest_listings(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    req: IngestListingsRequest,
) -> Result<Value> {
    db.ensure_schema().await?;
    let fallback_zip = if let Some(ref z) = req.zip.as_ref().filter(|s| !s.trim().is_empty()) {
        validate_zip_csv(z, 1, false)?[0].clone()
    } else {
        String::new()
    };

    let mut incoming: Vec<ListingInput> = Vec::new();
    if let Some(ref csv_txt) = req.csv.as_ref().filter(|s| !s.trim().is_empty()) {
        incoming.extend(parse_listing_csv(csv_txt, &fallback_zip));
    }
    for v in req.listings.iter().take(200) {
        if let Some(parsed) = ListingInput::from_json(v, &fallback_zip) {
            incoming.push(parsed);
        }
    }
    if incoming.is_empty() {
        return Err(SearchValidationError::new(
            "EMPTY_INGEST",
            "Provide non-empty `listings` array or `csv` text with valid 5-digit US ZIP codes.",
            400,
        )
        .into());
    }

    let batch_res = upsert_listings_batch(db, None, env, &incoming, true).await?;

    let mut touched_zips: Vec<String> = Vec::new();
    for item in &incoming {
        if item.zip.len() == 5 && !touched_zips.contains(&item.zip) {
            touched_zips.push(item.zip.clone());
        }
    }
    let now_ts = now_unix_secs();
    for z in touched_zips {
        let (act, sld) = db.count_listings_by_status_for_zip(&z).await?;
        let ex = db.get_watched_zip(&z).await?;
        let city = ex
            .as_ref()
            .and_then(|w| w.city.clone())
            .or_else(|| incoming.iter().find(|i| i.zip == z).and_then(|i| i.city.clone()));
        let state = ex
            .as_ref()
            .and_then(|w| w.state.clone())
            .or_else(|| incoming.iter().find(|i| i.zip == z).and_then(|i| i.state.clone()))
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty());
        let created_ts = ex
            .as_ref()
            .and_then(|w| proto::iso_to_unix_ts(&w.created_at))
            .unwrap_or(now_ts);

        db.upsert_row_raw(
            "watched_zips",
            &[
                ("zip", SqlParam::from(z.as_str())),
                ("city", SqlParam::from(city.as_deref())),
                ("state_code", SqlParam::from(state.as_deref())),
                ("lat", SqlParam::from(ex.as_ref().and_then(|w| w.lat))),
                ("lon", SqlParam::from(ex.as_ref().and_then(|w| w.lon))),
                ("active", SqlParam::I64(1)),
                (
                    "subscriber_count",
                    SqlParam::I64(ex.as_ref().map(|w| w.subscriber_count).unwrap_or(0)),
                ),
                (
                    "sync_interval_min",
                    SqlParam::I64(ex.as_ref().map(|w| w.sync_interval_min).unwrap_or(15)),
                ),
                ("last_synced_ts", SqlParam::I64(now_ts)),
                (
                    "last_sync_status",
                    SqlParam::I64(proto::watched_sync_status_to_code(Some("ok")) as i64),
                ),
                (
                    "last_new_count",
                    SqlParam::I64(batch_res.new_listings.len() as i64),
                ),
                (
                    "last_sold_count",
                    SqlParam::I64(batch_res.sold_listings.len() as i64),
                ),
                ("total_active_listings", SqlParam::I64(act)),
                ("total_sold_listings", SqlParam::I64(sld)),
                ("sources_used", SqlParam::from("Direct CSV/API Ingest")),
                ("last_error", SqlParam::Null),
                ("created_ts", SqlParam::I64(created_ts)),
            ],
            &["zip"],
        )
        .await?;
    }

    let alerts = if req.trigger_alerts && !batch_res.changed_listings.is_empty() {
        evaluate_alerts_for_listings(
            db,
            http,
            env,
            &batch_res.changed_listings,
            &req.base_origin,
        )
        .await?
    } else {
        json!({
            "subscriptionsChecked": 0,
            "notificationsTriggered": 0,
            "emailsSent": 0,
            "details": [],
        })
    };

    Ok(json!({
        "ok": true,
        "upserted": batch_res.upserted,
        "newCount": batch_res.new_listings.len(),
        "soldCount": batch_res.sold_listings.len(),
        "priceChangeCount": batch_res.price_changes.len(),
        "alerts": alerts,
    }))
}

fn detect_rate_limit_signal(warnings: &[String]) -> (bool, Option<i64>) {
    static RETRY_AFTER_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)retry[- ]after[:=\s]+(\d{1,5})").unwrap());
    let mut had_rl = false;
    let mut retry_after: Option<i64> = None;
    for w in warnings {
        let lc = w.to_ascii_lowercase();
        if lc.contains("429")
            || lc.contains("403")
            || lc.contains("cooldown")
            || lc.contains("captcha")
            || lc.contains("rate limit")
            || lc.contains("rate-limit")
            || lc.contains("too many requests")
        {
            had_rl = true;
            if let Some(cap) = RETRY_AFTER_RE.captures(w) {
                if let Ok(sec) = cap[1].parse::<i64>() {
                    retry_after = Some(
                        retry_after
                            .map(|r| std::cmp::Ord::max(r, sec))
                            .unwrap_or(sec),
                    );
                }
            }
        }
    }
    (had_rl, retry_after)
}

/// Re-translates active subscriptions whose `filter_schema_version` is older than
/// [`CURRENT_FILTER_SCHEMA_VERSION`], preserving any embedded `profileJson` / `lang`.
pub async fn retranslate_stale_subscriptions(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    max_batch: usize,
) -> Result<Value> {
    db.ensure_schema().await?;
    let limit = max_batch.clamp(1, 200) as i64;
    let stale = db
        .fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE active = 1 AND filter_schema_version < ? ORDER BY updated_at ASC LIMIT ?",
            &[CURRENT_FILTER_SCHEMA_VERSION.into(), limit.into()],
        )
        .await?;

    let mut retranslated = 0_usize;
    let mut errors: Vec<String> = Vec::new();
    for sub in &stale {
        let sub_zips: Vec<String> = serde_json::from_str(&sub.zips_json).unwrap_or_default();
        let old_val: Value =
            serde_json::from_str(&sub.parsed_filters_json).unwrap_or_else(|_| json!({}));

        match parse_requirements_to_query(
            db,
            http,
            env,
            &sub.requirements_text,
            &sub_zips,
            None,
        )
        .await
        {
            Ok(mut parsed) => {
                parsed.filters.schema_version = Some(CURRENT_FILTER_SCHEMA_VERSION);
                let mut next_val = serde_json::to_value(&parsed.filters)?;
                if let (Some(old_obj), Some(next_obj)) =
                    (old_val.as_object(), next_val.as_object_mut())
                {
                    for keep_key in ["profileJson", "profile_json", "profile", "lang"] {
                        if let Some(v) = old_obj.get(keep_key) {
                            next_obj.insert(keep_key.to_string(), v.clone());
                        }
                    }
                }
                let next_json = serde_json::to_string(&next_val)?;
                let cities_json = if parsed.filters.cities.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&parsed.filters.cities)?)
                };
                let now = now_iso();
                let mut updated = sub.clone();
                updated.parsed_filters_json = next_json;
                updated.cities_json = cities_json;
                updated.filter_schema_version = CURRENT_FILTER_SCHEMA_VERSION;
                updated.filters_refreshed_at = Some(now.clone());
                updated.updated_at = now;
                db.upsert_subscription_normalized(&updated).await?;
                retranslated += 1;
            }
            Err(e) => {
                errors.push(format!("{}: {e}", sub.id));
            }
        }
    }

    Ok(json!({
        "ok": true,
        "schemaVersion": CURRENT_FILTER_SCHEMA_VERSION,
        "checked": stale.len(),
        "retranslated": retranslated,
        "errors": errors,
    }))
}

/// Event-driven schema check spawned at startup or upon schema upgrade: runs
/// [`retranslate_stale_subscriptions`] asynchronously only when stale active rows exist.
pub fn spawn_event_retranslate_check(db: DbStore, http: HttpClient, env: EnvConfig) {
    tokio::spawn(async move {
        if let Err(e) = db.ensure_schema().await {
            tracing::warn!(error = %e, "spawn_event_retranslate_check ensure_schema warning");
            return;
        }
        let has_stale = db
            .fetch_optional_mapped(
                "SELECT subscription_sk FROM subscriptions WHERE active = 1 AND filter_schema_version < ? LIMIT 1",
                &[SqlParam::I64(CURRENT_FILTER_SCHEMA_VERSION)],
                |r| r.opt_i64("subscription_sk"),
            )
            .await
            .ok()
            .flatten()
            .flatten()
            .is_some();
        if has_stale {
            if let Err(e) = retranslate_stale_subscriptions(&db, &http, &env, 50).await {
                tracing::warn!(error = %e, "Event-based subscription retranslation warning");
            }
        }
    });
}

pub async fn run_background_sync_cycle(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    base_origin: &str,
    force_zip: Option<&str>,
    max_zips: usize,
) -> Result<Value> {
    run_background_sync_cycle_with_provider(
        db,
        http,
        env,
        base_origin,
        force_zip,
        max_zips,
        None,
    )
    .await
}

pub async fn run_background_sync_cycle_with_provider(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    base_origin: &str,
    force_zip: Option<&str>,
    max_zips: usize,
    provider: Option<&dyn LiveZipListingProvider>,
) -> Result<Value> {
    db.ensure_schema().await?;
    let started = now_unix_ms();

    let default_reports = Path::new("reports");
    if default_reports.is_dir() && db.count_listings_total().await? == 0 {
        let _ = seed_reports_from_dir(db, default_reports).await;
    }

    let mut target_zips: Vec<String> = Vec::new();
    let now_str = now_iso();
    if let Some(fz) = force_zip.map(str::trim).filter(|s| !s.is_empty()) {
        let validated = validate_zip_csv(fz, 1, false)?;
        target_zips.extend(validated);
    } else {
        let watched = db
            .list_active_watched_zips(max_zips.clamp(1, 20) as i64)
            .await?;
        let now_dt = chrono::Utc::now();
        for w in watched {
            if w.next_sync_after
                .as_deref()
                .map(|n| n > now_str.as_str())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(ref ls) = w.last_synced_at else {
                target_zips.push(w.zip);
                continue;
            };
            let min_interval_secs =
                (std::cmp::Ord::max(w.sync_interval_min, 5) as f64 * 60.0 * 0.8) as i64;
            let due = chrono::DateTime::parse_from_rfc3339(ls)
                .map(|dt| {
                    now_dt
                        .signed_duration_since(dt.with_timezone(&chrono::Utc))
                        .num_seconds()
                        >= min_interval_secs
                })
                .unwrap_or(true);
            if due {
                target_zips.push(w.zip);
            }
        }
    }

    let owner_id = format!(
        "{}-{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "pod".to_string()),
        Uuid::new_v4()
    );
    let base_delay_ms = env
        .get("SYNC_ZIP_DELAY_MS")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(2_200);

    let mut zip_reports = Vec::new();
    let mut all_changed = Vec::new();
    let mut skipped_lease_zips = Vec::new();
    for (idx, zip) in target_zips.iter().enumerate() {
        if idx > 0 {
            let zip_hash: u64 = zip
                .bytes()
                .fold(0_u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            let jitter_ms = (now_unix_ms() as u64 ^ zip_hash) % 1501;
            tokio::time::sleep(std::time::Duration::from_millis(base_delay_ms + jitter_ms)).await;
        }
        if !db.try_claim_watched_zip_lease(zip, &owner_id, 300).await? {
            skipped_lease_zips.push(zip.clone());
            continue;
        }
        let sync_res = sync_zip_listings_with_provider(db, http, env, zip, true, provider).await;
        match sync_res {
            Ok(res) => {
                let (had_rate_limit, retry_after_secs) = detect_rate_limit_signal(&res.warnings);
                let backoff_secs = db
                    .finish_watched_zip_lease_with_backoff(
                        zip,
                        &owner_id,
                        had_rate_limit,
                        retry_after_secs,
                    )
                    .await
                    .unwrap_or(0);
                zip_reports.push(json!({
                    "zip": res.zip,
                    "upserted": res.upserted,
                    "activeCount": res.active_count,
                    "soldCount": res.sold_count,
                    "newCount": res.new_listings.len(),
                    "newlySoldCount": res.sold_listings.len(),
                    "priceChangeCount": res.price_changes.len(),
                    "sourcesUsed": res.sources_used,
                    "warnings": res.warnings,
                    "rateLimited": had_rate_limit,
                    "backoffSecs": backoff_secs,
                }));
                all_changed.extend(res.changed_listings);
            }
            Err(e) => {
                let err_str = e.to_string();
                let (had_rl, retry_after) = detect_rate_limit_signal(std::slice::from_ref(&err_str));
                let _ = db
                    .finish_watched_zip_lease_with_backoff(zip, &owner_id, had_rl, retry_after)
                    .await;
                return Err(e);
            }
        }
    }

    let alert_summary =
        evaluate_alerts_for_listings(db, http, env, &all_changed, base_origin).await?;
    let digest_flush = flush_due_subscription_digests(db, env, base_origin).await?;

    Ok(json!({
        "ok": true,
        "elapsedMs": (now_unix_ms().saturating_sub(started)) as u64,
        "zipsSynced": zip_reports.len(),
        "skippedLeaseZips": skipped_lease_zips,
        "zipReports": zip_reports,
        "changesDetected": all_changed.len(),
        "alerts": alert_summary,
        "digestFlush": digest_flush,
    }))
}

pub async fn get_watched_zips_status(db: &DbStore, env: Option<&EnvConfig>) -> Result<Value> {
    db.ensure_schema().await?;
    let watched = db.list_active_watched_zips(50).await?;
    let (total_listings, active_listings, sold_listings) =
        db.count_listings_global_totals().await?;
    let active_subs = db.count_active_subscriptions().await?;

    let mut out = json!({
        "ok": true,
        "database": {
            "configured": true,
            "provider": db.provider,
            "dialect": db.dialect(),
            "driver": db.driver_name(),
            "heatwaveReady": true,
            "totalListings": total_listings,
            "activeListings": active_listings,
            "soldListings": sold_listings,
            "activeSubscriptions": active_subs,
        },
        "watchedZips": watched.into_iter().map(|r| json!({
            "zip": r.zip,
            "city": r.city,
            "state": r.state,
            "subscriberCount": r.subscriber_count,
            "activeListings": r.total_active_listings,
            "soldListings": r.total_sold_listings,
            "lastSyncedAt": r.last_synced_at,
            "lastSyncStatus": r.last_sync_status,
            "sourcesUsed": r.sources_used,
        })).collect::<Vec<_>>(),
    });

    if let Some(e) = env {
        out["smtp"] = smtp_status_json(e);
        out["syncConnectors"] = sync_connector_status(e);
    }

    Ok(out)
}

pub async fn query_listings(
    db: &DbStore,
    zip: Option<&str>,
    statuses: Option<&[String]>,
    limit: usize,
) -> Result<Value> {
    db.ensure_schema().await?;
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(z) = zip.map(str::trim).filter(|s| !s.is_empty()) {
        if z.len() != 5 || !z.chars().all(|c| c.is_ascii_digit()) {
            return Err(SearchValidationError::new(
                "INVALID_ZIP",
                "zip must be a 5-digit US ZIP code.",
                400,
            )
            .into());
        }
        clauses.push("l.zip = ?".to_string());
        params.push(SqlParam::Text(z.to_string()));
    }

    if let Some(sts) = statuses {
        let mut codes: Vec<i64> = sts
            .iter()
            .flat_map(|s| s.split(','))
            .map(str::trim)
            .filter(|s| matches!(*s, "for_sale" | "pending" | "sold" | "off_market"))
            .map(|s| i64::from(proto::listing_status_to_code(Some(s))))
            .collect();
        codes.sort_unstable();
        codes.dedup();
        if codes.len() == 1 {
            clauses.push("l.status = ?".to_string());
            params.push(SqlParam::I64(codes[0]));
        } else if !codes.is_empty() {
            let ph = vec!["?"; codes.len()].join(", ");
            clauses.push(format!("l.status IN ({ph})"));
            for c in codes {
                params.push(SqlParam::I64(c));
            }
        }
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let safe_limit = limit.clamp(1, 100) as i64;
    params.push(SqlParam::I64(safe_limit));

    let sql = format!(
        "SELECT l.listing_sk, l.external_id, l.report_slug, l.zip, l.state_code, l.county_fips, \
                l.city, l.street, l.address, l.lat, l.lon, l.status, l.property_category, \
                l.price, l.list_price, l.sold_price, l.estimated_value, l.price_per_sqft, \
                l.hoa_monthly, l.all_in_monthly, l.beds, l.baths, l.sqft, l.lot_sqft, \
                l.year_built, l.stories, l.days_on_market, l.listed_epoch_day, l.sold_epoch_day, \
                l.school_score, l.school_grade, l.walk_score, l.safety_score, l.overall_score, \
                l.electric_rate_kwh, l.max_download_mbps, l.tag_bits, l.known_bits, \
                l.first_seen_ts, l.last_seen_ts, l.status_changed_ts, l.price_changed_ts \
         FROM listings l{where_sql} \
         ORDER BY l.status_changed_ts DESC, l.external_id ASC \
         LIMIT ?"
    );

    let cores = db.search_listing_cores(&sql, &params).await?;
    let cards = db.hydrate_listing_cards(&cores).await?;
    let formatted: Vec<Value> = cards
        .iter()
        .map(ListingFullView::to_listing_row)
        .map(|r| format_listing_row(&r))
        .collect();

    Ok(json!({
        "ok": true,
        "dialect": db.dialect(),
        "count": formatted.len(),
        "listings": formatted,
    }))
}
