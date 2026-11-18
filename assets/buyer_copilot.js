/* Property Copilot — Role-Aware Chat Q&A + Workflow Tools (Comps, Disclosures, Offer Builder)
 * Integrates natively into the home tab bar (`#tab-assistant`), report header (`/r/:slug`),
 * and floating drawer (`#openEstateDrawer`).
 * Strictly honors AGENTS.md Data Honesty (never invents missing figures) and frosted light modal design.
 */
(function () {
  'use strict';

  if (window.__OPEN_ESTATE_BUYER_COPILOT_LOADED__) return;
  window.__OPEN_ESTATE_BUYER_COPILOT_LOADED__ = true;

  var ROLE_STORAGE_KEY = 'openestate:role_lens:v1';
  var USER_ID_KEY = 'openestate_user_id';
  var USER_EMAIL_KEY = 'openestate_user_email';
  var USER_NAME_KEY = 'openestate_user_name';
  var USER_SESSION_TOKEN_KEY = 'openestate_copilot_session_token';

  function getClientId() {
    try {
      var k = 'openestate_client_id';
      var v = localStorage.getItem(k);
      if (!v || v.length < 8) {
        v = 'oe_' + Math.random().toString(36).slice(2, 12) + Date.now().toString(36);
        localStorage.setItem(k, v);
      }
      return v;
    } catch (_) {
      return 'oe_session_ephemeral';
    }
  }

  function getStoredUser() {
    try {
      var userId = (localStorage.getItem(USER_ID_KEY) || '').trim();
      var email = (localStorage.getItem(USER_EMAIL_KEY) || '').trim();
      var displayName = (localStorage.getItem(USER_NAME_KEY) || '').trim();
      var sessionToken = (localStorage.getItem(USER_SESSION_TOKEN_KEY) || '').trim();
      if (!userId || !sessionToken) return null;
      return {
        userId: userId,
        email: email,
        displayName: displayName,
        sessionToken: sessionToken,
        emailVerified: true
      };
    } catch (_) {
      return null;
    }
  }

  function setStoredUser(user) {
    try {
      if (user && user.userId && user.sessionToken) {
        localStorage.setItem(USER_ID_KEY, String(user.userId).trim());
        localStorage.setItem(USER_EMAIL_KEY, String(user.email || '').trim());
        localStorage.setItem(USER_NAME_KEY, String(user.displayName || user.name || '').trim());
        localStorage.setItem(USER_SESSION_TOKEN_KEY, String(user.sessionToken).trim());
      } else {
        localStorage.removeItem(USER_ID_KEY);
        localStorage.removeItem(USER_EMAIL_KEY);
        localStorage.removeItem(USER_NAME_KEY);
        localStorage.removeItem(USER_SESSION_TOKEN_KEY);
      }
    } catch (_) {}
  }

  function copilotHeaders(extra) {
    var hdrs = { 'X-OpenEstate-Client-Id': getClientId() };
    var u = getStoredUser();
    if (u && u.sessionToken) {
      hdrs['X-OpenEstate-Session-Token'] = u.sessionToken;
    }
    if (extra) {
      Object.keys(extra).forEach(function (k) {
        hdrs[k] = extra[k];
      });
    }
    return hdrs;
  }

  function normalizeAssistantRole(raw) {
    if (raw == null) return 'unset';
    var r = String(raw).trim().toLowerCase();
    if (r === 'buyer' || r === 'seller' || r === 'agent') return r;
    return 'unset';
  }

  function readRawRoleFromEnv() {
    try {
      if (window.OpenEstateRole && typeof window.OpenEstateRole.getRole === 'function') {
        var r = window.OpenEstateRole.getRole();
        if (r) return r;
      }
    } catch (_) {}
    try {
      var ls = localStorage.getItem(ROLE_STORAGE_KEY);
      if (ls) return ls;
    } catch (_) {}
    return 'all';
  }

  function readInitialRole() {
    return normalizeAssistantRole(readRawRoleFromEnv());
  }

  function curLang() {
    var htmlLang = (document.documentElement.getAttribute('lang') || 'en').toLowerCase();
    if (htmlLang.indexOf('zh') === 0) return 'zh-CN';
    if (htmlLang.indexOf('es') === 0) return 'es';
    var activeBtn = document.querySelector('#langSwitch button.on');
    if (activeBtn && activeBtn.dataset && activeBtn.dataset.lang) {
      var dl = String(activeBtn.dataset.lang).toLowerCase();
      if (dl.indexOf('zh') === 0) return 'zh-CN';
      if (dl.indexOf('es') === 0) return 'es';
    }
    return 'en';
  }

  function L(en, zh, es) {
    var lg = curLang();
    if (lg === 'zh-CN') return zh || en;
    if (lg === 'es') return es || en;
    return en;
  }

  function brandName() {
    try {
      if (window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) {
        var bn = String(window.__OPEN_ESTATE_BRAND__.name).trim();
        if (bn) return bn;
      }
      var rep = readPageReport();
      if (rep && rep.brand && rep.brand.name && String(rep.brand.name).trim()) {
        return String(rep.brand.name).trim();
      }
    } catch (_) {}
    return ['Open', 'Estate'].join(' ');
  }

  function assistantBrandTitle() {
    return brandName() + ' AI';
  }

  function isDisplayLinksOn() {
    if (typeof window !== 'undefined' && window.OpenEstateCore && typeof window.OpenEstateCore.isDisplayLinksEnabled === 'function') {
      return window.OpenEstateCore.isDisplayLinksEnabled();
    }
    try {
      var m = (document.cookie || '').match(/(?:^|;\s*)display_link=([^;]*)/);
      if (m) return decodeURIComponent(m[1].trim()) === '1';
    } catch (_) {}
    return Boolean(typeof document !== 'undefined' && document.documentElement && document.documentElement.getAttribute('data-display-links') === '1');
  }

  function toRedirectHref(rawUrl, opts) {
    var clean = safeHttpUrl(rawUrl);
    if (!clean) return '';
    if (typeof window !== 'undefined' && typeof window.toReferenceRedirectUrl === 'function') {
      try {
        var r = window.toReferenceRedirectUrl(clean, opts || {});
        if (r) return r;
      } catch (_) {}
    }
    var o = opts || {};
    var p = new URLSearchParams();
    p.set('type', o.type || 'extension_reference');
    var did = String(o.dataId || o.data_id || 'copilot_reference').trim().toLowerCase().replace(/[^a-z0-9_-]+/g, '_');
    p.set('data_id', did || 'copilot_reference');
    if (o.slug) p.set('slug', String(o.slug));
    p.set('section', String(o.section || 'copilot'));
    p.set('ref', clean);
    return '/redirect?' + p.toString();
  }

  function syncCopilotColorScheme() {
    if (typeof document === 'undefined' || !document.documentElement) return 'light';
    var root = document.documentElement;
    var explicit = root.getAttribute('data-theme');
    var osDark = false;
    try {
      osDark = Boolean(typeof window.matchMedia === 'function' && window.matchMedia('(prefers-color-scheme: dark)').matches);
    } catch (_) {}
    var eff = (explicit === 'light' || explicit === 'dark') ? explicit : (osDark ? 'dark' : 'light');
    root.dataset.osScheme = osDark ? 'dark' : 'light';
    root.dataset.effectiveTheme = eff;
    return eff;
  }

  (function bindCopilotThemeWatcher() {
    syncCopilotColorScheme();
    try {
      if (typeof window.matchMedia === 'function') {
        var mq = window.matchMedia('(prefers-color-scheme: dark)');
        window.__bcThemeMq = mq;
        var onChange = function () { syncCopilotColorScheme(); };
        if (mq && typeof mq.addEventListener === 'function') mq.addEventListener('change', onChange);
        else if (mq && typeof mq.addListener === 'function') mq.addListener(onChange);
      }
      if (typeof MutationObserver !== 'undefined' && document.documentElement) {
        var mo = new MutationObserver(function () { syncCopilotColorScheme(); });
        mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
      }
    } catch (_) {}
  })();

  function roleModeBadgeText(role) {
    if (role === 'buyer') return L('Buyer Mode', '买家模式', 'Modo Comprador');
    if (role === 'seller') return L('Seller Mode', '卖家模式', 'Modo Vendedor');
    if (role === 'agent') return L('Agent Mode', '经纪人模式', 'Modo Agente');
    return '';
  }

  function roleLabelText(role) {
    if (role === 'buyer') return L('Buyer', '买家', 'Comprador');
    if (role === 'seller') return L('Seller', '卖家', 'Vendedor');
    if (role === 'agent') return L('Agent', '经纪人', 'Agente');
    return L('Neutral', '中立视角', 'Neutral');
  }

  function roleSystemPromptLabel(role) {
    if (role === 'buyer') {
      return L(
        'Advisor Prompt: Buyer Due-Diligence Advisor',
        '提示词：买方尽调与出价顾问',
        'Prompt: Asesor de Debida Diligencia (Comprador)'
      );
    }
    if (role === 'seller') {
      return L(
        'Advisor Prompt: Seller Listing & Prep Advisor',
        '提示词：卖方挂牌定价与备售顾问',
        'Prompt: Asesor de Venta y Preparación (Vendedor)'
      );
    }
    if (role === 'agent') {
      return L(
        'Advisor Prompt: Agent & Broker Co-Pilot',
        '提示词：持牌经纪人专业副驾',
        'Prompt: Copiloto para Agentes y Corredores'
      );
    }
    return L(
      'Advisor Prompt: Neutral Analyst (pick a role)',
      '提示词：中立房产分析师（请选择身份）',
      'Prompt: Analista Neutral (elija un rol)'
    );
  }

  function roleStarters(role) {
    if (role === 'buyer') {
      return [
        L(
          'What is a fair offer range vs recent comps, and what should I watch in escrow?',
          '对比近期周边成交，合理出价区间是多少？过户时有哪些关键风险要排查？',
          '¿Cuál es un rango de oferta justo frente a los comparables y qué vigilar en el cierre?'
        ),
        L(
          'What is the estimated monthly PITI carrying cost including property-tax reset?',
          '按新成交价重估房产税后，每月的真实月供（PITI）和持有成本是多少？',
          '¿Cuál es el costo mensual estimado (PITI) tras el reajuste de impuestos?'
        ),
        L(
          'Any permit, HOA, natural hazard, or title red flags on this property?',
          '这套房在施工许可、HOA、自然灾害或产权例外事项上有哪些红旗风险？',
          '¿Hay alertas en permisos, HOA, riesgos naturales o título en esta propiedad?'
        ),
        L(
          'What repair credits or inspection contingencies make sense here?',
          '结合房屋与披露情况，验房保护期与卖方维修抵扣（Credit）该如何谈判？',
          '¿Qué créditos por reparación o contingencias de inspección convienen aquí?'
        )
      ];
    }
    if (role === 'seller') {
      return [
        L(
          'What list price positioning do recent sold comps and active listings support?',
          '结合近期成交与周边在售竞品，这套房的挂牌定价区间如何定位最有竞争力？',
          '¿Qué precio de salida respaldan las ventas comparables y propiedades activas?'
        ),
        L(
          'Which pre-listing inspection, permit, or disclosure issues will buyers flag in escrow?',
          '挂牌前有哪些验房、未结许可或披露问题最容易被买家在托管期要求让价？',
          '¿Qué detalles de inspección, permisos o divulgación señalarán los compradores?'
        ),
        L(
          'Which pre-listing repairs or upgrades offer the best ROI before going live?',
          '挂牌前做哪些轻度翻新或维修最能提升净收益（ROI）？',
          '¿Qué mejoras o reparaciones previas ofrecen mejor retorno antes de publicar?'
        ),
        L(
          'What are customary seller closing costs, transfer taxes, and net proceeds here?',
          '本地卖方习惯承担哪些过户税与产权托管费？预计净到手收益如何估算？',
          '¿Cuáles son los costos de cierre habituales, impuestos de transferencia y neto del vendedor?'
        )
      ];
    }
    if (role === 'agent') {
      return [
        L(
          'Give me the defensible CMA price-per-sqft thesis and active competition brief.',
          '请整理有理有据的 CMA 单价支撑逻辑及周边活跃竞品对比简报。',
          'Deme la tesis CMA por pie² y el resumen de competencia activa.'
        ),
        L(
          'What statutory disclosures, permits, and HOA/title items must be verified before escrow?',
          '托管前必须核验哪些法定披露、施工许可、HOA 财务与产权 Schedule B 事项？',
          '¿Qué divulgaciones legales, permisos y puntos de HOA/título deben verificarse?'
        ),
        L(
          'Summarize local transfer-tax & title/escrow customs and client negotiation levers.',
          '汇总本地市/县过户税与产权托管费惯例，以及可向客户讲解的谈判筹码。',
          'Resuma las costumbres locales de impuestos/escrow y palancas de negociación.'
        ),
        L(
          'What rental yield, cap rate, and client presentation talking points stand out?',
          '这套房源在租金回报、资本化率（Cap Rate）与客户陈述上有哪些核心要点？',
          '¿Qué rendimiento de renta, cap rate y puntos clave destacan para el cliente?'
        )
      ];
    }
    return [
      L(
        'How does this home’s estimated value compare with nearby recent comps?',
        '这套房的估值与周边近期可比成交相比如何？',
        '¿Cómo se compara el valor estimado con las ventas comparables recientes?'
      ),
      L(
        'What are the main property risks, natural hazards, and permit or HOA findings?',
        '这套房有哪些主要的自然灾害、施工许可或 HOA 风险点？',
        '¿Cuáles son los principales riesgos, peligros naturales y hallazgos de permisos o HOA?'
      ),
      L(
        'What is the estimated monthly cost of ownership and local tax context?',
        '每月的持有成本估算以及当地房产税/过户税情况如何？',
        '¿Cuál es el costo mensual estimado de propiedad y el contexto fiscal?'
      )
    ];
  }

  function esc(val) {
    return String(val == null ? '' : val)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function fmtUsd(val) {
    if (val == null || val === '' || !Number.isFinite(Number(val))) return '—';
    return '$' + Math.round(Number(val)).toLocaleString('en-US');
  }

  function fmtNum(val) {
    if (val == null || val === '' || !Number.isFinite(Number(val))) return '—';
    return Number(val).toLocaleString('en-US');
  }

  function formatChatAnswerHtml(rawText) {
    if (typeof window.renderChatMarkdown === 'function') {
      try { return window.renderChatMarkdown(rawText); } catch (_) {}
    }
    var text = String(rawText || '').replace(/\r\n?/g, '\n').replace(/\u0000/g, '').trim();
    if (!text) return '';
    function fmtEmph(escaped) {
      return escaped
        .replace(/\*\*\*([^*]+?)\*\*\*/g, '<strong><em>$1</em></strong>')
        .replace(/\*\*([^*]+?)\*\*/g, '<strong>$1</strong>')
        .replace(/(^|[^\w])__([^_]+?)__(?!\w)/g, '$1<strong>$2</strong>')
        .replace(/(^|[^*])\*([^*\n]+?)\*(?!\*)/g, '$1<em>$2</em>')
        .replace(/(^|[^\w])_([^_\n]+?)_(?!\w)/g, '$1<em>$2</em>');
    }
    function formatInline(line) {
      var tokens = [];
      function stash(html) {
        var id = tokens.length;
        tokens.push(html);
        return '\uE000' + id + '\uE001';
      }
      function expand(s) {
        return s
          .replace(/\uE000(\d+)\uE001/g, function (_, i) { return tokens[Number(i)] || ''; })
          .replace(/\uE000(\d+)\uE001/g, function (_, i) { return tokens[Number(i)] || ''; });
      }
      var work = String(line || '').replace(/`([^`\n]+)`/g, function (_, code) {
        return stash('<code class="bc-md-code">' + esc(code) + '</code>');
      });
      work = work.replace(/\[([^\]\n]+)\]\(((?:[^()\s]+|\([^()\s]*\))+)\)/g, function (_, rawLbl, rawHref) {
        var cleanRawLbl = isDisplayLinksOn() ? String(rawLbl || '') : String(rawLbl || '').replace(/\s*↗\s*$/, '');
        var lblHtml = expand(fmtEmph(esc(cleanRawLbl)));
        var href = String(rawHref || '').trim();
        if (/^#[A-Za-z][A-Za-z0-9_-]*$/.test(href)) {
          return stash('<a href="' + esc(href) + '" class="bc-md-link" data-bc-anchor="' + esc(href.slice(1)) + '">' + lblHtml + '</a>');
        }
        if (/^https?:\/\//i.test(href) && isDisplayLinksOn()) {
          var cleanU = safeHttpUrl(href);
          if (cleanU) {
            var arrow = /↗\s*$/.test(cleanRawLbl) ? '' : ' ↗';
            return stash('<a href="' + esc(cleanU) + '" target="_blank" rel="noopener noreferrer" class="bc-md-link">' + lblHtml + arrow + '</a>');
          }
        }
        return stash(lblHtml);
      });
      return expand(fmtEmph(esc(work)));
    }
    var lines = text.split('\n');
    var blocks = [];
    var para = [];
    var listKind = null;
    var listItems = [];
    var inFence = false;
    var fenceLines = [];
    function flushPara() {
      if (!para.length) return;
      blocks.push('<p style="margin:0 0 6px">' + para.map(formatInline).join('<br>') + '</p>');
      para = [];
    }
    function flushList() {
      if (!listKind || !listItems.length) { listKind = null; listItems = []; return; }
      blocks.push('<' + listKind + ' style="margin:5px 0 7px;padding-left:18px">' + listItems.map(function (it) {
        return '<li style="margin-bottom:3px">' + formatInline(it) + '</li>';
      }).join('') + '</' + listKind + '>');
      listKind = null;
      listItems = [];
    }
    lines.forEach(function (rawLn) {
      var t = rawLn.trim();
      if (t.indexOf('```') === 0) {
        if (inFence) {
          blocks.push('<pre class="bc-md-pre"><code>' + esc(fenceLines.join('\n')) + '</code></pre>');
          fenceLines = [];
          inFence = false;
        } else {
          flushPara();
          flushList();
          inFence = true;
        }
        return;
      }
      if (inFence) { fenceLines.push(rawLn); return; }
      if (!t) { flushPara(); flushList(); return; }
      var hMatch = t.match(/^#{1,4}\s+(.+)$/);
      if (hMatch) {
        flushPara();
        flushList();
        blocks.push('<div style="font-weight:700;margin:6px 0 4px">' + formatInline(hMatch[1]) + '</div>');
        return;
      }
      var bullet = t.match(/^(?:•|-|\*)\s+(.+)$/);
      if (bullet) {
        flushPara();
        if (listKind && listKind !== 'ul') flushList();
        listKind = 'ul';
        listItems.push(bullet[1]);
        return;
      }
      var ol = t.match(/^\d+[.)]\s+(.+)$/);
      if (ol) {
        flushPara();
        if (listKind && listKind !== 'ol') flushList();
        listKind = 'ol';
        listItems.push(ol[1]);
        return;
      }
      flushList();
      para.push(t);
    });
    if (inFence && fenceLines.length) {
      blocks.push('<pre class="bc-md-pre"><code>' + esc(fenceLines.join('\n')) + '</code></pre>');
    }
    flushPara();
    flushList();
    return blocks.join('');
  }

  function injectStyles() {
    if (document.getElementById('oe-buyer-copilot-styles')) return;
    var st = document.createElement('style');
    st.id = 'oe-buyer-copilot-styles';
    st.textContent = [
      ':root,html[data-theme="light"]{',
      '  --bc-surface:#ffffff;--bc-soft:#f8fafc;--bc-soft-2:#f9fafb;--bc-elev:#f1f5f9;',
      '  --bc-ink:#111827;--bc-ink2:#374151;--bc-muted:#475569;--bc-subtle:#6b7280;',
      '  --bc-line:#e5e7eb;--bc-line-strong:#cbd5e1;--bc-inp-bg:#ffffff;--bc-inp-bd:#d1d5db;',
      '  --bc-brand:#0f766e;--bc-brand-fg:#0f766e;--bc-brand-hover:#0d9488;--bc-brandsoft:#effcf8;--bc-brandsoft-2:#f0fdfa;--bc-brand-bd:#99f6e4;',
      '  --bc-prompt-bg:#eef2ff;--bc-prompt-fg:#3730a3;--bc-prompt-bd:#c7d2fe;',
      '  --bc-warn-bg:#fffbeb;--bc-warn-fg:#92400e;--bc-warn-bd:#fde68a;',
      '  --bc-ok-bg:#ecfdf5;--bc-ok-fg:#065f46;--bc-ok-bd:#6ee7b7;',
      '  --bc-err-bg:#fef2f2;--bc-err-fg:#991b1b;--bc-err-bd:#fecaca;',
      '  --bc-info-bg:#eff6ff;--bc-info-fg:#1e3a8a;--bc-info-bd:#bfdbfe;',
      '  --bc-team-bg:#f5f3ff;--bc-team-fg:#4c1d95;--bc-team-bd:#ddd6fe;',
      '  --bc-code-bg:rgba(15,23,42,.07);--bc-pre-bg:#f1f5f9;--bc-pre-fg:#0f172a;',
      '  --bc-shadow:0 2px 10px rgba(17,24,39,.04);--bc-modal-shadow:0 20px 50px rgba(15,23,42,.14);',
      '}',
      '@media (prefers-color-scheme: dark){',
      '  :root:not([data-theme="light"]){',
      '    --bc-surface:#111827;--bc-soft:#1e293b;--bc-soft-2:#0f172a;--bc-elev:#293548;',
      '    --bc-ink:#f1f5f9;--bc-ink2:#cbd5e1;--bc-muted:#94a3b8;--bc-subtle:#94a3b8;',
      '    --bc-line:#1e293b;--bc-line-strong:#334155;--bc-inp-bg:#1e293b;--bc-inp-bd:#334155;',
      '    --bc-brand:#0f766e;--bc-brand-fg:#5eead4;--bc-brand-hover:#14b8a6;--bc-brandsoft:rgba(20,184,166,.16);--bc-brandsoft-2:rgba(20,184,166,.11);--bc-brand-bd:rgba(45,212,191,.42);',
      '    --bc-prompt-bg:rgba(99,102,241,.18);--bc-prompt-fg:#c7d2fe;--bc-prompt-bd:rgba(129,140,248,.4);',
      '    --bc-warn-bg:rgba(217,119,6,.18);--bc-warn-fg:#fde68a;--bc-warn-bd:rgba(251,191,36,.42);',
      '    --bc-ok-bg:rgba(16,185,129,.16);--bc-ok-fg:#a7f3d0;--bc-ok-bd:rgba(52,211,153,.42);',
      '    --bc-err-bg:rgba(220,38,38,.18);--bc-err-fg:#fecaca;--bc-err-bd:rgba(248,113,113,.45);',
      '    --bc-info-bg:rgba(37,99,235,.18);--bc-info-fg:#bfdbfe;--bc-info-bd:rgba(96,165,250,.42);',
      '    --bc-team-bg:rgba(124,58,237,.18);--bc-team-fg:#ddd6fe;--bc-team-bd:rgba(167,139,250,.42);',
      '    --bc-code-bg:rgba(255,255,255,.12);--bc-pre-bg:#0b1120;--bc-pre-fg:#e2e8f0;',
      '    --bc-shadow:0 6px 20px rgba(0,0,0,.38);--bc-modal-shadow:0 24px 60px rgba(0,0,0,.62);',
      '  }',
      '}',
      'html[data-theme="dark"]{',
      '  --bc-surface:#111827;--bc-soft:#1e293b;--bc-soft-2:#0f172a;--bc-elev:#293548;',
      '  --bc-ink:#f1f5f9;--bc-ink2:#cbd5e1;--bc-muted:#94a3b8;--bc-subtle:#94a3b8;',
      '  --bc-line:#1e293b;--bc-line-strong:#334155;--bc-inp-bg:#1e293b;--bc-inp-bd:#334155;',
      '  --bc-brand:#0f766e;--bc-brand-fg:#5eead4;--bc-brand-hover:#14b8a6;--bc-brandsoft:rgba(20,184,166,.16);--bc-brandsoft-2:rgba(20,184,166,.11);--bc-brand-bd:rgba(45,212,191,.42);',
      '  --bc-prompt-bg:rgba(99,102,241,.18);--bc-prompt-fg:#c7d2fe;--bc-prompt-bd:rgba(129,140,248,.4);',
      '  --bc-warn-bg:rgba(217,119,6,.18);--bc-warn-fg:#fde68a;--bc-warn-bd:rgba(251,191,36,.42);',
      '  --bc-ok-bg:rgba(16,185,129,.16);--bc-ok-fg:#a7f3d0;--bc-ok-bd:rgba(52,211,153,.42);',
      '  --bc-err-bg:rgba(220,38,38,.18);--bc-err-fg:#fecaca;--bc-err-bd:rgba(248,113,113,.45);',
      '  --bc-info-bg:rgba(37,99,235,.18);--bc-info-fg:#bfdbfe;--bc-info-bd:rgba(96,165,250,.42);',
      '  --bc-team-bg:rgba(124,58,237,.18);--bc-team-fg:#ddd6fe;--bc-team-bd:rgba(167,139,250,.42);',
      '  --bc-code-bg:rgba(255,255,255,.12);--bc-pre-bg:#0b1120;--bc-pre-fg:#e2e8f0;',
      '  --bc-shadow:0 6px 20px rgba(0,0,0,.38);--bc-modal-shadow:0 24px 60px rgba(0,0,0,.62);',
      '}',
      '.bc-shell{font-family:Inter,system-ui,-apple-system,sans-serif;color:var(--bc-ink);max-width:1080px;margin:0 auto}',
      '.bc-disclaimer-bar{display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap;background:var(--bc-soft);border:1px solid var(--bc-line-strong);border-radius:12px;padding:10px 14px;font-size:12px;color:var(--bc-muted);margin-bottom:14px}',
      '.bc-hdr-left{display:flex;flex-direction:column;gap:4px;min-width:220px;flex:1}',
      '.bc-hdr-title-row{display:flex;align-items:center;gap:8px;flex-wrap:wrap}',
      '.bc-brand-ttl{font-size:13.5px;font-weight:800;color:var(--bc-brand-fg)}',
      '.bc-mode-badge{display:inline-flex;align-items:center;padding:2px 9px;border-radius:999px;font-size:11px;font-weight:800;background:var(--bc-brandsoft);color:var(--bc-brand-fg);border:1px solid var(--bc-brand-bd)}',
      '.bc-mode-badge.seller{background:var(--bc-warn-bg);color:var(--bc-warn-fg);border-color:var(--bc-warn-bd)}',
      '.bc-mode-badge.agent{background:var(--bc-info-bg);color:var(--bc-info-fg);border-color:var(--bc-info-bd)}',
      '.bc-prompt-badge{display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:7px;font-size:11px;font-weight:700;background:var(--bc-prompt-bg);color:var(--bc-prompt-fg);border:1px solid var(--bc-prompt-bd)}',
      '.bc-role-bar{display:inline-flex;align-items:center;gap:5px;flex-wrap:wrap}',
      '.bc-role-pill{border:1px solid var(--bc-line-strong);background:var(--bc-surface);color:var(--bc-ink2);border-radius:999px;padding:5px 11px;font:700 12px Inter,sans-serif;cursor:pointer;transition:all .15s}',
      '.bc-role-pill:hover{border-color:var(--bc-brand-fg);color:var(--bc-brand-fg);background:var(--bc-brandsoft-2)}',
      '.bc-role-pill.act{background:var(--bc-brand);border-color:var(--bc-brand);color:#fff}',
      '.bc-role-pill.act.seller{background:#b45309;border-color:#b45309;color:#fff}',
      '.bc-role-pill.act.agent{background:#1d4ed8;border-color:#1d4ed8;color:#fff}',
      '.bc-role-prompt-card{background:var(--bc-brandsoft-2);border:1.5px solid var(--bc-brand-bd);border-radius:14px;padding:14px 16px;margin-bottom:14px}',
      '.bc-role-prompt-btns{display:flex;gap:8px;flex-wrap:wrap;margin-top:10px}',
      '.bc-search-card{background:var(--bc-surface);border:1px solid var(--bc-line);border-radius:16px;padding:16px 18px;box-shadow:var(--bc-shadow);position:relative}',
      '.bc-search-form{display:flex;align-items:center;gap:8px}',
      '.bc-search-inp{flex:1;border:1.5px solid var(--bc-inp-bd);border-radius:11px;padding:11px 14px;font:500 14.5px Inter,sans-serif;color:var(--bc-ink);background:var(--bc-inp-bg);outline:none;transition:border-color .15s}',
      '.bc-search-inp:focus{border-color:var(--bc-brand-fg);box-shadow:0 0 0 3px rgba(20,184,166,.16)}',
      '.bc-btn{border:0;border-radius:10px;padding:10px 16px;font:700 13.5px Inter,sans-serif;cursor:pointer;transition:all .15s;display:inline-flex;align-items:center;gap:6px;white-space:nowrap}',
      '.bc-btn:disabled{opacity:.55;cursor:not-allowed}',
      '.bc-btn-pri{background:var(--bc-brand);color:#fff}.bc-btn-pri:hover:not(:disabled){background:var(--bc-brand-hover)}',
      '.bc-btn-sec{background:var(--bc-soft);color:var(--bc-ink);border:1px solid var(--bc-line-strong)}.bc-btn-sec:hover:not(:disabled){background:var(--bc-elev)}',
      '.bc-btn-ghost{background:var(--bc-brandsoft);color:var(--bc-brand-fg);border:1px solid var(--bc-brand-bd)}.bc-btn-ghost:hover:not(:disabled){filter:brightness(.96)}',
      '.bc-ac{position:absolute;left:18px;right:18px;top:66px;background:var(--bc-surface);border:1px solid var(--bc-line-strong);border-radius:12px;box-shadow:0 12px 28px rgba(0,0,0,.22);z-index:80;max-height:260px;overflow-y:auto;display:none}',
      '.bc-ac.on{display:block}',
      '.bc-ac-item{padding:9px 14px;font-size:13.5px;cursor:pointer;border-bottom:1px solid var(--bc-line);color:var(--bc-ink)}',
      '.bc-ac-item:last-child{border-bottom:0}.bc-ac-item:hover{background:var(--bc-brandsoft);color:var(--bc-brand-fg)}',
      '.bc-err{margin-top:10px;background:var(--bc-err-bg);border:1px solid var(--bc-err-bd);color:var(--bc-err-fg);padding:10px 14px;border-radius:10px;font-size:13px;font-weight:600;display:none}',
      '.bc-err.on{display:block}',
      '.bc-prop-card{margin-top:16px;background:var(--bc-surface);border:1px solid var(--bc-line);border-radius:16px;padding:18px;box-shadow:var(--bc-shadow)}',
      '.bc-prop-top{display:flex;justify-content:space-between;align-items:flex-start;gap:12px;flex-wrap:wrap}',
      '.bc-prop-addr{font-size:18px;font-weight:800;color:var(--bc-ink);margin:0}',
      '.bc-badge{display:inline-flex;align-items:center;gap:5px;padding:4px 10px;border-radius:999px;font-size:12px;font-weight:800;letter-spacing:.01em}',
      '.bc-badge-sale{background:var(--bc-ok-bg);color:var(--bc-ok-fg);border:1px solid var(--bc-ok-bd)}',
      '.bc-badge-off{background:var(--bc-warn-bg);color:var(--bc-warn-fg);border:1px solid var(--bc-warn-bd)}',
      '.bc-facts-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(125px,1fr));gap:10px;margin-top:14px}',
      '.bc-fact-box{background:var(--bc-soft-2);border:1px solid var(--bc-line);border-radius:10px;padding:9px 11px}',
      '.bc-fact-lbl{font-size:11px;font-weight:700;color:var(--bc-subtle);text-transform:uppercase;letter-spacing:.03em}',
      '.bc-fact-val{font-size:15px;font-weight:800;color:var(--bc-ink);margin-top:2px;font-variant-numeric:tabular-nums}',
      '.bc-notice{margin-top:14px;background:var(--bc-warn-bg);border:1px solid var(--bc-warn-bd);color:var(--bc-warn-fg);padding:12px 14px;border-radius:12px;font-size:13.5px;font-weight:600;display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap}',
      '.bc-menu-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(215px,1fr));gap:12px;margin-top:16px}',
      '.bc-fn-card{text-align:left;background:var(--bc-surface);border:1.5px solid var(--bc-line);border-radius:14px;padding:14px;cursor:pointer;transition:all .15s;display:flex;flex-direction:column;gap:5px}',
      '.bc-fn-card:hover{border-color:var(--bc-brand-fg);background:var(--bc-soft)}',
      '.bc-fn-card.act{border-color:var(--bc-brand-fg);background:var(--bc-brandsoft);box-shadow:0 3px 12px rgba(15,118,110,.16)}',
      '.bc-fn-num{display:inline-flex;align-items:center;justify-content:center;min-width:24px;height:24px;padding:0 6px;border-radius:7px;background:var(--bc-brand);color:#fff;font-size:12px;font-weight:800}',
      '.bc-fn-title{font-size:15px;font-weight:800;color:var(--bc-ink);display:flex;align-items:center;gap:8px}',
      '.bc-fn-sub{font-size:12.5px;color:var(--bc-muted);line-height:1.4}',
      '.bc-work-panel{margin-top:16px;background:var(--bc-surface);border:1px solid var(--bc-line);border-radius:16px;padding:18px;box-shadow:var(--bc-shadow)}',
      '.bc-sec-hdr{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:12px}',
      '.bc-sec-title{font-size:16.5px;font-weight:800;color:var(--bc-ink);margin:0}',
      '.bc-chat-starters{display:flex;flex-wrap:wrap;gap:7px;margin-bottom:12px}',
      '.bc-chat-chip{border:1px solid var(--bc-brand-bd);background:var(--bc-brandsoft-2);color:var(--bc-brand-fg);border-radius:999px;padding:6px 12px;font:600 12.2px Inter,sans-serif;cursor:pointer;text-align:left;transition:all .15s}',
      '.bc-chat-chip:hover{background:var(--bc-brandsoft);border-color:var(--bc-brand-fg)}',
      '.bc-chat-log{border:1px solid var(--bc-line-strong);background:var(--bc-soft-2);border-radius:13px;padding:12px;max-height:380px;overflow-y:auto;display:flex;flex-direction:column;gap:10px;margin-bottom:12px}',
      '.bc-chat-msg{padding:10px 13px;border-radius:12px;font-size:13.3px;line-height:1.55;max-width:92%;word-break:break-word}',
      '.bc-chat-msg.user{align-self:flex-end;background:var(--bc-brand);color:#fff;font-weight:600}',
      '.bc-chat-msg.assistant{align-self:flex-start;background:var(--bc-soft);border:1px solid var(--bc-line-strong);color:var(--bc-ink)}',
      '.bc-md-code{font:500 11.8px ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--bc-code-bg);padding:1px 5px;border-radius:4px}',
      '.bc-chat-msg.user .bc-md-code{background:rgba(255,255,255,.2);color:#fff}',
      '.bc-md-pre{margin:6px 0;padding:8px 10px;border-radius:8px;background:var(--bc-pre-bg);border:1px solid var(--bc-line-strong);color:var(--bc-pre-fg);font:11.6px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;overflow-x:auto;white-space:pre}',
      '.bc-md-link{color:var(--bc-brand-fg);font-weight:600;text-decoration:underline;text-underline-offset:2px}',
      '.bc-chat-msg.user .bc-md-link{color:#ecfeff}',
      '.bc-chat-cites{display:flex;flex-wrap:wrap;gap:5px;margin-top:8px}',
      '.bc-chat-cite{font-size:11px;font-weight:600;background:var(--bc-elev);color:var(--bc-ink2);border:1px solid var(--bc-line-strong);border-radius:6px;padding:2px 7px}',
      '.bc-chat-followups{display:flex;flex-wrap:wrap;gap:6px;margin-top:8px}',
      '.bc-chat-fbtn{border:1px solid var(--bc-line-strong);background:var(--bc-surface);color:var(--bc-brand-fg);border-radius:999px;padding:4px 10px;font:600 11.8px Inter,sans-serif;cursor:pointer}',
      '.bc-chat-fbtn:hover{background:var(--bc-brandsoft);border-color:var(--bc-brand-fg)}',
      '.bc-chat-form{display:flex;gap:8px;align-items:center}',
      '.bc-chat-inp{flex:1;border:1.5px solid var(--bc-inp-bd);border-radius:10px;padding:10px 13px;font:500 13.5px Inter,sans-serif;color:var(--bc-ink);background:var(--bc-inp-bg);outline:none}',
      '.bc-chat-inp:focus{border-color:var(--bc-brand-fg);box-shadow:0 0 0 3px rgba(20,184,166,.14)}',
      '.bc-table-wrap{overflow-x:auto;border:1px solid var(--bc-line);border-radius:12px;margin-top:10px}',
      '.bc-table{width:100%;border-collapse:collapse;font-size:13px;font-variant-numeric:tabular-nums}',
      '.bc-table th{background:var(--bc-soft);color:var(--bc-ink2);font-weight:700;text-align:left;padding:9px 11px;border-bottom:1px solid var(--bc-line);white-space:nowrap}',
      '.bc-table td{padding:9px 11px;border-bottom:1px solid var(--bc-line);color:var(--bc-ink);vertical-align:top}',
      '.bc-table tr:last-child td{border-bottom:0}',
      '.bc-followup-bar{display:flex;flex-wrap:wrap;gap:8px;margin-top:14px;padding-top:14px;border-top:1px dashed var(--bc-line-strong)}',
      '.bc-fchip{border:1.5px solid var(--bc-brand-fg);background:var(--bc-surface);color:var(--bc-brand-fg);border-radius:999px;padding:8px 14px;font:700 13px Inter,sans-serif;cursor:pointer;transition:all .15s}',
      '.bc-fchip:hover,.bc-fchip.act{background:var(--bc-brand);border-color:var(--bc-brand);color:#fff}',
      '.bc-subcard{margin-top:14px;background:var(--bc-soft);border:1px solid var(--bc-line-strong);border-radius:13px;padding:15px}',
      '.bc-legal-banner{display:flex;align-items:flex-start;gap:10px;background:var(--bc-ok-bg);border:1px solid var(--bc-ok-bd);color:var(--bc-ok-fg);border-radius:12px;padding:11px 14px;font-size:12.8px;line-height:1.45;margin-bottom:14px}',
      '.bc-reg-bar{background:var(--bc-soft);border:1px solid var(--bc-line-strong);border-radius:12px;padding:11px 14px;margin-bottom:14px}',
      '.bc-reg-form{display:grid;grid-template-columns:1.2fr 1fr auto;gap:8px;align-items:center;margin-top:8px}',
      '@media(max-width:720px){.bc-reg-form{grid-template-columns:1fr}}',
      '.bc-mailto-card{background:var(--bc-soft);border:1px solid var(--bc-line-strong);border-radius:14px;padding:14px 16px;margin-bottom:14px}',
      '.bc-mailto-preview{background:var(--bc-surface);border:1px solid var(--bc-line);border-radius:10px;padding:10px 12px;font-size:12.2px;color:var(--bc-ink2);margin-top:9px;white-space:pre-wrap;max-height:175px;overflow-y:auto;line-height:1.45}',
      '.bc-disc-tabs{display:flex;flex-wrap:wrap;gap:6px;margin:12px 0 14px;padding-bottom:10px;border-bottom:1px solid var(--bc-line)}',
      '.bc-disc-tab{border:1px solid var(--bc-line-strong);background:var(--bc-surface);color:var(--bc-ink2);border-radius:999px;padding:6px 13px;font:700 12.5px Inter,sans-serif;cursor:pointer;transition:all .15s}',
      '.bc-disc-tab:hover{border-color:var(--bc-brand-fg);color:var(--bc-brand-fg);background:var(--bc-brandsoft-2)}',
      '.bc-disc-tab.act{background:var(--bc-brand);border-color:var(--bc-brand);color:#fff}',
      '.bc-pub-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(235px,1fr));gap:10px;margin-top:10px}',
      '.bc-pub-card{background:var(--bc-surface);border:1px solid var(--bc-line);border-radius:12px;padding:12px 14px;display:flex;flex-direction:column;gap:5px}',
      '.bc-mode-box{border:1px solid var(--bc-line);border-radius:14px;padding:15px 16px;background:var(--bc-surface);margin-bottom:14px}',
      '.bc-share-form{background:var(--bc-brandsoft-2);border:1px solid var(--bc-brand-bd);border-radius:12px;padding:12px 14px;margin-bottom:12px}',
      '.bc-share-box{margin-top:10px;background:var(--bc-ok-bg);border:1px solid var(--bc-ok-bd);border-radius:10px;padding:10px 12px}',
      '.bc-auth-banner{display:flex;justify-content:space-between;align-items:flex-start;gap:10px;flex-wrap:wrap;background:var(--bc-info-bg);border:1px solid var(--bc-info-bd);color:var(--bc-info-fg);border-radius:12px;padding:12px 14px;margin-bottom:10px}',
      '.bc-dealteam-banner{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;background:var(--bc-team-bg);border:1px solid var(--bc-team-bd);color:var(--bc-team-fg);border-radius:12px;padding:11px 14px;margin-bottom:10px;font-size:13px;font-weight:700}',
      '.bc-privity-box{background:var(--bc-warn-bg);border:1px solid var(--bc-warn-bd);color:var(--bc-warn-fg);border-radius:10px;padding:9px 12px;font-size:12px;line-height:1.45;margin-bottom:10px}',
      '.bc-dropzone{border:2px dashed var(--bc-brand-bd);background:var(--bc-brandsoft-2);border-radius:14px;padding:24px 18px;text-align:center;cursor:pointer;transition:all .15s}',
      '.bc-dropzone.drag{border-color:var(--bc-brand-fg);background:var(--bc-brandsoft)}',
      '.bc-prio-list{display:flex;flex-direction:column;gap:10px;margin-top:14px}',
      '.bc-prio-card{border:1px solid var(--bc-line);border-radius:12px;padding:13px 15px;background:var(--bc-surface)}',
      '.bc-prio-card.missing{background:var(--bc-soft-2);border-style:dashed;opacity:.88}',
      '.bc-redflags{margin-top:14px;background:var(--bc-err-bg);border:1px solid var(--bc-err-bd);border-radius:13px;padding:14px 16px}',
      '.bc-cite{display:inline-block;font-size:11px;font-weight:700;background:var(--bc-elev);color:var(--bc-ink2);border:1px solid var(--bc-line-strong);border-radius:6px;padding:1px 6px;margin-left:5px}',
      '.bc-pdf-cite-btn{padding:2px 8px;font-size:11.5px;border-radius:7px;margin-left:6px;vertical-align:middle}',
      '.bc-offer-grid{display:grid;grid-template-columns:1.15fr .95fr;gap:18px;align-items:start}',
      '@media(max-width:860px){.bc-offer-grid{grid-template-columns:1fr}}',
      '.bc-steps{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:14px}',
      '.bc-step-pill{border:1px solid var(--bc-line-strong);background:var(--bc-soft);color:var(--bc-ink2);border-radius:999px;padding:5px 11px;font:700 12px Inter,sans-serif;cursor:pointer}',
      '.bc-step-pill.act{background:var(--bc-brand);color:#fff;border-color:var(--bc-brand)}',
      '.bc-step-pill.done{border-color:var(--bc-ok-bd);background:var(--bc-ok-bg);color:var(--bc-ok-fg)}',
      '.bc-field{margin-bottom:12px}',
      '.bc-field label{display:block;font-size:12.5px;font-weight:700;color:var(--bc-ink2);margin-bottom:4px}',
      '.bc-inp,.bc-sel,.bc-ta{width:100%;box-sizing:border-box;border:1px solid var(--bc-inp-bd);border-radius:9px;padding:8px 11px;font:500 13.5px Inter,sans-serif;color:var(--bc-ink);background:var(--bc-inp-bg)}',
      '.bc-ta{min-height:68px;resize:vertical}',
      '.bc-row2{display:grid;grid-template-columns:1fr 1fr;gap:10px}',
      '.bc-summary-box{background:var(--bc-soft);border:1px solid var(--bc-line-strong);border-radius:14px;padding:15px;position:sticky;top:14px}',
      '.bc-sum-row{display:flex;justify-content:space-between;align-items:flex-start;gap:8px;padding:7px 0;border-bottom:1px solid var(--bc-line);font-size:12.8px}',
      '.bc-sum-row:last-child{border-bottom:0}',
      '.bc-edit-link{border:0;background:none;color:var(--bc-brand-fg);font:700 11.5px Inter,sans-serif;cursor:pointer;padding:0;margin-left:6px;text-decoration:underline}',
      /* Frosted light wash modal in light mode, cohesive dark frosted wash in dark mode */
      '.bc-modal-backdrop{position:fixed;inset:0;background:rgba(248,250,252,.82);backdrop-filter:blur(8px);-webkit-backdrop-filter:blur(8px);z-index:9990;display:none;align-items:flex-start;justify-content:center;overflow-y:auto;padding:28px 16px}',
      '@media (prefers-color-scheme: dark){html:not([data-theme="light"]) .bc-modal-backdrop{background:rgba(15,23,42,.78)}}',
      'html[data-theme="dark"] .bc-modal-backdrop{background:rgba(15,23,42,.78)}',
      '.bc-modal-backdrop.on{display:flex}',
      '.bc-modal-dialog{background:var(--bc-surface);color:var(--bc-ink);border:1px solid var(--bc-line-strong);border-radius:20px;box-shadow:var(--bc-modal-shadow);width:100%;max-width:1100px;padding:20px 22px;position:relative}'
    ].join('\n');
    document.head.appendChild(st);
  }

  function readDealTeamToken() {
    try {
      var sp = new URLSearchParams(window.location.search || '');
      return (sp.get('dealTeamToken') || sp.get('deal_team_token') || '').trim();
    } catch (_) {
      return '';
    }
  }

  function safeHttpUrl(raw) {
    var s = String(raw || '').trim();
    return /^https?:\/\/[^\s"'<>]+$/i.test(s) ? s : '';
  }

  function unwrapDisclosureAnalysis(node) {
    if (!node || typeof node !== 'object') return null;
    if (node.priorityReading) return node;
    if (node.analysis && typeof node.analysis === 'object') {
      if (node.analysis.priorityReading) return node.analysis;
      if (node.analysis.data && node.analysis.data.priorityReading) return node.analysis.data;
      return node.analysis;
    }
    if (node.data && typeof node.data === 'object') {
      if (node.data.priorityReading) return node.data;
      return node.data;
    }
    return node;
  }

  function buildMailtoHref(recipient, subject, body) {
    var cleanRecip = String(recipient || '').replace(/[\r\n?&<>]/g, '').trim();
    return 'mailto:' + cleanRecip + '?subject=' + encodeURIComponent(subject || '') + '&body=' + encodeURIComponent(body || '');
  }

  function fileToBase64(file) {
    return new Promise(function (resolve, reject) {
      var reader = new FileReader();
      reader.onload = function () {
        var res = String(reader.result || '');
        var comma = res.indexOf(',');
        resolve(comma >= 0 ? res.slice(comma + 1) : res);
      };
      reader.onerror = function () { reject(new Error('Failed to read file ' + file.name)); };
      reader.readAsDataURL(file);
    });
  }

  function buildModalTitleText(role) {
    var ttl = assistantBrandTitle();
    var base = L(
      ttl + ' — Chat, Comps, Disclosures & Offer Builder',
      ttl + ' — 问答 · 可比估价 · 披露精读 · 出价单',
      ttl + ' — Chat, Comparables, Divulgaciones y Oferta'
    );
    var badge = roleModeBadgeText(role);
    return '✦ ' + (badge ? (base + ' · ' + badge) : base);
  }

  function syncModalChrome(role) {
    var modalTitleEl = document.getElementById('bcModalTitle');
    if (modalTitleEl) {
      modalTitleEl.textContent = buildModalTitleText(role);
    }
    var closeBtnEl = document.getElementById('bcCloseModalBtn');
    if (closeBtnEl) {
      closeBtnEl.textContent = '✕ ' + L('Close', '关闭', 'Cerrar');
    }
  }

  function resolveRequestedFn(opts) {
    opts = opts || {};
    if (opts.autoOpenDisclosures || readDealTeamToken()) return 'disclosures';
    var raw = opts.initialFn || opts.activeFn || opts.fn || opts.tab || '';
    var lc = String(raw).trim().toLowerCase();
    if (lc === 'comps' || lc === 'comps_pricing') return 'comps_pricing';
    if (lc === 'disclosures' || lc === 'disc') return 'disclosures';
    if (lc === 'offer' || lc === 'offer_builder') return 'offer_builder';
    if (lc === 'chat' || lc === 'qa' || lc === 'ask') return 'chat';
    return 'chat';
  }

  function createCopilotInstance(mountEl, opts) {
    if (!mountEl) return null;
    injectStyles();
    opts = opts || {};

    var state = {
      role: readInitialRole(), // 'buyer' | 'seller' | 'agent' | 'unset'
      regUser: getStoredUser(),
      loading: false,
      sharingEnabled: false,
      error: null,
      property: null,
      hasFullReport: false,
      generatingReport: false,
      reportBuildStatus: '',
      allowOffMarketPreview: false,
      activeFn: 'chat', // 'chat' | 'comps_pricing' | 'disclosures' | 'offer_builder'
      chatHistory: [],
      chatBySlug: {},
      chatSending: false,
      compSet: null,
      followUps: {},
      activeFollowUp: null,
      disclosures: null,
      disclosuresState: null,
      loadingDisclosures: false,
      uploadingDisclosures: false,
      uploadingAuthorized: false,
      registeringUser: false,
      verifyingCode: false,
      pendingVerification: null,
      regCodeDraft: '',
      regEmailDraft: '',
      regNameDraft: '',
      sharingDealTeam: false,
      shareRecipientDraft: '',
      shareAttestedDraft: false,
      shareResult: null,
      pdfLocalPrompt: null,
      pdfLocalFileCount: 0,
      discSubTab: 'all', // 'all' | 'public' | 'private' | 'authorized'
      discNotice: null,
      authViewingReading: false,
      mailtoRecipient: '',
      authFormDraft: {
        uploaderRole: 'listing_agent',
        signerName: '',
        signerEmail: '',
        licenseNumber: '',
        authorizedToDistribute: false
      },
      offerEval: null,
      offerStep: 1
    };

    mountEl.innerHTML = [
      '<div class="bc-shell">',
      '  <div class="bc-disclaimer-bar" id="bcHeaderRoleBar"></div>',
      '  <div class="bc-search-card" id="bcSearchCard">',
      '    <form class="bc-search-form" id="bcSearchForm" novalidate>',
      '      <input type="text" class="bc-search-inp" id="bcAddrInput" autocomplete="off" placeholder="' + esc(L(
        'Enter a US property street address (e.g., 123 Synth Main St, San Jose, CA 95112)…',
        '输入美国房产街道地址（如 123 Synth Main St, San Jose, CA 95112）…',
        'Ingrese una dirección en EE. UU. (ej. 123 Synth Main St, San Jose, CA)…'
      )) + '">',
      '      <button type="submit" class="bc-btn bc-btn-pri" id="bcLookupBtn">' + esc(L('Check Property', '查询房源', 'Consultar')) + '</button>',
      '    </form>',
      '    <div class="bc-ac" id="bcSuggestBox" role="listbox"></div>',
      '    <div class="bc-err" id="bcErrBanner" role="alert"></div>',
      '  </div>',
      '  <div id="bcDynamicBody"></div>',
      '</div>'
    ].join('');

    var hdrBarEl = mountEl.querySelector('#bcHeaderRoleBar');
    var formEl = mountEl.querySelector('#bcSearchForm');
    var inpEl = mountEl.querySelector('#bcAddrInput');
    var btnEl = mountEl.querySelector('#bcLookupBtn');
    var acEl = mountEl.querySelector('#bcSuggestBox');
    var errEl = mountEl.querySelector('#bcErrBanner');
    var bodyEl = mountEl.querySelector('#bcDynamicBody');
    var renderedPdfCitations = [];

    function syncPdfStudioAnalysis(analysisObj) {
      try {
        if (analysisObj && window.OpenEstatePdfStudio && typeof window.OpenEstatePdfStudio.setAnalysis === 'function') {
          window.OpenEstatePdfStudio.setAnalysis(analysisObj, {
            slug: state.property && state.property.slug,
            address: (state.property && (state.property.matchedAddress || state.property.displayAddress)) || '',
            role: state.role !== 'unset' ? state.role : undefined
          });
        }
      } catch (_) {}
    }

    function hasPdfStudioLocalFiles() {
      try {
        if (!window.OpenEstatePdfStudio || typeof window.OpenEstatePdfStudio.hasLocalFiles !== 'function') return false;
        return !!window.OpenEstatePdfStudio.hasLocalFiles(state.property && state.property.slug);
      } catch (_) {
        return false;
      }
    }

    function launchPdfStudio(analysisObj, citationObj) {
      var slug = state.property && state.property.slug;
      var addr = (state.property && (state.property.matchedAddress || state.property.displayAddress)) || '';
      var hasLocal = hasPdfStudioLocalFiles();
      if (!hasLocal) {
        state.pdfLocalPrompt = {
          analysis: analysisObj || null,
          citation: citationObj || null
        };
      } else {
        state.pdfLocalPrompt = null;
      }
      if (analysisObj) syncPdfStudioAnalysis(analysisObj);
      try {
        if (window.OpenEstatePdfStudio && typeof window.OpenEstatePdfStudio.openStudio === 'function') {
          var openOpts = {
            slug: slug,
            address: addr,
            role: state.role !== 'unset' ? state.role : undefined,
            analysis: analysisObj || undefined
          };
          if (citationObj) openOpts.citation = citationObj;
          window.OpenEstatePdfStudio.openStudio(openOpts);
        } else {
          showError(L(
            'In-browser PDF Studio is still loading. You can still select your local PDF/.zip file below once ready.',
            '浏览器端 PDF 批注工作台正在加载，请稍候片刻重试。',
            'El Estudio PDF aún se está cargando.'
          ));
        }
      } catch (err) {
        showError(err && err.message ? err.message : 'Could not open PDF Studio.');
      }
      if (!hasLocal) renderBody();
    }

    function renderRolePromptCardHtml() {
      return [
        '<div class="bc-role-prompt-card" id="bcRolePromptCard">',
        '  <div style="font-weight:800;font-size:14px;color:var(--bc-brand-fg)">' + esc(L(
          'Welcome! Are you a Buyer, Seller, or Agent? Select your role to load the tailored advisor prompt & tools:',
          '欢迎！请问您是买家、卖家还是经纪人？请选择您的身份以加载专属顾问提示词与工具：',
          '¡Bienvenido! ¿Es Comprador, Vendedor o Agente? Seleccione su rol para cargar las herramientas y el asesor adecuado:'
        )) + '</div>',
        '  <div class="bc-role-prompt-btns">',
        '    <button type="button" class="bc-btn bc-btn-sec" data-bc-set-role="buyer">🏠 ' + esc(L('Buyer', '我是买家', 'Comprador')) + '</button>',
        '    <button type="button" class="bc-btn bc-btn-sec" data-bc-set-role="seller">🏷️ ' + esc(L('Seller', '我是卖家', 'Vendedor')) + '</button>',
        '    <button type="button" class="bc-btn bc-btn-sec" data-bc-set-role="agent">💼 ' + esc(L('Agent', '我是经纪人', 'Agente')) + '</button>',
        '  </div>',
        '</div>'
      ].join('');
    }

    function renderHeaderBar() {
      if (!hdrBarEl) return;
      var badge = roleModeBadgeText(state.role);
      var badgeCls = 'bc-mode-badge' + (state.role === 'seller' ? ' seller' : (state.role === 'agent' ? ' agent' : ''));
      var html = [
        '<div class="bc-hdr-left">',
        '  <div class="bc-hdr-title-row">',
        '    <span class="bc-brand-ttl">✦ ' + esc(assistantBrandTitle()) + '</span>',
        badge ? ('    <span class="' + badgeCls + '" id="bcRoleModeBadge">' + esc(badge) + '</span>') : '',
        '    <span class="bc-prompt-badge" id="bcSystemPromptBadge">' + esc(roleSystemPromptLabel(state.role)) + '</span>',
        '  </div>',
        '  <div>' + esc(L(
          'Grounded in licensed/public property records & county fee schedules. Outputs are informational — not legal or appraisal advice.',
          '基于真实公开产权与县市税费表计算，仅供参考，不构成法律或持牌估价意见。',
          'Basado en registros oficiales; informativo, no asesoría legal ni tasación.'
        )) + '</div>',
        '</div>',
        '<div class="bc-role-bar" id="bcRolePillsBar" role="group" aria-label="' + esc(L('Select role', '选择身份', 'Seleccionar rol')) + '">'
      ];
      ['buyer', 'seller', 'agent'].forEach(function (r) {
        var act = state.role === r ? (' act ' + r) : '';
        html.push(
          '<button type="button" class="bc-role-pill' + act + '" data-bc-set-role="' + r + '" aria-pressed="' + (state.role === r ? 'true' : 'false') + '">' +
            esc(roleLabelText(r)) +
          '</button>'
        );
      });
      html.push('</div>');
      hdrBarEl.innerHTML = html.join('');
      hdrBarEl.querySelectorAll('[data-bc-set-role]').forEach(function (b) {
        b.addEventListener('click', function () {
          setAssistantRole(b.getAttribute('data-bc-set-role'));
        });
      });
      syncModalChrome(state.role);
    }

    function setAssistantRole(rawRole) {
      var next = normalizeAssistantRole(rawRole);
      state.role = next;
      try {
        localStorage.setItem(ROLE_STORAGE_KEY, next === 'unset' ? 'all' : next);
      } catch (_) {}
      try {
        if (window.OpenEstateRole && typeof window.OpenEstateRole.setRole === 'function') {
          var targetExt = next === 'unset' ? 'all' : next;
          var curExt = typeof window.OpenEstateRole.getRole === 'function' ? window.OpenEstateRole.getRole() : null;
          if (curExt !== targetExt) {
            window.OpenEstateRole.setRole(targetExt);
          }
        }
      } catch (_) {}
      renderHeaderBar();
      renderBody();
    }

    window.addEventListener('openestate:rolechange', function (ev) {
      var incoming = ev && ev.detail && ev.detail.role != null ? ev.detail.role : readRawRoleFromEnv();
      var next = normalizeAssistantRole(incoming);
      if (next !== state.role) {
        state.role = next;
        renderHeaderBar();
        renderBody();
      } else {
        renderHeaderBar();
      }
    });

    renderHeaderBar();

    var suggestTimer = null;
    inpEl.addEventListener('input', function () {
      errEl.classList.remove('on');
      var q = inpEl.value.trim();
      clearTimeout(suggestTimer);
      if (q.length < 3) {
        acEl.classList.remove('on');
        acEl.innerHTML = '';
        return;
      }
      suggestTimer = setTimeout(async function () {
        try {
          var r = await fetch('/api/suggest?q=' + encodeURIComponent(q));
          if (!r.ok) return;
          var j = await r.json();
          var list = Array.isArray(j.suggestions) ? j.suggestions : (Array.isArray(j) ? j : []);
          if (!list.length) {
            acEl.classList.remove('on');
            return;
          }
          acEl.innerHTML = list.slice(0, 6).map(function (item) {
            var label = typeof item === 'string' ? item : (item.label || item.address || '');
            return '<div class="bc-ac-item" data-addr="' + esc(label) + '">' + esc(label) + '</div>';
          }).join('');
          acEl.classList.add('on');
          acEl.querySelectorAll('.bc-ac-item').forEach(function (row) {
            row.addEventListener('mousedown', function (ev) {
              ev.preventDefault();
              var picked = row.getAttribute('data-addr') || '';
              inpEl.value = picked;
              acEl.classList.remove('on');
              lookupProperty({ address: picked });
            });
          });
        } catch (_) {}
      }, 200);
    });

    document.addEventListener('mousedown', function (ev) {
      if (!mountEl.contains(ev.target)) acEl.classList.remove('on');
    });

    formEl.addEventListener('submit', function (ev) {
      ev.preventDefault();
      acEl.classList.remove('on');
      var addr = inpEl.value.trim();
      if (!addr) {
        showError(L('Please enter a US street address.', '请输入美国街道地址。', 'Ingrese una dirección válida.'));
        return;
      }
      lookupProperty({ address: addr });
    });

    function showError(msg) {
      if (!msg) {
        errEl.classList.remove('on');
        errEl.textContent = '';
        return;
      }
      errEl.textContent = msg;
      errEl.classList.add('on');
    }

    async function apiPost(url, payload) {
      var res = await fetch(url, {
        method: 'POST',
        credentials: 'same-origin',
        headers: copilotHeaders({ 'Content-Type': 'application/json' }),
        body: JSON.stringify(payload)
      });
      var data = await res.json().catch(function () { return {}; });
      if (res.status === 401 || data.code === 'AUTH_REQUIRED') {
        setStoredUser(null);
        state.regUser = null;
      }
      if (!res.ok || data.ok === false) {
        var err = new Error(data.error || ('Request failed (' + res.status + ')'));
        err.status = res.status;
        err.code = data.code || '';
        err.payload = data;
        throw err;
      }
      return data;
    }

    function isSharingEnabled() {
      return window.__CLOUD_SHARING_ENABLED__ === true || state.sharingEnabled === true;
    }

    async function syncSessionWithServer() {
      if (!isSharingEnabled()) return;
      try {
        var r = await fetch('/api/copilot/auth/me', {
          credentials: 'same-origin',
          headers: copilotHeaders(),
          cache: 'no-store'
        });
        var j = await r.json().catch(function () { return {}; });
        if (r.ok && j && j.authenticated && j.user && j.user.userId) {
          var existing = getStoredUser();
          state.regUser = {
            userId: j.user.userId,
            email: j.user.email || '',
            displayName: j.user.displayName || '',
            sessionToken: (existing && existing.sessionToken) || '',
            emailVerified: true
          };
        } else if (getStoredUser()) {
          setStoredUser(null);
          state.regUser = null;
        }
      } catch (_) {}
    }

    async function loadDisclosures(forceReload) {
      if (!state.property) return;
      if (!forceReload && state.disclosuresState && state.disclosuresState.slug === state.property.slug && !state.loadingDisclosures) {
        renderBody();
        return;
      }
      state.loadingDisclosures = true;
      renderBody();
      try {
        var tok = isSharingEnabled() ? readDealTeamToken() : '';
        var url = '/api/copilot/disclosures?slug=' + encodeURIComponent(state.property.slug);
        if (tok) url += '&dealTeamToken=' + encodeURIComponent(tok);
        var r = await fetch(url, {
          credentials: 'same-origin',
          headers: copilotHeaders(),
          cache: 'no-store'
        });
        if (r.status === 401) {
          setStoredUser(null);
          state.regUser = null;
          r = await fetch(url, {
            credentials: 'same-origin',
            headers: copilotHeaders(),
            cache: 'no-store'
          });
        }
        var j = await r.json().catch(function () { return {}; });
        if (r.ok && j && j.ok !== false) {
          if (typeof j.sharingEnabled === 'boolean') state.sharingEnabled = j.sharingEnabled;
          state.disclosuresState = j;
          var rawPriv = ('privateUserAnalysis' in j) ? j.privateUserAnalysis : j.data;
          state.disclosures = unwrapDisclosureAnalysis(rawPriv) || null;
          var studioSync = state.disclosures || unwrapDisclosureAnalysis(j.authorizedDisclosures) || unwrapDisclosureAnalysis(j.sharedWithMeAnalysis || j.sharedDealTeamAnalysis);
          if (studioSync) syncPdfStudioAnalysis(studioSync);
          if (!state.mailtoRecipient && j.mailtoRequest && j.mailtoRequest.recipientEmail) {
            state.mailtoRecipient = j.mailtoRequest.recipientEmail;
          }
        }
      } catch (err) {
        showError(err.message);
      } finally {
        state.loadingDisclosures = false;
        renderBody();
      }
    }

    async function registerAccount(email, displayName) {
      var cleanEmail = String(email || '').trim();
      var cleanName = String(displayName || '').trim();
      if (!cleanEmail || cleanEmail.indexOf('@') < 1) {
        showError(L('Please enter a valid email address.', '请输入有效的邮箱地址。', 'Ingrese un correo electrónico válido.'));
        return;
      }
      showError(null);
      state.registeringUser = true;
      state.regEmailDraft = cleanEmail;
      state.regNameDraft = cleanName;
      renderBody();
      try {
        var payload = {
          email: cleanEmail,
          role: state.role !== 'unset' ? state.role : 'buyer'
        };
        if (cleanName) payload.displayName = cleanName;
        var res = await apiPost('/api/copilot/auth/register', payload);
        var user = (res && res.user) ? res.user : res;
        if (!user || !user.userId) {
          throw new Error(L('Account registration did not return a user ID.', '注册响应缺少用户标识。', 'Falta el identificador de usuario.'));
        }
        var devCode = (res && res.smtpDisabled && res.devOrLocalVerificationCode)
          ? String(res.devOrLocalVerificationCode).trim()
          : '';
        state.pendingVerification = {
          userId: user.userId,
          email: user.email || cleanEmail,
          displayName: user.displayName || cleanName,
          smtpDisabled: !!(res && res.smtpDisabled),
          devOrLocalVerificationCode: devCode
        };
        state.regCodeDraft = devCode;
        state.discNotice = devCode
          ? L(
              'Verification step required: Local/offline verification code ' + devCode + ' is ready below — click Verify & Sign In to activate your signed session.',
              '需完成邮箱验证码校验：本地离线验证码 ' + devCode + ' 已自动填入下方，点击“验证并登录”即可启用签名会话。',
              'Verificación requerida: el código local ' + devCode + ' está listo abajo.'
            )
          : L(
              'Enter the 6-digit verification code sent to ' + (user.email || cleanEmail) + ' to activate your signed session.',
              '请输入发送至 ' + (user.email || cleanEmail) + ' 的 6 位验证码以启用签名会话。',
              'Ingrese el código de 6 dígitos enviado a ' + (user.email || cleanEmail) + '.'
            );
      } catch (err) {
        showError(err.message);
      } finally {
        state.registeringUser = false;
        renderBody();
      }
    }

    async function verifyAccountCode(email, code) {
      var cleanEmail = String(email || (state.pendingVerification && state.pendingVerification.email) || state.regEmailDraft || '').trim();
      var cleanCode = String(code || '').trim();
      if (!/^\d{6}$/.test(cleanCode)) {
        showError(L('Please enter the 6-digit numeric verification code.', '请输入 6 位数字验证码。', 'Ingrese el código numérico de 6 dígitos.'));
        return;
      }
      showError(null);
      state.verifyingCode = true;
      state.regCodeDraft = cleanCode;
      renderBody();
      try {
        var res = await apiPost('/api/copilot/auth/verify', {
          email: cleanEmail,
          code: cleanCode
        });
        var user = (res && res.user) ? res.user : null;
        var sessionToken = (res && res.sessionToken) ? String(res.sessionToken).trim() : '';
        if (!user || !user.userId || !sessionToken) {
          throw new Error(L('Verification did not return a signed session token.', '验证响应缺少签名会话令牌。', 'Falta el token de sesión firmado.'));
        }
        setStoredUser({
          userId: user.userId,
          email: user.email || cleanEmail,
          displayName: user.displayName || state.regNameDraft || '',
          sessionToken: sessionToken
        });
        state.regUser = getStoredUser();
        state.pendingVerification = null;
        state.regCodeDraft = '';
        state.shareResult = null;
        state.offerEval = null;
        state.discNotice = L(
          '✓ Verified & signed in as ' + (state.regUser.email || cleanEmail) + ' (HMAC-signed session active).',
          '✓ 已完成验证并登录：' + (state.regUser.email || cleanEmail) + '（HMAC 签名会话已生效）。',
          '✓ Verificado y conectado como ' + (state.regUser.email || cleanEmail) + '.'
        );
        await loadDisclosures(true);
      } catch (err) {
        showError(err.message);
      } finally {
        state.verifyingCode = false;
        renderBody();
      }
    }

    async function signOutAccount() {
      showError(null);
      try {
        await fetch('/api/copilot/auth/logout', {
          method: 'POST',
          credentials: 'same-origin',
          headers: copilotHeaders({ 'Content-Type': 'application/json' }),
          body: '{}'
        });
      } catch (_) {}
      setStoredUser(null);
      state.regUser = null;
      state.pendingVerification = null;
      state.regCodeDraft = '';
      state.shareResult = null;
      state.offerEval = null;
      state.discNotice = L(
        'Signed out and revoked active session.',
        '已退出并注销当前签名会话。',
        'Sesión cerrada y revocada.'
      );
      await loadDisclosures(true);
    }

    async function lookupProperty(reqObj, lookupOpts) {
      lookupOpts = lookupOpts || {};
      showError(null);
      btnEl.disabled = true;
      btnEl.textContent = L('Checking…', '核验中…', 'Verificando…');
      try {
        var lookupPayload = {};
        if (reqObj && reqObj.slug) lookupPayload.slug = reqObj.slug;
        if (reqObj && reqObj.address) lookupPayload.address = reqObj.address;
        if (state.role && state.role !== 'unset') lookupPayload.role = state.role;

        var data = await apiPost('/api/copilot/lookup', lookupPayload);
        var hasToken = isSharingEnabled() && !!readDealTeamToken();
        var targetFn = resolveRequestedFn(lookupOpts);
        if (typeof data.sharingEnabled === 'boolean') state.sharingEnabled = data.sharingEnabled;
        state.property = data.property;
        state.hasFullReport = !!data.hasFullReport;
        state.generatingReport = false;
        state.reportBuildStatus = '';
        state.allowOffMarketPreview = hasToken || targetFn === 'disclosures';
        state.activeFn = targetFn;
        var slugKey = (data.property && data.property.slug) || '';
        state.chatHistory = (slugKey && state.chatBySlug[slugKey]) ? state.chatBySlug[slugKey] : [];
        state.chatSending = false;
        state.activeFollowUp = null;
        state.compSet = null;
        state.followUps = {};
        state.disclosures = null;
        state.disclosuresState = null;
        state.shareResult = null;
        state.shareAttestedDraft = false;
        state.pdfLocalPrompt = null;
        state.pdfLocalFileCount = 0;
        state.discNotice = null;
        state.discSubTab = 'all';
        state.authViewingReading = false;
        state.mailtoRecipient = (data.property && data.property.materialRequestMailto && data.property.materialRequestMailto.recipientEmail) || '';
        state.offerEval = null;
        state.offerStep = 1;

        if (data.property && data.property.matchedAddress) {
          inpEl.value = data.property.matchedAddress;
        }
        if (data.savedState) {
          if (data.savedState.comps) {
            state.compSet = data.savedState.comps.compSet || data.savedState.comps;
            state.followUps = data.savedState.comps.followUps || {};
          }
          if (data.savedState.disclosures) {
            state.disclosures = unwrapDisclosureAnalysis(data.savedState.disclosures);
          }
          if (data.savedState.offer) {
            state.offerEval = data.savedState.offer;
            if (state.offerEval.draft && state.offerEval.draft.currentStep) {
              state.offerStep = state.offerEval.draft.currentStep;
            }
          }
        }
        renderBody();
        if (targetFn === 'disclosures') {
          await loadDisclosures(true);
        } else if (targetFn === 'comps_pricing' && !state.compSet) {
          await loadComps('strict');
        } else if (targetFn === 'offer_builder' && !state.offerEval) {
          await loadOfferDraft();
        }
      } catch (err) {
        state.property = null;
        renderBody();
        showError(err.message || 'Invalid or unresolvable property address.');
      } finally {
        btnEl.disabled = false;
        btnEl.textContent = L('Check Property', '查询房源', 'Consultar');
      }
    }

    async function generateFullReportInPlace() {
      if (!state.property || state.generatingReport) return;
      showError(null);
      state.generatingReport = true;
      state.reportBuildStatus = L('Starting full property report build…', '正在启动完整尽调报告生成…', 'Iniciando generación del informe…');
      renderBody();
      try {
        var addr = state.property.matchedAddress || state.property.slug;
        var res = await fetch('/api/report/enqueue', {
          method: 'POST',
          headers: copilotHeaders({ 'Content-Type': 'application/json' }),
          body: JSON.stringify({ address: addr })
        });
        var j = await res.json().catch(function () { return {}; });
        if (!res.ok || j.ok === false) {
          throw new Error(j.error || ('Report enqueue failed (' + res.status + ')'));
        }
        if (j.cached || j.status === 'cached') {
          state.hasFullReport = true;
          if (j.slug && state.property) state.property.slug = j.slug;
          state.generatingReport = false;
          state.reportBuildStatus = '';
          renderBody();
          return;
        }
        if (j.jobId) {
          for (var attempt = 0; attempt < 90; attempt++) {
            await new Promise(function (resolve) { setTimeout(resolve, 1200); });
            var jr = await fetch('/api/job/' + encodeURIComponent(j.jobId), { cache: 'no-store' });
            var snap = await jr.json().catch(function () { return {}; });
            if (snap.error) throw new Error(snap.error);
            if (snap.step) {
              state.reportBuildStatus = String(snap.step);
              renderBody();
            }
            if (snap.ready || snap.done) {
              state.hasFullReport = true;
              if (snap.slug && state.property) state.property.slug = snap.slug;
              state.generatingReport = false;
              state.reportBuildStatus = '';
              renderBody();
              return;
            }
          }
          throw new Error(L('Report generation is still running; open the full report page to track progress.', '报告仍在后台生成中，可点击查看完整报告页面跟踪进度。', 'La generación continúa; abra la página del informe.'));
        }
      } catch (err) {
        state.generatingReport = false;
        state.reportBuildStatus = '';
        showError(err.message);
        renderBody();
      }
    }

    async function sendChatQuestion(questionText) {
      if (!state.property || state.chatSending) return;
      var q = String(questionText || '').trim();
      if (!q) return;
      showError(null);
      var slugKey = state.property.slug;
      var histPayload = state.chatHistory
        .filter(function (m) { return !m.refused && (m.role === 'user' || m.role === 'assistant'); })
        .map(function (m) { return { role: m.role, content: m.content }; });

      state.chatHistory.push({ role: 'user', content: q });
      state.chatBySlug[slugKey] = state.chatHistory;
      state.chatSending = true;
      renderBody();

      try {
        var reqBody = {
          slug: slugKey,
          question: q,
          history: histPayload
        };
        if (state.role && state.role !== 'unset') {
          reqBody.role = state.role;
        }
        var res = await fetch('/api/ask', {
          method: 'POST',
          headers: copilotHeaders({ 'Content-Type': 'application/json' }),
          body: JSON.stringify(reqBody)
        });
        var j = await res.json().catch(function () { return {}; });
        if (res.status === 404 || j.code === 'NOT_FOUND') {
          state.hasFullReport = false;
          state.chatHistory.pop();
          state.chatSending = false;
          renderBody();
          return;
        }
        if (!res.ok || j.ok === false) {
          throw new Error(j.error || ('Request failed (' + res.status + ')'));
        }
        state.chatHistory.push({
          role: 'assistant',
          content: j.answer || '',
          citations: Array.isArray(j.citations) ? j.citations : [],
          followUps: Array.isArray(j.followUps) ? j.followUps : [],
          refused: !!j.refused,
          needsRoleSelection: !!j.needsRoleSelection,
          roleSelectionPrompt: j.roleSelectionPrompt || '',
          roleSystemPromptId: j.roleSystemPromptId || ''
        });
        state.chatBySlug[slugKey] = state.chatHistory;
      } catch (err) {
        state.chatHistory.push({
          role: 'assistant',
          content: L('Error: ', '请求出错：', 'Error: ') + err.message,
          citations: [],
          followUps: [],
          refused: true
        });
      } finally {
        state.chatSending = false;
        renderBody();
      }
    }

    async function loadComps(mode) {
      if (!state.property) return;
      state.loading = true;
      renderBody();
      try {
        var res = await apiPost('/api/copilot/comps', {
          slug: state.property.slug,
          mode: mode || 'strict'
        });
        state.compSet = res.compSet;
        state.followUps = {};
        state.activeFollowUp = null;
      } catch (err) {
        showError(err.message);
      } finally {
        state.loading = false;
        renderBody();
      }
    }

    async function loadFollowUp(followUpKey) {
      if (!state.property || !state.compSet) return;
      state.activeFollowUp = followUpKey;
      if (state.followUps[followUpKey]) {
        renderBody();
        return;
      }
      state.loading = true;
      renderBody();
      try {
        var res = await apiPost('/api/copilot/comps', {
          slug: state.property.slug,
          compSetId: state.compSet.compSetId,
          followUp: followUpKey
        });
        if (res.compSet) state.compSet = res.compSet;
        if (res.followUpResult) state.followUps[followUpKey] = res.followUpResult;
      } catch (err) {
        showError(err.message);
      } finally {
        state.loading = false;
        renderBody();
      }
    }

    async function uploadDisclosureFiles(fileList) {
      if (!state.property || !fileList || !fileList.length) return;
      showError(null);
      state.discNotice = null;
      state.pdfLocalPrompt = null;
      state.uploadingDisclosures = true;
      renderBody();
      try {
        var ingestPromise = null;
        try {
          if (window.OpenEstatePdfStudio && typeof window.OpenEstatePdfStudio.ingestFiles === 'function') {
            ingestPromise = window.OpenEstatePdfStudio.ingestFiles(fileList, state.property && state.property.slug);
          }
        } catch (_) {}
        var docs = [];
        for (var i = 0; i < fileList.length && i < 20; i++) {
          var f = fileList[i];
          var b64 = await fileToBase64(f);
          docs.push({ name: f.name, base64: b64 });
        }
        var res = await apiPost('/api/copilot/disclosures', {
          slug: state.property.slug,
          documents: docs
        });
        if (typeof res.sharingEnabled === 'boolean') state.sharingEnabled = res.sharingEnabled;
        if (ingestPromise) {
          try {
            var unpacked = await ingestPromise;
            if (Array.isArray(unpacked) && unpacked.length) state.pdfLocalFileCount = unpacked.length;
          } catch (_) {}
        }
        state.disclosures = unwrapDisclosureAnalysis(res.data || res);
        syncPdfStudioAnalysis(res.data || state.disclosures || res);
        state.discNotice = L(
          '✓ One-time analysis completed. Structured summary saved privately for your account/session; original files discarded immediately.',
          '✓ 一次性综合审阅已完成。结构化摘要已私密保存至您的账户/会话，原始文件已立即销毁。',
          '✓ Análisis único completado. Resumen guardado en privado para su cuenta/sesión; archivos originales descartados.'
        );
        await loadDisclosures(true);
      } catch (err) {
        if (err && (err.status === 409 || err.code === 'ONE_TIME_ANALYSIS_ALREADY_USED')) {
          state.discNotice = L(
            '✓ One-time comprehensive analysis has already been completed on this property (original files were discarded). Showing your saved private summary below.',
            '✓ 本套房源已完成过一次性综合审阅（原始文件已销毁）。下方为您展示已私密保存的摘要。',
            '✓ El análisis único ya fue completado para esta propiedad (archivos originales descartados). Mostrando su resumen guardado.'
          );
          await loadDisclosures(true);
        } else {
          showError(err.message);
        }
      } finally {
        state.uploadingDisclosures = false;
        renderBody();
      }
    }

    async function shareDisclosuresWithRegisteredUser(recipientEmail, collaboratorAttested) {
      if (!state.property || state.sharingDealTeam) return;
      showError(null);
      var cleanRecip = String(recipientEmail || '').trim();
      state.shareRecipientDraft = cleanRecip;
      state.shareAttestedDraft = !!collaboratorAttested;
      var u = state.regUser || getStoredUser();
      if (!u || !u.userId) {
        showError(L(
          'Please sign in and verify your email in the Account bar above before sharing redacted results.',
          '请先在上方账户栏完成邮箱验证登录，再向其他注册用户分享脱敏结果。',
          'Inicie sesión y verifique su correo arriba antes de compartir resultados redactados.'
        ));
        var regEmailEl = bodyEl.querySelector('#bcRegEmailInp') || bodyEl.querySelector('#bcRegCodeInp');
        if (regEmailEl && regEmailEl.focus) regEmailEl.focus();
        return;
      }
      if (!collaboratorAttested) {
        showError(L(
          'Please check the collaborator attestation box confirming the recipient is a Principal, Licensed Agent/Broker, or Attorney on this transaction.',
          '请先勾选协作者身份声明，确认接收方系本交易的当事人、持牌经纪人或律师。',
          'Marque la casilla confirmando que el destinatario es parte principal, agente con licencia o abogado en esta transacción.'
        ));
        return;
      }
      if (!cleanRecip || cleanRecip.indexOf('@') < 1) {
        showError(L(
          'Please enter the collaborator’s registered email address.',
          '请输入协作者的注册邮箱地址。',
          'Ingrese el correo registrado del colaborador.'
        ));
        return;
      }
      state.sharingDealTeam = true;
      renderBody();
      try {
        var res = await apiPost('/api/copilot/disclosures/share', {
          slug: state.property.slug,
          recipientEmail: cleanRecip,
          collaboratorAttested: true
        });
        state.shareResult = res;
        state.shareRecipientDraft = '';
        state.shareAttestedDraft = false;
      } catch (err) {
        if (err && (err.status === 401 || err.code === 'AUTH_REQUIRED' || err.code === 'REGISTERED_USER_REQUIRED')) {
          showError(L(
            'Verified session required or expired. Please sign in and verify your email in the Account bar above.',
            '需要有效的已验证签名会话。请在上方账户栏重新完成邮箱验证登录。',
            'Se requiere una sesión verificada válida. Vuelva a iniciar sesión arriba.'
          ));
        } else if (err && (err.status === 400 || err.code === 'COLLABORATOR_ATTESTATION_REQUIRED')) {
          showError(L(
            'Collaborator attestation is required before sharing redacted results.',
            '分享脱敏结果前须勾选协作者身份声明。',
            'Se requiere la certificación de colaborador antes de compartir.'
          ));
        } else if (err && (err.status === 404 || err.code === 'RECIPIENT_NOT_REGISTERED')) {
          showError(L(
            'Collaborator email is not a registered ' + brandName() + ' user yet. Ask them to register in the Account bar first.',
            '该协作者邮箱尚未在本平台注册。请对方先在账户栏完成邮箱注册。',
            'El correo del colaborador aún no está registrado. Pídale que se registre arriba.'
          ));
        } else {
          showError(err.message);
        }
      } finally {
        state.sharingDealTeam = false;
        renderBody();
      }
    }

    async function submitAuthorizedDisclosures(fileList) {
      if (!state.property || state.uploadingAuthorized) return;
      showError(null);
      var d = state.authFormDraft;
      if (!d.signerName || !d.signerName.trim()) {
        showError(L('Signer name is required.', '请填写发布人姓名。', 'Se requiere el nombre del firmante.'));
        return;
      }
      if (d.uploaderRole === 'listing_agent' && (!d.licenseNumber || !d.licenseNumber.trim())) {
        showError(L('License number is required for a listing agent.', '挂牌经纪人发布须填写执业执照号。', 'Se requiere número de licencia para agente listante.'));
        return;
      }
      if (!d.authorizedToDistribute) {
        showError(L('Please confirm authorization to distribute disclosures.', '请勾选确认您拥有分发该披露文件的授权。', 'Confirme autorización para distribuir divulgaciones.'));
        return;
      }
      if (!fileList || !fileList.length) {
        showError(L('Please select a .zip packet or PDF files to analyze.', '请选择需要解析的 .zip 压缩包或 PDF 文件。', 'Seleccione un .zip o PDFs para analizar.'));
        return;
      }
      state.uploadingAuthorized = true;
      renderBody();
      try {
        var authIngestPromise = null;
        try {
          if (window.OpenEstatePdfStudio && typeof window.OpenEstatePdfStudio.ingestFiles === 'function') {
            authIngestPromise = window.OpenEstatePdfStudio.ingestFiles(fileList, state.property && state.property.slug);
          }
        } catch (_) {}
        var docs = [];
        for (var i = 0; i < fileList.length && i < 20; i++) {
          var f = fileList[i];
          var b64 = await fileToBase64(f);
          docs.push({ name: f.name, base64: b64 });
        }
        var payload = {
          slug: state.property.slug,
          uploaderRole: d.uploaderRole || 'listing_agent',
          signerName: d.signerName.trim(),
          authorizedToDistribute: true,
          documents: docs
        };
        if (d.signerEmail && d.signerEmail.trim()) {
          payload.signerEmail = d.signerEmail.trim();
        }
        if (d.uploaderRole === 'listing_agent' && d.licenseNumber && d.licenseNumber.trim()) {
          payload.licenseNumber = d.licenseNumber.trim();
        }
        var res = await apiPost('/api/copilot/disclosures/authorized', payload);
        if (authIngestPromise) {
          try {
            var unpackedAuth = await authIngestPromise;
            if (Array.isArray(unpackedAuth) && unpackedAuth.length) state.pdfLocalFileCount = unpackedAuth.length;
          } catch (_) {}
        }
        state.authViewingReading = true;
        state.discNotice = L(
          '✓ Authorized disclosure summary published (original files discarded immediately).',
          '✓ 已发布授权披露摘要（原始文件已立即销毁）。',
          '✓ Resumen autorizado publicado (archivos originales descartados).'
        );
        if (state.disclosuresState && res.authorizedDisclosures) {
          state.disclosuresState.authorizedDisclosures = res.authorizedDisclosures;
        }
        if (res.authorizedDisclosures && res.authorizedDisclosures.data) {
          syncPdfStudioAnalysis(res.authorizedDisclosures.data);
        }
        await loadDisclosures(true);
      } catch (err) {
        showError(err.message);
      } finally {
        state.uploadingAuthorized = false;
        renderBody();
      }
    }

    async function loadOfferDraft() {
      if (!state.property) return;
      if (state.offerEval) {
        renderBody();
        return;
      }
      state.loading = true;
      renderBody();
      try {
        var r = await fetch('/api/copilot/offer?slug=' + encodeURIComponent(state.property.slug), {
          headers: copilotHeaders()
        });
        var j = await r.json();
        if (j.ok && j.offer) {
          state.offerEval = j.offer;
          state.offerStep = (j.offer.draft && j.offer.draft.currentStep) || 1;
        }
      } catch (err) {
        showError(err.message);
      } finally {
        state.loading = false;
        renderBody();
      }
    }

    async function submitOfferPatch(patchObj, nextStepOverride) {
      if (!state.property) return;
      showError(null);
      var payload = Object.assign({ slug: state.property.slug }, patchObj);
      try {
        var res = await apiPost('/api/copilot/offer', payload);
        state.offerEval = res.offer;
        if (nextStepOverride != null) {
          state.offerStep = nextStepOverride;
        } else if (res.offer && res.offer.draft && res.offer.draft.currentStep) {
          state.offerStep = res.offer.draft.currentStep;
        }
        renderBody();
      } catch (err) {
        showError(err.message);
      }
    }

    function openToolFn(fnName) {
      state.activeFn = fnName || 'chat';
      if (fnName && fnName !== 'chat') state.allowOffMarketPreview = true;
      if (fnName === 'comps_pricing' && !state.compSet) {
        loadComps('strict');
      } else if (fnName === 'disclosures') {
        loadDisclosures(true);
      } else if (fnName === 'offer_builder' && !state.offerEval) {
        loadOfferDraft();
      } else {
        renderBody();
      }
    }

    function renderBody() {
      renderedPdfCitations = [];
      var p = state.property;
      if (!p) {
        var preHtml = [];
        if (state.role === 'unset') {
          preHtml.push('<div style="margin-top:14px">' + renderRolePromptCardHtml() + '</div>');
        }
        bodyEl.innerHTML = preHtml.join('');
        bindDynamicEvents();
        return;
      }

      var bi = p.basicInfo || {};
      var onMarket = !!p.onMarket;
      var showMenu = onMarket || state.allowOffMarketPreview;

      var badgeCls = onMarket ? 'bc-badge bc-badge-sale' : 'bc-badge bc-badge-off';
      var reportHref = state.hasFullReport
        ? ('/r/' + encodeURIComponent(p.slug))
        : ('/report?address=' + encodeURIComponent(p.matchedAddress || p.slug));

      var html = [];
      html.push('<div class="bc-prop-card">');
      html.push('  <div class="bc-prop-top">');
      html.push('    <div>');
      html.push('      <h3 class="bc-prop-addr">' + esc(p.matchedAddress) + '</h3>');
      html.push('      <div style="margin-top:6px;display:flex;align-items:center;gap:8px;flex-wrap:wrap">');
      html.push('        <span class="' + badgeCls + '">' + (onMarket ? '● ' : '○ ') + esc(p.listingStatusLabel) + '</span>');
      if (bi.propertyType) {
        html.push('        <span style="font-size:12.5px;color:var(--bc-muted);font-weight:600">' + esc(bi.propertyType) + '</span>');
      }
      html.push('      </div>');
      html.push('    </div>');
      html.push('    <a class="bc-btn bc-btn-sec" href="' + esc(reportHref) + '" style="text-decoration:none;font-size:12.5px">' + esc(L('Full 50-Layer Report ↗', '查看完整尽调报告 ↗', 'Informe Completo ↗')) + '</a>');
      html.push('  </div>');

      html.push('  <div class="bc-facts-grid">');
      if (bi.listPrice != null) {
        html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">' + esc(L('List Price', '挂牌价', 'Precio Lista')) + '</div><div class="bc-fact-val" style="color:var(--bc-brand-fg)">' + esc(fmtUsd(bi.listPrice)) + '</div></div>');
      } else if (bi.lastSalePrice != null) {
        html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">' + esc(L('Last Recorded Sale', '上次成交价', 'Última Venta')) + '</div><div class="bc-fact-val">' + esc(fmtUsd(bi.lastSalePrice)) + '</div></div>');
      }
      html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">' + esc(L('Beds / Baths', '卧室 / 卫浴', 'Hab / Baños')) + '</div><div class="bc-fact-val">' + esc(bi.beds != null ? bi.beds : '—') + ' / ' + esc(bi.baths != null ? bi.baths : '—') + '</div></div>');
      html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">' + esc(L('Living Area', '室内面积', 'Área Habitable')) + '</div><div class="bc-fact-val">' + (bi.sqft != null ? (esc(fmtNum(bi.sqft)) + ' sq ft') : '—') + '</div></div>');
      html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">' + esc(L('Year Built', '建造年份', 'Año Const.')) + '</div><div class="bc-fact-val">' + esc(bi.yearBuilt != null ? bi.yearBuilt : '—') + '</div></div>');
      if (bi.hoaMonthly != null) {
        html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">HOA</div><div class="bc-fact-val">' + esc(fmtUsd(bi.hoaMonthly)) + '/mo</div></div>');
      }
      if (bi.daysOnMarket != null) {
        html.push('<div class="bc-fact-box"><div class="bc-fact-lbl">' + esc(L('Days on Market', '上市天数', 'Días en Mercado')) + '</div><div class="bc-fact-val">' + esc(bi.daysOnMarket) + 'd</div></div>');
      }
      html.push('  </div>');

      if (!onMarket && p.statusNotice) {
        html.push('  <div class="bc-notice">');
        html.push('    <span>⚠ ' + esc(p.statusNotice) + '</span>');
        if (!state.allowOffMarketPreview) {
          html.push('    <button type="button" class="bc-btn bc-btn-sec" id="bcPreviewOffMarketBtn" style="font-size:12px;padding:7px 11px">' + esc(L('Open ' + brandName() + ' Tools Anyway (Off-Market)', '仍要打开 ' + brandName() + ' 分析工具', 'Abrir Herramientas ' + brandName() + ' Igual')) + '</button>');
        }
        html.push('  </div>');
      }

      html.push('  <div style="margin-top:16px;font-size:12.5px;font-weight:800;color:var(--bc-ink2);text-transform:uppercase;letter-spacing:.04em">' + esc(L('Choose a Mode or Workflow:', '选择对话模式或功能模块：', 'Seleccione un Modo o Función:')) + '</div>');
      html.push('  <div class="bc-menu-grid">');
      var cards = [
        {
          id: 'chat',
          num: '💬',
          t: L('Chat Q&A', 'AI 房产问答', 'Chat de Preguntas'),
          s: L('Role-tailored Q&A across valuation, monthly cost, hazards, HOA, permits & strategy', '按买家/卖家/经纪人视角实时问答估值、持有成本、灾害、HOA、许可与谈判策略', 'Preguntas y respuestas adaptadas a su rol sobre precio, costos, riesgos y estrategia'),
          always: true
        },
        {
          id: 'comps_pricing',
          num: '1',
          t: L('Comps & Pricing', '市场成交对比与估价', 'Comparables y Precio'),
          s: L('Sold comps table first, then optional price range, similar active listings, or pending sales', '先看可比成交表，再按任意顺序查看估价区间、相似在售或待过户房源', 'Tabla de comparables y seguimiento opcional'),
          always: false
        },
        {
          id: 'disclosures',
          num: '2',
          t: L('Disclosures', '披露报告优先级审阅', 'Divulgaciones'),
          s: L('Drop .zip or PDFs — read 1→6 by priority, flag missing docs & share redacted facts', '拖拽上传 .zip 或多份 PDF，按 1→6 优先级精读、提示缺失文件并安全分享脱敏要点', 'Suba .zip o PDFs por prioridad 1→6 y comparta hallazgos redactados'),
          always: false
        },
        {
          id: 'offer_builder',
          num: '3',
          t: L('Offer Builder', '出价单（Offer）生成器', 'Constructor de Oferta'),
          s: L('Guided 6-step offer builder with editable 2.5% commission, local tax/fee splits & live summary', '6 步引导式出价摘要：默认可改 2.5% 佣金、县市过户税费自动查表、每步实时可编辑汇总', 'Formulario guiado de 6 pasos con resumen en vivo'),
          always: false
        }
      ];
      cards.forEach(function (c) {
        if (!c.always && !showMenu) return;
        var actCls = state.activeFn === c.id ? ' act' : '';
        html.push(
          '<button type="button" class="bc-fn-card' + actCls + '" data-fn-pick="' + c.id + '">' +
            '<div class="bc-fn-title"><span class="bc-fn-num">' + esc(c.num) + '</span><span>' + esc(c.t) + '</span></div>' +
            '<div class="bc-fn-sub">' + esc(c.s) + '</div>' +
          '</button>'
        );
      });
      html.push('  </div>');
      html.push('</div>'); // .bc-prop-card

      if (state.activeFn !== 'chat' && state.role === 'unset') {
        html.push('<div style="margin-top:14px">' + renderRolePromptCardHtml() + '</div>');
      }

      if (state.activeFn === 'chat') {
        html.push(renderChatPanel());
      } else if (state.activeFn === 'comps_pricing') {
        html.push(renderFunction1());
      } else if (state.activeFn === 'disclosures') {
        html.push(renderFunction2());
      } else if (state.activeFn === 'offer_builder') {
        html.push(renderFunction3());
      }

      bodyEl.innerHTML = html.join('');
      bindDynamicEvents();
      if (state.activeFn === 'chat') {
        syncBcChatScroll();
      }
    }

    function syncBcChatScroll() {
      var logEl = bodyEl.querySelector('#bcChatLog');
      if (!logEl) return;
      var hasEntries = state.chatHistory.length > 0;
      var last = hasEntries ? state.chatHistory[state.chatHistory.length - 1] : null;
      var lastRole = last ? (last.role === 'user' ? 'user' : 'assistant') : '';
      var msgEl = null;
      if (!state.chatSending && lastRole === 'assistant') {
        var asstEls = logEl.querySelectorAll('[data-bc-chat-entry="assistant"]');
        if (asstEls && asstEls.length) msgEl = asstEls[asstEls.length - 1];
      }
      if (window.OpenEstateCore && typeof window.OpenEstateCore.syncChatLogScroll === 'function') {
        window.OpenEstateCore.syncChatLogScroll(logEl, {
          hasEntries: hasEntries,
          pending: !!state.chatSending,
          lastRole: lastRole,
          msgEl: msgEl
        });
        return;
      }
      if (!hasEntries) {
        logEl.scrollTop = 0;
      } else if (state.chatSending || lastRole === 'user' || !msgEl) {
        logEl.scrollTop = logEl.scrollHeight;
      } else if (typeof msgEl.offsetTop === 'number') {
        logEl.scrollTop = Math.max(0, msgEl.offsetTop);
      }
    }

    function renderChatPanel() {
      var p = state.property || {};
      var starters = roleStarters(state.role);
      var out = ['<div class="bc-work-panel" id="bcChatPanel">'];
      out.push('<div class="bc-sec-hdr">');
      out.push('  <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">');
      out.push('    <h4 class="bc-sec-title">' + esc(L(assistantBrandTitle() + ' — Property Q&A', assistantBrandTitle() + ' — 房产问答对话', assistantBrandTitle() + ' — Preguntas y Respuestas')) + '</h4>');
      var badgeTxt = roleModeBadgeText(state.role);
      if (badgeTxt) {
        var bCls = 'bc-mode-badge' + (state.role === 'seller' ? ' seller' : (state.role === 'agent' ? ' agent' : ''));
        out.push('    <span class="' + bCls + '">' + esc(badgeTxt) + '</span>');
      }
      out.push('  </div>');
      out.push('  <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">');
      out.push('    <span class="bc-prompt-badge">' + esc(roleSystemPromptLabel(state.role)) + '</span>');
      if (state.chatHistory.length) {
        out.push('    <button type="button" class="bc-btn bc-btn-sec" id="bcChatClearBtn" style="padding:5px 10px;font-size:12px">' + esc(L('Clear Chat', '清空对话', 'Limpiar')) + '</button>');
      }
      out.push('  </div>');
      out.push('</div>');

      if (state.role === 'unset') {
        out.push(renderRolePromptCardHtml());
      }

      if (!state.hasFullReport) {
        var rptUrl = '/report?address=' + encodeURIComponent(p.matchedAddress || p.slug || '');
        out.push('<div class="bc-notice" id="bcNoFullReportBanner" style="margin:0 0 12px">');
        out.push('  <div style="flex:1;min-width:240px">');
        out.push('    <div style="font-weight:800">' + esc(L('Full 50-Layer Report not generated for this address yet', '该房源尚未生成缓存的完整 50 维尽调报告', 'El informe completo de 50 capas aún no se ha generado')) + '</div>');
        out.push('    <div style="font-size:12.5px;font-weight:500;margin-top:3px">' + esc(
          state.generatingReport && state.reportBuildStatus
            ? state.reportBuildStatus
            : L('Generate the full report in 1 click to unlock live AI Q&A, or jump right into Comps, Disclosures, or Offer Builder now.', '点击一键生成完整报告以启用 AI 深度问答，或立即使用下方可比成交、披露审阅与出价单工具。', 'Genere el informe completo con 1 clic para habilitar el chat IA, o use Comparables, Divulgaciones y Oferta ahora.')
        ) + '</div>');
        out.push('  </div>');
        out.push('  <div style="display:flex;gap:7px;flex-wrap:wrap">');
        out.push('    <button type="button" class="bc-btn bc-btn-pri" id="bcGenerateReportBtn" style="padding:7px 12px;font-size:12px"' + (state.generatingReport ? ' disabled' : '') + '>' + esc(state.generatingReport ? L('Generating Report…', '正在生成报告…', 'Generando…') : L('Generate Full Report', '一键生成完整报告', 'Generar Informe Completo')) + '</button>');
        out.push('    <a class="bc-btn bc-btn-sec" href="' + esc(rptUrl) + '" style="text-decoration:none;padding:7px 11px;font-size:12px">' + esc(L('Open Report Page ↗', '打开报告页 ↗', 'Abrir Informe ↗')) + '</a>');
        out.push('    <button type="button" class="bc-btn bc-btn-ghost" data-fn-jump="comps_pricing" style="padding:7px 11px;font-size:12px">' + esc(L('1. Comps', '1. 可比成交', '1. Comparables')) + '</button>');
        out.push('    <button type="button" class="bc-btn bc-btn-ghost" data-fn-jump="disclosures" style="padding:7px 11px;font-size:12px">' + esc(L('2. Disclosures', '2. 披露审阅', '2. Divulgaciones')) + '</button>');
        out.push('    <button type="button" class="bc-btn bc-btn-ghost" data-fn-jump="offer_builder" style="padding:7px 11px;font-size:12px">' + esc(L('3. Offer Builder', '3. 出价单', '3. Oferta')) + '</button>');
        out.push('  </div>');
        out.push('</div>');
      }

      out.push('<div style="font-size:12px;font-weight:700;color:var(--bc-muted);margin-bottom:6px">' + esc(L('Suggested Questions for ', '推荐提问（', 'Preguntas Sugeridas (')) + esc(roleLabelText(state.role)) + (curLang() === 'en' ? ':' : '）：') + '</div>');
      out.push('<div class="bc-chat-starters" id="bcChatStarterChips">');
      starters.forEach(function (sq) {
        out.push('<button type="button" class="bc-chat-chip" data-chat-starter="' + esc(sq) + '">' + esc(sq) + '</button>');
      });
      out.push('</div>');

      out.push('<div class="bc-chat-log" id="bcChatLog">');
      if (!state.chatHistory.length) {
        out.push('<div class="bc-chat-msg assistant">' + esc(L(
          'Ask anything about ',
          '欢迎提问关于 ',
          'Pregunte cualquier detalle sobre '
        )) + '<strong>' + esc(p.matchedAddress || p.slug || 'this property') + '</strong>' + esc(L(
          ' — valuation vs comps, monthly cost, hazards, HOA, permits, disclosures, or strategy.',
          ' 的任何问题——估值与可比成交、持有成本、自然灾害、HOA、施工许可、披露风险或谈判策略。',
          ' — valoración vs comparables, costo mensual, riesgos, HOA, permisos o estrategia.'
        )) + '</div>');
      } else {
        state.chatHistory.forEach(function (msg) {
          var cls = msg.role === 'user' ? 'user' : 'assistant';
          out.push('<div class="bc-chat-msg ' + cls + '" data-bc-chat-entry="' + cls + '">');
          if (msg.role === 'assistant') {
            out.push('<div>' + formatChatAnswerHtml(msg.content) + '</div>');
            if (Array.isArray(msg.citations) && msg.citations.length) {
              out.push('<div class="bc-chat-cites">');
              msg.citations.slice(0, 6).forEach(function (c) {
                out.push('<span class="bc-chat-cite">' + esc(c) + '</span>');
              });
              out.push('</div>');
            }
            if (msg.needsRoleSelection && state.role === 'unset') {
              out.push('<div style="margin-top:8px;padding-top:8px;border-top:1px dashed var(--bc-line-strong);font-size:12px;color:var(--bc-brand-fg);font-weight:700">' + esc(msg.roleSelectionPrompt || L('Pick Buyer, Seller, or Agent above to tailor future answers.', '在上方选择买家、卖家或经纪人身份即可获得专属策略建议。', 'Elija Comprador, Vendedor o Agente arriba para personalizar respuestas.')) + '</div>');
            }
            if (Array.isArray(msg.followUps) && msg.followUps.length) {
              out.push('<div class="bc-chat-followups">');
              msg.followUps.slice(0, 3).forEach(function (fq) {
                out.push('<button type="button" class="bc-chat-fbtn" data-chat-followup="' + esc(fq) + '">↪ ' + esc(fq) + '</button>');
              });
              out.push('</div>');
            }
          } else {
            out.push('<div>' + formatChatAnswerHtml(msg.content) + '</div>');
          }
          out.push('</div>');
        });
      }
      if (state.chatSending) {
        out.push('<div class="bc-chat-msg assistant">' + esc(L('Reviewing verified property records…', '正在核查真实房产档案并生成回答…', 'Consultando registros verificados…')) + '</div>');
      }
      out.push('</div>');

      out.push('<form class="bc-chat-form" id="bcChatForm">');
      out.push('  <input type="text" class="bc-chat-inp" id="bcChatInput" autocomplete="off" placeholder="' + esc(L(
        'Ask a question about this property…',
        '输入关于这套房产的问题…',
        'Haga una pregunta sobre esta propiedad…'
      )) + '"' + (state.chatSending ? ' disabled' : '') + '>');
      out.push('  <button type="submit" class="bc-btn bc-btn-pri" id="bcChatSendBtn"' + (state.chatSending ? ' disabled' : '') + '>' + esc(L('Ask AI', '发送提问', 'Preguntar')) + '</button>');
      out.push('</form>');
      out.push('</div>');
      return out.join('');
    }

    function renderFunction1() {
      var out = ['<div class="bc-work-panel" id="bcPanelFn1">'];
      out.push('<div class="bc-sec-hdr">');
      out.push('  <h4 class="bc-sec-title">1. ' + esc(L('Market Comps & Pricing Analysis', '市场可比成交（Comps）与定价分析', 'Comparables de Mercado y Precio')) + '</h4>');
      if (state.compSet) {
        var isExp = state.compSet.mode === 'expanded';
        out.push('  <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">');
        out.push('    <span style="font-size:11.5px;color:var(--bc-subtle);font-weight:700">Set ID: ' + esc(state.compSet.compSetId) + '</span>');
        if (isExp) {
          out.push('    <button type="button" class="bc-btn bc-btn-sec" id="bcSwitchStrictBtn" style="padding:6px 11px;font-size:12px">' + esc(L('Use Strict Filter (≤3 mo, ≤1 mi)', '切回严格筛选（≤3个月, ≤1英里）', 'Usar Filtro Estricto')) + '</button>');
        } else if (state.compSet.canExpandSearch) {
          out.push('    <button type="button" class="bc-btn bc-btn-ghost" id="bcExpandCompsBtn" style="padding:6px 11px;font-size:12px">' + esc(L('Expand to 6 Months / 2 Miles', '扩展至 6 个月 / 2 英里', 'Ampliar a 6 Meses / 2 Millas')) + '</button>');
        }
        out.push('  </div>');
      }
      out.push('</div>');

      if (state.loading && !state.compSet) {
        out.push('<p style="color:var(--bc-muted);font-size:13.5px">' + esc(L('Loading comparable sales…', '正在检索周边可比成交…', 'Cargando ventas comparables…')) + '</p></div>');
        return out.join('');
      }

      if (!state.compSet) {
        out.push('<p style="color:var(--bc-ink2);font-size:13.5px;margin:0 0 12px">' + esc(L('Do you want to generate market comps (90%–110% sq ft, similar age, same property type, past 3 months, within 1 mile)?', '是否生成周边市场可比成交（面积 90%–110%、房龄相近、同类型、近 3 个月、1 英里内）？', '¿Desea generar los comparables de mercado?')) + '</p>');
        out.push('<button type="button" class="bc-btn bc-btn-pri" id="bcRunCompsYesBtn">' + esc(L('Yes — Show Market Comps', '是 — 生成可比成交表', 'Sí — Mostrar Comparables')) + '</button>');
        out.push('</div>');
        return out.join('');
      }

      var cs = state.compSet;
      var comps = Array.isArray(cs.comps) ? cs.comps : [];
      out.push('<div style="font-size:12.5px;color:var(--bc-muted);margin-bottom:8px">' + esc(cs.filterSummary || '') + '</div>');

      if (cs.canExpandSearch && cs.expandPrompt) {
        out.push('<div class="bc-notice" style="margin:8px 0 12px"><span>' + esc(cs.expandPrompt) + '</span><button type="button" class="bc-btn bc-btn-pri" id="bcExpandCompsInlineBtn" style="padding:7px 12px;font-size:12px">' + esc(L('Expand to 6 Months / 2 Miles', '扩展至 6 个月 / 2 英里', 'Ampliar a 6 Meses / 2 Millas')) + '</button></div>');
      }

      if (!comps.length) {
        out.push('<div class="bc-subcard" style="color:var(--bc-muted);font-size:13.5px">' + esc(L('No verified arm’s-length closed sales matched these exact bounds in the connected feed.', '当前数据源中暂无完全符合该严格筛选条件的近期过户成交记录。', 'No se encontraron ventas cerradas con estos filtros exactos.')) + '</div>');
      } else {
        out.push('<div style="display:flex;gap:14px;flex-wrap:wrap;font-size:13px;font-weight:700;color:var(--bc-brand-fg);margin-top:6px">');
        out.push('  <span>' + esc(L('Matched Comps:', '匹配成交数：', 'Comparables:')) + ' ' + comps.length + '</span>');
        if (cs.avgPricePerSqft != null) {
          out.push('  <span>' + esc(L('Avg $/Sq Ft:', '平均单价：', 'Promedio $/Pie²:')) + ' $' + esc(fmtNum(cs.avgPricePerSqft)) + '/sq ft</span>');
        }
        out.push('</div>');
        out.push('<div class="bc-table-wrap"><table class="bc-table"><thead><tr>');
        out.push('<th>' + esc(L('Address', '地址', 'Dirección')) + '</th>');
        out.push('<th>' + esc(L('Sold Price', '成交价', 'Precio Venta')) + '</th>');
        out.push('<th>$/Sq Ft</th>');
        out.push('<th>Sq Ft</th>');
        out.push('<th>' + esc(L('Beds/Baths', '房/卫', 'Hab/Baños')) + '</th>');
        out.push('<th>' + esc(L('Yr Built', '年份', 'Año')) + '</th>');
        out.push('<th>' + esc(L('Distance', '距离', 'Distancia')) + '</th>');
        out.push('<th>' + esc(L('Sold Date', '成交日', 'Fecha')) + '</th>');
        out.push('</tr></thead><tbody>');
        comps.forEach(function (c) {
          out.push('<tr>');
          out.push('<td><strong>' + esc(c.address) + '</strong>' + (c.matchTier === 'expanded' ? ' <span class="bc-cite">6mo/2mi</span>' : '') + '</td>');
          out.push('<td style="font-weight:800;color:var(--bc-brand-fg)">' + esc(fmtUsd(c.soldPrice)) + '</td>');
          out.push('<td>$' + esc(fmtNum(c.pricePerSqft)) + '</td>');
          out.push('<td>' + esc(fmtNum(c.sqft)) + '</td>');
          out.push('<td>' + esc(c.beds != null ? c.beds : '—') + ' / ' + esc(c.baths != null ? c.baths : '—') + '</td>');
          out.push('<td>' + esc(c.yearBuilt != null ? c.yearBuilt : '—') + '</td>');
          out.push('<td>' + esc(c.distanceMi != null ? (c.distanceMi + ' mi') : '—') + '</td>');
          out.push('<td>' + esc(c.soldDate || '—') + '</td>');
          out.push('</tr>');
        });
        out.push('</tbody></table></div>');
      }

      // Optional non-linear follow-up group (pickable in any order, reusing cs.compSetId)
      out.push('<div class="bc-followup-bar">');
      out.push('<span style="font-size:12.5px;font-weight:700;color:var(--bc-ink2);align-self:center;margin-right:4px">' + esc(L('Optional Follow-Ups (any order, same comp set):', '可选后续分析（任意顺序，复用同一组 Comps）：', 'Análisis Opcionales (cualquier orden):')) + '</span>');
      var fOpts = [
        { k: 'estimate', lbl: L('Estimate Price Range', '生成估价区间', 'Estimar Rango de Precio') },
        { k: 'similar_active', lbl: L('Similar Properties For Sale', '查看相似在售房源', 'Propiedades Similares en Venta') },
        { k: 'sold_or_pending', lbl: L('Recently Sold or Pending', '近期成交或待过户（Pending）', 'Vendidas o Pendientes') }
      ];
      fOpts.forEach(function (fo) {
        var act = state.activeFollowUp === fo.k ? ' act' : '';
        out.push('<button type="button" class="bc-fchip' + act + '" data-followup="' + fo.k + '">' + esc(fo.lbl) + '</button>');
      });
      out.push('</div>');

      if (state.activeFollowUp) {
        var fuData = state.followUps[state.activeFollowUp];
        if (!fuData && state.loading) {
          out.push('<div class="bc-subcard">' + esc(L('Computing follow-up from pinned comp set…', '正在基于已锁定 Comps 计算…', 'Calculando…')) + '</div>');
        } else if (fuData) {
          out.push(renderFollowUpCard(fuData));
        }
      }

      out.push('</div>');
      return out.join('');
    }

    function renderFollowUpCard(fu) {
      if (fu.followUp === 'estimate') {
        var rng = fu.estimateRangeUsd;
        var conf = fu.confidence || {};
        var h = ['<div class="bc-subcard">'];
        h.push('<div style="display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:8px">');
        h.push('  <h5 style="margin:0;font-size:15px;font-weight:800">' + esc(L('Comp-Based Price Estimate', '基于同组 Comps 的价格区间估算', 'Estimación Basada en Comparables')) + '</h5>');
        h.push('  <span class="bc-badge bc-badge-sale">' + esc(L('Confidence:', '置信度：', 'Confianza:')) + ' ' + esc(conf.level || '—') + '</span>');
        h.push('</div>');
        if (rng && rng.low != null && rng.high != null) {
          h.push('<div style="font-size:24px;font-weight:800;color:var(--bc-brand-fg);margin-top:8px;font-variant-numeric:tabular-nums">' + esc(fmtUsd(rng.low)) + ' – ' + esc(fmtUsd(rng.high)) + ' <span style="font-size:14px;color:var(--bc-ink2);font-weight:700">(Midpoint: ' + esc(fmtUsd(rng.mid)) + ')</span></div>');
        } else {
          h.push('<div style="font-size:13.5px;color:var(--bc-warn-fg);margin-top:8px;font-weight:600">' + esc(conf.rationale || 'Insufficient comparable data to compute range.') + '</div>');
        }
        if (rng && conf.rationale) {
          h.push('<div style="font-size:12.5px;color:var(--bc-muted);margin-top:6px">' + esc(conf.rationale) + '</div>');
        }
        h.push('</div>');
        return h.join('');
      }

      if (fu.followUp === 'similar_active') {
        var act = Array.isArray(fu.activeListings) ? fu.activeListings : [];
        var h2 = ['<div class="bc-subcard">'];
        h2.push('<h5 style="margin:0 0 4px;font-size:15px;font-weight:800">' + esc(L('Similar Properties Actively For Sale (±15% sq ft, ≤2 mi)', '周边相似在售房源（±15% 面积，2 英里内）', 'Propiedades Similares en Venta')) + '</h5>');
        h2.push('<div style="font-size:12px;color:var(--bc-muted);margin-bottom:8px">' + esc(fu.filterSummary || '') + '</div>');
        if (!act.length) {
          h2.push('<div style="font-size:13px;color:var(--bc-muted)">' + esc(L('No active listings within ±15% sq ft and 2 miles found in current feed.', '当前数据源在 2 英里及 ±15% 面积范围内暂无匹配的在售房源。', 'No se encontraron propiedades activas similares.')) + '</div>');
        } else {
          h2.push('<div class="bc-table-wrap"><table class="bc-table"><thead><tr><th>Address</th><th>Price</th><th>$/Sq Ft</th><th>Sq Ft</th><th>Bedrooms</th><th>Year Built</th><th>Distance</th></tr></thead><tbody>');
          act.forEach(function (a) {
            h2.push('<tr><td><strong>' + esc(a.address) + '</strong></td><td style="font-weight:800;color:var(--bc-brand-fg)">' + esc(fmtUsd(a.price)) + '</td><td>$' + esc(fmtNum(a.pricePerSqft)) + '</td><td>' + esc(fmtNum(a.sqft)) + '</td><td>' + esc(a.beds != null ? a.beds : '—') + '</td><td>' + esc(a.yearBuilt != null ? a.yearBuilt : '—') + '</td><td>' + esc(a.distanceMi != null ? (a.distanceMi + ' mi') : '—') + '</td></tr>');
          });
          h2.push('</tbody></table></div>');
        }
        h2.push('</div>');
        return h2.join('');
      }

      if (fu.followUp === 'sold_or_pending') {
        var pend = Array.isArray(fu.pendingListings) ? fu.pendingListings : [];
        var sold = Array.isArray(fu.recentlySold) ? fu.recentlySold : [];
        var h3 = ['<div class="bc-subcard">'];
        h3.push('<h5 style="margin:0 0 8px;font-size:15px;font-weight:800">' + esc(L('Pending / Under Contract & Recently Sold (Same Comp Set)', '待过户（Pending）与近期成交房源（同组锁定 Comps）', 'Pendientes y Vendidas Recientemente')) + '</h5>');
        h3.push('<div style="font-size:12.5px;font-weight:700;color:var(--bc-ink2);margin-bottom:4px">' + esc(L('Pending / Under Contract Nearby:', '周边待过户（Pending）：', 'Pendientes:')) + ' ' + pend.length + '</div>');
        if (pend.length) {
          h3.push('<div class="bc-table-wrap" style="margin-bottom:12px"><table class="bc-table"><thead><tr><th>Address</th><th>Status</th><th>List Price</th><th>Sq Ft</th><th>Beds</th><th>Distance</th></tr></thead><tbody>');
          pend.forEach(function (p) {
            h3.push('<tr><td><strong>' + esc(p.address) + '</strong></td><td><span class="bc-badge bc-badge-off">Pending</span></td><td>' + esc(fmtUsd(p.listPrice)) + '</td><td>' + esc(fmtNum(p.sqft)) + '</td><td>' + esc(p.beds != null ? p.beds : '—') + '</td><td>' + esc(p.distanceMi != null ? (p.distanceMi + ' mi') : '—') + '</td></tr>');
          });
          h3.push('</tbody></table></div>');
        } else {
          h3.push('<div style="font-size:12.5px;color:var(--bc-subtle);margin-bottom:10px">' + esc(L('No nearby Pending listings reported in current feed; showing pinned Recently Sold comps below.', '当前数据源暂无周边待过户（Pending）记录；下方展示锁定的近期成交表。', 'Sin propiedades pendientes cercanas; mostrando vendidas abajo.')) + '</div>');
        }
        h3.push('<div style="font-size:12.5px;font-weight:700;color:var(--bc-ink2)">' + esc(L('Pinned Recently Sold Comps Reused:', '复用的近期过户成交数：', 'Comparables Vendidos:')) + ' ' + sold.length + (fu.compSetAvgPricePerSqft ? (' · Avg $' + fmtNum(fu.compSetAvgPricePerSqft) + '/sq ft') : '') + '</div>');
        h3.push('</div>');
        return h3.join('');
      }
      return '';
    }

    function buildFallbackMailto(prop) {
      if (!prop) return null;
      var addr = prop.matchedAddress || prop.slug || 'Subject Property';
      var apn = prop.basicInfo && prop.basicInfo.apn ? String(prop.basicInfo.apn) : '';
      var subj = 'Disclosure & Inspection Packet Request — ' + addr + (apn ? (' (APN ' + apn + ')') : '');
      var body = [
        'Hello,',
        '',
        'I am evaluating ' + addr + (apn ? (' (APN ' + apn + ')') : '') + ' and would appreciate copies of any available seller disclosures and inspection reports (.zip packet or individual PDFs):',
        '1. Home, Roof & Sewer Lateral Inspection Reports',
        '2. Termite / Pest (WDO) Inspection Report',
        '3. Preliminary Title Report (Prelim)',
        '4. Natural Hazard Disclosure (NHD) & Seller Disclosures (TDS / SPQ)',
        '5. HOA Financials, CC&Rs & Reserve Study (if applicable)',
        '6. Building Permits & Final Sign-offs',
        '',
        'Thank you!'
      ].join('\n');
      return {
        recipientEmail: '',
        recipientLabel: L('Listing Agent or Cooperating Agent Email (optional)', '挂牌经纪人或合作经纪人邮箱（选填）', 'Correo del agente (opcional)'),
        subject: subj,
        body: body,
        mailtoHref: buildMailtoHref('', subj, body),
        uploadFollowUpPrompt: L(
          'Step 2: Once they reply with the .zip or PDF reports, upload them in ' + assistantBrandTitle() + ' below for your one-time private reading.',
          '第 2 步：收到对方回复的 .zip 披露包或 PDF 报告后，请在下方 ' + assistantBrandTitle() + ' 上传以获取您的一次性私密精读。',
          'Paso 2: Cuando reciba los PDF o el paquete .zip, súbalos en ' + assistantBrandTitle() + ' abajo para su lectura privada única.'
        )
      };
    }

    function renderPriorityReadingHtml(analysisObj, optsPr) {
      optsPr = optsPr || {};
      var pr = analysisObj && analysisObj.priorityReading;
      if (!pr) return '';
      var out = [];

      var aiHeadline = String(analysisObj.executiveHeadline || analysisObj.headline || '').trim();
      var aiExec = String(analysisObj.executiveSummary || '').trim();
      var aiNeg = String(analysisObj.negotiationStrategy || '').trim();
      var aiQuestions = Array.isArray(analysisObj.topQuestionsForSeller) ? analysisObj.topQuestionsForSeller : [];
      var aiEnhanced = !!analysisObj.aiEnhanced;
      var synthModelLabel = String(analysisObj.model || '').trim();
      if (aiHeadline || aiExec || aiNeg) {
        out.push('<div class="bc-notice" id="bcAiPacketBriefCard" style="margin-top:12px;display:block;border-left:4px solid var(--bc-brand-fg)">');
        out.push('  <div style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap;margin-bottom:6px">');
        out.push('    <strong style="font-size:14px;color:var(--bc-ink)">' + esc(aiHeadline || L('Disclosure Packet Briefing', '披露包综合研判', 'Resumen del Paquete')) + '</strong>');
        if (aiEnhanced) {
          out.push('    <span class="bc-badge bc-badge-sale" id="bcSynthBadge">✨ ' + esc(L('AI Synthesized', 'AI 深度综合', 'Sintetizado con IA')) + (synthModelLabel ? (' · ' + esc(synthModelLabel)) : '') + '</span>');
        }
        out.push('  </div>');
        if (aiExec) {
          out.push('  <div style="font-size:13px;line-height:1.5;color:#1e293b;margin-bottom:6px">' + esc(aiExec) + '</div>');
        }
        if (aiNeg) {
          out.push('  <div style="font-size:12.5px;line-height:1.45;color:var(--bc-muted);margin-bottom:4px"><strong>' + esc(L('Negotiation Strategy:', '谈判策略建议：', 'Estrategia de Negociación:')) + '</strong> ' + esc(aiNeg) + '</div>');
        }
        if (aiQuestions.length) {
          out.push('  <div style="font-size:12px;color:var(--bc-muted);margin-top:6px"><strong>' + esc(L('Top Questions for Seller:', '向卖方核实要点：', 'Preguntas Clave al Vendedor:')) + '</strong> ' + esc(aiQuestions.slice(0, 3).join(' · ')) + '</div>');
        }
        out.push('</div>');
      }

      var missing = Array.isArray(pr.missingDocuments) ? pr.missingDocuments : [];
      if (missing.length) {
        out.push('<div class="bc-notice" style="margin-top:12px"><div><strong>' + esc(L('Missing / Skipped Priority Reports:', '缺失并跳过的优先级文件：', 'Documentos Faltantes Omitidos:')) + '</strong> ' + esc(missing.map(function (m) { return '#' + m.priority + ' ' + m.label; }).join(' · ')) + '</div></div>');
      }

      var secs = Array.isArray(pr.prioritySections) ? pr.prioritySections : [];
      out.push('<div class="bc-prio-list">');
      secs.forEach(function (sec) {
        var present = !!(sec.present || sec.status === 'present');
        out.push('<div class="bc-prio-card' + (present ? '' : ' missing') + '">');
        out.push('  <div style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap">');
        out.push('    <div style="font-weight:800;font-size:14px;color:var(--bc-ink)">#' + esc(sec.priority) + ' · ' + esc(sec.label) + '</div>');
        out.push('    <span class="bc-badge ' + (present ? 'bc-badge-sale' : 'bc-badge-off') + '">' + esc(present ? L('Reviewed', '已审阅', 'Revisado') : L('Missing — Skipped', '未提供 — 已跳过', 'Faltante')) + '</span>');
        out.push('  </div>');
        out.push('  <div style="font-size:12.5px;color:var(--bc-muted);margin-top:5px">' + esc(sec.summary || '') + '</div>');
        var items = Array.isArray(sec.problematicItems) ? sec.problematicItems : [];
        if (items.length) {
          out.push('  <ul style="margin:8px 0 0;padding-left:18px;font-size:13px;color:#1f2937">');
          items.forEach(function (it) {
            var srcDoc = it.sourceDoc || it.sourceDocument || 'Document';
            var srcPg = it.sourcePage != null ? it.sourcePage : 1;
            var citeIdx = renderedPdfCitations.length;
            renderedPdfCitations.push({
              analysis: analysisObj,
              citation: {
                sourceDoc: srcDoc,
                sourcePage: srcPg,
                title: it.title || '',
                detail: it.detail || '',
                severity: it.severity || '',
                searchTerms: [it.title || '', it.detail || ''].filter(Boolean)
              }
            });
            out.push('<li style="margin-bottom:5px"><strong>[' + esc(it.severity) + '] ' + esc(it.title) + ':</strong> ' + esc(it.detail) + '<span class="bc-cite">' + esc(srcDoc) + ', p.' + esc(srcPg) + '</span><button type="button" class="bc-btn bc-btn-ghost bc-pdf-cite-btn" data-pdf-cite-idx="' + citeIdx + '">🎯 ' + esc(L('Point & Circle in PDF', 'PDF 定位圈注', 'Señalar en PDF')) + '</button></li>');
          });
          out.push('  </ul>');
        }
        out.push('</div>');
      });
      out.push('</div>');

      var rfs = Array.isArray(pr.redFlags) ? pr.redFlags : [];
      out.push('<div class="bc-redflags">');
      out.push('  <div style="display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:8px">');
      out.push('    <h5 style="margin:0;font-size:15px;font-weight:800;color:var(--bc-err-fg)">🚩 ' + esc(L('Summary Red Flags', '核心红旗风险汇总（Red Flags）', 'Resumen de Banderas Rojas')) + ' (' + rfs.length + ')</h5>');
      if (rfs.length && optsPr.allowOfferImport !== false) {
        out.push('    <button type="button" class="bc-btn bc-btn-sec" data-send-redflags="1" id="' + esc(optsPr.offerBtnId || 'bcSendRedFlagsToOfferBtn') + '" style="padding:6px 11px;font-size:12px">' + esc(L('Use in Offer Builder →', '导入出价单生成器 →', 'Usar en Oferta →')) + '</button>');
      }
      out.push('  </div>');
      if (!rfs.length) {
        out.push('  <p style="margin:8px 0 0;font-size:13px;color:#7f1d1d">' + esc(L('No Critical or Major red flags detected across uploaded files. Always review complete original documents.', '已上传文件中未检出重大红旗项，请仍务必通读原始报告全文。', 'No se detectaron banderas rojas críticas en los archivos subidos.')) + '</p>');
      } else {
        out.push('  <ul style="margin:8px 0 0;padding-left:18px;font-size:13px;color:#7f1d1d">');
        rfs.forEach(function (rf) {
          var rfDoc = rf.sourceDoc || rf.sourceDocument || 'Document';
          var rfPg = rf.sourcePage != null ? rf.sourcePage : 1;
          var rfIdx = renderedPdfCitations.length;
          renderedPdfCitations.push({
            analysis: analysisObj,
            citation: {
              sourceDoc: rfDoc,
              sourcePage: rfPg,
              title: rf.title || '',
              detail: rf.detail || '',
              severity: rf.severity || 'Major',
              searchTerms: [rf.title || '', rf.detail || ''].filter(Boolean)
            }
          });
          out.push('<li style="margin-bottom:6px"><strong>' + esc(rf.title) + '</strong> (' + esc(rf.category) + ') — ' + esc(rf.detail) + '<span class="bc-cite">' + esc(rfDoc) + ', p.' + esc(rfPg) + '</span><button type="button" class="bc-btn bc-btn-ghost bc-pdf-cite-btn" data-pdf-cite-idx="' + rfIdx + '">🎯 ' + esc(L('Point & Circle in PDF', 'PDF 定位圈注', 'Señalar en PDF')) + '</button></li>');
        });
        out.push('  </ul>');
      }
      out.push('  <div style="margin-top:10px;padding-top:8px;border-top:1px solid #fecaca;font-size:12px;color:var(--bc-err-fg);font-weight:600">' + esc(pr.standingDisclaimer || 'Informational research only — not legal advice, tax advice, a home inspection, or a licensed real-estate appraisal.') + '</div>');
      out.push('</div>');

      return out.join('');
    }

    function renderPublicRecordBaselineHtml(pubEq) {
      var out = ['<div class="bc-mode-box" id="bcPublicBaselineBox">'];
      out.push('<div style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap;margin-bottom:8px">');
      out.push('  <h5 style="margin:0;font-size:15px;font-weight:800;color:var(--bc-ink)">🏛️ ' + esc(L('1. Public-Record Baseline (Always Available · 0 Uploads Needed)', '1. 政府公开数据基准（所有用户免费可见 · 无需上传）', '1. Base de Registros Públicos (Siempre Disponible)')) + '</h5>');
      out.push('  <span class="bc-badge bc-badge-sale">' + esc(L('Open Government Data', '公开政府数据', 'Datos Públicos')) + '</span>');
      out.push('</div>');
      out.push('<p style="font-size:12.5px;color:var(--bc-muted);margin:0 0 8px">' + esc(L(
        'Open-data equivalents from public GIS hazard layers, county assessor/deed records, public permits, and HOA/special tax district profiles — always accessible even before private seller disclosures arrive.',
        '基于公开 GIS 自然灾害图层、县府税单/产权历史、公开施工许可及 HOA/特别税区档案自动生成——无需等待卖家披露包即可立即核查。',
        'Equivalentes públicos de riesgos GIS, historial fiscal y escrituras del condado, permisos públicos y contexto HOA/impuestos especiales.'
      )) + '</p>');

      var items = (pubEq && Array.isArray(pubEq.items)) ? pubEq.items : [];
      if (state.loadingDisclosures && !items.length) {
        out.push('<div style="font-size:12.5px;color:var(--bc-subtle)">' + esc(L('Loading public-record baseline…', '正在加载公开档案基准…', 'Cargando base pública…')) + '</div>');
      } else if (!items.length) {
        out.push('<div style="font-size:12.5px;color:var(--bc-subtle)">' + esc(L('No additional public-record items returned for this parcel yet. Generate the full 50-layer report for deep GIS hazard & permit screening.', '当前暂未返回额外公开记录条目，可点击生成完整 50 维尽调报告查看详细 GIS 灾害与许可记录。', 'Sin registros públicos adicionales aún. Genere el informe completo para análisis GIS.')) + '</div>');
      } else {
        out.push('<div class="bc-pub-grid">');
        items.forEach(function (it) {
          var title = it.title || it.label || it.category || 'Public Record';
          var status = it.status || it.badge || '';
          var summary = it.summary || it.detail || it.description || '—';
          var srcName = it.source || it.sourceName || '';
          var srcUrl = safeHttpUrl(it.sourceUrl || it.url);
          var redirSrcUrl = srcUrl ? toRedirectHref(srcUrl, { section: 'copilot', slug: state.slug, dataId: it.extensionId || it.dataId || '', name: srcName || title }) : '';
          var bullets = Array.isArray(it.highlights) ? it.highlights : (Array.isArray(it.facts) ? it.facts : []);
          out.push('<div class="bc-pub-card">');
          out.push('  <div style="display:flex;justify-content:space-between;align-items:flex-start;gap:6px">');
          out.push('    <strong style="font-size:13.5px;color:var(--bc-ink)">' + esc(title) + '</strong>');
          if (status) out.push('    <span class="bc-badge bc-badge-sale" style="font-size:11px;padding:2px 8px">' + esc(status) + '</span>');
          out.push('  </div>');
          out.push('  <div style="font-size:12.5px;color:var(--bc-ink2);line-height:1.42">' + esc(summary) + '</div>');
          if (bullets.length) {
            out.push('  <ul style="margin:4px 0 0;padding-left:16px;font-size:12px;color:var(--bc-muted)">');
            bullets.slice(0, 5).forEach(function (b) {
              var bTxt = typeof b === 'string' ? b : ((b && b.label ? b.label + ': ' : '') + (b && (b.value != null ? b.value : b.detail) != null ? (b.value != null ? b.value : b.detail) : '—'));
              out.push('<li>' + esc(bTxt) + '</li>');
            });
            out.push('  </ul>');
          }
          var showSrcLink = Boolean(srcUrl && isDisplayLinksOn());
          if (srcName || showSrcLink) {
            out.push('  <div style="margin-top:auto;padding-top:4px;font-size:11.5px;color:var(--bc-subtle)">' +
              (showSrcLink ? ('<a href="' + esc(redirSrcUrl || srcUrl) + '" data-direct-url="' + esc(srcUrl) + '" target="_blank" rel="noopener noreferrer" style="color:var(--bc-brand-fg);font-weight:700;text-decoration:none">' + esc(srcName || 'Official Source') + ' ↗</a>') : esc(srcName)) +
              '</div>');
          }
          out.push('</div>');
        });
        out.push('</div>');
      }
      if (pubEq && pubEq.disclaimer) {
        out.push('<div style="margin-top:10px;font-size:11.5px;color:var(--bc-subtle)">' + esc(pubEq.disclaimer) + '</div>');
      }
      out.push('</div>');
      return out.join('');
    }

    function renderRegUserBarHtml() {
      var u = state.regUser || getStoredUser();
      var pv = state.pendingVerification;
      var out = ['<div class="bc-reg-bar" id="bcRegUserBar">'];
      if (u && u.userId) {
        var who = u.email || u.displayName || u.userId;
        out.push('<div style="display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap">');
        out.push('  <div style="font-size:13px;font-weight:700;color:var(--bc-ok-fg)">👤 ' + esc(L(
          'Signed in as ' + who + ' (Verified Session)',
          '已验证登录：' + who + '（签名会话）',
          'Sesión verificada iniciada como ' + who
        )) + '</div>');
        out.push('  <button type="button" class="bc-btn bc-btn-sec" id="bcRegSignOutBtn" style="padding:6px 12px;font-size:12px">' + esc(L('Sign Out', '退出登录', 'Cerrar Sesión')) + '</button>');
        out.push('</div>');
      } else if (pv && pv.email) {
        out.push('<div style="font-weight:800;font-size:13px;color:var(--bc-ink)">🔐 ' + esc(L(
          'Verify Email for ' + pv.email + ' (6-Digit Challenge)',
          '验证邮箱：' + pv.email + '（6 位数字验证码）',
          'Verificar correo para ' + pv.email + ' (Código de 6 dígitos)'
        )) + '</div>');
        if (pv.smtpDisabled && pv.devOrLocalVerificationCode) {
          out.push('<div id="bcLocalVerifyHint" style="margin-top:6px;padding:7px 10px;border-radius:8px;background:var(--bc-info-bg);border:1px solid var(--bc-info-bd);color:var(--bc-info-fg);font-size:12px;display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap">');
          out.push('  <span>' + esc(L('Local / Offline Mode (SMTP disabled) — Verification Code: ', '本地离线模式（未启用 SMTP）— 验证码：', 'Modo Local (SMTP deshabilitado) — Código: ')) + '<code id="bcDevVerifyCodeBadge" style="font-weight:800;font-size:13px;padding:1px 6px;border-radius:5px;background:var(--bc-surface)">' + esc(pv.devOrLocalVerificationCode) + '</code></span>');
          out.push('  <button type="button" class="bc-btn bc-btn-pri" id="bcOneClickVerifyBtn" style="padding:5px 11px;font-size:12px"' + (state.verifyingCode ? ' disabled' : '') + '>' + esc(L('1-Click Verify & Sign In', '一键验证并登录', 'Verificar en 1 Clic')) + '</button>');
          out.push('</div>');
        }
        out.push('<form class="bc-reg-form" id="bcVerifyForm" style="margin-top:8px">');
        out.push('  <input type="text" class="bc-inp" id="bcRegCodeInp" inputmode="numeric" pattern="[0-9]{6}" maxlength="6" value="' + esc(state.regCodeDraft || '') + '" placeholder="' + esc(L('Enter 6-digit verification code', '输入 6 位数字验证码', 'Código de 6 dígitos')) + '" required>');
        out.push('  <button type="submit" class="bc-btn bc-btn-pri" id="bcVerifySubmitBtn" style="padding:8px 14px;font-size:12.5px"' + (state.verifyingCode ? ' disabled' : '') + '>' + esc(state.verifyingCode ? L('Verifying…', '验证中…', 'Verificando…') : L('Verify & Sign In', '验证并登录', 'Verificar y Entrar')) + '</button>');
        out.push('  <button type="button" class="bc-btn bc-btn-sec" id="bcVerifyCancelBtn" style="padding:8px 12px;font-size:12px">' + esc(L('Change Email', '更换邮箱', 'Cambiar Correo')) + '</button>');
        out.push('</form>');
      } else {
        out.push('<div style="font-weight:800;font-size:13px;color:var(--bc-ink)">👤 ' + esc(L(
          'Verified Account (Required to Share or Receive Redacted Factual Findings)',
          '验证注册账户（向协作者分享或接收脱敏事实摘要须完成邮箱验证登录）',
          'Cuenta Verificada (Requerida para Compartir o Recibir Hallazgos Redactados)'
        )) + '</div>');
        out.push('<form class="bc-reg-form" id="bcRegForm">');
        out.push('  <input type="email" class="bc-inp" id="bcRegEmailInp" value="' + esc(state.regEmailDraft || '') + '" placeholder="' + esc(L('Your email (e.g. alex@example.com)', '您的邮箱（如 alex@example.com）', 'Su correo electrónico')) + '" required>');
        out.push('  <input type="text" class="bc-inp" id="bcRegNameInp" value="' + esc(state.regNameDraft || '') + '" placeholder="' + esc(L('Display name (optional)', '显示名称（选填）', 'Nombre (opcional)')) + '">');
        out.push('  <button type="submit" class="bc-btn bc-btn-pri" id="bcRegSubmitBtn" style="padding:8px 14px;font-size:12.5px"' + (state.registeringUser ? ' disabled' : '') + '>' + esc(state.registeringUser ? L('Sending Code…', '发送验证码…', 'Enviando…') : L('Sign In / Register', '登录 / 注册', 'Entrar / Registrar')) + '</button>');
        out.push('</form>');
      }
      out.push('</div>');
      return out.join('');
    }

    function renderFunction2() {
      var ds = state.disclosuresState || {};
      var mailtoReq = ds.mailtoRequest || (state.property && state.property.materialRequestMailto) || buildFallbackMailto(state.property);
      var pubEq = ds.publicRecordEquivalents || (state.property && state.property.publicRecordEquivalents) || null;
      var rawPriv = ('privateUserAnalysis' in ds) ? ds.privateUserAnalysis : (ds.data || state.disclosures || null);
      var privAnalysis = unwrapDisclosureAnalysis(rawPriv);
      var hasPrivateCompleted = !!(ds.oneTimeCompleted || (rawPriv && rawPriv.oneTimeCompleted) || (privAnalysis && privAnalysis.priorityReading));
      var sharedRaw = ds.sharedWithMeAnalysis || ds.sharedDealTeamAnalysis || null;
      if (Array.isArray(sharedRaw)) sharedRaw = sharedRaw[0] || null;
      var sharedAnalysis = unwrapDisclosureAnalysis(sharedRaw);
      var authDisc = ds.authorizedDisclosures || null;
      var authAnalysis = unwrapDisclosureAnalysis(authDisc);
      var subTab = state.discSubTab || 'all';
      var retentionNotice = (ds.fileRetentionPolicy && ds.fileRetentionPolicy.notice) || L(
        'One-time private analysis: We analyze your uploaded files once in memory, store only your structured summary privately for your account/session, and immediately discard the original PDFs/.zip. We never save, host, or distribute your uploaded files.',
        '一次性私密解析：我们仅在内存中一次性解析您上传的文件，仅在您的账户/会话中私密保存结构化摘要，并立即销毁原始 PDF/.zip 文件。我们绝不保存、托管或向任何人分发您的原始文件。',
        'Análisis privado único: analizamos sus archivos una sola vez en memoria, guardamos solo su resumen estructurado de forma privada para su cuenta/sesión y descartamos de inmediato los PDF/.zip originales. Nunca guardamos, alojamos ni distribuimos sus archivos.'
      );

      var out = ['<div class="bc-work-panel" id="bcPanelFn2">'];
      out.push('<div class="bc-sec-hdr">');
      out.push('  <h4 class="bc-sec-title">2. ' + esc(L('Disclosure Priority Reader (1 → 6)', '披露文件优先级精读（按 1 → 6 顺序）', 'Lector Prioritario de Divulgaciones')) + '</h4>');
      out.push('  <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">');
      if (isSharingEnabled() && sharedAnalysis && sharedAnalysis.priorityReading) {
        out.push('    <span class="bc-badge bc-badge-sale" id="bcDealTeamBadge">🔒 ' + esc(L('Redacted Summary Shared With Your Account', '已接收来自注册协作者的脱敏摘要', 'Resumen Redactado Compartido con Su Cuenta')) + '</span>');
      }
      var localFileCount = state.pdfLocalFileCount || 0;
      if (!localFileCount && hasPdfStudioLocalFiles()) {
        try {
          var st = window.OpenEstatePdfStudio && typeof window.OpenEstatePdfStudio.getState === 'function'
            ? window.OpenEstatePdfStudio.getState()
            : null;
          if (st && Array.isArray(st.docs) && st.docs.length) localFileCount = st.docs.length;
        } catch (_) {}
      }
      var studioBtnLbl = L('Open Split-Screen PDF Studio & Annotator', '打开分屏 PDF 批注工作台', 'Abrir Estudio PDF en Pantalla Dividida');
      if (localFileCount > 0) {
        studioBtnLbl += L(
          ' (' + localFileCount + (localFileCount === 1 ? ' file' : ' files') + ')',
          '（' + localFileCount + ' 份文件）',
          ' (' + localFileCount + (localFileCount === 1 ? ' archivo' : ' archivos') + ')'
        );
      }
      out.push('    <button type="button" class="bc-btn bc-btn-ghost" id="bcOpenPdfStudioBtn" style="padding:7px 12px;font-size:12.5px">📄 ' + esc(studioBtnLbl) + '</button>');
      out.push('  </div>');
      out.push('</div>');

      // Explicit Legal / No-Storage Banner
      out.push('<div class="bc-legal-banner" id="bcFileRetentionBanner">');
      out.push('  <span style="font-size:18px;line-height:1.1">🔒</span>');
      out.push('  <div>');
      out.push('    <div style="font-weight:800;margin-bottom:2px">' + esc(L('Zero Original File Storage & Non-Distribution Policy', '原始文件零留存与绝不分发承诺', 'Política de Cero Almacenamiento y No Distribución')) + '</div>');
      out.push('    <div>' + esc(retentionNotice) + '</div>');
      out.push('  </div>');
      out.push('</div>');

      if (state.pdfLocalPrompt && !hasPdfStudioLocalFiles()) {
        out.push('<div class="bc-notice" id="bcPdfLocalPromptBanner" style="margin-bottom:12px">');
        out.push('  <div style="flex:1;min-width:220px">');
        out.push('    <div style="font-weight:800">' + esc(L('Select your local PDF or .zip to view & circle pages in-browser', '选择本地 PDF 或 .zip 文件以在浏览器端查看并圈注原文', 'Seleccione su PDF o .zip local para ver y señalar páginas')) + '</div>');
        out.push('    <div style="font-size:12px;font-weight:500;margin-top:2px">' + esc(L('Original files are never stored on our server. Pick your local copy to load it into PDF Studio memory and jump directly to the citation.', '原始文件从未保存于服务器。请选择您本地保存的报告文件以载入工作台并直接跳转定位。', 'Los archivos originales nunca se guardan en el servidor. Seleccione su copia local para cargarla en memoria.')) + '</div>');
        out.push('  </div>');
        out.push('  <label class="bc-btn bc-btn-pri" style="padding:7px 12px;font-size:12px;cursor:pointer">' + esc(L('Choose Local PDF / .ZIP', '选择本地 PDF / .ZIP', 'Elegir PDF / .ZIP Local')) + '<input type="file" id="bcPdfLocalLoadInp" multiple accept=".pdf,.zip,application/pdf,application/zip" style="display:none"></label>');
        out.push('</div>');
      }

      // Registered Account Bar (hosted sharing mode only)
      if (isSharingEnabled()) {
        out.push(renderRegUserBarHtml());
      }

      if (state.discNotice) {
        out.push('<div class="bc-notice" style="background:#ecfdf5;border-color:#6ee7b7;color:var(--bc-ok-fg);margin-bottom:12px"><span>' + esc(state.discNotice) + '</span></div>');
      }

      // Incoming Redacted Factual Summary shared with this Registered User (hosted sharing mode only)
      if (isSharingEnabled() && sharedRaw && sharedAnalysis && sharedAnalysis.priorityReading) {
        var privityText = sharedRaw.privityDisclaimer || sharedAnalysis.privityDisclaimer || L(
          'Factual findings only — original uploaded files were discarded, verbatim quotes are omitted, and sensitive PII is redacted. Sharing does not transfer inspector privity or replace independent professional due diligence.',
          '仅含结构化事实要点 — 原始文件已销毁，报告原文摘录已省略，敏感 PII 已脱敏。本摘要不构成验房师责任转移或替代专业尽职调查。',
          'Solo hallazgos fácticos — archivos originales descartados, texto literal omitido y PII redactada. No transfiere responsabilidad del inspector.'
        );
        out.push('<div class="bc-mode-box" id="bcSharedDealTeamCard" style="border-color:#c4b5fd;background:#faf5ff">');
        out.push('  <div class="bc-dealteam-banner">');
        out.push('    <span>🔒 ' + esc(L(
          'Redacted Summary Shared With Your Registered Account — Original Content Omitted & PII Redacted',
          '已向您的注册账户分享脱敏摘要 — 原文内容已省略且个人隐私信息已脱敏',
          'Resumen Redactado Compartido con Su Cuenta Registrada — Contenido Original Omitido y PII Redactada'
        )) + (sharedRaw.label ? (' · ' + esc(sharedRaw.label)) : '') + '</span>');
        out.push('    <span style="font-size:11.5px;font-weight:600">' +
          (sharedRaw.sharedByEmailMasked ? (esc(L('From: ', '分享自：', 'De: ')) + esc(sharedRaw.sharedByEmailMasked) + ' · ') : '') +
          esc(sharedRaw.createdAt || '') +
          '</span>');
        out.push('  </div>');
        out.push('  <div class="bc-privity-box" id="bcSharedPrivityDisclaimer"><strong>' + esc(L('Privity & Redaction Notice:', '脱敏与责任声明：', 'Aviso de Redacción y Privacidad:')) + '</strong> ' + esc(privityText) + '</div>');
        out.push(renderPriorityReadingHtml(sharedAnalysis, { offerBtnId: 'bcSendSharedRedFlagsToOfferBtn' }));
        out.push('</div>');
      }

      // Step 1 — mailto: Prefilled Disclosure / Inspection Material Request Card
      if (mailtoReq) {
        var curRecip = state.mailtoRecipient || mailtoReq.recipientEmail || '';
        var dynHref = buildMailtoHref(curRecip, mailtoReq.subject, mailtoReq.body);
        out.push('<div class="bc-mailto-card" id="bcMailtoRequestCard">');
        out.push('  <div style="display:flex;justify-content:space-between;align-items:flex-start;gap:10px;flex-wrap:wrap">');
        out.push('    <div>');
        out.push('      <div style="font-weight:800;font-size:14.5px;color:var(--bc-ink)">✉️ ' + esc(L('Step 1 — Need the disclosures or inspection reports? Draft an email request (mailto:)', '第 1 步 — 尚未拿到披露包或验房报告？一键生成请求邮件（mailto:）', 'Paso 1 — ¿Necesita las divulgaciones o inspecciones? Redacte un correo (mailto:)')) + '</div>');
        out.push('      <div style="font-size:12.5px;color:var(--bc-muted);margin-top:2px">' + esc(L('Opens directly in your own mail app with property address, APN, and the 6 priority reports prefilled — zero server SMTP needed.', '直接调用您本机邮箱客户端打开，已自动填好房屋地址、APN 及 6 类核心尽调文件清单——无需服务器代发。', 'Abre su aplicación de correo con dirección, APN y la lista de 6 informes prioritarios prellenados.')) + '</div>');
        out.push('    </div>');
        out.push('  </div>');
        out.push('  <div style="display:grid;grid-template-columns:1fr auto auto;gap:8px;align-items:end;margin-top:10px">');
        out.push('    <div>');
        out.push('      <label for="bcMailtoRecipientInp" style="display:block;font-size:11.5px;font-weight:700;color:var(--bc-ink2);margin-bottom:3px">' + esc(mailtoReq.recipientLabel || L('Recipient Email (optional — Listing Agent or Cooperating Agent)', '收件人邮箱（选填 — 挂牌经纪人或合作经纪人）', 'Correo del destinatario (opcional)')) + '</label>');
        out.push('      <input type="email" class="bc-inp" id="bcMailtoRecipientInp" value="' + esc(curRecip) + '" placeholder="agent@example.com">');
        out.push('    </div>');
        out.push('    <a class="bc-btn bc-btn-pri" id="bcMailtoOpenLink" href="' + esc(dynHref) + '" style="text-decoration:none">✉️ ' + esc(L('Open in Mail App (mailto:)', '在邮箱应用中打开 (mailto:)', 'Abrir en Correo (mailto:)')) + '</a>');
        out.push('    <button type="button" class="bc-btn bc-btn-sec" id="bcMailtoCopyBtn">' + esc(L('Copy Email Draft', '复制邮件草稿', 'Copiar Borrador')) + '</button>');
        out.push('  </div>');
        out.push('  <div class="bc-mailto-preview"><strong>' + esc(L('Subject:', '主题：', 'Asunto:')) + '</strong> ' + esc(mailtoReq.subject || '') + '\n\n' + esc(mailtoReq.body || '') + '</div>');
        out.push('  <div style="margin-top:8px;font-size:12.5px;font-weight:700;color:var(--bc-brand-fg)">' + esc(mailtoReq.uploadFollowUpPrompt || L(
          'Step 2: Once they reply with the .zip or PDF reports, upload them in ' + assistantBrandTitle() + ' below for your one-time private reading.',
          '第 2 步：收到对方回复的 .zip 压缩包或 PDF 报告后，请在下方 ' + assistantBrandTitle() + ' 上传以获取您的一次性私密精读。',
          'Paso 2: Cuando reciba los PDF o el paquete .zip, súbalos en ' + assistantBrandTitle() + ' abajo para su lectura privada única.'
        )) + '</div>');
        out.push('</div>');
      }

      // Disclosure Access Sub-Tabs (Listing/Seller Authorized shown only when hosted sharing is enabled)
      out.push('<div class="bc-disc-tabs" id="bcDiscTabs" role="tablist">');
      var discTabs = [
        { k: 'all', lbl: isSharingEnabled() ? L('All 3 Access Modes', '全部 3 种安全查阅方式', 'Los 3 Modos Seguros') : L('All Analysis Modes', '全部解析方式', 'Todos los Modos') },
        { k: 'public', lbl: L('1. Public-Record Baseline', '1. 政府公开数据基准', '1. Base Pública') },
        { k: 'private', lbl: L('2. My One-Time Private Upload', '2. 我的一次性私密上传', '2. Mi Carga Privada Única') + (hasPrivateCompleted ? ' ✓' : '') }
      ];
      if (isSharingEnabled()) {
        discTabs.push({ k: 'authorized', lbl: L('3. Listing / Seller Authorized', '3. 挂牌经纪/卖方授权披露', '3. Autorizado Vendedor/Agente') + (authDisc ? ' ✓' : '') });
      }
      discTabs.forEach(function (dt) {
        out.push('<button type="button" role="tab" aria-selected="' + (subTab === dt.k ? 'true' : 'false') + '" class="bc-disc-tab' + (subTab === dt.k ? ' act' : '') + '" data-disc-subtab="' + dt.k + '">' + esc(dt.lbl) + '</button>');
      });
      out.push('</div>');

      // Sub-Section 1: Public-Record Baseline
      if (subTab === 'all' || subTab === 'public') {
        out.push(renderPublicRecordBaselineHtml(pubEq));
      }

      // Sub-Section 2: My One-Time Private Upload (+ Registered-User Redacted Share in hosted sharing mode)
      if (subTab === 'all' || subTab === 'private') {
        out.push('<div class="bc-mode-box" id="bcPrivateUploadSection">');
        out.push('  <div style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap;margin-bottom:8px">');
        out.push('    <h5 style="margin:0;font-size:15px;font-weight:800;color:var(--bc-ink)">🔐 ' + esc(isSharingEnabled() ? L('2. My One-Time Private Upload & Registered-User Share', '2. 我的一次性私密上传与注册用户脱敏分享', '2. Mi Carga Privada Única y Compartir con Usuario Registrado') : L('2. My One-Time Private Upload (.ZIP / PDFs)', '2. 我的一次性私密上传（.ZIP / 多份 PDF）', '2. Mi Carga Privada Única (.ZIP / PDFs)')) + '</h5>');
        if (hasPrivateCompleted) {
          out.push('    <span class="bc-badge bc-badge-sale" id="bcOneTimeCompletedBadge">✓ ' + esc(L('One-time comprehensive analysis completed (original files discarded)', '已完成一次性综合分析（原始文件已销毁）', 'Análisis único completado (archivos descartados)')) + '</span>');
        }
        out.push('  </div>');

        if (hasPrivateCompleted && privAnalysis) {
          out.push('  <p style="font-size:12.5px;color:var(--bc-muted);margin:0 0 10px">' + esc(L(
            'Your structured reading is stored privately for your session. Re-uploading on this property is locked to protect your one-time allowance; original PDFs/.zip were immediately discarded.',
            '您的结构化审阅结果已私密保存至当前会话中。本套房源已完成一次性解析并锁定重复上传；原始 PDF/.zip 文件已立即销毁。',
            'Su lectura estructurada está guardada en privado para su sesión. Los archivos originales fueron descartados de inmediato.'
          )) + (rawPriv && rawPriv.analyzedAt ? (' · ' + esc(L('Analyzed:', '解析时间：', 'Analizado:')) + ' ' + esc(rawPriv.analyzedAt)) : '') + '</p>');

          // Registered-User Redacted Share Form (#bcShareDealTeamForm) — hosted sharing mode only
          if (isSharingEnabled()) {
            out.push('  <form class="bc-share-form" id="bcShareDealTeamForm">');
            out.push('    <div style="font-size:13.5px;font-weight:800;color:var(--bc-ok-fg)">' + esc(L('Share Redacted Factual Summary with a Verified Transaction Party', '向同交易已验证协作者分享脱敏事实摘要', 'Compartir Resumen Fáctico Redactado con Parte de la Transacción')) + '</div>');
            out.push('    <div style="font-size:12px;color:var(--bc-ok-fg);margin:4px 0 9px;line-height:1.4">' + esc(L(
              'Zero original document distribution: Original PDFs/.zip were immediately discarded, verbatim prose is stripped, and SSN/ITIN/EIN, phone numbers, emails, loan/escrow/routing/account numbers, and personal mailing addresses are scrubbed. Informational/non-reliance scope only — no inspector, title, DRE, or legal privity transfers.',
              '原始文件零分发：上传的原始 PDF/.zip 已立即销毁，原文摘录已剥离，SSN/税号、电话、邮箱、贷款/托管/路由/银行账号及个人邮寄地址均已自动脱敏。仅供信息参考且不可替代独立尽调，不转移验房师、产权或法律合同关系（Privity）。',
              'Cero distribución de documentos originales: los PDF/.zip originales se descartaron de inmediato, se omitió el texto literal y se redactaron SSN/EIN, teléfonos, correos, números de cuenta/préstamo y direcciones personales. Alcance estrictamente informativo sin transferencia de responsabilidad.'
            )) + '</div>');
            out.push('    <label style="display:flex;align-items:flex-start;gap:8px;font-size:12px;font-weight:600;color:var(--bc-ok-fg);margin:0 0 9px;cursor:pointer"><input type="checkbox" id="bcShareAttestChk"' + (state.shareAttestedDraft ? ' checked' : '') + ' style="margin-top:2px"> <span>' + esc(L(
              'I attest that the recipient is a Principal (buyer/seller), Licensed Real Estate Agent/Broker, or Licensed Attorney actively participating in this same property transaction; zero original documents are distributed, PII is redacted, and shared findings are strictly informational/non-reliance.',
              '本人郑重声明：接收方系参与本套房产同一交易的当事人（买方/卖方）、持牌房地产经纪人或执业律师；确认零原始文件分发、个人隐私已脱敏，且分享的事实要点仅供非依赖性信息参考。',
              'Certifico que el destinatario es Parte Principal, Agente Inmobiliario con Licencia o Abogado en esta misma transacción; cero documentos originales se distribuyen y el resumen es solo informativo.'
            )) + '</span></label>');
            out.push('    <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center">');
            out.push('      <input type="email" class="bc-inp" id="bcShareRecipientEmailInp" value="' + esc(state.shareRecipientDraft || '') + '" placeholder="' + esc(L("Collaborator's registered email", '协作者的注册邮箱地址', 'Correo registrado del colaborador')) + '" style="flex:1;min-width:220px" required>');
            out.push('      <button type="submit" class="bc-btn bc-btn-pri" id="bcShareDealTeamBtn"' + (state.sharingDealTeam ? ' disabled' : '') + '>' + esc(state.sharingDealTeam ? L('Sharing Redacted Summary…', '正在脱敏分享…', 'Compartiendo…') : L('Share Redacted Result', '分享脱敏结果', 'Compartir Resultado Redactado')) + '</button>');
            out.push('    </div>');
            if (!state.regUser && !getStoredUser()) {
              out.push('    <div style="margin-top:6px;font-size:11.5px;color:var(--bc-warn-fg);font-weight:600">' + esc(L('Note: Sign in or register in the Account bar above first to share with another registered account.', '提示：请先在上方注册账户栏登录，然后再向对方注册账户发送脱敏摘要。', 'Nota: Inicie sesión o regístrese arriba primero para poder compartir.')) + '</div>');
            }
            if (state.shareResult && state.shareResult.ok) {
              out.push('    <div class="bc-share-box" id="bcShareResultBox">');
              out.push('      <div style="font-size:12.5px;font-weight:800;color:var(--bc-ok-fg)">✓ ' + esc(L(
                'Redacted factual summary shared with ' + (state.shareResult.recipientEmail || 'registered user') + '. Original files were discarded, verbatim quotes omitted, and sensitive PII redacted.',
                '已向注册用户 ' + (state.shareResult.recipientEmail || '') + ' 分享脱敏事实摘要（原始文件已销毁、原文摘录已省略、敏感 PII 已脱敏）。',
                'Resumen fáctico redactado compartido con ' + (state.shareResult.recipientEmail || 'usuario registrado') + '. Archivos originales descartados y PII redactada.'
              )) + '</div>');
              out.push('    </div>');
            }
            out.push('  </form>');
          }

          out.push(renderPriorityReadingHtml(privAnalysis, { offerBtnId: 'bcSendRedFlagsToOfferBtn' }));
        } else {
          out.push('  <p style="font-size:13px;color:var(--bc-muted);margin:0 0 12px">' + esc(L(
            'Export the disclosure packet (.zip or PDFs) from your portal (e.g., HomeLight "Download All") and drop it below for your one-time private reading. Documents are read in strict priority order: 1) Home/Roof Inspection, 2) Termite, 3) Prelim Title, 4) NHD, 5) HOA Financials, 6) Permits.',
            '从披露门户（如 HomeLight "Download All"）下载 .zip 压缩包或多份 PDF 拖入下方完成一次性私密精读。系统严格按 1) 验房/屋顶、2) 白蚁、3) 初步产权 Prelim、4) 自然灾害 NHD、5) HOA 财务、6) 施工许可 Permits 顺序审阅。',
            'Arrastre el paquete .zip o PDFs para su lectura privada única en orden estricto 1→6.'
          )) + '</p>');
          out.push('  <div class="bc-dropzone" id="bcDiscDropzone">');
          out.push('    <input type="file" id="bcDiscFileInput" multiple accept=".pdf,.zip,application/pdf,application/zip" style="display:none">');
          if (state.uploadingDisclosures) {
            out.push('    <div style="font-weight:800;color:var(--bc-brand-fg);font-size:14.5px">' + esc(L('Extracting & reading disclosures in priority order…', '正在提取并按 1→6 优先级审阅披露文件…', 'Analizando documentos por prioridad…')) + '</div>');
          } else {
            out.push('    <div style="font-weight:800;color:var(--bc-brand-fg);font-size:14.5px">' + esc(L('Drop Disclosure .ZIP or PDF Reports Here (or Click to Browse)', '拖拽披露包 .ZIP 或多份 PDF 到此处（或点击选择文件）', 'Arrastre el .ZIP o PDFs aquí (o haga clic)')) + '</div>');
            out.push('    <div style="font-size:12px;color:var(--bc-subtle);margin-top:4px">' + esc(L('One-time comprehensive analysis per session · Up to 20 PDFs or 1 .zip · Originals discarded immediately', '每房源每会话限一次性全面审阅 · 最多 20 份 PDF 或 1 个 .zip · 原始文件解析后立即销毁', 'Análisis único por sesión · Hasta 20 PDFs o 1 .zip · Originales descartados de inmediato')) + '</div>');
          }
          out.push('  </div>');
        }
        out.push('</div>');
      }

      // Sub-Section 3: Listing Agent / Seller Authorized Upload (hosted sharing mode only)
      if (isSharingEnabled() && (subTab === 'all' || subTab === 'authorized')) {
        out.push('<div class="bc-mode-box" id="bcAuthorizedSection" style="margin-bottom:0">');
        out.push('  <div style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap;margin-bottom:8px">');
        out.push('    <h5 style="margin:0;font-size:15px;font-weight:800;color:var(--bc-ink)">🏢 ' + esc(L('3. Listing Agent / Seller Authorized Disclosures', '3. 挂牌经纪人 / 卖方授权披露摘要', '3. Divulgaciones Autorizadas por Agente Listante / Vendedor')) + '</h5>');
        if (authDisc) {
          var roleBadgeLbl = authDisc.uploaderRole === 'seller'
            ? L('Verified Seller Upload', '卖方授权发布', 'Publicado por Vendedor')
            : L('Verified Listing Agent Upload', '挂牌经纪人授权发布', 'Publicado por Agente Listante');
          out.push('    <span class="bc-badge bc-badge-sale" id="bcAuthVerifiedBadge">✓ ' + esc(roleBadgeLbl) + '</span>');
        }
        out.push('  </div>');

        if (authDisc) {
          out.push('  <div class="bc-auth-banner" id="bcAuthorizedMetaCard">');
          out.push('    <div>');
          out.push('      <div style="font-weight:800;font-size:13.5px">' +
            esc(authDisc.uploaderRole === 'seller' ? L('Seller', '卖方/房主', 'Vendedor') : L('Listing Agent', '挂牌经纪人', 'Agente Listante')) +
            ': ' + esc(authDisc.signerName || '—') +
            (authDisc.licenseNumber ? (' · Lic #' + esc(authDisc.licenseNumber)) : '') +
            (authDisc.revision != null ? (' · Rev #' + esc(authDisc.revision)) : '') +
            '</div>');
          if (authDisc.publishedAt) {
            out.push('      <div style="font-size:12px;margin-top:2px">' + esc(L('Published:', '发布时间：', 'Publicado:')) + ' ' + esc(authDisc.publishedAt) + '</div>');
          }
          if (authDisc.attestation) {
            out.push('      <div style="font-size:12px;margin-top:4px;opacity:.92">' + esc(authDisc.attestation) + '</div>');
          }
          out.push('    </div>');
          if (authAnalysis && authAnalysis.priorityReading) {
            out.push('    <button type="button" class="bc-btn bc-btn-sec" id="bcToggleAuthReadingBtn" style="padding:7px 12px;font-size:12px">' +
              esc((state.authViewingReading || !hasPrivateCompleted) ? L('Hide Authorized Summary', '收起授权披露摘要', 'Ocultar Resumen') : L('View Authorized 1→6 Reading', '查看授权 1→6 精读', 'Ver Lectura 1→6')) +
              '</button>');
          }
          out.push('  </div>');
          if (authAnalysis && authAnalysis.priorityReading && (state.authViewingReading || !hasPrivateCompleted)) {
            out.push(renderPriorityReadingHtml(authAnalysis, { offerBtnId: 'bcSendAuthRedFlagsToOfferBtn' }));
          }
        } else {
          out.push('  <p style="font-size:12.5px;color:var(--bc-muted);margin:0 0 10px">' + esc(L(
            'No authorized disclosure summary has been published by the listing agent or seller for this property yet.',
            '挂牌经纪人或卖方尚未发布本房源的公开授权披露摘要。',
            'El agente listante o vendedor aún no ha publicado un resumen autorizado para esta propiedad.'
          )) + '</p>');
        }

        // Collapsible Listing Agent / Seller Publish Form
        var ad = state.authFormDraft || {};
        var isSellerRole = ad.uploaderRole === 'seller';
        out.push('  <details id="bcAuthorizedPublishDetails" style="margin-top:10px;border:1px solid var(--bc-line-strong);border-radius:11px;padding:10px 13px;background:var(--bc-soft)">');
        out.push('    <summary style="cursor:pointer;font-weight:800;font-size:13px;color:var(--bc-brand-fg)">' + esc(L(
          'Are you the Listing Agent or Seller? Publish / Update Authorized Disclosure Summary',
          '您是挂牌经纪人或房主？发布 / 更新授权披露摘要',
          '¿Es el Agente Listante o Vendedor? Publicar / Actualizar Resumen Autorizado'
        )) + '</summary>');
        out.push('    <form id="bcAuthUploadForm" style="margin-top:12px">');
        out.push('      <div class="bc-row2">');
        out.push('        <div class="bc-field"><label for="bcAuthRoleSel">' + esc(L('Role *', '发布方身份 *', 'Rol *')) + '</label><select class="bc-sel" id="bcAuthRoleSel" name="uploaderRole">' +
          '<option value="listing_agent"' + (!isSellerRole ? ' selected' : '') + '>' + esc(L('Listing Agent', '挂牌经纪人 (Listing Agent)', 'Agente Listante')) + '</option>' +
          '<option value="seller"' + (isSellerRole ? ' selected' : '') + '>' + esc(L('Seller / Property Owner', '卖方 / 房主 (Seller)', 'Vendedor / Propietario')) + '</option>' +
          '</select></div>');
        out.push('        <div class="bc-field"><label for="bcAuthSignerName">' + esc(L('Full Legal Name *', '签署人姓名 *', 'Nombre Completo *')) + '</label><input type="text" class="bc-inp" id="bcAuthSignerName" name="signerName" value="' + esc(ad.signerName || '') + '" placeholder="Jane Doe" required></div>');
        out.push('      </div>');
        out.push('      <div class="bc-row2">');
        out.push('        <div class="bc-field" id="bcAuthLicenseWrap" style="' + (isSellerRole ? 'display:none' : '') + '"><label for="bcAuthLicenseInp">' + esc(L('Real Estate License # *', '房产经纪执照号 *', 'Número de Licencia *')) + '</label><input type="text" class="bc-inp" id="bcAuthLicenseInp" name="licenseNumber" value="' + esc(ad.licenseNumber || '') + '" placeholder="e.g. CA DRE #01999999"' + (!isSellerRole ? ' required' : '') + '></div>');
        out.push('        <div class="bc-field"><label for="bcAuthSignerEmail">' + esc(L('Contact Email (optional)', '联系邮箱（选填）', 'Correo de Contacto (opcional)')) + '</label><input type="email" class="bc-inp" id="bcAuthSignerEmail" name="signerEmail" value="' + esc(ad.signerEmail || '') + '" placeholder="listing@example.com"></div>');
        out.push('      </div>');
        out.push('      <div class="bc-field"><label for="bcAuthFileInput">' + esc(L('Disclosure .ZIP or PDF Reports *', '选择披露包 .ZIP 或多份 PDF 文件 *', 'Paquete .ZIP o PDFs de Divulgación *')) + '</label><input type="file" class="bc-inp" id="bcAuthFileInput" multiple accept=".pdf,.zip,application/pdf,application/zip"></div>');
        out.push('      <div class="bc-field"><label style="display:flex;align-items:flex-start;gap:8px;font-weight:600;cursor:pointer"><input type="checkbox" id="bcAuthConsentChk" name="authorizedToDistribute"' + (ad.authorizedToDistribute ? ' checked' : '') + ' style="margin-top:3px"> <span>' + esc(L(
          'I am the listing agent or property owner authorized to distribute these disclosures to prospective buyers; I understand original files are discarded immediately after analysis.',
          '本人确认系该房源的授权挂牌经纪人或产权人，有权向潜在买家分发上述披露信息；本人知悉原始文件将在解析后立即销毁。',
          'Soy el agente listante o propietario autorizado para distribuir estas divulgaciones a posibles compradores; entiendo que los archivos originales se descartan de inmediato.'
        )) + '</span></label></div>');
        out.push('      <button type="submit" class="bc-btn bc-btn-pri" id="bcAuthSubmitBtn"' + (state.uploadingAuthorized ? ' disabled' : '') + '>' + esc(state.uploadingAuthorized ? L('Analyzing & Publishing…', '正在解析并发布…', 'Publicando…') : L('Publish Authorized Summary (Discard Original Files)', '发布授权披露摘要（立即销毁原文件）', 'Publicar Resumen Autorizado')) + '</button>');
        out.push('    </form>');
        out.push('  </details>');
        out.push('</div>');
      }

      out.push('</div>');
      return out.join('');
    }

    function payerOptionsHtml(selectedVal) {
      var opts = [
        { v: 'seller', l: L('Seller pays 100%', '卖方承担 100%', 'Vendedor 100%') },
        { v: 'buyer', l: L('Buyer pays 100%', '买方承担 100%', 'Comprador 100%') },
        { v: 'split_50_50', l: L('Split 50% / 50%', '买卖双方 50/50 分摊', '50% / 50%') }
      ];
      return opts.map(function (o) {
        return '<option value="' + o.v + '"' + (selectedVal === o.v ? ' selected' : '') + '>' + esc(o.l) + '</option>';
      }).join('');
    }

    function renderFunction3() {
      var out = ['<div class="bc-work-panel" id="bcPanelFn3">'];
      out.push('<div class="bc-sec-hdr">');
      out.push('  <h4 class="bc-sec-title">3. ' + esc(L('Guided Offer Builder (Non-Binding Review Summary)', '引导式出价单生成器（仅供买方与经纪人审阅，非签署合同）', 'Constructor Guiado de Oferta (No Vinculante)')) + '</h4>');
      out.push('</div>');

      if (!state.offerEval) {
        out.push('<p style="font-size:13.5px;color:var(--bc-muted)">' + esc(L('Initializing jurisdiction tax & closing schedule…', '正在加载当地过户税费与产权惯例默认配置…', 'Cargando costos locales…')) + '</p></div>');
        return out.join('');
      }

      var ev = state.offerEval;
      var d = ev.draft || {};
      var ctx = ev.context || {};
      var jd = ctx.jurisdictionDefaults || {};
      var rs = ev.runningSummary || {};
      var wpw = rs.whoPaysWhat || {};
      var done = Array.isArray(d.completedSteps) ? d.completedSteps : [];
      var step = state.offerStep || 1;

      var stepLabels = [
        { n: 1, l: L('1. Offer Price', '1. 出价金额', '1. Precio') },
        { n: 2, l: L('2. Downpayment & Loan', '2. 首付与贷款', '2. Enganche y Préstamo') },
        { n: 3, l: L('3. Close of Escrow', '3. 过户周期 COE', '3. Cierre COE') },
        { n: 4, l: L('4. Contingencies', '4. 保护期条款', '4. Contingencias') },
        { n: 5, l: L('5. Who Pays What', '5. 费用分摊与佣金', '5. Quién Paga Qué') },
        { n: 6, l: L('6. Repairs & Other', '6. 维修与其他条款', '6. Reparaciones y Otros') }
      ];

      out.push('<div class="bc-steps">');
      stepLabels.forEach(function (sl) {
        var cls = 'bc-step-pill' + (step === sl.n ? ' act' : (done.indexOf(sl.n) >= 0 ? ' done' : ''));
        out.push('<button type="button" class="' + cls + '" data-offer-step="' + sl.n + '">' + esc(sl.l) + '</button>');
      });
      out.push('</div>');

      out.push('<div class="bc-offer-grid">');

      // Left column: Active Step Guided Input Group
      out.push('  <form id="bcOfferStepForm" class="bc-subcard" style="margin-top:0;background:var(--bc-surface)">');
      if (step === 1) {
        out.push('<h5 style="margin:0 0 10px;font-size:15px;font-weight:800">' + esc(L('Group 1: What price do you want to offer?', '第 1 组：您计划出价多少？', 'Grupo 1: ¿Qué precio desea ofertar?')) + '</h5>');
        if (ctx.listPriceUsd != null || (ctx.compEstimateRangeUsd && ctx.compEstimateRangeUsd.low != null)) {
          out.push('<div style="font-size:12.5px;color:var(--bc-brand-fg);background:var(--bc-brandsoft);border:1px solid var(--bc-brand-bd);border-radius:9px;padding:8px 10px;margin-bottom:12px">');
          if (ctx.listPriceUsd != null) out.push('<div>• ' + esc(L('Current List Price:', '当前挂牌价：', 'Precio de Lista:')) + ' <strong>' + esc(fmtUsd(ctx.listPriceUsd)) + '</strong></div>');
          if (ctx.compEstimateRangeUsd && ctx.compEstimateRangeUsd.low != null) {
            out.push('<div>• ' + esc(L('Function 1 Comp Range:', '模块1可比估价区间：', 'Rango Comparable:')) + ' <strong>' + esc(fmtUsd(ctx.compEstimateRangeUsd.low)) + ' – ' + esc(fmtUsd(ctx.compEstimateRangeUsd.high)) + '</strong></div>');
          }
          out.push('</div>');
        }
        out.push('<div class="bc-field"><label>' + esc(L('Offer Price (USD)', '出价金额 (USD)', 'Precio de Oferta (USD)')) + '</label><input type="number" class="bc-inp" name="offerPriceUsd" min="1000" step="1000" value="' + esc(d.offerPriceUsd != null ? d.offerPriceUsd : '') + '" placeholder="e.g. 1250000" required></div>');
      } else if (step === 2) {
        out.push('<h5 style="margin:0 0 10px;font-size:15px;font-weight:800">' + esc(L('Group 2: Downpayment, Loan Split & Earnest Deposit', '第 2 组：首付比例、贷款比例与定金', 'Grupo 2: Enganche y Préstamo')) + '</h5>');
        out.push('<div class="bc-field"><label>' + esc(L('Financing Type', '贷款/付款类型', 'Tipo de Financiamiento')) + '</label><select class="bc-sel" name="financingType" id="bcFinTypeSel">' +
          '<option value="conventional"' + (d.financingType === 'conventional' ? ' selected' : '') + '>Conventional Loan</option>' +
          '<option value="fha"' + (d.financingType === 'fha' ? ' selected' : '') + '>FHA Loan</option>' +
          '<option value="va"' + (d.financingType === 'va' ? ' selected' : '') + '>VA Loan</option>' +
          '<option value="cash"' + (d.financingType === 'cash' ? ' selected' : '') + '>All-Cash (100% Down)</option>' +
          '</select></div>');
        out.push('<div class="bc-row2">');
        out.push('  <div class="bc-field"><label>' + esc(L('Downpayment (%)', '首付比例 (%)', 'Enganche (%)')) + '</label><input type="number" class="bc-inp" name="downPaymentPct" id="bcDpPctInp" min="0" max="100" step="0.5" value="' + esc(d.downPaymentPct) + '"></div>');
        out.push('  <div class="bc-field"><label>' + esc(L('Loan (%)', '贷款比例 (%)', 'Préstamo (%)')) + '</label><input type="number" class="bc-inp" id="bcLoanPctMirror" disabled value="' + esc(d.loanPct) + '"></div>');
        out.push('</div>');
        out.push('<div class="bc-field"><label>' + esc(L('Earnest Money Deposit (%)', '诚意金 EMD (%)', 'Depósito de Garantía (%)')) + '</label><input type="number" class="bc-inp" name="earnestMoneyPct" min="0" max="25" step="0.25" value="' + esc(d.earnestMoneyPct) + '"></div>');
      } else if (step === 3) {
        out.push('<h5 style="margin:0 0 10px;font-size:15px;font-weight:800">' + esc(L('Group 3: Close of Escrow (COE) Timeline', '第 3 组：过户交割周期（COE）', 'Grupo 3: Plazo de Cierre (COE)')) + '</h5>');
        out.push('<div class="bc-row2">');
        out.push('  <div class="bc-field"><label>' + esc(L('Days to Close (COE)', '过户天数（天）', 'Días para Cerrar')) + '</label><input type="number" class="bc-inp" name="coeDays" min="3" max="365" value="' + esc(d.coeDays) + '"></div>');
        out.push('  <div class="bc-field"><label>' + esc(L('Target Close Date (Optional)', '目标过户日（选填）', 'Fecha Objetivo (Opcional)')) + '</label><input type="date" class="bc-inp" name="targetCloseDate" value="' + esc(d.targetCloseDate || '') + '"></div>');
        out.push('</div>');
      } else if (step === 4) {
        out.push('<h5 style="margin:0 0 10px;font-size:15px;font-weight:800">' + esc(L('Group 4: Contract Contingencies & Rent-Back', '第 4 组：合同保护期条款（Contingencies）与返租', 'Grupo 4: Contingencias y Rent-Back')) + '</h5>');
        out.push('<div class="bc-row2"><div class="bc-field"><label><input type="checkbox" name="loanContingencyEnabled"' + (d.loanContingencyEnabled ? ' checked' : '') + '> ' + esc(L('Loan Contingency', '贷款保护期', 'Contingencia de Préstamo')) + '</label><input type="number" class="bc-inp" name="loanContingencyDays" min="0" max="120" value="' + esc(d.loanContingencyDays) + '" placeholder="Days"></div>');
        out.push('<div class="bc-field"><label><input type="checkbox" name="inspectionContingencyEnabled"' + (d.inspectionContingencyEnabled ? ' checked' : '') + '> ' + esc(L('Inspection Contingency', '验房保护期', 'Contingencia de Inspección')) + '</label><input type="number" class="bc-inp" name="inspectionContingencyDays" min="0" max="90" value="' + esc(d.inspectionContingencyDays) + '" placeholder="Days"></div></div>');
        out.push('<div class="bc-field"><label><input type="checkbox" name="appraisalContingencyEnabled"' + (d.appraisalContingencyEnabled ? ' checked' : '') + '> ' + esc(L('Appraisal Contingency (Days)', '估价保护期（天）', 'Contingencia de Tasación')) + '</label><input type="number" class="bc-inp" name="appraisalContingencyDays" min="0" max="90" value="' + esc(d.appraisalContingencyDays) + '"></div>');
        out.push('<div class="bc-field" style="padding-top:6px;border-top:1px solid var(--bc-line)"><label><input type="checkbox" name="rentBackEnabled"' + (d.rentBackEnabled ? ' checked' : '') + '> ' + esc(L('Seller Rent-Back After Closing', '卖家过户后返租（Rent-Back）', 'Rent-Back del Vendedor')) + '</label>');
        out.push('<div class="bc-row2" style="margin-top:6px"><div><label style="font-size:11.5px">Rent-Back Days</label><input type="number" class="bc-inp" name="rentBackDays" min="0" max="180" value="' + esc(d.rentBackDays) + '"></div><div><label style="font-size:11.5px">Security Deposit ($)</label><input type="number" class="bc-inp" name="rentBackDepositUsd" min="0" step="250" value="' + esc(d.rentBackDepositUsd) + '"></div></div></div>');
        out.push('<div class="bc-field" style="padding-top:6px;border-top:1px solid var(--bc-line)"><label><input type="checkbox" name="saleOfPropertyContingencyEnabled"' + (d.saleOfPropertyContingencyEnabled ? ' checked' : '') + '> ' + esc(L('Contingent on Sale of Buyer’s Existing Property', '买方先卖房保护条款（Sale of Buyer’s Property）', 'Contingente a Venta de Propiedad del Comprador')) + '</label>');
        out.push('<div class="bc-row2" style="margin-top:6px"><input type="text" class="bc-inp" name="saleOfPropertyAddress" placeholder="Buyer property address" value="' + esc(d.saleOfPropertyAddress || '') + '"><input type="number" class="bc-inp" name="saleOfPropertyDays" min="0" max="180" value="' + esc(d.saleOfPropertyDays) + '" placeholder="Days"></div></div>');
      } else if (step === 5) {
        out.push('<h5 style="margin:0 0 6px;font-size:15px;font-weight:800">' + esc(L('Group 5: Who Pays What (Editable Commission & Local Tax/Fee Lookup)', '第 5 组：费用分摊（可修改买方佣金 + 当地税费惯例查表）', 'Grupo 5: Quién Paga Qué')) + '</h5>');
        out.push('<div style="font-size:11.8px;color:var(--bc-muted);background:var(--bc-soft);border:1px solid var(--bc-line-strong);border-radius:8px;padding:8px 10px;margin-bottom:8px"><strong>' + esc(jd.regionName || 'Local Custom') + ':</strong> ' + esc(jd.titleEscrowCustomNote || '') + ' ' + esc(jd.transferTaxCustomNote || '') + '</div>');
        out.push('<div id="bcPostNarStepNotice" style="font-size:11.5px;color:var(--bc-info-fg);background:var(--bc-info-bg);border:1px solid var(--bc-info-bd);border-radius:8px;padding:7px 10px;margin-bottom:10px">' + esc(jd.postNarSettlementNotice || L(
          'Post-NAR Settlement Notice: Buyer-broker compensation is 100% negotiable, never fixed by law, and MLS-participating buyer agents generally require a written Buyer-Broker Representation Agreement before touring.',
          'NAR 和解新规提示：买方经纪佣金 100% 可协商、绝非法定固定，合作经纪人带看前通常须签署书面《买方经纪代理协议》。',
          'Aviso Post-NAR: La comisión del agente comprador es 100% negociable y requiere un acuerdo escrito de representación antes de visitar propiedades.'
        )) + '</div>');
        out.push('<div class="bc-row2"><div class="bc-field"><label>' + esc(L('Buyer Agent Commission % (Negotiable)', '买方经纪人佣金 %（可协商修改）', 'Comisión Agente Comprador %')) + '</label><input type="number" class="bc-inp" name="buyerAgentCommissionPct" min="0" max="10" step="0.05" value="' + esc(d.buyerAgentCommissionPct) + '"></div>');
        out.push('<div class="bc-field"><label>' + esc(L('Commission Paid By', '佣金承担方', 'Comisión Pagada Por')) + '</label><select class="bc-sel" name="buyerAgentCommissionPayer">' + payerOptionsHtml(d.buyerAgentCommissionPayer) + '</select></div></div>');
        out.push('<div class="bc-row2"><div class="bc-field"><label>' + esc(L('City Transfer Tax', '市级过户税（City Transfer Tax）', 'Impuesto Municipal')) + ' (' + esc(fmtUsd(wpw.cityTransferTax && wpw.cityTransferTax.totalUsd)) + ')</label><select class="bc-sel" name="cityTransferTaxPayer">' + payerOptionsHtml(d.cityTransferTaxPayer) + '</select></div>');
        out.push('<div class="bc-field"><label>' + esc(L('County/State Transfer Tax', '县/州过户税（County Transfer Tax）', 'Impuesto del Condado')) + ' (' + esc(fmtUsd(wpw.countyTransferTax && wpw.countyTransferTax.totalUsd)) + ')</label><select class="bc-sel" name="countyTransferTaxPayer">' + payerOptionsHtml(d.countyTransferTaxPayer) + '</select></div></div>');
        out.push('<div class="bc-row2"><div class="bc-field"><label>' + esc(L('Escrow / Settlement Fee', '过户托管费（Escrow Fee）', 'Tarifa de Escrow')) + '</label><select class="bc-sel" name="escrowFeePayer">' + payerOptionsHtml(d.escrowFeePayer) + '</select></div>');
        out.push('<div class="bc-field"><label>' + esc(L('Owner’s Title Insurance', '业主产权保险（Title Insurance）', 'Seguro de Título')) + '</label><select class="bc-sel" name="ownersTitlePayer">' + payerOptionsHtml(d.ownersTitlePayer) + '</select></div></div>');
        out.push('<div class="bc-field" style="padding-top:6px;border-top:1px solid var(--bc-line)"><label><input type="checkbox" name="homeWarrantyEnabled"' + (d.homeWarrantyEnabled ? ' checked' : '') + '> ' + esc(L('Home Warranty Plan', '房屋保修计划（Home Warranty）', 'Garantía Residencial')) + '</label>');
        out.push('<div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:8px;margin-top:6px"><div><label style="font-size:11px">Months</label><input type="number" class="bc-inp" name="homeWarrantyMonths" min="0" max="60" value="' + esc(d.homeWarrantyMonths) + '"></div><div><label style="font-size:11px">Max Cost ($)</label><input type="number" class="bc-inp" name="homeWarrantyMaxUsd" min="0" step="50" value="' + esc(d.homeWarrantyMaxUsd) + '"></div><div><label style="font-size:11px">Paid By</label><select class="bc-sel" name="homeWarrantyPayer"><option value="seller"' + (d.homeWarrantyPayer === 'seller' ? ' selected' : '') + '>Seller</option><option value="buyer"' + (d.homeWarrantyPayer === 'buyer' ? ' selected' : '') + '>Buyer</option></select></div></div></div>');
      } else {
        out.push('<h5 style="margin:0 0 10px;font-size:15px;font-weight:800">' + esc(L('Group 6: Seller Repairs, Credits & Other Input', '第 6 组：卖方维修要求、过户抵扣与其他条款', 'Grupo 6: Reparaciones y Otros Términos')) + '</h5>');
        if (Array.isArray(ctx.SuggestedDisclosureRedFlags) && ctx.SuggestedDisclosureRedFlags.length) {
          out.push('<div style="margin-bottom:10px"><button type="button" class="bc-btn bc-btn-ghost" id="bcImportDiscBtn" style="padding:6px 11px;font-size:12px">' + esc(L('Import Red Flags from Disclosures Reader', '一键导入披露审阅红旗项', 'Importar Banderas Rojas')) + '</button></div>');
        }
        out.push('<div class="bc-field"><label>' + esc(L('Seller Credit Ask at Closing ($)', '要求卖方过户返现抵扣（Seller Credit $）', 'Crédito del Vendedor ($)')) + '</label><input type="number" class="bc-inp" name="sellerCreditUsd" min="0" step="500" value="' + esc(d.sellerCreditUsd || 0) + '"></div>');
        out.push('<div class="bc-field"><label>' + esc(L('Seller Repair Requests', '要求卖方维修事项', 'Solicitudes de Reparación')) + '</label><textarea class="bc-ta" name="sellerRepairs" placeholder="e.g., Section 1 termite clearance prior to close…">' + esc(d.sellerRepairs || '') + '</textarea></div>');
        out.push('<div class="bc-field"><label>' + esc(L('Other Terms / Included Appliances', '其他附加条款 / 包含家电设施', 'Otros Términos')) + '</label><textarea class="bc-ta" name="otherTerms" placeholder="e.g., Washer, dryer, and kitchen refrigerator included…">' + esc(d.otherTerms || '') + '</textarea></div>');
      }

      out.push('<div style="display:flex;justify-content:space-between;gap:8px;margin-top:14px">');
      if (step > 1) {
        out.push('<button type="button" class="bc-btn bc-btn-sec" id="bcOfferPrevBtn">← ' + esc(L('Previous', '上一步', 'Anterior')) + '</button>');
      } else {
        out.push('<span></span>');
      }
      out.push('<button type="submit" class="bc-btn bc-btn-pri">' + esc(step < 6 ? L('Save & Next Step →', '保存并下一步 →', 'Guardar y Siguiente →') : L('Save Final Offer Summary ✓', '保存最终出价摘要 ✓', 'Guardar Resumen Final ✓')) + '</button>');
      out.push('</div>');
      out.push('  </form>');

      // Right column: Live Editable Running Summary
      out.push('  <div class="bc-summary-box">');
      out.push('    <div style="display:flex;justify-content:space-between;align-items:center;gap:8px;margin-bottom:8px">');
      out.push('      <strong style="font-size:14.5px;color:var(--bc-ink)">' + esc(L('Running Offer Summary (Click any step to edit)', '实时出价汇总（点击任一项可随时修改）', 'Resumen en Vivo (Editable)')) + '</strong>');
      out.push('    </div>');
      out.push('    <div class="bc-sum-row"><span>1. ' + esc(L('Offer Price', '出价金额', 'Precio')) + '</span><span><strong>' + esc(rs.offerPriceUsd != null ? fmtUsd(rs.offerPriceUsd) : L('Not set', '待输入', 'Sin definir')) + '</strong><button type="button" class="bc-edit-link" data-offer-step="1">Edit</button></span></div>');
      out.push('    <div class="bc-sum-row"><span>2. ' + esc(L('Downpayment / Loan', '首付 / 贷款', 'Enganche / Préstamo')) + '</span><span>' + esc(rs.downPaymentPct + '% (' + fmtUsd(rs.downPaymentUsd) + ') / ' + rs.loanPct + '% ' + String(rs.financingType || '').toUpperCase()) + '<button type="button" class="bc-edit-link" data-offer-step="2">Edit</button></span></div>');
      out.push('    <div class="bc-sum-row"><span>3. ' + esc(L('Close of Escrow', '过户周期 COE', 'Cierre COE')) + '</span><span>' + esc(rs.coeDays + ' days' + (rs.targetCloseDate ? (' (' + rs.targetCloseDate + ')') : '')) + '<button type="button" class="bc-edit-link" data-offer-step="3">Edit</button></span></div>');
      out.push('    <div class="bc-sum-row"><span>4. ' + esc(L('Contingencies', '保护条款', 'Contingencias')) + '</span><span style="text-align:right;max-width:62%">' + esc((rs.activeContingencies || []).join(' · ')) + '<button type="button" class="bc-edit-link" data-offer-step="4">Edit</button></span></div>');
      var commObj = wpw.buyerAgentCommission || {};
      out.push('    <div class="bc-sum-row"><span>5a. ' + esc(L('Buyer Agent Commission', '买方经纪佣金', 'Comisión Agente')) + '</span><span>' + esc(commObj.pct + '% (' + fmtUsd(commObj.totalUsd) + ' · ' + (commObj.payer || 'seller') + ')') + '<button type="button" class="bc-edit-link" data-offer-step="5">Edit</button></span></div>');
      out.push('    <div class="bc-sum-row"><span>5b. ' + esc(L('City / County Transfer Tax', '市/县过户税', 'Impuestos Transf.')) + '</span><span style="text-align:right">City: ' + esc(fmtUsd(wpw.cityTransferTax && wpw.cityTransferTax.totalUsd)) + ' (' + esc(d.cityTransferTaxPayer) + ')<br>County: ' + esc(fmtUsd(wpw.countyTransferTax && wpw.countyTransferTax.totalUsd)) + ' (' + esc(d.countyTransferTaxPayer) + ')<button type="button" class="bc-edit-link" data-offer-step="5">Edit</button></span></div>');
      out.push('    <div class="bc-sum-row"><span>5c. ' + esc(L('Escrow / Title / Warranty', '托管/产权险/保修', 'Escrow / Título / Garantía')) + '</span><span style="text-align:right">Escrow: ' + esc(d.escrowFeePayer) + ' · Title: ' + esc(d.ownersTitlePayer) + '<br>Warranty: ' + esc(d.homeWarrantyEnabled ? (d.homeWarrantyMonths + 'm / ' + fmtUsd(d.homeWarrantyMaxUsd)) : 'None') + '<button type="button" class="bc-edit-link" data-offer-step="5">Edit</button></span></div>');
      out.push('    <div class="bc-sum-row"><span>6. ' + esc(L('Seller Credit / Repairs', '卖方抵扣与维修', 'Crédito y Reparaciones')) + '</span><span style="text-align:right">' + esc(fmtUsd(rs.sellerCreditUsd || 0)) + (rs.sellerRepairs ? (' · ' + String(rs.sellerRepairs).slice(0, 38) + '…') : '') + '<button type="button" class="bc-edit-link" data-offer-step="6">Edit</button></span></div>');
      if (rs.estimatedBuyerCashBeforeLenderFeesUsd != null) {
        out.push('    <div class="bc-sum-row" style="background:var(--bc-brandsoft);padding:8px;border-radius:8px;margin-top:6px"><span style="font-weight:800;color:var(--bc-brand-fg)">' + esc(L('Est. Buyer Cash (Down + Allocated Taxes/Comm - Credit)', '买方预计现金支出（不含贷款杂费）', 'Efectivo Estimado Comprador')) + '</span><strong style="color:var(--bc-brand-fg);font-size:14.5px">' + esc(fmtUsd(rs.estimatedBuyerCashBeforeLenderFeesUsd)) + '</strong></div>');
      }
      out.push('    <div style="display:flex;gap:8px;flex-wrap:wrap;margin-top:12px">');
      out.push('      <button type="button" class="bc-btn bc-btn-pri" id="bcPrintOfferBtn" style="flex:1;justify-content:center;padding:8px 12px;font-size:12.5px">' + esc(L('Print / Review Offer Summary', '打印 / 导出出价审阅单', 'Imprimir Resumen')) + '</button>');
      out.push('      <button type="button" class="bc-btn bc-btn-sec" id="bcCopyOfferMdBtn" style="padding:8px 12px;font-size:12.5px">' + esc(L('Copy Text', '复制摘要', 'Copiar')) + '</button>');
      out.push('    </div>');
      out.push('    <div style="margin-top:10px;font-size:11.5px;color:var(--bc-warn-fg);background:var(--bc-warn-bg);border:1px solid var(--bc-warn-bd);border-radius:8px;padding:7px 9px;font-weight:600">' + esc((ev.finalReview && ev.finalReview.disclaimer) || 'For buyer and agent review only — NOT a signed contract.') + '</div>');
      var postNarBody = String((ev.finalReview && ev.finalReview.postNarSettlementNotice) || jd.postNarSettlementNotice || 'Buyer-broker compensation is 100% negotiable and never fixed by law; MLS-participating agents generally require a written Buyer-Broker Representation Agreement before touring.').replace(/^Post-NAR[^:]*:\s*/i, '');
      var transferTaxBody = String((ev.finalReview && ev.finalReview.transferTaxDisclaimer) || jd.transferTaxCustomarinessDisclaimer || 'Pre-filled state/county/city transfer-tax rates and buyer/seller splits reflect general local custom for informational modeling only, not statutory mandates; verify exact rates and allocations with escrow/counsel.').replace(/^State,\s*County[^:]*:\s*/i, '');
      out.push('    <div id="bcPostNarNotice" style="margin-top:7px;font-size:11.2px;color:var(--bc-muted);line-height:1.42"><strong>' + esc(L('Post-NAR Buyer-Broker Notice:', 'NAR 买方代理协议提示：', 'Aviso Post-NAR:')) + '</strong> ' + esc(postNarBody) + '</div>');
      out.push('    <div id="bcTransferTaxDisclaimer" style="margin-top:5px;font-size:11.2px;color:var(--bc-muted);line-height:1.42"><strong>' + esc(L('Transfer-Tax Customariness Disclaimer:', '过户税地方惯例免责声明：', 'Aviso sobre Impuestos de Transferencia:')) + '</strong> ' + esc(transferTaxBody) + '</div>');
      out.push('  </div>');

      out.push('</div>'); // .bc-offer-grid
      out.push('</div>');
      return out.join('');
    }

    function bindDynamicEvents() {
      bodyEl.querySelectorAll('[data-bc-set-role]').forEach(function (rb) {
        rb.addEventListener('click', function () {
          setAssistantRole(rb.getAttribute('data-bc-set-role'));
        });
      });

      var offBtn = bodyEl.querySelector('#bcPreviewOffMarketBtn');
      if (offBtn) {
        offBtn.addEventListener('click', function () {
          state.allowOffMarketPreview = true;
          renderBody();
        });
      }

      bodyEl.querySelectorAll('[data-fn-pick]').forEach(function (card) {
        card.addEventListener('click', function () {
          openToolFn(card.getAttribute('data-fn-pick'));
        });
      });

      bodyEl.querySelectorAll('[data-fn-jump]').forEach(function (jb) {
        jb.addEventListener('click', function () {
          openToolFn(jb.getAttribute('data-fn-jump'));
        });
      });

      // Chat Q&A panel events
      var genBtn = bodyEl.querySelector('#bcGenerateReportBtn');
      if (genBtn) {
        genBtn.addEventListener('click', function () {
          generateFullReportInPlace();
        });
      }
      var chatClearBtn = bodyEl.querySelector('#bcChatClearBtn');
      if (chatClearBtn) {
        chatClearBtn.addEventListener('click', function () {
          state.chatHistory = [];
          if (state.property && state.property.slug) {
            state.chatBySlug[state.property.slug] = [];
          }
          renderBody();
        });
      }
      bodyEl.querySelectorAll('[data-chat-starter]').forEach(function (chip) {
        chip.addEventListener('click', function () {
          sendChatQuestion(chip.getAttribute('data-chat-starter'));
        });
      });
      bodyEl.querySelectorAll('[data-chat-followup]').forEach(function (fbtn) {
        fbtn.addEventListener('click', function () {
          sendChatQuestion(fbtn.getAttribute('data-chat-followup'));
        });
      });
      var chatForm = bodyEl.querySelector('#bcChatForm');
      var chatInput = bodyEl.querySelector('#bcChatInput');
      if (chatForm && chatInput) {
        chatForm.addEventListener('submit', function (ev) {
          ev.preventDefault();
          sendChatQuestion(chatInput.value);
        });
      }

      // Function 1 Comps events
      var runCompsBtn = bodyEl.querySelector('#bcRunCompsYesBtn');
      if (runCompsBtn) runCompsBtn.addEventListener('click', function () { loadComps('strict'); });

      var expBtn = bodyEl.querySelector('#bcExpandCompsBtn') || bodyEl.querySelector('#bcExpandCompsInlineBtn');
      if (expBtn) expBtn.addEventListener('click', function () { loadComps('expanded'); });
      var expInline = bodyEl.querySelector('#bcExpandCompsInlineBtn');
      if (expInline && expInline !== expBtn) expInline.addEventListener('click', function () { loadComps('expanded'); });

      var strictBtn = bodyEl.querySelector('#bcSwitchStrictBtn');
      if (strictBtn) strictBtn.addEventListener('click', function () { loadComps('strict'); });

      bodyEl.querySelectorAll('[data-followup]').forEach(function (chip) {
        chip.addEventListener('click', function () {
          loadFollowUp(chip.getAttribute('data-followup'));
        });
      });

      // Function 2 Registered User Account bar events
      var regEmailInp = bodyEl.querySelector('#bcRegEmailInp');
      var regNameInp = bodyEl.querySelector('#bcRegNameInp');
      if (regEmailInp) {
        regEmailInp.addEventListener('input', function () { state.regEmailDraft = regEmailInp.value; });
      }
      if (regNameInp) {
        regNameInp.addEventListener('input', function () { state.regNameDraft = regNameInp.value; });
      }
      var regForm = bodyEl.querySelector('#bcRegForm');
      if (regForm) {
        regForm.addEventListener('submit', function (ev) {
          ev.preventDefault();
          registerAccount(regEmailInp && regEmailInp.value, regNameInp && regNameInp.value);
        });
      }
      var regCodeInp = bodyEl.querySelector('#bcRegCodeInp');
      if (regCodeInp) {
        regCodeInp.addEventListener('input', function () { state.regCodeDraft = regCodeInp.value; });
      }
      var verifyForm = bodyEl.querySelector('#bcVerifyForm');
      if (verifyForm) {
        verifyForm.addEventListener('submit', function (ev) {
          ev.preventDefault();
          var pvEmail = state.pendingVerification && state.pendingVerification.email;
          verifyAccountCode(pvEmail, regCodeInp && regCodeInp.value);
        });
      }
      var oneClickVerifyBtn = bodyEl.querySelector('#bcOneClickVerifyBtn');
      if (oneClickVerifyBtn) {
        oneClickVerifyBtn.addEventListener('click', function () {
          var pvNow = state.pendingVerification || {};
          verifyAccountCode(pvNow.email, pvNow.devOrLocalVerificationCode || (regCodeInp && regCodeInp.value));
        });
      }
      var verifyCancelBtn = bodyEl.querySelector('#bcVerifyCancelBtn');
      if (verifyCancelBtn) {
        verifyCancelBtn.addEventListener('click', function () {
          state.pendingVerification = null;
          state.regCodeDraft = '';
          showError(null);
          renderBody();
        });
      }
      var regSignOutBtn = bodyEl.querySelector('#bcRegSignOutBtn');
      if (regSignOutBtn) {
        regSignOutBtn.addEventListener('click', function () {
          signOutAccount();
        });
      }

      // Function 2 mailto: request card events
      var mailtoInp = bodyEl.querySelector('#bcMailtoRecipientInp');
      var mailtoLink = bodyEl.querySelector('#bcMailtoOpenLink');
      var mailtoCopyBtn = bodyEl.querySelector('#bcMailtoCopyBtn');
      var curMailto = (state.disclosuresState && state.disclosuresState.mailtoRequest) || (state.property && state.property.materialRequestMailto) || buildFallbackMailto(state.property);
      if (mailtoInp && curMailto) {
        mailtoInp.addEventListener('input', function () {
          state.mailtoRecipient = mailtoInp.value;
          if (mailtoLink) {
            mailtoLink.setAttribute('href', buildMailtoHref(mailtoInp.value, curMailto.subject, curMailto.body));
          }
        });
      }
      if (mailtoCopyBtn && curMailto) {
        mailtoCopyBtn.addEventListener('click', async function () {
          var recip = (mailtoInp && mailtoInp.value) || state.mailtoRecipient || curMailto.recipientEmail || '';
          var draftTxt = (recip ? ('To: ' + recip + '\n') : '') + 'Subject: ' + (curMailto.subject || '') + '\n\n' + (curMailto.body || '');
          try {
            await navigator.clipboard.writeText(draftTxt);
            mailtoCopyBtn.textContent = L('Copied ✓', '已复制 ✓', 'Copiado ✓');
            setTimeout(function () { mailtoCopyBtn.textContent = L('Copy Email Draft', '复制邮件草稿', 'Copiar Borrador'); }, 1800);
          } catch (_) {}
        });
      }

      // Function 2 sub-tabs
      bodyEl.querySelectorAll('[data-disc-subtab]').forEach(function (tabBtn) {
        tabBtn.addEventListener('click', function () {
          state.discSubTab = tabBtn.getAttribute('data-disc-subtab') || 'all';
          renderBody();
        });
      });

      // Function 2 dropzone events
      var dz = bodyEl.querySelector('#bcDiscDropzone');
      var fi = bodyEl.querySelector('#bcDiscFileInput');
      if (dz && fi && !state.uploadingDisclosures) {
        dz.addEventListener('click', function () { fi.click(); });
        dz.addEventListener('dragover', function (e) { e.preventDefault(); dz.classList.add('drag'); });
        dz.addEventListener('dragleave', function () { dz.classList.remove('drag'); });
        dz.addEventListener('drop', function (e) {
          e.preventDefault();
          dz.classList.remove('drag');
          if (e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files.length) {
            uploadDisclosureFiles(e.dataTransfer.files);
          }
        });
        fi.addEventListener('change', function () {
          if (fi.files && fi.files.length) uploadDisclosureFiles(fi.files);
        });
      }

      // Function 2 PDF Studio buttons & local loader
      var openStudioBtn = bodyEl.querySelector('#bcOpenPdfStudioBtn');
      if (openStudioBtn) {
        openStudioBtn.addEventListener('click', function () {
          var dsNow = state.disclosuresState || {};
          var bestAnalysis = state.disclosures || unwrapDisclosureAnalysis(dsNow.authorizedDisclosures) || unwrapDisclosureAnalysis(dsNow.sharedWithMeAnalysis || dsNow.sharedDealTeamAnalysis) || null;
          launchPdfStudio(bestAnalysis, null);
        });
      }
      var pdfLocalInp = bodyEl.querySelector('#bcPdfLocalLoadInp');
      if (pdfLocalInp) {
        pdfLocalInp.addEventListener('change', async function () {
          if (pdfLocalInp.files && pdfLocalInp.files.length) {
            try {
              if (window.OpenEstatePdfStudio && typeof window.OpenEstatePdfStudio.ingestFiles === 'function') {
                var unpackedLocal = await window.OpenEstatePdfStudio.ingestFiles(pdfLocalInp.files, state.property && state.property.slug);
                if (Array.isArray(unpackedLocal) && unpackedLocal.length) {
                  state.pdfLocalFileCount = unpackedLocal.length;
                }
              }
            } catch (_) {}
            var pending = state.pdfLocalPrompt || {};
            state.pdfLocalPrompt = null;
            renderBody();
            launchPdfStudio(pending.analysis || state.disclosures || null, pending.citation || null);
          }
        });
      }
      bodyEl.querySelectorAll('[data-pdf-cite-idx]').forEach(function (pbtn) {
        pbtn.addEventListener('click', function () {
          var idx = Number(pbtn.getAttribute('data-pdf-cite-idx'));
          var entry = renderedPdfCitations[idx];
          if (!entry) return;
          launchPdfStudio(entry.analysis, entry.citation);
        });
      });

      // Function 2 Registered-User Redacted Share form events
      var shareRecipInp = bodyEl.querySelector('#bcShareRecipientEmailInp');
      if (shareRecipInp) {
        shareRecipInp.addEventListener('input', function () { state.shareRecipientDraft = shareRecipInp.value; });
      }
      var shareAttestChk = bodyEl.querySelector('#bcShareAttestChk');
      if (shareAttestChk) {
        shareAttestChk.addEventListener('change', function () { state.shareAttestedDraft = !!shareAttestChk.checked; });
      }
      var shareForm = bodyEl.querySelector('#bcShareDealTeamForm');
      if (shareForm) {
        shareForm.addEventListener('submit', function (ev) {
          ev.preventDefault();
          shareDisclosuresWithRegisteredUser(
            shareRecipInp && shareRecipInp.value,
            !!(shareAttestChk && shareAttestChk.checked)
          );
        });
      }

      // Function 2 Authorized Disclosures toggle & publish form
      var toggleAuthRead = bodyEl.querySelector('#bcToggleAuthReadingBtn');
      if (toggleAuthRead) {
        toggleAuthRead.addEventListener('click', function () {
          state.authViewingReading = !state.authViewingReading;
          renderBody();
        });
      }
      var authRoleSel = bodyEl.querySelector('#bcAuthRoleSel');
      var authLicenseWrap = bodyEl.querySelector('#bcAuthLicenseWrap');
      var authLicenseInp = bodyEl.querySelector('#bcAuthLicenseInp');
      if (authRoleSel) {
        authRoleSel.addEventListener('change', function () {
          state.authFormDraft.uploaderRole = authRoleSel.value;
          var isSeller = authRoleSel.value === 'seller';
          if (authLicenseWrap) authLicenseWrap.style.display = isSeller ? 'none' : '';
          if (authLicenseInp) {
            if (isSeller) authLicenseInp.removeAttribute('required');
            else authLicenseInp.setAttribute('required', 'required');
          }
        });
      }
      var authSignerInp = bodyEl.querySelector('#bcAuthSignerName');
      if (authSignerInp) {
        authSignerInp.addEventListener('input', function () { state.authFormDraft.signerName = authSignerInp.value; });
      }
      var authEmailInp = bodyEl.querySelector('#bcAuthSignerEmail');
      if (authEmailInp) {
        authEmailInp.addEventListener('input', function () { state.authFormDraft.signerEmail = authEmailInp.value; });
      }
      if (authLicenseInp) {
        authLicenseInp.addEventListener('input', function () { state.authFormDraft.licenseNumber = authLicenseInp.value; });
      }
      var authConsentChk = bodyEl.querySelector('#bcAuthConsentChk');
      if (authConsentChk) {
        authConsentChk.addEventListener('change', function () { state.authFormDraft.authorizedToDistribute = !!authConsentChk.checked; });
      }
      var authForm = bodyEl.querySelector('#bcAuthUploadForm');
      if (authForm) {
        authForm.addEventListener('submit', function (ev) {
          ev.preventDefault();
          state.authFormDraft.uploaderRole = (authRoleSel && authRoleSel.value) || 'listing_agent';
          state.authFormDraft.signerName = (authSignerInp && authSignerInp.value) || '';
          state.authFormDraft.signerEmail = (authEmailInp && authEmailInp.value) || '';
          state.authFormDraft.licenseNumber = (authLicenseInp && authLicenseInp.value) || '';
          state.authFormDraft.authorizedToDistribute = !!(authConsentChk && authConsentChk.checked);
          var authFi = bodyEl.querySelector('#bcAuthFileInput');
          submitAuthorizedDisclosures(authFi && authFi.files);
        });
      }

      bodyEl.querySelectorAll('[data-send-redflags="1"]').forEach(function (toOfferBtn) {
        toOfferBtn.addEventListener('click', async function () {
          state.activeFn = 'offer_builder';
          state.allowOffMarketPreview = true;
          await submitOfferPatch({ importDisclosures: true, currentStep: 6 }, 6);
        });
      });

      // Function 3 Offer Builder events
      bodyEl.querySelectorAll('[data-offer-step]').forEach(function (el) {
        el.addEventListener('click', function () {
          state.offerStep = Number(el.getAttribute('data-offer-step')) || 1;
          renderBody();
        });
      });

      var prevBtn = bodyEl.querySelector('#bcOfferPrevBtn');
      if (prevBtn) {
        prevBtn.addEventListener('click', function () {
          state.offerStep = Math.max(1, (state.offerStep || 1) - 1);
          renderBody();
        });
      }

      var dpInp = bodyEl.querySelector('#bcDpPctInp');
      var lnMir = bodyEl.querySelector('#bcLoanPctMirror');
      var finSel = bodyEl.querySelector('#bcFinTypeSel');
      if (dpInp && lnMir) {
        dpInp.addEventListener('input', function () {
          var v = Math.max(0, Math.min(100, Number(dpInp.value) || 0));
          lnMir.value = String(Math.round((100 - v) * 100) / 100);
        });
      }
      if (finSel && dpInp && lnMir) {
        finSel.addEventListener('change', function () {
          if (finSel.value === 'cash') {
            dpInp.value = '100';
            lnMir.value = '0';
          }
        });
      }

      var impBtn = bodyEl.querySelector('#bcImportDiscBtn');
      if (impBtn) {
        impBtn.addEventListener('click', function () {
          submitOfferPatch({ importDisclosures: true, currentStep: 6 }, 6);
        });
      }

      var offerForm = bodyEl.querySelector('#bcOfferStepForm');
      if (offerForm) {
        offerForm.addEventListener('submit', function (ev) {
          ev.preventDefault();
          var patch = { step: state.offerStep };
          offerForm.querySelectorAll('input, select, textarea').forEach(function (inp) {
            if (!inp.name) return;
            if (inp.type === 'checkbox') {
              patch[inp.name] = !!inp.checked;
            } else if (inp.type === 'number') {
              if (inp.value !== '') patch[inp.name] = Number(inp.value);
            } else {
              patch[inp.name] = inp.value;
            }
          });
          var nextStep = state.offerStep < 6 ? state.offerStep + 1 : 6;
          submitOfferPatch(patch, nextStep);
        });
      }

      var printBtn = bodyEl.querySelector('#bcPrintOfferBtn');
      if (printBtn && state.offerEval && state.offerEval.finalReview) {
        printBtn.addEventListener('click', function () {
          var w = window.open('', '_blank', 'noopener,noreferrer,width=880,height=760');
          if (w) {
            w.document.open();
            w.document.write(state.offerEval.finalReview.printableHtml || '');
            w.document.close();
            setTimeout(function () { try { w.print(); } catch (_) {} }, 250);
          }
        });
      }

      var copyBtn = bodyEl.querySelector('#bcCopyOfferMdBtn');
      if (copyBtn && state.offerEval && state.offerEval.finalReview) {
        copyBtn.addEventListener('click', async function () {
          try {
            await navigator.clipboard.writeText(state.offerEval.finalReview.markdownSummary || '');
            copyBtn.textContent = L('Copied ✓', '已复制 ✓', 'Copiado ✓');
            setTimeout(function () { copyBtn.textContent = L('Copy Text', '复制摘要', 'Copiar'); }, 1800);
          } catch (_) {}
        });
      }
    }

    if (getStoredUser()) {
      syncSessionWithServer();
    }
    var initTargetFn = resolveRequestedFn(opts);
    if (opts.initialSlug) {
      lookupProperty({ slug: opts.initialSlug }, { initialFn: initTargetFn, autoOpenDisclosures: initTargetFn === 'disclosures' });
    } else if (opts.initialAddress) {
      inpEl.value = opts.initialAddress;
      lookupProperty({ address: opts.initialAddress }, { initialFn: initTargetFn, autoOpenDisclosures: initTargetFn === 'disclosures' });
    } else {
      renderBody();
    }

    return {
      lookupProperty: lookupProperty,
      getSlug: function () { return (state.property && state.property.slug) || ''; },
      setRole: setAssistantRole,
      openFn: openToolFn,
      openChat: function () { return openToolFn('chat'); },
      openComps: function () { return openToolFn('comps_pricing'); },
      openDisclosures: function () { return openToolFn('disclosures'); },
      openOffer: function () { return openToolFn('offer_builder'); },
      rerender: function () {
        state.role = readInitialRole();
        state.regUser = getStoredUser();
        renderHeaderBar();
        if (inpEl) {
          inpEl.setAttribute('placeholder', L(
            'Enter a US property street address (e.g., 123 Synth Main St, San Jose, CA 95112)…',
            '输入美国房产街道地址（如 123 Synth Main St, San Jose, CA 95112）…',
            'Ingrese una dirección en EE. UU. (ej. 123 Synth Main St, San Jose, CA)…'
          ));
        }
        if (btnEl && !btnEl.disabled) {
          btnEl.textContent = L('Check Property', '查询房源', 'Consultar');
        }
        renderBody();
      }
    };
  }

  // Modal launcher for `/r/:slug` report pages
  var modalInstance = null;
  function openReportCopilotModal(slug, address, modalOpts) {
    modalOpts = modalOpts || {};
    injectStyles();
    var curRole = readInitialRole();
    var backdrop = document.getElementById('bcReportModalBackdrop');
    if (!backdrop) {
      backdrop = document.createElement('div');
      backdrop.id = 'bcReportModalBackdrop';
      backdrop.className = 'bc-modal-backdrop';
      backdrop.innerHTML = [
        '<div class="bc-modal-dialog" role="dialog" aria-modal="true" aria-label="' + esc(assistantBrandTitle()) + '">',
        '  <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:12px">',
        '    <strong id="bcModalTitle" style="font-size:16px;color:var(--bc-brand-fg)">' + esc(buildModalTitleText(curRole)) + '</strong>',
        '    <button type="button" class="bc-btn bc-btn-sec" id="bcCloseModalBtn" style="padding:6px 12px;font-size:12.5px">✕ ' + esc(L('Close', '关闭', 'Cerrar')) + '</button>',
        '  </div>',
        '  <div id="bcReportModalMount"></div>',
        '</div>'
      ].join('');
      document.body.appendChild(backdrop);
      backdrop.addEventListener('click', function (ev) {
        if (ev.target === backdrop) backdrop.classList.remove('on');
      });
      var closeBtn = backdrop.querySelector('#bcCloseModalBtn');
      if (closeBtn) closeBtn.addEventListener('click', function () { backdrop.classList.remove('on'); });
    }
    backdrop.classList.add('on');
    syncModalChrome(curRole);

    var mount = document.getElementById('bcReportModalMount');
    var targetFn = resolveRequestedFn(modalOpts);
    var autoDisc = targetFn === 'disclosures';
    if (!modalInstance) {
      modalInstance = createCopilotInstance(
        mount,
        slug
          ? { initialSlug: slug, initialFn: targetFn, autoOpenDisclosures: autoDisc }
          : { initialAddress: address, initialFn: targetFn, autoOpenDisclosures: autoDisc }
      );
    } else if (slug && modalInstance.getSlug && modalInstance.getSlug() === slug) {
      modalInstance.rerender();
      modalInstance.openFn(targetFn);
    } else if (slug) {
      modalInstance.lookupProperty({ slug: slug }, { initialFn: targetFn, autoOpenDisclosures: autoDisc });
    } else if (address) {
      modalInstance.lookupProperty({ address: address }, { initialFn: targetFn, autoOpenDisclosures: autoDisc });
    } else {
      modalInstance.rerender();
      modalInstance.openFn(targetFn);
    }
  }

  window.OpenEstateBuyerCopilot = {
    mount: createCopilotInstance,
    openModal: openReportCopilotModal
  };

  function readPageReport() {
    try {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.getRep === 'function') {
        var cr = window.OpenEstateCore.getRep();
        if (cr && typeof cr === 'object') return cr;
      }
    } catch (_) {}
    if (window.__REPORT__ && typeof window.__REPORT__ === 'object') return window.__REPORT__;
    if (window.R && typeof window.R === 'object') return window.R;
    try {
      var el = document.getElementById('report-data');
      if (el && el.textContent) return JSON.parse(el.textContent);
    } catch (_) {}
    return {};
  }

  function resolvePageSlugAndAddress() {
    var r = readPageReport();
    var slug = (r.meta && r.meta.slug) || '';
    if (!slug) {
      var m = (window.location.pathname || '').match(/^\/r\/([^/?#]+)/);
      if (m && m[1]) {
        try { slug = decodeURIComponent(m[1]); } catch (_) { slug = m[1]; }
      }
    }
    var addr = (r.meta && r.meta.displayAddress) || (r.geo && r.geo.matchedAddress) || '';
    return { slug: slug, address: addr };
  }

  function autoBind() {
    var hasDealToken = !!readDealTeamToken();
    var homeMount = document.getElementById('buyerCopilotHomeMount');
    if (homeMount && !homeMount.dataset.bcMounted) {
      homeMount.dataset.bcMounted = '1';
      var params = new URLSearchParams(window.location.search);
      var initAddr = (params.get('address') || params.get('q') || '').trim();
      var initSlug = (params.get('slug') || '').trim();
      var initOpts = initSlug
        ? { initialSlug: initSlug, autoOpenDisclosures: hasDealToken }
        : (initAddr ? { initialAddress: initAddr, autoOpenDisclosures: hasDealToken } : {});
      window.__homeBuyerCopilot = createCopilotInstance(homeMount, initOpts);
    }

    var hdrBtn = document.getElementById('buyerCopilotHdrBtn');
    if (hdrBtn && !hdrBtn.dataset.bcBound) {
      hdrBtn.dataset.bcBound = '1';
      hdrBtn.addEventListener('click', function () {
        var info = resolvePageSlugAndAddress();
        openReportCopilotModal(info.slug, info.address);
      });
      if (hasDealToken && !homeMount) {
        var infoAuto = resolvePageSlugAndAddress();
        if (infoAuto.slug || infoAuto.address) {
          openReportCopilotModal(infoAuto.slug, infoAuto.address, { autoOpenDisclosures: true });
        }
      }
    }

    if (!document.documentElement.dataset.bcLangHooked) {
      document.documentElement.dataset.bcLangHooked = '1';
      document.addEventListener('click', function (ev) {
        if (ev.target && ev.target.closest && ev.target.closest('#langSwitch button')) {
          setTimeout(function () {
            syncModalChrome(readInitialRole());
            if (modalInstance && typeof modalInstance.rerender === 'function') {
              modalInstance.rerender();
            }
            if (window.__homeBuyerCopilot && typeof window.__homeBuyerCopilot.rerender === 'function') {
              window.__homeBuyerCopilot.rerender();
            }
          }, 25);
        }
      });
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', autoBind);
  } else {
    autoBind();
  }
})();
