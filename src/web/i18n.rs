// Backend Fluent i18n loader & HTTP request language resolver (`fluent-templates` + `unic-langid`).

use axum::http::{header, HeaderMap};
use fluent_templates::{fluent_bundle::FluentValue, Loader};
use std::borrow::Cow;
use std::collections::HashMap;
use unic_langid::{langid, LanguageIdentifier};

pub const LANG_EN: LanguageIdentifier = langid!("en-US");
pub const LANG_ZH: LanguageIdentifier = langid!("zh-CN");
pub const LANG_ES: LanguageIdentifier = langid!("es");
pub const LANG_COOKIE_NAME: &str = "open_estate_lang";
pub const DEFAULT_BRAND_NAME: &str = "Open Estate";

fluent_templates::static_loader! {
    pub static LOCALES = {
        locales: "./locales",
        fallback_language: "en-US",
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

/// Normalize a raw language tag (`zh`, `zh-CN`, `zh-Hans`, `es`, `es-MX`, `en`, `en-US`) into a supported `LanguageIdentifier`,
/// defaulting to `en-US`.
pub fn resolve_lang_str(raw: &str) -> LanguageIdentifier {
    parse_lang_tag(raw).unwrap_or(LANG_EN)
}

/// Normalize a raw language tag (`zh`, `zh-CN`, `zh-Hans`, `es`, `es-MX`, `en`, `en-US`) into a supported `LanguageIdentifier`.
pub fn parse_lang_tag(raw: &str) -> Option<LanguageIdentifier> {
    let lc = raw.trim().to_ascii_lowercase();
    if lc.is_empty() {
        return None;
    }
    if lc == "zh" || lc.starts_with("zh-") || lc.starts_with("zh_") {
        Some(LANG_ZH)
    } else if lc == "es" || lc.starts_with("es-") || lc.starts_with("es_") {
        Some(LANG_ES)
    } else if lc == "en" || lc.starts_with("en-") || lc.starts_with("en_") {
        Some(LANG_EN)
    } else {
        None
    }
}

fn read_cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    for part in cookie_header.split(';') {
        let trimmed = part.trim();
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim() == name {
                let val = v.trim();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Resolves preferred UI language from `?lang=`, the `open_estate_lang` cookie,
/// and `Accept-Language` header, defaulting to `en-US`.
pub fn resolve_lang(query_lang: Option<&str>, headers: &HeaderMap) -> LanguageIdentifier {
    if let Some(ql) = query_lang {
        if let Some(lid) = parse_lang_tag(ql) {
            return lid;
        }
    }

    if let Some(cookie_raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        if let Some(val) = read_cookie_value(cookie_raw, LANG_COOKIE_NAME) {
            if let Some(lid) = parse_lang_tag(val) {
                return lid;
            }
        }
    }

    if let Some(accept_raw) = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
    {
        let mut best: Option<(f32, LanguageIdentifier)> = None;
        for item in accept_raw.split(',') {
            let mut segs = item.trim().split(';');
            let tag = segs.next().unwrap_or("").trim();
            let Some(lid) = parse_lang_tag(tag) else {
                continue;
            };
            let mut q = 1.0_f32;
            for param in segs {
                let p = param.trim();
                if let Some(val) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                    if let Ok(parsed_q) = val.trim().parse::<f32>() {
                        if parsed_q.is_finite() {
                            q = parsed_q.clamp(0.0, 1.0);
                        }
                    }
                }
            }
            if q <= 0.0 {
                continue;
            }
            if best.as_ref().map(|(bq, _)| q > *bq).unwrap_or(true) {
                best = Some((q, lid));
            }
        }
        if let Some((_, lid)) = best {
            return lid;
        }
    }

    LANG_EN
}

pub fn is_zh(lang: &LanguageIdentifier) -> bool {
    lang.language.as_str() == "zh"
}

pub fn is_es(lang: &LanguageIdentifier) -> bool {
    lang.language.as_str() == "es"
}

pub fn short_code(lang: &LanguageIdentifier) -> &'static str {
    if is_zh(lang) {
        "zh"
    } else if is_es(lang) {
        "es"
    } else {
        "en"
    }
}

pub fn bcp47_code(lang: &LanguageIdentifier) -> &'static str {
    if is_zh(lang) {
        "zh-CN"
    } else if is_es(lang) {
        "es"
    } else {
        "en"
    }
}

pub fn to_scoring_lang(lang: &LanguageIdentifier) -> crate::scoring::catalog::Lang {
    if is_zh(lang) {
        crate::scoring::catalog::Lang::ZhCn
    } else if is_es(lang) {
        crate::scoring::catalog::Lang::Es
    } else {
        crate::scoring::catalog::Lang::En
    }
}

pub fn t(lang: &LanguageIdentifier, key: &str) -> String {
    t_brand(lang, key, DEFAULT_BRAND_NAME)
}

pub fn t_brand(lang: &LanguageIdentifier, key: &str, brand_name: &str) -> String {
    let eff = if brand_name.trim().is_empty() {
        DEFAULT_BRAND_NAME
    } else {
        brand_name.trim()
    };
    let mut map: HashMap<Cow<'static, str>, FluentValue<'_>> = HashMap::with_capacity(1);
    map.insert(Cow::Borrowed("brand"), FluentValue::from(eff));
    LOCALES.lookup_with_args(lang, key, &map)
}

pub fn t_args(lang: &LanguageIdentifier, key: &str, args: &[(&str, &str)]) -> String {
    t_brand_args(lang, key, DEFAULT_BRAND_NAME, args)
}

pub fn t_brand_args(
    lang: &LanguageIdentifier,
    key: &str,
    brand_name: &str,
    args: &[(&str, &str)],
) -> String {
    let eff = if brand_name.trim().is_empty() {
        DEFAULT_BRAND_NAME
    } else {
        brand_name.trim()
    };
    let mut map: HashMap<Cow<'static, str>, FluentValue<'_>> = HashMap::with_capacity(args.len() + 1);
    map.insert(Cow::Borrowed("brand"), FluentValue::from(eff));
    for &(k, v) in args {
        map.insert(Cow::Owned(k.to_string()), FluentValue::from(v));
    }
    LOCALES.lookup_with_args(lang, key, &map)
}
