// Server-rendered HTML for `/alerts` — Subscription Manager UI (`GET /alerts`)
// Supporting token/manage-link authentication, browser-saved alert cards,
// quick alert creation with cadence (`instant` / `daily` / `weekly`),
// inline editing, pause/resume (with 402 paywall handling), live match previews,
// and EN / 中文 / ES i18n via `fluent-templates` + `i18next`.

use crate::shared::format_brand_wordmark_html;
use crate::web::favicon::{logo_mark_img, FAVICON_HEAD_TAGS};
use crate::web::i18n::{
    bcp47_code, is_es, is_zh, resolve_lang_str, t, t_brand, LANG_COOKIE_NAME,
};
use crate::web::render::{
    localize_i18n_json_for_brand, safe_inline_js, safe_inline_json, I18NEXT_JS, I18N_EN_JSON,
    I18N_ES_JSON, I18N_ZH_JSON,
};
use minijinja::{context, Environment};
use once_cell::sync::Lazy;

const ALERTS_MANAGER_TEMPLATE: &str = r###"<!doctype html>
<html lang="{{ html_lang }}">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>{{ page_title }}</title>
  {{ favicon_tags | safe }}
  <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;600;700;800&display=swap" rel="stylesheet">
  <style>
    :root{
      --brand:#0f766e;--brand2:#14b8a6;--brand-dark:#0d655e;--brand-soft:#effcf8;
      --ink:#111827;--muted:#4b5563;--subtle:#6b7280;--line:#e5e7eb;--card-border:#d1d5db;
      --bg:#f5f6f8;--ok-bg:#ecfdf5;--ok-fg:#065f46;--ok-bd:#a7f3d0;
      --warn-bg:#fffbeb;--warn-fg:#92400e;--warn-bd:#fde68a;
      --err-bg:#fef2f2;--err-fg:#991b1b;--err-bd:#fecaca;
    }
    *{box-sizing:border-box}
    body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.55 Inter,system-ui,-apple-system,sans-serif}
    .wrap{max-width:1060px;margin:0 auto;padding:22px 20px 64px}
    .topbar{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-bottom:18px;flex-wrap:wrap}
    .logo{display:inline-flex;align-items:center;gap:10px;font-weight:800;font-size:21px;color:var(--ink);text-decoration:none}
    .logo .logo-mark{width:34px;height:34px;flex:0 0 34px;display:block}
    .logo b{color:var(--brand)}
    .navwrap{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
    .navlink{border:1px solid var(--line);background:#fff;color:#374151;font:700 13px Inter,sans-serif;padding:6px 12px;border-radius:9px;text-decoration:none;transition:border-color .14s,color .14s,background .14s}
    .navlink:hover{border-color:var(--brand);color:var(--brand);background:var(--brand-soft)}
    .navlink.on{background:var(--brand);border-color:var(--brand);color:#fff}
    .langsw{display:inline-flex;align-items:center;background:#fff;border:1px solid var(--card-border);border-radius:999px;padding:2px;gap:2px}
    .langsw button{border:0;background:transparent;border-radius:999px;padding:4px 11px;font:700 12px Inter,sans-serif;color:var(--muted);cursor:pointer}
    .langsw button.on{background:var(--brand);color:#fff;box-shadow:0 1px 3px rgba(15,118,110,.3)}
    .hero{margin-bottom:18px;display:flex;justify-content:space-between;align-items:flex-end;gap:12px;flex-wrap:wrap}
    h1{font-size:28px;line-height:1.2;letter-spacing:-.6px;margin:0 0 5px}
    .sub{color:var(--muted);font-size:14.5px;margin:0;max-width:740px}
    .stack{display:flex;flex-direction:column;gap:16px}
    .card{background:#fff;border:1px solid var(--card-border);border-radius:16px;padding:20px;box-shadow:0 10px 30px -20px rgba(17,24,39,.18)}
    .card-hd{display:flex;justify-content:space-between;align-items:flex-start;gap:10px;flex-wrap:wrap;margin-bottom:10px}
    .card-title{font-size:17.5px;font-weight:800;margin:0;color:var(--ink)}
    .card-sub{font-size:13.2px;color:var(--muted);margin:0 0 14px}
    .row2{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin-bottom:12px}
    .row3{display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;margin-bottom:12px}
    @media(max-width:760px){.row3{grid-template-columns:1fr 1fr}}
    @media(max-width:640px){.row2,.row3{grid-template-columns:1fr}.wrap{padding:16px 14px 48px}h1{font-size:23px}}
    .lbl{display:block;font-size:12.5px;font-weight:700;color:#374151;margin-bottom:5px}
    .tinput,.nlarea,.stsel{width:100%;border:1px solid var(--card-border);border-radius:10px;padding:9px 12px;font:14px Inter,system-ui,sans-serif;outline:none;background:#fff;color:var(--ink)}
    .nlarea{resize:vertical;min-height:74px;line-height:1.45}
    .tinput:focus,.nlarea:focus,.stsel:focus{border-color:var(--brand);box-shadow:0 0 0 3px rgba(20,184,166,.16)}
    .chkrow{display:flex;gap:16px;flex-wrap:wrap;margin:10px 0 4px;font-size:13.5px;color:#374151}
    .chkrow label{display:inline-flex;align-items:center;gap:6px;cursor:pointer}
    .chkrow input{accent-color:var(--brand)}
    .barfoot{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-top:12px;flex-wrap:wrap}
    .btnpri{border:0;background:var(--brand);color:#fff;font:700 13.5px Inter,sans-serif;padding:10px 18px;border-radius:10px;cursor:pointer;text-decoration:none;display:inline-flex;align-items:center;gap:6px}
    .btnpri:hover{background:var(--brand-dark)}
    .btnpri:disabled{opacity:.6;cursor:wait}
    .btnsec{border:1px solid var(--brand);background:var(--brand-soft);color:var(--brand);font:700 12.5px Inter,sans-serif;padding:7px 12px;border-radius:9px;cursor:pointer;text-decoration:none;display:inline-flex;align-items:center;gap:5px}
    .btnsec:hover{background:#d1fae5}
    .btnmut{border:1px solid var(--card-border);background:#fff;color:#374151;font:600 12.5px Inter,sans-serif;padding:7px 12px;border-radius:9px;cursor:pointer;text-decoration:none;display:inline-flex;align-items:center;gap:5px}
    .btnmut:hover{background:#f3f4f6}
    .btndel{border:1px solid var(--err-bd);background:#fff;color:var(--err-fg);font:700 12.5px Inter,sans-serif;padding:7px 12px;border-radius:9px;cursor:pointer;display:inline-flex;align-items:center;gap:5px}
    .btndel:hover{background:var(--err-bg)}
    .pill{display:inline-flex;align-items:center;gap:5px;font-size:11px;font-weight:800;padding:3px 9px;border-radius:999px;border:1px solid transparent}
    .pill-active{background:var(--ok-bg);color:var(--ok-fg);border-color:var(--ok-bd)}
    .pill-paused{background:var(--warn-bg);color:var(--warn-fg);border-color:var(--warn-bd)}
    .pill-freq{background:var(--brand-soft);color:var(--brand);border-color:#99f6e4}
    .pill-meta{background:#f3f4f6;color:#4b5563;border-color:#e5e7eb}
    .bgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(275px,1fr));gap:12px}
    .bcard{background:#f9fafb;border:1px solid var(--line);border-radius:13px;padding:13px 14px;display:flex;flex-direction:column;gap:6px}
    .bcard:hover{border-color:#99f6e4}
    .subcard{border:1px solid var(--card-border);border-radius:16px;padding:18px 20px;background:#fff;box-shadow:0 10px 26px -20px rgba(17,24,39,.16)}
    .subcard.paused{border-color:#fde68a;background:linear-gradient(180deg,#fffef9 0%,#fff 90px)}
    .subhead{display:flex;justify-content:space-between;align-items:flex-start;gap:10px;flex-wrap:wrap;padding-bottom:12px;margin-bottom:12px;border-bottom:1px solid var(--line)}
    .subacts{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;margin-top:14px;padding-top:12px;border-top:1px solid var(--line)}
    .actgrp{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
    .statusmsg{font-size:13px;font-weight:600;min-height:18px}
    .banner{border-radius:12px;padding:11px 14px;font-size:13.5px;font-weight:600;margin-top:10px;display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap}
    .banner-ok{background:var(--ok-bg);border:1px solid var(--ok-bd);color:var(--ok-fg)}
    .banner-warn{background:var(--warn-bg);border:1px solid var(--warn-bd);color:var(--warn-fg)}
    .banner-err{background:var(--err-bg);border:1px solid var(--err-bd);color:var(--err-fg)}
    /* Compact Redfin/Zillow-style preview match cards */
    .mwrap{margin-top:14px;padding-top:14px;border-top:1px dashed var(--card-border)}
    .mgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(250px,1fr));gap:12px;margin-top:10px}
    .hcard{border:1px solid var(--line);border-radius:13px;overflow:hidden;background:#fff;display:flex;flex-direction:column;transition:border-color .14s,box-shadow .14s}
    .hcard:hover{border-color:var(--brand2);box-shadow:0 8px 22px -15px rgba(15,118,110,.38)}
    .hphoto{height:142px;background:#f3f4f6;position:relative;overflow:hidden;border-bottom:1px solid #f1f5f9;display:grid;place-items:center;color:#9ca3af}
    .hphoto img{width:100%;height:100%;object-fit:cover;display:block}
    .hbadge{position:absolute;top:9px;left:9px;font-size:10.5px;font-weight:800;letter-spacing:.3px;padding:2px 8px;border-radius:999px;box-shadow:0 2px 6px rgba(0,0,0,.15)}
    .hb-sale{background:#0f766e;color:#fff}.hb-pend{background:#b45309;color:#fff}.hb-sold{background:#6d28d9;color:#fff}
    .hbody{padding:11px 13px 13px;display:flex;flex-direction:column;gap:4px;flex:1}
    .hprice{font-size:19px;font-weight:800;color:var(--ink);line-height:1.2;font-variant-numeric:tabular-nums}
    .hold{font-size:12.5px;font-weight:600;color:var(--subtle);text-decoration:line-through;margin-left:5px}
    .hspecs{font-size:12.5px;color:#374151;font-weight:600}
    .haddr{font-size:13px;color:var(--muted);line-height:1.35;margin-bottom:6px}
    .hfoot{margin-top:auto;padding-top:6px;display:flex;justify-content:space-between;align-items:center;gap:8px}
    .inline-form{display:flex;gap:9px;align-items:center;flex-wrap:wrap}
    .inline-form .tinput{flex:1 1 260px}
    footer{margin-top:28px;padding-top:14px;border-top:1px solid var(--line);display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;font-size:12px;color:var(--subtle)}
  </style>
</head>
<body>
  <div class="wrap">
    <div class="topbar">
      <a class="logo" href="/">{{ logo_mark | safe }}<span>{{ brand_wordmark | safe }}</span></a>
      <div class="navwrap">
        <a class="navlink" href="/" id="navSearch">{{ nav_search }}</a>
        <a class="navlink" href="/shop" id="navShop">{{ nav_shop }}</a>
        <a class="navlink" href="/?tab=alerts" id="navHomeAlerts">{{ nav_alerts_tab }}</a>
        <a class="navlink" href="/deal-room" id="navDealRoom">{{ nav_deal_room }}</a>
        <a class="navlink on" href="/alerts" id="navManageAlerts">{{ nav_manage_alerts }}</a>
        <div class="langsw" id="langSwitch" title="Switch language (English / 简体中文 / Español)">
          <button type="button" data-lang="en" class="{% if not is_zh and not is_es %}on{% endif %}">EN</button>
          <button type="button" data-lang="zh-CN" class="{% if is_zh %}on{% endif %}">中文</button>
          <button type="button" data-lang="es" class="{% if is_es %}on{% endif %}">ES</button>
        </div>
      </div>
    </div>

    <div class="hero">
      <div>
        <h1 id="pageTitle">{{ hero_title }}</h1>
        <p class="sub" id="pageSub">{{ hero_sub }}</p>
      </div>
      <a class="btnmut" href="/alerts" id="backOverviewBtn" style="display:none"></a>
    </div>

    <div id="tokenBannerHost"></div>

    <div id="managedSection" class="stack" style="display:none"></div>

    <div id="overviewSection" class="stack">
      <div class="card" id="findAlertsCard">
        <h2 class="card-title" id="findCardTitle">{{ find_title }}</h2>
        <p class="card-sub" id="findCardSub">{{ find_sub }}</p>
        <form id="manageLinkForm" class="inline-form">
          <input id="manageLinkEmail" class="tinput" type="email" placeholder="buyer@example.com" required>
          <button type="submit" id="manageLinkBtn" class="btnpri">{{ find_btn }}</button>
        </form>
        <div id="manageLinkMsg" class="statusmsg" style="margin-top:9px"></div>
      </div>

      <div class="card" id="browserSavedCard">
        <div class="card-hd">
          <div>
            <h2 class="card-title" id="browserSavedTitle">{{ saved_title }}</h2>
            <p class="card-sub" id="browserSavedSub" style="margin:3px 0 0">{{ saved_sub }}</p>
          </div>
          <span class="pill pill-meta" id="browserSavedCount">0</span>
        </div>
        <div id="browserSavedGrid" class="bgrid" style="margin-top:12px"></div>
      </div>

      <div class="card" id="quickCreateCard">
        <h2 class="card-title" id="quickCreateTitle">{{ create_title }}</h2>
        <p class="card-sub" id="quickCreateSub">{{ create_sub }}</p>
        <form id="quickAlertForm">
          <div class="row3">
            <div>
              <label class="lbl" for="qcEmail" id="qcEmailLbl">{{ lbl_email }}</label>
              <input id="qcEmail" class="tinput" type="email" placeholder="buyer@example.com" required>
            </div>
            <div>
              <label class="lbl" for="qcName" id="qcNameLbl">{{ lbl_nickname }}</label>
              <input id="qcName" class="tinput" type="text" placeholder="e.g. Primary Home Search" maxlength="100">
            </div>
            <div>
              <label class="lbl" for="qcZips" id="qcZipsLbl">{{ lbl_zips }}</label>
              <input id="qcZips" class="tinput" type="text" placeholder="5-digit US ZIP(s), comma-separated" required>
            </div>
          </div>
          <div style="margin-bottom:10px">
            <label class="lbl" for="qcReq" id="qcReqLbl">{{ lbl_req }}</label>
            <textarea id="qcReq" class="nlarea" placeholder="{{ ph_req }}" required></textarea>
          </div>
          <div class="row2">
            <div>
              <label class="lbl" for="qcFreq" id="qcFreqLbl">{{ lbl_freq }}</label>
              <select id="qcFreq" class="stsel">
                <option value="instant">{{ freq_instant }}</option>
                <option value="daily">{{ freq_daily }}</option>
                <option value="weekly">{{ freq_weekly }}</option>
              </select>
            </div>
            <div class="chkrow" style="align-self:end;margin:0;padding-bottom:6px">
              <label><input type="checkbox" id="qcNew" checked> <span id="qcNewLbl">{{ lbl_notify_new }}</span></label>
              <label><input type="checkbox" id="qcSold"> <span id="qcSoldLbl">{{ lbl_notify_sold }}</span></label>
            </div>
          </div>
          <div class="barfoot">
            <div id="qcStatusMsg" class="statusmsg"></div>
            <button type="submit" id="qcSubmitBtn" class="btnpri">{{ btn_subscribe }}</button>
          </div>
        </form>
      </div>
    </div>

    <footer>
      <div><b>{{ brand_name }}</b> · {{ brand_domain }}</div>
      <div style="display:flex;gap:14px;flex-wrap:wrap">
        <a href="/" id="footSearch" style="color:var(--subtle);text-decoration:none;font-weight:600">{{ nav_search }}</a>
        <a href="/deal-room" id="footDealRoom" style="color:var(--subtle);text-decoration:none;font-weight:600">{{ nav_deal_room }}</a>
        <a href="/alerts" id="footManageAlerts" style="color:var(--subtle);text-decoration:none;font-weight:600">{{ nav_manage_alerts }}</a>
      </div>
    </footer>
  </div>

  <script>{{ i18next_js | safe }}</script>
  <script>
  window.__OPEN_ESTATE_ALERTS_BOOT__ = {{ boot_json | safe }};
  window.__OPEN_ESTATE_I18N_RESOURCES__ = {
    en: { translation: {{ i18n_en_json | safe }} },
    "zh-CN": { translation: {{ i18n_zh_json | safe }} },
    es: { translation: {{ i18n_es_json | safe }} }
  };
  </script>
  <script>{{ alerts_client_js | safe }}</script>
</body>
</html>"###;

const ALERTS_CLIENT_JS: &str = r###"
(function(){
  'use strict';
  const BOOT = window.__OPEN_ESTATE_ALERTS_BOOT__ || {};
  const LANG_COOKIE = BOOT.langCookie || 'open_estate_lang';
  const BRAND_NAME = BOOT.brandName || __ALERTS_BRAND_NAME_JSON__;
  const MY_ALERTS_KEY = 'openestate:my_alerts:v1';
  const i18nRes = window.__OPEN_ESTATE_I18N_RESOURCES__ || { en: { translation: {} }, 'zh-CN': { translation: {} }, es: { translation: {} } };

  const urlParams = new URLSearchParams(location.search);
  const urlLang = (urlParams.get('lang') || '').toLowerCase();
  const bootLang = String(BOOT.initLang || 'en').toLowerCase();
  const effInitLang = urlLang || bootLang;
  let CUR_LANG = effInitLang.startsWith('zh') ? 'zh-CN' : effInitLang.startsWith('es') ? 'es' : 'en';

  if (window.i18next) {
    window.i18next.init({
      lng: CUR_LANG,
      fallbackLng: 'en',
      resources: i18nRes,
      interpolation: { escapeValue: false }
    });
  }

  const T = (key, vars, fallback) => {
    if (window.i18next && typeof window.i18next.t === 'function') {
      const out = window.i18next.t(key, vars || {});
      if (out && out !== key) return out;
    }
    return fallback || key;
  };
  const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
  const escH = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const fmtCurrency = (v) => {
    const n = Number(v);
    if (!Number.isFinite(n) || n <= 0) return '';
    try {
      return new Intl.NumberFormat(CUR_LANG === 'zh-CN' ? 'zh-CN' : CUR_LANG === 'es' ? 'es-US' : 'en-US', { style: 'currency', currency: 'USD', maximumFractionDigits: 0 }).format(Math.round(n));
    } catch { return '$' + Math.round(n).toLocaleString('en-US'); }
  };
  const fmtNum = (v) => {
    const n = Number(v);
    if (!Number.isFinite(n)) return '';
    try { return new Intl.NumberFormat(CUR_LANG === 'zh-CN' ? 'zh-CN' : CUR_LANG === 'es' ? 'es-US' : 'en-US').format(n); }
    catch { return String(n); }
  };
  const safeHttpUrl = (raw) => {
    const s = String(raw || '').trim();
    return /^https?:\/\/[^\s"'<>]+$/i.test(s) ? s : '';
  };
  const safeNavHref = (raw, fallback) => {
    const s = String(raw || '').trim();
    if (/^\/[^\s"'<>]*$/.test(s) || /^https?:\/\/[^\s"'<>]+$/i.test(s)) return s;
    return fallback || '/';
  };

  const urlToken = (urlParams.get('token') || '').trim();
  const urlManage = (urlParams.get('manage') || '').trim();
  const hasAuthToken = Boolean(urlToken || urlManage);
  let loadedSubscriptions = [];
  const matchesCache = new Map();
  const cardFeedback = new Map();

  function readMyAlerts() {
    try {
      const arr = JSON.parse(localStorage.getItem(MY_ALERTS_KEY) || '[]');
      return Array.isArray(arr) ? arr.filter((x) => x && x.email && Array.isArray(x.zips)) : [];
    } catch { return []; }
  }
  function writeMyAlerts(arr) {
    try {
      if (arr && arr.length) localStorage.setItem(MY_ALERTS_KEY, JSON.stringify(arr.slice(0, 12)));
      else localStorage.removeItem(MY_ALERTS_KEY);
    } catch {}
  }
  function syncLocalAlertRecord(sub) {
    if (!sub || !sub.id) return;
    const prev = readMyAlerts();
    const idx = prev.findIndex((x) => x.id === sub.id || (sub.unsubscribeToken && x.unsubscribeToken === sub.unsubscribeToken));
    if (idx >= 0) {
      const merged = Object.assign({}, prev[idx], {
        id: sub.id,
        email: sub.email || prev[idx].email,
        name: sub.name != null ? sub.name : (prev[idx].name || ''),
        zips: Array.isArray(sub.zips) ? sub.zips : prev[idx].zips,
        requirements: sub.requirementsText != null ? sub.requirementsText : (sub.requirements != null ? sub.requirements : prev[idx].requirements),
        notifyOnNew: sub.notifyOnNew != null ? Boolean(sub.notifyOnNew) : prev[idx].notifyOnNew,
        notifyOnSold: sub.notifyOnSold != null ? Boolean(sub.notifyOnSold) : prev[idx].notifyOnSold,
        frequency: sub.frequency || prev[idx].frequency || 'instant',
        active: sub.active != null ? Boolean(sub.active) : prev[idx].active,
        unsubscribeToken: sub.unsubscribeToken || prev[idx].unsubscribeToken || ''
      });
      const next = prev.slice();
      next[idx] = merged;
      writeMyAlerts(next);
    }
  }
  function prependLocalAlertRecord(sub, fallbackEmail, fallbackReq, fallbackNew, fallbackSold, fallbackFreq, fallbackName) {
    const subObj = sub || {};
    const entry = {
      id: subObj.id || String(Date.now()),
      email: subObj.email || fallbackEmail || '',
      name: subObj.name != null ? subObj.name : (fallbackName || ''),
      zips: Array.isArray(subObj.zips) ? subObj.zips : [],
      requirements: subObj.requirementsText != null ? subObj.requirementsText : (fallbackReq || ''),
      notifyOnNew: subObj.notifyOnNew != null ? Boolean(subObj.notifyOnNew) : Boolean(fallbackNew),
      notifyOnSold: subObj.notifyOnSold != null ? Boolean(subObj.notifyOnSold) : Boolean(fallbackSold),
      frequency: subObj.frequency || fallbackFreq || 'instant',
      active: subObj.active !== false,
      unsubscribeToken: subObj.unsubscribeToken || '',
      createdAt: subObj.createdAt || new Date().toISOString()
    };
    const prev = readMyAlerts().filter((x) => x.id !== entry.id);
    writeMyAlerts([entry, ...prev]);
    return entry;
  }
  function removeLocalAlertById(id) {
    if (!id) return;
    writeMyAlerts(readMyAlerts().filter((x) => x.id !== id));
  }

  function freqLabel(freq) {
    const f = String(freq || 'instant').toLowerCase();
    if (f === 'daily') return T('home.alertsFreqDaily', null, L('Daily digest', '每日汇总', 'Resumen diario'));
    if (f === 'weekly') return T('home.alertsFreqWeekly', null, L('Weekly digest', '每周汇总', 'Resumen semanal'));
    return T('home.alertsFreqInstant', null, L('Instant', '实时推送', 'Inmediato'));
  }
  function freqShortLabel(freq) {
    const f = String(freq || 'instant').toLowerCase();
    if (f === 'daily') return L('Daily', '每日汇总', 'Diario');
    if (f === 'weekly') return L('Weekly', '每周汇总', 'Semanal');
    return L('Instant', '实时', 'Inmediato');
  }

  function authQueryForSub(sub) {
    if (urlManage) return 'manage=' + encodeURIComponent(urlManage);
    const tok = (sub && sub.unsubscribeToken) || urlToken;
    return tok ? ('token=' + encodeURIComponent(tok)) : '';
  }

  function applyStaticCopy() {
    document.documentElement.lang = CUR_LANG;
    document.title = L('Saved Home Alerts — ' + BRAND_NAME, '房源订阅管理中心 — ' + BRAND_NAME, 'Alertas Guardadas de Vivienda — ' + BRAND_NAME);
    const setTxt = (id, txt) => { const el = document.getElementById(id); if (el) el.textContent = txt; };
    const setPh = (id, txt) => { const el = document.getElementById(id); if (el) el.setAttribute('placeholder', txt); };
    setTxt('navSearch', L('Search', '首页搜索', 'Buscar'));
    setTxt('navShop', L('Shop', '条件选房', 'Comprar'));
    setTxt('navHomeAlerts', L('Alerts', '邮编订阅', 'Alertas'));
    setTxt('navDealRoom', L('Deal Room', '比房对比室', 'Deal Room'));
    setTxt('navManageAlerts', L('Manage Alerts', '管理订阅', 'Mis Alertas'));
    setTxt('footSearch', L('Search', '首页搜索', 'Buscar'));
    setTxt('footDealRoom', L('Deal Room', '比房对比室', 'Deal Room'));
    setTxt('footManageAlerts', L('Manage Alerts', '管理订阅', 'Mis Alertas'));
    setTxt('pageTitle', hasAuthToken
      ? L('Manage Your Home Alerts', '管理您的房源订阅提醒', 'Administre sus alertas de vivienda')
      : L('Home Alert Preferences', '房源订阅管理中心', 'Centro de alertas guardadas'));
    setTxt('pageSub', hasAuthToken
      ? L('Edit criteria, switch between instant/daily/weekly delivery, pause or resume, and preview current matches.', '随时编辑选房条件、切换实时/每日/每周推送频率、暂停或恢复提醒，并实时预览当前匹配房源。', 'Edite criterios, cambie entre envío inmediato/diario/semanal, pause o reactive y previsualice propiedades.')
      : L('Look up your saved alerts by email, jump straight into alerts saved in this browser, or create a new alert.', '通过邮箱找回管理链接、一键管理本浏览器已保存的提醒，或立即创建新的房源提醒。', 'Busque sus alertas por correo, administre las guardadas en este navegador o cree una nueva alerta.'));
    const backBtn = document.getElementById('backOverviewBtn');
    if (backBtn) {
      backBtn.style.display = hasAuthToken ? 'inline-flex' : 'none';
      backBtn.textContent = L('← All Saved Alerts', '← 返回订阅中心', '← Ver todas las alertas');
    }
    setTxt('findCardTitle', L('Find & Manage Your Saved Alerts', '查找并管理已保存的订阅', 'Busque y administre sus alertas'));
    setTxt('findCardSub', L('Enter the email address you used to subscribe and we will email a secure 24-hour management link.', '输入您订阅时使用的邮箱，我们将向您发送一封 24 小时有效的安全管理链接邮件。', 'Ingrese su correo y le enviaremos un enlace seguro válido por 24 horas para administrar sus alertas.'));
    setTxt('manageLinkBtn', L('Email Manage Link', '发送管理链接', 'Enviar enlace'));
    setTxt('browserSavedTitle', L('Saved in This Browser', '本浏览器已保存的提醒', 'Guardadas en este navegador'));
    setTxt('browserSavedSub', L('One-click access for alerts created in this browser', '一键直达当前浏览器创建的订阅设置', 'Acceso directo a alertas creadas en este navegador'));
    setTxt('quickCreateTitle', L('Create New Home Alert', '创建新的房源提醒', 'Crear nueva alerta'));
    setTxt('quickCreateSub', T('home.alertsSubtitle', null, L('Get email notifications when matching listings appear or sell in your target ZIP codes.', '当目标邮编出现新挂牌、降价或匹配房源时发送邮件通知。', 'Reciba avisos por correo cuando aparezcan propiedades compatibles.')));
    setTxt('qcEmailLbl', T('home.alertsEmail', null, L('Email address *', '邮箱地址 *', 'Correo electrónico *')));
    setTxt('qcNameLbl', L('Alert nickname (optional)', '订阅备注名（可选）', 'Nombre de la alerta (opcional)'));
    setPh('qcName', L('e.g. Primary Home Search', '例如：自住四房搜索', 'ej. Búsqueda principal'));
    setTxt('qcZipsLbl', T('home.alertsZips', null, L('Target 5-digit ZIP code(s) *', '目标 5 位邮编 *', 'Códigos ZIP (5 dígitos) *')));
    setPh('qcZips', T('home.alertsZipsPlaceholder', null, L('5-digit US ZIP(s), comma-separated', '输入 5 位美国邮编（多个用逗号分隔）', 'Códigos ZIP de EE. UU. (separados por comas)')));
    setTxt('qcReqLbl', T('home.alertsReq', null, L('Your home requirements *', '您的选房要求 *', 'Sus requisitos de vivienda *')));
    setPh('qcReq', T('home.alertsReqPlaceholder', null, 'e.g., 4+ bed detached house under $2.25M'));
    setTxt('qcFreqLbl', T('home.alertsFreqLabel', null, L('Alert frequency', '邮件推送频率', 'Frecuencia de envío')));
    setTxt('qcNewLbl', T('home.alertsNotifyNew', null, L('Notify on new / price-reduced listings', '新上市或降价房源匹配时通知', 'Notificar propiedades nuevas o rebajadas')));
    setTxt('qcSoldLbl', T('home.alertsNotifySold', null, L('Also notify when matching homes sell', '同邮编房源成交时也通知', 'También notificar cierres de venta')));
    setTxt('qcSubmitBtn', T('home.alertsSubmitBtn', null, L('Subscribe to Area Alerts', '立即订阅邮编提醒', 'Suscribirse a Alertas')));
    const qcFreq = document.getElementById('qcFreq');
    if (qcFreq && qcFreq.options.length >= 3) {
      qcFreq.options[0].textContent = T('home.alertsFreqInstant', null, 'Instant');
      qcFreq.options[1].textContent = T('home.alertsFreqDaily', null, 'Daily digest');
      qcFreq.options[2].textContent = T('home.alertsFreqWeekly', null, 'Weekly digest');
    }
    document.querySelectorAll('#langSwitch button[data-lang]').forEach((b) => {
      b.classList.toggle('on', b.getAttribute('data-lang') === CUR_LANG);
    });
  }

  function renderBrowserSaved() {
    const grid = document.getElementById('browserSavedGrid');
    const cnt = document.getElementById('browserSavedCount');
    if (!grid) return;
    const list = readMyAlerts();
    if (cnt) cnt.textContent = String(list.length);
    if (!list.length) {
      grid.innerHTML = '<div style="grid-column:1/-1;font-size:13.5px;color:var(--subtle)">' +
        escH(L('No alerts saved in this browser yet — create one below or request a manage link by email above.', '当前浏览器暂无本地保存的订阅记录 — 您可在下方快速创建，或在上方通过邮箱找回。', 'Aún no hay alertas guardadas en este navegador — cree una abajo o solicite un enlace por correo arriba.')) +
        '</div>';
      return;
    }
    grid.innerHTML = list.map((a, idx) => {
      const tok = String(a.unsubscribeToken || '').trim();
      const isPaused = a.active === false;
      const statusPill = isPaused
        ? '<span class="pill pill-paused">' + escH(L('Paused', '已暂停', 'Pausada')) + '</span>'
        : '<span class="pill pill-active">' + escH(L('Active', '运行中', 'Activa')) + '</span>';
      const fPill = '<span class="pill pill-freq">' + escH(freqShortLabel(a.frequency)) + '</span>';
      const actionHtml = tok
        ? '<a class="btnpri" style="padding:6px 12px;font-size:12.5px" href="/alerts?token=' + encodeURIComponent(tok) + '">' + escH(L('Manage Alert →', '一键管理 →', 'Administrar →')) + '</a>'
        : '<button type="button" class="btnsec" data-email-manage="' + escH(a.email) + '">' + escH(L('Email Manage Link', '发送管理链接', 'Enviar enlace')) + '</button>';
      return '<div class="bcard">' +
        '<div style="display:flex;justify-content:space-between;align-items:center;gap:6px">' +
          '<div style="font-weight:800;font-size:14.5px">' + (a.name ? escH(a.name) + ' · ' : '') + 'ZIP ' + escH((a.zips || []).join(', ')) + '</div>' +
          '<button type="button" data-rm-local="' + idx + '" title="Remove from browser" style="border:0;background:none;color:#9ca3af;cursor:pointer;font:800 16px/1 Inter,sans-serif">×</button>' +
        '</div>' +
        '<div style="display:flex;gap:6px;flex-wrap:wrap;align-items:center">' + statusPill + fPill + '</div>' +
        '<div style="font-size:12.5px;color:var(--muted)">' + escH(a.email) + '</div>' +
        '<div style="font-size:12px;color:var(--subtle);white-space:nowrap;overflow:hidden;text-overflow:ellipsis">' + escH(a.requirements || '') + '</div>' +
        '<div style="margin-top:auto;padding-top:6px">' + actionHtml + '</div>' +
      '</div>';
    }).join('');

    grid.querySelectorAll('[data-rm-local]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const idx = Number(btn.getAttribute('data-rm-local'));
        writeMyAlerts(readMyAlerts().filter((_, j) => j !== idx));
        renderBrowserSaved();
      });
    });
    grid.querySelectorAll('[data-email-manage]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const em = btn.getAttribute('data-email-manage') || '';
        const inp = document.getElementById('manageLinkEmail');
        if (inp) {
          inp.value = em;
          inp.focus();
        }
        requestManageLink(em);
      });
    });
  }

  function renderCompactHomeCards(matches) {
    if (!Array.isArray(matches) || !matches.length) {
      return '<div class="banner banner-warn" style="margin-top:8px">' +
        '<span>' + escH(L('No active/matching listings in our synced ZIP cache right now — we will notify you as soon as matching homes appear.', '当前缓存房源中暂未匹配到完全符合条件的房源 — 一旦有新上市或降价房源匹配，系统将立即通知您。', 'No hay viviendas coincidentes en caché en este momento — le avisaremos en cuanto aparezcan.')) + '</span>' +
        '<a class="btnsec" href="/shop">' + escH(L('Browse Homes →', '前往条件选房 →', 'Explorar casas →')) + '</a>' +
      '</div>';
    }
    const cards = matches.map((m) => {
      const stRaw = String(m.status || 'active').toLowerCase();
      const isSold = stRaw.includes('sold');
      const isPend = !isSold && stRaw.includes('pend');
      const hbCls = isSold ? 'hb-sold' : isPend ? 'hb-pend' : 'hb-sale';
      const hbTxt = isSold
        ? T('home.badgeSold', null, L('SOLD', '已成交', 'VENDIDA'))
        : isPend
          ? T('home.badgePending', null, L('PENDING', '交易中', 'PENDIENTE'))
          : T('home.badgeSale', null, L('FOR SALE', '在售', 'EN VENTA'));
      const photo = safeHttpUrl(m.photoUrl);
      const photoHtml = photo
        ? '<img src="' + escH(photo) + '" alt="' + escH(m.address || '') + '" loading="lazy">'
        : '<svg width="42" height="42" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 10.5 12 3l9 7.5"/><path d="M5 9.5V20h14V9.5"/><path d="M9 20v-6h6v6"/></svg>';
      const priceVal = m.price || m.listPrice || m.soldPrice || m.estimatedValue || m.estValue;
      const priceStr = fmtCurrency(priceVal) || T('home.viewEstimate', null, L('View Price', '查看价格', 'Ver precio'));
      const oldVal = Number(m.oldPrice || m.previousPrice);
      const oldHtml = (Number.isFinite(oldVal) && Number.isFinite(Number(priceVal)) && oldVal > Number(priceVal))
        ? '<span class="hold">' + escH(fmtCurrency(oldVal)) + '</span>'
        : '';
      const specs = [
        m.beds != null ? fmtNum(m.beds) + ' ' + L('bd', '房', 'hab') : null,
        m.baths != null ? fmtNum(m.baths) + ' ' + L('ba', '卫', 'ba') : null,
        m.sqft ? fmtNum(m.sqft) + ' ' + L('sq ft', '平方英尺', 'pies²') : null,
      ].filter(Boolean).join(' · ');
      const addrLine = m.address || [m.street, m.city, m.state, m.zip].filter(Boolean).join(', ');
      const reportHref = m.reportSlug
        ? ('/r/' + encodeURIComponent(m.reportSlug) + '/')
        : safeNavHref(m.reportLink, '/report?address=' + encodeURIComponent(addrLine));
      return '<div class="hcard">' +
        '<div class="hphoto">' + photoHtml + '<span class="hbadge ' + hbCls + '">' + escH(hbTxt) + '</span></div>' +
        '<div class="hbody">' +
          '<div class="hprice">' + escH(priceStr) + oldHtml + '</div>' +
          (specs ? '<div class="hspecs">' + escH(specs) + '</div>' : '') +
          '<div class="haddr">' + escH(addrLine) + '</div>' +
          '<div class="hfoot">' +
            '<span style="font-size:11.5px;color:var(--subtle)">' + escH(m.zip ? ('ZIP ' + m.zip) : '') + '</span>' +
            '<a class="btnsec" style="padding:5px 10px;font-size:12px" href="' + escH(reportHref) + '">' + escH(L('View Home →', '查看房源 →', 'Ver propiedad →')) + '</a>' +
          '</div>' +
        '</div>' +
      '</div>';
    }).join('');
    return '<div class="mgrid">' + cards + '</div>';
  }

  function renderPreviewPanelHtml(id, matches) {
    return '<div class="mwrap"><div style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap"><strong style="font-size:13.5px">' +
      escH(L('Current Matching Homes (' + matches.length + ')', '当前匹配房源预览（' + matches.length + ' 套）', 'Propiedades compatibles (' + matches.length + ')')) +
      '</strong><button type="button" class="btnmut" data-close-preview="' + escH(id) + '">' + escH(L('Hide Preview', '收起预览', 'Ocultar')) + '</button></div>' +
      renderCompactHomeCards(matches) + '</div>';
  }

  function updateCardPreviewHost(subId) {
    const host = document.getElementById('managedSection');
    if (!host) return;
    const card = Array.from(host.querySelectorAll('[data-card-id]')).find((el) => el.getAttribute('data-card-id') === subId);
    if (!card) return;
    const previewHost = card.querySelector('[data-preview-host]');
    if (!previewHost) return;
    const cached = matchesCache.get(subId);
    previewHost.innerHTML = cached ? renderPreviewPanelHtml(subId, cached) : '';
    const closeBtn = previewHost.querySelector('[data-close-preview]');
    if (closeBtn) {
      closeBtn.addEventListener('click', () => {
        matchesCache.delete(subId);
        updateCardPreviewHost(subId);
      });
    }
  }

  function renderManagedList() {
    const host = document.getElementById('managedSection');
    if (!host) return;
    if (!loadedSubscriptions.length) {
      host.innerHTML = '<div class="card">' +
        '<h2 class="card-title">' + escH(L('No active or paused alerts found for this link', '未找到此管理链接对应的订阅记录', 'No se encontraron alertas para este enlace')) + '</h2>' +
        '<p class="card-sub" style="margin-top:6px">' + escH(L('Request a new 24-hour management link below or create a new alert.', '请在下方重新发送管理链接邮件，或创建新的房源提醒。', 'Solicite un nuevo enlace abajo o cree una nueva alerta.')) + '</p>' +
        '<a class="btnsec" href="/alerts">' + escH(L('Open Alert Center →', '打开订阅中心 →', 'Abrir centro de alertas →')) + '</a>' +
      '</div>';
      const ov = document.getElementById('overviewSection');
      if (ov) ov.style.display = 'flex';
      return;
    }

    host.innerHTML = loadedSubscriptions.map((sub) => {
      const id = String(sub.id || '');
      const isActive = sub.active !== false;
      const freq = String(sub.frequency || 'instant').toLowerCase();
      const zipsStr = Array.isArray(sub.zips) ? sub.zips.join(', ') : '';
      const reqStr = String(sub.requirementsText || sub.requirements || '');
      const nameStr = String(sub.name || '');
      const statusBadge = isActive
        ? '<span class="pill pill-active">' + escH(L('Active', '监控中', 'Activa')) + '</span>'
        : '<span class="pill pill-paused">' + escH(L('Paused', '已暂停', 'Pausada')) + '</span>';
      const fBadge = '<span class="pill pill-freq">' + escH(freqShortLabel(freq)) + '</span>';
      const matchBadge = (sub.matchCount != null)
        ? '<span class="pill pill-meta">' + escH(L(sub.matchCount + ' sent', '已推送 ' + sub.matchCount + ' 次', sub.matchCount + ' envíos')) + '</span>'
        : '';
      const cachedMatch = matchesCache.get(id);
      const previewHtml = cachedMatch ? renderPreviewPanelHtml(id, cachedMatch) : '';
      const fb = cardFeedback.get(id);
      const fbStyle = fb ? ('color:' + (fb.isErr ? 'var(--err-fg)' : 'var(--ok-fg)')) : '';
      const fbHtml = fb ? (fb.isHtml ? fb.html : escH(fb.text || '')) : '';

      return '<div class="subcard' + (isActive ? '' : ' paused') + '" data-card-id="' + escH(id) + '">' +
        '<div class="subhead">' +
          '<div>' +
            '<div style="display:flex;align-items:center;gap:7px;flex-wrap:wrap">' +
              '<strong style="font-size:16px">' + escH(nameStr || ('ZIP ' + (zipsStr || '—'))) + '</strong>' +
              statusBadge + fBadge + matchBadge +
            '</div>' +
            '<div style="font-size:12.5px;color:var(--muted);margin-top:3px">' + escH(sub.email || '') + (zipsStr ? ' · ZIP ' + escH(zipsStr) : '') + '</div>' +
          '</div>' +
        '</div>' +
        '<form class="sub-edit-form" data-sub-id="' + escH(id) + '">' +
          '<div class="row3">' +
            '<div>' +
              '<label class="lbl">' + escH(L('Alert nickname', '订阅名称 / 备注', 'Nombre de alerta')) + '</label>' +
              '<input name="name" class="tinput" type="text" maxlength="100" value="' + escH(nameStr) + '" placeholder="' + escH(L('e.g. Primary Search', '例如：重点关注学区房', 'ej. Búsqueda principal')) + '">' +
            '</div>' +
            '<div>' +
              '<label class="lbl">' + escH(T('home.alertsZips', null, L('Target ZIP(s)', '目标邮编', 'Códigos ZIP'))) + '</label>' +
              '<input name="zips" class="tinput" type="text" value="' + escH(zipsStr) + '" required>' +
            '</div>' +
            '<div>' +
              '<label class="lbl">' + escH(T('home.alertsFreqLabel', null, L('Alert frequency', '邮件推送频率', 'Frecuencia'))) + '</label>' +
              '<select name="frequency" class="stsel">' +
                '<option value="instant"' + (freq === 'instant' ? ' selected' : '') + '>' + escH(T('home.alertsFreqInstant', null, 'Instant')) + '</option>' +
                '<option value="daily"' + (freq === 'daily' ? ' selected' : '') + '>' + escH(T('home.alertsFreqDaily', null, 'Daily digest')) + '</option>' +
                '<option value="weekly"' + (freq === 'weekly' ? ' selected' : '') + '>' + escH(T('home.alertsFreqWeekly', null, 'Weekly digest')) + '</option>' +
              '</select>' +
            '</div>' +
          '</div>' +
          '<div style="margin-bottom:8px">' +
            '<label class="lbl">' + escH(T('home.alertsReq', null, L('Home requirements', '选房条件要求', 'Requisitos'))) + '</label>' +
            '<textarea name="requirements" class="nlarea" required>' + escH(reqStr) + '</textarea>' +
          '</div>' +
          '<div class="chkrow">' +
            '<label><input name="notifyOnNew" type="checkbox"' + (sub.notifyOnNew !== false ? ' checked' : '') + '> <span>' + escH(T('home.alertsNotifyNew', null, L('Notify on new / price-reduced listings', '新上市或降价通知', 'Nuevas o rebajadas'))) + '</span></label>' +
            '<label><input name="notifyOnSold" type="checkbox"' + (sub.notifyOnSold ? ' checked' : '') + '> <span>' + escH(T('home.alertsNotifySold', null, L('Also notify when matching homes sell', '成交过户也通知', 'Notificar ventas'))) + '</span></label>' +
          '</div>' +
          '<div class="subacts">' +
            '<div class="actgrp">' +
              '<button type="submit" class="btnpri" data-action="save">' + escH(L('Save Changes', '保存修改', 'Guardar cambios')) + '</button>' +
              '<button type="button" class="' + (isActive ? 'btnmut' : 'btnsec') + '" data-action="toggle-active">' + escH(isActive ? L('Pause Alert', '暂停提醒', 'Pausar') : L('Resume Alert', '恢复开启', 'Reactivar')) + '</button>' +
              '<button type="button" class="btnsec" data-action="preview">' + escH(L('Preview Current Matches', '预览当前匹配房源', 'Ver coincidencias')) + '</button>' +
            '</div>' +
            '<button type="button" class="btndel" data-action="delete">' + escH(L('Delete', '删除订阅', 'Eliminar')) + '</button>' +
          '</div>' +
          '<div class="statusmsg" data-card-msg="' + escH(id) + '" style="margin-top:8px;' + fbStyle + '">' + fbHtml + '</div>' +
        '</form>' +
        '<div data-preview-host="' + escH(id) + '">' + previewHtml + '</div>' +
      '</div>';
    }).join('');

    host.querySelectorAll('.sub-edit-form').forEach((form) => {
      const subId = form.getAttribute('data-sub-id') || '';
      const subObj = loadedSubscriptions.find((x) => x.id === subId);
      const msgEl = form.querySelector('[data-card-msg]');
      const setMsg = (html, isErr) => {
        if (!msgEl) return;
        msgEl.style.color = isErr ? 'var(--err-fg)' : 'var(--ok-fg)';
        msgEl.innerHTML = html;
      };

      form.addEventListener('submit', async (ev) => {
        ev.preventDefault();
        const saveBtn = form.querySelector('[data-action="save"]');
        if (saveBtn) saveBtn.disabled = true;
        setMsg(escH(L('Saving changes…', '正在保存更改…', 'Guardando cambios…')), false);
        try {
          const payload = {
            name: (form.elements.name && form.elements.name.value || '').trim(),
            zips: (form.elements.zips && form.elements.zips.value || '').trim(),
            requirements: (form.elements.requirements && form.elements.requirements.value || '').trim(),
            frequency: (form.elements.frequency && form.elements.frequency.value) || 'instant',
            notifyOnNew: Boolean(form.elements.notifyOnNew && form.elements.notifyOnNew.checked),
            notifyOnSold: Boolean(form.elements.notifyOnSold && form.elements.notifyOnSold.checked),
          };
          const qs = authQueryForSub(subObj);
          const res = await fetch('/api/subscriptions/' + encodeURIComponent(subId) + (qs ? ('?' + qs) : ''), {
            method: 'PATCH',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(payload),
          });
          const j = await res.json().catch(() => ({}));
          if (!res.ok || !j.ok) {
            setMsg(escH(j.error || j.message || L('Update failed', '保存失败', 'Error al guardar')), true);
            return;
          }
          if (j.subscription) {
            loadedSubscriptions = loadedSubscriptions.map((x) => x.id === subId ? Object.assign({}, x, j.subscription) : x);
            syncLocalAlertRecord(j.subscription);
            matchesCache.delete(subId);
            cardFeedback.set(subId, { text: L('✓ Changes saved', '✓ 订阅修改已保存', '✓ Cambios guardados'), isErr: false });
            renderManagedList();
          }
        } catch (err) {
          setMsg(escH(err.message || err), true);
        } finally {
          if (saveBtn) saveBtn.disabled = false;
        }
      });

      const toggleBtn = form.querySelector('[data-action="toggle-active"]');
      if (toggleBtn) {
        toggleBtn.addEventListener('click', async () => {
          if (!subObj) return;
          toggleBtn.disabled = true;
          const nextActive = subObj.active === false;
          setMsg(escH(nextActive ? L('Resuming…', '正在恢复…', 'Reactivando…') : L('Pausing…', '正在暂停…', 'Pausando…')), false);
          try {
            const qs = authQueryForSub(subObj);
            const res = await fetch('/api/subscriptions/' + encodeURIComponent(subId) + (qs ? ('?' + qs) : ''), {
              method: 'PATCH',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ active: nextActive }),
            });
            const j = await res.json().catch(() => ({}));
            if (res.status === 402 || (j && j.code === 'PAYWALL_ALERT_LIMIT_REACHED')) {
              const upgHref = safeNavHref(j.upgradeUrl, '/?upgrade=alerts#billing');
              const upgLabel = L('Upgrade Plan →', '升级套餐 →', 'Mejorar plan →');
              const limitInfo = (j.activeCount != null && j.maxActiveNlAlerts != null)
                ? ' (' + j.activeCount + '/' + j.maxActiveNlAlerts + ')'
                : '';
              setMsg(
                '<div class="banner banner-warn"><span>' + escH((j.error || L('Active alert limit reached.', '已达到当前套餐活跃提醒上限。', 'Límite de alertas activas alcanzado.')) + limitInfo) + '</span>' +
                '<a class="btnpri" style="padding:6px 12px;font-size:12px" href="' + escH(upgHref) + '">' + escH(upgLabel) + '</a></div>',
                false
              );
              return;
            }
            if (!res.ok || !j.ok) {
              setMsg(escH(j.error || j.message || L('Action failed', '操作失败', 'Error')), true);
              return;
            }
            if (j.subscription) {
              loadedSubscriptions = loadedSubscriptions.map((x) => x.id === subId ? Object.assign({}, x, j.subscription) : x);
              syncLocalAlertRecord(j.subscription);
              cardFeedback.set(subId, {
                text: nextActive ? L('✓ Alert resumed', '✓ 订阅已恢复开启', '✓ Alerta reactivada') : L('✓ Alert paused', '✓ 订阅已暂停', '✓ Alerta pausada'),
                isErr: false
              });
              renderManagedList();
            }
          } catch (err) {
            setMsg(escH(err.message || err), true);
          } finally {
            toggleBtn.disabled = false;
          }
        });
      }

      const prevBtn = form.querySelector('[data-action="preview"]');
      if (prevBtn) {
        prevBtn.addEventListener('click', async () => {
          prevBtn.disabled = true;
          setMsg(escH(L('Loading matching homes…', '正在加载当前匹配房源…', 'Cargando coincidencias…')), false);
          try {
            const qs = authQueryForSub(subObj);
            const res = await fetch('/api/subscriptions/' + encodeURIComponent(subId) + '/matches' + (qs ? ('?' + qs) : ''), { cache: 'no-store' });
            const j = await res.json().catch(() => ({}));
            if (!res.ok || !j.ok) {
              setMsg(escH(j.error || j.message || L('Failed to load matches', '加载匹配房源失败', 'Error al cargar')), true);
              return;
            }
            matchesCache.set(subId, Array.isArray(j.matches) ? j.matches : []);
            setMsg('', false);
            updateCardPreviewHost(subId);
          } catch (err) {
            setMsg(escH(err.message || err), true);
          } finally {
            prevBtn.disabled = false;
          }
        });
      }

      const delBtn = form.querySelector('[data-action="delete"]');
      if (delBtn) {
        delBtn.addEventListener('click', async () => {
          delBtn.disabled = true;
          setMsg(escH(L('Deleting…', '正在删除…', 'Eliminando…')), false);
          try {
            const qs = authQueryForSub(subObj);
            const res = await fetch('/api/subscriptions/' + encodeURIComponent(subId) + (qs ? ('?' + qs) : ''), {
              method: 'DELETE',
              headers: { 'Content-Type': 'application/json' },
            });
            const j = await res.json().catch(() => ({}));
            if (!res.ok || !j.ok) {
              setMsg(escH(j.error || j.message || L('Delete failed', '删除失败', 'Error al eliminar')), true);
              return;
            }
            loadedSubscriptions = loadedSubscriptions.filter((x) => x.id !== subId);
            matchesCache.delete(subId);
            cardFeedback.delete(subId);
            removeLocalAlertById(subId);
            renderManagedList();
            renderBrowserSaved();
          } catch (err) {
            setMsg(escH(err.message || err), true);
          } finally {
            delBtn.disabled = false;
          }
        });
      }
    });

    host.querySelectorAll('[data-close-preview]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const sid = btn.getAttribute('data-close-preview') || '';
        matchesCache.delete(sid);
        updateCardPreviewHost(sid);
      });
    });
  }

  async function loadManagedSubscriptions() {
    const managedHost = document.getElementById('managedSection');
    const overviewHost = document.getElementById('overviewSection');
    const bannerHost = document.getElementById('tokenBannerHost');
    if (!hasAuthToken) {
      if (managedHost) managedHost.style.display = 'none';
      if (overviewHost) overviewHost.style.display = 'flex';
      return;
    }
    if (managedHost) {
      managedHost.style.display = 'flex';
      managedHost.innerHTML = '<div class="card">' + escH(L('Loading your saved alerts…', '正在加载您的订阅设置…', 'Cargando sus alertas guardadas…')) + '</div>';
    }
    if (overviewHost) overviewHost.style.display = 'none';
    try {
      const qs = urlManage ? ('manage=' + encodeURIComponent(urlManage)) : ('token=' + encodeURIComponent(urlToken));
      const res = await fetch('/api/subscriptions?' + qs, { cache: 'no-store' });
      const j = await res.json().catch(() => ({}));
      if (!res.ok || !j.ok) {
        if (bannerHost) {
          bannerHost.innerHTML = '<div class="banner banner-err">' + escH(j.error || j.message || L('This management link is invalid or has expired.', '该管理链接无效或已过期。', 'Este enlace de administración no es válido o venció.')) + '</div>';
        }
        loadedSubscriptions = [];
        if (managedHost) managedHost.style.display = 'none';
        if (overviewHost) overviewHost.style.display = 'flex';
        return;
      }
      loadedSubscriptions = Array.isArray(j.subscriptions) ? j.subscriptions : [];
      loadedSubscriptions.forEach(syncLocalAlertRecord);
      if (bannerHost && loadedSubscriptions.length) {
        const em = j.email || loadedSubscriptions[0].email || '';
        bannerHost.innerHTML = em
          ? '<div class="banner banner-ok" style="margin-bottom:14px"><span>' + escH(L('Managing ' + loadedSubscriptions.length + ' alert(s) for ' + em, '正在管理 ' + em + ' 的 ' + loadedSubscriptions.length + ' 条房源提醒', 'Administrando ' + loadedSubscriptions.length + ' alerta(s) de ' + em)) + '</span></div>'
          : '';
      }
      renderManagedList();
    } catch (err) {
      if (bannerHost) {
        bannerHost.innerHTML = '<div class="banner banner-err">' + escH(String(err.message || err)) + '</div>';
      }
      if (overviewHost) overviewHost.style.display = 'flex';
    }
  }

  async function requestManageLink(emailRaw) {
    const msg = document.getElementById('manageLinkMsg');
    const btn = document.getElementById('manageLinkBtn');
    const email = String(emailRaw || '').trim();
    if (!email) return;
    if (btn) btn.disabled = true;
    if (msg) {
      msg.style.color = 'var(--brand)';
      msg.textContent = L('Sending manage link…', '正在发送安全管理链接…', 'Enviando enlace…');
    }
    try {
      const res = await fetch('/api/subscriptions/manage-link', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email, lang: CUR_LANG }),
      });
      const j = await res.json().catch(() => ({}));
      if (!res.ok || !j.ok) {
        if (msg) {
          msg.style.color = 'var(--err-fg)';
          msg.textContent = j.error || j.message || L('Could not send manage link.', '发送失败，请稍后重试。', 'No se pudo enviar el enlace.');
        }
        return;
      }
      if (msg) {
        msg.style.color = 'var(--ok-fg)';
        msg.textContent = L(
          '✓ Check your inbox — if saved alerts exist for that email, a 24-hour manage link has been sent.',
          '✓ 若该邮箱名下有已保存的订阅，我们已向其发送 24 小时有效管理链接，请查收邮件。',
          '✓ Revise su correo — si hay alertas guardadas, enviamos un enlace válido por 24 horas.'
        );
      }
    } catch (err) {
      if (msg) {
        msg.style.color = 'var(--err-fg)';
        msg.textContent = String(err.message || err);
      }
    } finally {
      if (btn) btn.disabled = false;
    }
  }

  const manageLinkForm = document.getElementById('manageLinkForm');
  if (manageLinkForm) {
    manageLinkForm.addEventListener('submit', (ev) => {
      ev.preventDefault();
      requestManageLink(document.getElementById('manageLinkEmail')?.value || '');
    });
  }

  const quickAlertForm = document.getElementById('quickAlertForm');
  if (quickAlertForm) {
    quickAlertForm.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const btn = document.getElementById('qcSubmitBtn');
      const msg = document.getElementById('qcStatusMsg');
      const emailVal = (document.getElementById('qcEmail')?.value || '').trim();
      const nameVal = (document.getElementById('qcName')?.value || '').trim();
      const zipsVal = (document.getElementById('qcZips')?.value || '').trim();
      const reqVal = (document.getElementById('qcReq')?.value || '').trim();
      const freqVal = (document.getElementById('qcFreq')?.value || 'instant').trim();
      const notifyNewVal = Boolean(document.getElementById('qcNew')?.checked);
      const notifySoldVal = Boolean(document.getElementById('qcSold')?.checked);
      if (btn) btn.disabled = true;
      if (msg) {
        msg.style.color = 'var(--brand)';
        msg.textContent = T('home.submittingAlert', null, L('Creating alert…', '正在创建订阅…', 'Creando alerta…'));
      }
      try {
        const res = await fetch('/api/subscriptions', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            email: emailVal,
            name: nameVal,
            zips: zipsVal,
            requirements: reqVal,
            notifyOnNew: notifyNewVal,
            notifyOnSold: notifySoldVal,
            frequency: freqVal,
          }),
        });
        const j = await res.json().catch(() => ({}));
        if (res.status === 402 || (j && j.code === 'PAYWALL_ALERT_LIMIT_REACHED')) {
          const upgHref = safeNavHref(j.upgradeUrl, '/?upgrade=alerts#billing');
          if (msg) {
            msg.style.color = 'var(--warn-fg)';
            msg.innerHTML = escH(j.error || L('Active alert limit reached.', '已达到活跃订阅数量上限。', 'Límite de alertas activas alcanzado.')) +
              ' <a href="' + escH(upgHref) + '" style="color:var(--brand);font-weight:700;text-decoration:underline">' + escH(L('Upgrade Plan →', '升级套餐 →', 'Mejorar plan →')) + '</a>';
          }
          return;
        }
        if (!res.ok || !j.ok) {
          if (msg) {
            msg.style.color = 'var(--err-fg)';
            msg.textContent = j.error || j.message || T('home.subFailed', null, L('Subscription failed', '创建订阅失败', 'Error al suscribirse'));
          }
          return;
        }
        const entry = prependLocalAlertRecord(j.subscription, emailVal, reqVal, notifyNewVal, notifySoldVal, freqVal, nameVal);
        renderBrowserSaved();
        const manageHref = entry.unsubscribeToken ? ('/alerts?token=' + encodeURIComponent(entry.unsubscribeToken)) : '/alerts';
        if (msg) {
          msg.style.color = 'var(--ok-fg)';
          msg.innerHTML = escH(T('home.subSuccess', { zips: (entry.zips || []).join(', ') }, L('✓ Alert created', '✓ 订阅已开启', '✓ Alerta creada'))) +
            (entry.unsubscribeToken ? (' <a href="' + escH(manageHref) + '" style="color:var(--brand);font-weight:700;text-decoration:underline;margin-left:8px">' + escH(L('Manage this alert →', '立即管理此提醒 →', 'Administrar alerta →')) + '</a>') : '');
        }
      } catch (err) {
        if (msg) {
          msg.style.color = 'var(--err-fg)';
          msg.textContent = String(err.message || err);
        }
      } finally {
        if (btn) btn.disabled = false;
      }
    });
  }

  function setLang(next) {
    CUR_LANG = (next === 'zh-CN' || next === 'zh') ? 'zh-CN' : next === 'es' ? 'es' : 'en';
    document.cookie = LANG_COOKIE + '=' + encodeURIComponent(CUR_LANG) + ';path=/;max-age=31536000;SameSite=Lax';
    if (window.i18next) window.i18next.changeLanguage(CUR_LANG);
    applyStaticCopy();
    renderBrowserSaved();
    if (hasAuthToken && loadedSubscriptions.length) renderManagedList();
    try {
      const u = new URL(location.href);
      u.searchParams.set('lang', CUR_LANG === 'zh-CN' ? 'zh' : CUR_LANG);
      history.replaceState(null, '', u.toString());
    } catch {}
  }

  document.querySelectorAll('#langSwitch button[data-lang]').forEach((b) => {
    b.addEventListener('click', () => setLang(b.getAttribute('data-lang')));
  });

  applyStaticCopy();
  renderBrowserSaved();
  if (hasAuthToken) loadManagedSubscriptions();
})();
"###;

static ALERTS_JINJA_ENV: Lazy<Environment<'static>> = Lazy::new(|| {
    let mut env = Environment::new();
    env.add_template("alerts", ALERTS_MANAGER_TEMPLATE).unwrap();
    env
});

pub fn render_alerts_manager_html_for_lang(lang: &str) -> String {
    render_alerts_manager_html(lang, "Open Estate", "open-estate.local")
}

pub fn render_alerts_manager_html(lang: &str, brand_name: &str, brand_domain: &str) -> String {
    let eff_brand = if brand_name.trim().is_empty() {
        "Open Estate"
    } else {
        brand_name.trim()
    };
    let eff_domain = if brand_domain.trim().is_empty() {
        "open-estate.local"
    } else {
        brand_domain.trim()
    };
    let brand_wordmark = format_brand_wordmark_html(eff_brand);
    let lang = resolve_lang_str(lang);
    let zh = is_zh(&lang);
    let es = is_es(&lang);
    let pick = |en: &'static str, zh_s: &'static str, es_s: &'static str| -> &'static str {
        if zh {
            zh_s
        } else if es {
            es_s
        } else {
            en
        }
    };
    let init_lang = bcp47_code(&lang);
    let boot_json = safe_inline_json(
        &serde_json::to_string(&serde_json::json!({
            "initLang": init_lang,
            "langCookie": LANG_COOKIE_NAME,
            "brandName": eff_brand,
            "brandDomain": eff_domain,
        }))
        .unwrap_or_else(|_| "{}".into()),
    );
    let deal_room_nav = t_brand(&lang, "deal-room-title", eff_brand)
        .split('—')
        .next()
        .unwrap_or("Deal Room")
        .trim()
        .to_string();

    let tmpl = ALERTS_JINJA_ENV.get_template("alerts").unwrap();
    tmpl.render(context! {
        favicon_tags => FAVICON_HEAD_TAGS.as_str(),
        logo_mark => logo_mark_img(34),
        brand_wordmark => brand_wordmark,
        brand_name => eff_brand,
        brand_domain => eff_domain,
        html_lang => init_lang,
        is_zh => zh,
        is_es => es,
        page_title => format!("{} — {eff_brand}", pick("Saved Home Alerts", "房源订阅管理中心", "Alertas Guardadas")),
        nav_search => pick("Search", "首页搜索", "Buscar"),
        nav_shop => pick("Shop", "条件选房", "Comprar"),
        nav_alerts_tab => pick("Alerts", "邮编订阅", "Alertas"),
        nav_deal_room => deal_room_nav,
        nav_manage_alerts => pick("Manage Alerts", "管理订阅", "Mis Alertas"),
        hero_title => pick("Home Alert Preferences", "房源订阅管理中心", "Centro de alertas guardadas"),
        hero_sub => pick(
            "Look up your saved alerts by email, jump straight into alerts saved in this browser, or create a new alert.",
            "通过邮箱找回管理链接、一键管理本浏览器已保存的提醒，或立即创建新的房源提醒。",
            "Busque sus alertas por correo, administre las guardadas en este navegador o cree una nueva alerta."
        ),
        find_title => pick("Find & Manage Your Saved Alerts", "查找并管理已保存的订阅", "Busque y administre sus alertas"),
        find_sub => pick(
            "Enter the email address you used to subscribe and we will email a secure 24-hour management link.",
            "输入您订阅时使用的邮箱，我们将向您发送一封 24 小时有效的安全管理链接邮件。",
            "Ingrese su correo y le enviaremos un enlace seguro válido por 24 horas para administrar sus alertas."
        ),
        find_btn => pick("Email Manage Link", "发送管理链接", "Enviar enlace"),
        saved_title => pick("Saved in This Browser", "本浏览器已保存的提醒", "Guardadas en este navegador"),
        saved_sub => pick(
            "One-click access for alerts created in this browser",
            "一键直达当前浏览器创建的订阅设置",
            "Acceso directo a alertas creadas en este navegador"
        ),
        create_title => pick("Create New Home Alert", "创建新的房源提醒", "Crear nueva alerta"),
        create_sub => t(&lang, "alerts-subtitle"),
        lbl_email => t(&lang, "alerts-email-label"),
        lbl_nickname => pick("Alert nickname (optional)", "订阅备注名（可选）", "Nombre de la alerta (opcional)"),
        lbl_zips => t(&lang, "alerts-zips-label"),
        lbl_req => t(&lang, "alerts-req-label"),
        ph_req => t(&lang, "alerts-req-placeholder"),
        lbl_freq => t(&lang, "alerts-freq-label"),
        freq_instant => t(&lang, "alerts-freq-instant"),
        freq_daily => t(&lang, "alerts-freq-daily"),
        freq_weekly => t(&lang, "alerts-freq-weekly"),
        lbl_notify_new => t(&lang, "alerts-notify-new"),
        lbl_notify_sold => t(&lang, "alerts-notify-sold"),
        btn_subscribe => t(&lang, "alerts-submit-btn"),
        i18next_js => safe_inline_js(I18NEXT_JS),
        boot_json => boot_json,
        i18n_en_json => safe_inline_json(&localize_i18n_json_for_brand(I18N_EN_JSON, eff_brand, eff_domain)),
        i18n_zh_json => safe_inline_json(&localize_i18n_json_for_brand(I18N_ZH_JSON, eff_brand, eff_domain)),
        i18n_es_json => safe_inline_json(&localize_i18n_json_for_brand(I18N_ES_JSON, eff_brand, eff_domain)),
        alerts_client_js => safe_inline_js(&ALERTS_CLIENT_JS.replace(
            "__ALERTS_BRAND_NAME_JSON__",
            &safe_inline_json(
                &serde_json::to_string(eff_brand).unwrap_or_else(|_| "\"Open Estate\"".into()),
            ),
        )),
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_alerts_manager_trilingual_with_nav_brand_and_controls() {
        let html_en = render_alerts_manager_html("en-US", "Open Estate", "open-estate.local");
        assert!(html_en.contains("lang=\"en\""));
        assert!(html_en.contains("Open <b>Estate</b>"));
        assert!(html_en.contains("href=\"/\""));
        assert!(html_en.contains("href=\"/shop\""));
        assert!(html_en.contains("href=\"/?tab=alerts\""));
        assert!(html_en.contains("href=\"/deal-room\""));
        assert!(html_en.contains("href=\"/alerts\""));
        assert!(html_en.contains("Find &amp; Manage Your Saved Alerts") || html_en.contains("Find & Manage Your Saved Alerts"));
        assert!(html_en.contains("Email Manage Link"));
        assert!(html_en.contains("Saved in This Browser"));
        assert!(html_en.contains("Create New Home Alert"));
        assert!(html_en.contains("openestate:my_alerts:v1"));
        assert!(html_en.contains("/api/subscriptions/manage-link"));
        assert!(html_en.contains("PAYWALL_ALERT_LIMIT_REACHED"));
        assert!(html_en.contains("value=\"instant\""));
        assert!(html_en.contains("value=\"daily\""));
        assert!(html_en.contains("value=\"weekly\""));

        let html_zh = render_alerts_manager_html("zh-CN", "Open Estate", "open-estate.local");
        assert!(html_zh.contains("lang=\"zh-CN\""));
        assert!(html_zh.contains("房源订阅管理中心"));
        assert!(html_zh.contains("发送管理链接"));
        assert!(html_zh.contains("本浏览器已保存的提醒"));

        let html_es = render_alerts_manager_html("es", "Open Estate", "open-estate.local");
        assert!(html_es.contains("lang=\"es\""));
        assert!(html_es.contains("Centro de alertas guardadas"));
        assert!(html_es.contains("Enviar enlace"));
    }

    #[test]
    fn rendered_alerts_manager_has_zero_leaks_and_valid_js() {
        const FORBIDDEN: &[&str] = &[
            "gpt-4o-mini",
            "gpt-4.1",
            "rust-sqlx-sqlite",
            "fallbackUsed",
            "deterministic-fallback",
            "max reasoning",
            "aiModel",
            "OPENAI_API_KEY",
            "effort",
            "provider",
        ];
        for lang in ["en-US", "zh-CN", "es"] {
            let html = render_alerts_manager_html(lang, "Open Estate", "open-estate.local");
            assert!(
                !crate::shared::has_retired_brand(&html),
                "alerts manager ({lang}) leaked retired brand identifier"
            );
            let lower = html.to_ascii_lowercase();
            for tok in FORBIDDEN {
                assert!(
                    !lower.contains(&tok.to_ascii_lowercase()),
                    "alerts manager ({lang}) leaked forbidden token `{tok}`"
                );
            }
        }
        if let Ok(ver) = std::process::Command::new("node").arg("--version").output() {
            if ver.status.success() {
                let tmp = std::env::temp_dir().join(format!(
                    "alerts-ui-check-{}.js",
                    uuid::Uuid::new_v4()
                ));
                std::fs::write(&tmp, ALERTS_CLIENT_JS).unwrap();
                let out = std::process::Command::new("node")
                    .arg("--check")
                    .arg(&tmp)
                    .output()
                    .unwrap();
                let _ = std::fs::remove_file(&tmp);
                assert!(
                    out.status.success(),
                    "node --check failed on ALERTS_CLIENT_JS: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
}

