// MV3 CSP-safe i18next + Intl formatting wrapper for popup.js.
(function initExtI18n(root) {
  'use strict';

  function normalizeLang(raw) {
    const lc = String(raw || '').trim().toLowerCase();
    if (lc.startsWith('zh')) return 'zh-CN';
    if (lc.startsWith('es')) return 'es';
    return 'en';
  }

  function toShortLang(raw) {
    const norm = normalizeLang(raw);
    if (norm === 'zh-CN') return 'zh';
    if (norm === 'es') return 'es';
    return 'en';
  }

  function nextLangCycle(cur) {
    const short = toShortLang(cur);
    if (short === 'en') return 'zh';
    if (short === 'zh') return 'es';
    return 'en';
  }

  function appendLangParam(rawUrl, lang) {
    const short = toShortLang(lang);
    if (short !== 'zh' && short !== 'es') return String(rawUrl || '');
    const s = String(rawUrl || '');
    if (s.includes('lang=')) return s;
    const hashIdx = s.indexOf('#');
    const basePart = hashIdx >= 0 ? s.slice(0, hashIdx) : s;
    const hashPart = hashIdx >= 0 ? s.slice(hashIdx) : '';
    const sep = basePart.includes('?') ? '&' : '?';
    return `${basePart}${sep}lang=${short}${hashPart}`;
  }

  function resolveI18next() {
    if (root.i18next && typeof root.i18next.t === 'function') return root.i18next;
    if (typeof require === 'function') {
      try {
        return require('./vendor/i18next.min.js');
      } catch {
        return null;
      }
    }
    return null;
  }

  function resolveResources() {
    if (root.__EXT_I18N_RESOURCES__) return root.__EXT_I18N_RESOURCES__;
    if (typeof require === 'function') {
      try {
        const en = require('./locales/en.json');
        const zh = require('./locales/zh-CN.json');
        const es = require('./locales/es.json');
        return {
          en: { translation: en },
          'zh-CN': { translation: zh },
          zh: { translation: zh },
          es: { translation: es },
          'es-ES': { translation: es },
          'es-419': { translation: es },
        };
      } catch {
        return {};
      }
    }
    return {};
  }

  let initialized = false;

  function ensureInit(lang) {
    const i18n = resolveI18next();
    const targetLng = normalizeLang(lang);
    if (!i18n) return null;
    if (!initialized || !i18n.isInitialized) {
      i18n.init({
        lng: targetLng,
        fallbackLng: 'en',
        initImmediate: false,
        resources: resolveResources(),
        interpolation: { escapeValue: false },
      });
      initialized = true;
    } else if (i18n.language !== targetLng) {
      i18n.changeLanguage(targetLng);
    }
    return i18n;
  }

  function setLanguage(lang) {
    ensureInit(lang);
  }

  function t(key, vars, langOverride) {
    const i18n = ensureInit(langOverride || (resolveI18next()?.language || 'en'));
    if (!i18n) return String(key);
    return i18n.t(key, vars || {});
  }

  function intlLocale(lang) {
    const norm = normalizeLang(lang);
    if (norm === 'zh-CN') return 'zh-CN';
    if (norm === 'es') return 'es-US';
    return 'en-US';
  }

  function formatNumber(n, lang) {
    if (!Number.isFinite(Number(n))) return '—';
    return new Intl.NumberFormat(intlLocale(lang), { maximumFractionDigits: 0 }).format(Math.round(Number(n)));
  }

  function formatCurrency(n, lang) {
    if (!Number.isFinite(Number(n))) return '—';
    return new Intl.NumberFormat(intlLocale(lang), {
      style: 'currency',
      currency: 'USD',
      currencyDisplay: 'narrowSymbol',
      maximumFractionDigits: 0,
    }).format(Math.round(Number(n)));
  }

  function formatCompactCurrency(n, lang) {
    if (!Number.isFinite(Number(n)) || Number(n) <= 0) return '—';
    const v = Number(n);
    const locale = intlLocale(lang);
    if (v >= 1e6) {
      const m = new Intl.NumberFormat(locale, { maximumFractionDigits: 2 }).format(v / 1e6);
      return `$${m}M`;
    }
    if (v >= 1e3) {
      const k = new Intl.NumberFormat(locale, { maximumFractionDigits: 0 }).format(Math.round(v / 1e3));
      return `$${k}K`;
    }
    return formatCurrency(v, lang);
  }

  ensureInit('en');

  root.ExtI18n = Object.freeze({
    normalizeLang,
    toShortLang,
    nextLangCycle,
    appendLangParam,
    ensureInit,
    setLanguage,
    t,
    formatNumber,
    formatCurrency,
    formatCompactCurrency,
  });
})(typeof globalThis !== 'undefined' ? globalThis : self);
