// RFC-compliant async SMTP & MIME email dispatcher built on `lettre` (`Tokio1Executor` + `rustls`).
// Supports plain unencrypted local testing (`SMTP_SECURE=none`), STARTTLS (ports 587/2525),
// implicit TLS (port 465), AUTH PLAIN / LOGIN, UTF-8 subjects, multipart/alternative,
// and RFC 8058 one-click `List-Unsubscribe` + `List-Unsubscribe-Post` headers.

use crate::shared::{format_brand_wordmark_html, is_truthy_flag, EnvConfig};
use anyhow::{anyhow, Context, Result};
use html_escape::encode_safe;
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::error::Error as StdError;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmailRequest {
    pub to: Vec<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub reply_to: Option<String>,
    pub unsubscribe_url: Option<String>,
}

impl EmailRequest {
    pub fn single(to: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            to: vec![to.into()],
            subject: subject.into(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailDelivery {
    pub ok: bool,
    pub host: String,
    pub port: u16,
    pub secure: String,
    pub recipients: Vec<String>,
    pub server_response: String,
}

#[derive(Clone, Debug)]
pub struct ListUnsubscribeHeader(pub String);

impl Header for ListUnsubscribeHeader {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("List-Unsubscribe")
    }

    fn parse(s: &str) -> std::result::Result<Self, Box<dyn StdError + Send + Sync>> {
        Ok(Self(s.trim().trim_matches(&['<', '>'][..]).to_string()))
    }

    fn display(&self) -> HeaderValue {
        let clean: String = self.0.chars().filter(|c| c.is_ascii_graphic()).collect();
        let val = format!("<{clean}>");
        HeaderValue::dangerous_new_pre_encoded(Self::name(), val.clone(), val)
    }
}

#[derive(Clone, Debug)]
pub struct ListUnsubscribePostHeader;

impl Header for ListUnsubscribePostHeader {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("List-Unsubscribe-Post")
    }

    fn parse(_s: &str) -> std::result::Result<Self, Box<dyn StdError + Send + Sync>> {
        Ok(Self)
    }

    fn display(&self) -> HeaderValue {
        let val = "List-Unsubscribe=One-Click".to_string();
        HeaderValue::dangerous_new_pre_encoded(Self::name(), val.clone(), val)
    }
}

fn strip_crlf(s: &str) -> String {
    s.replace(['\r', '\n'], " ").trim().to_string()
}

pub fn resolve_secure_mode(env: &EnvConfig, port: u16) -> &'static str {
    let raw = env
        .get("SMTP_SECURE")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match raw.as_str() {
        "none" | "off" | "false" | "plain" => "off",
        "starttls" => "starttls",
        "tls" | "ssl" | "on" | "true" | "implicit" => "on",
        _ => {
            if port == 465 {
                "on"
            } else {
                "starttls"
            }
        }
    }
}

fn resolve_host_port_secure(env: &EnvConfig) -> Option<(String, u16, &'static str)> {
    let raw_host = strip_crlf(env.get("SMTP_HOST")?);
    if raw_host.is_empty() {
        return None;
    }
    let (host, inline_port) = match raw_host.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !h.contains(':') => match p.parse::<u16>() {
            Ok(port_num) => (h.trim().to_string(), Some(port_num)),
            Err(_) => (raw_host, None),
        },
        _ => (raw_host, None),
    };
    let explicit_port = env
        .get("SMTP_PORT")
        .and_then(|s| s.trim().parse::<u16>().ok())
        .or(inline_port);
    let default_port = match env
        .get("SMTP_SECURE")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "starttls" => 587,
        "none" | "off" | "false" | "plain" => 25,
        _ => 465,
    };
    let port = explicit_port.unwrap_or(default_port);
    let secure = resolve_secure_mode(env, port);
    Some((host, port, secure))
}

pub fn smtp_status_json(env: &EnvConfig) -> Value {
    let configured = env.smtp_configured();
    let resolved = resolve_host_port_secure(env);
    let (host, port, secure) = match resolved {
        Some((h, p, s)) if configured => (Some(h), Some(p), s),
        _ => (None, None, resolve_secure_mode(env, 465)),
    };
    json!({
        "configured": configured,
        "host": host,
        "port": port,
        "secure": secure,
        "from": env.get("SMTP_FROM").or_else(|| env.get("SMTP_USER")),
    })
}

static HTML_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
static MULTI_SPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

fn parse_mailbox(spec: &str) -> Result<Mailbox> {
    let clean = strip_crlf(spec);
    clean.parse::<Mailbox>().with_context(|| {
        format!(
            "Invalid email mailbox specification: {}",
            &clean[..clean.len().min(64)]
        )
    })
}

pub fn build_lettre_message(
    env: &EnvConfig,
    req: &EmailRequest,
) -> Result<(Message, Vec<String>, String)> {
    let from_raw = env
        .get("SMTP_FROM")
        .or_else(|| env.get("SMTP_USER"))
        .ok_or_else(|| anyhow!("SMTP_FROM (or SMTP_USER) is not configured"))?;
    let from_mb = if !from_raw.contains('<') {
        let custom_name = env
            .get("SMTP_FROM_NAME")
            .map(strip_crlf)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("bot"));
        let disp = custom_name.unwrap_or_else(|| strip_crlf(&env.brand_name()));
        let safe_disp: String = disp
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
            .collect();
        if !safe_disp.trim().is_empty() {
            parse_mailbox(&format!("{} <{}>", safe_disp.trim(), strip_crlf(from_raw)))
                .or_else(|_| parse_mailbox(from_raw))?
        } else {
            parse_mailbox(from_raw)?
        }
    } else {
        parse_mailbox(from_raw)?
    };
    let ehlo_domain = from_mb.email.domain().trim().to_string();
    let ehlo_domain = if ehlo_domain.is_empty() {
        env.brand_domain()
    } else {
        ehlo_domain
    };

    if req.to.is_empty() {
        return Err(anyhow!("At least one recipient email is required"));
    }

    let default_subj = format!("{} Property Alert", env.brand_name());
    let mut builder =
        Message::builder()
            .from(from_mb)
            .subject(strip_crlf(if req.subject.trim().is_empty() {
                &default_subj
            } else {
                &req.subject
            }));

    let mut rcpt_addrs = Vec::with_capacity(req.to.len());
    for raw_to in &req.to {
        let mb = parse_mailbox(raw_to)?;
        rcpt_addrs.push(mb.email.to_string());
        builder = builder.to(mb);
    }

    if let Some(rt) = req
        .reply_to
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env.get("SMTP_REPLY_TO"))
    {
        builder = builder.reply_to(parse_mailbox(rt)?);
    }

    if let Some(ref unsub_url) = req.unsubscribe_url {
        let clean_unsub = strip_crlf(unsub_url);
        if !clean_unsub.is_empty() {
            builder = builder
                .header(ListUnsubscribeHeader(clean_unsub))
                .header(ListUnsubscribePostHeader);
        }
    }

    let plain_body = match (&req.text, &req.html) {
        (Some(t), _) if !t.is_empty() => t.clone(),
        (_, Some(h)) => {
            let stripped = HTML_TAG_RE.replace_all(h, " ");
            MULTI_SPACE_RE
                .replace_all(&stripped, " ")
                .trim()
                .to_string()
        }
        _ => String::new(),
    };

    let msg = if let Some(ref html_body) = req.html.as_ref().filter(|h| !h.is_empty()) {
        builder.multipart(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(plain_body),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body((*html_body).clone()),
                ),
        )?
    } else {
        builder.header(ContentType::TEXT_PLAIN).body(plain_body)?
    };

    Ok((msg, rcpt_addrs, ehlo_domain))
}

pub async fn send_smtp_email(env: &EnvConfig, req: &EmailRequest) -> Result<EmailDelivery> {
    let (host, port, secure) =
        resolve_host_port_secure(env).ok_or_else(|| anyhow!("SMTP_HOST is not configured"))?;

    let (message, recipients, ehlo_domain) = build_lettre_message(env, req)?;

    let mut transport_builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
        .port(port)
        .hello_name(ClientId::Domain(ehlo_domain))
        .timeout(Some(Duration::from_secs(20)));

    transport_builder = match secure {
        "off" => transport_builder.tls(Tls::None),
        "starttls" => {
            let tls_params = TlsParameters::new(host.clone())
                .context("Failed to initialize STARTTLS parameters")?;
            transport_builder.tls(Tls::Required(tls_params))
        }
        _ => {
            let tls_params = TlsParameters::new(host.clone())
                .context("Failed to initialize SMTPS TLS parameters")?;
            transport_builder.tls(Tls::Wrapper(tls_params))
        }
    };

    if let (Some(user), Some(pass)) = (env.get("SMTP_USER"), env.get("SMTP_PASS")) {
        transport_builder = transport_builder
            .credentials(Credentials::new(user.to_string(), pass.to_string()))
            .authentication(vec![Mechanism::Plain, Mechanism::Login]);
    }

    let transport: AsyncSmtpTransport<Tokio1Executor> = transport_builder.build();
    let resp = transport
        .send(message)
        .await
        .map_err(|e| anyhow!("SMTP delivery failed: {e}"))?;
    crate::metering::add_email_sent(1);

    let server_response: String = resp
        .message()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect();

    Ok(EmailDelivery {
        ok: true,
        host,
        port,
        secure: secure.to_string(),
        recipients,
        server_response,
    })
}

pub use crate::web::hooks::SectionReadyNotification;

/// Checks recipient allowlist when test-mode email guard is enabled (`EMAIL_TEST_MODE=1` or
/// `SECTION_EMAIL_TEST_MODE=1`) so tests never dispatch email to arbitrary addresses.
pub fn enforce_test_recipient_allowlist(env: &EnvConfig, recipients: &[String]) -> Result<()> {
    let test_mode = env
        .get("SECTION_EMAIL_TEST_MODE")
        .or_else(|| env.get("EMAIL_TEST_MODE"))
        .map(is_truthy_flag)
        .unwrap_or(false);
    if !test_mode {
        return Ok(());
    }
    let raw_allowlist = env
        .get("SECTION_TEST_RECIPIENT_ALLOWLIST")
        .or_else(|| env.get("EMAIL_TEST_RECIPIENT_ALLOWLIST"))
        .unwrap_or("");
    let allowed: Vec<String> = raw_allowlist
        .split([',', ';', ' '])
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if allowed.is_empty() {
        return Err(anyhow!(
            "SECTION_EMAIL_TEST_MODE is enabled but EMAIL_TEST_RECIPIENT_ALLOWLIST / SECTION_TEST_RECIPIENT_ALLOWLIST is empty"
        ));
    }
    for rcpt in recipients {
        let norm = rcpt
            .rsplit_once('<')
            .map(|(_, right)| right.trim_end_matches('>'))
            .unwrap_or(rcpt.as_str())
            .trim()
            .to_ascii_lowercase();
        if !allowed.iter().any(|a| a == &norm) {
            return Err(anyhow!(
                "Recipient '{norm}' rejected by test-mode recipient allowlist guard"
            ));
        }
    }
    Ok(())
}

/// Builds an RFC-compliant multipart/alternative `EmailRequest` with RFC 8058
/// `List-Unsubscribe` headers and a section deep link (`/r/:slug/#<section>`).
pub fn build_section_ready_email_request(
    env: &EnvConfig,
    notif: &SectionReadyNotification,
) -> EmailRequest {
    let brand_name = env.brand_name();
    let origin = notif.base_origin.trim().trim_end_matches('/');
    let deep_link = if !notif.deep_link_url.trim().is_empty() {
        notif.deep_link_url.trim().to_string()
    } else if origin.is_empty() {
        format!("/r/{}/#{}", notif.slug.trim(), notif.section_id.trim())
    } else {
        format!(
            "{origin}/r/{}/#{}",
            notif.slug.trim(),
            notif.section_id.trim()
        )
    };
    let unsub_url = if !notif.unsubscribe_url.trim().is_empty() {
        notif.unsubscribe_url.trim().to_string()
    } else if origin.is_empty() {
        format!(
            "/u/section/{}",
            urlencoding::encode(notif.unsubscribe_token.trim())
        )
    } else {
        format!(
            "{origin}/u/section/{}",
            urlencoding::encode(notif.unsubscribe_token.trim())
        )
    };

    let section_label = strip_crlf(&notif.section_label);
    let display_address = strip_crlf(&notif.display_address);
    let subject = format!("{brand_name} — {section_label} is ready for {display_address}");

    let plain_text = format!(
        "{brand_name}: {section_label} is now ready for {display_address}.\n\n\
         View section directly:\n{deep_link}\n\n\
         Unsubscribe from updates for this section:\n{unsub_url}\n"
    );

    let wordmark_html = format_brand_wordmark_html(&brand_name);
    let esc_section = encode_safe(&section_label);
    let esc_addr = encode_safe(&display_address);
    let esc_deep_link = encode_safe(&deep_link);
    let esc_unsub = encode_safe(&unsub_url);

    let html = format!(
        r#"<!doctype html><html lang="en"><body style="margin:0;padding:20px;background:#f5f6f8;font-family:Inter,-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#111827">
  <div style="max-width:580px;margin:0 auto">
    <div style="background:#ffffff;border:1px solid #e5e7eb;border-radius:14px;padding:22px 24px">
      <div style="font-weight:800;font-size:20px;color:#111827;margin-bottom:14px">{wordmark_html}</div>
      <div style="display:inline-block;background:#ecfdf5;border:1px solid #a7f3d0;color:#065f46;font-size:11.5px;font-weight:700;padding:3px 10px;border-radius:999px;margin-bottom:10px">Section Ready</div>
      <h2 style="margin:0 0 8px;font-size:18px;font-weight:800;color:#111827">{esc_section} is ready</h2>
      <p style="margin:0 0 16px;font-size:14.5px;color:#374151;line-height:1.55">Your requested report section for <strong>{esc_addr}</strong> has finished loading and is now available on the property report.</p>
      <div style="margin-bottom:8px">
        <a href="{esc_deep_link}" style="display:inline-block;background:#0f766e;color:#ffffff;text-decoration:none;font-size:14px;font-weight:700;padding:10px 18px;border-radius:9px">Open {esc_section} →</a>
      </div>
    </div>
    <div style="font-size:12px;color:#6b7280;margin-top:14px;text-align:center;line-height:1.6">
      <a href="{esc_unsub}" style="color:#6b7280;text-decoration:underline">Unsubscribe from {esc_section} updates</a>
    </div>
  </div>
</body></html>"#
    );

    EmailRequest {
        to: vec![notif.email.clone()],
        subject,
        text: Some(plain_text),
        html: Some(html),
        reply_to: None,
        unsubscribe_url: Some(unsub_url),
    }
}

/// Dispatches a section-ready notification email via configured SMTP transport.
pub async fn send_section_ready_email(
    env: &EnvConfig,
    notif: &SectionReadyNotification,
) -> Result<EmailDelivery> {
    let req = build_section_ready_email_request(env, notif);
    enforce_test_recipient_allowlist(env, &req.to)?;
    send_smtp_email(env, &req).await
}
