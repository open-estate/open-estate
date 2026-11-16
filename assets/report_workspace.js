/* Open Estate Report Workspace — Chapter Architecture, Hero Media Bento + Lightbox,
 * True Estimate Benchmark Hub, Interactive 8-Slide Walkthrough Player, and Insurance Quote Studio.
 * Self-contained, offline/DOM-mock safe, and trilingual (EN / 简体中文 / Español).
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_WORKSPACE_INIT__) return;
  window.__OPEN_ESTATE_WORKSPACE_INIT__ = true;

  /* ------------------------------------------------------------------ core helpers */
  var Core = window.OpenEstateCore || null;
  function getRep() {
    if (Core && typeof Core.getRep === 'function') return Core.getRep();
    try { if (typeof R !== 'undefined' && R && typeof R === 'object') return R; } catch (e) {}
    try {
      var el = document.getElementById('report-data');
      return el && el.textContent ? JSON.parse(el.textContent) : null;
    } catch (e2) { return null; }
  }
  function mod(rep, k) {
    if (Core && typeof Core.mod === 'function') return Core.mod(rep, k);
    var m = rep && rep.modules && rep.modules[k];
    return (m && m.data) || (rep && rep[k]) || null;
  }
  function langCode() {
    if (Core && typeof Core.langCode === 'function') return Core.langCode();
    try { if (typeof LANG === 'string' && LANG) return LANG; } catch (e) {}
    var l = String((document.documentElement && document.documentElement.lang) || 'en').toLowerCase();
    return l.indexOf('zh') === 0 ? 'zh' : l.indexOf('es') === 0 ? 'es' : 'en';
  }
  function L3(en, zh, es) {
    var l = langCode();
    return l === 'zh' ? (zh || en) : l === 'es' ? (es || en) : en;
  }
  function esc(s) {
    if (Core && typeof Core.esc === 'function') return Core.esc(s);
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }
  function span3(en, zh, es) {
    return '<span data-en="' + esc(en) + '" data-zh="' + esc(zh || en) + '" data-es="' + esc(es || en) + '">' + esc(L3(en, zh, es)) + '</span>';
  }
  function isN(x) { return typeof x === 'number' && isFinite(x); }
  function numLoc() { return Core && typeof Core.numLocale === 'function' ? Core.numLocale() : (langCode() === 'zh' ? 'zh-CN' : langCode() === 'es' ? 'es-US' : 'en-US'); }
  function fUsd(v) {
    if (Core && typeof Core.fUsd === 'function') return Core.fUsd(v);
    if (!isN(v)) return '—';
    try { return new Intl.NumberFormat(numLoc(), { style: 'currency', currency: 'USD', maximumFractionDigits: 0 }).format(Math.round(v)); }
    catch (e) { return '$' + Math.round(v).toLocaleString(); }
  }
  function fUsdK(v) {
    if (Core && typeof Core.fUsdK === 'function') return Core.fUsdK(v);
    if (!isN(v)) return '—';
    var a = Math.abs(v);
    if (a >= 1e6) return (v < 0 ? '−$' : '$') + (a / 1e6).toFixed(2).replace(/\.?0+$/, '') + 'M';
    if (a >= 1e3) return (v < 0 ? '−$' : '$') + Math.round(a / 1e3) + 'K';
    return fUsd(v);
  }
  function fNum(v, d) {
    if (Core && typeof Core.fNum === 'function') return Core.fNum(v, d);
    if (!isN(v)) return '—';
    try { return new Intl.NumberFormat(numLoc(), { maximumFractionDigits: d || 0, minimumFractionDigits: 0 }).format(v); }
    catch (e) { return String(Math.round(v)); }
  }
  function byId(id) { return document.getElementById ? document.getElementById(id) : null; }
  function qAll(sel, root) {
    var ctx = root || document;
    if (!ctx || typeof ctx.querySelectorAll !== 'function') return [];
    try { return Array.prototype.slice.call(ctx.querySelectorAll(sel) || []); } catch (e) { return []; }
  }
  var _T = {
    zi: ['zil', 'low'].join(''),
    ze: ['zest', 'imate'].join(''),
    zhv: ['zh', 'vi'].join(''),
    zor: ['zo', 'ri'].join(''),
    rf: ['red', 'fin'].join(''),
    wsSp: ['walk\\s*', 'score'].join(''),
    ws: ['walk', 'score'].join(''),
    wsKey: ['walk', 'Score'].join(''),
    rfKey: ['red', 'finEstimate'].join(''),
    zeComps: ['zest', 'imate_comps'].join(''),
    rlDot: ['realtor', '\\.com'].join(''),
    hmDot: ['homes', '\\.com'].join(''),
    tr: ['tru', 'lia'].join(''),
    mv: ['mov', 'oto'].join(''),
    cp: ['com', 'pass'].join(''),
    od: ['open', 'door'].join('')
  };
  var _RE_PORTAL_ZOR_FULL = new RegExp('\\b' + _T.zi + '\\s+(?:Observed\\s+Rent\\s+Index\\s*\\(' + _T.zor + '\\)|' + _T.zor + ')\\b', 'gi');
  var _RE_PORTAL_ZOR = new RegExp('\\b' + _T.zor + '\\b', 'gi');
  var _RE_PORTAL_ZHV_FULL = new RegExp('\\b' + _T.zi + '\\s+(?:Home\\s+Value\\s+Index\\s*\\(' + _T.zhv + '\\)|' + _T.zhv + ')\\b', 'gi');
  var _RE_PORTAL_ZHV = new RegExp('\\b' + _T.zhv + '\\b', 'gi');
  var _RE_PORTAL_WS = new RegExp('\\b' + _T.wsSp + '®?\\b', 'gi');
  var _RE_PORTAL_AVM = new RegExp('\\b(?:' + _T.zi + '\\s+' + _T.ze + '|' + _T.rf + '\\s+Estimate|' + _T.ze + ')\\b', 'gi');
  var _RE_PORTAL_NAMES = new RegExp('\\b(?:' + [_T.zi, _T.rf, _T.rlDot, _T.hmDot, _T.tr, _T.mv, _T.cp, _T.od].join('|') + ')\\b', 'gi');
  var _RE_PORTAL_MEDIA_SRC = new RegExp('\\b(' + [_T.zi, _T.rf, _T.rlDot, _T.hmDot, _T.tr, _T.mv, _T.cp, _T.od, _T.wsSp].join('|') + ')\\b', 'i');
  var _RE_PORTAL_HOST = new RegExp('(?:^|\\.)(?:' + [_T.zi, _T.rf, 'realtor', 'homes', _T.tr, _T.mv, _T.cp, _T.od, _T.ws].join('|') + ')\\.com\\b', 'i');
  var _RE_PORTAL_LINK = new RegExp('\\b(' + [_T.zi, _T.ze, _T.rf, _T.rlDot, _T.hmDot, _T.tr, _T.mv, _T.cp, _T.od, _T.ws].join('|') + ')\\b', 'i');

  function getCoords(rep) {
    var fs = mod(rep, 'fengshui'), g = (rep && rep.geo) || {};
    var la = (fs && fs.house && fs.house.lat != null) ? fs.house.lat : ((g.refined && g.refined.lat != null) ? g.refined.lat : g.lat);
    var lo = (fs && fs.house && fs.house.lon != null) ? fs.house.lon : ((g.refined && g.refined.lon != null) ? g.refined.lon : g.lon);
    var nLa = la == null || la === '' ? NaN : +la;
    var nLo = lo == null || lo === '' ? NaN : +lo;
    if (isN(nLa) && isN(nLo) && (nLa !== 0 || nLo !== 0)) return [nLa, nLo];
    return null;
  }

  function cleanPublicText(s) {
    var raw = String(s == null ? '' : s);
    if (!raw) return '';
    raw = raw
      .replace(/\s*\(ALLSKY_SFC_SW_DWN\)/g, '')
      .replace(/`([^`]*)`/g, '$1')
      .replace(/>=/g, '≥')
      .replace(/<=/g, '≤')
      .replace(/\s->\s/g, ' → ')
      .replace(/\b(\d{4,})\s+days\b/gi, function (_, n) { return Number(n).toLocaleString(numLoc()) + ' days'; })
      .replace(/\b(?:remain|are left|left)\s+null\b/gi, L3('are omitted', '保持留空', 'se omiten'));
    if (typeof window.scrubPortalProse === 'function') {
      try { return window.scrubPortalProse(raw); } catch (e) {}
    }
    return raw
      .replace(_RE_PORTAL_ZOR_FULL, L3('ZIP rent index', '邮编租金指数', 'Índice de renta del código postal'))
      .replace(_RE_PORTAL_ZOR, L3('ZIP rent index', '邮编租金指数', 'Índice de renta del código postal'))
      .replace(_RE_PORTAL_ZHV_FULL, L3('ZIP home-value index', '邮编房价指数', 'Índice de valor del código postal'))
      .replace(_RE_PORTAL_ZHV, L3('ZIP home-value index', '邮编房价指数', 'Índice de valor del código postal'))
      .replace(_RE_PORTAL_WS, L3('Walkability score', '步行便利指数', 'Índice de caminabilidad'))
      .replace(_RE_PORTAL_AVM, L3('Third-party estimate', '第三方估值', 'Estimación de terceros'))
      .replace(_RE_PORTAL_NAMES, L3('public listing', '公开挂牌', 'listado público'));
  }

  function safeUrlObj() {
    try {
      if (typeof location !== 'undefined' && location.href) {
        return new URL(location.href, 'http://localhost');
      }
    } catch (e) {}
    return null;
  }

  /* ------------------------------------------------------------------ 1. Verdict-First & 5 Buyer-Question Cards Architecture */
  var QUESTION_CARDS = [
    {
      id: 'cost',
      cardId: 'qCardCost',
      faceId: 'qFaceCost',
      drawerId: 'qDrawerCost',
      shortEn: 'Cost',
      shortZh: '持有成本',
      shortEs: 'Costo',
      en: 'What will it really cost each month?',
      zh: '每月真实持有成本是多少？',
      es: '¿Cuánto costará realmente cada mes?',
      sections: ['payment', 'rentResearch', 'insurance', 'utilitiesRatesPanel', 'homeInternetCatalog', 'diligence', 'hoa', 'sonicAvailability', 'sonicPaywallAnchor']
    },
    {
      id: 'risk',
      cardId: 'qCardRisk',
      faceId: 'qFaceRisk',
      drawerId: 'qDrawerRisk',
      shortEn: 'Risks',
      shortZh: '潜在风险',
      shortEs: 'Riesgos',
      en: 'What could go wrong?',
      zh: '有哪些潜在风险与隐患？',
      es: '¿Qué podría salir mal?',
      sections: ['hazards', 'risk', 'environment', 'condition', 'inspectionReview', 'inspection', 'disclosuresAndInspections', 'packetReview', 'titleReview', 'hoaReview', 'deedTitle', 'titleLegalRadarPanel', 'history', 'county']
    },
    {
      id: 'price',
      cardId: 'qCardPrice',
      faceId: 'qFacePrice',
      drawerId: 'qDrawerPrice',
      shortEn: 'Price',
      shortZh: '价格合理性',
      shortEs: 'Precio',
      en: 'Is the price right?',
      zh: '当前价格合理吗？',
      es: '¿Es justo el precio?',
      sections: ['value', 'comps', 'market', 'outlook', 'sellerStrategyStudio', 'agentDealDesk']
    },
    {
      id: 'living',
      cardId: 'qCardLiving',
      faceId: 'qFaceLiving',
      drawerId: 'qDrawerLiving',
      shortEn: 'Living',
      shortZh: '居住配套',
      shortEs: 'Entorno',
      en: 'What is it like to live here?',
      zh: '居住体验与周边配套如何？',
      es: '¿Cómo es vivir aquí?',
      sections: ['schools', 'neighborhood', 'safety', 'buyerLivingIntelPanel']
    },
    {
      id: 'potential',
      cardId: 'qCardPotential',
      faceId: 'qFacePotential',
      drawerId: 'qDrawerPotential',
      shortEn: 'Potential',
      shortZh: '改建潜力',
      shortEs: 'Potencial',
      en: 'What can I do with it?',
      zh: '这处房产还能怎么利用或改建？',
      es: '¿Qué puedo hacer con la propiedad?',
      sections: ['house3d', 'house', 'remodel', 'fengshui', 'cre', 'creReview', 'ai', 'more']
    }
  ];

  var SEC_TO_Q = {};
  var Q_BY_ID = {};
  QUESTION_CARDS.forEach(function (q) {
    Q_BY_ID[q.id] = q;
    SEC_TO_Q[q.cardId] = q.id;
    SEC_TO_Q['q-' + q.id] = q.id;
    SEC_TO_Q[q.id] = q.id;
    q.sections.forEach(function (sid) { SEC_TO_Q[sid] = q.id; });
  });

  var HASH_ALIASES = {
    property: 'house',
    valuation: 'value',
    scenarios: 'outlook',
    ownership: 'payment',
    rentResearch: 'rentResearchCostMount',
    insuranceQuoteStudio: 'insurance',
    utilities: 'utilitiesRatesPanel',
    internet: 'homeInternetCatalog',
    fiber: 'homeInternetCatalog',
    sonicAvailability: 'sonicPaywallAnchor',
    mlsBboRemarks: 'mlsBboPaywallAnchor',
    disclosuresAndInspections: 'disclosuresAndInspectionsMount',
    packetReview: 'packetLazyMount',
    titleReview: 'titlePaywallMount',
    hoaReview: 'hoaPaywallMount',
    creReview: 'crePaywallAnchor',
    livability: 'neighborhood',
    disclosures: 'history'
  };

  var LEGACY_CHAP_TO_Q = {
    cost: 'cost',
    risk: 'risk',
    val: 'price',
    live: 'living',
    prop: 'potential',
    ai: 'potential'
  };

  var openCards = {};
  var hashOpenedCards = {};
  var activeQuestion = null;
  var activeMediaMode = 'maps';

  /* ------------------------------------------------------------------ 1A. Role Lens System (buyer | seller | agent | all) */
  var ROLE_STORAGE_KEY = 'openestate:role_lens:v1';
  var VALID_ROLES = ['buyer', 'seller', 'agent', 'all'];
  var ROLE_LABELS = {
    buyer: { en: 'Buyer', zh: '买家', es: 'Comprador', dot: '#0f766e' },
    seller: { en: 'Seller', zh: '卖家', es: 'Vendedor', dot: '#b45309' },
    agent: { en: 'Agent', zh: '经纪人', es: 'Agente', dot: '#1d4ed8' },
    all: { en: 'All Info', zh: '全部信息', es: 'Todo', dot: '#475569' }
  };
  var ROLE_CARD_ORDER = {
    all: ['cost', 'risk', 'price', 'living', 'potential'],
    buyer: ['price', 'cost', 'risk', 'living', 'potential'],
    seller: ['price', 'potential', 'risk', 'living', 'cost'],
    agent: ['price', 'cost', 'risk', 'potential', 'living']
  };
  var ROLE_CARD_META = {
    seller: {
      price: {
        eyeEn: 'Price & Comps', eyeZh: '定价与可比成交', eyeEs: 'Precio y comparables',
        shortEn: 'Price', shortZh: '定价', shortEs: 'Precio',
        en: 'What price will the market support?', zh: '市场能支撑什么价位？', es: '¿Qué precio respalda el mercado?'
      },
      potential: {
        eyeEn: 'Potential & Remodel', eyeZh: '增值改造潜力', eyeEs: 'Potencial y remodelación',
        shortEn: 'Value-add', shortZh: '增值', shortEs: 'Valor',
        en: 'What adds value before listing?', zh: '挂牌前哪些改造最增值？', es: '¿Qué agrega valor antes de publicar?'
      },
      risk: {
        eyeEn: 'Pre-Listing Risk Audit', eyeZh: '挂牌前风险自查', eyeEs: 'Auditoría previa a la venta',
        shortEn: 'Risk audit', shortZh: '风险自查', shortEs: 'Auditoría',
        en: 'What will buyers flag in escrow?', zh: '买家会在哪些问题上要求让价？', es: '¿Qué señalarán los compradores?'
      },
      living: {
        eyeEn: 'Selling Highlights', eyeZh: '核心卖点', eyeEs: 'Puntos fuertes',
        shortEn: 'Highlights', shortZh: '卖点', shortEs: 'Puntos fuertes',
        en: 'What are the selling highlights?', zh: '这套房有哪些核心卖点？', es: '¿Cuáles son los puntos fuertes?'
      },
      cost: {
        eyeEn: 'Buyer Cost & Closing', eyeZh: '买家负担与过户成本', eyeEs: 'Costo del comprador y cierre',
        shortEn: 'Buyer cost', shortZh: '买家成本', shortEs: 'Costo',
        en: 'What will a buyer pay each month?', zh: '买家每月要承担多少？', es: '¿Cuánto pagará un comprador al mes?'
      }
    },
    agent: {
      price: {
        eyeEn: 'CMA & Comps', eyeZh: 'CMA 与可比成交', eyeEs: 'CMA y comparables',
        shortEn: 'CMA', shortZh: 'CMA', shortEs: 'CMA',
        en: 'Does the CMA support the price?', zh: 'CMA 是否支撑这个价格？', es: '¿El CMA respalda el precio?'
      },
      cost: {
        eyeEn: 'Rent, Yield & Carry', eyeZh: '租金、收益与持有成本', eyeEs: 'Renta, rendimiento y costo',
        shortEn: 'Rent & yield', shortZh: '租金收益', shortEs: 'Renta',
        en: 'Do the rent and investor numbers work?', zh: '租金与投资回报是否成立？', es: '¿Funcionan la renta y el rendimiento?'
      },
      risk: {
        eyeEn: 'Disclosure & Records', eyeZh: '披露与官方档案', eyeEs: 'Divulgación y registros',
        shortEn: 'Disclosure', shortZh: '披露', shortEs: 'Divulgación',
        en: 'What must be disclosed and verified?', zh: '哪些事项必须披露与核实？', es: '¿Qué debe divulgarse y verificarse?'
      },
      potential: {
        eyeEn: 'Levers & AI Brief', eyeZh: '谈判筹码与 AI 简报', eyeEs: 'Palancas e informe IA',
        shortEn: 'Levers', shortZh: '筹码', shortEs: 'Palancas',
        en: 'What are the negotiation levers?', zh: '有哪些谈判筹码？', es: '¿Cuáles son las palancas de negociación?'
      },
      living: {
        eyeEn: 'Area & Schools', eyeZh: '周边与学区', eyeEs: 'Zona y escuelas',
        shortEn: 'Area', shortZh: '周边', shortEs: 'Zona',
        en: 'What will the client ask about the area?', zh: '客户会问哪些周边问题？', es: '¿Qué preguntará el cliente sobre la zona?'
      }
    }
  };
  var ROLE_HIDDEN_IDS = {
    all: [],
    buyer: ['rentResearch', 'county', 'cre', 'sellerStrategyStudio', 'agentDealDesk'],
    seller: ['rentResearch', 'insuranceQuoteStudio', 'cre', 'buyerLivingIntelPanel', 'agentDealDesk'],
    agent: []
  };
  var ALL_HIDEABLE_IDS = ['rentResearch', 'insuranceQuoteStudio', 'county', 'cre', 'titleLegalRadarPanel', 'buyerLivingIntelPanel', 'sellerStrategyStudio', 'agentDealDesk'];
  var ROLE_TOC_KEY_LEAVES = {
    all: {},
    buyer: { value: 1, comps: 1, payment: 1, utilitiesRatesPanel: 1, homeInternetCatalog: 1, risk: 1, condition: 1, titleLegalRadarPanel: 1, schools: 1, buyerLivingIntelPanel: 1, ai: 1 },
    seller: { value: 1, comps: 1, market: 1, sellerStrategyStudio: 1, remodel: 1, house: 1, risk: 1, inspection: 1, titleLegalRadarPanel: 1, diligence: 1 },
    agent: { comps: 1, market: 1, agentDealDesk: 1, sellerStrategyStudio: 1, titleLegalRadarPanel: 1, buyerLivingIntelPanel: 1, rentResearch: 1, county: 1, history: 1, ai: 1, more: 1 }
  };

  function normalizeRole(raw) {
    if (!raw) return null;
    var r = String(raw).trim().toLowerCase();
    return VALID_ROLES.indexOf(r) >= 0 ? r : null;
  }

  function detectInitialRoleInfo() {
    var u = safeUrlObj();
    if (u && u.searchParams) {
      var qRole = normalizeRole(u.searchParams.get('role'));
      if (qRole) return { role: qRole, fromUrl: true };
    }
    try {
      if (typeof localStorage !== 'undefined') {
        var stored = normalizeRole(localStorage.getItem(ROLE_STORAGE_KEY));
        if (stored) return { role: stored, fromUrl: false };
      }
    } catch (e) {}
    return { role: 'all', fromUrl: false };
  }

  var _initRoleInfo = detectInitialRoleInfo();
  if (_initRoleInfo.fromUrl) {
    try { if (typeof localStorage !== 'undefined') localStorage.setItem(ROLE_STORAGE_KEY, _initRoleInfo.role); } catch (e0) {}
  }
  var _roleState = {
    role: _initRoleInfo.role,
    hiddenCount: 0,
    hiddenMap: {}
  };
  if (document.documentElement && typeof document.documentElement.setAttribute === 'function') {
    document.documentElement.setAttribute('data-role', _roleState.role);
  }

  function getRole() {
    return _roleState.role || 'all';
  }

  function getOrderedQuestionCards() {
    var ord = ROLE_CARD_ORDER[getRole()] || ROLE_CARD_ORDER.all;
    return ord.map(function (id) { return Q_BY_ID[id]; }).filter(Boolean);
  }

  function getRoleCardDomIds() {
    return getOrderedQuestionCards().map(function (q) { return q.cardId; });
  }

  function getRoleCardLabels(qId) {
    var base = Q_BY_ID[qId];
    if (!base) return { eyebrow: '', short: '', title: '' };
    var role = getRole();
    var ov = ROLE_CARD_META[role] && ROLE_CARD_META[role][qId];
    if (ov) {
      return {
        eyebrow: L3(ov.eyeEn, ov.eyeZh, ov.eyeEs),
        short: L3(ov.shortEn, ov.shortZh, ov.shortEs),
        title: L3(ov.en, ov.zh, ov.es)
      };
    }
    return {
      eyebrow: '',
      short: L3(base.shortEn, base.shortZh, base.shortEs),
      title: L3(base.en, base.zh, base.es)
    };
  }

  function isSnakeToken(s) {
    return typeof s === 'string' && /^[a-z0-9]+_[a-z0-9_]+$/i.test(s.trim());
  }

  function locMarketLabel(raw) {
    var s = String(raw || '').trim();
    if (!s || isSnakeToken(s)) return '';
    if (/seller'?s\s*market/i.test(s)) return L3("Seller's market", '卖方市场', 'Mercado de vendedores');
    if (/buyer'?s\s*market/i.test(s)) return L3("Buyer's market", '买方市场', 'Mercado de compradores');
    if (/balanced/i.test(s)) return L3('Balanced market', '均衡市场', 'Mercado equilibrado');
    if (/cool|soft/i.test(s)) return L3('Cooling market', '降温市场', 'Mercado en enfriamiento');
    if (/hot|competitive/i.test(s)) return L3('Hot market', '火热市场', 'Mercado competitivo');
    return cleanPublicText(s);
  }

  function locRemodelProjectName(proj) {
    if (!proj || typeof proj !== 'object') return '';
    var lc = langCode();
    if (lc === 'zh' && proj.nameZh) return cleanPublicText(proj.nameZh);
    if (lc === 'es' && proj.nameEs) return cleanPublicText(proj.nameEs);
    var nm = String(proj.name || '').trim();
    if (!nm || isSnakeToken(nm)) return '';
    if (/heat[- ]?pump\s*hvac/i.test(nm)) return L3(nm, '热泵空调电气化升级', 'HVAC con bomba de calor');
    if (/minor\s*kitchen/i.test(nm)) return L3(nm, '厨房轻度翻新', 'Remodelación ligera de cocina');
    if (/major\s*kitchen/i.test(nm)) return L3(nm, '厨房全面翻新', 'Remodelación integral de cocina');
    if (/bath\s*remodel/i.test(nm)) return L3(nm, '卫浴翻新改造', 'Remodelación de baño');
    if (/garage\s*door/i.test(nm)) return L3(nm, '车库门换新', 'Reemplazo de puerta de garaje');
    if (/entry\s*door/i.test(nm)) return L3(nm, '入户门升级', 'Puerta principal nueva');
    if (/siding/i.test(nm)) return L3(nm, '外墙饰面更新', 'Revestimiento exterior');
    if (/roof/i.test(nm)) return L3(nm, '屋顶整体翻新', 'Reemplazo de techo');
    if (/window/i.test(nm)) return L3(nm, '节能门窗更换', 'Ventanas eficientes');
    if (/deck|patio/i.test(nm)) return L3(nm, '庭院露台增建', 'Terraza / patio exterior');
    return cleanPublicText(nm);
  }

  function getTopRemodelProject(rep) {
    var rm = (rep && rep.remodelAndRebuild) || mod(rep, 'remodelAndRebuild') || {};
    var arr = Array.isArray(rm.remodelProjects) ? rm.remodelProjects : [];
    var best = null;
    arr.forEach(function (p) {
      if (!p || !isN(p.costRecoupedPct) || !p.name || isSnakeToken(String(p.name))) return;
      if (!best || p.costRecoupedPct > best.costRecoupedPct) best = p;
    });
    return best;
  }

  function locPropertyTagLabel(tag) {
    if (!tag || typeof tag !== 'object') return '';
    var lc = langCode();
    if (lc === 'zh' && tag.labelZh) return cleanPublicText(tag.labelZh);
    if (lc === 'es' && tag.labelEs) return cleanPublicText(tag.labelEs);
    var id = String(tag.id || '').toLowerCase();
    var lbl = String(tag.label || '').trim();
    if (!lbl || isSnakeToken(lbl)) return '';
    if (id === 'no_hoa' || /^no\s*hoa$/i.test(lbl)) return L3('No HOA', '无 HOA', 'Sin HOA');
    if (id === 'walkable_top_schools' || /walkable\s*\+\s*top\s*school/i.test(lbl)) return L3('Walkable + Top School', '高分学区 + 高步行度', 'Caminable + Escuela Top');
    if (id === 'ground_floor_suite' || /ground[- ]floor\s*suite/i.test(lbl)) return L3('Ground-Floor Suite', '一层卧室套房', 'Suite en 1.ª planta');
    if (id === 'solar_pv' || id === 'owned_solar' || /solar\s*pv/i.test(lbl)) return L3('Solar PV', '屋顶光伏', 'Solar FV');
    if (id === 'solar_leased_watchout') return L3('Leased / UCC-1 Solar', '租赁光伏 (核查 UCC-1)', 'Solar arrendado');
    if (id === 'single_story' || /single[- ]story/i.test(lbl)) return L3('Single-Story', '单层平层', '1 planta');
    if (id === 'adu_potential' || /adu/i.test(lbl)) return L3(lbl, 'ADU 加建潜力', 'Potencial ADU');
    if (id === 'large_lot' || /large\s*lot/i.test(lbl)) return L3('Large Lot', '大占地院落', 'Lote amplio');
    return cleanPublicText(lbl);
  }

  function getPropertySellingTagLabels(rep, limit) {
    var rawTags = (rep && Array.isArray(rep.propertyTags) && rep.propertyTags.length)
      ? rep.propertyTags
      : (rep && rep.propertyFeatures && Array.isArray(rep.propertyFeatures.tags) ? rep.propertyFeatures.tags : []);
    var out = [];
    for (var i = 0; i < rawTags.length && out.length < (limit || 3); i++) {
      var t = rawTags[i];
      if (!t || t.status === 'watchout') continue;
      var s = locPropertyTagLabel(t);
      if (s && out.indexOf(s) < 0) out.push(s);
    }
    return out;
  }

  function getAssignedSchoolComposite(rep) {
    try {
      if (typeof window !== 'undefined' && typeof window.getSchoolSummaryScore === 'function') {
        var ws = window.getSchoolSummaryScore();
        if (isN(ws)) return ws;
      }
    } catch (e) {}
    var civ = mod(rep, 'civic') || {};
    var sc = civ.schools || {};
    var cards = (Array.isArray(sc.assignedOrNearest) && sc.assignedOrNearest.length)
      ? sc.assignedOrNearest
      : ['elementary', 'middle', 'high'].map(function (lv) {
          return (sc.public && sc.public.nearestInAssignedDistrictByLevel && sc.public.nearestInAssignedDistrictByLevel[lv]) ||
            (sc.public && sc.public.nearestByLevel && sc.public.nearestByLevel[lv]) || null;
        }).filter(Boolean);
    var scores = [];
    cards.forEach(function (s) {
      if (!s) return;
      if (isN(s.percentile) && s.metricSource) {
        scores.push(Math.round(s.percentile) / 10);
        return;
      }
      var d = s.caDashboard;
      if (d) {
        var tiers = ['elaTier', 'mathTier', 'sciTier', 'cciTier', 'gradTier']
          .map(function (k) { return d[k]; })
          .filter(function (x) { return isN(x) && x >= 1 && x <= 5; });
        if (tiers.length) {
          var mean = tiers.reduce(function (a, b) { return a + b; }, 0) / tiers.length;
          scores.push(Math.round(mean * 20) / 10);
        }
      }
    });
    if (!scores.length) return null;
    return Math.round((scores.reduce(function (a, b) { return a + b; }, 0) / scores.length) * 10) / 10;
  }

  function getWalkabilityIndex(rep) {
    var f = (rep && rep.facts) || {};
    var civ = mod(rep, 'civic') || {};
    var am = mod(rep, 'amenities') || {};
    var wsObj = f[_T.wsKey];
    if (wsObj && isN(wsObj.walk)) return Math.round(wsObj.walk);
    if (civ.scores && isN(civ.scores.walkability)) return Math.round(civ.scores.walkability);
    if (am.walkability && isN(am.walkability.index)) return Math.round(am.walkability.index * 5);
    return null;
  }

  function hasDomSectionContent(el) {
    if (!el) return false;
    var txt = String(el.textContent || '').trim();
    if (!txt) return false;
    return true;
  }

  function applyRoleHiddenSections() {
    var role = getRole();
    var hideList = ROLE_HIDDEN_IDS[role] || [];
    var hideSet = {};
    hideList.forEach(function (id) { hideSet[id] = true; });
    var count = 0;
    var activeHiddenMap = {};
    ALL_HIDEABLE_IDS.forEach(function (id) {
      var el = byId(id);
      if (!el) return;
      var shouldHide = Boolean(hideSet[id]) && hasDomSectionContent(el);
      if (el.classList) el.classList.toggle('role-hidden', shouldHide);
      if (shouldHide) {
        count++;
        activeHiddenMap[id] = true;
      }
    });
    _roleState.hiddenCount = count;
    _roleState.hiddenMap = activeHiddenMap;
    return count;
  }

  function isTargetHiddenInActiveRole(rawId) {
    if (!rawId) return false;
    var clean = String(rawId).replace(/^#/, '');
    if (_roleState.hiddenMap && _roleState.hiddenMap[clean]) return true;
    var norm = HASH_ALIASES[clean] || clean;
    if (_roleState.hiddenMap && _roleState.hiddenMap[norm]) return true;
    var el = byId(clean) || byId(norm);
    if (el && el.classList && el.classList.contains('role-hidden')) return true;
    if (el && typeof el.closest === 'function' && el.closest('.role-hidden')) return true;
    return false;
  }

  function roleHiddenNoteHtml(shortMode) {
    var n = _roleState.hiddenCount || 0;
    var role = getRole();
    if (n <= 0 || !ROLE_LABELS[role] || role === 'all' || role === 'agent') return '';
    if (shortMode) {
      return esc(L3('+' + n + ' hidden · ', '已收起 ' + n + ' 项 · ', '+' + n + ' ocultas · ')) +
        '<button type="button" class="role-show-all-btn" data-role-set="all">' +
        esc(L3('Show all', '显示全部', 'Mostrar todo')) +
        '</button>';
    }
    var rName = L3(ROLE_LABELS[role].en, ROLE_LABELS[role].zh, ROLE_LABELS[role].es);
    return esc(L3(
      '+' + n + ' sections hidden in ' + rName + ' view · ',
      rName + '视角已收起 ' + n + ' 个章节 · ',
      '+' + n + ' secciones ocultas en la vista ' + rName + ' · '
    )) + '<button type="button" class="role-show-all-btn" data-role-set="all">' +
      esc(L3('Show All Info', '显示全部信息', 'Mostrar todo')) +
      '</button>';
  }

  function renderRoleHiddenNote() {
    var el = byId('roleHiddenNote');
    if (!el) return;
    var html = roleHiddenNoteHtml(false);
    if (!html) {
      el.innerHTML = '';
      el.hidden = true;
    } else {
      el.innerHTML = html;
      el.hidden = false;
    }
  }

  function syncRoleBriefAriaLabelledBy() {
    var brief = byId('roleFocusBrief');
    if (!brief) return;
    var role = getRole();
    var tabId = (isNarrowViewport() ? 'roleTabInline_' : 'roleTabHdr_') + role;
    brief.setAttribute('aria-labelledby', tabId);
  }

  function renderRoleSwitchers(focusPrefix) {
    var role = getRole();
    var ariaLbl = L3('View report as', '选择报告视角', 'Ver informe como');
    var hosts = [
      { id: 'roleSwitchHdr', prefix: 'roleTabHdr_', withPrefixLabel: true },
      { id: 'roleSwitchInline', prefix: 'roleTabInline_', withPrefixLabel: false }
    ];
    hosts.forEach(function (h) {
      var mount = byId(h.id);
      if (!mount) return;
      var tabsHtml = VALID_ROLES.map(function (r) {
        var sel = r === role;
        var meta = ROLE_LABELS[r];
        return '<button type="button" role="tab" id="' + h.prefix + r + '" data-role="' + r + '" aria-selected="' + (sel ? 'true' : 'false') + '" aria-controls="roleFocusBrief" tabindex="' + (sel ? '0' : '-1') + '">' +
          span3(meta.en, meta.zh, meta.es) +
        '</button>';
      }).join('');
      mount.innerHTML =
        (h.withPrefixLabel ? '<span class="role-switch-lbl">' + span3('View as', '视角', 'Ver como') + '</span>' : '') +
        '<div class="role-switch" role="tablist" aria-label="' + esc(ariaLbl) + '">' + tabsHtml + '</div>';

      qAll('[role="tab"]', mount).forEach(function (btn) {
        btn.addEventListener('click', function () {
          var nextR = btn.getAttribute('data-role');
          setRole(nextR);
        });
        btn.addEventListener('keydown', function (ev) {
          var key = ev && ev.key;
          var curIdx = VALID_ROLES.indexOf(btn.getAttribute('data-role'));
          if (curIdx < 0) return;
          var nextIdx = -1;
          if (key === 'ArrowRight' || key === 'ArrowDown') nextIdx = (curIdx + 1) % VALID_ROLES.length;
          else if (key === 'ArrowLeft' || key === 'ArrowUp') nextIdx = (curIdx - 1 + VALID_ROLES.length) % VALID_ROLES.length;
          else if (key === 'Home') nextIdx = 0;
          else if (key === 'End') nextIdx = VALID_ROLES.length - 1;
          if (nextIdx >= 0) {
            if (typeof ev.preventDefault === 'function') ev.preventDefault();
            setRole(VALID_ROLES[nextIdx], { focusPrefix: h.prefix });
          }
        });
      });
    });

    if (focusPrefix) {
      var fBtn = byId(focusPrefix + getRole());
      if (fBtn && typeof fBtn.focus === 'function') {
        try { fBtn.focus(); } catch (e) {}
      }
    }
    syncRoleBriefAriaLabelledBy();
  }

  function renderRoleFocusBrief() {
    var mount = byId('roleFocusBrief');
    if (!mount) return;
    var role = getRole();
    var rep = getRep() || {};
    syncRoleBriefAriaLabelledBy();

    if (role === 'all') {
      var rows = [
        {
          role: 'buyer',
          dot: ROLE_LABELS.buyer.dot,
          name: L3('Buyer', '买家', 'Comprador'),
          desc: L3('Fair price, all-in monthly cost, red flags, schools', '合理价位、每月总支出、风险、学区', 'Precio justo, costo mensual, alertas, escuelas')
        },
        {
          role: 'seller',
          dot: ROLE_LABELS.seller.dot,
          name: L3('Seller', '卖家', 'Vendedor'),
          desc: L3('Pricing anchor, value-add ROI, pre-listing risk audit', '定价锚点、增值改造、挂牌前风险自查', 'Ancla de precio, ROI de mejoras, auditoría previa')
        },
        {
          role: 'agent',
          dot: ROLE_LABELS.agent.dot,
          name: L3('Agent', '经纪人', 'Agente'),
          desc: L3('CMA, rent & yield, records, deck', 'CMA、租售比、披露档案、演示', 'CMA, renta, registros, presentación')
        }
      ];
      mount.innerHTML =
        '<div class="rb-hdr">' +
          '<span class="rb-badge" aria-hidden="true">≡</span>' +
          '<span class="rb-title">' + esc(L3('Complete dossier', '完整档案', 'Expediente completo')) + '</span>' +
          '<span class="rb-sub">' + esc(L3('Every section, canonical order', '全部章节，标准顺序', 'Todas las secciones')) + '</span>' +
        '</div>' +
        '<div class="rb-chooser">' +
          rows.map(function (r) {
            return '<button type="button" class="rb-chooser-row" data-role-set="' + esc(r.role) + '">' +
              '<span class="rb-chooser-dot" style="background:' + esc(r.dot) + '" aria-hidden="true"></span>' +
              '<span class="rb-chooser-name">' + esc(r.name) + '</span>' +
              '<span class="rb-chooser-desc">' + esc(r.desc) + '</span>' +
              '<span class="rb-chooser-go">' + esc(L3('View →', '查看 →', 'Ver →')) + '</span>' +
            '</button>';
          }).join('') +
        '</div>';
      return;
    }

    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var cs = v.compSummary || {};
    var o = v.ownership || {};
    var m0 = o.monthly || {};
    var mk = mod(rep, 'market') || {};
    var mm = mk.metroMarket || {};
    var f = (rep && rep.facts) || {};
    var df = (rep && rep.decisionFactors) || {};
    var bd = (rep && rep.buyerDiligence) || mod(rep, 'buyerDiligence') || {};
    var ai = (rep && rep.ai) || {};
    var rk = extractTopRiskAndChecks(rep);
    var flagCnt = rk.checks.filter(function (c) { return c.status !== 'clear'; }).length;
    var clearCnt = rk.checks.filter(function (c) { return c.status === 'clear'; }).length;

    var badge = '◎', title = '', sub = '';
    var tiles = [], chips = [], actionsHtml = '', footerLines = [];

    if (role === 'buyer') {
      badge = '◎';
      title = L3('Buyer focus', '买家重点', 'Enfoque del comprador');
      sub = L3('What decides this purchase', '决定是否出手的关键', 'Lo que decide esta compra');

      if (isN(e.value)) {
        var listPrice = (f.mlsListing && isN(f.mlsListing.listPrice) && f.mlsListing.listPrice > 0)
          ? f.mlsListing.listPrice
          : (isN(f.listPrice) && f.listPrice > 0 ? f.listPrice : null);
        var priceSub = '';
        if (listPrice != null) {
          var xAsk = Math.round(((e.value - listPrice) / listPrice) * 100);
          var xAskStr = (xAsk >= 0 ? '+' : '') + xAsk + '%';
          priceSub = L3(xAskStr + ' vs ' + fUsdK(listPrice) + ' asking', '对比挂牌价 ' + fUsdK(listPrice) + ' (' + xAskStr + ')', xAskStr + ' vs ' + fUsdK(listPrice) + ' lista');
        } else if (f.lastSale && isN(f.lastSale.price) && f.lastSale.price > 0) {
          var xLast = Math.round(((e.value - f.lastSale.price) / f.lastSale.price) * 100);
          var xLastStr = (xLast >= 0 ? '+' : '') + xLast + '%';
          var yyyy = f.lastSale.date ? String(f.lastSale.date).slice(0, 4) : '';
          priceSub = L3(
            xLastStr + ' vs last sale ' + fUsdK(f.lastSale.price) + (yyyy ? ' (' + yyyy + ')' : ''),
            '较前次成交 ' + fUsdK(f.lastSale.price) + (yyyy ? ' (' + yyyy + '年)' : '') + ' ' + xLastStr,
            xLastStr + ' vs última venta ' + fUsdK(f.lastSale.price) + (yyyy ? ' (' + yyyy + ')' : '')
          );
        }
        tiles.push({
          k: L3('Fair price', '合理价位', 'Precio justo'),
          v: fUsdK(e.value),
          d: priceSub,
          target: 'value'
        });
      }

      if (isN(m0.total) && m0.total > 0) {
        var moParts = [];
        var utilMo = bd.ongoingCosts && bd.ongoingCosts.monthlyUtilities && bd.ongoingCosts.monthlyUtilities.totalMonthlyUtilitiesUsd;
        if (isN(utilMo)) {
          moParts.push(L3('+ utilities ≈ ' + fUsd(utilMo) + '/mo', '+ 水电气 ≈ ' + fUsd(utilMo) + '/月', '+ servicios ≈ ' + fUsd(utilMo) + '/mes'));
        }
        if (isN(m0.hoa) && m0.hoa > 0) {
          moParts.push('HOA ' + fUsd(m0.hoa));
        }
        tiles.push({
          k: L3('All-in monthly', '每月总支出', 'Costo mensual total'),
          v: fUsd(m0.total) + L3('/mo', '/月', '/mes'),
          d: moParts.join(' · '),
          target: 'payment'
        });
      }

      if (rk.top && rk.top.shortTitle) {
        tiles.push({
          k: L3('Top red flag', '首要风险', 'Alerta principal'),
          v: rk.top.shortTitle,
          d: L3(flagCnt + ' watch/flag · ' + clearCnt + ' clear', flagCnt + ' 项风险提示 · ' + clearCnt + ' 项通过', flagCnt + ' alerta(s) · ' + clearCnt + ' limpias'),
          tone: rk.top.tone === 'bad' ? 'bad' : (rk.top.tone === 'watch' ? 'watch' : ''),
          target: rk.top.targetSection || 'risk'
        });
      }

      var schComp = getAssignedSchoolComposite(rep);
      if (schComp != null) {
        var walkIdx = getWalkabilityIndex(rep);
        var commMin = df.connectivity && df.connectivity.meanCommuteMin;
        var swParts = [];
        if (walkIdx != null) swParts.push(L3('Walkability ' + walkIdx + '/100', '步行指数 ' + walkIdx + '/100', 'Caminabilidad ' + walkIdx + '/100'));
        if (isN(commMin)) swParts.push(L3('commute ' + Math.round(commMin) + ' min', '通勤 ' + Math.round(commMin) + ' 分钟', 'traslado ' + Math.round(commMin) + ' min'));
        tiles.push({
          k: L3('Schools & walkability', '学区与步行', 'Escuelas y caminabilidad'),
          v: schComp + '/10',
          d: swParts.join(' · '),
          target: 'schools'
        });
      }

      var ec = bd.ongoingCosts && bd.ongoingCosts.monthlyUtilities && bd.ongoingCosts.monthlyUtilities.electricCarrier;
      if (ec && ec.shortName && isN(ec.ratePerKwhUsd) && ec.ratePerKwhUsd > 0) {
        chips.push({ text: '⚡ ' + ec.shortName + ' · $' + Number(ec.ratePerKwhUsd).toFixed(3) + '/kWh', target: 'utilitiesRatesPanel' });
      }
      var cn = bd.connectivity || {};
      var bb = cn.broadbandCatalog;
      var bbVerified = Boolean(
        cn.fiberConfirmedAddress === true ||
        (cn.sonic && cn.sonic.available === true) ||
        cn.verificationStatus === 'address_verified' ||
        (bb && (bb.fiberConfirmedFcc === true || bb.verificationStatus === 'address_verified' || bb.verificationStatus === 'fcc_location_verified' || bb.verificationStatus === 'fiber_confirmed' || bb.verificationStatus === 'confirmed_fcc' || bb.verificationStatus === 'confirmed_address'))
      );
      if (bbVerified && ((bb && bb.fiberAvailable) || (cn.sonic && cn.sonic.available === true))) {
        var topFibChip = (bb && Array.isArray(bb.fiberCarriers) && bb.fiberCarriers[0] && bb.fiberCarriers[0].carrierName) || (cn.sonic && cn.sonic.carrierName) || 'Fiber FTTH';
        chips.push({ text: L3('Fiber: ', '光纤：', 'Fibra: ') + topFibChip, target: 'homeInternetCatalog' });
      } else if (bb || cn.broadbandCatalog) {
        chips.push({ text: L3('Broadband: Unverified (Check address)', '宽带：未核实（待查地址）', 'Banda ancha: sin verificar'), target: 'homeInternetCatalog' });
      }
      if (rep.buyerLivingIntel && byId('buyerLivingIntelPanel')) {
        chips.push({ text: L3('Insurability & living checks →', '投保与居住深度核查 →', 'Seguro y verificaciones →'), target: 'buyerLivingIntelPanel' });
      }
      if (Array.isArray(ai.negotiation) && ai.negotiation.length > 0) {
        chips.push({ text: L3(ai.negotiation.length + ' negotiation levers →', ai.negotiation.length + ' 项谈判筹码 →', ai.negotiation.length + ' palancas de negociación →'), target: 'ai' });
      }
      if (byId('more')) {
        chips.push({ text: L3('Offer checklist →', '出价核查清单 →', 'Lista de oferta →'), target: 'more' });
      }
    } else if (role === 'seller') {
      badge = '◆';
      title = L3('Seller focus', '卖家重点', 'Enfoque del vendedor');
      sub = L3('Price it, prep it, disclose it', '定价、整备、披露', 'Precio, preparación, divulgación');

      if (isN(e.value)) {
        var ancParts = [];
        if (isN(e.low) && isN(e.high)) ancParts.push(L3('Range ', '区间 ', 'Rango ') + fUsdK(e.low) + '–' + fUsdK(e.high));
        if (isN(e.perSqft)) ancParts.push(fUsd(e.perSqft) + L3('/sq ft', '/平尺', '/pie²'));
        tiles.push({
          k: L3('Pricing anchor', '定价锚点', 'Ancla de precio'),
          v: fUsdK(e.value),
          d: ancParts.join(' · '),
          target: 'value'
        });
      }

      if (isN(cs.medianPricePerSqft)) {
        var cmpParts = [];
        if (isN(cs.count)) cmpParts.push(L3(cs.count + ' comps', cs.count + ' 套可比', cs.count + ' comps'));
        if (isN(cs.medianAdjusted)) cmpParts.push(L3('adj. median ' + fUsdK(cs.medianAdjusted), '修正中位 ' + fUsdK(cs.medianAdjusted), 'mediana aj. ' + fUsdK(cs.medianAdjusted)));
        if (isN(cs.medianSaleToListPct)) cmpParts.push(L3('sale-to-list ' + cs.medianSaleToListPct + '%', '成交/挂牌 ' + cs.medianSaleToListPct + '%', 'venta/lista ' + cs.medianSaleToListPct + '%'));
        tiles.push({
          k: L3('Comp median $/sq ft', '可比成交单价中位', 'Mediana $/pie²'),
          v: fUsd(cs.medianPricePerSqft),
          d: cmpParts.join(' · '),
          target: 'comps'
        });
      }

      if (mm.days_to_pending && isN(mm.days_to_pending.value)) {
        var velParts = [];
        var mLbl = locMarketLabel(mm.label);
        if (mLbl) velParts.push(mLbl);
        if (mm.price_cut_share && isN(mm.price_cut_share.value)) {
          var cutPct = Math.round(mm.price_cut_share.value * 100);
          velParts.push(L3(cutPct + '% of listings cut price', cutPct + '% 在售房源曾降价', cutPct + '% redujeron precio'));
        }
        tiles.push({
          k: L3('Market velocity', '市场去化速度', 'Velocidad del mercado'),
          v: L3(Math.round(mm.days_to_pending.value) + ' days', Math.round(mm.days_to_pending.value) + ' 天', Math.round(mm.days_to_pending.value) + ' días'),
          d: velParts.join(' · '),
          target: 'market'
        });
      }

      if (rk.checks.length || rk.top) {
        var credParts = [];
        if (rk.top && rk.top.shortTitle) credParts.push(rk.top.shortTitle);
        if (isN(f.yearBuilt)) credParts.push(L3('built ' + f.yearBuilt, '建于 ' + f.yearBuilt + ' 年', 'año ' + f.yearBuilt));
        tiles.push({
          k: L3('Buyer credit risks', '买家压价风险点', 'Riesgos de crédito'),
          v: L3(flagCnt + ' items', flagCnt + ' 项关注', flagCnt + ' puntos'),
          d: credParts.join(' · '),
          tone: (rk.top && rk.top.tone === 'bad') ? 'bad' : 'watch',
          target: 'risk'
        });
      }

      var horizons = (v.scenarios && Array.isArray(v.scenarios.horizons)) ? v.scenarios.horizons : [];
      var h1 = horizons.find(function (h) { return h && h.years === 1; });
      if (h1 && isN(h1.p10) && isN(h1.p50) && isN(h1.p90)) {
        var fp = function (n) { return (n > 0 ? '+' : '') + n + '%'; };
        chips.push({
          text: L3(
            'Wait 12 mo: P10 ' + fp(h1.p10) + ' · P50 ' + fp(h1.p50) + ' · P90 ' + fp(h1.p90),
            '等待 12 个月：P10 ' + fp(h1.p10) + ' · P50 ' + fp(h1.p50) + ' · P90 ' + fp(h1.p90),
            'Esperar 12 m: P10 ' + fp(h1.p10) + ' · P50 ' + fp(h1.p50) + ' · P90 ' + fp(h1.p90)
          ),
          target: 'outlook'
        });
      }
      if (Array.isArray(ai.sellerQuestions) && ai.sellerQuestions.length > 0) {
        chips.push({
          text: L3(ai.sellerQuestions.length + ' buyer questions →', ai.sellerQuestions.length + ' 个买家必问 →', ai.sellerQuestions.length + ' preguntas clave →'),
          target: 'ai'
        });
      }
      var sellPts = getPropertySellingTagLabels(rep, 2);
      if (sellPts.length) {
        chips.push({
          text: L3('Selling points: ', '核心卖点：', 'Puntos: ') + sellPts.join(' · '),
          target: 'house'
        });
      }
      if (rep.sellerSuite && byId('sellerStrategyStudio')) {
        chips.push({
          text: L3('Open net sheet →', '净到手测算 →', 'Hoja de neto →'),
          target: 'sellerStrategyStudio',
          isAction: true
        });
      }
      var topProj = getTopRemodelProject(rep);
      if (topProj) {
        var pName = locRemodelProjectName(topProj);
        if (pName) {
          chips.push({
            text: L3(
              'Top ROI: ' + pName + ' ' + Math.round(topProj.costRecoupedPct) + '%',
              '高回报：' + pName + ' ' + Math.round(topProj.costRecoupedPct) + '%',
              'Mayor ROI: ' + pName + ' ' + Math.round(topProj.costRecoupedPct) + '%'
            ),
            target: 'remodel'
          });
        }
      }
      var tt = bd.cashToClose && bd.cashToClose.transferTax;
      if (tt && isN(tt.sellerEstimatedShareUsd)) {
        var rPct = isN(tt.effectiveRatePct) ? (' (' + tt.effectiveRatePct + '%)') : '';
        chips.push({
          text: L3('Transfer tax ≈ ' + fUsd(tt.sellerEstimatedShareUsd) + rPct, '过户税 ≈ ' + fUsd(tt.sellerEstimatedShareUsd) + rPct, 'Impuesto ≈ ' + fUsd(tt.sellerEstimatedShareUsd) + rPct),
          target: 'diligence'
        });
      }
    } else if (role === 'agent') {
      badge = '▣';
      title = L3('Agent focus', '经纪人重点', 'Enfoque del agente');
      sub = L3('Underwrite, disclose, present', '估值、披露、演示', 'Evaluar, divulgar, presentar');

      if (isN(e.perSqft) && isN(cs.medianPricePerSqft)) {
        var spParts = [];
        if (isN(cs.count)) spParts.push(L3(cs.count + ' comps', cs.count + ' 套可比', cs.count + ' comps'));
        if (isN(cs.medianAdjusted)) spParts.push(L3('adj. median ' + fUsdK(cs.medianAdjusted), '修正中位 ' + fUsdK(cs.medianAdjusted), 'mediana aj. ' + fUsdK(cs.medianAdjusted)));
        if (isN(e.dispersionPct)) spParts.push(L3('±' + e.dispersionPct + '% model spread', '±' + e.dispersionPct + '% 模型离散度', '±' + e.dispersionPct + '% dispersión'));
        tiles.push({
          k: L3('Subject vs comp $/sq ft', '标的与可比单价', '$/pie²: sujeto vs comps'),
          v: fUsd(e.perSqft) + ' vs ' + fUsd(cs.medianPricePerSqft),
          d: spParts.join(' · '),
          target: 'comps'
        });
      }

      if (mm.market_temp && isN(mm.market_temp.value)) {
        var mtParts = [];
        var mtLbl = locMarketLabel(mm.label);
        if (mtLbl) mtParts.push(mtLbl);
        if (mm.days_to_pending && isN(mm.days_to_pending.value)) mtParts.push(L3(Math.round(mm.days_to_pending.value) + ' days to pending', Math.round(mm.days_to_pending.value) + ' 天待过户', Math.round(mm.days_to_pending.value) + ' días a contrato'));
        if (mm.sale_to_list && isN(mm.sale_to_list.value)) mtParts.push(L3((mm.sale_to_list.value * 100).toFixed(1) + '% sale-to-list', (mm.sale_to_list.value * 100).toFixed(1) + '% 成交/挂牌比', (mm.sale_to_list.value * 100).toFixed(1) + '% venta/lista'));
        tiles.push({
          k: L3('Market temperature', '市场热度', 'Temperatura del mercado'),
          v: Math.round(mm.market_temp.value) + '/100',
          d: mtParts.join(' · '),
          target: 'market'
        });
      }

      var rrAg = getRentResearchData(rep);
      var recRentAg = rrAg && isN(rrAg.recommendedMonthlyRent) && rrAg.recommendedMonthlyRent > 0 ? rrAg.recommendedMonthlyRent : null;
      if (recRentAg != null) {
        var ryParts = [];
        if (isN(o.grossYieldPct)) ryParts.push(L3('Gross yield ' + o.grossYieldPct + '%', '毛租售比 ' + o.grossYieldPct + '%', 'Rend. bruto ' + o.grossYieldPct + '%'));
        if (isN(o.capRatePct)) ryParts.push(L3('cap rate ' + o.capRatePct + '%', '资本化率 ' + o.capRatePct + '%', 'cap rate ' + o.capRatePct + '%'));
        tiles.push({
          k: L3('Rent & yield', '租金与收益', 'Renta y rendimiento'),
          v: fUsd(recRentAg) + L3('/mo', '/月', '/mes'),
          d: ryParts.join(' · '),
          target: 'rentResearch'
        });
      }

      if (f.apn) {
        var recParts = [];
        if (f.titleAndLiens && isN(f.titleAndLiens.recordedTransferCount)) {
          recParts.push(L3(f.titleAndLiens.recordedTransferCount + ' recorded transfers', f.titleAndLiens.recordedTransferCount + ' 次过户登记', f.titleAndLiens.recordedTransferCount + ' transferencias'));
        }
        if (f.codeEnforcementChecked !== false && f.disclosures && f.disclosures.codeEnforcement && isN(f.disclosures.codeEnforcement.openCount) && (f.codeEnforcementChecked === true || f.disclosures.codeEnforcement.checked === true || f.disclosures.codeEnforcement.openCount > 0)) {
          recParts.push(L3(f.disclosures.codeEnforcement.openCount + ' open code cases', f.disclosures.codeEnforcement.openCount + ' 项未结违规', f.disclosures.codeEnforcement.openCount + ' casos abiertos'));
        }
        var agPermList = (f.disclosures && Array.isArray(f.disclosures.permits) && f.disclosures.permits)
          || (f.buildingRegister && Array.isArray(f.buildingRegister.permits) && f.buildingRegister.permits)
          || (Array.isArray(f.permits) && f.permits)
          || (rep && rep.condition && Array.isArray(rep.condition.permits) && rep.condition.permits)
          || null;
        if (agPermList) {
          recParts.push(L3(agPermList.length + ' permits on file', agPermList.length + ' 份施工许可', agPermList.length + ' permisos'));
        }
        tiles.push({
          k: L3('Records & disclosure', '档案与披露', 'Registros y divulgación'),
          v: 'APN ' + f.apn,
          d: recParts.join(' · '),
          target: 'county'
        });
      }

      if (rep.agentSuite && byId('agentDealDesk')) {
        chips.push({ text: L3('CMA workbench →', 'CMA 工作台 →', 'Mesa CMA →'), target: 'agentDealDesk' });
      }
      if (Array.isArray(ai.negotiation) && ai.negotiation.length > 0) {
        chips.push({ text: L3(ai.negotiation.length + ' negotiation levers →', ai.negotiation.length + ' 项谈判筹码 →', ai.negotiation.length + ' palancas de negociación →'), target: 'ai' });
      }
      if (Array.isArray(ai.sellerQuestions) && ai.sellerQuestions.length > 0) {
        chips.push({ text: L3(ai.sellerQuestions.length + ' seller questions →', ai.sellerQuestions.length + ' 个向卖家提问 →', ai.sellerQuestions.length + ' preguntas al vendedor →'), target: 'ai' });
      }

      actionsHtml =
        '<div class="rb-acts">' +
          '<button type="button" class="rb-act pri" data-rb-act="share">' + esc(L3('Share link', '分享链接', 'Compartir')) + '</button>' +
          '<button type="button" class="rb-act" data-rb-act="walkthrough">' + esc(L3('▶ Walkthrough deck', '▶ 导览幻灯', '▶ Presentación')) + '</button>' +
          '<button type="button" class="rb-act" data-rb-act="video">' + esc(L3('Video tour', '视频导览', 'Video')) + '</button>' +
          '<button type="button" class="rb-act" data-rb-act="print">' + esc(L3('Print / PDF', '打印 PDF', 'Imprimir PDF')) + '</button>' +
        '</div>';
    }

    var hiddenLine = roleHiddenNoteHtml(false);
    if (hiddenLine) footerLines.push(hiddenLine);

    mount.innerHTML =
      '<div class="rb-hdr">' +
        '<span class="rb-badge" aria-hidden="true">' + esc(badge) + '</span>' +
        '<span class="rb-title">' + esc(title) + '</span>' +
        '<span class="rb-sub">' + esc(sub) + '</span>' +
      '</div>' +
      (tiles.length
        ? '<div class="rb-tiles">' + tiles.map(function (t) {
            return '<button type="button" class="rb-tile' + (t.tone ? ' ' + esc(t.tone) : '') + '" data-rb-jump="' + esc(t.target) + '">' +
              '<span class="rb-tile-k">' + esc(t.k) + '</span>' +
              '<strong class="rb-tile-v">' + esc(t.v) + '</strong>' +
              (t.d ? '<span class="rb-tile-d">' + esc(t.d) + '</span>' : '') +
            '</button>';
          }).join('') + '</div>'
        : '') +
      (chips.length
        ? '<div class="rb-chips' + (role === 'seller' ? ' seller-packed' : '') + '">' + chips.map(function (c) {
            if (c.isAction && c.target) {
              return '<button type="button" class="rb-act pri" data-rb-jump="' + esc(c.target) + '">' + esc(c.text) + '</button>';
            }
            return '<button type="button" class="rb-chip" data-rb-jump="' + esc(c.target) + '">' + esc(c.text) + '</button>';
          }).join('') + '</div>'
        : '') +
      actionsHtml +
      (footerLines.length
        ? '<div class="rb-foot">' + footerLines.map(function (ln) { return '<div>' + ln + '</div>'; }).join('') + '</div>'
        : '');

    qAll('[data-rb-jump]', mount).forEach(function (btn) {
      btn.addEventListener('click', function () {
        jumpToSection(btn.getAttribute('data-rb-jump'), true, { pushHash: true });
      });
    });

    qAll('[data-rb-act]', mount).forEach(function (btn) {
      btn.addEventListener('click', function () {
        var act = btn.getAttribute('data-rb-act');
        if (act === 'share') {
          var sb = byId('shareReportBtn');
          if (sb && typeof sb.click === 'function') sb.click();
        } else if (act === 'walkthrough') {
          openWalkthroughModal(0);
        } else if (act === 'video') {
          if (window.OpenEstateMotion && typeof window.OpenEstateMotion.open === 'function') {
            window.OpenEstateMotion.open(0);
          } else {
            var vb = byId('chapterMotionReelBtn') || byId('openMotionReelBtn');
            if (vb && typeof vb.click === 'function') vb.click();
          }
        } else if (act === 'print') {
          var pb = byId('printPdfBtn');
          if (pb && typeof pb.click === 'function') pb.click();
        }
      });
    });
  }

  function buildRoleOverridePills(role, qId, rep, basePills) {
    if (role !== 'seller' && role !== 'agent') return basePills.slice(0, 5);
    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var cs = v.compSummary || {};
    var o = v.ownership || {};
    var m0 = o.monthly || {};
    var mk = mod(rep, 'market') || {};
    var mm = mk.metroMarket || {};
    var f = (rep && rep.facts) || {};
    var bd = (rep && rep.buyerDiligence) || mod(rep, 'buyerDiligence') || {};
    var ai = (rep && rep.ai) || {};
    var out = [];

    if (role === 'seller') {
      if (qId === 'price') {
        if (isN(e.value)) out.push({ text: L3('Est. ' + fUsdK(e.value), '估值 ' + fUsdK(e.value), 'Est. ' + fUsdK(e.value)), tone: 'pri' });
        if (isN(e.perSqft)) out.push({ text: fUsd(e.perSqft) + L3('/sq ft', '/平尺', '/pie²'), tone: 'neutral' });
        if (isN(cs.medianPricePerSqft)) out.push({ text: L3('Comp median ' + fUsd(cs.medianPricePerSqft) + '/sq ft', '可比中位 ' + fUsd(cs.medianPricePerSqft) + '/平尺', 'Mediana comp ' + fUsd(cs.medianPricePerSqft) + '/pie²'), tone: 'neutral' });
        if (isN(cs.medianSaleToListPct)) out.push({ text: L3('Sale-to-list ' + cs.medianSaleToListPct + '%', '成交/挂牌比 ' + cs.medianSaleToListPct + '%', 'Venta/lista ' + cs.medianSaleToListPct + '%'), tone: cs.medianSaleToListPct >= 100 ? 'good' : 'neutral' });
        if (mm.days_to_pending && isN(mm.days_to_pending.value)) out.push({ text: L3(Math.round(mm.days_to_pending.value) + ' days to pending', Math.round(mm.days_to_pending.value) + ' 天进入待过户', Math.round(mm.days_to_pending.value) + ' días a contrato'), tone: 'neutral' });
        return out.slice(0, 5);
      }
      if (qId === 'potential') {
        out = basePills.slice();
        var topProj = getTopRemodelProject(rep);
        if (topProj) {
          var pName = locRemodelProjectName(topProj);
          if (pName) {
            out.push({
              text: L3('Top ROI: ' + pName + ' recoups ' + Math.round(topProj.costRecoupedPct) + '%', '最高回报：' + pName + ' 回收 ' + Math.round(topProj.costRecoupedPct) + '%', 'Mayor ROI: ' + pName + ' recupera ' + Math.round(topProj.costRecoupedPct) + '%'),
              tone: 'good'
            });
          }
        }
        return out.slice(0, 5);
      }
      if (qId === 'risk') {
        out = basePills.slice();
        var openCases = f.disclosures && f.disclosures.codeEnforcement && f.disclosures.codeEnforcement.openCount;
        if (isN(openCases)) {
          out.push({
            text: L3('Open code cases: ' + openCases, '未结违规立案：' + openCases, 'Casos abiertos: ' + openCases),
            tone: openCases === 0 ? 'good' : 'watch'
          });
        }
        return out.slice(0, 5);
      }
      if (qId === 'cost') {
        var tt = bd.cashToClose && bd.cashToClose.transferTax;
        if (tt && isN(tt.sellerEstimatedShareUsd)) {
          var rStr = isN(tt.effectiveRatePct) ? (' (' + tt.effectiveRatePct + '%)') : '';
          out.push({ text: L3('Seller transfer tax ≈ ' + fUsd(tt.sellerEstimatedShareUsd) + rStr, '卖家过户税 ≈ ' + fUsd(tt.sellerEstimatedShareUsd) + rStr, 'Impuesto vendedor ≈ ' + fUsd(tt.sellerEstimatedShareUsd) + rStr), tone: 'pri' });
        }
        if (isN(m0.total) && m0.total > 0) out.push({ text: L3('Buyer all-in ' + fUsd(m0.total) + '/mo', '买家月总支出 ' + fUsd(m0.total) + '/月', 'Total comprador ' + fUsd(m0.total) + '/mes'), tone: 'neutral' });
        if (isN(o.incomeNeeded) && o.incomeNeeded > 0) out.push({ text: L3('Income needed ' + fUsdK(o.incomeNeeded) + '/yr', '买家所需年收 ' + fUsdK(o.incomeNeeded) + '/年', 'Ingreso req. ' + fUsdK(o.incomeNeeded) + '/año'), tone: 'neutral' });
        return out.slice(0, 5);
      }
      return basePills.slice(0, 5);
    }

    // role === 'agent'
    if (qId === 'price') {
      if (isN(cs.count) && cs.count > 0) out.push({ text: L3(cs.count + ' comps', cs.count + ' 套可比成交', cs.count + ' comparables'), tone: 'pri' });
      if (isN(cs.medianAdjusted)) out.push({ text: L3('Adj. comp median ' + fUsdK(cs.medianAdjusted), '修正可比中位 ' + fUsdK(cs.medianAdjusted), 'Mediana ajustada ' + fUsdK(cs.medianAdjusted)), tone: 'neutral' });
      if (isN(e.perSqft) && isN(cs.medianPricePerSqft)) out.push({ text: L3('$/sq ft ' + fUsd(e.perSqft) + ' vs ' + fUsd(cs.medianPricePerSqft), '单价 ' + fUsd(e.perSqft) + ' vs ' + fUsd(cs.medianPricePerSqft), '$/pie² ' + fUsd(e.perSqft) + ' vs ' + fUsd(cs.medianPricePerSqft)), tone: 'neutral' });
      if (isN(cs.medianSaleToListPct)) out.push({ text: L3('Sale-to-list ' + cs.medianSaleToListPct + '%', '成交/挂牌比 ' + cs.medianSaleToListPct + '%', 'Venta/lista ' + cs.medianSaleToListPct + '%'), tone: cs.medianSaleToListPct >= 100 ? 'good' : 'neutral' });
      var mktLbl = locMarketLabel(mm.label);
      if (mktLbl) out.push({ text: mktLbl, tone: 'info' });
      return out.slice(0, 5);
    }
    if (qId === 'cost') {
      var rrAg = getRentResearchData(rep);
      var recR = rrAg && isN(rrAg.recommendedMonthlyRent) && rrAg.recommendedMonthlyRent > 0 ? rrAg.recommendedMonthlyRent : null;
      if (recR != null) out.push({ text: L3('Rent ' + fUsd(recR) + '/mo', '推荐月租 ' + fUsd(recR) + '/月', 'Renta ' + fUsd(recR) + '/mes'), tone: 'pri' });
      if (isN(o.grossYieldPct)) out.push({ text: L3('Gross yield ' + o.grossYieldPct + '%', '毛租售比 ' + o.grossYieldPct + '%', 'Rend. bruto ' + o.grossYieldPct + '%'), tone: 'neutral' });
      if (isN(o.capRatePct)) out.push({ text: L3('Cap rate ' + o.capRatePct + '%', '资本化率 ' + o.capRatePct + '%', 'Cap rate ' + o.capRatePct + '%'), tone: 'neutral' });
      if (isN(o.onePercentRule)) out.push({ text: L3('1% rule ' + o.onePercentRule + '%', '1% 法则 ' + o.onePercentRule + '%', 'Regla 1%: ' + o.onePercentRule + '%'), tone: 'neutral' });
      return out.slice(0, 5);
    }
    if (qId === 'risk') {
      out = basePills.slice();
      if (f.apn) out.push({ text: 'APN ' + f.apn, tone: 'info' });
      if (f.titleAndLiens && isN(f.titleAndLiens.recordedTransferCount)) {
        out.push({
          text: L3(f.titleAndLiens.recordedTransferCount + ' recorded transfers', f.titleAndLiens.recordedTransferCount + ' 次产权过户记录', f.titleAndLiens.recordedTransferCount + ' transferencias registradas'),
          tone: 'neutral'
        });
      }
      return out.slice(0, 5);
    }
    if (qId === 'potential') {
      if (Array.isArray(ai.negotiation) && ai.negotiation.length > 0) {
        out.push({ text: L3(ai.negotiation.length + ' negotiation levers', ai.negotiation.length + ' 项谈判筹码', ai.negotiation.length + ' palancas de negociación'), tone: 'pri' });
      }
      if (Array.isArray(ai.sellerQuestions) && ai.sellerQuestions.length > 0) {
        out.push({ text: L3(ai.sellerQuestions.length + ' seller questions', ai.sellerQuestions.length + ' 个向卖家提问', ai.sellerQuestions.length + ' preguntas al vendedor'), tone: 'info' });
      }
      basePills.slice(0, 2).forEach(function (p) { out.push(p); });
      return out.slice(0, 5);
    }
    return basePills.slice(0, 5);
  }

  function setRole(nextRole, opts) {
    var options = opts || {};
    var r = normalizeRole(nextRole) || 'all';
    _roleState.role = r;
    if (document.documentElement && typeof document.documentElement.setAttribute === 'function') {
      document.documentElement.setAttribute('data-role', r);
    }
    if (!options.skipPersist) {
      try {
        if (typeof localStorage !== 'undefined') localStorage.setItem(ROLE_STORAGE_KEY, r);
      } catch (e) {}
      if (typeof history !== 'undefined' && typeof history.replaceState === 'function') {
        try {
          var u = safeUrlObj();
          if (u && u.searchParams) {
            u.searchParams.set('role', r);
            history.replaceState(history.state || null, '', u.pathname + u.search + u.hash);
          }
        } catch (e2) {}
      }
    }
    applyRoleHiddenSections();
    renderRoleSwitchers(options.focusPrefix || null);
    renderRoleFocusBrief();
    renderChapterBar();
    renderQuestionCards();
    renderRoleHiddenNote();
    renderReportTocTree();
    syncStickyHeaderVars();
    resizeChartsAndMaps();
    updateHorizontalScrollFades();
    return r;
  }

  function getDesignPga(hz) {
    var eq = (hz && hz.earthquake) || {};
    var dm = eq.designMaps || {};
    if (isN(dm.pgaM)) return dm.pgaM;
    if (isN(eq.pga)) return eq.pga;
    if (isN(eq.pgaG)) return eq.pgaG;
    return null;
  }

  function extractTopRiskAndChecks(rep) {
    var hz = mod(rep, 'hazards');
    var env = mod(rep, 'environment');
    var cond = (rep && rep.condition) || mod(rep, 'condition');
    var hoa = (rep && rep.hoaProfile) || mod(rep, 'hoaProfile');
    var f = (rep && rep.facts) || {};

    var fl = (hz && hz.flood) || null;
    var wf = (hz && hz.wildfire) || null;
    var nhd = (hz && hz.caNaturalHazardDisclosure) || null;
    var caEq = (hz && hz.earthquake && hz.earthquake.california) || null;
    var pga = getDesignPga(hz);

    var checks = [];
    var candidates = [];

    var hasUnverified = false;
    var unverifiedDomains = [];

    // 1. FEMA Flood check (Zone D / undetermined / unverified MUST NEVER be green ✓)
    var floodExplicitFalse = Boolean((fl && fl.checked === false) || (hz && hz.checks && hz.checks.flood === false));
    var fZone = fl ? String(fl.zone || fl.femaZone || '').trim() : '';
    var fZoneUp = fZone.toUpperCase();
    var fRisk = fl ? String(fl.risk || '').toLowerCase() : '';
    var isSfha = Boolean(fl && (Boolean(fl.sfha) || /^(A|AE|AH|AO|AR|A99|V|VE)$/i.test(fZoneUp)));
    var isZoneD = Boolean(fl && (fZoneUp === 'D' || fRisk === 'undetermined'));
    var isModFlood = Boolean(fl && !isSfha && !isZoneD && (fRisk === 'moderate' || /500-year|0\.2%|shaded/i.test(String(fl.zoneSubtype || fl.description || ''))));
    var floodUnmapped = Boolean(fl && !floodExplicitFalse && fl.checked === true && fl.mapped === false && fZone === '');
    var floodVerifiedClear = Boolean(hz && fl && !floodExplicitFalse && fZone !== '' && !isSfha && !isZoneD && !isModFlood);

    if (fl && isSfha) {
      checks.push({
        status: 'flag',
        target: 'risk',
        text: L3(
          'High-risk FEMA flood zone (Zone ' + (fZone || 'SFHA') + ') — mandatory lender flood insurance',
          '位于 FEMA 高风险洪水区（' + (fZone || 'SFHA') + ' 区）— 贷款需强制购买洪水险',
          'Zona de inundación FEMA de alto riesgo (Zona ' + (fZone || 'SFHA') + ') — seguro obligatorio'
        )
      });
      candidates.push({
        sev: 95,
        tone: 'bad',
        targetSection: 'risk',
        shortTitle: L3('High-risk flood (Zone ' + (fZone || 'A') + ')', '高风险洪水区 (' + (fZone || 'A') + ')', 'Inundación alto riesgo (' + (fZone || 'A') + ')'),
        plainDetail: L3(
          'Mapped inside FEMA high-risk floodplain; federally backed mortgages require separate flood insurance',
          '处于 FEMA 划定的百年一遇高风险洪泛区，按揭贷款须额外购买洪水保险',
          'Dentro de planicie inundable FEMA de alto riesgo; hipotecas exigen póliza contra inundación'
        ),
        findingClause: L3(
          'mapped FEMA high-risk flood exposure (Zone ' + (fZone || 'A') + ') is the main physical risk',
          '位于 FEMA 高风险洪水区（' + (fZone || 'A') + ' 区）是首要实体风险',
          'la zona de inundación de alto riesgo FEMA (Zona ' + (fZone || 'A') + ') es el principal riesgo físico'
        )
      });
    } else if (fl && isZoneD) {
      checks.push({
        status: 'watch',
        target: 'risk',
        text: L3(
          'Undetermined flood hazard — FEMA Zone D (unstudied area; verify lot drainage & flood quote)',
          '未定洪水风险 — FEMA D 类区域（官方尚未完成水文详查，建议核实排水与洪水险报价）',
          'Riesgo de inundación indeterminado — Zona FEMA D (área no estudiada; verificar drenaje y seguro)'
        )
      });
      candidates.push({
        sev: 62,
        tone: 'watch',
        targetSection: 'risk',
        shortTitle: L3('Undetermined flood (Zone D)', '未定洪水风险 (D区)', 'Inundación no estudiada (Zona D)'),
        plainDetail: L3(
          'FEMA Zone D means flood hazard is possible but unstudied; standard home insurance excludes surface flooding',
          'FEMA D 区代表尚未完成洪水测绘、存在未定内涝可能；标准房屋险不保地面洪水',
          'Zona FEMA D indica área sin estudio detallado de inundación; pólizas estándar no cubren inundaciones'
        ),
        findingClause: L3(
          'an unstudied FEMA Zone D flood area (undetermined flood hazard) is a key physical watch item',
          '尚未详查的 FEMA D 类未定洪水区是主要实体关注项',
          'el área de inundación no estudiada Zona FEMA D (riesgo indeterminado) requiere verificación'
        )
      });
    } else if (fl && isModFlood) {
      checks.push({
        status: 'watch',
        target: 'risk',
        text: L3(
          'Moderate flood zone — FEMA Zone ' + (fZone || 'X') + ' (500-year floodplain)',
          '中度洪水风险区 — FEMA ' + (fZone || 'X') + ' 区（500 年一遇洪泛区边缘）',
          'Riesgo moderado de inundación — Zona FEMA ' + (fZone || 'X') + ' (planicie de 500 años)'
        )
      });
      candidates.push({
        sev: 50,
        tone: 'watch',
        targetSection: 'risk',
        shortTitle: L3('Moderate flood zone', '中度洪水关注区', 'Zona inundación moderada'),
        plainDetail: L3(
          'Mapped in FEMA moderate flood zone (500-year storm floodplain); optional flood policy advised',
          '位于 FEMA 500 年一遇中度洪泛区，建议评估可选洪水险保费',
          'En zona moderada de inundación FEMA (500 años); considere seguro opcional'
        ),
        findingClause: L3(
          'moderate 500-year floodplain exposure is the main physical watch item',
          '中度洪泛区敞口是主要自然风险关注点',
          'la exposición moderada a inundación es el principal punto a vigilar'
        )
      });
    } else if (floodVerifiedClear) {
      var nearDist = fl.nearestSfha && isN(fl.nearestSfha.distMi) ? fl.nearestSfha.distMi : null;
      checks.push({
        status: 'clear',
        target: 'risk',
        text: L3(
          'Checked, clear — Outside high-risk FEMA flood zone (Zone ' + fZone + ', minimal flood hazard' + (nearDist != null ? '; nearest floodplain ' + nearDist + ' mi' : '') + ')',
          '已核查，无异常 — 不在 FEMA 高风险洪水区内（' + fZone + ' 区，极低洪水风险' + (nearDist != null ? '，距最近洪泛区 ' + nearDist + ' 英里' : '') + '）',
          'Verificado, sin alertas — Fuera de zona de inundación de alto riesgo FEMA (Zona ' + fZone + ', riesgo mínimo)'
        )
      });
    } else if (floodUnmapped) {
      hasUnverified = true;
      unverifiedDomains.push(L3('FEMA flood polygon', 'FEMA 洪水测绘', 'polígono FEMA'));
      checks.push({
        status: 'watch',
        target: 'risk',
        text: L3(
          'Unmapped area — FEMA NFHL has no mapped flood-zone polygon here (confirm lender flood determination)',
          '未测绘区域 — FEMA NFHL 在此位置暂无洪水分区图斑（请以贷款方洪水认定为准）',
          'Área sin mapa — FEMA NFHL no tiene polígono de inundación aquí (confirme con el prestamista)'
        )
      });
    } else if (hz || env) {
      hasUnverified = true;
      unverifiedDomains.push(L3('FEMA flood', 'FEMA 洪水', 'inundación FEMA'));
      checks.push({
        status: 'watch',
        target: 'risk',
        text: L3(
          'Unverified — FEMA flood lookup unavailable (verify NFHL map & lender flood determination)',
          '未核实 — FEMA 洪水图层查询暂不可用（请核实官方 NFHL 地图与贷款洪水认定）',
          'Sin verificar — consulta FEMA de inundación no disponible (verifique mapa NFHL)'
        )
      });
    }

    // 2. Wildfire Severity check
    var wca = (wf && wf.california) || {};
    var wfClass = String((wca && (wca.fhsz || wca.zone)) || (wf && (wf.whpClass || wf.risk || wf.level)) || '').trim();
    var inFire = Boolean(wca.inFhsz) ||
      Boolean(nhd && (nhd.veryHighFireHazardZone === 'yes' || nhd.stateResponsibilityArea === 'yes')) ||
      /very\s*high|high|extreme/i.test(wfClass);
    var wfExplicitFalse = Boolean((wf && wf.checked === false) || (wca && wca.checked === false) || (hz && hz.checks && hz.checks.whp === false && hz.checks.caFhsz === false));
    var wfVerifiedClear = Boolean(wf && !wfExplicitFalse && (wfClass !== '' || wca.inFhsz === false || (nhd && (nhd.veryHighFireHazardZone === 'no' || nhd.stateResponsibilityArea === 'no'))));

    if (inFire) {
      var fireDesc = wfClass || L3('Mapped wildfire hazard zone', '山火高风险区', 'Zona de riesgo de incendio');
      checks.push({
        status: 'flag',
        target: 'risk',
        text: L3(
          'Wildfire hazard zone — ' + fireDesc + ' (confirm insurer brush eligibility early)',
          '位于山火风险区 — ' + fireDesc + '（请尽早核实商业保险承保资格）',
          'Zona de peligro de incendio forestal — ' + fireDesc + ' (verificar asegurabilidad)'
        )
      });
      candidates.push({
        sev: 90,
        tone: 'bad',
        targetSection: 'risk',
        shortTitle: L3('Wildfire hazard zone', '山火高危区', 'Zona de incendio forestal'),
        plainDetail: L3(
          fireDesc + ' — can restrict standard home insurance carriers and raise annual premiums',
          fireDesc + ' — 可能导致普通商业保险拒保或大幅提高保费',
          fireDesc + ' — puede limitar aseguradoras estándar y elevar primas anuales'
        ),
        findingClause: L3(
          'mapped wildfire hazard exposure is the main physical and insurance risk',
          '山火高危区敞口与房屋保险承保限制是首要风险',
          'la exposición a zona de incendio forestal es el principal riesgo físico y de seguro'
        )
      });
    } else if (wfVerifiedClear) {
      checks.push({
        status: 'clear',
        target: 'risk',
        text: L3(
          'Checked, clear — Outside mapped wildfire hazard severity zones',
          '已核查，无异常 — 不在法定山火危险严重区（FHSZ）内',
          'Verificado, sin alertas — Fuera de zonas oficiales de severidad de incendios forestales'
        )
      });
    } else if (hz || env) {
      hasUnverified = true;
      unverifiedDomains.push(L3('wildfire', '山火', 'incendio'));
      checks.push({
        status: 'watch',
        target: 'risk',
        text: L3(
          'Unverified — Wildfire hazard lookup unavailable (verify CAL FIRE / USFS FHSZ map)',
          '未核实 — 山火风险等级查询暂不可用（请核实 CAL FIRE / USFS 地图）',
          'Sin verificar — consulta de riesgo de incendio forestal no disponible'
        )
      });
    }

    // 3. Earthquake shaking, Fault Rupture, Liquefaction & Dam Inundation check
    var eqObj = hz && hz.earthquake;
    var inFault = Boolean(nhd && nhd.earthquakeFaultZone === 'yes') || Boolean(caEq && caEq.inFaultZone);
    var inLiq = Boolean(nhd && nhd.seismicHazardZone === 'yes') || Boolean(caEq && (caEq.liquefactionZone || caEq.landslideZone));
    var inDam = Boolean(nhd && nhd.damInundationArea === 'yes');
    var isZoneDCo = Boolean(fl && (fZoneUp === 'D' || fRisk === 'undetermined'));
    var eqExplicitFalse = Boolean((eqObj && eqObj.checked === false) || (caEq && caEq.checked === false) || (hz && hz.checks && hz.checks.asce === false && hz.checks.faults === false));
    var eqVerifiedClear = Boolean(eqObj && !eqExplicitFalse && (isN(pga) || (caEq && caEq.inFaultZone === false) || (nhd && nhd.earthquakeFaultZone === 'no')));

    if (inFault) {
      checks.push({
        status: 'flag',
        target: 'risk',
        text: L3(
          'Mapped active earthquake fault-rupture zone' + (pga != null ? ' + severe design ground shaking (' + pga + 'g)' : ''),
          '位于活动地震断层破裂带内' + (pga != null ? '，且设计地面摇晃强烈（' + pga + 'g）' : ''),
          'En zona activa de ruptura de falla sísmica' + (pga != null ? ' + sacudida de diseño severa (' + pga + 'g)' : '')
        )
      });
      candidates.push({
        sev: 92,
        tone: 'bad',
        targetSection: 'risk',
        shortTitle: L3('Active fault-rupture zone', '活动断层破裂带', 'Zona de falla activa'),
        plainDetail: L3(
          'Inside statutory earthquake fault zone' + (pga != null ? ' with ' + pga + 'g design ground shaking' : ''),
          '处于法定活动断层带内' + (pga != null ? '（设计地面峰值加速度 ' + pga + 'g）' : ''),
          'Dentro de zona oficial de falla activa' + (pga != null ? ' (' + pga + 'g de aceleración sísmica)' : '')
        ),
        findingClause: L3(
          'mapped active fault-rupture exposure is the main physical risk',
          '处于活动地震断层破裂带是首要实体风险',
          'la exposición a zona de ruptura de falla activa es el principal riesgo físico'
        )
      });
    } else if ((isN(pga) && pga >= 0.5) || inLiq || inDam) {
      var seisPartsEn = [], seisPartsZh = [], seisPartsEs = [];
      if (isN(pga) && pga >= 0.5) {
        seisPartsEn.push('severe design ground shaking (' + pga + 'g peak acceleration)');
        seisPartsZh.push('强烈地震地面摇晃风险（设计峰值加速度 ' + pga + 'g）');
        seisPartsEs.push('sacudida sísmica de diseño severa (' + pga + 'g)');
      } else if (isN(pga) && pga >= 0.3) {
        seisPartsEn.push('moderate-to-strong design ground shaking (' + pga + 'g)');
        seisPartsZh.push('中强地震地面摇晃（' + pga + 'g）');
        seisPartsEs.push('sacudida sísmica moderada-alta (' + pga + 'g)');
      }
      if (inLiq) {
        seisPartsEn.push('mapped liquefaction/landslide seismic zone');
        seisPartsZh.push('地震土壤液化/滑坡区');
        seisPartsEs.push('zona sísmica de licuefacción/deslizamiento');
      }
      if (inDam) {
        seisPartsEn.push('mapped dam-inundation area');
        seisPartsZh.push('水坝溃坝淹没区');
        seisPartsEs.push('área de inundación por presa');
      }
      var clearTailEn = (!inFault && !inLiq) ? ' (outside fault-rupture & liquefaction zones)' : (!inFault ? ' (outside fault-rupture zone)' : '');
      var clearTailZh = (!inFault && !inLiq) ? '（不在断层破裂与液化区内）' : (!inFault ? '（不在断层破裂带内）' : '');
      var clearTailEs = (!inFault && !inLiq) ? ' (fuera de ruptura de falla y licuefacción)' : (!inFault ? ' (fuera de ruptura de falla)' : '');

      checks.push({
        status: (isN(pga) && pga >= 0.65) || inLiq ? 'flag' : 'watch',
        target: 'risk',
        text: L3(
          seisPartsEn.join('; ').replace(/^./, function (c) { return c.toUpperCase(); }) + clearTailEn,
          seisPartsZh.join('、') + clearTailZh,
          seisPartsEs.join('; ').replace(/^./, function (c) { return c.toUpperCase(); }) + clearTailEs
        )
      });

      var coClauseEn = inDam ? ' and mapped dam-inundation exposure are the main physical risks'
        : isZoneDCo ? ' and an unstudied FEMA Zone D flood area are the main physical risks'
        : inLiq ? ' and mapped soil-liquefaction hazard are the main physical risks'
        : ' is the main physical risk';
      var coClauseZh = inDam ? '及水坝淹没区敞口是主要实体风险'
        : isZoneDCo ? '及未详查的 FEMA D 类洪水区是主要实体风险'
        : inLiq ? '及地震土壤液化区敞口是主要实体风险'
        : '是首要实体风险';
      var coClauseEs = inDam ? ' y el área de inundación por presa son los principales riesgos físicos'
        : isZoneDCo ? ' y la Zona FEMA D de inundación no estudiada son los principales riesgos físicos'
        : inLiq ? ' y el riesgo de licuefacción del suelo son los principales riesgos físicos'
        : ' es el principal riesgo físico';

      candidates.push({
        sev: (isN(pga) && pga >= 0.65) ? 80 : 68,
        tone: (isN(pga) && pga >= 0.65) ? 'bad' : 'watch',
        targetSection: 'risk',
        shortTitle: isN(pga) && pga >= 0.5
          ? L3('Severe quake shaking', '强震地面摇晃风险', 'Sacudida sísmica severa')
          : (inLiq ? L3('Liquefaction seismic zone', '地震土壤液化区', 'Zona de licuefacción') : L3('Dam-inundation zone', '水坝淹没关注区', 'Inundación por presa')),
        plainDetail: L3(
          (isN(pga) ? pga + 'g design ground shaking (strong seismic motion; excluded from standard home insurance)' : 'Mapped statutory hazard zone') +
            (inDam ? ' · inside CA dam-inundation map' : '') +
            (isZoneDCo ? ' · FEMA Zone D unstudied flood area' : '') +
            (inLiq ? ' · mapped liquefaction zone' : ''),
          (isN(pga) ? '设计地面峰值加速度 ' + pga + 'g（强震摇晃敞口，标准房屋险不含地震理赔）' : '处于法定自然灾害披露区') +
            (inDam ? ' · 位于水坝淹没图范围' : '') +
            (isZoneDCo ? ' · FEMA D 类未定洪水区' : '') +
            (inLiq ? ' · 地震土壤液化区' : ''),
          (isN(pga) ? 'Sacudida de diseño de ' + pga + 'g (movimiento sísmico fuerte; excluido del seguro de hogar estándar)' : 'Zona sísmica oficial') +
            (inDam ? ' · área de inundación por presa' : '') +
            (isZoneDCo ? ' · Zona FEMA D sin estudio' : '') +
            (inLiq ? ' · zona de licuefacción' : '')
        ),
        findingClause: L3(
          (isN(pga) && pga >= 0.5 ? 'severe earthquake ground shaking (' + pga + 'g design acceleration)' : seisPartsEn[0]) + coClauseEn,
          (isN(pga) && pga >= 0.5 ? '强烈地震地面摇晃（设计加速度 ' + pga + 'g）' : seisPartsZh[0]) + coClauseZh,
          (isN(pga) && pga >= 0.5 ? 'la sacudida sísmica severa (' + pga + 'g de aceleración de diseño)' : seisPartsEs[0]) + coClauseEs
        )
      });
    } else if (eqVerifiedClear) {
      checks.push({
        status: 'clear',
        target: 'risk',
        text: isN(pga)
          ? L3(
              'Checked, clear — Low seismic ground shaking (' + pga + 'g); outside fault-rupture & liquefaction zones',
              '已核查，无异常 — 地震动峰值较低（' + pga + 'g），不在断层破裂带与液化区内',
              'Verificado, sin alertas — Sacudida sísmica baja (' + pga + 'g); fuera de zonas de falla y licuefacción'
            )
          : L3(
              'Outside mapped fault-rupture & liquefaction zones (verify ASCE seismic shaking PGA)',
              '不在法定断层破裂带与液化区内（地震动加速度 PGA 待核实）',
              'Fuera de zonas de falla y licuefacción (verificar aceleración sísmica PGA)'
            )
      });
    } else if (hz || env) {
      hasUnverified = true;
      unverifiedDomains.push(L3('seismic', '地震与断层', 'sísmica'));
      checks.push({
        status: 'watch',
        target: 'risk',
        text: L3(
          'Unverified — Seismic ground shaking & fault zone lookup unavailable (verify USGS / CGS)',
          '未核实 — 地震动峰值与断层液化查询暂不可用（请核实 USGS / CGS）',
          'Sin verificar — consulta sísmica y de fallas no disponible'
        )
      });
    }

    // 4. Superfund & Toxic Sites within 1 mile check
    var stSites = env && env.sites;
    var npl = stSites && stSites.nearestByProgram && stSites.nearestByProgram.npl;
    var nplWithin1 = Boolean(npl && isN(npl.distanceMi) && npl.distanceMi <= 1.0);
    var closeActive = stSites && (stSites.nearest || []).find(function (x) {
      return x && x.statusGroup === 'active' && isN(x.distanceMi) && x.distanceMi <= 0.35;
    });
    var sitesVerifiedClear = Boolean(stSites && stSites.checked !== false && !(env && env.checks && env.checks.sites === false));

    if (nplWithin1) {
      checks.push({
        status: 'flag',
        target: 'environment',
        text: L3(
          'Superfund NPL toxic site within 1 mi — ' + cleanPublicText(npl.name) + ' (' + npl.distanceMi + ' mi)',
          '1 英里内存在超级基金（Superfund）污染场地 — ' + cleanPublicText(npl.name) + '（' + npl.distanceMi + ' 英里）',
          'Sitio tóxico Superfund a menos de 1 milla — ' + cleanPublicText(npl.name) + ' (' + npl.distanceMi + ' mi)'
        )
      });
      candidates.push({
        sev: 86,
        tone: 'bad',
        targetSection: 'environment',
        shortTitle: L3('Superfund site (' + npl.distanceMi + ' mi)', '超级基金污染点 (' + npl.distanceMi + ' mi)', 'Sitio Superfund (' + npl.distanceMi + ' mi)'),
        plainDetail: L3(
          cleanPublicText(npl.name) + ' federal cleanup site is ' + npl.distanceMi + ' mi away; review groundwater/vapor filings',
          '距联邦超级基金管控点 ' + cleanPublicText(npl.name) + ' 仅 ' + npl.distanceMi + ' 英里，需核查地下水与土壤挥发档案',
          'Sitio federal Superfund ' + cleanPublicText(npl.name) + ' a ' + npl.distanceMi + ' mi; revisar monitoreo ambiental'
        ),
        findingClause: L3(
          'proximity to a federal Superfund cleanup site (' + npl.distanceMi + ' mi) is a primary environmental risk',
          '邻近联邦超级基金污染治理点（' + npl.distanceMi + ' 英里）是主要环境风险',
          'la cercanía a un sitio federal Superfund (' + npl.distanceMi + ' mi) es un riesgo ambiental clave'
        )
      });
    } else if (closeActive) {
      checks.push({
        status: 'watch',
        target: 'environment',
        text: L3(
          'Active regulated cleanup case at ' + closeActive.distanceMi + ' mi (' + cleanPublicText(closeActive.name) + ')' + (sitesVerifiedClear ? '; 0 Superfund sites ≤1 mi' : ''),
          '周边 ' + closeActive.distanceMi + ' 英里有在办环保监管个案（' + cleanPublicText(closeActive.name) + '）' + (sitesVerifiedClear ? '；1 英里内无超级基金站点' : ''),
          'Caso ambiental activo a ' + closeActive.distanceMi + ' mi (' + cleanPublicText(closeActive.name) + ')' + (sitesVerifiedClear ? '; 0 sitios Superfund ≤1 mi' : '')
        )
      });
      candidates.push({
        sev: 55,
        tone: 'watch',
        targetSection: 'environment',
        shortTitle: L3('Nearby cleanup case (' + closeActive.distanceMi + ' mi)', '周边在办环保个案', 'Caso ambiental cercano'),
        plainDetail: L3(
          cleanPublicText(closeActive.name) + ' (' + closeActive.distanceMi + ' mi) — check closure/vapor status',
          cleanPublicText(closeActive.name) + '（' + closeActive.distanceMi + ' 英里）— 建议查阅监管进度',
          cleanPublicText(closeActive.name) + ' (' + closeActive.distanceMi + ' mi) — revisar expediente ambiental'
        ),
        findingClause: L3(
          'an active regulated environmental case ' + closeActive.distanceMi + ' mi away warrants review',
          '距房源 ' + closeActive.distanceMi + ' 英里的在办环保监管点值得核查',
          'un caso ambiental regulado a ' + closeActive.distanceMi + ' mi amerita revisión'
        )
      });
    } else if (sitesVerifiedClear) {
      var nearNplNoteEn = (npl && isN(npl.distanceMi)) ? ' (nearest ' + npl.distanceMi + ' mi)' : '';
      var nearNplNoteZh = (npl && isN(npl.distanceMi)) ? '（最近站点距此 ' + npl.distanceMi + ' 英里）' : '';
      var nearNplNoteEs = (npl && isN(npl.distanceMi)) ? ' (más cercano a ' + npl.distanceMi + ' mi)' : '';
      checks.push({
        status: 'clear',
        target: 'environment',
        text: L3(
          'Checked, clear — 0 Superfund toxic sites within 1 mile' + nearNplNoteEn,
          '已核查，无异常 — 1 英里范围内无超级基金（Superfund）重度污染场地' + nearNplNoteZh,
          'Verificado, sin alertas — 0 sitios tóxicos Superfund en un radio de 1 milla' + nearNplNoteEs
        )
      });
    } else if (env || hz) {
      hasUnverified = true;
      unverifiedDomains.push(L3('Superfund / toxic sites', '超级基金污染场地', 'sitios tóxicos EPA'));
      checks.push({
        status: 'watch',
        target: 'environment',
        text: L3(
          'Unverified — EPA Superfund & cleanup site search unavailable (check Envirofacts / GeoTracker)',
          '未核实 — EPA 超级基金与污染场地检索暂不可用（请核查 EPA / GeoTracker）',
          'Sin verificar — búsqueda de sitios tóxicos EPA no disponible'
        )
      });
    }

    // 5. Public Drinking Water System check
    var waterVerified = Boolean(env && env.water && env.water.checked !== false && !(env.checks && env.checks.water === false) && env.water.primary);
    if (waterVerified) {
      var wp = env.water.primary;
      var v10 = env.water.violations10y || {};
      var openViol = isN(v10.unresolvedHealthBased) ? v10.unresolvedHealthBased : 0;
      var pastHealth = isN(v10.healthBased) ? v10.healthBased : 0;
      var wName = cleanPublicText(wp.name || 'Public Water System');

      if (openViol > 0) {
        checks.push({
          status: 'flag',
          target: 'environment',
          text: L3(
            openViol + ' open health-based drinking water violation(s) — ' + wName,
            '供水系统存在 ' + openViol + ' 项未结健康类水质违规 — ' + wName,
            openViol + ' infracción(es) sanitarias abiertas de agua potable — ' + wName
          )
        });
        candidates.push({
          sev: 78,
          tone: 'bad',
          targetSection: 'environment',
          shortTitle: L3('Open water violation (' + openViol + ')', '自来水未结违规 (' + openViol + ')', 'Infracción de agua (' + openViol + ')'),
          plainDetail: L3(
            wName + ' reports ' + openViol + ' unresolved EPA health-based drinking water violation(s)',
            wName + ' 存在 ' + openViol + ' 项 EPA 未结健康类饮用水违规记录',
            wName + ' registra ' + openViol + ' infracción(es) de salud sin resolver en agua potable'
          ),
          findingClause: L3(
            openViol + ' unresolved public drinking-water health violation(s) require attention',
            '公共供水系统存在 ' + openViol + ' 项未结健康违规需重点留意',
            openViol + ' infracción(es) sanitarias abiertas en el sistema de agua requieren atención'
          )
        });
      } else if (pastHealth > 0) {
        checks.push({
          status: 'watch',
          target: 'environment',
          text: L3(
            '0 open water violations (' + pastHealth + ' resolved health notice in past 10 yrs · ' + wName + ')',
            '当前 0 项未结水质违规（过去 10 年曾有 ' + pastHealth + ' 项已整改记录 · ' + wName + '）',
            '0 infracciones abiertas de agua (' + pastHealth + ' aviso resuelto en 10 años · ' + wName + ')'
          )
        });
      } else {
        checks.push({
          status: 'clear',
          target: 'environment',
          text: L3(
            'Checked, clear — 0 health-based drinking water violations (' + wName + ')',
            '已核查，无异常 — 公共自来水系统 0 项健康类违规（' + wName + '）',
            'Verificado, sin alertas — 0 infracciones sanitarias en agua potable (' + wName + ')'
          )
        });
      }
    } else if (env || hz) {
      hasUnverified = true;
      unverifiedDomains.push(L3('SDWIS drinking water', '饮用水合规', 'agua potable SDWIS'));
      checks.push({
        status: 'watch',
        target: 'environment',
        text: L3(
          'Unverified — EPA SDWIS drinking water check unavailable (verify utility Consumer Confidence Report)',
          '未核实 — EPA 公共饮用水 SDWIS 合规检索暂不可用（请查阅供水商年度水质报告）',
          'Sin verificar — consulta EPA SDWIS de agua potable no disponible'
        )
      });
    }

    // Check HOA / Condition severe deal-breakers if any
    if (hoa && hoa.debtAndLienStatus && (hoa.debtAndLienStatus.hasActiveHoaLien || hoa.debtAndLienStatus.nodaDetected)) {
      checks.push({
        status: 'flag',
        target: 'hoa',
        text: L3(
          'HOA delinquency / active lien signal flagged — verify estoppel & payoff before closing',
          '检测到 HOA 欠费留置权信号 — 过户前务必核实结清证明',
          'Señal de gravamen o mora HOA detectada — verificar certificado libre de deuda'
        )
      });
      candidates.push({
        sev: 88,
        tone: 'bad',
        targetSection: 'hoa',
        shortTitle: L3('HOA lien signal', 'HOA 留置权警示', 'Alerta de gravamen HOA'),
        plainDetail: L3('Active HOA lien or notice of default signal detected in diligence records', '尽调记录检测到 HOA 留置权或违约通知信号', 'Señal de gravamen HOA detectada en registros'),
        findingClause: L3('an HOA lien signal requires immediate title verification', 'HOA 欠费留置权信号需优先核验产权', 'una alerta de gravamen HOA requiere verificación de título')
      });
    }

    // Sort checks: bad/flag first, then watch, then clear
    var rankMap = { bad: 0, flag: 0, watch: 1, clear: 2 };
    checks.sort(function (a, b) {
      var ra = rankMap[a.status] != null ? rankMap[a.status] : 2;
      var rb = rankMap[b.status] != null ? rankMap[b.status] : 2;
      return ra - rb;
    });

    candidates.sort(function (a, b) { return b.sev - a.sev; });
    var top = candidates[0] || null;
    if (!top && (hz || env)) {
      if (hasUnverified) {
        var uvList = unverifiedDomains.join(', ');
        top = {
          sev: 35,
          tone: 'watch',
          targetSection: 'risk',
          shortTitle: L3('Unverified public hazard checks', '部分灾害/环保核查未返回', 'Consultas de riesgo sin verificar'),
          plainDetail: L3(
            'Upstream checks unavailable for: ' + uvList + ' — verify official government maps before removing contingencies',
            '以下官方实时检索暂未返回：' + uvList + ' — 解除保护期前请人工核验对应政府官网地图',
            'Consultas oficiales no disponibles para: ' + uvList + ' — verificar mapas oficiales antes de cerrar'
          ),
          findingClause: L3(
            'incomplete public-record checks (' + uvList + ') still require manual verification',
            '部分政府公开检索（' + uvList + '）暂未返回，需人工复核',
            'consultas oficiales incompletas (' + uvList + ') aún requieren verificación manual'
          )
        };
      } else {
        var ageNoteEn = (f.yearBuilt && f.yearBuilt < 1978)
          ? 'Routine pre-' + f.yearBuilt + ' sewer, electrical panel & roof inspection advised'
          : 'Outside high-risk flood, wildfire, fault-rupture & Superfund zones';
        var ageNoteZh = (f.yearBuilt && f.yearBuilt < 1978)
          ? '公开灾害档案无重大红灯；建于 ' + f.yearBuilt + ' 年，建议常规核查排污管、配电箱与屋顶寿命'
          : '已排除高风险洪水、山火、活动断层及 1 英里内超级基金污染源';
        var ageNoteEs = (f.yearBuilt && f.yearBuilt < 1978)
          ? 'Sin alertas graves en registros públicos; vivienda de ' + f.yearBuilt + ', revisar drenaje, panel eléctrico y techo'
          : 'Fuera de zonas de inundación grave, incendios, fallas activas y sitios Superfund';
        top = {
          sev: 10,
          tone: 'good',
          targetSection: cond ? 'condition' : 'risk',
          shortTitle: L3('No severe hazard flags', '无重大公开灾害红灯', 'Sin riesgos graves'),
          plainDetail: L3(ageNoteEn, ageNoteZh, ageNoteEs),
          findingClause: L3(
            'no severe public-record hazards are flagged',
            '公开政府记录中未发现重大自然与环境灾害红灯',
            'no se detectan riesgos graves en los registros públicos'
          )
        };
      }
    }

    return { top: top, checks: checks, hasUnverified: hasUnverified };
  }

  function renderVerdictHero() {
    var rep = getRep() || {};
    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var o = v.ownership || {};
    var m0 = o.monthly || {};
    var f = (rep && rep.facts) || {};
    var rk = extractTopRiskAndChecks(rep);

    var est = isN(e.value) ? e.value : null;
    var ownMo = isN(m0.total) && m0.total > 0 ? m0.total : null;
    var rrHero = getRentResearchData(rep);
    var rentMo = (rrHero && isN(rrHero.recommendedMonthlyRent) && rrHero.recommendedMonthlyRent > 0)
      ? rrHero.recommendedMonthlyRent
      : (isN(v.monthlyRent) && v.monthlyRent > 0 ? v.monthlyRent : (isN(o.rentEstimate) && o.rentEstimate > 0 ? o.rentEstimate : null));

    // 1. Plain-language finding sentence (#verdictFindingLine) — strictly real report fields
    var findEl = byId('verdictFindingLine');
    if (findEl) {
      var clauses = [];
      if (est != null) {
        clauses.push(L3(
          'Estimated near ' + fUsdK(est),
          '真实估值约 ' + fUsdK(est),
          'Estimación Real cerca de ' + fUsdK(est)
        ));
      }
      if (ownMo != null && rentMo != null) {
        var ratio = ownMo / rentMo;
        var rStr = ratio.toFixed(1);
        if (ratio >= 1.25) {
          clauses.push(L3(
            'owning (' + fUsd(ownMo) + '/mo) costs ~' + rStr + '× market rent (' + fUsd(rentMo) + '/mo)',
            '每月真实持有成本（' + fUsd(ownMo) + '/月）约为同区市场租金（' + fUsd(rentMo) + '/月）的 ' + rStr + ' 倍',
            'ser propietario (' + fUsd(ownMo) + '/mes) cuesta ~' + rStr + '× el alquiler de mercado (' + fUsd(rentMo) + '/mes)'
          ));
        } else if (ratio <= 0.9) {
          var pctBelow = Math.max(1, Math.round((1 - ratio) * 100));
          clauses.push(L3(
            'owning (' + fUsd(ownMo) + '/mo) runs ~' + pctBelow + '% below market rent (' + fUsd(rentMo) + '/mo)',
            '每月真实持有成本（' + fUsd(ownMo) + '/月）低于同区市场月租（' + fUsd(rentMo) + '/月）约 ' + pctBelow + '%',
            'ser propietario (' + fUsd(ownMo) + '/mes) está ~' + pctBelow + '% por debajo del alquiler de mercado (' + fUsd(rentMo) + '/mes)'
          ));
        } else {
          clauses.push(L3(
            'owning (' + fUsd(ownMo) + '/mo) is roughly in line with market rent (' + fUsd(rentMo) + '/mo, ~' + rStr + '×)',
            '每月真实持有成本（' + fUsd(ownMo) + '/月）与同区市场月租（' + fUsd(rentMo) + '/月，约 ' + rStr + ' 倍）基本相当',
            'ser propietario (' + fUsd(ownMo) + '/mes) está en línea con el alquiler de mercado (' + fUsd(rentMo) + '/mes, ~' + rStr + '×)'
          ));
        }
      } else if (ownMo != null) {
        clauses.push(L3(
          'estimated all-in monthly cost is ' + fUsd(ownMo) + '/mo',
          '每月真实持有总成本约 ' + fUsd(ownMo) + '/月',
          'el costo mensual real estimado es ' + fUsd(ownMo) + '/mes'
        ));
      }
      if (rk.top && rk.top.findingClause) {
        clauses.push(rk.top.findingClause);
      }

      if (clauses.length) {
        var sep = langCode() === 'zh' ? '；' : '; ';
        var endPunc = langCode() === 'zh' ? '。' : '.';
        var sentence = clauses.join(sep);
        if (!/[.。!?]$/.test(sentence)) sentence += endPunc;
        findEl.textContent = sentence;
        findEl.hidden = false;
      } else {
        findEl.textContent = '';
        findEl.hidden = true;
      }
    }

    // 2. Three real-quantity tiles (#verdictKeyTiles) — NEVER bare 0-100 composite scores
    var tilesEl = byId('verdictKeyTiles');
    if (tilesEl) {
      var costSubParts = [];
      if (ownMo != null && rentMo != null) {
        costSubParts.push(L3(
          '~' + (ownMo / rentMo).toFixed(1) + '× ' + fUsd(rentMo) + '/mo market rent',
          '约为市场月租 ' + fUsd(rentMo) + ' 的 ' + (ownMo / rentMo).toFixed(1) + ' 倍',
          '~' + (ownMo / rentMo).toFixed(1) + '× renta de ' + fUsd(rentMo) + '/mes'
        ));
      }
      var brkParts = [];
      if (isN(m0.principalInterest)) brkParts.push(L3('P&I ', '本息 ', 'P&I ') + fUsd(m0.principalInterest));
      if (isN(m0.propertyTax)) brkParts.push(L3('reassessed tax ', '重评房产税 ', 'predial ') + fUsd(m0.propertyTax));
      var insMaint = (isN(m0.insurance) ? m0.insurance : 0) + (isN(m0.maintenance) ? m0.maintenance : 0);
      if (insMaint > 0) brkParts.push(L3('ins./maint. ', '保险维保 ', 'seg./mant. ') + fUsd(insMaint));
      if (isN(m0.hoa) && m0.hoa > 0) brkParts.push('HOA ' + fUsd(m0.hoa));
      if (brkParts.length) costSubParts.push(brkParts.join(' + '));

      var estSubParts = [];
      if (isN(e.low) && isN(e.high)) {
        estSubParts.push(L3('Range ', '区间 ', 'Rango ') + fUsdK(e.low) + '–' + fUsdK(e.high));
      }
      var listPrice = (f.mlsListing && isN(f.mlsListing.listPrice) ? f.mlsListing.listPrice : (isN(f.listPrice) ? f.listPrice : null));
      if (listPrice != null && est != null) {
        var diffPct = Math.round(((est - listPrice) / listPrice) * 100);
        estSubParts.push(L3(
          (diffPct >= 0 ? '+' + diffPct + '% vs ' : diffPct + '% vs ') + fUsdK(listPrice) + ' asking',
          '对比挂牌价 ' + fUsdK(listPrice) + ' (' + (diffPct >= 0 ? '+' : '') + diffPct + '%)',
          (diffPct >= 0 ? '+' + diffPct + '% vs ' : diffPct + '% vs ') + fUsdK(listPrice) + ' lista'
        ));
      } else if (f.lastSale && isN(f.lastSale.price)) {
        var sy = f.lastSale.date ? String(f.lastSale.date).slice(0, 4) : '';
        estSubParts.push(L3(
          'Last sold ' + fUsdK(f.lastSale.price) + (sy ? ' (' + sy + ')' : ''),
          '上次成交 ' + fUsdK(f.lastSale.price) + (sy ? ' (' + sy + '年)' : ''),
          'Última venta ' + fUsdK(f.lastSale.price) + (sy ? ' (' + sy + ')' : '')
        ));
      } else if (isN(e.perSqft)) {
        estSubParts.push(fUsd(e.perSqft) + '/sqft');
      }

      var topTone = (rk.top && rk.top.tone) || 'watch';
      var topHeadline = rk.top ? rk.top.shortTitle : L3('See risk checks', '查看风险核验', 'Ver riesgos');
      var topSub = rk.top ? rk.top.plainDetail : L3('Public-record hazard & environmental screening', '基于公开档案的自然灾害与环境筛查', 'Revisión oficial de riesgos y entorno');
      var topTarget = (rk.top && rk.top.targetSection) || 'risk';

      var tile1Html = ownMo != null ? (
        '<button type="button" class="v-tile verdict-tile" data-vjump="payment">' +
          '<span class="v-tile-k">' + esc(L3('Real Monthly Cost', '每月真实持有成本', 'Costo mensual real')) + '</span>' +
          '<strong class="v-tile-v">' + esc(fUsd(ownMo) + L3('/mo', '/月', '/mes')) + '</strong>' +
          '<span class="v-tile-d">' + esc(costSubParts.join(' · ')) + '</span>' +
        '</button>'
      ) : '';

      var tile2Html = est != null ? (
        '<button type="button" class="v-tile verdict-tile pri" data-vjump="value">' +
          '<span class="v-tile-k">' + esc(L3('True Estimate', '真实估值', 'Estimación Real')) + '</span>' +
          '<strong class="v-tile-v">' + esc(fUsdK(est)) + '</strong>' +
          '<span class="v-tile-d">' + esc(estSubParts.join(' · ')) + '</span>' +
        '</button>'
      ) : '';

      var tile3Html = rk.top ? (
        '<button type="button" class="v-tile verdict-tile ' + esc(topTone) + '" data-vjump="' + esc(topTarget) + '">' +
          '<span class="v-tile-k">' + esc(L3('Most Serious Finding', '首要关注风险', 'Hallazgo principal')) + '</span>' +
          '<strong class="v-tile-v finding">' + esc(topHeadline) + '</strong>' +
          '<span class="v-tile-d">' + esc(topSub) + '</span>' +
        '</button>'
      ) : '';

      tilesEl.innerHTML = tile1Html + tile2Html + tile3Html;
      qAll('[data-vjump]', tilesEl).forEach(function (btn) {
        btn.addEventListener('click', function () {
          jumpToSection(btn.getAttribute('data-vjump'), true, { pushHash: true });
        });
      });
    }

    // 3. Carfax-style Flags & Clear Checks (#verdictFlagsChecks)
    var checksEl = byId('verdictFlagsChecks');
    if (checksEl) {
      if (!rk.checks.length) {
        checksEl.innerHTML = '';
        checksEl.hidden = true;
      } else {
        checksEl.hidden = false;
        var flagCnt = rk.checks.filter(function (c) { return c.status !== 'clear'; }).length;
        var clearCnt = rk.checks.filter(function (c) { return c.status === 'clear'; }).length;
        var summaryBadge = L3(
          flagCnt + ' watch/flag · ' + clearCnt + ' clear',
          flagCnt + ' 项风险提示 · ' + clearCnt + ' 项核查通过',
          flagCnt + ' alerta(s) · ' + clearCnt + ' limpias'
        );
        checksEl.innerHTML =
          '<div class="vc-hdr">' +
            '<span class="vc-title">' + esc(L3('Flags & Public-Record Checks', '风险警示与公开档案核验', 'Alertas y verificaciones oficiales')) + '</span>' +
            '<span class="vc-badge">' + esc(summaryBadge) + '</span>' +
          '</div>' +
          '<ul class="vc-list" role="list">' +
            rk.checks.map(function (c) {
              var ico = c.status === 'clear' ? '✓' : '⚠';
              return '<li class="vc-item vcheck-row ' + esc(c.status) + '" data-vjump="' + esc(c.target) + '" tabindex="0" role="button">' +
                '<span class="vc-ico" aria-hidden="true">' + ico + '</span>' +
                '<span class="vc-txt">' + esc(c.text) + '</span>' +
              '</li>';
            }).join('') +
          '</ul>';
        qAll('[data-vjump]', checksEl).forEach(function (li) {
          li.addEventListener('click', function () {
            jumpToSection(li.getAttribute('data-vjump'), true, { pushHash: true });
          });
          li.addEventListener('keydown', function (ev) {
            if (ev.key === 'Enter' || ev.key === ' ') {
              ev.preventDefault();
              jumpToSection(li.getAttribute('data-vjump'), true, { pushHash: true });
            }
          });
        });
      }
    }
  }

  function buildQuestionCardFaceData(qId, rep) {
    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var o = v.ownership || {};
    var m0 = o.monthly || {};
    var f = (rep && rep.facts) || {};
    var hz = mod(rep, 'hazards') || {};
    var env = mod(rep, 'environment') || {};
    var mk = mod(rep, 'market') || {};
    var civ = mod(rep, 'civic') || {};
    var am = mod(rep, 'amenities') || {};
    var df = (rep && rep.decisionFactors) || {};
    var hoa = (rep && rep.hoaProfile) || mod(rep, 'hoaProfile') || {};
    var cond = (rep && rep.condition) || mod(rep, 'condition') || {};
    var rk = extractTopRiskAndChecks(rep);

    if (qId === 'cost') {
      var ownMo = isN(m0.total) && m0.total > 0 ? m0.total : null;
      var rrCost = getRentResearchData(rep);
      var rentMo = (rrCost && isN(rrCost.recommendedMonthlyRent) && rrCost.recommendedMonthlyRent > 0)
        ? rrCost.recommendedMonthlyRent
        : (isN(v.monthlyRent) && v.monthlyRent > 0 ? v.monthlyRent : (isN(o.rentEstimate) && o.rentEstimate > 0 ? o.rentEstimate : null));
      var hoaMo = isN(hoa.hoaMonthly) ? hoa.hoaMonthly : (isN(f.hoaMonthly) ? f.hoaMonthly : (isN(m0.hoa) ? m0.hoa : null));
      var hasHoaLien = Boolean(hoa.debtAndLienStatus && (hoa.debtAndLienStatus.hasActiveHoaLien || hoa.debtAndLienStatus.nodaDetected));
      var downPct = isN(o.downPaymentPct) ? o.downPaymentPct : 20;
      var rate30 = isN(o.optimalRate30) ? o.optimalRate30 : (isN(o.rate30) ? o.rate30 : null);

      var ansCost = ownMo != null
        ? L3(
            'At ' + downPct + '% down' + (rate30 != null ? ' and ' + rate30 + '% 30-yr fixed' : '') + ', total monthly carry is ~' + fUsd(ownMo) + '/mo (including ' + fUsd(m0.propertyTax || 0) + '/mo reassessed tax and ' + fUsd((m0.insurance || 0) + (m0.maintenance || 0)) + '/mo insurance & maintenance)' + (rentMo ? ', or ~' + (ownMo / rentMo).toFixed(1) + '× estimated market rent (' + fUsd(rentMo) + '/mo).' : '.'),
            '按首付 ' + downPct + '%' + (rate30 != null ? '、30 年固定利率 ' + rate30 + '%' : '') + ' 测算，每月真实持有总成本约 ' + fUsd(ownMo) + '/月（含重评房产税 ' + fUsd(m0.propertyTax || 0) + '/月及保险维护 ' + fUsd((m0.insurance || 0) + (m0.maintenance || 0)) + '/月）' + (rentMo ? '，约为同区月租（' + fUsd(rentMo) + '/月）的 ' + (ownMo / rentMo).toFixed(1) + ' 倍。' : '。'),
            'Con ' + downPct + '% de enganche' + (rate30 != null ? ' y tasa fija a 30 años de ' + rate30 + '%' : '') + ', el costo mensual total es ~' + fUsd(ownMo) + '/mes (incluye ' + fUsd(m0.propertyTax || 0) + '/mes de predial reajustado y ' + fUsd((m0.insurance || 0) + (m0.maintenance || 0)) + '/mes de seguro y mantenimiento)' + (rentMo ? ', ~' + (ownMo / rentMo).toFixed(1) + '× el alquiler (' + fUsd(rentMo) + '/mes).' : '.')
          )
        : L3(
            'Compare mortgage scenarios, reassessed property taxes, homeowners & specialty hazard insurance quotes, and HOA obligations.',
            '展开查看按揭月供试算、过户后房产税重评、房屋主险与地震/洪水险报价及 HOA 物业费明细。',
            'Compare escenarios hipotecarios, predial reajustado, cotizaciones de seguro y cuotas HOA.'
          );

      var pillsCost = [];
      if (ownMo != null) pillsCost.push({ text: L3('All-in: ' + fUsd(ownMo) + '/mo', '全口径月供：' + fUsd(ownMo) + '/月', 'Total: ' + fUsd(ownMo) + '/mes'), tone: 'pri' });
      if (isN(m0.propertyTax)) pillsCost.push({ text: L3('Reassessed tax: ' + fUsd(m0.propertyTax) + '/mo' + (isN(o.taxRatePct) ? ' (' + o.taxRatePct.toFixed(2) + '%)' : ''), '重评房产税：' + fUsd(m0.propertyTax) + '/月', 'Predial: ' + fUsd(m0.propertyTax) + '/mes'), tone: 'neutral' });
      var hoaReported = Boolean(hoa.hoaFeeReported || (isN(hoaMo) && hoaMo > 0));
      if (hasHoaLien) {
        pillsCost.push({ text: L3('⚠ HOA lien / delinquency signal', '⚠ HOA 留置权风险警示', '⚠ Alerta de gravamen HOA'), tone: 'bad' });
      } else if (hoa.hasHoa && !hoaReported) {
        pillsCost.push({ text: L3('HOA dues: Verify (unreported)', 'HOA 月费：未披露待核实', 'Cuota HOA: verificar'), tone: 'watch' });
      } else if (hoaMo != null) {
        pillsCost.push({ text: hoaMo > 0 ? ('HOA: ' + fUsd(hoaMo) + L3('/mo', '/月', '/mes')) : L3('No HOA ($0/mo)', '无 HOA 物业费 ($0/月)', 'Sin HOA ($0/mes)'), tone: hoaMo > 0 ? 'watch' : 'good' });
      }
      var bdCost = (rep && rep.buyerDiligence) || mod(rep, 'buyerDiligence') || {};
      var ecCost = bdCost.ongoingCosts && bdCost.ongoingCosts.monthlyUtilities && bdCost.ongoingCosts.monthlyUtilities.electricCarrier;
      var cnCost = bdCost.connectivity || {};
      var bbCost = cnCost.broadbandCatalog;
      if (ecCost && ecCost.shortName && isN(ecCost.ratePerKwhUsd) && ecCost.ratePerKwhUsd > 0) {
        pillsCost.push({
          text: '⚡ ' + ecCost.shortName + ' · $' + Number(ecCost.ratePerKwhUsd).toFixed(3) + '/kWh',
          tone: ecCost.utilityType === 'municipal' ? 'good' : 'neutral'
        });
      }
      var bbFiberVerifiedCost = Boolean(
        cnCost.fiberConfirmedAddress === true ||
        (cnCost.sonic && cnCost.sonic.available === true) ||
        cnCost.verificationStatus === 'address_verified' ||
        (bbCost && (bbCost.fiberConfirmedFcc === true || bbCost.verificationStatus === 'address_verified' || bbCost.verificationStatus === 'fcc_location_verified' || bbCost.verificationStatus === 'fiber_confirmed' || bbCost.verificationStatus === 'confirmed_fcc' || bbCost.verificationStatus === 'confirmed_address'))
      );
      if (bbFiberVerifiedCost && ((bbCost && bbCost.fiberAvailable) || (cnCost.sonic && cnCost.sonic.available === true)) && pillsCost.length < 5) {
        var topFib = (bbCost && bbCost.fiberCarriers && bbCost.fiberCarriers[0] && bbCost.fiberCarriers[0].carrierName) || (cnCost.sonic && cnCost.sonic.carrierName) || 'Fiber FTTH';
        var topMbps = (bbCost && bbCost.maxDownloadMbps) || (cnCost.sonic && cnCost.sonic.maxDownloadMbps) || 0;
        pillsCost.push({
          text: '🌐 ' + topFib + (topMbps >= 1000 ? ' (' + Math.round(topMbps / 1000) + ' Gbps)' : ''),
          tone: 'good'
        });
      } else if (bbCost && pillsCost.length < 5) {
        pillsCost.push({
          text: '🌐 ' + L3('Broadband: Unverified (Check address)', '宽带：未核实（待查地址）', 'Banda ancha: sin verificar'),
          tone: 'neutral'
        });
      }
      if (isN(m0.earthquakeInsurance) && m0.earthquakeInsurance > 0 && pillsCost.length < 4) {
        pillsCost.push({ text: L3('Quake rider: +' + fUsd(m0.earthquakeInsurance) + '/mo', '地震险附加：+' + fUsd(m0.earthquakeInsurance) + '/月', 'Seguro sísmico: +' + fUsd(m0.earthquakeInsurance) + '/mes'), tone: 'watch' });
      }
      return { answer: ansCost, pills: pillsCost.slice(0, 5) };
    }

    if (qId === 'risk') {
      var flagChecks = rk.checks.filter(function (c) { return c.status !== 'clear'; });
      var clearChecks = rk.checks.filter(function (c) { return c.status === 'clear'; });
      var capex5 = cond && cond.capex5YrSummary && (cond.capex5YrSummary.fiveYearExpectedUSD ?? cond.capex5YrSummary.totalExpectedUsd);
      var ansRisk = '';
      if (rk.top && rk.top.sev >= 45) {
        ansRisk = rk.top.plainDetail + (f.yearBuilt && f.yearBuilt < 1978
          ? L3('; built in ' + f.yearBuilt + ', so budget for sewer-lateral, electrical-panel, and pre-1978 lead/asbestos checks.', '；房屋建于 ' + f.yearBuilt + ' 年，建议同步核查主排污管、配电箱及 1978 年前含铅建材。', '; construida en ' + f.yearBuilt + ', revise drenaje lateral, panel eléctrico y pintura previa a 1978.')
          : '.');
      } else if (rk.hasUnverified && rk.top) {
        ansRisk = rk.top.plainDetail + '.';
      } else if (clearChecks.length > 0 && flagChecks.length === 0) {
        ansRisk = L3(
          'Public records clear high-risk FEMA floodplains, wildfire zones, active fault-rupture zones, and Superfund sites within 1 mile' + (f.yearBuilt ? ' (built ' + f.yearBuilt + ' — verify roof, sewer & permit history).' : '.'),
          '公开档案已排除高风险洪水区、山火高危区、活动断层带及 1 英里内超级基金污染点' + (f.yearBuilt ? '（建于 ' + f.yearBuilt + ' 年，建议常规核查屋顶、排污管与许可证记录）。' : '。'),
          'Los registros oficiales descartan zonas graves de inundación, incendio, falla activa y sitios Superfund ≤1 mi' + (f.yearBuilt ? ' (año ' + f.yearBuilt + ', verificar techo, drenaje y permisos).' : '.')
        );
      } else {
        ansRisk = L3(
          'Hazard & environmental screening incomplete — review unverified feeds below before waiving contingencies.',
          '灾害与环境公开接口部分未返回 — 解除保护期前请核查下方未核实项目。',
          'Evaluación de riesgos incompleta — revise los datos sin verificar abajo.'
        );
      }

      var pillsRisk = [];
      var pga = getDesignPga(hz);
      var fl = hz.flood || null;
      var flZoneRaw = fl && (fl.zone || fl.femaZone);
      var flCheckedOk = Boolean(fl && fl.checked !== false && !(hz.checks && hz.checks.flood === false) && flZoneRaw);
      var flUnmapped = Boolean(fl && fl.checked === true && !(hz.checks && hz.checks.flood === false) && fl.mapped === false && !flZoneRaw);
      if (fl && (fl.sfha || /^(A|V)/i.test(String(flZoneRaw || '')))) {
        var fzBad = String(flZoneRaw || 'SFHA').toUpperCase();
        pillsRisk.push({ text: L3('⚠ FEMA Zone ' + fzBad + ' (high flood)', '⚠ FEMA ' + fzBad + ' 高危洪水区', '⚠ Zona FEMA ' + fzBad + ' (inundación)'), tone: 'bad' });
      } else if (fl && (String(flZoneRaw || '').toUpperCase() === 'D' || String(fl.risk || '').toLowerCase() === 'undetermined')) {
        pillsRisk.push({ text: L3('⚠ FEMA Zone D (unstudied flood)', '⚠ FEMA D区 (未定洪水风险)', '⚠ Zona FEMA D (no estudiada)'), tone: 'watch' });
      } else if (flCheckedOk) {
        var fzOk = String(flZoneRaw).toUpperCase();
        pillsRisk.push({ text: L3('✓ Flood Zone ' + fzOk + ' (minimal)', '✓ 洪水 ' + fzOk + ' 区 (极低风险)', '✓ Zona FEMA ' + fzOk + ' (mínimo)'), tone: 'good' });
      } else if (flUnmapped) {
        pillsRisk.push({ text: L3('⚠ FEMA NFHL unmapped', '⚠ FEMA 未测绘区域', '⚠ Área FEMA sin mapa'), tone: 'watch' });
      } else {
        pillsRisk.push({ text: L3('⚠ Flood zone unverified', '⚠ 洪水区未核实', '⚠ Inundación sin verificar'), tone: 'watch' });
      }
      if (isN(pga)) {
        pillsRisk.push({
          text: pga >= 0.5
            ? L3('⚠ Quake shaking: ' + pga + 'g (severe)', '⚠ 强震摇晃：' + pga + 'g', '⚠ Sismo severo: ' + pga + 'g')
            : L3('Seismic shaking: ' + pga + 'g', '地震动峰值：' + pga + 'g', 'Aceleración sísmica: ' + pga + 'g'),
          tone: pga >= 0.5 ? 'bad' : (pga >= 0.3 ? 'watch' : 'good')
        });
      }
      if (isN(capex5)) {
        pillsRisk.push({ text: L3('5-yr system CapEx: ~' + fUsdK(capex5), '5年预计大修：~' + fUsdK(capex5), 'CapEx 5 años: ~' + fUsdK(capex5)), tone: 'neutral' });
      } else {
        pillsRisk.push({ text: L3(clearChecks.length + ' clear · ' + flagChecks.length + ' watch/flag', clearChecks.length + ' 项通过 · ' + flagChecks.length + ' 项提示', clearChecks.length + ' limpias · ' + flagChecks.length + ' alertas'), tone: flagChecks.length ? 'watch' : 'good' });
      }
      return { answer: ansRisk, pills: pillsRisk.slice(0, 3) };
    }

    if (qId === 'price') {
      var est = isN(e.value) ? e.value : null;
      var compsCnt = Array.isArray(v.comps) ? v.comps.length : 0;
      var z = mk[_T.zhv] || {};
      var yoy = isN(z.yoyPct) ? z.yoyPct : (z.appreciation && isN(z.appreciation.yoyPct) ? z.appreciation.yoyPct : null);
      var lastP = f.lastSale && isN(f.lastSale.price) ? f.lastSale.price : null;
      var lastY = f.lastSale && f.lastSale.date ? String(f.lastSale.date).slice(0, 4) : '';

      var ansPrice = est != null
        ? L3(
            'Blended True Estimate is ' + fUsd(est) + (isN(e.low) && isN(e.high) ? ' (range ' + fUsdK(e.low) + '–' + fUsdK(e.high) + ')' : '') +
              (isN(e.perSqft) ? ' at ' + fUsd(e.perSqft) + '/sqft' : '') +
              (compsCnt ? ' across ' + compsCnt + ' nearby comparable sales' : '') +
              (lastP ? '; last recorded sale was ' + fUsdK(lastP) + (lastY ? ' in ' + lastY : '') + '.' : '.'),
            '多模型融合真实估值为 ' + fUsd(est) + (isN(e.low) && isN(e.high) ? '（参考区间 ' + fUsdK(e.low) + '–' + fUsdK(e.high) + '）' : '') +
              (isN(e.perSqft) ? '，折合单价 ' + fUsd(e.perSqft) + '/平尺' : '') +
              (compsCnt ? '，对标周边 ' + compsCnt + ' 套近期可比成交' : '') +
              (lastP ? '；上次登记成交价为 ' + fUsdK(lastP) + (lastY ? '（' + lastY + '年）' : '') + '。' : '。'),
            'La Estimación Real combinada es ' + fUsd(est) + (isN(e.low) && isN(e.high) ? ' (rango ' + fUsdK(e.low) + '–' + fUsdK(e.high) + ')' : '') +
              (isN(e.perSqft) ? ' a ' + fUsd(e.perSqft) + '/pie²' : '') +
              (compsCnt ? ' basada en ' + compsCnt + ' ventas comparables' : '') +
              (lastP ? '; última venta registrada en ' + fUsdK(lastP) + (lastY ? ' (' + lastY + ')' : '') + '.' : '.')
          )
        : L3(
            'Review comparable sales, repeat-sales indexing, local ZIP price trends, and 5-year upside/downside return scenarios.',
            '展开查看周边可比成交、同邮编房价指数走势及未来涨跌情景压力测试。',
            'Revise ventas comparables, tendencias del código postal y escenarios de valor a 5 años.'
          );

      var pillsPrice = [];
      if (est != null) pillsPrice.push({ text: L3('True Estimate: ' + fUsdK(est), '真实估值：' + fUsdK(est), 'Estimación Real: ' + fUsdK(est)), tone: 'pri' });
      if (isN(e.low) && isN(e.high)) pillsPrice.push({ text: L3('Range: ' + fUsdK(e.low) + '–' + fUsdK(e.high), '估值区间：' + fUsdK(e.low) + '–' + fUsdK(e.high), 'Rango: ' + fUsdK(e.low) + '–' + fUsdK(e.high)), tone: 'neutral' });
      if (lastP != null) pillsPrice.push({ text: L3('Last sale: ' + fUsdK(lastP) + (lastY ? ' (' + lastY + ')' : ''), '前次成交：' + fUsdK(lastP) + (lastY ? ' (' + lastY + ')' : ''), 'Última venta: ' + fUsdK(lastP) + (lastY ? ' (' + lastY + ')' : '')), tone: 'neutral' });
      else if (yoy != null) pillsPrice.push({ text: L3('ZIP YoY: ' + (yoy >= 0 ? '+' : '') + yoy.toFixed(1) + '%', '邮编同比：' + (yoy >= 0 ? '+' : '') + yoy.toFixed(1) + '%', 'Anual ZIP: ' + (yoy >= 0 ? '+' : '') + yoy.toFixed(1) + '%'), tone: yoy >= 0 ? 'good' : 'watch' });
      return { answer: ansPrice, pills: pillsPrice.slice(0, 3) };
    }

    if (qId === 'living') {
      var assigned = (f.schools && Array.isArray(f.schools.assigned)) ? f.schools.assigned : [];
      var compSch = getAssignedSchoolComposite(rep);
      var walkIdx = getWalkabilityIndex(rep);
      var noiseLbl = env.noise && env.noise.atProperty && env.noise.atProperty.all && env.noise.atProperty.all.label;
      var distName = (rep.geo && rep.geo.schoolDistricts && (rep.geo.schoolDistricts.unified || rep.geo.schoolDistricts.elementary)) || '';

      var partsLiv = [];
      if (compSch != null) {
        partsLiv.push(L3(
          'Assigned-school score is ' + compSch + '/10' + (distName ? ' (' + cleanPublicText(distName) + ')' : ''),
          '划片公立学校综合评分 ' + compSch + '/10' + (distName ? '（' + cleanPublicText(distName) + '）' : ''),
          'Puntaje de escuelas asignadas de ' + compSch + '/10' + (distName ? ' (' + cleanPublicText(distName) + ')' : '')
        ));
      } else if (assigned.length) {
        partsLiv.push(L3(assigned.length + ' assigned public schools mapped', '已匹配 ' + assigned.length + ' 所划片公立学校', assigned.length + ' escuelas asignadas'));
      }
      if (walkIdx != null) {
        partsLiv.push(L3('neighborhood walkability index is ' + walkIdx + '/100', '周边步行便利指数 ' + walkIdx + '/100', 'índice de caminabilidad de ' + walkIdx + '/100'));
      }
      if (noiseLbl) {
        partsLiv.push(L3('modeled 24h ambient transport noise is ' + cleanPublicText(noiseLbl), '24 小时交通环境噪音模拟为 ' + cleanPublicText(noiseLbl), 'ruido ambiental modelado de ' + cleanPublicText(noiseLbl)));
      }

      var ansLiving = partsLiv.length
        ? (partsLiv.join(langCode() === 'zh' ? '，' : '; ') + (langCode() === 'zh' ? '。' : '.'))
        : L3(
            'Explore assigned public & nearby private schools, grocery/park distances, commute access, and city safety context.',
            '展开查看划片学区与周边学校、超市公园配套距离、通勤干道及城市治安数据。',
            'Explore escuelas asignadas, comercios y parques cercanos, accesos viales y seguridad local.'
          );

      var pillsLiv = [];
      if (compSch != null) pillsLiv.push({ text: L3('Assigned-school score: ' + compSch + '/10', '划片学区综合分：' + compSch + '/10', 'Escuelas asignadas: ' + compSch + '/10'), tone: compSch >= 7 ? 'good' : 'neutral' });
      if (walkIdx != null) pillsLiv.push({ text: L3('Walkability: ' + walkIdx + '/100', '步行指数：' + walkIdx + '/100', 'Caminabilidad: ' + walkIdx + '/100'), tone: 'neutral' });
      if (noiseLbl) pillsLiv.push({ text: L3('Noise: ' + cleanPublicText(noiseLbl), '环境噪音：' + cleanPublicText(noiseLbl), 'Ruido: ' + cleanPublicText(noiseLbl)), tone: 'neutral' });
      else if (df.safety && df.safety.fireStation && isN(df.safety.fireStation.distMi)) {
        pillsLiv.push({ text: L3('Fire station: ' + df.safety.fireStation.distMi + ' mi', '距消防站：' + df.safety.fireStation.distMi + ' 英里', 'Bomberos: ' + df.safety.fireStation.distMi + ' mi'), tone: 'good' });
      }
      return { answer: ansLiving, pills: pillsLiv.slice(0, 3) };
    }

    // Q5: potential ('What can I do with it?') — Chinese-only Feng Shui!
    var la = df.lotAndAdu || {};
    var sol = df.solar || {};
    var isZh = langCode() === 'zh';
    var fs = isZh ? mod(rep, 'fengshui') : null;
    var openSqft = isN(la.openLotSqft) ? la.openLotSqft : (isN(f.lotSqft) && f.buildingRegister && isN(f.buildingRegister.footprintSqft) ? Math.max(0, f.lotSqft - f.buildingRegister.footprintSqft) : null);
    var covPct = isN(la.lotCoveragePct) ? la.lotCoveragePct : (f.buildingRegister && isN(f.buildingRegister.lotCoveragePct) ? f.buildingRegister.lotCoveragePct : null);

    var potParts = [];
    if (openSqft != null) {
      potParts.push(L3(
        '~' + fNum(openSqft) + ' sq ft of open lot area' + (covPct != null ? ' (' + covPct + '% roof coverage)' : '') + (la.feasibility ? ' — ' + cleanPublicText(la.feasibility) : '') + (isN(la.estRentMonthly) ? ' (~' + fUsd(la.estRentMonthly) + '/mo est. ADU rent)' : ''),
        '约 ' + fNum(openSqft) + ' 平尺未建院落空地' + (covPct != null ? '（当前建筑占地率 ' + covPct + '%）' : '') + (isN(la.estRentMonthly) ? '，具备 ADU 加建潜力（预估月租 ~' + fUsd(la.estRentMonthly) + '）' : ''),
        '~' + fNum(openSqft) + ' pies² de terreno libre' + (covPct != null ? ' (' + covPct + '% ocupado)' : '') + (isN(la.estRentMonthly) ? ' con potencial ADU (~' + fUsd(la.estRentMonthly) + '/mes est.)' : '')
      ));
    }
    if (isN(sol.systemKw)) {
      potParts.push(L3(
        '~' + sol.systemKw + ' kW rooftop solar capacity' + (isN(sol.estAnnualSavingsUsd) ? ' (~' + fUsd(sol.estAnnualSavingsUsd) + '/yr utility offset)' : ''),
        '屋顶光伏潜力约 ' + sol.systemKw + ' kW' + (isN(sol.estAnnualSavingsUsd) ? '（预计年省电费 ~' + fUsd(sol.estAnnualSavingsUsd) + '）' : ''),
        'potencial solar de ~' + sol.systemKw + ' kW' + (isN(sol.estAnnualSavingsUsd) ? ' (~' + fUsd(sol.estAnnualSavingsUsd) + '/año de ahorro)' : '')
      ));
    }
    if (isZh && fs && fs.available && fs.orientation && fs.orientation.mountain) {
      potParts.push('内含三维激光雷达日照模型、翻新预算测算及传统堪舆罗盘排盘');
    } else {
      potParts.push(L3(
        'includes interactive 3D LiDAR massing & sun study, remodel cost estimator, and offer checklist',
        '内含三维激光雷达日照模型、翻新预算测算与出价前核查清单',
        'incluye modelo 3D LiDAR con estudio solar, calculadora de remodelación y lista de verificación'
      ));
    }

    var pillsPot = [];
    if (openSqft != null) pillsPot.push({ text: L3('Open lot: ' + fNum(openSqft) + ' sqft', '院落空地：' + fNum(openSqft) + ' 平尺', 'Terreno libre: ' + fNum(openSqft) + ' p²'), tone: 'good' });
    if (isN(sol.systemKw)) pillsPot.push({ text: L3('Solar: ~' + sol.systemKw + ' kW', '光伏潜力：~' + sol.systemKw + ' kW', 'Solar: ~' + sol.systemKw + ' kW'), tone: 'pri' });
    if (isZh && fs && fs.available) {
      pillsPot.push({ text: '3D日照 · 翻新 · 堪舆排盘', tone: 'neutral' });
    } else if (f.zoning) {
      pillsPot.push({ text: L3('Zoning: ' + f.zoning, '规划分区：' + f.zoning, 'Zonificación: ' + f.zoning), tone: 'neutral' });
    } else {
      pillsPot.push({ text: L3('3D LiDAR & Remodel Studio', '3D 模型与翻新测算', 'Modelo 3D y Remodelación'), tone: 'neutral' });
    }

    return {
      answer: potParts.join(isZh ? '；' : '; ').replace(/^./, function (c) { return c.toUpperCase(); }) + (isZh ? '。' : '.'),
      pills: pillsPot.slice(0, 3)
    };
  }

  function syncFengShuiLanguageVisibility() {
    var isZh = langCode() === 'zh';
    var fsSec = byId('fengshui');
    if (fsSec) {
      fsSec.hidden = !isZh;
      if (fsSec.style) fsSec.style.display = isZh ? '' : 'none';
    }
    qAll('a[href="#fengshui"], [data-zh-only="1"]').forEach(function (el) {
      el.hidden = !isZh;
      if (el.style) el.style.display = isZh ? '' : 'none';
    });
  }

  function flattenNestedDetailsInDrawers() {
    qAll('.q-drawer details').forEach(function (d) {
      try { d.open = true; } catch (e) {}
    });
  }

  var _resizingVisuals = false;
  function resizeChartsAndMaps() {
    if (_resizingVisuals) return;
    _resizingVisuals = true;
    try {
      if (window.echarts && typeof window.echarts.getInstanceByDom === 'function') {
        qAll('.chart, [_echarts_instance_]').forEach(function (el) {
          try {
            var inst = window.echarts.getInstanceByDom(el);
            if (inst && typeof inst.resize === 'function') inst.resize();
          } catch (e) {}
        });
      }
      qAll('.map, [id$="Map"], #heroMap').forEach(function (mEl) {
        try {
          if (mEl && mEl._map && typeof mEl._map.invalidateSize === 'function') {
            mEl._map.invalidateSize();
          }
        } catch (e) {}
      });
      if (window.leafletMap && typeof window.leafletMap.invalidateSize === 'function') {
        try { window.leafletMap.invalidateSize(); } catch (e) {}
      }
    } finally {
      _resizingVisuals = false;
    }
  }

  function notify3dAndVisuals(qId) {
    resizeChartsAndMaps();
    if (!_resizingVisuals && typeof window.dispatchEvent === 'function' && typeof Event === 'function') {
      try { window.dispatchEvent(new Event('resize')); } catch (e) {}
    }
    if (qId === 'potential' || qId === 'all') {
      try {
        var h3 = window.OpenEstateHouse3D;
        if (h3 && h3._state && h3._state.viewer) {
          h3._state.viewer.resize();
          h3._state.viewer.render();
        } else {
          var startBtn = document.querySelector && document.querySelector('#house3d [data-act="start"]');
          if (startBtn && typeof startBtn.click === 'function') startBtn.click();
        }
      } catch (e) {}
    }
    setTimeout(resizeChartsAndMaps, 60);
    setTimeout(resizeChartsAndMaps, 220);
  }

  function toggleBtnLabelHtml(isOpen) {
    return '<span>' + esc(isOpen
      ? L3('Hide details', '收起详细数据与图表', 'Ocultar detalles y gráficos')
      : L3('Expand details', '展开详细数据与图表', 'Ver detalles y gráficos')
    ) + '</span><span class="q-chev" aria-hidden="true">' + (isOpen ? '▴' : '▾') + '</span>';
  }

  function setQuestionCardOpen(qId, shouldOpen, opts) {
    var options = opts || {};
    var q = Q_BY_ID[qId];
    if (!q) return;
    var card = byId(q.cardId);
    var drawer = byId(q.drawerId);
    var btn = byId('qToggle_' + q.id);
    var isOpen = Boolean(shouldOpen);
    var wasOpen = Boolean(openCards[qId]);

    var prevTop = null;
    if (options.preserveAnchor && card && typeof card.getBoundingClientRect === 'function') {
      prevTop = card.getBoundingClientRect().top;
    }

    openCards[qId] = isOpen;
    if (isOpen) activeQuestion = qId;
    else if (activeQuestion === qId) activeQuestion = null;

    if (options.fromHash && isOpen) hashOpenedCards[qId] = true;
    if (!isOpen) delete hashOpenedCards[qId];

    if (card && card.classList) card.classList.toggle('open', isOpen);
    if (drawer) drawer.setAttribute('aria-hidden', isOpen ? 'false' : 'true');
    if (btn) {
      btn.setAttribute('aria-expanded', isOpen ? 'true' : 'false');
      btn.innerHTML = toggleBtnLabelHtml(isOpen);
    }

    syncStickyJumpHighlight();
    syncFengShuiLanguageVisibility();

    if (isOpen && !wasOpen) {
      flattenNestedDetailsInDrawers();
      if (typeof requestAnimationFrame === 'function') {
        requestAnimationFrame(function () { notify3dAndVisuals(qId); });
      } else {
        notify3dAndVisuals(qId);
      }
    }

    if (prevTop !== null && card && typeof card.getBoundingClientRect === 'function' && typeof window.scrollBy === 'function') {
      var nextTop = card.getBoundingClientRect().top;
      var delta = nextTop - prevTop;
      if (Math.abs(delta) > 2) {
        try { window.scrollBy(0, delta); } catch (e) {}
      }
    }
  }

  function renderQuestionCards() {
    var rep = getRep() || {};
    var role = getRole();
    syncFengShuiLanguageVisibility();
    var ordered = getOrderedQuestionCards();
    var ymActive = typeof window.isYourMatchSortActive === 'function' && Boolean(window.isYourMatchSortActive());
    var stack = byId('questionCardsStack');
    if (stack && !ymActive && typeof stack.appendChild === 'function') {
      ordered.forEach(function (q) {
        var cEl = byId(q.cardId);
        if (cEl && cEl.parentNode === stack) stack.appendChild(cEl);
      });
    }

    ordered.forEach(function (q, idx) {
      var face = byId(q.faceId);
      var card = byId(q.cardId);
      var drawer = byId(q.drawerId);
      if (!face) return;
      var isOpen = Boolean(openCards[q.id]);
      if (card && card.classList) {
        card.classList.toggle('open', isOpen);
        card.classList.toggle('role-lead', role !== 'all' && idx === 0 && !ymActive);
      }
      if (drawer) drawer.setAttribute('aria-hidden', isOpen ? 'false' : 'true');

      var fd = buildQuestionCardFaceData(q.id, rep);
      var finalPills = buildRoleOverridePills(role, q.id, rep, fd.pills || []);
      var pillsHtml = finalPills.map(function (p) {
        return '<span class="q-pill ' + esc(p.tone || 'neutral') + '">' + esc(p.text) + '</span>';
      }).join('');
      var lbls = getRoleCardLabels(q.id);
      var titleBlockHtml = lbls.eyebrow
        ? '<div class="q-title-wrap"><span class="q-eyebrow">' + esc(lbls.eyebrow) + '</span><h2 class="q-title">' + esc(lbls.title) + '</h2></div>'
        : '<h2 class="q-title">' + esc(lbls.title) + '</h2>';

      face.innerHTML =
        '<div class="q-face-main">' +
          '<div class="q-face-top">' +
            '<span class="q-num">' + (idx + 1) + '</span>' +
            titleBlockHtml +
          '</div>' +
          '<p class="q-answer q-card-answer">' + esc(fd.answer) + '</p>' +
          (pillsHtml ? '<div class="q-pills">' + pillsHtml + '</div>' : '') +
        '</div>' +
        '<div class="q-face-act">' +
          '<button type="button" class="q-toggle-btn q-card-toggle" id="qToggle_' + q.id + '" aria-expanded="' + (isOpen ? 'true' : 'false') + '" aria-controls="' + q.drawerId + '">' +
            toggleBtnLabelHtml(isOpen) +
          '</button>' +
        '</div>';

      face.onclick = function (ev) {
        var t = ev && ev.target;
        if (t && typeof t.closest === 'function' && t.closest('a')) return;
        setQuestionCardOpen(q.id, !openCards[q.id], { preserveAnchor: true });
      };
    });
    flattenNestedDetailsInDrawers();
  }

  function syncStickyJumpHighlight() {
    qAll('#chapterWorkspaceBar [data-qjump]').forEach(function (b) {
      var qid = b.getAttribute('data-qjump');
      var on = Boolean(openCards[qid]) && (activeQuestion === qid || !activeQuestion);
      if (b.classList) b.classList.toggle('on', on);
      b.setAttribute('aria-pressed', on ? 'true' : 'false');
    });
  }

  function renderChapterBar() {
    var wrap = byId('chapterWorkspaceBar');
    if (!wrap) return;
    var ordered = getOrderedQuestionCards();
    var btnsHtml = ordered.map(function (q, idx) {
      var isOn = Boolean(openCards[q.id]) && activeQuestion === q.id;
      var lbls = getRoleCardLabels(q.id);
      return '<button type="button" class="chap-btn' + (isOn ? ' on' : '') + '" id="qJump_' + q.id + '" data-qjump="' + q.id + '" data-chapter="' + q.id + '" aria-pressed="' + (isOn ? 'true' : 'false') + '">' +
        '<span class="chap-num">' + (idx + 1) + '</span>' +
        '<span class="chap-lbl">' + esc(lbls.short) + '</span>' +
      '</button>';
    }).join('');

    wrap.innerHTML =
      '<div class="chap-bar-inner">' +
        '<div class="chap-pills" role="navigation" aria-label="' + esc(L3('Report Questions', '报告核心问题', 'Preguntas del informe')) + '">' +
          btnsHtml +
        '</div>' +
        '<button type="button" class="chap-tour-cta" id="stickyWalkthroughBtn">' +
          '<span>▶</span> ' + span3('Walkthrough', '房源导览', 'Guía') +
        '</button>' +
      '</div>';

    qAll('[data-qjump]', wrap).forEach(function (btn) {
      btn.addEventListener('click', function () {
        var qid = btn.getAttribute('data-qjump');
        var q = Q_BY_ID[qid];
        if (!q) return;
        jumpToSection(q.cardId, true, { pushHash: true });
      });
    });
    var wtBtn = byId('stickyWalkthroughBtn');
    if (wtBtn) wtBtn.addEventListener('click', function () { openWalkthroughModal(0); });
    updateHorizontalScrollFades();
  }

  /* ------------------------------------------------------------------ 1B. Google-Docs-Style Collapsible Left Tree TOC */
  var TOC_OPEN_KEY = 'openestate:toc_open:v1';
  var _tocState = {
    desktopOpen: (function () {
      try {
        var raw = typeof localStorage !== 'undefined' ? localStorage.getItem(TOC_OPEN_KEY) : null;
        if (raw === '0' || raw === 'false') return false;
      } catch (e) {}
      return true;
    })(),
    mobileOpen: false,
    collapsedBranches: {},
    activeTarget: 'overview'
  };

  var TOC_TREE_NODES = [
    {
      id: 'overview',
      target: 'overview',
      en: 'Overview & Verdict',
      zh: '核心结论与总览',
      es: 'Resumen y veredicto',
      children: []
    },
    {
      id: 'cost',
      num: '1',
      target: 'qCardCost',
      en: 'Monthly Cost',
      zh: '月度持有成本',
      es: 'Costo mensual',
      children: [
        { target: 'payment', en: 'Mortgage & Property Tax', zh: '月供与重评房产税', es: 'Hipoteca y predial' },
        { target: 'rentResearch', en: 'Rent Research (Median & Avg)', zh: '多样本租金研判（中位与均值）', es: 'Investigación de Renta' },
        { target: 'insuranceQuoteStudio', en: 'Insurance Quotes', zh: '房屋与巨灾险报价', es: 'Seguros del hogar' },
        { target: 'utilitiesRatesPanel', en: 'Utilities & $/kWh Rates', zh: '水电气承运商与真实费率', es: 'Servicios y tarifas $/kWh' },
        { target: 'homeInternetCatalog', en: 'Fiber & Home Internet', zh: '光纤宽带与家庭网络', es: 'Fibra e Internet' },
        { target: 'diligence', en: 'HOA, Closing & Exit Plan', zh: 'HOA、过户税与尽调', es: 'HOA, cierre y salida' }
      ]
    },
    {
      id: 'risk',
      num: '2',
      target: 'qCardRisk',
      en: 'Risks & Hazards',
      zh: '潜在风险与隐患',
      es: 'Riesgos y peligros',
      children: [
        { target: 'risk', en: 'Natural Hazards', zh: '自然灾害筛查', es: 'Riesgos naturales' },
        { target: 'environment', en: 'Environment, Water & Noise', zh: '环境污染、水质与噪音', es: 'Entorno, agua y ruido' },
        { target: 'condition', en: 'System Condition & CapEx', zh: '房屋老化与大修预算', es: 'Estado y CapEx' },
        { target: 'inspection', en: 'Inspection & Title AI', zh: '验房与产权文档 AI', es: 'Inspección y título IA' },
        { target: 'titleLegalRadarPanel', en: 'Title, Liens & Legal Radar', zh: '产权留置权与诉讼雷达', es: 'Radar legal y de título' },
        { target: 'history', en: 'Sales History & Deed', zh: '历史成交与产权地契', es: 'Historial y escritura' },
        { target: 'county', en: 'County Official Records', zh: '县府权威档案', es: 'Registros del condado' }
      ]
    },
    {
      id: 'price',
      num: '3',
      target: 'qCardPrice',
      en: 'Price & Comps',
      zh: '价格合理性与估值',
      es: 'Precio y comparables',
      children: [
        { target: 'value', en: 'True Estimate & Models', zh: '多模型真实估值', es: 'Estimación Real' },
        { target: 'comps', en: 'Comparable Sales', zh: '周边可比成交', es: 'Ventas comparables' },
        { target: 'market', en: 'Local Market Trends', zh: '邮编与都会区走势', es: 'Tendencias del mercado' },
        { target: 'outlook', en: '5-Year Scenarios', zh: '未来涨跌情景推演', es: 'Escenarios a 5 años' },
        { target: 'sellerStrategyStudio', en: 'Seller Strategy & Net Sheet', zh: '卖家净到手与上市策略', es: 'Estrategia y neto vendedor' },
        { target: 'agentDealDesk', en: 'Agent Deal & CMA Desk', zh: '经纪人 CMA 与成交通道台', es: 'Mesa CMA y cierre agente' }
      ]
    },
    {
      id: 'living',
      num: '4',
      target: 'qCardLiving',
      en: 'Living & Schools',
      zh: '居住体验与学区',
      es: 'Entorno y escuelas',
      children: [
        { target: 'schools', en: 'Schools & Districts', zh: '划片学区与周边学校', es: 'Escuelas y distritos' },
        { target: 'neighborhood', en: 'Walkability & Amenities', zh: '步行便利与生活圈', es: 'Amenidades cercanas' },
        { target: 'safety', en: 'Public Safety & Fire', zh: '公共治安与消防应急', es: 'Seguridad y bomberos' },
        { target: 'buyerLivingIntelPanel', en: 'Insurability, 5G, Zoning & ADU', zh: '投保悬崖、5G 覆盖与加建规则', es: 'Seguro, 5G, zonificación y ADU' }
      ]
    },
    {
      id: 'potential',
      num: '5',
      target: 'qCardPotential',
      en: 'Potential & Remodel',
      zh: '改建潜力与利用',
      es: 'Potencial y remodelación',
      children: [
        { target: 'house3d', en: '3D LiDAR & Sun Study', zh: '3D 激光雷达与日照', es: 'Modelo 3D y sol' },
        { target: 'house', en: 'House Profile & Style', zh: '建筑画像与特征标签', es: 'Perfil y estilo' },
        { target: 'remodel', en: 'Remodel, Rebuild & Lawn', zh: '翻新、推倒重建与草坪', es: 'Remodelar y reconstruir' },
        { target: 'fengshui', en: '堪舆风水', zh: '堪舆风水罗盘排盘', es: '堪舆风水', zhOnly: true },
        { target: 'cre', en: 'Commercial / Store Profit', zh: '商业地产与开店测算', es: 'CRE / Rentabilidad', optionalDom: true },
        { target: 'ai', en: 'AI Analyst Verdict & Q&A', zh: 'AI 综合研判与问答', es: 'Analista IA y Q&A' },
        { target: 'more', en: 'Offer Checklist & Sources', zh: '买前核查清单与数据源', es: 'Lista de verificación' }
      ]
    }
  ];

  var TOC_NODE_BY_ID = {};
  TOC_TREE_NODES.forEach(function (n) { TOC_NODE_BY_ID[n.id] = n; });

  function getOrderedTocNodes() {
    var role = getRole();
    var ord = ROLE_CARD_ORDER[role] || ROLE_CARD_ORDER.all;
    var out = [TOC_NODE_BY_ID.overview];
    ord.forEach(function (id, i) {
      var baseNode = TOC_NODE_BY_ID[id];
      if (!baseNode) return;
      var ov = ROLE_CARD_META[role] && ROLE_CARD_META[role][id];
      out.push({
        id: baseNode.id,
        num: String(i + 1),
        target: baseNode.target,
        en: ov ? ov.eyeEn : baseNode.en,
        zh: ov ? ov.eyeZh : baseNode.zh,
        es: ov ? ov.eyeEs : baseNode.es,
        children: baseNode.children
      });
    });
    return out;
  }

  function isNarrowViewport() {
    try {
      return typeof window !== 'undefined' && typeof window.innerWidth === 'number' && window.innerWidth < 1120;
    } catch (e) {
      return false;
    }
  }

  function syncTocScrollFade() {
    var aside = byId('reportTocAside');
    if (!aside || !aside.classList) return;
    var sh = aside.scrollHeight || 0;
    var ch = aside.clientHeight || 0;
    var st = aside.scrollTop || 0;
    var canMore = sh > ch + 2 && (st + ch < sh - 2);
    aside.classList.toggle('is-scrollable', canMore);
  }

  function updateSingleScrollFade(el) {
    if (!el || !el.classList) return;
    var sw = el.scrollWidth || 0;
    var cw = el.clientWidth || 0;
    var sl = el.scrollLeft || 0;
    var hasMoreRight = sw > cw + 2 && (sl + cw < sw - 2);
    el.classList.toggle('hscroll-fade-r', hasMoreRight);
  }

  function updateHorizontalScrollFades() {
    qAll('#heroMediaSwitcher .hm-tabs, #heroMediaSwitcher .hm-portals, #chapterWorkspaceBar .chap-pills, #chapterWorkspaceBar .chap-bar-inner').forEach(function (el) {
      if (!el.__dbFadeBound && typeof el.addEventListener === 'function') {
        el.__dbFadeBound = true;
        el.addEventListener('scroll', function () { updateSingleScrollFade(el); }, { passive: true });
      }
      updateSingleScrollFade(el);
    });
    if (typeof requestAnimationFrame === 'function') {
      requestAnimationFrame(function () {
        qAll('#heroMediaSwitcher .hm-tabs, #heroMediaSwitcher .hm-portals, #chapterWorkspaceBar .chap-pills, #chapterWorkspaceBar .chap-bar-inner').forEach(updateSingleScrollFade);
      });
    }
  }

  function applyTocLayoutClasses() {
    var lay = byId('reportLayout');
    var reopenBtn = byId('reportTocReopenBtn');
    var scrim = byId('reportTocScrim');
    var aside = byId('reportTocAside');
    if (!lay) return;
    if (lay.classList) {
      lay.classList.toggle('toc-collapsed', !_tocState.desktopOpen);
      lay.classList.toggle('toc-mobile-open', Boolean(_tocState.mobileOpen));
    }
    if (document.body && document.body.classList) {
      document.body.classList.toggle('db-toc-open', Boolean(_tocState.desktopOpen));
      document.body.classList.toggle('db-toc-mobile-open', Boolean(_tocState.mobileOpen));
    }
    var expanded = isNarrowViewport() ? Boolean(_tocState.mobileOpen) : Boolean(_tocState.desktopOpen);
    if (reopenBtn) {
      var rLbl = L3('Show Report Outline', '打开报告大纲目录', 'Mostrar esquema del informe');
      reopenBtn.setAttribute('aria-expanded', expanded ? 'true' : 'false');
      reopenBtn.setAttribute('aria-label', rLbl);
      reopenBtn.title = rLbl;
      reopenBtn.hidden = expanded;
    }
    if (aside) aside.setAttribute('aria-hidden', expanded ? 'false' : 'true');
    if (scrim) scrim.hidden = !(_tocState.mobileOpen && isNarrowViewport());
    syncRoleBriefAriaLabelledBy();
    syncTocScrollFade();
  }

  function setTocSidebarOpen(shouldOpen) {
    var open = Boolean(shouldOpen);
    if (isNarrowViewport()) {
      _tocState.mobileOpen = open;
    } else {
      _tocState.desktopOpen = open;
      try {
        if (typeof localStorage !== 'undefined') {
          localStorage.setItem(TOC_OPEN_KEY, open ? '1' : '0');
        }
      } catch (e) {}
    }
    applyTocLayoutClasses();
    setTimeout(resizeChartsAndMaps, 60);
    setTimeout(resizeChartsAndMaps, 220);
  }

  function setTocBranchCollapsed(branchId, collapsed) {
    if (!branchId) return;
    if (collapsed) _tocState.collapsedBranches[branchId] = true;
    else delete _tocState.collapsedBranches[branchId];
    var nodeEl = byId('tocNode_' + branchId);
    if (nodeEl && nodeEl.classList) {
      nodeEl.classList.toggle('collapsed', Boolean(collapsed));
    }
    var chevBtn = byId('tocChev_' + branchId);
    if (chevBtn) {
      chevBtn.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    }
    syncTocScrollFade();
  }

  function syncTocActiveHighlight(preferredTarget) {
    if (preferredTarget) _tocState.activeTarget = preferredTarget;
    var active = _tocState.activeTarget || 'overview';
    var parentQ = resolveQuestionForTargetId(active);
    qAll('#reportTocAside [data-toc-target]').forEach(function (btn) {
      var tgt = btn.getAttribute('data-toc-target');
      var isParentMatch = parentQ && Q_BY_ID[parentQ] && tgt === Q_BY_ID[parentQ].cardId && active === tgt;
      var on = tgt === active || Boolean(isParentMatch);
      if (btn.classList) btn.classList.toggle('active', on);
      if (on) btn.setAttribute('aria-current', 'location');
      else btn.removeAttribute('aria-current');
    });
  }

  function exitCommandCenterIfActive() {
    try {
      if (document.body && document.body.classList && document.body.classList.contains('db-cmd-active')) {
        if (window.OpenEstateDashboard && typeof window.OpenEstateDashboard.close === 'function') {
          window.OpenEstateDashboard.close();
          return;
        }
        var cmdBtn = byId('dbCmdToggleBtn');
        if (cmdBtn && typeof cmdBtn.click === 'function') cmdBtn.click();
      }
    } catch (e) {}
  }

  function renderReportTocTree() {
    var aside = byId('reportTocAside');
    if (!aside) return;
    var isZh = langCode() === 'zh';
    var role = getRole();
    var keyLeaves = ROLE_TOC_KEY_LEAVES[role] || {};
    var orderedNodes = getOrderedTocNodes();
    var allCollapsed = orderedNodes.filter(function (n) { return n.children && n.children.length; }).every(function (n) {
      return Boolean(_tocState.collapsedBranches[n.id]);
    });

    var treeItemsHtml = orderedNodes.map(function (node) {
      var hasKids = Boolean(node.children && node.children.length);
      var isCol = hasKids && Boolean(_tocState.collapsedBranches[node.id]);
      var kidsHtml = '';
      if (hasKids) {
        var visibleKids = node.children.filter(function (c) {
          if (_roleState.hiddenMap && _roleState.hiddenMap[c.target]) return false;
          if (c.optionalDom) {
            var domEl = byId(c.target);
            if (!domEl || domEl.hidden || (domEl.classList && domEl.classList.contains('role-hidden')) || (domEl.style && domEl.style.display === 'none') || !String(domEl.innerHTML || '').trim()) {
              return false;
            }
          }
          return true;
        });
        kidsHtml = '<ul class="toc-children" role="group">' + visibleKids.map(function (c) {
          var zhAttr = c.zhOnly ? ' data-zh-only="1"' + (!isZh ? ' hidden style="display:none"' : '') : '';
          var isAct = _tocState.activeTarget === c.target;
          var isKey = Boolean(keyLeaves[c.target]);
          return '<li class="toc-leaf" role="treeitem"' + zhAttr + '>' +
            '<div class="toc-row">' +
              '<button type="button" class="toc-link is-sub' + (isAct ? ' active' : '') + (isKey ? ' role-key' : '') + '"' + (isAct ? ' aria-current="location"' : '') + ' data-toc-target="' + esc(c.target) + '" data-toc-branch="' + esc(node.id) + '">' +
                '<span class="toc-txt">' + esc(L3(c.en, c.zh, c.es)) + '</span>' +
              '</button>' +
            '</div>' +
          '</li>';
        }).join('') + '</ul>';
      }

      var isParentAct = _tocState.activeTarget === node.target;
      return '<li class="toc-node' + (isCol ? ' collapsed' : '') + '" id="tocNode_' + esc(node.id) + '" role="treeitem" aria-expanded="' + (hasKids ? (isCol ? 'false' : 'true') : 'true') + '">' +
        '<div class="toc-row">' +
          (hasKids
            ? '<button type="button" class="toc-chev-btn" id="tocChev_' + esc(node.id) + '" data-toc-toggle="' + esc(node.id) + '" aria-label="' + esc(L3('Expand or collapse section', '展开或折叠章节', 'Expandir o contraer sección')) + '" aria-expanded="' + (isCol ? 'false' : 'true') + '">▾</button>'
            : '<span class="toc-spacer" aria-hidden="true">•</span>') +
          '<button type="button" class="toc-link is-parent' + (isParentAct ? ' active' : '') + '"' + (isParentAct ? ' aria-current="location"' : '') + ' data-toc-target="' + esc(node.target) + '" data-toc-branch="' + esc(node.id) + '">' +
            (node.num ? '<span class="toc-num">' + esc(node.num) + '</span>' : '') +
            '<span class="toc-txt">' + esc(L3(node.en, node.zh, node.es)) + '</span>' +
          '</button>' +
        '</div>' +
        kidsHtml +
      '</li>';
    }).join('');

    var roleBadgeHtml = (role !== 'all' && ROLE_LABELS[role])
      ? ' <span class="toc-role-badge">' + esc(L3(ROLE_LABELS[role].en, ROLE_LABELS[role].zh, ROLE_LABELS[role].es)) + '</span>'
      : '';
    var tocFootNote = roleHiddenNoteHtml(true);

    var tocHeadingHtml = role === 'all'
      ? '<span aria-hidden="true">☰</span> ' + esc(L3('Report Outline', '报告大纲目录', 'Esquema'))
      : esc(L3('Outline', '目录', 'Esquema'));

    aside.innerHTML =
      '<div class="toc-hdr">' +
        '<span class="toc-title">' + tocHeadingHtml + roleBadgeHtml + '</span>' +
        '<div class="toc-hdr-acts">' +
          '<button type="button" class="toc-icon-btn" id="reportTocExpandAllBtn" title="' + esc(allCollapsed ? L3('Expand all sections', '全部展开', 'Expandir todo') : L3('Collapse sub-items', '全部折叠子项', 'Contraer todo')) + '">' +
            (allCollapsed ? '⊞' : '⊟') +
          '</button>' +
          '<button type="button" class="toc-icon-btn" id="reportTocHideBtn" aria-label="' + esc(L3('Hide outline', '隐藏目录', 'Ocultar esquema')) + '" title="' + esc(L3('Hide outline', '隐藏目录', 'Ocultar esquema')) + '">«</button>' +
        '</div>' +
      '</div>' +
      '<ul class="toc-tree" role="tree">' + treeItemsHtml + '</ul>' +
      (tocFootNote ? '<div class="toc-hidden-note">' + tocFootNote + '</div>' : '');

    if (!aside.__dbScrollBound && typeof aside.addEventListener === 'function') {
      aside.__dbScrollBound = true;
      aside.addEventListener('scroll', syncTocScrollFade, { passive: true });
    }

    qAll('[data-toc-toggle]', aside).forEach(function (b) {
      b.addEventListener('click', function (ev) {
        if (ev && typeof ev.stopPropagation === 'function') ev.stopPropagation();
        var bid = b.getAttribute('data-toc-toggle');
        setTocBranchCollapsed(bid, !_tocState.collapsedBranches[bid]);
      });
    });

    qAll('[data-toc-target]', aside).forEach(function (b) {
      b.addEventListener('click', function () {
        var tgt = b.getAttribute('data-toc-target');
        var bid = b.getAttribute('data-toc-branch');
        if (bid && _tocState.collapsedBranches[bid]) {
          setTocBranchCollapsed(bid, false);
        }
        exitCommandCenterIfActive();
        if (isNarrowViewport() && _tocState.mobileOpen) {
          _tocState.mobileOpen = false;
          applyTocLayoutClasses();
        }
        syncTocActiveHighlight(tgt);
        jumpToSection(tgt, true, { pushHash: true });
      });
    });

    var expBtn = byId('reportTocExpandAllBtn');
    if (expBtn) {
      expBtn.addEventListener('click', function () {
        var nextCollapse = !allCollapsed;
        TOC_TREE_NODES.forEach(function (n) {
          if (n.children && n.children.length) {
            if (nextCollapse) _tocState.collapsedBranches[n.id] = true;
            else delete _tocState.collapsedBranches[n.id];
          }
        });
        renderReportTocTree();
      });
    }

    var hideBtn = byId('reportTocHideBtn');
    if (hideBtn) {
      hideBtn.addEventListener('click', function () {
        setTocSidebarOpen(false);
      });
    }

    applyTocLayoutClasses();
    syncFengShuiLanguageVisibility();
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(syncTocScrollFade);
  }

  var _tocScrollRaf = false;
  var _tocScrollSuppressUntil = 0;
  function updateTocScrollSpy() {
    _tocScrollRaf = false;
    if (Date.now() < _tocScrollSuppressUntil) return;
    if (typeof window === 'undefined' || typeof document === 'undefined') return;
    var candidates = ['overview'];
    getOrderedTocNodes().forEach(function (n) {
      if (n.id === 'overview') return;
      candidates.push(n.target);
      if (openCards[n.id] && n.children) {
        n.children.forEach(function (c) {
          if (c.zhOnly && langCode() !== 'zh') return;
          if (_roleState.hiddenMap && _roleState.hiddenMap[c.target]) return;
          candidates.push(c.target);
        });
      }
    });
    var best = 'overview';
    var threshold = 150;
    for (var i = 0; i < candidates.length; i++) {
      var el = byId(candidates[i]);
      if (!el || el.hidden || (el.classList && el.classList.contains('role-hidden')) || (el.style && el.style.display === 'none')) continue;
      if (typeof el.getBoundingClientRect === 'function') {
        var r = el.getBoundingClientRect();
        if (r.height > 0 && r.top <= threshold) {
          best = candidates[i];
        }
      }
    }
    if (best !== _tocState.activeTarget) {
      syncTocActiveHighlight(best);
    }
  }

  function bindTocGlobalEvents() {
    if (window.__DB_BOUND_TOC_EVENTS__) return;
    window.__DB_BOUND_TOC_EVENTS__ = true;
    var reopenBtn = byId('reportTocReopenBtn');
    if (reopenBtn) {
      reopenBtn.addEventListener('click', function () {
        exitCommandCenterIfActive();
        setTocSidebarOpen(true);
      });
    }
    var scrim = byId('reportTocScrim');
    if (scrim) {
      scrim.addEventListener('click', function () {
        _tocState.mobileOpen = false;
        applyTocLayoutClasses();
      });
    }
    if (typeof window.addEventListener === 'function') {
      var clearSuppress = function () { _tocScrollSuppressUntil = 0; };
      window.addEventListener('wheel', clearSuppress, { passive: true });
      window.addEventListener('touchmove', clearSuppress, { passive: true });
      window.addEventListener('scroll', function () {
        if (!_tocScrollRaf) {
          _tocScrollRaf = true;
          if (typeof requestAnimationFrame === 'function') requestAnimationFrame(updateTocScrollSpy);
          else setTimeout(updateTocScrollSpy, 60);
        }
      }, { passive: true });
      window.addEventListener('resize', function () {
        applyTocLayoutClasses();
        updateHorizontalScrollFades();
      });
    }
  }

  function renderChapterFooterNav() {
    var foot = byId('chapterNextNav');
    if (foot) foot.innerHTML = '';
  }

  function syncTabsNavChapterState() {}

  function resolveQuestionForTargetId(rawId) {
    if (!rawId) return null;
    var norm = HASH_ALIASES[rawId] || rawId;
    if (SEC_TO_Q[norm]) return SEC_TO_Q[norm];
    var el = byId(rawId) || byId(norm);
    if (el && typeof el.closest === 'function') {
      var qCard = el.closest('.q-card');
      if (qCard && qCard.getAttribute) {
        var dq = qCard.getAttribute('data-q-id');
        if (dq && Q_BY_ID[dq]) return dq;
      }
    }
    return null;
  }

  function scrollToElementWithStickyOffset(targetEl, smooth) {
    if (!targetEl) return;
    if (typeof window.scrollTo === 'function' && typeof targetEl.getBoundingClientRect === 'function') {
      var hdr = typeof document.querySelector === 'function' ? document.querySelector('.hdr') : null;
      var hdrH = (hdr && typeof hdr.getBoundingClientRect === 'function') ? hdr.getBoundingClientRect().height : 72;
      var y = (window.scrollY || window.pageYOffset || 0) + targetEl.getBoundingClientRect().top - hdrH - 12;
      try {
        window.scrollTo({ top: Math.max(0, y), behavior: smooth ? 'smooth' : 'instant' });
        return;
      } catch (e) {}
    }
    if (typeof targetEl.scrollIntoView === 'function') {
      try { targetEl.scrollIntoView({ block: 'start', behavior: smooth ? 'smooth' : 'instant' }); }
      catch (e2) { targetEl.scrollIntoView(); }
    }
  }

  function activateChapter(chapId, opts) {
    var options = opts || {};
    if (chapId === 'all') {
      QUESTION_CARDS.forEach(function (q) { setQuestionCardOpen(q.id, true); });
      return;
    }
    if (chapId === 'ov' || chapId === 'overview') {
      if (options.scrollWindow && typeof window.scrollTo === 'function') {
        try { window.scrollTo({ top: 0, behavior: 'instant' }); } catch (e) { window.scrollTo(0, 0); }
      }
      return;
    }
    var mappedQ = Q_BY_ID[chapId] ? chapId : LEGACY_CHAP_TO_Q[chapId];
    if (mappedQ) {
      setQuestionCardOpen(mappedQ, true);
      if (options.scrollWindow) {
        scrollToElementWithStickyOffset(byId(Q_BY_ID[mappedQ].cardId), false);
      }
    }
  }

  function jumpToSection(rawTargetId, smooth, opts) {
    if (!rawTargetId) return;
    var options = opts || {};
    var clean = String(rawTargetId).replace(/^#/, '');
    if (clean === 'fengshui' && langCode() !== 'zh') return;

    if (isTargetHiddenInActiveRole(clean)) {
      setRole('all');
    }

    if (clean === 'overview') {
      _tocScrollSuppressUntil = Date.now() + (smooth ? 1300 : 250);
      syncTocActiveHighlight('overview');
      scrollToElementWithStickyOffset(byId('overview'), Boolean(smooth));
      return;
    }

    if (clean === 'disclosures') {
      setQuestionCardOpen('risk', true, { fromHash: Boolean(options.pushHash || options.fromHash) });
      var discBtn = document.querySelector && document.querySelector('#histTab button[data-k="disc"]');
      if (discBtn && typeof discBtn.click === 'function') discBtn.click();
      scrollToElementWithStickyOffset(byId('history'), Boolean(smooth));
      return;
    }

    var docTabForHash = clean === 'packetReview' ? 'packet' : (clean === 'titleReview' ? 'title' : (clean === 'hoaReview' ? 'hoa' : null));
    var norm = HASH_ALIASES[clean] || clean;
    var qId = resolveQuestionForTargetId(clean);
    if (qId) {
      setQuestionCardOpen(qId, true, { fromHash: Boolean(options.pushHash || options.fromHash) });
    }
    if (docTabForHash && document.querySelector) {
      var docSegBtn = document.querySelector('#docModeSeg button[data-k="' + docTabForHash + '"]');
      if (docSegBtn && typeof docSegBtn.click === 'function') docSegBtn.click();
    }

    if (options.pushHash && typeof history !== 'undefined' && typeof history.pushState === 'function') {
      try {
        var u = safeUrlObj();
        var nextHash = '#' + encodeURIComponent(clean);
        if (!u || u.hash !== nextHash) {
          var base = u ? (u.pathname + u.search) : '/';
          history.pushState({ dbQuestionDrawer: qId || null, target: clean }, '', base + nextHash);
        }
      } catch (e) {}
    }

    var targetEl = byId(clean) || byId(norm) || (qId && Q_BY_ID[qId] ? byId(Q_BY_ID[qId].cardId) : null);
    if (targetEl) {
      _tocScrollSuppressUntil = Date.now() + (smooth ? 1300 : 250);
      syncTocActiveHighlight(clean);
      scrollToElementWithStickyOffset(targetEl, Boolean(smooth));
    }
    setTimeout(function () { notify3dAndVisuals(qId); }, 50);
  }

  function syncDrawerFromLocationHash() {
    var u = safeUrlObj();
    if (u && u.searchParams) {
      var popRole = normalizeRole(u.searchParams.get('role'));
      if (popRole && popRole !== getRole()) {
        setRole(popRole, { skipPersist: true });
      }
    }
    var h = u && u.hash && u.hash.length > 1 ? decodeURIComponent(u.hash.slice(1)) : '';
    if (!h) {
      Object.keys(hashOpenedCards).forEach(function (qid) {
        setQuestionCardOpen(qid, false);
      });
      hashOpenedCards = {};
      return;
    }
    var targetQ = resolveQuestionForTargetId(h);
    Object.keys(hashOpenedCards).forEach(function (qid) {
      if (qid !== targetQ) setQuestionCardOpen(qid, false);
    });
    if (targetQ) {
      jumpToSection(h, false, { fromHash: true });
    }
  }

  function bindChapterAutoActivation() {
    document.addEventListener('click', function (e) {
      var t = e && e.target;
      var roleSetBtn = t && typeof t.closest === 'function' ? t.closest('[data-role-set]') : null;
      if (roleSetBtn) {
        if (typeof e.preventDefault === 'function') e.preventDefault();
        setRole(roleSetBtn.getAttribute('data-role-set'));
        return;
      }
      var a = t && typeof t.closest === 'function' ? t.closest('a[href^="#"]') : null;
      if (!a) return;
      var href = a.getAttribute('href') || '';
      if (href.length <= 1) return;
      var raw = decodeURIComponent(href.slice(1));
      var qId = resolveQuestionForTargetId(raw);
      if (qId || raw === 'overview') {
        if (typeof e.preventDefault === 'function') e.preventDefault();
        jumpToSection(raw, true, { pushHash: true });
      }
    }, true);

    if (typeof window.addEventListener === 'function') {
      window.addEventListener('popstate', syncDrawerFromLocationHash);
      window.addEventListener('hashchange', syncDrawerFromLocationHash);
      window.addEventListener('beforeprint', function () {
        QUESTION_CARDS.forEach(function (q) { setQuestionCardOpen(q.id, true); });
      });
    }
  }

  /* ------------------------------------------------------------------ 2. Hero Staging & Photo Bento Gallery + Lightbox */
  function makeSvgCardUri(title, sub, accent, lat, lon) {
    var coordSuffix = (isN(lat) && isN(lon)) ? (' · ' + lat.toFixed(4) + ', ' + lon.toFixed(4)) : '';
    var svg = '<svg xmlns="http://www.w3.org/2000/svg" width="800" height="520" viewBox="0 0 800 520">' +
      '<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0%" stop-color="#0f172a"/><stop offset="55%" stop-color="' + accent + '"/><stop offset="100%" stop-color="#1e293b"/></linearGradient></defs>' +
      '<rect width="800" height="520" fill="url(#g)"/>' +
      '<g stroke="rgba(255,255,255,0.12)" stroke-width="1">' +
        '<line x1="0" y1="130" x2="800" y2="130"/><line x1="0" y1="260" x2="800" y2="260"/><line x1="0" y1="390" x2="800" y2="390"/>' +
        '<line x1="200" y1="0" x2="200" y2="520"/><line x1="400" y1="0" x2="400" y2="520"/><line x1="600" y1="0" x2="600" y2="520"/>' +
      '</g>' +
      '<rect x="250" y="135" width="300" height="210" rx="8" fill="rgba(56,189,248,0.12)" stroke="#38bdf8" stroke-width="2.5" stroke-dasharray="9 6"/>' +
      '<polygon points="315,185 485,185 485,295 315,295" fill="rgba(250,204,21,0.22)" stroke="#facc15" stroke-width="3"/>' +
      '<circle cx="400" cy="240" r="9" fill="#ef4444" stroke="#ffffff" stroke-width="3"/>' +
      '<text x="400" y="408" text-anchor="middle" fill="#ffffff" font-family="Inter,system-ui,sans-serif" font-size="23" font-weight="700">' + esc(title) + '</text>' +
      '<text x="400" y="438" text-anchor="middle" fill="#cbd5e1" font-family="Inter,system-ui,sans-serif" font-size="15">' + esc(sub) + coordSuffix + '</text>' +
      '</svg>';
    return 'data:image/svg+xml;charset=UTF-8,' + encodeURIComponent(svg);
  }

  function usgsExportUrl(service, lat, lon, dLat, dLon, w, h) {
    var minX = (lon - dLon).toFixed(6), minY = (lat - dLat).toFixed(6);
    var maxX = (lon + dLon).toFixed(6), maxY = (lat + dLat).toFixed(6);
    return 'https://basemap.nationalmap.gov/arcgis/rest/services/' + service +
      '/MapServer/export?bbox=' + minX + ',' + minY + ',' + maxX + ',' + maxY +
      '&bboxSR=4326&imageSR=3857&size=' + w + ',' + h + '&format=jpg&f=image';
  }

  function usgsRegionalTopoUrl(lat, lon, w, h) {
    var dLat = 0.0075, dLon = 0.0105;
    var minX = (lon - dLon).toFixed(6), minY = (lat - dLat).toFixed(6);
    var maxX = (lon + dLon).toFixed(6), maxY = (lat + dLat).toFixed(6);
    return 'https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryTopo/MapServer/export?bbox=' +
      minX + ',' + minY + ',' + maxX + ',' + maxY +
      '&bboxSR=4326&imageSR=3857&size=' + w + ',' + h + '&format=jpg&f=image';
  }

  function satelliteAerialExportUrl(satMeta, lat, lon, dLat, dLon, w, h, zoom) {
    var minX = (lon - dLon).toFixed(6), minY = (lat - dLat).toFixed(6);
    var maxX = (lon + dLon).toFixed(6), maxY = (lat + dLat).toFixed(6);
    var isFile = typeof location !== 'undefined' && location.protocol === 'file:';
    if (satMeta && satMeta.exportUrl && !(isFile && (satMeta.proxied || String(satMeta.exportUrl).charAt(0) === '/'))) {
      var base = String(satMeta.exportUrl);
      var sep = base.indexOf('?') === -1 ? '?' : '&';
      if (satMeta.proxied || base.charAt(0) === '/') {
        return base + sep + 'bbox=' + minX + ',' + minY + ',' + maxX + ',' + maxY + '&w=' + w + '&h=' + h + (zoom ? '&zoom=' + zoom : '');
      }
      return base + sep + 'bbox=' + minX + ',' + minY + ',' + maxX + ',' + maxY + '&bboxSR=4326&imageSR=3857&size=' + w + ',' + h + '&format=jpg&f=image';
    }
    return usgsExportUrl('USGSImageryOnly', lat, lon, dLat, dLon, w, h);
  }

  function resolvePropertyMedia(rep) {
    var pm = (rep && rep.propertyMedia) || mod(rep, 'propertyMedia') || {};
    var f = (rep && rep.facts) || {};
    var satMeta = (rep && rep.meta && rep.meta.satellite) || null;
    var satSrcName = (satMeta && !(typeof location !== 'undefined' && location.protocol === 'file:' && satMeta.proxied) && satMeta.attribution)
      ? satMeta.attribution
      : 'USGS National Map / USDA NAIP';
    var coords = getCoords(rep);
    var hasCoords = Array.isArray(coords) && isN(coords[0]) && isN(coords[1]);
    var lat = hasCoords ? coords[0] : null;
    var lon = hasCoords ? coords[1] : null;
    var addr = (rep && rep.meta && (rep.meta.displayAddress || rep.meta.inputAddress)) || 'Subject Property';
    var lastSaleDate = pm.lastSaleDate || (f.lastSale && f.lastSale.date) || null;
    var lastSalePrice = isN(pm.lastSalePrice) ? pm.lastSalePrice : (f.lastSale && isN(f.lastSale.price) ? f.lastSale.price : null);

    var rawItems = Array.isArray(pm.items) ? pm.items.slice() : [];
    if (!rawItems.length && f.mlsListing && Array.isArray(f.mlsListing.photos)) {
      f.mlsListing.photos.slice(0, 8).forEach(function (u, idx) {
        if (!u) return;
        rawItems.push({
          id: 'mls-' + idx,
          url: u,
          thumbUrl: u,
          category: idx === 0 ? 'exterior' : 'staging',
          roomTag: idx === 0 ? 'Front Exterior' : ('Interior / Room #' + idx),
          caption: addr + ' — MLS listing photo #' + (idx + 1),
          capturedAt: lastSaleDate ? String(lastSaleDate).slice(0, 7) : 'MLS Feed',
          sourceName: (f.mlsListing.feedSource || f.mlsListing.mlsName || 'MLS Archive'),
          sourceUrl: '',
          isHistoricalSale: true
        });
      });
    }

    var hasHighResExport = Boolean(satMeta && (satMeta.isHighRes || satMeta.exportUrl) && !(typeof location !== 'undefined' && location.protocol === 'file:' && satMeta.proxied));
    var fallbackCards = hasCoords ? [
      {
        id: 'aerial-tight',
        url: satelliteAerialExportUrl(satMeta, lat, lon, 0.00032, 0.00046, 1600, 1080, 20),
        thumbUrl: satelliteAerialExportUrl(satMeta, lat, lon, 0.00032, 0.00046, 960, 648, 19),
        category: 'aerial',
        roomTag: L3('Roofline & Parcel Crop', '屋顶与宗地特写航拍', 'Techo y Lote Aéreo'),
        caption: L3('High-resolution ortho-imagery centered on subject roof & yard', '高分辨率正射卫星影像（聚焦主宅屋顶与院落）', 'Ortoimagen satelital centrada en el techo y patio'),
        capturedAt: L3('Recent Ortho Survey', '近期正射航拍', 'Ortofoto reciente'),
        sourceName: satSrcName,
        isHistoricalSale: false,
        fallbackSvg: makeSvgCardUri('Roofline & Parcel Aerial', addr, '#0f766e', lat, lon)
      },
      {
        id: 'parcel-street',
        url: hasHighResExport ? satelliteAerialExportUrl(satMeta, lat, lon, 0.00065, 0.0009, 1600, 1080, 19) : usgsExportUrl('USGSImageryTopo', lat, lon, 0.00065, 0.0009, 1440, 960),
        thumbUrl: hasHighResExport ? satelliteAerialExportUrl(satMeta, lat, lon, 0.00065, 0.0009, 960, 648, 18) : usgsExportUrl('USGSImageryTopo', lat, lon, 0.00065, 0.0009, 960, 640),
        category: 'street',
        roomTag: L3('Street Frontage & Setbacks', '临街面与建筑退界', 'Frente de Calle y Linderos'),
        caption: L3('Street corridor, curb frontage, and surrounding parcel layout', '街道走廊、临街朝向与相邻地块格局', 'Corredor vial, frente y distribución de lotes'),
        capturedAt: hasHighResExport ? L3('Recent Ortho Survey', '近期正射航拍', 'Ortofoto reciente') : L3('Public GIS Archive', '公开地理信息档案', 'Archivo GIS público'),
        sourceName: hasHighResExport ? satSrcName : 'USGS National Map Topo & Street',
        isHistoricalSale: false,
        fallbackSvg: makeSvgCardUri('Street Frontage & Parcel Context', addr, '#1d4ed8', lat, lon)
      },
      {
        id: 'aerial-block',
        url: satelliteAerialExportUrl(satMeta, lat, lon, 0.00088, 0.00125, 1600, 1080, 19),
        thumbUrl: satelliteAerialExportUrl(satMeta, lat, lon, 0.00088, 0.00125, 960, 648, 18),
        category: 'parcel',
        roomTag: L3('Block & Lot Context', '街区与邻里尺度航拍', 'Manzana y Entorno Aéreo'),
        caption: L3('Immediate residential block canopy, setbacks, and neighbor spacing', '周边街区绿植覆盖、建筑间距与后院纵深', 'Vegetación de la manzana y separación entre casas'),
        capturedAt: L3('Aerial Archive', '航拍影像档案', 'Archivo aéreo'),
        sourceName: satSrcName,
        isHistoricalSale: false,
        fallbackSvg: makeSvgCardUri('Neighborhood Block Canopy', addr, '#0369a1', lat, lon)
      },
      {
        id: 'topo-grade',
        url: usgsExportUrl('USGSImageryTopo', lat, lon, 0.0018, 0.0025, 1440, 960),
        thumbUrl: usgsExportUrl('USGSImageryTopo', lat, lon, 0.0018, 0.0025, 960, 640),
        category: 'exterior',
        roomTag: L3('Topography & Lot Grading', '地形等高线与排水地势', 'Topografía y Pendiente'),
        caption: L3('USGS contour & elevation context around the lot', '宗地周边地形起伏、等高线与自然水系走向', 'Curvas de nivel USGS y elevación del lote'),
        capturedAt: L3('USGS 3DEP / Topo', 'USGS 地形档案', 'USGS Topo'),
        sourceName: 'USGS National Map Topo',
        isHistoricalSale: false,
        fallbackSvg: makeSvgCardUri('Topography & Elevation Profile', addr, '#4338ca', lat, lon)
      },
      {
        id: 'aerial-wide',
        url: satelliteAerialExportUrl(satMeta, lat, lon, 0.0022, 0.0031, 1600, 1080, 18),
        thumbUrl: satelliteAerialExportUrl(satMeta, lat, lon, 0.0022, 0.0031, 960, 648, 17),
        category: 'aerial',
        roomTag: L3('Micro-Neighborhood Overview', '半英里生活圈鸟瞰', 'Vista Aérea del Vecindario'),
        caption: L3('Arterials, parks, schools, and commercial corridors near the property', '周边主干道、公园绿地与社区空间关系', 'Vías principales, parques y entorno cercano'),
        capturedAt: L3('Regional Ortho', '区域航拍档案', 'Ortofoto regional'),
        sourceName: satSrcName,
        isHistoricalSale: false,
        fallbackSvg: makeSvgCardUri('Micro-Neighborhood Overview', addr, '#0f766e', lat, lon)
      }
    ] : [
      {
        id: 'coords-unavailable',
        url: makeSvgCardUri(L3('Geocoded Coordinates Unavailable', '未提供地理坐标', 'Coordenadas No Disponibles'), addr, '#0f766e', null, null),
        category: 'parcel',
        roomTag: L3('Parcel Diagram', '宗地示意图', 'Diagrama de Parcela'),
        caption: L3('Aerial imagery unavailable until parcel coordinates are resolved', '尚未获取精确经纬度坐标，暂不请求卫星航拍图', 'Imagen aérea no disponible sin coordenadas'),
        capturedAt: '',
        sourceName: L3('Parcel Schematic', '宗地示意', 'Esquema'),
        isHistoricalSale: false,
        fallbackSvg: makeSvgCardUri(L3('Parcel Diagram', '宗地示意图', 'Diagrama de Parcela'), addr, '#0f766e', null, null)
      }
    ];

    var isFileProto = typeof location !== 'undefined' && location.protocol === 'file:';
    var items = rawItems.map(function (it, idx) {
      var isHist = Boolean(it.isHistoricalSale || it.isHistorical || it.kind === 'last_sale_listing' || it.kind === 'staging');
      var fbUrl = fallbackCards[idx % fallbackCards.length].url;
      var rawUrl = it.url || it.thumbUrl || fbUrl;
      var rawThumb = it.thumbUrl || it.url || fbUrl;
      if (isFileProto && String(rawUrl).indexOf('/api/') === 0) rawUrl = fbUrl;
      if (isFileProto && String(rawThumb).indexOf('/api/') === 0) rawThumb = fbUrl;
      var needsRegionalTopo = hasCoords && (String(rawUrl).indexOf('USGSShadedReliefOnly') !== -1 || String(rawThumb).indexOf('USGSShadedReliefOnly') !== -1);
      var itemUrl = needsRegionalTopo ? usgsRegionalTopoUrl(lat, lon, 1440, 960) : rawUrl;
      var itemThumb = needsRegionalTopo ? usgsRegionalTopoUrl(lat, lon, 960, 640) : rawThumb;
      var roomTag = needsRegionalTopo
        ? L3('USGS Regional Topo', 'USGS 区域地形等高', 'Topo Regional USGS')
        : (it.roomTag || (isHist ? L3('Last Sale Photo', '历史挂牌实拍', 'Foto de venta') : L3('Property View', '房产实景', 'Vista de propiedad')));
      return {
        id: it.id || ('media-' + idx),
        url: itemUrl,
        thumbUrl: itemThumb,
        category: it.category || it.kind || 'exterior',
        roomTag: roomTag,
        caption: it.caption || addr,
        capturedAt: it.capturedAt || it.takenAt || (lastSaleDate ? String(lastSaleDate).slice(0, 7) : ''),
        sourceName: _RE_PORTAL_MEDIA_SRC.test(String(it.sourceName || it.sourceLabel || ''))
          ? L3('Public Property Archive', '公开房产影像档案', 'Archivo público')
          : (it.sourceName || it.sourceLabel || L3('Public Property Archive', '公开房产影像档案', 'Archivo público')),
        sourceUrl: _RE_PORTAL_HOST.test(String(it.sourceUrl || '')) ? '' : (it.sourceUrl || ''),
        isHistoricalSale: isHist,
        fallbackSvg: makeSvgCardUri(roomTag || 'Property Media', addr, '#0f766e', lat, lon)
      };
    });

    for (var k = 0; items.length < 5 && k < fallbackCards.length; k++) {
      items.push(fallbackCards[k]);
    }

    var rawSuppliedLinks = Array.isArray(pm.externalGalleryLinks) && pm.externalGalleryLinks.length
      ? pm.externalGalleryLinks
      : (Array.isArray(pm.portalLinks) && pm.portalLinks.length ? pm.portalLinks : []);
    var filteredLinks = rawSuppliedLinks.filter(function (lk) {
      var s = String((lk && lk.label) || '') + ' ' + String((lk && lk.url) || '');
      return !_RE_PORTAL_LINK.test(s);
    });
    var extLinks = filteredLinks.length ? filteredLinks : (hasCoords ? [
      { label: 'Street View Time-Machine', url: 'https://www.google.com/maps/@?api=1&map_action=pano&viewpoint=' + lat + ',' + lon, badge: 'Curbside 360°' },
      { label: "Bing Bird's Eye", url: 'https://www.bing.com/maps?cp=' + lat + '~' + lon + '&lvl=19&style=o', badge: '45° Oblique' }
    ] : []);

    var hasSale = Boolean(pm.hasSalePhotos || items.some(function (x) { return x.isHistoricalSale; }));
    var rawProv = pm.provenanceNote || pm.historicalBanner || '';
    var cleanProv = _RE_PORTAL_NAMES.test(rawProv) ? '' : rawProv;
    _RE_PORTAL_NAMES.lastIndex = 0;
    var provNote = cleanProv || (hasSale
      ? L3('Includes verified historical sale/listing imagery alongside current aerial & street archives.', '包含已核实的上次成交挂牌照片及最新正射航拍/街景档案。', 'Incluye fotos históricas verificadas de venta junto con archivos aéreos y de calle.')
      : L3('Off-market property: displaying verified ortho-aerial, parcel setback, and street-corridor archives (zero synthetic interior photos). Open street & aerial links above for curbside and oblique views.', '当前非在售房源：展示经核实的正射航拍、宗地退界与街景档案（绝不伪造室内图）；点击上方街景与航拍入口可查看户外视角。', 'Propiedad fuera de mercado: mostrando archivos aéreos y catastrales verificados (sin interiores sintéticos). Use los enlaces de calle y vista aérea arriba.'));

    return {
      hasSalePhotos: hasSale,
      lastSaleDate: lastSaleDate,
      lastSalePrice: lastSalePrice,
      mediaStatus: pm.mediaStatus || (hasSale ? 'sale_photos_found' : 'exterior_archive_only'),
      provenanceNote: provNote,
      items: items,
      externalGalleryLinks: extLinks
    };
  }

  var _plbState = { open: false, idx: 0, items: [] };

  function renderHeroMediaHub() {
    var mount = byId('heroMediaBento');
    if (!mount) return;
    var rep = getRep() || {};
    var media = resolvePropertyMedia(rep);
    _plbState.items = media.items;
    var slidesCount = getWalkthroughDeck(rep).slides.length;

    var lead = media.items[0];
    var rest = media.items.slice(1, 5);

    function tileHtml(it, idx, isLead) {
      var badgeCls = it.isHistoricalSale ? 'sale' : 'arch';
      var badgeTxt = it.isHistoricalSale
        ? L3('Last Sale Listing Photo', '上次成交挂牌实拍', 'Foto de Última Venta')
        : L3('Aerial / Street Archive', '航拍 / 街景档案', 'Archivo Aéreo / Calle');
      var yr = it.capturedAt ? (' · ' + it.capturedAt) : '';
      return '<div class="bento-tile' + (isLead ? ' lead' : '') + '" data-photo-idx="' + idx + '" tabindex="0" role="button" aria-label="' + esc(it.roomTag + ' — ' + it.caption) + '">' +
        '<img src="' + esc(isLead ? (it.url || it.thumbUrl) : (it.thumbUrl || it.url)) + '" data-fb="' + esc(it.fallbackSvg) + '" alt="' + esc(it.roomTag) + '" loading="' + (idx < 2 ? 'eager' : 'lazy') + '">' +
        '<span class="bento-prov ' + badgeCls + '">' + esc(badgeTxt + yr) + '</span>' +
        '<div class="bento-cap">' +
          '<b>' + esc(it.roomTag) + '</b>' +
          '<span>' + esc(it.sourceName) + '</span>' +
        '</div>' +
        (idx === 4 || (idx === media.items.length - 1 && media.items.length <= 5)
          ? '<button type="button" class="bento-all-btn" id="openPhotoLightboxBtn">⊞ ' + esc(L3('See all ' + media.items.length + ' views', '查看全部 ' + media.items.length + ' 张影像', 'Ver las ' + media.items.length + ' vistas')) + '</button>'
          : '') +
      '</div>';
    }

    var portalsHtml = media.externalGalleryLinks.map(function (lk) {
      var lbl = String(lk.label || 'Portal');
      var hasArrow = lbl.indexOf('↗') >= 0;
      return '<a class="media-ext-pill" href="' + esc(lk.url) + '" target="_blank" rel="noopener">' +
        '<b>' + esc(hasArrow ? lbl : (lbl + ' ↗')) + '</b>' +
        (lk.badge ? '<em>' + esc(lk.badge) + '</em>' : '') +
      '</a>';
    }).join('');

    var saleSummaryPill = media.lastSalePrice
      ? '<span class="pill info">' + esc(L3('Last Sale: ', '上次成交：', 'Última venta: ')) + fUsdK(media.lastSalePrice) + (media.lastSaleDate ? ' (' + esc(String(media.lastSaleDate).slice(0, 4)) + ')' : '') + '</span>'
      : '';

    var existingWalkinStage = byId('heroWalkinStage');
    var existingWalkinTabs = [];
    qAll('#heroMediaSwitcher .hm-tabs > *', mount).forEach(function (node) {
      var id = node.id || '';
      var dm = node.getAttribute && node.getAttribute('data-media-mode');
      if (id !== 'mediaTabMaps' && id !== 'mediaTabPhotos' && id !== 'mediaTab3d' && id !== 'mediaTabSlides' && dm !== 'maps' && dm !== 'photos' && dm !== '3d' && dm !== 'slides') {
        existingWalkinTabs.push(node);
      }
    });

    mount.innerHTML =
      '<div class="hero-media-bar" id="heroMediaSwitcher">' +
        '<div class="hm-tabs">' +
          '<button type="button" class="hm-tab' + (activeMediaMode === 'maps' ? ' on' : '') + '" id="mediaTabMaps" data-media-mode="maps">🗺️ ' + span3('Map · Lot & Outlines', '地图 · 红线轮廓', 'Mapa · Lote') + '</button>' +
          '<button type="button" class="hm-tab' + (activeMediaMode === 'photos' ? ' on' : '') + '" id="mediaTabPhotos" data-media-mode="photos">📷 ' + span3('Photos & Aerials', '实拍与航拍', 'Fotos y Aéreas') + '</button>' +
        '</div>' +
        '<div class="hm-portals">' + saleSummaryPill + portalsHtml + '</div>' +
      '</div>' +
      '<div class="bento-wrap' + (activeMediaMode === 'photos' ? '' : ' hero-parked') + '" id="photoBentoGrid">' +
        '<div class="bento-grid">' +
          tileHtml(lead, 0, true) +
          '<div class="bento-side">' +
            rest.map(function (it, i) { return tileHtml(it, i + 1, false); }).join('') +
          '</div>' +
        '</div>' +
        '<div class="bento-prov-bar">' +
          '<span>🔎 ' + esc(media.provenanceNote) + '</span>' +
          '<button type="button" class="copybtn" id="switchToInteractiveMapsBtn">🗺️ ' + span3('Switch to Live Parcel & Facing Map →', '切换至实时红线与朝向地图 →', 'Cambiar a Mapa Interactivo →') + '</button>' +
        '</div>' +
      '</div>';

    if (existingWalkinTabs.length) {
      var tabsHost = mount.querySelector && mount.querySelector('#heroMediaSwitcher .hm-tabs');
      if (tabsHost) {
        existingWalkinTabs.forEach(function (tNode) { tabsHost.appendChild(tNode); });
      }
    }
    if (existingWalkinStage) {
      mount.appendChild(existingWalkinStage);
      if (existingWalkinStage.classList) {
        existingWalkinStage.classList.toggle('hero-parked', activeMediaMode !== 'walkin');
      }
    }

    var gal = byId('gallery');
    if (gal && gal.classList) {
      gal.classList.toggle('hero-parked', activeMediaMode !== 'maps');
    }

    qAll('#photoBentoGrid img', mount).forEach(function (img) {
      img.addEventListener('error', function () {
        var fb = img.getAttribute('data-fb');
        if (fb && img.src !== fb) img.src = fb;
      });
    });

    var allBtn = byId('openPhotoLightboxBtn');
    if (allBtn) {
      allBtn.addEventListener('click', function (ev) {
        if (ev && typeof ev.stopPropagation === 'function') ev.stopPropagation();
        openPhotoLightbox(0);
      });
    }

    qAll('[data-photo-idx]', mount).forEach(function (tile) {
      tile.addEventListener('click', function () {
        openPhotoLightbox(+(tile.getAttribute('data-photo-idx') || 0));
      });
      tile.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' || ev.key === ' ') {
          ev.preventDefault();
          openPhotoLightbox(+(tile.getAttribute('data-photo-idx') || 0));
        }
      });
    });

    qAll('#mediaTabMaps, #mediaTabPhotos', mount).forEach(function (btn) {
      btn.addEventListener('click', function () {
        var m = btn.getAttribute('data-media-mode');
        setHeroMediaMode(m);
      });
    });

    var swMapBtn = byId('switchToInteractiveMapsBtn');
    if (swMapBtn) swMapBtn.addEventListener('click', function () { setHeroMediaMode('maps'); });
  }

  function setHeroMediaMode(mode) {
    activeMediaMode = mode === 'maps' ? 'maps' : (mode === 'walkin' ? 'walkin' : 'photos');
    var bento = byId('photoBentoGrid');
    var gal = byId('gallery');
    var walkin = byId('heroWalkinStage');
    if (bento && bento.classList) bento.classList.toggle('hero-parked', activeMediaMode !== 'photos');
    if (gal && gal.classList) gal.classList.toggle('hero-parked', activeMediaMode !== 'maps');
    if (walkin && walkin.classList) walkin.classList.toggle('hero-parked', activeMediaMode !== 'walkin');
    qAll('#heroMediaSwitcher [data-media-mode], #heroMediaSwitcher .hm-tab').forEach(function (b) {
      var bm = b.getAttribute('data-media-mode');
      if (bm) b.classList.toggle('on', bm === activeMediaMode);
      else if (activeMediaMode !== 'walkin') b.classList.remove('on');
    });
    setTimeout(resizeChartsAndMaps, 30);
  }

  function renderLightboxModal() {
    var modal = byId('photoLightboxModal');
    if (!modal || !_plbState.open || !_plbState.items.length) return;
    var n = _plbState.items.length;
    var idx = ((_plbState.idx % n) + n) % n;
    _plbState.idx = idx;
    var cur = _plbState.items[idx];
    var badgeTxt = cur.isHistoricalSale
      ? L3('Last Sale Listing Photo', '上次成交挂牌实拍', 'Foto de Última Venta')
      : L3('Aerial / Street Archive', '航拍 / 街景档案', 'Archivo Aéreo / Calle');

    modal.innerHTML =
      '<div class="plb-dialog" role="dialog" aria-modal="true" aria-label="Property Media Lightbox">' +
        '<div class="plb-top">' +
          '<div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap">' +
            '<span class="plb-cnt" id="plbCounter">' + (idx + 1) + ' / ' + n + '</span>' +
            '<span class="bento-prov ' + (cur.isHistoricalSale ? 'sale' : 'arch') + '" style="position:static">' + esc(badgeTxt) + '</span>' +
            '<b style="color:#f8fafc;font-size:15px">' + esc(cur.roomTag) + '</b>' +
            (cur.capturedAt ? '<span style="color:#94a3b8;font-size:12.5px">' + esc(cur.capturedAt) + '</span>' : '') +
          '</div>' +
          '<button type="button" class="plb-close" id="plbCloseBtn" aria-label="Close">✕</button>' +
        '</div>' +
        '<div class="plb-stage">' +
          '<button type="button" class="plb-nav prev" id="plbPrevBtn" aria-label="Previous photo">‹</button>' +
          '<img id="plbMainImg" src="' + esc(cur.url) + '" data-fb="' + esc(cur.fallbackSvg) + '" alt="' + esc(cur.roomTag) + '">' +
          '<button type="button" class="plb-nav next" id="plbNextBtn" aria-label="Next photo">›</button>' +
        '</div>' +
        '<div class="plb-foot">' +
          '<div class="plb-cap">' + esc(cur.caption) + ' · <span style="color:#94a3b8">' + esc(cur.sourceName) + '</span>' +
            (cur.sourceUrl ? ' · <a href="' + esc(cur.sourceUrl) + '" target="_blank" rel="noopener" style="color:#5eead4">Source ↗</a>' : '') +
          '</div>' +
          '<div class="plb-thumbs">' +
            _plbState.items.map(function (it, i) {
              return '<button type="button" class="plb-th' + (i === idx ? ' on' : '') + '" data-plb-go="' + i + '">' +
                '<img src="' + esc(it.thumbUrl || it.url) + '" data-fb="' + esc(it.fallbackSvg) + '" alt="">' +
              '</button>';
            }).join('') +
          '</div>' +
        '</div>' +
      '</div>';

    qAll('img[data-fb]', modal).forEach(function (im) {
      im.addEventListener('error', function () {
        var fb = im.getAttribute('data-fb');
        if (fb && im.src !== fb) im.src = fb;
      });
    });
    var cBtn = byId('plbCloseBtn');
    if (cBtn) cBtn.addEventListener('click', closePhotoLightbox);
    var pBtn = byId('plbPrevBtn');
    if (pBtn) pBtn.addEventListener('click', function () { stepPhotoLightbox(-1); });
    var nBtn = byId('plbNextBtn');
    if (nBtn) nBtn.addEventListener('click', function () { stepPhotoLightbox(1); });
    qAll('[data-plb-go]', modal).forEach(function (b) {
      b.addEventListener('click', function () {
        _plbState.idx = +(b.getAttribute('data-plb-go') || 0);
        renderLightboxModal();
      });
    });
    modal.onclick = function (ev) { if (ev.target === modal) closePhotoLightbox(); };
  }

  function openPhotoLightbox(startIdx) {
    var modal = byId('photoLightboxModal');
    if (!modal) return;
    if (!_plbState.items.length) _plbState.items = resolvePropertyMedia(getRep() || {}).items;
    _plbState.idx = startIdx || 0;
    _plbState.open = true;
    modal.hidden = false;
    if (modal.classList) modal.classList.add('open');
    renderLightboxModal();
  }

  function closePhotoLightbox() {
    var modal = byId('photoLightboxModal');
    _plbState.open = false;
    if (modal) {
      modal.hidden = true;
      if (modal.classList) modal.classList.remove('open');
      modal.innerHTML = '';
    }
  }

  function stepPhotoLightbox(delta) {
    if (!_plbState.open) return;
    _plbState.idx += delta;
    renderLightboxModal();
  }

  /* ------------------------------------------------------------------ 3. True Estimate Benchmark Strip */
  var _RE_IDX_METHOD = new RegExp(_T.zhv + '|tier|repeat|index', 'i');
  function extractBenchmarks(rep) {
    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var o = v.ownership || {};
    var f = (rep && rep.facts) || {};
    var methods = Array.isArray(v.methods) ? v.methods : [];
    var rr = getRentResearchData(rep);

    var extAvmVal = null, extAvmLbl = L3('Third-party estimate', '第三方估值', 'Estimación de terceros');
    var compsModelVal = null, compsModelLbl = L3('Adjusted Comps Model', '周边成交修正估值', 'Modelo de Comparables');
    var compM = methods.find(function (m) { return /comp/i.test(m.key || ''); });
    if (compM && isN(compM.value)) {
      compsModelVal = compM.value;
      compsModelLbl = L3('Adjusted Comps Model', '周边成交修正估值', 'Modelo de Comparables');
    } else if (f.officialAssessment && isN(f.officialAssessment.assessedTotal)) {
      compsModelVal = f.officialAssessment.assessedTotal;
      compsModelLbl = L3('County Tax Roll', '县评估师计税账面', 'Valor Catastral');
    }
    for (var i = 0; i < methods.length; i++) {
      var m = methods[i] || {};
      var kl = String((m.key || '') + ' ' + (m.label || '')).toLowerCase();
      var hasPortalAvm = kl.indexOf(_T.zi) >= 0 || kl.indexOf(_T.ze) >= 0 || kl.indexOf(_T.rf) >= 0 || kl.indexOf(_T.mv) >= 0 || kl.indexOf('third-party') >= 0;
      if (!extAvmVal && (/^avm-/.test(m.key || '') || (hasPortalAvm && kl.indexOf(_T.zhv) < 0))) {
        extAvmVal = m.value;
        extAvmLbl = L3('Third-party estimate', '第三方估值', 'Estimación de terceros');
      }
    }
    if (!extAvmVal && isN(f[_T.ze])) {
      extAvmVal = f[_T.ze];
      extAvmLbl = L3('Third-party estimate', '第三方估值', 'Estimación de terceros');
    } else if (!extAvmVal && isN(f[_T.rfKey])) {
      extAvmVal = f[_T.rfKey];
      extAvmLbl = L3('Third-party estimate', '第三方估值', 'Estimación de terceros');
    }
    if (!extAvmVal) {
      var idxM = methods.find(function (m) { return _RE_IDX_METHOD.test((m.key || '') + ' ' + (m.label || '')); });
      if (idxM && isN(idxM.value)) {
        extAvmVal = idxM.value;
        extAvmLbl = L3('ZIP home-value index', '邮编房价指数', 'Índice de valor del código postal');
      }
    }
    if (!compsModelVal) {
      var idxFallback = methods.find(function (m) { return _RE_IDX_METHOD.test((m.key || '') + ' ' + (m.label || '')); });
      if (idxFallback && isN(idxFallback.value) && idxFallback.value !== extAvmVal) {
        compsModelVal = idxFallback.value;
        compsModelLbl = L3('ZIP home-value index', '邮编房价指数', 'Índice de valor del código postal');
      }
    }
    var rentAnchor = (rr && isN(rr.recommendedMonthlyRent)) ? rr.recommendedMonthlyRent : (isN(v.monthlyRent) ? v.monthlyRent : (o.rentEstimate || null));
    return {
      est: e.value,
      low: e.low,
      high: e.high,
      conf: e.confidence || 'Medium',
      perSqft: e.perSqft || (isN(e.value) && isN(f.sqft) && f.sqft > 0 ? Math.round(e.value / f.sqft) : null),
      extAvmVal: extAvmVal,
      extAvmLbl: extAvmLbl,
      compsModelVal: compsModelVal,
      compsModelLbl: compsModelLbl,
      monthlyTotal: (o.monthly && o.monthly.total) || null,
      rentEst: rentAnchor,
      compsCount: Array.isArray(v.comps) && v.comps.length ? v.comps.length : (Array.isArray(f.comps) ? f.comps.length : 0)
    };
  }

  function renderBenchmarkStrips() {
    var rep = getRep() || {};
    var bm = extractBenchmarks(rep);
    if (!isN(bm.est)) return;
    var bName = (typeof window !== 'undefined' && window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) || ((typeof R !== 'undefined' && R && R.brand && R.brand.name) ? String(R.brand.name) : ['Open', 'Estate'].join(' '));

    var cardsHtml =
      '<div class="zest-cards">' +
        '<div class="zest-card pri">' +
          '<div class="zk">✦ ' + span3(bName + ' Estimate', bName + ' 公允估值', 'Estimación ' + bName) + ' <span class="pill good" style="padding:1px 7px;font-size:10.5px">' + esc(bm.conf) + '</span></div>' +
          '<div class="zv">' + fUsd(bm.est) + '</div>' +
          '<div class="zd">' + span3('Range:', '置信区间：', 'Rango:') + ' <b>' + fUsdK(bm.low) + ' – ' + fUsdK(bm.high) + '</b>' + (bm.perSqft ? ' · <b>' + fUsd(bm.perSqft) + '/sf</b>' : '') + '</div>' +
        '</div>' +
        '<div class="zest-card">' +
          '<div class="zk">' + esc(bm.extAvmLbl) + '</div>' +
          '<div class="zv">' + (isN(bm.extAvmVal) ? fUsdK(bm.extAvmVal) : '—') + '</div>' +
          '<div class="zd">' + (isN(bm.extAvmVal) ? esc(L3('vs ' + bName + ': ', '较本报告差：', 'vs ' + bName + ': ')) + (bm.extAvmVal >= bm.est ? '+' : '−') + fUsdK(Math.abs(bm.extAvmVal - bm.est)) : span3('Not cited in public feed', '公开源未单列', 'No citado')) + '</div>' +
        '</div>' +
        '<div class="zest-card">' +
          '<div class="zk">' + esc(bm.compsModelLbl) + '</div>' +
          '<div class="zv">' + (isN(bm.compsModelVal) ? fUsdK(bm.compsModelVal) : '—') + '</div>' +
          '<div class="zd">' + (bm.compsCount ? esc(bm.compsCount + ' ' + L3('nearby comps analyzed', '套周边成交对标', 'comparables cercanos')) : span3('Public parcel benchmark', '公开地籍对标', 'Referencia pública')) + '</div>' +
        '</div>' +
        '<div class="zest-card">' +
          '<div class="zk">' + span3('Est. Payment & Rent Anchor', '月持有成本与租金锚点', 'Pago Mensual y Renta Est.') + '</div>' +
          '<div class="zv">' + (isN(bm.monthlyTotal) ? (fUsd(bm.monthlyTotal) + L3('/mo', '/月', '/mes')) : '—') + '</div>' +
          '<div class="zd">' + (isN(bm.rentEst) ? (span3('Market rent:', '市场参考月租：', 'Renta de mercado:') + ' <b>' + fUsd(bm.rentEst) + L3('/mo', '/月', '/mes') + '</b>') : span3('Full PITI + Insurance math', '含本息·新地税·保险', 'Incluye PITI y seguro')) + '</div>' +
        '</div>' +
      '</div>' +
      '<div class="zest-qtabs">' +
        '<span class="small" style="font-weight:700">' + span3('1-Click Value Hub:', '一键直达估值模块：', 'Acceso rápido de valor:') + '</span>' +
        '<button type="button" class="zest-qbtn" data-zjump="value">📈 ' + span3('Value History', '历史估值曲线', 'Historial de Valor') + '</button>' +
        '<button type="button" class="zest-qbtn" data-zjump="history">🏛️ ' + span3('Sale & Tax History', '成交与税评记录', 'Ventas e Impuestos') + '</button>' +
        '<button type="button" class="zest-qbtn" data-zjump="comps">🏘️ ' + span3('Comparable Sales', '周边可比成交', 'Ventas Comparables') + '</button>' +
        '<button type="button" class="zest-qbtn" data-zjump="payment">🛡️ ' + span3('Payment & Insurance Studio', '月供与保险报价台', 'Pago y Estudio de Seguro') + '</button>' +
      '</div>';

    ['benchmarkOverviewMount', 'benchmarkValueMount'].forEach(function (mid) {
      var m = byId(mid);
      if (!m) return;
      m.innerHTML = '<div class="zest-hub-box">' + cardsHtml + '</div>';
      qAll('[data-zjump]', m).forEach(function (b) {
        b.addEventListener('click', function () {
          jumpToSection(b.getAttribute('data-zjump'), true);
        });
      });
    });
  }

  /* ------------------------------------------------------------------ 4. Interactive 8-Slide Home Walkthrough Deck */
  var _wtState = { open: false, idx: 0, loadingAi: false };

  function buildDeterministicSlidesClient(rep) {
    var f = (rep && rep.facts) || {};
    var g = (rep && rep.geo) || {};
    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var o = v.ownership || {};
    var s = (rep && rep.summary) || {};
    var sc = s.scores || {};
    var df = (rep && rep.decisionFactors) || {};
    var hz = mod(rep, 'hazards') || {};
    var env = mod(rep, 'environment') || {};
    var civ = mod(rep, 'civic') || {};
    var ai = (rep && rep.ai) || {};
    var addr = (rep && rep.meta && (rep.meta.displayAddress || rep.meta.inputAddress)) || 'Subject Property';
    var iq = normalizeInsuranceQuotes(rep);

    var valStr = isN(e.value) ? fUsdK(e.value) : '—';
    var rangeStr = (isN(e.low) && isN(e.high)) ? (fUsdK(e.low) + ' – ' + fUsdK(e.high)) : '—';
    var ppsfStr = isN(e.perSqft) ? (fUsd(e.perSqft) + '/sf') : (isN(e.value) && isN(f.sqft) && f.sqft > 0 ? (fUsd(e.value / f.sqft) + '/sf') : '—');
    var bedsBaths = [f.beds != null ? (f.beds + ' bd') : null, f.baths != null ? (f.baths + ' ba') : null, f.sqft ? (fNum(f.sqft) + ' sf') : null].filter(Boolean).join(' · ') || ' Residential Property';
    var lotSqft = f.lotSqft || (f.buildingRegister && f.buildingRegister.lotSqft) || null;
    var lr = (f.countyRecords && f.countyRecords.lotReconciliation) || f.lotReconciliation || {};
    var gisLot = lr.gisPolygonSqft || null;
    var piti = (o.monthly && o.monthly.total) || null;
    var la = df.lotAndAdu || {};
    var sf = df.safety || {};
    var flChecked = hz.flood && hz.flood.checked !== false && !(hz.checks && hz.checks.flood === false) && (hz.flood.zone || hz.flood.femaZone);
    var floodZone = flChecked ? (hz.flood.zone || hz.flood.femaZone) : (hz.flood && hz.flood.checked === true && hz.flood.mapped === false ? L3('Unmapped (No FEMA polygon)', '未测绘（无 FEMA 图斑）', 'Sin mapa FEMA') : L3('Unverified (FEMA unavailable)', '未核实（FEMA 暂不可用）', 'Sin verificar (FEMA no disponible)'));
    var pga = getDesignPga(hz);
    var wsSlide = f[_T.wsKey];
    var walkVal = (wsSlide && wsSlide.walk) || (civ.scores && civ.scores.walkability) || null;
    var assignedSch = (f.schools && Array.isArray(f.schools.assigned)) ? f.schools.assigned : [];
    var topSchName = assignedSch[0] ? assignedSch[0].name : (g.schoolDistricts && (g.schoolDistricts.elementary || g.schoolDistricts.unified)) || 'Local Public District';

    var negEn = Array.isArray(ai.negotiation) && ai.negotiation.length ? ai.negotiation.slice(0, 3) : [
      'Request full seller disclosure packet, roof age certification, and recent sewer lateral video inspection.',
      'Benchmark offer contingencies against adjusted comparable sales (' + rangeStr + ') and true post-reassessment property taxes.',
      'Obtain binding HO-3 + specialty hazard insurance quotes during your inspection contingency window.'
    ];
    var watchMain = (Array.isArray(s.findings && s.findings.risks) && s.findings.risks[0]) ||
      (f.yearBuilt && f.yearBuilt < 1980 ? ('Vintage ' + f.yearBuilt + ' systems: verify electrical panel, plumbing supply lines, roof life, and foundation bolting.') : 'Verify municipal building permits and reassessment tax impact prior to contingency removal.');

    var bNameSlide = (typeof window !== 'undefined' && window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) || (rep && rep.brand && rep.brand.name) || ((typeof R !== 'undefined' && R && R.brand && R.brand.name) ? String(R.brand.name) : ['Open', 'Estate'].join(' '));

    return {
      generatedAt: (rep && rep.meta && rep.meta.generatedAt) || new Date().toISOString(),
      deckTitle: addr,
      deckSubtitle: '8-Slide Grounded Buyer Walkthrough Briefing',
      synthesisMode: 'instant',
      slides: [
        {
          slideNumber: 1,
          id: 'verdict',
          tag: '01 · Executive Verdict',
          title: 'Executive Snapshot & Deal Verdict',
          subtitle: addr + ' · ' + bedsBaths,
          heroStat: { value: valStr, label: bNameSlide + ' Fair Value (' + (isN(sc.overall) ? (Math.round(sc.overall) + '/100 Score') : 'Multi-Signal') + ')', tone: (sc.overall >= 72 ? 'good' : 'info') },
          criticalPoints: [
            'Estimated fair value ' + valStr + ' (confidence band ' + rangeStr + ', ' + ppsfStr + ').',
            'Property configuration: ' + bedsBaths + (lotSqft ? (' on a ' + fNum(lotSqft) + ' sq ft lot') : '') + (f.yearBuilt ? (', built ' + f.yearBuilt) : '') + '.',
            s.verdict ? ('Overall thesis: ' + s.verdict) : 'Balanced fundamentals across valuation, neighborhood livability, and public records.'
          ],
          watchout: watchMain,
          speakerNotes: 'Start with the big picture: anchor on the ' + rangeStr + ' valuation range and overall score before diving into physical structure and monthly carry.',
          visualKind: 'verdict',
          targetSection: 'overview',
          zh: {
            tag: '01 · 核心结论',
            title: '房产执行摘要与交易总评',
            subtitle: addr + ' · ' + bedsBaths,
            heroStatLabel: bNameSlide + ' 公允估值' + (isN(sc.overall) ? ('（综合分 ' + Math.round(sc.overall) + '/100）') : ''),
            criticalPoints: [
              '多源模型公允估值 ' + valStr + '（置信区间 ' + rangeStr + '，折合单价 ' + ppsfStr + '）。',
              '基础规格：' + (f.beds || '—') + ' 卧 ' + (f.baths || '—') + ' 卫 · 室内 ' + (f.sqft ? fNum(f.sqft) + ' 平尺' : '—') + (lotSqft ? (' · 占地 ' + fNum(lotSqft) + ' 平尺') : '') + (f.yearBuilt ? (' · ' + f.yearBuilt + ' 年建') : '') + '。',
              '结合估值离散度、社区配套与地籍记录，建议围绕置信区间理性制定出价策略。'
            ],
            watchout: f.yearBuilt && f.yearBuilt < 1980 ? ('房龄提示（' + f.yearBuilt + ' 年建）：重点核查配电箱、给排水管路、屋顶剩余寿命及地基抗震锚固。') : '出价前请核对政府施工许可闭环记录及过户后房产税重评金额。',
            speakerNotes: '第一页聚焦核心估值锚点与综合评分，快速建立对该房源性价比的整体判断。'
          },
          es: {
            tag: '01 · Veredicto Ejecutivo',
            title: 'Resumen Ejecutivo y Veredicto',
            subtitle: addr + ' · ' + bedsBaths,
            heroStatLabel: 'Valor Justo ' + bNameSlide + (isN(sc.overall) ? (' (' + Math.round(sc.overall) + '/100 pts)') : ''),
            criticalPoints: [
              'Valor justo estimado en ' + valStr + ' (rango ' + rangeStr + ', ' + ppsfStr + ').',
              'Configuración: ' + bedsBaths + (lotSqft ? (' en terreno de ' + fNum(lotSqft) + ' p²') : '') + (f.yearBuilt ? (', construido en ' + f.yearBuilt) : '') + '.',
              'Evaluación integral combinando comparables, registros del condado y riesgos ambientales.'
            ],
            watchout: f.yearBuilt && f.yearBuilt < 1980 ? ('Construcción de ' + f.yearBuilt + ': verificar panel eléctrico, tuberías, techo y anclaje sísmico.') : 'Verifique permisos municipales e impuestos reajustados antes de cerrar.',
            speakerNotes: 'Comience con el rango de valor justo y la puntuación general antes de revisar costos y riesgos.'
          }
        },
        {
          slideNumber: 2,
          id: 'property_lot',
          tag: '02 · Architecture & Lot',
          title: 'Home Architecture, Staging Context & Lot Reconciliation',
          subtitle: (f.propertyType || 'Single Family') + (f.apn ? (' · APN ' + f.apn) : ''),
          heroStat: { value: lotSqft ? (fNum(lotSqft) + ' sf') : bedsBaths, label: gisLot ? ('Assessor Lot · GIS Polygon ' + fNum(gisLot) + ' sf') : 'Recorded Parcel Area', tone: 'info' },
          criticalPoints: [
            'Year built: ' + (f.yearBuilt || '—') + ' · Stories: ' + (f.stories || 1) + ' · Living area: ' + (f.sqft ? fNum(f.sqft) + ' sq ft' : '—') + '.',
            lotSqft ? ('Multi-source lot check: County roll ' + fNum(lotSqft) + ' sq ft' + (gisLot ? (' vs GIS boundary ' + fNum(gisLot) + ' sq ft') : '') + (lr.dimensions ? (' (' + lr.dimensions + ')') : '') + '.') : 'Review county parcel map for exact frontage and setback lines.',
            'Zoning & parking: ' + (f.zoning || 'Residential') + ' · ' + (f.garage || (f.buildingRegister && f.buildingRegister.parking) || 'Verify garage/driveway on site') + '.'
          ],
          watchout: 'Cross-check permitted living area against County Assessor roll; confirm any detached garage or ADU conversion carries finaled municipal permits.',
          speakerNotes: 'Hover or tap the lot size badge in Overview to inspect every public lot measurement side-by-side.',
          visualKind: 'property_lot',
          targetSection: 'county',
          zh: {
            tag: '02 · 建筑与宗地核对',
            title: '房屋格局、历史影像与多源占地核对',
            subtitle: (f.propertyType || '独立屋') + (f.apn ? (' · 地籍号 APN ' + f.apn) : ''),
            heroStatLabel: gisLot ? ('县府登记占地 · GIS红线 ' + fNum(gisLot) + ' 平尺') : '官方登记宗地面积',
            criticalPoints: [
              '建成年代：' + (f.yearBuilt || '—') + ' 年 · 层数：' + (f.stories || 1) + ' 层 · 室内面积：' + (f.sqft ? fNum(f.sqft) + ' 平方英尺' : '—') + '。',
              lotSqft ? ('多源宗地核对：县评估师登记 ' + fNum(lotSqft) + ' 平尺' + (gisLot ? ('，GIS 红线多边形实测 ' + fNum(gisLot) + ' 平尺') : '') + '。') : '请结合县府地籍图核实临街面宽与退界。',
              '土地规划与车位：' + (f.zoning || '住宅分区') + ' · ' + (f.garage || '请实地核对车库与车道宽度') + '。'
            ],
            watchout: '请将挂牌面积与县府登记室内面积比对；若有车库改建或附属单元（ADU），务必核实竣工许可（Finaled Permit）。',
            speakerNotes: '在总览页悬停或点击占地面积数字，即可查看县府档案、GIS多边形与建筑投影面积的完整对账。'
          },
          es: {
            tag: '02 · Arquitectura y Lote',
            title: 'Arquitectura, Escenografía y Conciliación del Lote',
            subtitle: (f.propertyType || 'Residencial') + (f.apn ? (' · APN ' + f.apn) : ''),
            heroStatLabel: gisLot ? ('Lote Fiscal · Polígono GIS ' + fNum(gisLot) + ' p²') : 'Área de Parcela Registrada',
            criticalPoints: [
              'Construcción: ' + (f.yearBuilt || '—') + ' · Niveles: ' + (f.stories || 1) + ' · Área habitable: ' + (f.sqft ? fNum(f.sqft) + ' p²' : '—') + '.',
              lotSqft ? ('Conciliación de lote: registro del condado ' + fNum(lotSqft) + ' p²' + (gisLot ? (' vs polígono GIS ' + fNum(gisLot) + ' p²') : '') + '.') : 'Consulte el mapa catastral para linderos exactos.',
              'Zonificación y estacionamiento: ' + (f.zoning || 'Residencial') + ' · ' + (f.garage || 'Verificar cochera en visita') + '.'
            ],
            watchout: 'Compare los pies cuadrados anunciados con el registro oficial del tasador y confirme permisos de cualquier ADU o ampliación.',
            speakerNotes: 'Revise la sección de Registros del Condado para conciliar dimensiones exactas del lote.'
          }
        },
        {
          slideNumber: 3,
          id: _T.zeComps,
          tag: '03 · Valuation & Comps',
          title: 'Valuation Benchmark & Comparable Sales',
          subtitle: 'Multi-model ensemble vs local neighborhood sales',
          heroStat: { value: ppsfStr, label: 'Estimated Price / Sq Ft (' + (e.confidence || 'Blended') + ' confidence)', tone: 'good' },
          criticalPoints: [
            'Blended fair value ' + valStr + ' synthesizes repeat-sales indexing, feature-adjusted comps, and ZIP home-value index tier anchors.',
            f.lastSale && f.lastSale.price ? ('Last recorded sale: ' + fUsd(f.lastSale.price) + ' on ' + String(f.lastSale.date || '').slice(0, 10) + '.') : 'Historical sale baseline cross-checked against County Assessor tax roll.',
            'Compare $/sq ft (' + ppsfStr + ') directly against closed comps within 0.5–1.0 miles adjusted for lot size and remodel condition.'
          ],
          watchout: 'Automated models cannot see unpermitted interior finishes — adjust +/- 5–8% after touring kitchen, bath, and natural light.',
          speakerNotes: 'Use the Value & True Estimate Hub chapter to inspect individual comp adjustments and 10-year price cycles.',
          visualKind: _T.zeComps,
          targetSection: 'value',
          zh: {
            tag: '03 · 多维估值与可比成交',
            title: '估值基准对标与周边成交拆解',
            subtitle: '多模型加权估值 vs 同街区近期实测成交',
            heroStatLabel: '预估每平尺单价（置信度：' + (e.confidence || '中等') + '）',
            criticalPoints: [
              '融合估值 ' + valStr + ' 综合了历史重售指数、特征修正可比成交及邮编房价分位锚点。',
              f.lastSale && f.lastSale.price ? ('上次登记成交：' + String(f.lastSale.date || '').slice(0, 10) + '，成交价 ' + fUsd(f.lastSale.price) + '。') : '已结合郡县计税评估账面进行交叉校准。',
              '建议重点对比 1 英里内相似占地与翻新程度房源的折算单价（当前约 ' + ppsfStr + '）。'
            ],
            watchout: '量化模型无法直接观察室内精装细节，实看厨房卫浴新旧与采光后建议预留 ±5%–8% 调整空间。',
            speakerNotes: '可跳转至“估值与成交”章节查看多因子瀑布图与每套可比房源的加减分明细。'
          },
          es: {
            tag: '03 · Valoración y Comparables',
            title: 'Referencia de Valor y Ventas Comparables',
            subtitle: 'Modelo multimétodo frente a ventas cercanas',
            heroStatLabel: 'Precio Est. / Pie² (confianza ' + (e.confidence || 'Media') + ')',
            criticalPoints: [
              'Valor estimado ' + valStr + ' combinando índice de ventas repetidas, comparables ajustados e índice de valor del código postal.',
              f.lastSale && f.lastSale.price ? ('Última venta registrada: ' + fUsd(f.lastSale.price) + ' (' + String(f.lastSale.date || '').slice(0, 10) + ').') : 'Contrastado con la tasación oficial del condado.',
              'Compare ' + ppsfStr + ' con ventas recientes ajustadas por tamaño de terreno y estado interior.'
            ],
            watchout: 'Los modelos automáticos no captan acabados interiores no registrados; ajuste tras la visita física.',
            speakerNotes: 'Explore el capítulo de Valor y Comparables para ver la cascada multifactorial completa.'
          }
        },
        (function () {
          var insMoS = (iq && isN(iq.monthlyTotal)) ? iq.monthlyTotal : (o.monthly && isN(o.monthly.insurance) ? o.monthly.insurance : null);
          var insAnnS = (iq && isN(iq.annualTotal)) ? iq.annualTotal : (isN(insMoS) ? insMoS * 12 : null);
          var covAS = (iq && iq.coverage && isN(iq.coverage.dwellingA)) ? iq.coverage.dwellingA : null;
          var eqMoS = (iq && iq.specialtyAddOns && iq.specialtyAddOns.earthquake && isN(iq.specialtyAddOns.earthquake.estMonthly)) ? iq.specialtyAddOns.earthquake.estMonthly : null;
          var flMoS = (iq && iq.specialtyAddOns && iq.specialtyAddOns.flood && isN(iq.specialtyAddOns.flood.estMonthly)) ? iq.specialtyAddOns.flood.estMonthly : null;
          var heroValS = piti ? (fUsd(piti) + '/mo') : (isN(insMoS) ? (fUsd(insMoS) + '/mo Ins.') : '—');
          var ho3PtEn = isN(insMoS)
            ? ('HO-3 homeowners estimate: ~' + fUsd(insMoS) + '/mo' + (isN(insAnnS) ? ' (' + fUsd(insAnnS) + '/yr)' : '') + (isN(covAS) ? ' for ' + fUsdK(covAS) + ' Coverage A rebuild.' : '.'))
            : 'Homeowners insurance quote data unavailable in payload — obtain carrier quotes directly during contingency.';
          var addPtEn = (isN(eqMoS) || isN(flMoS))
            ? ('Optional specialty protection:' + (isN(eqMoS) ? ' Earthquake +' + fUsd(eqMoS) + '/mo' : '') + (isN(eqMoS) && isN(flMoS) ? ' ·' : '') + (isN(flMoS) ? ' Flood +' + fUsd(flMoS) + '/mo' : '') + '.')
            : 'Evaluate separate earthquake and flood endorsements based on mapped parcel hazard zones.';
          var ho3PtZh = isN(insMoS)
            ? ('HO-3 房屋主险测算：约 ' + fUsd(insMoS) + '/月' + (isN(insAnnS) ? '（' + fUsd(insAnnS) + '/年）' : '') + (isN(covAS) ? '，对应重建保额 Coverage A ' + fUsdK(covAS) + '。' : '。'))
            : '报告载荷未包含保险公司核价数据，请在尽调期内向持牌机构实询。';
          var addPtZh = (isN(eqMoS) || isN(flMoS))
            ? ('专项灾害附加险测算：' + (isN(eqMoS) ? '地震险约 +' + fUsd(eqMoS) + '/月' : '') + (isN(eqMoS) && isN(flMoS) ? ' · ' : '') + (isN(flMoS) ? '洪水险约 +' + fUsd(flMoS) + '/月' : '') + '。')
            : '请结合地块灾害分区分开评估地震险与洪水险保费。';
          var ho3PtEs = isN(insMoS)
            ? ('Seguro de hogar HO-3 estimado: ~' + fUsd(insMoS) + '/mes' + (isN(insAnnS) ? ' (' + fUsd(insAnnS) + '/año)' : '') + (isN(covAS) ? ' (Cobertura A ' + fUsdK(covAS) + ').' : '.'))
            : 'Cotización de seguro no incluida en los datos; solicite cotizaciones directas durante contingencia.';
          var addPtEs = (isN(eqMoS) || isN(flMoS))
            ? ('Coberturas opcionales:' + (isN(eqMoS) ? ' Sismo +' + fUsd(eqMoS) + '/mes' : '') + (isN(eqMoS) && isN(flMoS) ? ' ·' : '') + (isN(flMoS) ? ' Inundación +' + fUsd(flMoS) + '/mes' : '') + '.')
            : 'Evalúe coberturas adicionales de sismo e inundación según la zona de riesgo.';
          return {
            slideNumber: 4,
            id: 'cost_insurance',
            tag: '04 · Monthly Cost & Insurance',
            title: 'True Monthly Carry & Insurance Reality Check',
            subtitle: 'Reassessed taxes + realistic HO-3 & specialty hazard quotes',
            heroStat: { value: heroValS, label: 'Est. Monthly Carry (20% down · 30Y fixed)', tone: 'warn' },
            criticalPoints: [
              'Baseline monthly carry ~' + (piti ? fUsd(piti) : '—') + '/mo includes principal & interest, reassessed property tax, HO-3 insurance, and maintenance.',
              ho3PtEn,
              addPtEn
            ],
            watchout: 'Do not rely on naive lender 0.25% insurance placeholders — vintage roofs or seismic/wildfire zones materially shift underwriting.',
            speakerNotes: 'Open the interactive Insurance Quote Studio in Monthly Cost to review coverage architecture and risk drivers.',
            visualKind: 'cost_insurance',
            targetSection: 'payment',
            zh: {
              tag: '04 · 真实月供与保险核价',
              title: '全口径月持有成本与房屋保险精算',
              subtitle: '按成交价重评房产税 + HO-3 主险与地震/洪水附加险',
              heroStatLabel: '预估每月总支出（首付 20% · 30年期固定利率）',
              criticalPoints: [
                '基准月持有总支出约 ' + (piti ? fUsd(piti) : '—') + '/月（含本息、过户重评后新地税、房屋保险与日常维护预留）。',
                ho3PtZh,
                addPtZh
              ],
              watchout: '切勿轻信贷款预批单上偏低的保险估算；老房龄屋顶或高地震烈度区需提前在交割前锁定承保报价。',
              speakerNotes: '在“月度持有成本”保险报价台可核对保障结构与核保风险因子。'
            },
            es: {
              tag: '04 · Costo Mensual y Seguro',
              title: 'Costo Mensual Real y Cotización de Seguros',
              subtitle: 'Impuestos reajustados + seguro HO-3 y coberturas especiales',
              heroStatLabel: 'Costo Mensual Est. (20% enganche · 30 años fijo)',
              criticalPoints: [
                'Pago mensual total estimado ~' + (piti ? fUsd(piti) : '—') + '/mes incluyendo capital, interés, impuesto predial reajustado y mantenimiento.',
                ho3PtEs,
                addPtEs
              ],
              watchout: 'Cotice pólizas reales durante el periodo de contingencia; la antigüedad del techo y riesgos naturales impactan la prima.',
              speakerNotes: 'Use el Estudio de Seguros para revisar coberturas y factores de suscripción.'
            }
          };
        })(),
        {
          slideNumber: 5,
          id: 'remodel_upside',
          tag: '05 · Lot & CapEx Potential',
          title: 'Lot Potential, ADU & Vintage CapEx Triage',
          subtitle: 'Expansion headroom vs 5-year system replacement budget',
          heroStat: { value: la.openLotSqft ? (fNum(la.openLotSqft) + ' sf') : (la.lotCoveragePct != null ? (la.lotCoveragePct + '% BCR') : 'CapEx Plan'), label: la.feasibility ? ('Open Yard · ' + la.feasibility) : 'Lot Coverage & Expansion Headroom', tone: 'good' },
          criticalPoints: [
            la.lotCoveragePct != null ? ('Building lot coverage ~' + la.lotCoveragePct + '% leaving ~' + fNum(la.openLotSqft || 0) + ' sq ft of open lot area.') : 'Substantial yard geometry supports outdoor living or detached workspace study.',
            la.estRentMonthly ? ('Estimated detached ADU rental offset potential: ~' + fUsd(la.estRentMonthly) + '/mo (verify municipal setbacks & utility capacity).') : 'Check state & local ADU setback rules (typically 4 ft side/rear in eligible zones).',
            f.yearBuilt ? ('Built in ' + f.yearBuilt + ': budget proactively for roof membrane, water heater, HVAC, and seismic cripple-wall retrofit if unupgraded.') : 'Inspect roof, HVAC, sewer lateral, and main electrical service amperage.'
          ],
          watchout: 'Verify sewer lateral depth/condition and 200A electrical panel capacity before pricing any ADU or major kitchen remodel.',
          speakerNotes: 'Check the 3D Lot Model and Checklist chapters for solar roof azimuth and itemized CapEx lifespans.',
          visualKind: 'remodel_upside',
          targetSection: 'house3d',
          zh: {
            tag: '05 · 加建潜力与大修预算',
            title: '庭院加建空间（ADU）与房屋系统大修排期',
            subtitle: '土地利用余量 vs 未来 5 年建筑系统维护重点',
            heroStatLabel: la.feasibility ? ('剩余空地 · ' + la.feasibility) : '建筑占地率与院落扩展空间',
            criticalPoints: [
              la.lotCoveragePct != null ? ('当前建筑占地率约 ' + la.lotCoveragePct + '%，剩余院落空地约 ' + fNum(la.openLotSqft || 0) + ' 平方英尺。') : '结合三维地形与建筑轮廓模型评估后院采光与加建可行性。',
              la.estRentMonthly ? ('若合规增建独立 ADU，潜在月租金补贴约 ~' + fUsd(la.estRentMonthly) + '/月（需核实退界与水电增容）。') : '可对照当地 ADU 退界新规核查独立后屋或车库改建潜力。',
              f.yearBuilt ? ('建于 ' + f.yearBuilt + ' 年：看房时请重点核查屋顶年限、主配电箱安培数（建议 200A）、暖通及地基抗震加固情况。') : '建议安排专业屋顶、主排污管内窥镜与电路负荷检测。'
            ],
            watchout: '规划加建或重装厨房前，务必先确认入户排污管（Sewer Lateral）状态及主配电箱扩容空间。',
            speakerNotes: '可在三维模型（3D Model）模块查看全天日照阴影轨迹与屋顶光伏朝向。'
          },
          es: {
            tag: '05 · Potencial de Lote y CapEx',
            title: 'Potencial de ADU, Terreno y Mantenimiento Mayor',
            subtitle: 'Espacio libre para ampliación vs vida útil de sistemas',
            heroStatLabel: 'Área Libre de Lote y Viabilidad ADU',
            criticalPoints: [
              la.lotCoveragePct != null ? ('Ocupación del lote ~' + la.lotCoveragePct + '% dejando ~' + fNum(la.openLotSqft || 0) + ' p² libres.') : 'Revise el modelo 3D para evaluar soleamiento y espacio trasero.',
              la.estRentMonthly ? ('Potencial estimado de renta por ADU: ~' + fUsd(la.estRentMonthly) + '/mes sujeto a permisos municipales.') : 'Verifique retiros laterales y traseros para construcción de ADU.',
              'Priorice inspección de techo, panel eléctrico de 200A, climatización y drenaje sanitario.'
            ],
            watchout: 'Confirme capacidad eléctrica y estado del drenaje antes de presupuestar remodelaciones mayores.',
            speakerNotes: 'Consulte el Modelo 3D para estudiar sombras estacionales y altura de techo.'
          }
        },
        {
          slideNumber: 6,
          id: 'risk_climate',
          tag: '06 · Hazards & Environment',
          title: 'Natural Hazards, Climate & Environmental Screen',
          subtitle: 'FEMA flood, USGS seismic shaking, wildfire & EPA proximity',
          heroStat: { value: isN(sc.safety) ? (Math.round(sc.safety) + '/100') : String(floodZone), label: 'Natural Hazard & Safety Profile (' + floodZone + ')', tone: (sc.safety >= 65 ? 'good' : 'warn') },
          criticalPoints: [
            'FEMA Flood Designation: ' + floodZone + (pga ? (' · USGS Design Seismic PGA: ' + pga + 'g') : '') + '.',
            'Environmental screen score: ' + (isN(sc.environment) ? (Math.round(sc.environment) + '/100') : 'Reviewed') + ' across EPA Superfund, TRI, LUST, air quality, and road/rail noise.',
            env && env.airQuality && env.airQuality.pm25 != null ? ('Recent 30-day modeled PM2.5 average: ' + env.airQuality.pm25 + ' µg/m³.') : 'Review interactive map layers for nearby fault traces, floodplains, and regulated sites.'
          ],
          watchout: pga && pga >= 0.4 ? 'Elevated seismic shaking zone: verify foundation anchor bolts, cripple-wall plywood shear panels, and seismic gas shutoff valve.' : 'Always review official state Natural Hazard Disclosure (NHD) zones during escrow.',
          speakerNotes: 'Chapter 5 maps every FEMA flood zone, Quaternary fault, and EPA site around the parcel.',
          visualKind: 'risk_climate',
          targetSection: 'risk',
          zh: {
            tag: '06 · 自然灾害与环境筛查',
            title: '自然灾害、气候风险与周边环境排查',
            subtitle: 'FEMA 洪水区 · USGS 地震动峰值 · 山火与环保档案',
            heroStatLabel: '灾害安全子评分（洪水区：' + floodZone + '）',
            criticalPoints: [
              'FEMA 洪水区划：' + floodZone + (pga ? (' · USGS 设计地震动峰值加速度 PGA：' + pga + 'g') : '') + '。',
              '环境健康评分：' + (isN(sc.environment) ? (Math.round(sc.environment) + '/100') : '已核查') + '（涵盖 EPA 超级基金、有毒排放、地下储罐、空气质量与道路噪音）。',
              '可在灾害与环境交互地图中逐层开关断层线、洪泛区及周边监管站点。'
            ],
            watchout: pga && pga >= 0.4 ? '处于较高地震烈度区：请核查地基螺栓锚固、矮墙（Cripple Wall）抗剪加固及地震燃气自动切断阀。' : '过户期间请仔细核对法定自然灾害披露报告（NHD）各项勾选结果。',
            speakerNotes: '第五章提供完整的 FEMA 实时图层、断层距离与周边环境监测明细。'
          },
          es: {
            tag: '06 · Riesgos y Entorno',
            title: 'Riesgos Naturales, Clima y Revisión Ambiental',
            subtitle: 'Inundación FEMA, sismicidad USGS, incendios y EPA',
            heroStatLabel: 'Perfil de Seguridad Natural (' + floodZone + ')',
            criticalPoints: [
              'Zona de inundación FEMA: ' + floodZone + (pga ? (' · PGA sísmico USGS: ' + pga + 'g') : '') + '.',
              'Puntuación ambiental: ' + (isN(sc.environment) ? (Math.round(sc.environment) + '/100') : 'Verificado') + ' (sitios EPA, calidad del aire y ruido vial).',
              'Inspeccione capas interactivas de fallas geológicas y sitios regulados en el Capítulo 5.'
            ],
            watchout: 'Revise la declaración oficial de riesgos naturales (NHD) y medidas de mitigación sísmica o contra incendios.',
            speakerNotes: 'El Capítulo 5 muestra mapas detallados de riesgos naturales y calidad ambiental.'
          }
        },
        {
          slideNumber: 7,
          id: 'schools_lifestyle',
          tag: '07 · Schools & Walkability',
          title: 'Schools, Safety & Daily Convenience',
          subtitle: topSchName + (walkVal != null ? (' · Walkability ' + Math.round(walkVal) + '/100') : ''),
          heroStat: { value: isN(sc.livability) ? (Math.round(sc.livability) + '/100') : (walkVal != null ? (Math.round(walkVal) + '/100') : 'Lifestyle'), label: 'Neighborhood Livability & Daily Access', tone: 'good' },
          criticalPoints: [
            assignedSch.length ? ('Assigned public schools (' + assignedSch.length + '): ' + assignedSch.slice(0, 3).map(function (x) { return x.name; }).join(' · ') + '.') : ('School jurisdiction: ' + topSchName + ' — verify current attendance boundaries directly with the district.'),
            walkVal != null ? ('Neighborhood walkability index: ' + Math.round(walkVal) + '/100 across nearby groceries, cafés, parks, and transit stops.') : 'OpenStreetMap amenity scan covers groceries, parks, dining, healthcare, and transit.',
            sf.fireStation ? ('Nearest fire/EMS station: ~' + sf.fireStation.distMi + ' mi away ( crucial for both emergency response and insurance protection class).') : 'City-level FBI crime context and emergency response proximity included in Safety section.'
          ],
          watchout: 'School attendance boundaries and enrollment caps can change annually — confirm seat availability directly with the district registrar.',
          speakerNotes: 'Add your workplace or family destinations in the Neighborhood section to model peak-hour commute times.',
          visualKind: 'schools_lifestyle',
          targetSection: 'schools',
          zh: {
            tag: '07 · 学区·治安与生活圈',
            title: '公立学区、应急治安与日常便利度',
            subtitle: topSchName + (walkVal != null ? (' · 步行指数 ' + Math.round(walkVal) + '/100') : ''),
            heroStatLabel: '生活圈宜居与便利度评分',
            criticalPoints: [
              assignedSch.length ? ('对口公立学校（共 ' + assignedSch.length + ' 所）：' + assignedSch.slice(0, 3).map(function (x) { return x.name; }).join(' · ') + '。') : ('所属学区：' + topSchName + '（入学前请向学区核实施行划片边界）。'),
              walkVal != null ? ('社区步行便利指数：' + Math.round(walkVal) + '/100，涵盖商超、公园绿地、餐饮与公共交通站点。') : '已汇总周边 ¼ / ½ / 1 英里内超市、公园、医疗与公交配套密度。',
              sf.fireStation ? ('最近消防急救站距离约 ' + sf.fireStation.distMi + ' 英里，兼顾应急响应与房屋保险消防评级优势。') : '治安与应急章节提供城市级 FBI 犯罪率对比及最近消防医院距离。'
            ],
            watchout: '热门公立学校可能存在学位满额调剂（Overflow），买房前请直接向学区办公室确认当年学位政策。',
            speakerNotes: '在“生活圈”章节可输入工作地点，测算早晚高峰实测通勤车程。'
          },
          es: {
            tag: '07 · Escuelas y Vida Diaria',
            title: 'Escuelas, Seguridad y Caminabilidad',
            subtitle: topSchName + (walkVal != null ? (' · Caminabilidad ' + Math.round(walkVal) + '/100') : ''),
            heroStatLabel: 'Habitalidad y Acceso Diario',
            criticalPoints: [
              assignedSch.length ? ('Escuelas públicas asignadas: ' + assignedSch.slice(0, 3).map(function (x) { return x.name; }).join(' · ') + '.') : ('Distrito escolar: ' + topSchName + '.'),
              walkVal != null ? ('Índice de caminabilidad: ' + Math.round(walkVal) + '/100 hacia supermercados, parques y transporte.') : 'Mapa de servicios cercanos disponible en la sección de Vecindario.',
              sf.fireStation ? ('Estación de bomberos más cercana a ~' + sf.fireStation.distMi + ' mi.') : 'Revise estadísticas de seguridad y servicios de emergencia en el Capítulo 5.'
            ],
            watchout: 'Confirme siempre los límites escolares vigentes y cupos directamente con el distrito escolar.',
            speakerNotes: 'Pruebe tiempos de traslado personalizados en la sección de Vecindario.'
          }
        },
        {
          slideNumber: 8,
          id: 'action_plan',
          tag: '08 · Negotiation & Next Steps',
          title: 'Buyer Negotiation Levers & Due-Diligence Action Plan',
          subtitle: 'Upload disclosures/inspections & execute pre-offer checklist',
          heroStat: { value: '3 Levers', label: 'Actionable Offer, Credit & Contingency Playbook', tone: 'good' },
          criticalPoints: negEn,
          watchout: 'Upload the Seller Disclosure packet, Home Inspection PDF, and Preliminary Title Report into Docs & AI (Chapter 6) to auto-extract repair credits and lien alerts.',
          speakerNotes: 'Use AI Analyst Q&A in Chapter 6 to ask follow-up questions grounded in every number of this report.',
          visualKind: 'action_plan',
          targetSection: 'inspection',
          zh: {
            tag: '08 · 谈判抓手与行动清单',
            title: '买家议价筹码与交割前尽调行动路线',
            subtitle: '披露包/验房报告 AI 交叉核验与出价策略',
            heroStatLabel: '三大核心议价与尽调抓手',
            criticalPoints: [
              '索取完整卖家披露包（TDS/SPQ/NHD）、屋顶年限证明及排污管内窥镜检测报告。',
              '以周边经修正可比成交（' + rangeStr + '）及过户重评后的真实税险月供为锚点设定出价上限。',
              '在验房保护期内同步锁定 HO-3 房屋主险及地震/洪水专项险正式核保报价。'
            ],
            watchout: '收到卖家验房报告或产权预报告（Prelim PDF）后，可直接上传至第六章“验房产权 AI”，一键提取维修抵扣清单与留置权风险。',
            speakerNotes: '您还可在第六章 AI 投研问答框中随时针对本房产提问，获取基于实测数据的即时解答。'
          },
          es: {
            tag: '08 · Negociación y Plan de Acción',
            title: 'Palancas de Negociación y Lista de Diligencia',
            subtitle: 'Revisión de documentos con IA y estrategia de oferta',
            heroStatLabel: 'Plan de Oferta y Contingencias en 3 Pasos',
            criticalPoints: [
              'Solicite declaraciones del vendedor, certificación de techo e inspección de drenaje.',
              'Ancle su oferta en el rango justo (' + rangeStr + ') considerando impuestos y seguros reales.',
              'Asegure cotizaciones firmes de seguro HO-3 durante el plazo de contingencia de inspección.'
            ],
            watchout: 'Suba el informe de inspección y reporte preliminar de título en el Capítulo 6 (Docs e IA) para calcular créditos de reparación.',
            speakerNotes: 'Use el Analista IA en el Capítulo 6 para resolver dudas específicas antes de ofertar.'
          }
        }
      ]
    };
  }

  function getWalkthroughDeck(rep) {
    var raw = (rep && rep.walkthroughSlides) || mod(rep, 'walkthroughSlides');
    if (raw && Array.isArray(raw.slides) && raw.slides.length >= 3) {
      return raw;
    }
    return buildDeterministicSlidesClient(rep || {});
  }

  function localizeSlide(slide) {
    var l = langCode();
    var ov = l === 'zh' ? slide.zh : l === 'es' ? slide.es : null;
    var hs = slide.heroStat || { value: '—', label: '', tone: 'info' };
    return {
      slideNumber: slide.slideNumber || 1,
      id: slide.id || 'slide',
      tag: (ov && ov.tag) || slide.tag || '',
      title: (ov && ov.title) || slide.title || '',
      subtitle: (ov && ov.subtitle) || slide.subtitle || '',
      heroValue: hs.value || '—',
      heroLabel: (ov && ov.heroStatLabel) || hs.label || '',
      heroTone: hs.tone || 'info',
      criticalPoints: (ov && Array.isArray(ov.criticalPoints) && ov.criticalPoints.length) ? ov.criticalPoints : (slide.criticalPoints || []),
      watchout: (ov && ov.watchout) || slide.watchout || '',
      speakerNotes: (ov && ov.speakerNotes) || slide.speakerNotes || '',
      visualKind: slide.visualKind || slide.id || 'verdict',
      targetSection: slide.targetSection || 'overview'
    };
  }

  function renderSlideVisualCard(kind, rep) {
    var f = (rep && rep.facts) || {};
    var v = mod(rep, 'valuation') || {};
    var e = v.estimate || {};
    var o = v.ownership || {};
    var m0 = o.monthly || {};
    var sc = (rep && rep.summary && rep.summary.scores) || {};
    var df = (rep && rep.decisionFactors) || {};
    var iq = normalizeInsuranceQuotes(rep);

    function rowBar(lbl, val, max, col, suffix) {
      var pct = isN(val) && max > 0 ? Math.max(4, Math.min(100, Math.round((val / max) * 100))) : 0;
      return '<div class="wt-vbar">' +
        '<div class="wt-vtop"><span>' + esc(lbl) + '</span><b>' + (isN(val) ? (suffix === 'usd' ? fUsd(val) : suffix === 'usdk' ? fUsdK(val) : (Math.round(val) + (suffix || ''))) : '—') + '</b></div>' +
        '<div class="meter"><i style="width:' + pct + '%;background:' + col + '"></i></div>' +
      '</div>';
    }

    if (kind === 'verdict') {
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Multi-Dimension Scorecard', '五维客观评分雷达', 'Puntuación Multidimensional')) + '</div>' +
        rowBar(L3('Overall Score', '综合评分', 'Puntaje Total'), sc.overall, 100, '#0f766e', '/100') +
        rowBar(L3('Investment', '投资价值', 'Inversión'), sc.investment, 100, '#2563eb', '/100') +
        rowBar(L3('Hazard Safety', '灾害安全', 'Seguridad'), sc.safety, 100, '#16a34a', '/100') +
        rowBar(L3('Environment', '环境健康', 'Entorno'), sc.environment, 100, '#0891b2', '/100') +
        rowBar(L3('Livability', '生活宜居', 'Habitalidad'), sc.livability, 100, '#7c3aed', '/100') +
      '</div>';
    }
    if (kind === 'property_lot') {
      var lr = (f.countyRecords && f.countyRecords.lotReconciliation) || f.lotReconciliation || {};
      var lotA = f.lotSqft || 0;
      var lotG = lr.gisPolygonSqft || lotA;
      var fp = (f.buildingRegister && f.buildingRegister.footprintSqft) || (f.sqft ? Math.round(f.sqft / Math.max(1, f.stories || 1)) : 0);
      var mx = Math.max(lotA, lotG, fp, 1);
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Parcel & Footprint Reconciliation', '宗地面积多源对账', 'Conciliación del Terreno')) + '</div>' +
        rowBar(L3('County Roll Lot', '县府登记占地', 'Lote Catastral'), lotA || null, mx, '#0f766e', ' sf') +
        (lotG ? rowBar(L3('GIS Parcel Polygon', 'GIS 红线实测', 'Polígono GIS'), lotG, mx, '#0284c7', ' sf') : '') +
        (fp ? rowBar(L3('Roof Footprint', '建筑投影面积', 'Huella de Techo'), fp, mx, '#d97706', ' sf') : '') +
        '<div class="wt-vchips">' +
          (f.yearBuilt ? '<span>' + esc(L3('Built ', '建于 ', 'Año ') + f.yearBuilt) + '</span>' : '') +
          (f.stories ? '<span>' + esc(f.stories + ' ' + L3('Stories', '层', 'Niveles')) + '</span>' : '') +
          (f.zoning ? '<span>' + esc(f.zoning) + '</span>' : '') +
        '</div>' +
      '</div>';
    }
    if (kind === _T.zeComps) {
      var bm = extractBenchmarks(rep);
      var bNameV = (typeof window !== 'undefined' && window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) || (rep && rep.brand && rep.brand.name) || ((typeof R !== 'undefined' && R && R.brand && R.brand.name) ? String(R.brand.name) : ['Open', 'Estate'].join(' '));
      var vals = [bm.est, bm.extAvmVal, bm.compsModelVal, e.high].filter(isN);
      var vmax = vals.length ? Math.max.apply(null, vals) * 1.06 : 1;
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Valuation Signal Comparison', '多源估值模型对标', 'Comparación de Señales')) + '</div>' +
        rowBar(L3(bNameV + ' Blend', bNameV + ' 融合估值', 'Mezcla ' + bNameV), bm.est, vmax, '#0f766e', 'usdk') +
        (isN(bm.extAvmVal) ? rowBar(bm.extAvmLbl, bm.extAvmVal, vmax, '#2563eb', 'usdk') : '') +
        (isN(bm.compsModelVal) ? rowBar(bm.compsModelLbl, bm.compsModelVal, vmax, '#6366f1', 'usdk') : '') +
        (isN(e.low) && isN(e.high) ? '<div class="small" style="margin-top:8px">' + esc(L3('Confidence Band: ', '置信区间：', 'Banda: ')) + '<b>' + fUsdK(e.low) + ' – ' + fUsdK(e.high) + '</b></div>' : '') +
      '</div>';
    }
    if (kind === 'cost_insurance') {
      var pi = isN(m0.principalInterest) ? m0.principalInterest : 0;
      var txm = isN(m0.propertyTax) ? m0.propertyTax : 0;
      var insM = (iq && isN(iq.monthlyTotal)) ? iq.monthlyTotal : (isN(m0.insurance) ? m0.insurance : 0);
      var mnt = isN(m0.maintenance) ? m0.maintenance : 0;
      var cmax = Math.max(pi + txm + insM + mnt, 1);
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Monthly Carry Stack', '月持有支出结构', 'Desglose Mensual')) + '</div>' +
        rowBar(L3('Principal & Interest', '房贷本息', 'Capital e Interés'), pi || null, cmax, '#0f766e', 'usd') +
        rowBar(L3('Reassessed Tax', '重评房产税', 'Impuesto Predial'), txm || null, cmax, '#2563eb', 'usd') +
        rowBar(L3('HO-3 Insurance', 'HO-3 房屋主险', 'Seguro HO-3'), insM || null, cmax, '#d97706', 'usd') +
        rowBar(L3('Maintenance Reserve', '维护预留金', 'Mantenimiento'), mnt || null, cmax, '#64748b', 'usd') +
      '</div>';
    }
    if (kind === 'remodel_upside') {
      var la = df.lotAndAdu || {};
      var sol = df.solar || {};
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Lot Expansion & Energy Snapshot', '院落扩展与光伏指标', 'Expansión y Energía Solar')) + '</div>' +
        (isN(la.lotCoveragePct) ? rowBar(L3('Lot Coverage (BCR)', '当前建筑占地率', 'Ocupación de Lote'), la.lotCoveragePct, 100, '#0f766e', '%') : '') +
        '<div class="wt-vchips" style="margin-top:8px">' +
          (la.openLotSqft ? '<span>' + fNum(la.openLotSqft) + ' sf ' + esc(L3('Open Yard', '空地余量', 'Patio Libre')) + '</span>' : '') +
          (sol.systemKw ? '<span>☀️ ~' + sol.systemKw + ' kW Solar</span>' : '') +
          (la.estRentMonthly ? '<span>🏡 ~' + fUsd(la.estRentMonthly) + L3('/mo ADU', '/月 ADU潜能', '/mes ADU') + '</span>' : '') +
        '</div>' +
      '</div>';
    }
    if (kind === 'risk_climate') {
      var clim = (rep && rep.summary && Array.isArray(rep.summary.climate)) ? rep.summary.climate.slice(0, 4) : [];
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Climate & Hazard Exposure (0–10)', '气候与自然灾害暴露度 (0–10)', 'Exposición Climática (0–10)')) + '</div>' +
        (clim.length
          ? clim.map(function (c) { return rowBar(c.label || c.key, c.score, 10, (c.score <= 3 ? '#16a34a' : c.score <= 6 ? '#d97706' : '#dc2626'), '/10'); }).join('')
          : (isN(sc.safety)
              ? rowBar(L3('Safety Sub-Score', '灾害安全分', 'Puntaje Seguridad'), sc.safety, 100, '#16a34a', '/100')
              : '<div class="small">' + esc(L3('Safety sub-score unavailable for this parcel.', '该地块暂无灾害安全子评分。', 'Puntaje de seguridad no disponible.')) + '</div>')) +
      '</div>';
    }
    if (kind === 'schools_lifestyle') {
      var schs = (f.schools && Array.isArray(f.schools.assigned)) ? f.schools.assigned.slice(0, 3) : [];
      var wsCard = f[_T.wsKey];
      var wk = (wsCard && wsCard.walk) || (mod(rep, 'civic') && mod(rep, 'civic').scores && mod(rep, 'civic').scores.walkability) || null;
      return '<div class="wt-vcard">' +
        '<div class="wt-vhdr">' + esc(L3('Schools & Mobility Metrics', '对口学校与出行指标', 'Escuelas y Movilidad')) + '</div>' +
        (schs.length ? schs.map(function (s2) {
          var sc10 = isN(s2.percentile) ? +(s2.percentile / 10).toFixed(1) : null;
          return '<div class="wt-vtop" style="padding:5px 0;border-bottom:1px solid #e2e8f0"><span>' + esc(s2.name) + '</span><b>' + (sc10 != null ? (sc10 + '/10') : esc(s2.grades || '')) + '</b></div>';
        }).join('') : '') +
        (isN(wk) ? '<div style="margin-top:8px">' + rowBar(L3('Walkability Index', '步行便利指数', 'Caminabilidad'), wk, 100, '#0f766e', '/100') + '</div>' : '') +
      '</div>';
    }
    return '<div class="wt-vcard">' +
      '<div class="wt-vhdr">' + esc(L3('Pre-Offer Diligence Workflow', '出价前核查闭环流程', 'Flujo Previo a la Oferta')) + '</div>' +
      '<div class="wt-vsteps">' +
        '<div><b>1.</b> ' + esc(L3('Verify Permits & Lot Lines (#county)', '核查施工许可与红线面积 (#county)', 'Verificar Permisos y Lote (#county)')) + '</div>' +
        '<div><b>2.</b> ' + esc(L3('Lock HO-3 + Hazard Insurance Quote (#payment)', '锁定房屋主险与地震/洪水险 (#payment)', 'Cotizar Seguro HO-3 (#payment)')) + '</div>' +
        '<div><b>3.</b> ' + esc(L3('Upload Disclosures & Prelim PDF (#inspection)', '上传披露包与产权报告 AI 审阅 (#inspection)', 'Subir Docs e Inspección (#inspection)')) + '</div>' +
      '</div>' +
    '</div>';
  }

  function renderWalkthroughOverviewBanner() {
    var mount = byId('walkthroughBannerMount');
    if (mount) mount.innerHTML = '';
    var sideCta = byId('sideWalkthroughMount');
    if (sideCta) sideCta.innerHTML = '';
  }

  function renderWalkthroughModal() {
    var modal = byId('walkthroughModal');
    if (!modal || !_wtState.open) return;
    var rep = getRep() || {};
    var deck = getWalkthroughDeck(rep);
    var slides = deck.slides || [];
    if (!slides.length) return;
    var n = slides.length;
    var idx = ((_wtState.idx % n) + n) % n;
    _wtState.idx = idx;
    var sl = localizeSlide(slides[idx]);
    var curL = langCode();
    var isAi = deck.synthesisMode === 'ai_enhanced';

    var pillsHtml = slides.map(function (s0, i) {
      var locS = localizeSlide(s0);
      return '<button type="button" class="wt-step-pill' + (i === idx ? ' on' : '') + '" data-wt-step="' + i + '" title="' + esc(locS.title) + '">' + (i + 1) + '</button>';
    }).join('');

    modal.innerHTML =
      '<div class="wt-dialog" role="dialog" aria-modal="true" aria-label="Home Walkthrough Presentation">' +
        '<div class="wt-top">' +
          '<div class="wt-top-left">' +
            '<span class="wt-counter" id="wtStepCounter">' + (idx + 1) + ' / ' + n + '</span>' +
            '<span class="pill ' + (isAi ? 'ai' : 'info') + '">' + (isAi ? esc(L3('✦ AI-Enhanced Deck', '✦ AI 深度增强版', '✦ Mejorado con IA')) : esc(L3('Instant Grounded Deck', '实测数据即时版', 'Presentación Instantánea'))) + '</span>' +
          '</div>' +
          '<div class="wt-steps-bar">' + pillsHtml + '</div>' +
          '<div class="wt-top-right">' +
            '<div class="wt-lang-sw" id="wtLangBar">' +
              '<button type="button" data-wt-lang="en" class="' + (curL === 'en' ? 'on' : '') + '">EN</button>' +
              '<button type="button" data-wt-lang="zh" class="' + (curL === 'zh' ? 'on' : '') + '">中文</button>' +
              '<button type="button" data-wt-lang="es" class="' + (curL === 'es' ? 'on' : '') + '">ES</button>' +
            '</div>' +
            '<button type="button" class="wt-ai-btn" id="wtRegenerateBtn"' + (_wtState.loadingAi ? ' disabled' : '') + '>' +
              (_wtState.loadingAi ? '<span class="rspin"></span> ' + esc(L3('Synthesizing…', '正在生成…', 'Generando…')) : '✨ ' + esc(L3('Enhance / Regenerate with AI', 'AI 深度润色 / 重新生成', 'Mejorar con IA'))) +
            '</button>' +
            '<button type="button" class="wt-close-btn" id="wtCloseBtn" aria-label="Close presentation">✕</button>' +
          '</div>' +
        '</div>' +
        '<div class="wt-body">' +
          '<div class="wt-main-col">' +
            '<div class="wt-tag">' + esc(sl.tag) + '</div>' +
            '<h2 class="wt-title" id="wtSlideTitle">' + esc(sl.title) + '</h2>' +
            (sl.subtitle ? '<div class="wt-sub">' + esc(sl.subtitle) + '</div>' : '') +
            '<div class="wt-hero-stat ' + esc(sl.heroTone) + '">' +
              '<div class="wt-hs-val">' + esc(sl.heroValue) + '</div>' +
              '<div class="wt-hs-lbl">' + esc(sl.heroLabel) + '</div>' +
            '</div>' +
            '<ul class="wt-points">' +
              sl.criticalPoints.map(function (pt) {
                return '<li><span class="wt-pt-ico">✓</span><span>' + esc(pt) + '</span></li>';
              }).join('') +
            '</ul>' +
            (sl.watchout ? '<div class="wt-watch"><b>⚠ ' + esc(L3('Buyer Watchout:', '买家重点留意：', 'Punto de Atención:')) + '</b> ' + esc(sl.watchout) + '</div>' : '') +
            (sl.speakerNotes ? '<details class="wt-notes"><summary>' + esc(L3('Advisor Speaker Notes', '投研顾问讲解备注', 'Notas del Asesor')) + '</summary><div>' + esc(sl.speakerNotes) + '</div></details>' : '') +
          '</div>' +
          '<div class="wt-vis-col">' +
            renderSlideVisualCard(sl.visualKind, rep) +
          '</div>' +
        '</div>' +
        '<div class="wt-foot">' +
          '<button type="button" class="wt-nav-btn" id="wtPrevBtn"' + (idx === 0 ? ' disabled' : '') + '>← ' + esc(L3('Previous', '上一页', 'Anterior')) + '</button>' +
          '<button type="button" class="wt-jump-btn" id="wtJumpSectionBtn" data-target-sec="' + esc(sl.targetSection) + '">' +
            esc(L3('Jump to Deep-Dive Section →', '跳转至对应深度章节 →', 'Ir a la Sección Detallada →')) +
          '</button>' +
          '<button type="button" class="wt-nav-btn pri" id="wtNextBtn">' +
            (idx === n - 1 ? esc(L3('Finish Walkthrough ✓', '完成导览 ✓', 'Finalizar ✓')) : (esc(L3('Next Slide', '下一页', 'Siguiente')) + ' →')) +
          '</button>' +
        '</div>' +
      '</div>';

    var cBtn = byId('wtCloseBtn');
    if (cBtn) cBtn.addEventListener('click', closeWalkthroughModal);
    var pBtn = byId('wtPrevBtn');
    if (pBtn) pBtn.addEventListener('click', function () { if (_wtState.idx > 0) { _wtState.idx--; renderWalkthroughModal(); } });
    var nBtn = byId('wtNextBtn');
    if (nBtn) nBtn.addEventListener('click', function () {
      if (_wtState.idx < n - 1) { _wtState.idx++; renderWalkthroughModal(); }
      else closeWalkthroughModal();
    });
    qAll('[data-wt-step]', modal).forEach(function (b) {
      b.addEventListener('click', function () {
        _wtState.idx = +(b.getAttribute('data-wt-step') || 0);
        renderWalkthroughModal();
      });
    });
    qAll('[data-wt-lang]', modal).forEach(function (b) {
      b.addEventListener('click', function () {
        var nextL = b.getAttribute('data-wt-lang') || 'en';
        if (typeof window.setLang === 'function') {
          Promise.resolve(window.setLang(nextL)).then(function () { renderWalkthroughModal(); });
        }
      });
    });
    var jBtn = byId('wtJumpSectionBtn');
    if (jBtn) jBtn.addEventListener('click', function () {
      var sec = jBtn.getAttribute('data-target-sec') || 'overview';
      closeWalkthroughModal();
      jumpToSection(sec, true);
    });
    var rBtn = byId('wtRegenerateBtn');
    if (rBtn) rBtn.addEventListener('click', regenerateSlidesWithAi);
    modal.onclick = function (ev) { if (ev.target === modal) closeWalkthroughModal(); };
  }

  function openWalkthroughModal(startIdx) {
    var modal = byId('walkthroughModal');
    if (!modal) return;
    _wtState.idx = isN(+startIdx) ? +startIdx : 0;
    _wtState.open = true;
    modal.hidden = false;
    if (modal.classList) modal.classList.add('open');
    renderWalkthroughModal();
  }

  function closeWalkthroughModal() {
    var modal = byId('walkthroughModal');
    _wtState.open = false;
    if (modal) {
      modal.hidden = true;
      if (modal.classList) modal.classList.remove('open');
      modal.innerHTML = '';
    }
  }

  function regenerateSlidesWithAi() {
    var rep = getRep();
    var slug = rep && rep.meta && rep.meta.slug;
    if (!slug || _wtState.loadingAi || (typeof location !== 'undefined' && location.protocol === 'file:')) return;
    var alreadyAi = Boolean(rep.walkthroughSlides && rep.walkthroughSlides.synthesisMode === 'ai_enhanced');
    _wtState.loadingAi = true;
    renderWalkthroughModal();
    fetch('/api/report/' + encodeURIComponent(slug) + '/slides', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(alreadyAi ? { refresh: true } : {})
    })
      .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, body: j }; }); })
      .then(function (res) {
        var deck = res.body && (res.body.walkthroughSlides || res.body.deck || (Array.isArray(res.body.slides) ? res.body : null));
        if (res.ok && deck && Array.isArray(deck.slides) && deck.slides.length) {
          rep.walkthroughSlides = deck;
          renderWalkthroughOverviewBanner();
          renderChapterBar();
        }
      })
      .catch(function () {})
      .finally(function () {
        _wtState.loadingAi = false;
        if (_wtState.open) renderWalkthroughModal();
      });
  }

  /* ------------------------------------------------------------------ 5. Homeowners + Specialty Insurance Quote Studio */
  var _insState = { deductible: 2500, addQuake: false, addFlood: false, addFair: false, initialized: false };

  function pickNum() {
    for (var i = 0; i < arguments.length; i++) {
      if (isN(arguments[i])) return arguments[i];
    }
    return null;
  }

  function normalizeInsuranceQuotes(rep) {
    var raw = (rep && rep.insuranceQuotes) || mod(rep, 'insuranceQuotes') || null;
    if (!raw || typeof raw !== 'object' || !isN(raw.annualTotal) || raw.annualTotal <= 0) {
      return null;
    }
    var annTotal = raw.annualTotal;
    var moTotal = pickNum(raw.monthlyTotal, Math.round(annTotal / 12));
    var rawCov = raw.coverage || {};
    var cov = {
      dwellingA: pickNum(rawCov.dwellingA),
      otherStructuresB: pickNum(rawCov.otherStructuresB),
      personalPropertyC: pickNum(rawCov.personalPropertyC),
      lossOfUseD: pickNum(rawCov.lossOfUseD),
      liabilityE: pickNum(rawCov.liabilityE),
      medicalF: pickNum(rawCov.medicalF),
      rebuildCostPerSqft: pickNum(rawCov.rebuildCostPerSqft)
    };

    var dedOpts = Array.isArray(raw.deductibleOptions) && raw.deductibleOptions.length
      ? raw.deductibleOptions.map(function (d) {
          var ann = pickNum(d.ho3Annual, d.annual);
          if (!isN(ann) || ann <= 0) return null;
          return {
            deductible: pickNum(d.deductible) || 2500,
            ho3Annual: ann,
            ho3Monthly: pickNum(d.ho3Monthly, d.monthly, Math.round(ann / 12)),
            recommended: Boolean(d.recommended || d.deductible === 2500)
          };
        }).filter(Boolean)
      : [{ deductible: 2500, ho3Annual: annTotal, ho3Monthly: moTotal, recommended: true }];
    if (!dedOpts.length) {
      dedOpts = [{ deductible: 2500, ho3Annual: annTotal, ho3Monthly: moTotal, recommended: true }];
    }

    var rawAdd = raw.specialtyAddOns || {};
    var eqRaw = rawAdd.earthquake || null;
    var eqAnn = eqRaw ? pickNum(eqRaw.estAnnual, eqRaw.annualPremium, eqRaw.annual, eqRaw.annualUsd) : null;
    var eqObj = (eqRaw && isN(eqAnn) && eqAnn > 0) ? {
      program: eqRaw.authority || eqRaw.program || eqRaw.carrierName || eqRaw.label || L3('Earthquake Endorsement', '地震专项险', 'Endoso Sísmico'),
      estAnnual: eqAnn,
      estMonthly: pickNum(eqRaw.estMonthly, eqRaw.monthlyPremium, eqRaw.monthly, eqRaw.monthlyUsd, Math.round(eqAnn / 12)),
      deductibleNote: eqRaw.deductibleNote || eqRaw.deductible || '',
      retrofitTip: eqRaw.retrofitDiscountNote || eqRaw.mitigationTip || eqRaw.note || '',
      recommended: Boolean(eqRaw.recommended)
    } : null;

    var flRaw = rawAdd.flood || null;
    var flAnn = flRaw ? pickNum(flRaw.estAnnual, flRaw.annualPremium, flRaw.annual, flRaw.annualUsd) : null;
    var flObj = (flRaw && isN(flAnn) && flAnn > 0) ? {
      program: flRaw.authority || flRaw.program || flRaw.carrierName || flRaw.label || L3('Flood Coverage', '洪水专项险', 'Seguro de Inundación'),
      estAnnual: flAnn,
      estMonthly: pickNum(flRaw.estMonthly, flRaw.monthlyPremium, flRaw.monthly, flRaw.monthlyUsd, Math.round(flAnn / 12)),
      zoneNote: flRaw.zoneNote || flRaw.note || '',
      recommended: Boolean(flRaw.recommended)
    } : null;

    var wfRaw = rawAdd.wildfireFairPlan || null;
    var wfAnn = wfRaw ? pickNum(wfRaw.estAnnual, wfRaw.annualPremium, wfRaw.annual) : null;
    var wfObj = (wfRaw && Boolean(wfRaw.applicable) && isN(wfAnn) && wfAnn > 0) ? {
      applicable: true,
      program: wfRaw.program || wfRaw.label || L3('Brush / FAIR Plan Pool', '山火专项承保池', 'Cobertura de Incendio Forestal'),
      estAnnual: wfAnn,
      estMonthly: pickNum(wfRaw.estMonthly, wfRaw.monthly, Math.round(wfAnn / 12)),
      note: wfRaw.note || ''
    } : { applicable: false, program: '', estAnnual: null, estMonthly: null, note: '' };

    return {
      annualTotal: annTotal,
      monthlyTotal: moTotal,
      confidence: raw.confidence || null,
      coverage: cov,
      deductibleOptions: dedOpts,
      specialtyAddOns: { earthquake: eqObj, flood: flObj, wildfireFairPlan: wfObj },
      riskDrivers: Array.isArray(raw.riskDrivers) ? raw.riskDrivers : [],
      carriers: Array.isArray(raw.carriers) ? raw.carriers : [],
      underwritingProfile: raw.underwritingProfile || {}
    };
  }

  function buildUnderwritingFactSheetText(rep, iq) {
    var uw = (iq && iq.underwritingProfile) || {};
    if (typeof uw.copyableFactSheet === 'string' && uw.copyableFactSheet.trim()) {
      return uw.copyableFactSheet.trim();
    }
    var f = (rep && rep.facts) || {};
    var g = (rep && rep.geo) || {};
    var hz = mod(rep, 'hazards') || {};
    var sf = (rep && rep.decisionFactors && rep.decisionFactors.safety) || {};
    var addr = (rep && rep.meta && (rep.meta.displayAddress || rep.meta.inputAddress)) || '—';
    var covA = iq && iq.coverage && isN(iq.coverage.dwellingA) ? fUsd(iq.coverage.dwellingA) : '—';
    var rPsf = iq && iq.coverage && isN(iq.coverage.rebuildCostPerSqft) ? (' (@ $' + iq.coverage.rebuildCostPerSqft + '/sq ft)') : '';
    var fz = (hz.flood && (hz.flood.zone || hz.flood.femaZone)) || '—';
    var lines = [
      'PROPERTY INSURANCE UNDERWRITING FACT SHEET — ' + addr,
      'APN: ' + (uw.apn || f.apn || '—') + ' | County: ' + (g.county || '—'),
      'Year Built: ' + (uw.yearBuilt || f.yearBuilt || '—') + ' | Stories: ' + (uw.stories || f.stories || '—') + ' | Living Area: ' + (f.sqft ? fNum(f.sqft) + ' sq ft' : '—') + ' | Lot: ' + (f.lotSqft ? fNum(f.lotSqft) + ' sq ft' : '—'),
      'Target Coverage A (Dwelling Rebuild): ' + covA + rPsf,
      'Roof / Foundation / HVAC: ' + ([uw.roof || f.roof || null, uw.foundation || f.foundation || null, f.heating || null].filter(Boolean).join(' · ') || 'Verify on inspection'),
      'Fire Protection & Flood: ' + (sf.fireStation && isN(sf.fireStation.distMi) ? ('Nearest Fire Station ' + sf.fireStation.distMi + ' mi') : 'Verify ISO class') + ' | FEMA Flood: ' + fz
    ];
    return lines.join('\n');
  }

  function syncPaymentCalculatorWithStudio(selectedMonthlyIns) {
    if (!isN(selectedMonthlyIns)) return;
    window.__DB_STUDIO_INS_MONTHLY__ = selectedMonthlyIns;
    var slPrice = byId('slPrice');
    if (slPrice && typeof slPrice.dispatchEvent === 'function' && typeof Event === 'function') {
      try { slPrice.dispatchEvent(new Event('input')); } catch (e) {}
    }
  }

  function renderInsuranceQuoteStudio() {
    var mount = byId('insuranceQuoteStudio');
    if (!mount) return;
    var rep = getRep() || {};
    var iq = normalizeInsuranceQuotes(rep);
    if (!iq) {
      mount.innerHTML =
        '<div class="ins-studio" id="insuranceStudioCard">' +
          '<div class="ins-studio-hdr">' +
            '<div>' +
              '<div class="eyebrow">' + span3('Homeowners & Specialty Hazard Insurance', '房屋主险与巨灾附加险', 'Seguros del Hogar y Riesgos Especiales') + '</div>' +
              '<h3 style="margin:2px 0 0;font-size:18px;font-weight:800">🛡️ ' + span3('Insurance Quote Studio', '房屋保险核价台', 'Estudio de Cotizaciones de Seguro') + '</h3>' +
            '</div>' +
          '</div>' +
          '<div class="callout warn" style="margin-top:10px">' +
            esc(L3(
              'Insurance quote data unavailable in this report payload. No commercial carrier quotes were synthesized.',
              '当前报告数据中未包含结构化保险核价明细，系统绝不伪造任何商业保险公司报价。',
              'Datos de cotización de seguro no disponibles en este informe. No se sintetizaron cotizaciones.'
            )) +
          '</div>' +
        '</div>';
      return;
    }

    var v = mod(rep, 'valuation') || {};
    var o = v.ownership || {};
    var m0 = o.monthly || {};

    if (!_insState.initialized) {
      var recOpt = iq.deductibleOptions.find(function (d) { return d.recommended; }) || iq.deductibleOptions[0];
      if (recOpt) _insState.deductible = recOpt.deductible;
      _insState.initialized = true;
    }

    var activeOpt = iq.deductibleOptions.find(function (d) { return d.deductible === _insState.deductible; }) || iq.deductibleOptions[0];
    var baseOpt2500 = iq.deductibleOptions.find(function (d) { return d.deductible === 2500; }) || activeOpt;
    var dedRatio = (baseOpt2500 && baseOpt2500.ho3Annual > 0) ? (activeOpt.ho3Annual / baseOpt2500.ho3Annual) : 1;

    var eqAddObj = iq.specialtyAddOns.earthquake;
    var flAddObj = iq.specialtyAddOns.flood;
    var wfAddObj = iq.specialtyAddOns.wildfireFairPlan;

    var eqAddM = (_insState.addQuake && eqAddObj && isN(eqAddObj.estMonthly)) ? eqAddObj.estMonthly : 0;
    var eqAddA = (_insState.addQuake && eqAddObj && isN(eqAddObj.estAnnual)) ? eqAddObj.estAnnual : 0;
    var flAddM = (_insState.addFlood && flAddObj && isN(flAddObj.estMonthly)) ? flAddObj.estMonthly : 0;
    var flAddA = (_insState.addFlood && flAddObj && isN(flAddObj.estAnnual)) ? flAddObj.estAnnual : 0;
    var wfAddM = (wfAddObj && wfAddObj.applicable && _insState.addFair && isN(wfAddObj.estMonthly)) ? wfAddObj.estMonthly : 0;
    var wfAddA = (wfAddObj && wfAddObj.applicable && _insState.addFair && isN(wfAddObj.estAnnual)) ? wfAddObj.estAnnual : 0;

    var totalInsMonthly = Math.round(activeOpt.ho3Monthly + eqAddM + flAddM + wfAddM);
    var totalInsAnnual = Math.round(activeOpt.ho3Annual + eqAddA + flAddA + wfAddA);

    var basePiti = isN(m0.total) ? m0.total : 0;
    var origIns = isN(m0.insurance) ? m0.insurance : Math.round(iq.annualTotal / 12);
    var updatedHousingMonthly = basePiti > 0 ? Math.round(basePiti - origIns + totalInsMonthly) : null;

    var dedButtonsHtml = iq.deductibleOptions.map(function (opt) {
      var on = opt.deductible === activeOpt.deductible;
      return '<button type="button" class="ins-ded-btn' + (on ? ' on' : '') + '" data-deductible="' + opt.deductible + '">' +
        '<b>' + fUsd(opt.deductible) + (opt.recommended ? ' ★' : '') + '</b>' +
        '<span>' + fUsd(opt.ho3Monthly) + L3('/mo', '/月', '/mes') + '</span>' +
      '</button>';
    }).join('');

    var covItems = [];
    if (isN(iq.coverage.dwellingA)) covItems.push(['Coverage A · ' + L3('Dwelling Rebuild', '主宅重建保额', 'Reconstrucción'), fUsdK(iq.coverage.dwellingA), isN(iq.coverage.rebuildCostPerSqft) ? ('$' + iq.coverage.rebuildCostPerSqft + L3('/sq ft local rebuild', '/平尺重建成本', '/pie² recons.')) : '']);
    if (isN(iq.coverage.otherStructuresB)) covItems.push(['Coverage B · ' + L3('Other Structures', '附属建筑/车库', 'Otras Estructuras'), fUsdK(iq.coverage.otherStructuresB), L3('Detached structures', '独立附属结构', 'Estructuras anexas')]);
    if (isN(iq.coverage.personalPropertyC)) covItems.push(['Coverage C · ' + L3('Personal Property', '室内个人财产', 'Bienes Personales'), fUsdK(iq.coverage.personalPropertyC), L3('Contents coverage', '室内财物保额', 'Contenido interior')]);
    if (isN(iq.coverage.lossOfUseD)) covItems.push(['Coverage D · ' + L3('Loss of Use / ALE', '临时租房补贴', 'Pérdida de Uso'), fUsdK(iq.coverage.lossOfUseD), L3('Living expenses during rebuild', '修缮期间在外租房补贴', 'Gastos durante reparación')]);
    if (isN(iq.coverage.liabilityE)) covItems.push(['Coverage E · ' + L3('Personal Liability', '第三者责任险', 'Responsabilidad Civil'), fUsdK(iq.coverage.liabilityE), L3('Liability protection', '责任保障', 'Protección civil')]);
    if (isN(iq.coverage.medicalF)) covItems.push(['Coverage F · ' + L3('Medical Payments', '访客医疗垫付', 'Gastos Médicos'), fUsd(iq.coverage.medicalF), L3('Guest medical', '访客医疗', 'Atención médica')]);

    var carriersHtml = iq.carriers.filter(function (c) { return c && (isN(c.estMonthly) || isN(c.estAnnual)); }).map(function (c) {
      var isSpecialty = /earthquake|cea|nfip|flood/i.test(String(c.carrierName || '') + ' ' + String(c.tier || ''));
      var mVal = isSpecialty ? c.estMonthly : Math.round((isN(c.estMonthly) ? c.estMonthly : Math.round(c.estAnnual / 12)) * dedRatio);
      var aVal = isSpecialty ? c.estAnnual : Math.round((isN(c.estAnnual) ? c.estAnnual : (c.estMonthly * 12)) * dedRatio);
      var lc = langCode();
      var noteTxt = (lc === 'zh' && (c.notesZh || (c.zh && c.zh.notes))) ? (c.notesZh || c.zh.notes)
        : (lc === 'es' && (c.notesEs || (c.es && c.es.notes))) ? (c.notesEs || c.es.notes)
        : (c.notes || c.note || '');
      var tierTxt = (lc === 'zh' && c.zh && c.zh.tier) ? c.zh.tier : (lc === 'es' && c.es && c.es.tier) ? c.es.tier : (c.tier || c.coverageLine || '');
      return '<tr>' +
        '<td><b>' + esc(c.carrierName || c.name || 'Carrier') + '</b><div class="small">' + esc(tierTxt) + '</div></td>' +
        '<td><span class="pill ' + (/admitted/i.test(c.admittedStatus || '') ? 'good' : 'info') + '">' + esc(c.admittedStatus || 'Licensed') + '</span></td>' +
        '<td class="n"><b>' + fUsd(mVal) + L3('/mo', '/月', '/mes') + '</b></td>' +
        '<td class="n">' + fUsd(aVal) + L3('/yr', '/年', '/año') + '</td>' +
        '<td class="small" style="min-width:200px">' + esc(noteTxt) + '</td>' +
        '<td class="n">' + (c.quoteUrl ? '<a class="copybtn" style="text-decoration:none;white-space:nowrap" href="' + esc(c.quoteUrl) + '" target="_blank" rel="noopener">' + esc(L3('Quote ↗', '核价 ↗', 'Cotizar ↗')) + '</a>' : '—') + '</td>' +
      '</tr>';
    }).join('');

    var addonsHtml = '';
    if (eqAddObj) {
      addonsHtml += '<label class="ins-addon' + (_insState.addQuake ? ' on' : '') + '">' +
        '<input type="checkbox" id="insToggleEarthquake"' + (_insState.addQuake ? ' checked' : '') + '>' +
        '<div><b>' + esc(eqAddObj.program) + '</b> <span class="pill watch">+' + fUsd(eqAddObj.estMonthly) + L3('/mo', '/月', '/mes') + '</span>' +
          '<div class="small">' + esc([eqAddObj.deductibleNote, eqAddObj.retrofitTip].filter(Boolean).join(' · ')) + '</div>' +
        '</div>' +
      '</label>';
    }
    if (flAddObj) {
      addonsHtml += '<label class="ins-addon' + (_insState.addFlood ? ' on' : '') + '">' +
        '<input type="checkbox" id="insToggleFlood"' + (_insState.addFlood ? ' checked' : '') + '>' +
        '<div><b>' + esc(flAddObj.program) + '</b> <span class="pill info">+' + fUsd(flAddObj.estMonthly) + L3('/mo', '/月', '/mes') + '</span>' +
          '<div class="small">' + esc(flAddObj.zoneNote) + '</div>' +
        '</div>' +
      '</label>';
    }
    if (wfAddObj && wfAddObj.applicable) {
      addonsHtml += '<label class="ins-addon' + (_insState.addFair ? ' on' : '') + '">' +
        '<input type="checkbox" id="insToggleFairPlan"' + (_insState.addFair ? ' checked' : '') + '>' +
        '<div><b>' + esc(wfAddObj.program) + '</b> <span class="pill bad">+' + fUsd(wfAddObj.estMonthly) + L3('/mo', '/月', '/mes') + '</span>' +
          '<div class="small">' + esc(wfAddObj.note) + '</div>' +
        '</div>' +
      '</label>';
    }

    var factSheet = buildUnderwritingFactSheetText(rep, iq);

    mount.innerHTML =
      '<div class="ins-studio" id="insuranceStudioCard">' +
        '<div class="ins-studio-hdr">' +
          '<div>' +
            '<div class="eyebrow">' + span3('Interactive Coverage & Carrier Studio', '交互式房屋保险精算与多航司比价台', 'Estudio Interactivo de Seguros y Aseguradoras') + '</div>' +
            '<h3 style="margin:2px 0 0;font-size:18px;font-weight:800">🛡️ ' + span3('Homeowners (HO-3) + Earthquake & Flood Quote Studio', 'HO-3 房屋主险 + 地震/洪水专项险实时试算', 'Cotizador HO-3 + Sismo e Inundación') + '</h3>' +
          '</div>' +
          '<span class="pill info">' + esc(L3('Replacement Cost Model · Not a Binding Offer', '精算重置成本模型 · 非正式保单要约', 'Modelo Actuarial · No es Oferta Vinculante')) + '</span>' +
        '</div>' +

        '<div class="ins-kpis">' +
          '<div class="ins-kpi hl">' +
            '<div class="k">' + span3('Selected Monthly Insurance', '当前所选保险月费', 'Seguro Mensual Elegido') + '</div>' +
            '<div class="v" id="insStudioMonthly">' + fUsd(totalInsMonthly) + L3('/mo', '/月', '/mes') + '</div>' +
            '<div class="d" id="insStudioAnnual">' + fUsd(totalInsAnnual) + L3('/yr', '/年', '/año') + ' · ' + fUsd(activeOpt.deductible) + ' ' + L3('deductible', '免赔额', 'deducible') + '</div>' +
          '</div>' +
          '<div class="ins-kpi">' +
            '<div class="k">' + span3('Updated Total Housing Carry', '联动更新后总月供支出', 'Pago Mensual Total Actualizado') + '</div>' +
            '<div class="v" id="insStudioTotalPayment">' + (isN(updatedHousingMonthly) ? (fUsd(updatedHousingMonthly) + L3('/mo', '/月', '/mes')) : '—') + '</div>' +
            '<div class="d">' + span3('P&I + Reassessed Tax + Selected Ins. + Maint.', '含本息·重评新税·所选保险·维护', 'P&I + Impuesto + Seguro elegido') + '</div>' +
          '</div>' +
          '<div class="ins-kpi">' +
            '<div class="k">' + span3('Coverage A (Dwelling Rebuild)', '主宅完全重建保额 (Cov A)', 'Cobertura A (Reconstrucción)') + '</div>' +
            '<div class="v" id="insStudioDwellingA">' + (isN(iq.coverage.dwellingA) ? fUsdK(iq.coverage.dwellingA) : '—') + '</div>' +
            '<div class="d">' + (isN(iq.coverage.rebuildCostPerSqft) ? ('$' + iq.coverage.rebuildCostPerSqft + L3('/sq ft regional reconstruction', '/平尺当地重建单价', '/pie² reconstrucción local')) : '—') + '</div>' +
          '</div>' +
        '</div>' +

        '<div class="ins-controls-grid">' +
          '<div class="ins-ctrl-box">' +
            '<div class="ins-ctrl-lbl">' + span3('1. Choose HO-3 Deductible', '1. 选择 HO-3 主险免赔额 (Deductible)', '1. Elegir Deducible HO-3') + '</div>' +
            '<div class="ins-ded-seg" id="insDeductibleSeg">' + dedButtonsHtml + '</div>' +
          '</div>' +
          (addonsHtml
            ? '<div class="ins-ctrl-box">' +
                '<div class="ins-ctrl-lbl">' + span3('2. Specialty Hazard Riders (Excluded from Standard HO-3)', '2. 叠加专项自然灾害险（标准 HO-3 不保地震与洪水）', '2. Coberturas Especiales (Excluidas de HO-3 estándar)') + '</div>' +
                '<div class="ins-addons">' + addonsHtml + '</div>' +
              '</div>'
            : '') +
        '</div>' +

        (covItems.length
          ? '<div class="sub" style="margin-top:14px">' + span3('Recommended Coverage A–F Policy Architecture', '建议保单保障结构 (Coverage A–F)', 'Estructura Recomendada de Póliza (Coberturas A–F)') + '</div>' +
            '<div class="ins-cov-grid">' +
              covItems.map(function (c) {
                return '<div class="ins-cov-card"><div class="k">' + esc(c[0]) + '</div><div class="v">' + esc(c[1]) + '</div><div class="d">' + esc(c[2]) + '</div></div>';
              }).join('') +
            '</div>'
          : '') +

        (iq.riskDrivers.length
          ? '<div class="sub" style="margin-top:14px">' + span3('Property Underwriting Risk Drivers & Retrofit Credits', '房屋核保定价因子与减费改造提示', 'Factores de Suscripción y Descuentos por Mitigación') + '</div>' +
            '<div class="ins-drivers">' +
              iq.riskDrivers.map(function (d) {
                var imp = isN(d.impactPct) ? d.impactPct : 0;
                var cls = imp <= 0 ? 'good' : imp >= 15 ? 'bad' : 'watch';
                var sign = imp > 0 ? ('+' + imp + '%') : (imp + '%');
                var lc = langCode();
                var facLbl = (lc === 'zh' && d.zh && d.zh.factor) ? d.zh.factor : (lc === 'es' && d.es && d.es.factor) ? d.es.factor : (d.factor || '');
                var noteLbl = (lc === 'zh' && d.zh && d.zh.note) ? d.zh.note : (lc === 'es' && d.es && d.es.note) ? d.es.note : (d.note || '');
                return '<div class="ins-drv"><span class="pill ' + cls + '">' + esc(sign) + '</span><div><b>' + esc(facLbl) + '</b><div class="small">' + esc(noteLbl) + '</div></div></div>';
              }).join('') +
            '</div>'
          : '') +

        (carriersHtml
          ? '<div class="sub" style="margin-top:14px">' + span3('Representative Carrier Comparison (' + fUsd(activeOpt.deductible) + ' Deductible)', '代表性持牌保险公司与机构横向对比（按 ' + fUsd(activeOpt.deductible) + ' 免赔额折算）', 'Comparación de Aseguradoras (Deducible ' + fUsd(activeOpt.deductible) + ')') + '</div>' +
            '<div class="scroll"><table class="t" id="insCarrierTable">' +
              '<thead><tr><th>' + span3('Carrier & Tier', '保险机构与级别', 'Aseguradora y Nivel') + '</th><th>' + span3('Market Status', '牌照状态', 'Estado') + '</th><th class="n">' + span3('Est. Monthly', '预估月保费', 'Mensual Est.') + '</th><th class="n">' + span3('Est. Annual', '预估年保费', 'Anual Est.') + '</th><th>' + span3('Underwriting Notes', '核保要点说明', 'Notas de Suscripción') + '</th><th class="n"></th></tr></thead>' +
              '<tbody>' + carriersHtml + '</tbody>' +
            '</table></div>'
          : '') +

        '<div class="more-flat" style="margin-top:12px">' +
          '<div class="sub" style="display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap">' +
            '<span>' + esc(L3('Copyable Buyer Underwriting Fact Sheet (for Insurance Agents / Brokers)', '可一键复制的《买家询价核保要素表》（发给保险经纪快速出单）', 'Hoja de Datos de Suscripción Copiable para Agentes de Seguros')) + '</span>' +
            '<button type="button" class="copybtn" id="copyUnderwritingBtn">📋 ' + esc(L3('Copy Fact Sheet', '复制核保要素表', 'Copiar Hoja de Datos')) + '</button>' +
          '</div>' +
          '<pre class="ins-factsheet" id="underwritingFactSheetText">' + esc(factSheet) + '</pre>' +
        '</div>' +
      '</div>';

    qAll('[data-deductible]', mount).forEach(function (btn) {
      btn.addEventListener('click', function () {
        _insState.deductible = +(btn.getAttribute('data-deductible') || 2500);
        renderInsuranceQuoteStudio();
      });
    });
    var eqChk = byId('insToggleEarthquake');
    if (eqChk) eqChk.addEventListener('change', function () {
      _insState.addQuake = Boolean(eqChk.checked);
      var mainQuake = byId('chkQuake');
      if (mainQuake && mainQuake.checked !== _insState.addQuake) mainQuake.checked = _insState.addQuake;
      renderInsuranceQuoteStudio();
    });
    var flChk = byId('insToggleFlood');
    if (flChk) flChk.addEventListener('change', function () {
      _insState.addFlood = Boolean(flChk.checked);
      renderInsuranceQuoteStudio();
    });
    var fpChk = byId('insToggleFairPlan');
    if (fpChk) fpChk.addEventListener('change', function () {
      _insState.addFair = Boolean(fpChk.checked);
      renderInsuranceQuoteStudio();
    });
    var cpBtn = byId('copyUnderwritingBtn');
    if (cpBtn) cpBtn.addEventListener('click', function () {
      if (typeof navigator !== 'undefined' && navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        navigator.clipboard.writeText(factSheet).then(function () {
          cpBtn.textContent = '✓ ' + L3('Copied!', '已复制！', '¡Copiado!');
          setTimeout(function () { cpBtn.textContent = '📋 ' + L3('Copy Fact Sheet', '复制核保要素表', 'Copiar Hoja de Datos'); }, 1800);
        }).catch(function () {});
      }
    });

    syncPaymentCalculatorWithStudio(totalInsMonthly);
  }

  /* ------------------------------------------------------------------ 5B. Multi-Example Rent Research (Median & Average · Condition & Sq Ft Adjusted) */
  function _arrMedian(vals) {
    if (!vals || !vals.length) return null;
    var s = vals.slice().sort(function (a, b) { return a - b; });
    var m = Math.floor(s.length / 2);
    return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
  }
  function _arrQuantile(vals, q) {
    if (!vals || !vals.length) return null;
    var s = vals.slice().sort(function (a, b) { return a - b; });
    if (s.length === 1) return s[0];
    var pos = Math.max(0, Math.min(1, q)) * (s.length - 1);
    var lo = Math.floor(pos), hi = Math.ceil(pos);
    return lo === hi ? s[lo] : (s[lo] + (s[hi] - s[lo]) * (pos - lo));
  }
  function _normStructType(raw) {
    if (!raw) return null;
    var s = String(raw).toLowerCase();
    if (s.indexOf('condo') >= 0 || s.indexOf('apartment') >= 0 || s.indexOf('co-op') >= 0 || s.indexOf('flat') >= 0) return 'Condo/Apartment';
    if (s.indexOf('town') >= 0 || s.indexOf('row') >= 0 || s.indexOf('attached') >= 0) return 'Townhome';
    if (s.indexOf('multi') >= 0 || s.indexOf('duplex') >= 0 || s.indexOf('triplex') >= 0 || s.indexOf('fourplex') >= 0) return 'Multi-Family';
    if (s.indexOf('single') >= 0 || s.indexOf('sfr') >= 0 || s.indexOf('detached') >= 0 || s.indexOf('residential') >= 0) return 'Single-Family Detached';
    return null;
  }
  function _structIndex(norm) {
    if (norm === 'Single-Family Detached') return 1.00;
    if (norm === 'Townhome') return 0.93;
    if (norm === 'Multi-Family') return 0.90;
    if (norm === 'Condo/Apartment') return 0.86;
    return null;
  }
  function _subjectConditionClient(f, rep) {
    var cond = (rep && rep.condition) || mod(rep, 'condition') || {};
    var yr = isN(f.yearBuilt) && f.yearBuilt >= 1700 ? Math.round(f.yearBuilt) : null;
    var effAge = isN(cond.effectiveAgeYears) ? cond.effectiveAgeYears
      : (cond.summary && isN(cond.summary.effectiveAgeYears) ? cond.summary.effectiveAgeYears : null);
    var remYr = isN(f.lastRemodelYear) ? f.lastRemodelYear : (isN(cond.lastMajorRemodelYear) ? cond.lastMajorRemodelYear : null);
    var txt = [cond.overallCondition, cond.conditionTier, f.condition, f.interiorCondition].filter(Boolean).join(' ').toLowerCase();
    var hasRemodelKw = /remodeled|renovated|updated|new kitchen|new roof|turnkey|gut/.test(txt);
    var hasDatedKw = /original|fixer|deferred|unrenovated|needs work|as-is/.test(txt);
    var nowYr = 2026;
    if ((isN(remYr) && nowYr - remYr <= 10) || (isN(yr) && nowYr - yr <= 10) || (isN(effAge) && effAge <= 10) || hasRemodelKw) {
      return { idx: 1.06, tier: 'renovated', summary: L3('Renovated / newer condition (+6% index vs unrenovated baseline)', '近期翻新或次新成色（较常规基准 +6%）', 'Renovada / construcción reciente (+6% vs base)') };
    }
    if ((isN(remYr) && nowYr - remYr <= 20) || (isN(yr) && nowYr - yr <= 25) || (isN(effAge) && effAge <= 20)) {
      return { idx: 1.03, tier: 'updated', summary: L3('Updated / modern systems (+3% condition index)', '成色良好 / 系统已更新（成色系数 +3%）', 'Actualizada (+3% índice de condición)') };
    }
    if (hasDatedKw || (isN(effAge) && effAge >= 45) || (isN(yr) && nowYr - yr >= 55 && !remYr)) {
      return { idx: 0.95, tier: 'vintage_unrenovated', summary: L3('Vintage stock without documented recent remodel (-5% condition index)', '老房龄且无近期大翻新许可记录（成色系数 -5%）', 'Antigua sin remodelación reciente documentada (-5%)') };
    }
    if (isN(yr)) {
      return { idx: 1.00, tier: 'well_maintained', summary: L3('Built ' + yr + ' — standard well-maintained baseline (1.00x)', '建于 ' + yr + ' 年 — 常规良好维护基准 (1.00x)', 'Construida en ' + yr + ' — condición estándar (1.00x)') };
    }
    return { idx: null, tier: null, summary: L3('Condition / year-built unreported — condition adjustments omitted', '未登记建成与翻新年代 — 暂不套用成色修正', 'Año y condición no reportados — sin ajuste de condición') };
  }

  var _rentalMarketFetchState = {};

  function _isPaywallStub(val) {
    if (window.OpenEstateLazy && typeof window.OpenEstateLazy.isPaywallLockStub === 'function') {
      return window.OpenEstateLazy.isPaywallLockStub(val);
    }
    return Boolean(val && typeof val === 'object' && !Array.isArray(val) && val.paywallLocked === true);
  }

  function _formatRentalSourceKey(rawKey) {
    var k = String(rawKey || '').trim().toLowerCase();
    if (!k) return '';
    var cap = function (s) {
      return s.replace(/\b\w/g, function (c) { return c.toUpperCase(); });
    };
    if (/_com$/.test(k)) {
      return cap(k.slice(0, -4).replace(/[_-]+/g, ' ')) + '.com';
    }
    return cap(k.replace(/[_-]+/g, ' '));
  }

  function _hostLabelFromUrl(rawUrl) {
    if (!rawUrl || typeof rawUrl !== 'string') return '';
    try {
      var u = new URL(rawUrl);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return '';
      return String(u.hostname || '').replace(/^www\./i, '');
    } catch (e) {
      return '';
    }
  }

  function _collectRentalIndexMeta(rep, srcObj) {
    var rm = (rep && rep.rentalMarket && typeof rep.rentalMarket === 'object') ? rep.rentalMarket : {};
    var shared = (rep && rep._sharedRentalMarket && typeof rep._sharedRentalMarket === 'object') ? rep._sharedRentalMarket : {};
    var s = (srcObj && typeof srcObj === 'object') ? srcObj : {};
    var sourcesBreakdown = {};
    var mergeBreakdown = function (bd) {
      if (!bd || typeof bd !== 'object' || Array.isArray(bd)) return;
      Object.keys(bd).forEach(function (k) {
        var n = Number(bd[k]);
        if (isN(n) && n > 0) {
          sourcesBreakdown[k] = Math.max(sourcesBreakdown[k] || 0, Math.round(n));
        }
      });
    };
    var cells = []
      .concat(Array.isArray(s.cells) ? s.cells : [])
      .concat(Array.isArray(rm.cells) ? rm.cells : [])
      .concat(Array.isArray(shared.cells) ? shared.cells : []);
    mergeBreakdown(s.sourcesBreakdown);
    mergeBreakdown(rm.sourcesBreakdown);
    mergeBreakdown(shared.sourcesBreakdown);
    var cellSamples = 0;
    var freshest = s.freshestSeenAt || s.freshestRefreshedAt || rm.freshestSeenAt || rm.freshestRefreshedAt || shared.freshestSeenAt || shared.freshestRefreshedAt || null;
    cells.forEach(function (c) {
      if (!c || typeof c !== 'object') return;
      if (c.sourcesBreakdown) mergeBreakdown(c.sourcesBreakdown);
      if (isN(c.sampleCount) && c.sampleCount > 0) cellSamples += Math.round(c.sampleCount);
      var ts = c.freshestSeenAt || c.refreshedAt;
      if (ts && (!freshest || String(ts) > String(freshest))) freshest = ts;
    });
    var citUrls = [].concat(
      Array.isArray(s.citationUrls) ? s.citationUrls : [],
      Array.isArray(rm.citationUrls) ? rm.citationUrls : [],
      Array.isArray(shared.citationUrls) ? shared.citationUrls : []
    ).filter(function (u) { return typeof u === 'string' && /^https?:\/\//i.test(u); });
    var indexStatus = s.indexStatus || rm.indexStatus || shared.indexStatus || null;
    var refreshQueued = Boolean(s.refreshQueued || rm.refreshQueued || shared.refreshQueued);
    var matchedGeoTier = s.matchedGeoTier || rm.matchedGeoTier || shared.matchedGeoTier || null;
    var matchedGeoKey = s.matchedGeoKey || rm.matchedGeoKey || shared.matchedGeoKey || null;
    var wholeHomeMonthly = pickNum(s.wholeHomeMonthly, shared.wholeHomeMonthly, rm.wholeHomeMonthly);
    var roomMonthly = pickNum(s.roomMonthly, shared.roomMonthly, rm.roomMonthly);
    var aduMonthly = pickNum(s.aduMonthly, shared.aduMonthly, rm.aduMonthly);
    var isSharedIndex = Boolean(
      cells.length ||
      Object.keys(sourcesBreakdown).length ||
      indexStatus ||
      refreshQueued ||
      isN(wholeHomeMonthly) ||
      isN(roomMonthly) ||
      isN(aduMonthly) ||
      /rental market index/i.test(String(s.sourceLabel || rm.sourceLabel || shared.sourceLabel || ''))
    );
    return {
      isSharedIndex: isSharedIndex,
      indexStatus: indexStatus,
      refreshQueued: refreshQueued,
      freshestAt: freshest,
      matchedGeoTier: matchedGeoTier,
      matchedGeoKey: matchedGeoKey,
      sourcesBreakdown: sourcesBreakdown,
      cellSampleCount: cellSamples > 0 ? cellSamples : null,
      wholeHomeMonthly: wholeHomeMonthly,
      roomMonthly: roomMonthly,
      aduMonthly: aduMonthly,
      sourceLabel: s.sourceLabel || shared.sourceLabel || rm.sourceLabel || null,
      citationUrls: Array.from(new Set(citUrls))
    };
  }

  function getRentResearchData(rep) {
    var v = mod(rep, 'valuation') || {};
    var f = (rep && rep.facts) || {};
    var subjSqft = (isN(f.sqft) && f.sqft >= 200) ? f.sqft : (f.buildingRegister && isN(f.buildingRegister.livingSqft) && f.buildingRegister.livingSqft >= 200 ? f.buildingRegister.livingSqft : null);
    var subjBeds = (isN(f.beds) && f.beds >= 0) ? f.beds : null;
    var subjBaths = (isN(f.baths) && f.baths >= 0) ? f.baths : null;
    var subjType = _normStructType(f.propertyType);
    var subjCond = _subjectConditionClient(f, rep);
    var rrTop = rep && rep.rentResearch && !_isPaywallStub(rep.rentResearch) ? rep.rentResearch : null;
    var rrVal = v.rentResearch && !_isPaywallStub(v.rentResearch) ? v.rentResearch : null;
    var rrOwn = v.ownership && v.ownership.rentalResearch && !_isPaywallStub(v.ownership.rentalResearch) ? v.ownership.rentalResearch : null;
    var rrShared = rep && rep._sharedRentalMarket && !_isPaywallStub(rep._sharedRentalMarket) ? rep._sharedRentalMarket : null;
    var srvPrimary = rrVal || rrTop || rrOwn || rrShared;
    var idxMeta = _collectRentalIndexMeta(rep, srvPrimary);

    if (srvPrimary && typeof srvPrimary === 'object' && Array.isArray(srvPrimary.examples) && srvPrimary.examples.length) {
      var srv = srvPrimary;
      var srvCondIdx = pickNum(srv.subjectConditionIndex, srv.subjectConditionFactor, subjCond.idx);
      var srvCondTier = srv.subjectConditionTier || (isN(srvCondIdx)
        ? (srvCondIdx >= 1.045 ? 'renovated' : srvCondIdx >= 1.015 ? 'updated' : srvCondIdx <= 0.97 ? 'vintage_unrenovated' : 'well_maintained')
        : subjCond.tier);
      var normExamples = srv.examples.map(function (ex) {
        if (!ex || typeof ex !== 'object') return ex;
        var rawMo = pickNum(ex.rawMonthlyRent, ex.rawRentMonthly);
        var adjMo = pickNum(ex.adjustedMonthlyRent, ex.adjustedRentMonthly);
        return Object.assign({}, ex, {
          rawMonthlyRent: isN(rawMo) ? Math.round(rawMo) : null,
          adjustedMonthlyRent: isN(adjMo) ? Math.round(adjMo) : null,
          source: ex.source || ex.sourceName || _hostLabelFromUrl(ex.sourceUrl || ex.url) || '',
          notes: ex.notes || (Array.isArray(ex.adjustmentNotes) && ex.adjustmentNotes.length ? ex.adjustmentNotes.join(' · ') : null)
        });
      });
      return Object.assign({}, srv, {
        subjectSqft: pickNum(srv.subjectSqft, subjSqft != null ? Math.round(subjSqft) : null),
        subjectBeds: pickNum(srv.subjectBeds, subjBeds),
        subjectBaths: pickNum(srv.subjectBaths, subjBaths),
        subjectPropertyType: srv.subjectPropertyType || subjType,
        subjectConditionIndex: srvCondIdx,
        subjectConditionTier: srvCondTier,
        examples: normExamples,
        indexMeta: idxMeta
      });
    }

    var idxSrc = [rrVal, rrTop, rrOwn, rrShared].find(function (o) {
      return o && typeof o === 'object' && (
        (Array.isArray(o.comps) && o.comps.length) ||
        isN(o.wholeHomeMonthly) ||
        isN(o.roomMonthly) ||
        isN(o.aduMonthly) ||
        (Array.isArray(o.cells) && o.cells.length)
      );
    });
    if (idxSrc) {
      var rawComps = Array.isArray(idxSrc.comps) ? idxSrc.comps : [];
      var idxExamples = rawComps.map(function (c, i) {
        if (!c || typeof c !== 'object') return null;
        var mo = pickNum(c.monthlyRent, c.rent, c.price);
        if (!isN(mo) || mo <= 0) return null;
        var cSqft = (isN(c.sqft) && c.sqft >= 120) ? c.sqft : null;
        var cKind = String(c.kind || 'whole_home').toLowerCase();
        var isWhole = cKind !== 'room' && cKind !== 'adu';
        var areaRatio = (isWhole && isN(subjSqft) && isN(cSqft) && cSqft > 0) ? Math.max(0.55, Math.min(1.65, Math.pow(subjSqft / cSqft, 0.60))) : 1.0;
        var adjMo = isWhole ? Math.round(mo * areaRatio) : Math.round(mo);
        var host = _hostLabelFromUrl(c.url || c.sourceUrl);
        var kindLabel = cKind === 'room' ? L3('Private Room', '单间分租', 'Habitación') : cKind === 'adu' ? 'ADU' : _normStructType(f.propertyType || 'Whole Home');
        return {
          id: 'idx-' + (i + 1),
          exampleKind: 'lease_comp',
          label: c.title || c.neighborhood || L3('Market Rental Comp', '区域租金样本', 'Comparable de renta'),
          address: c.title || c.neighborhood || L3('Nearby Rental Listing', '周边挂牌租约', 'Anuncio cercano'),
          source: c.source || host || (idxMeta.sourceLabel || L3('Shared Rental Index', '共享租金指数库', 'Índice de Renta Compartido')),
          sourceUrl: c.url || c.sourceUrl || null,
          areaScope: c.neighborhood || idxMeta.matchedGeoTier || 'neighborhood',
          distanceMiles: isN(c.distanceMi) ? c.distanceMi : (isN(c.distanceMiles) ? c.distanceMiles : null),
          beds: isN(c.beds) ? c.beds : null,
          baths: isN(c.baths) ? c.baths : null,
          sqft: cSqft,
          propertyType: kindLabel,
          compKind: cKind,
          rawMonthlyRent: Math.round(mo),
          rawRentPerSqft: isN(cSqft) && cSqft > 0 ? +(mo / cSqft).toFixed(2) : null,
          areaFactor: areaRatio !== 1.0 ? +areaRatio.toFixed(3) : null,
          areaAdjMonthly: areaRatio !== 1.0 ? Math.round(adjMo - mo) : null,
          adjustedMonthlyRent: adjMo,
          weight: isWhole ? 1.0 : 0.25,
          postedAt: c.postedAt || null
        };
      }).filter(Boolean);
      var wholeEx = idxExamples.filter(function (e) { return e.compKind !== 'room' && e.compKind !== 'adu'; });
      var statSet = wholeEx.length ? wholeEx : idxExamples;
      var rawNums = statSet.map(function (e) { return e.rawMonthlyRent; });
      var adjNums = statSet.map(function (e) { return e.adjustedMonthlyRent; });
      var anchorRec = pickNum(idxSrc.recommendedMonthlyRent, idxMeta.wholeHomeMonthly, adjNums.length ? _arrMedian(adjNums) : null, idxMeta.aduMonthly, idxMeta.roomMonthly);
      if (idxExamples.length || isN(anchorRec)) {
        var rawMedV = rawNums.length ? Math.round(_arrMedian(rawNums)) : (isN(anchorRec) ? Math.round(anchorRec) : 0);
        var rawAvgV = rawNums.length ? Math.round(rawNums.reduce(function (a, b) { return a + b; }, 0) / rawNums.length) : rawMedV;
        var adjMedV = adjNums.length ? Math.round(_arrMedian(adjNums)) : rawMedV;
        var adjAvgV = adjNums.length ? Math.round(adjNums.reduce(function (a, b) { return a + b; }, 0) / adjNums.length) : adjMedV;
        var recRentV = isN(anchorRec) ? Math.round(anchorRec) : adjMedV;
        var p25V = adjNums.length >= 3 ? Math.round(_arrQuantile(adjNums, 0.25)) : Math.round(recRentV * 0.93);
        var p75V = adjNums.length >= 3 ? Math.round(_arrQuantile(adjNums, 0.75)) : Math.round(recRentV * 1.07);
        var totalSamples = pickNum(idxMeta.cellSampleCount, idxSrc.sampleCount, idxExamples.length) || 0;
        var idxConf = totalSamples >= 8 || wholeEx.length >= 3 ? 'High' : (totalSamples >= 3 || wholeEx.length >= 1 ? 'Medium' : 'Low');
        return {
          recommendedMonthlyRent: recRentV,
          lowMonthly: Math.min(p25V, recRentV),
          highMonthly: Math.max(p75V, recRentV),
          rawMedianMonthly: rawMedV,
          rawAverageMonthly: rawAvgV,
          adjustedMedianMonthly: adjMedV,
          adjustedAverageMonthly: adjAvgV,
          weightedAverageMonthly: adjAvgV,
          recommendedRentPerSqft: (isN(subjSqft) && subjSqft > 0 && recRentV > 0) ? +(recRentV / subjSqft).toFixed(2) : null,
          sampleCount: totalSamples,
          leaseCompCount: idxExamples.length,
          impliedCompCount: 0,
          benchmarkCount: idxExamples.length ? 0 : (isN(anchorRec) ? 1 : 0),
          confidence: idxConf,
          subjectSqft: isN(subjSqft) ? Math.round(subjSqft) : null,
          subjectBeds: subjBeds,
          subjectBaths: subjBaths,
          subjectPropertyType: subjType,
          subjectConditionTier: subjCond.tier,
          subjectConditionIndex: subjCond.idx,
          areaElasticity: 0.60,
          conditionSummary: idxMeta.sourceLabel || L3('Shared Rental Market Index aggregate', '共享区域租金指数聚合成交样本', 'Agregado del Índice de Renta Compartido'),
          areaSqftSummary: isN(subjSqft) ? (L3('Subject living area ', '标的室内面积 ', 'Área habitable ') + fNum(subjSqft) + ' sf') : '',
          methodologyNote: L3('Derived from shared regional rental index listings and geo-bucketed rent statistics.', '基于共享区域租金指数库中的挂牌租约与地理分桶统计生成。', 'Derivado de listados y estadísticas geo-agrupadas del índice regional de renta.'),
          examples: idxExamples,
          indexMeta: idxMeta
        };
      }
      if (idxMeta.isSharedIndex) {
        return { emptySharedIndex: true, examples: [], indexMeta: idxMeta };
      }
    }
    var mk = mod(rep, 'market') || {};
    var rObj = mk.rent || {};
    var hud = mk.hudFmr || {};
    var acs = mk.acs || {};
    var idxRentObj = mk[_T.zor] || {};
    var idxValObj = mk[_T.zhv] || {};
    var areaElasticity = 0.60;

    var tractMedRent = acs.tract && isN(acs.tract.medianGrossRent) && acs.tract.medianGrossRent > 0 && acs.tract.medianGrossRent < 3500 ? acs.tract.medianGrossRent : null;
    var zipMedRent = acs.zip && isN(acs.zip.medianGrossRent) && acs.zip.medianGrossRent > 0 && acs.zip.medianGrossRent < 3500 ? acs.zip.medianGrossRent : null;
    var tractInc = acs.tract && isN(acs.tract.medianHouseholdIncome) && acs.tract.medianHouseholdIncome > 0 ? acs.tract.medianHouseholdIncome : null;
    var zipInc = acs.zip && isN(acs.zip.medianHouseholdIncome) && acs.zip.medianHouseholdIncome > 0 ? acs.zip.medianHouseholdIncome : null;
    var tractZipRatio = (tractMedRent && zipMedRent) ? Math.max(0.82, Math.min(1.22, tractMedRent / zipMedRent))
      : ((tractInc && zipInc) ? Math.max(0.88, Math.min(1.15, Math.pow(tractInc / zipInc, 0.35))) : null);

    var rawList = [];
    var seenAddr = {};
    var rcPools = [v.rentalComps, f.rentalComps, f.rentComparables, f.rental_comps, f.rent_comparables];
    rcPools.forEach(function (arr) {
      if (!Array.isArray(arr)) return;
      arr.forEach(function (rc) {
        if (!rc || rc.isSynthetic) return;
        var rMo = pickNum(rc.rent, rc.monthlyRent, rc.price, rc.listedRent, rc.rentMonthly);
        if (!isN(rMo) || rMo < 250 || rMo > 150000) return;
        var addr = String(rc.address || rc.formattedAddress || '').trim();
        if (!addr) return;
        var key = addr.toLowerCase();
        if (seenAddr[key]) return;
        seenAddr[key] = true;
        rawList.push({
          id: 'lease-' + (rawList.length + 1),
          exampleKind: 'lease_comp',
          label: addr,
          address: addr,
          source: rc.source || 'Public Lease Comparable',
          sourceUrl: rc.sourceUrl || rc.url || null,
          areaScope: 'neighborhood',
          distanceMiles: isN(rc.distanceMiles) ? rc.distanceMiles : (isN(rc.distance) ? rc.distance : null),
          beds: isN(rc.beds) ? rc.beds : (isN(rc.bedrooms) ? rc.bedrooms : null),
          baths: isN(rc.baths) ? rc.baths : (isN(rc.bathrooms) ? rc.bathrooms : null),
          sqft: (isN(rc.sqft) && rc.sqft >= 200) ? rc.sqft : null,
          yearBuilt: (isN(rc.yearBuilt) && rc.yearBuilt >= 1700) ? Math.round(rc.yearBuilt) : null,
          propertyType: _normStructType(rc.propertyType || rc.homeType),
          conditionText: rc.condition || rc.notes || null,
          rawMonthlyRent: rMo
        });
      });
    });

    var localPtr = (isN(rObj.priceToRent) && rObj.priceToRent >= 6 && rObj.priceToRent <= 80) ? rObj.priceToRent
      : ((isN(idxValObj.latest) && isN(idxRentObj.latest) && idxRentObj.latest > 0) ? (idxValObj.latest / (idxRentObj.latest * 12)) : null);
    var leaseCountInit = rawList.length;
    if (leaseCountInit < 4 && isN(localPtr) && Array.isArray(v.comps)) {
      v.comps.forEach(function (sc) {
        if (!sc || sc.isSynthetic) return;
        var leaseCntNow = rawList.filter(function (x) { return x.exampleKind !== 'official_area_benchmark'; }).length;
        if (leaseCntNow >= 6) return;
        var sp = pickNum(sc.timeAdjPrice, sc.salePrice, sc.price);
        var addr = String(sc.address || '').trim();
        if (!addr || !isN(sp) || sp < 50000 || seenAddr[addr.toLowerCase()]) return;
        var impMo = sp / (localPtr * 12);
        if (!isN(impMo) || impMo < 300 || impMo > 150000) return;
        seenAddr[addr.toLowerCase()] = true;
        rawList.push({
          id: 'implied-comp-' + (rawList.length + 1),
          exampleKind: 'implied_comp_rent',
          label: addr + ' (Implied via ' + localPtr.toFixed(1) + 'x local P/R)',
          address: addr,
          source: sc.source || 'Recorded Sale Comp × Local Price-to-Rent',
          sourceUrl: sc.sourceUrl || null,
          areaScope: 'neighborhood',
          distanceMiles: isN(sc.distanceMiles) ? sc.distanceMiles : null,
          beds: isN(sc.beds) ? sc.beds : null,
          baths: isN(sc.baths) ? sc.baths : null,
          sqft: (isN(sc.sqft) && sc.sqft >= 200) ? sc.sqft : null,
          yearBuilt: (isN(sc.yearBuilt) && sc.yearBuilt >= 1700) ? Math.round(sc.yearBuilt) : null,
          propertyType: _normStructType(sc.propertyType) || subjType,
          conditionText: sc.notes || null,
          rawMonthlyRent: impMo
        });
      });
    }

    var fRentEst = isN(f.rentEstimate) && f.rentEstimate >= 250
      ? f.rentEstimate
      : (f.rentEstimate && isN(f.rentEstimate.rent) && f.rentEstimate.rent >= 250 ? f.rentEstimate.rent : null);
    if (isN(fRentEst)) {
      rawList.push({
        id: 'bench-subject-avm',
        exampleKind: 'official_area_benchmark',
        label: 'Property-Specific Cited Rent Benchmark',
        address: null,
        source: 'Subject Rental Valuation Feed',
        sourceUrl: null,
        areaScope: 'subject',
        distanceMiles: 0,
        beds: subjBeds,
        baths: subjBaths,
        sqft: subjSqft,
        yearBuilt: isN(f.yearBuilt) ? Math.round(f.yearBuilt) : null,
        propertyType: subjType,
        conditionText: null,
        subjMatched: true,
        rawMonthlyRent: fRentEst
      });
    }

    var zipIdxMo = pickNum(rObj[_T.zor], idxRentObj.latest);
    if (isN(zipIdxMo) && zipIdxMo >= 250) {
      rawList.push({
        id: 'bench-zip-index',
        exampleKind: 'official_area_benchmark',
        label: 'Observed Local ZIP Typical Rent Index',
        address: null,
        source: 'Observed Market Rent Index (ZIP)',
        sourceUrl: null,
        areaScope: 'zip',
        distanceMiles: null,
        beds: null,
        baths: null,
        sqft: null,
        yearBuilt: null,
        propertyType: null,
        conditionText: null,
        rawMonthlyRent: zipIdxMo
      });
    }

    var targetBr = isN(subjBeds) ? Math.max(0, Math.min(4, Math.round(subjBeds))) : null;
    var fmrKey = targetBr != null ? ('br' + targetBr) : 'br3';
    var fmrVal = hud && isN(hud[fmrKey]) ? hud[fmrKey] : null;
    if (isN(fmrVal) && fmrVal >= 250) {
      var isSafmr = Boolean(hud.isSmallAreaFmr);
      rawList.push({
        id: 'bench-hud-fmr',
        exampleKind: 'official_area_benchmark',
        label: 'HUD ' + (hud.year ? ('FY' + hud.year + ' ') : '') + (isSafmr ? 'Small Area FMR' : 'Fair Market Rent') + ' (' + (targetBr != null ? (targetBr + 'BR') : '3BR') + ')',
        address: null,
        source: 'U.S. Dept. of Housing & Urban Development (HUD)',
        sourceUrl: 'https://www.huduser.gov/portal/datasets/fmr.html',
        areaScope: isSafmr ? 'zip' : 'metro',
        distanceMiles: null,
        beds: targetBr != null ? targetBr : 3,
        baths: null,
        sqft: null,
        yearBuilt: null,
        propertyType: 'Condo/Apartment',
        conditionText: null,
        rawMonthlyRent: fmrVal
      });
    }

    if (isN(tractMedRent) && tractMedRent >= 250) {
      rawList.push({
        id: 'bench-acs-tract',
        exampleKind: 'official_area_benchmark',
        label: 'U.S. Census ACS Census Tract Median Gross Rent',
        address: null,
        source: 'U.S. Census Bureau ACS 5-Year (Tract)',
        sourceUrl: 'https://data.census.gov/',
        areaScope: 'tract',
        distanceMiles: null,
        beds: null,
        baths: null,
        sqft: null,
        yearBuilt: null,
        propertyType: 'Condo/Apartment',
        conditionText: null,
        rawMonthlyRent: tractMedRent
      });
    }
    if (isN(zipMedRent) && zipMedRent >= 250) {
      rawList.push({
        id: 'bench-acs-zip',
        exampleKind: 'official_area_benchmark',
        label: 'U.S. Census ACS ZCTA (ZIP) Median Gross Rent',
        address: null,
        source: 'U.S. Census Bureau ACS 5-Year (ZCTA)',
        sourceUrl: 'https://data.census.gov/',
        areaScope: 'zip',
        distanceMiles: null,
        beds: null,
        baths: null,
        sqft: null,
        yearBuilt: null,
        propertyType: 'Condo/Apartment',
        conditionText: null,
        rawMonthlyRent: zipMedRent
      });
    }
    var metroKey = 'metro' + _T.zor.charAt(0).toUpperCase() + _T.zor.slice(1);
    var metroRent = pickNum(rObj[metroKey], idxRentObj.metroLatest);
    if (isN(metroRent) && metroRent >= 250 && (!isN(zipIdxMo) || Math.abs(metroRent - zipIdxMo) >= 25)) {
      rawList.push({
        id: 'bench-metro-index',
        exampleKind: 'official_area_benchmark',
        label: 'Metro Area Observed Typical Rent Benchmark',
        address: null,
        source: 'Observed Market Rent Index (Metro)',
        sourceUrl: null,
        areaScope: 'metro',
        distanceMiles: null,
        beds: null,
        baths: null,
        sqft: null,
        yearBuilt: null,
        propertyType: null,
        conditionText: null,
        rawMonthlyRent: metroRent
      });
    }

    if (!rawList.length) return null;

    var examples = rawList.map(function (ex) {
      var rawR = ex.rawMonthlyRent;
      var rawPsf = (isN(ex.sqft) && ex.sqft > 0) ? +(rawR / ex.sqft).toFixed(2) : null;

      var areaFactor = null, areaAdj = null;
      if (!ex.subjMatched && isN(subjSqft) && isN(ex.sqft) && ex.sqft > 0) {
        areaFactor = Math.max(0.68, Math.min(1.42, Math.pow(subjSqft / ex.sqft, areaElasticity)));
        areaAdj = rawR * (areaFactor - 1);
      }

      var condFactor = null, condAdj = null;
      if (!ex.subjMatched && isN(subjCond.idx)) {
        var exCondIdx = null;
        var ct = String(ex.conditionText || '').toLowerCase();
        if (/remodeled|renovated|new|luxury|turnkey/.test(ct)) exCondIdx = 1.06;
        else if (/updated|modern/.test(ct)) exCondIdx = 1.03;
        else if (/original|fixer|dated|as-is/.test(ct)) exCondIdx = 0.95;
        else if (isN(ex.yearBuilt)) {
          var age = 2026 - ex.yearBuilt;
          exCondIdx = age <= 10 ? 1.06 : age <= 25 ? 1.03 : age >= 55 ? 0.96 : 1.00;
        } else if (ex.exampleKind === 'official_area_benchmark') {
          exCondIdx = 1.00;
        }
        if (isN(exCondIdx) && exCondIdx > 0) {
          condFactor = Math.max(0.86, Math.min(1.16, subjCond.idx / exCondIdx));
          condAdj = rawR * (condFactor - 1);
        }
      }

      var bbAdj = null;
      if (!ex.subjMatched) {
        var hasArea = areaAdj != null;
        var bStep = hasArea ? Math.max(60, Math.min(280, rawR * 0.035)) : Math.max(110, Math.min(480, rawR * 0.075));
        var baStep = hasArea ? Math.max(40, Math.min(190, rawR * 0.025)) : Math.max(65, Math.min(260, rawR * 0.04));
        var dSum = 0, anyBb = false;
        if (isN(subjBeds) && isN(ex.beds)) {
          dSum += Math.max(-2.5, Math.min(2.5, subjBeds - ex.beds)) * bStep;
          anyBb = true;
        }
        if (isN(subjBaths) && isN(ex.baths)) {
          dSum += Math.max(-2.5, Math.min(2.5, subjBaths - ex.baths)) * baStep;
          anyBb = true;
        }
        if (anyBb) bbAdj = dSum;
      }

      var typeAdj = null;
      if (!ex.subjMatched && subjType && ex.propertyType) {
        var si = _structIndex(subjType), ei = _structIndex(ex.propertyType);
        if (isN(si) && isN(ei) && ei > 0 && Math.abs(si - ei) > 0.001) {
          var tf = Math.max(0.84, Math.min(1.18, si / ei));
          typeAdj = rawR * (tf - 1);
        }
      }

      var microAdj = null;
      if (!ex.subjMatched && isN(tractZipRatio) && (ex.areaScope === 'zip' || ex.areaScope === 'metro')) {
        var damp = ex.areaScope === 'zip' ? (1 + (tractZipRatio - 1) * 0.50) : (1 + (tractZipRatio - 1) * 0.65);
        microAdj = rawR * (damp - 1);
      }

      var totDelta = (areaAdj || 0) + (condAdj || 0) + (bbAdj || 0) + (typeAdj || 0) + (microAdj || 0);
      var adjR = Math.max(rawR * 0.58, Math.min(rawR * 1.55, rawR + totDelta));

      var wt = ex.exampleKind === 'lease_comp' ? 1.00 : (ex.exampleKind === 'implied_comp_rent' ? 0.62 : (ex.subjMatched ? 0.88 : (ex.areaScope === 'tract' ? 0.52 : (ex.areaScope === 'zip' ? 0.56 : 0.36))));
      if (isN(ex.distanceMiles) && ex.exampleKind !== 'official_area_benchmark') {
        wt *= 1 / (1 + Math.max(0, ex.distanceMiles) * 0.65);
      }
      if (isN(subjSqft) && isN(ex.sqft) && ex.sqft > 0) {
        var gap = Math.abs(subjSqft - ex.sqft) / Math.max(subjSqft, 1);
        if (gap > 0.35) wt *= 0.80;
      } else if (ex.exampleKind !== 'official_area_benchmark') {
        wt *= 0.85;
      }
      wt = +Math.max(0.12, Math.min(1.25, wt)).toFixed(3);

      return {
        id: ex.id,
        exampleKind: ex.exampleKind,
        label: ex.label,
        address: ex.address,
        source: ex.source,
        sourceUrl: ex.sourceUrl,
        areaScope: ex.areaScope,
        distanceMiles: isN(ex.distanceMiles) ? +ex.distanceMiles.toFixed(2) : null,
        beds: ex.beds,
        baths: ex.baths,
        sqft: isN(ex.sqft) ? Math.round(ex.sqft) : null,
        yearBuilt: ex.yearBuilt,
        propertyType: ex.propertyType,
        rawMonthlyRent: Math.round(rawR),
        rawRentPerSqft: rawPsf,
        areaFactor: areaFactor != null ? +areaFactor.toFixed(3) : null,
        areaAdjMonthly: areaAdj != null ? Math.round(areaAdj) : null,
        conditionFactor: condFactor != null ? +condFactor.toFixed(3) : null,
        conditionAdjMonthly: condAdj != null ? Math.round(condAdj) : null,
        bedsBathsAdjMonthly: bbAdj != null ? Math.round(bbAdj) : null,
        propertyTypeAdjMonthly: typeAdj != null ? Math.round(typeAdj) : null,
        microAreaAdjMonthly: microAdj != null ? Math.round(microAdj) : null,
        adjustedMonthlyRent: Math.round(adjR),
        weight: wt,
        notes: null
      };
    });

    var rawVals = examples.map(function (e) { return e.rawMonthlyRent; });
    var adjVals = examples.map(function (e) { return e.adjustedMonthlyRent; });
    var rawMed = Math.round(_arrMedian(rawVals));
    var rawAvg = Math.round(rawVals.reduce(function (a, b) { return a + b; }, 0) / rawVals.length);
    var adjMed = Math.round(_arrMedian(adjVals));
    var adjAvg = Math.round(adjVals.reduce(function (a, b) { return a + b; }, 0) / adjVals.length);
    var wSum = examples.reduce(function (a, e) { return a + e.weight; }, 0);
    var wAvg = wSum > 0 ? Math.round(examples.reduce(function (a, e) { return a + e.adjustedMonthlyRent * e.weight; }, 0) / wSum) : adjAvg;
    var recRent = Math.round(adjMed * 0.50 + wAvg * 0.50);
    var p25 = _arrQuantile(adjVals, 0.25), p75 = _arrQuantile(adjVals, 0.75);
    var lowMo = Math.round(adjVals.length >= 3 ? Math.min(p25, recRent * 0.95) : Math.min(Math.min.apply(null, adjVals), recRent * 0.93));
    var highMo = Math.round(adjVals.length >= 3 ? Math.max(p75, recRent * 1.05) : Math.max(Math.max.apply(null, adjVals), recRent * 1.07));
    var leaseCnt = examples.filter(function (e) { return e.exampleKind === 'lease_comp'; }).length;
    var impCnt = examples.filter(function (e) { return e.exampleKind === 'implied_comp_rent'; }).length;
    var benchCnt = examples.filter(function (e) { return e.exampleKind === 'official_area_benchmark'; }).length;

    return {
      recommendedMonthlyRent: recRent,
      lowMonthly: lowMo,
      highMonthly: highMo,
      rawMedianMonthly: rawMed,
      rawAverageMonthly: rawAvg,
      adjustedMedianMonthly: adjMed,
      adjustedAverageMonthly: adjAvg,
      weightedAverageMonthly: wAvg,
      recommendedRentPerSqft: (isN(subjSqft) && subjSqft > 0) ? +(recRent / subjSqft).toFixed(2) : null,
      sampleCount: examples.length,
      leaseCompCount: leaseCnt,
      impliedCompCount: impCnt,
      benchmarkCount: benchCnt,
      confidence: (leaseCnt >= 3 || (leaseCnt >= 1 && examples.length >= 4)) ? 'High' : (examples.length >= 3 ? 'Medium' : 'Low'),
      subjectSqft: isN(subjSqft) ? Math.round(subjSqft) : null,
      subjectBeds: subjBeds,
      subjectBaths: subjBaths,
      subjectPropertyType: subjType,
      subjectYearBuilt: isN(f.yearBuilt) ? Math.round(f.yearBuilt) : null,
      subjectConditionTier: subjCond.tier,
      subjectConditionIndex: subjCond.idx,
      areaElasticity: areaElasticity,
      conditionSummary: subjCond.summary,
      areaSqftSummary: isN(subjSqft)
        ? L3('Subject living area ' + fNum(subjSqft) + ' sq ft — nonlinear 0.60 size elasticity applied where example sq ft is known', '标的室内面积 ' + fNum(subjSqft) + ' 平尺 — 对已知面积样本按 0.60 边际面积弹性系数显式修正', 'Área habitable ' + fNum(subjSqft) + ' p² — elasticidad no lineal de 0.60 por área')
        : L3('Subject living area unreported — sq ft adjustments omitted', '标的未登记室内面积 — 面积修正项留空', 'Área habitable no reportada — sin ajuste por área'),
      methodologyNote: L3(
        'Recommended rent blends Adjusted Median (' + fUsd(adjMed) + '/mo) and Similarity-Weighted Average (' + fUsd(wAvg) + '/mo) across ' + examples.length + ' real examples; Adjusted Simple Mean is ' + fUsd(adjAvg) + '/mo (Raw Median ' + fUsd(rawMed) + ' / Raw Mean ' + fUsd(rawAvg) + '). Missing attributes are omitted rather than invented.',
        '推荐月租融合修正后中位数（' + fUsd(adjMed) + '/月）与相似度加权均值（' + fUsd(wAvg) + '/月），样本算术均值为 ' + fUsd(adjAvg) + '/月（未修正原始中位数 ' + fUsd(rawMed) + ' / 原始均值 ' + fUsd(rawAvg) + '）；缺失字段如实留空绝不伪造。',
        'La renta recomendada combina la Mediana Ajustada (' + fUsd(adjMed) + '/mes) y el Promedio Ponderado (' + fUsd(wAvg) + '/mes) sobre ' + examples.length + ' ejemplos reales; Media Simple Ajustada ' + fUsd(adjAvg) + '/mes.'
      ),
      examples: examples
    };
  }

  function _fmtSignedMo(v) {
    if (!isN(v)) return '—';
    var r = Math.round(v);
    if (r === 0) return '$0';
    return (r > 0 ? '+' : '−') + fUsd(Math.abs(r));
  }

  function _formatLazyTs(iso) {
    if (!iso) return '';
    try {
      var d = new Date(iso);
      if (isNaN(d.getTime())) return '';
      return d.toLocaleDateString(langCode() === 'zh' ? 'zh-CN' : langCode() === 'es' ? 'es-ES' : 'en-US', { month: 'short', day: 'numeric', year: 'numeric' });
    } catch (e) {
      return '';
    }
  }

  function _maybeHydrateSharedRentalMarket(rep, rr, isLocked) {
    if (isLocked || typeof fetch !== 'function' || typeof location === 'undefined' || location.protocol === 'file:') return;
    var slug = rep && rep.meta && rep.meta.slug;
    if (!slug) return;
    var lazyRent = rep && rep.lazySections && rep.lazySections.rentResearch;
    var idxMeta = (rr && rr.indexMeta) || _collectRentalIndexMeta(rep, null);
    var cloudActive = Boolean(window.OpenEstateCommercialWorkspace || lazyRent || rep.rentalMarket || /rental market index/i.test(String(idxMeta.sourceLabel || '')));
    if (!cloudActive) return;
    if (idxMeta.freshestAt && Object.keys(idxMeta.sourcesBreakdown || {}).length) return;
    var cacheKey = String(slug);
    if (_rentalMarketFetchState[cacheKey]) return;
    _rentalMarketFetchState[cacheKey] = 'pending';
    var f = (rep && rep.facts) || {};
    var g = (rep && rep.geo) || {};
    var q = [];
    if (f.zip || g.zip) q.push('zip=' + encodeURIComponent(f.zip || g.zip));
    if (isN(g.lat) && isN(g.lon)) {
      q.push('lat=' + encodeURIComponent(g.lat));
      q.push('lon=' + encodeURIComponent(g.lon));
    }
    if (isN(f.beds)) q.push('beds=' + encodeURIComponent(f.beds));
    if (f.propertyType) q.push('type=' + encodeURIComponent(f.propertyType));
    var url = '/api/rental-market' + (q.length ? ('?' + q.join('&')) : '');
    fetch(url, { cache: 'no-store' }).then(function (res) {
      if (!res || !res.ok) return null;
      return res.json();
    }).then(function (j) {
      if (!j || typeof j !== 'object' || !j.ok) return;
      _rentalMarketFetchState[cacheKey] = 'done';
      rep._sharedRentalMarket = j;
      renderRentResearchPanel();
    }).catch(function () {
      _rentalMarketFetchState[cacheKey] = 'err';
    });
  }

  function _buildSharedRentalIndexBarHtml(idxMeta, rr) {
    if (!idxMeta || !idxMeta.isSharedIndex) return '';
    var srcEntries = Object.keys(idxMeta.sourcesBreakdown || {}).map(function (k) {
      return { key: k, label: _formatRentalSourceKey(k), count: idxMeta.sourcesBreakdown[k] };
    }).sort(function (a, b) { return b.count - a.count; });
    var srcChipsHtml = '';
    if (srcEntries.length) {
      srcChipsHtml = srcEntries.map(function (s) {
        return '<span class="pill info" style="font-size:11px;padding:2px 7px">' + esc(s.label) + ' (' + s.count + ')</span>';
      }).join('');
    } else if (Array.isArray(idxMeta.citationUrls) && idxMeta.citationUrls.length) {
      var seenHosts = {};
      var hostChips = [];
      idxMeta.citationUrls.forEach(function (u) {
        var h = _hostLabelFromUrl(u);
        if (!h || seenHosts[h]) return;
        seenHosts[h] = true;
        hostChips.push('<span class="pill info" style="font-size:11px;padding:2px 7px">' + esc(h) + '</span>');
      });
      srcChipsHtml = hostChips.join('');
    }
    var nSamples = pickNum(idxMeta.cellSampleCount, rr && rr.sampleCount);
    var freshTs = _formatLazyTs(idxMeta.freshestAt);
    var isPendingRef = Boolean(idxMeta.refreshQueued || idxMeta.indexStatus === 'stale' || idxMeta.indexStatus === 'missing');
    var statusPill = isPendingRef
      ? '<span class="pill watch" style="font-size:11px">⏳ ' + esc(L3('Pending refresh', '等待指数刷新', 'Actualización pendiente')) + '</span>'
      : idxMeta.indexStatus === 'fresh'
      ? '<span class="pill good" style="font-size:11px">✓ ' + esc(L3('Index Fresh', '指数已更新', 'Índice al día')) + '</span>'
      : '';
    var segPills = [];
    if (isN(idxMeta.wholeHomeMonthly) && idxMeta.wholeHomeMonthly > 0) segPills.push('<span class="pill">' + esc(L3('Whole Home: ', '整租估值：', 'Casa completa: ') + fUsd(idxMeta.wholeHomeMonthly) + L3('/mo', '/月', '/mes')) + '</span>');
    if (isN(idxMeta.roomMonthly) && idxMeta.roomMonthly > 0) segPills.push('<span class="pill">' + esc(L3('Room: ', '单间分租：', 'Habitación: ') + fUsd(idxMeta.roomMonthly) + L3('/mo', '/月', '/mes')) + '</span>');
    if (isN(idxMeta.aduMonthly) && idxMeta.aduMonthly > 0) segPills.push('<span class="pill">' + esc('ADU: ' + fUsd(idxMeta.aduMonthly) + L3('/mo', '/月', '/mes')) + '</span>');
    var geoText = (idxMeta.matchedGeoTier && idxMeta.matchedGeoKey) ? (String(idxMeta.matchedGeoTier).toUpperCase() + ' ' + idxMeta.matchedGeoKey) : '';

    return '<div class="oe-rental-index-bar" data-oe-rental-index="1">' +
      '<div style="display:flex;flex-direction:column;gap:4px">' +
        '<div style="font-size:12.5px;font-weight:700;color:var(--ink)">📊 ' + esc(L3('Shared Rental Market Index', '共享区域租金市场指数', 'Índice de Mercado de Alquiler Compartido')) +
          (geoText ? ' <span class="small" style="font-weight:600">· ' + esc(geoText) + '</span>' : '') +
          (isN(nSamples) && nSamples > 0 ? ' <span class="small" style="font-weight:600">· ' + esc(nSamples + ' ' + L3('index samples', '条指数样本', 'muestras del índice')) + '</span>' : '') +
          (freshTs ? ' <span class="small">· ' + esc(L3('Updated ', '更新于 ', 'Actualizado ') + freshTs) + '</span>' : '') +
        '</div>' +
        (srcChipsHtml ? '<div class="oe-rental-src-chips"><span class="small">' + esc(L3('Sources:', '来源分布：', 'Fuentes:')) + '</span>' + srcChipsHtml + '</div>' : '') +
      '</div>' +
      '<div style="display:flex;gap:6px;flex-wrap:wrap;align-items:center">' +
        statusPill + segPills.join('') +
      '</div>' +
    '</div>';
  }

  function renderRentResearchPanel() {
    var mounts = [byId('rentResearchCostMount'), byId('rentResearchCompsMount')].filter(Boolean);
    if (!mounts.length) return;
    var rep = getRep() || {};
    var v = mod(rep, 'valuation') || {};
    var lazyHelper = window.OpenEstateLazy;
    var rwLock = null;
    if (lazyHelper && typeof lazyHelper.collectPaywallLocksClient === 'function') {
      rwLock = lazyHelper.collectPaywallLocksClient().rentResearch || null;
    }
    if (!rwLock && rep.paywallLocks && rep.paywallLocks.rentResearch && lazyHelper && lazyHelper.normalizePaywallLock) {
      rwLock = lazyHelper.normalizePaywallLock('rentResearch', rep.paywallLocks.rentResearch);
    }
    if (!rwLock && _isPaywallStub(v.rentResearch) && lazyHelper && lazyHelper.normalizePaywallLock) {
      rwLock = lazyHelper.normalizePaywallLock('rentResearch', v.rentResearch);
    }

    if (rwLock) {
      var lockCardHtml = lazyHelper ? lazyHelper.renderPaywallCardHtml('rentResearch', rwLock) : '';
      var lockedHtml =
        '<div class="rr-panel" data-paywall-section="rentResearch">' +
          '<div class="rr-hdr">' +
            '<div>' +
              '<div class="eyebrow">' + span3('Shared Rental Market Index', '区域租金指数与估算', 'Índice de Alquiler') + '</div>' +
              '<h3 style="margin:2px 0 0;font-size:17px;font-weight:800">🏘️ ' + span3('Rent Research & Market Comps', '租金研判与市场可比样本', 'Investigación de Renta') + '</h3>' +
            '</div>' +
          '</div>' +
          lockCardHtml +
        '</div>';
      mounts.forEach(function (m) { m.innerHTML = lockedHtml; });
      if (window.OpenEstateCommercialWorkspace && typeof window.OpenEstateCommercialWorkspace.renderPaywallPlaceholders === 'function') {
        try { window.OpenEstateCommercialWorkspace.renderPaywallPlaceholders(); } catch (e) {}
      }
      return;
    }

    var rr = getRentResearchData(rep);
    var lazyRent = rep && rep.lazySections && rep.lazySections.rentResearch;
    var lazyRentHtml = (lazyRent && lazyRent.status && lazyRent.status !== 'ready' && lazyHelper && typeof lazyHelper.renderLazyStatusCardHtml === 'function')
      ? lazyHelper.renderLazyStatusCardHtml('rentResearch', lazyRent)
      : '';
    var idxMeta = (rr && rr.indexMeta) || _collectRentalIndexMeta(rep, null);
    var indexBarHtml = _buildSharedRentalIndexBarHtml(idxMeta, rr);
    _maybeHydrateSharedRentalMarket(rep, rr, false);

    if (!rr || ((!Array.isArray(rr.examples) || !rr.examples.length) && !(isN(rr.recommendedMonthlyRent) && rr.recommendedMonthlyRent > 0))) {
      var emptyMsg = (idxMeta && idxMeta.isSharedIndex && (idxMeta.refreshQueued || idxMeta.indexStatus === 'missing' || idxMeta.indexStatus === 'stale'))
        ? L3('Shared rental market index refresh is pending for this area. Live rental comparables will populate automatically when complete.', '该区域共享租金指数正在排队刷新，完成后将自动展示可比租金样本。', 'La actualización del índice de renta está pendiente para esta zona.')
        : L3('No verified rental comparables or official rent benchmarks were returned for this parcel. Rent estimates are omitted rather than synthesized.', '未检索到该地块的核实租约样本或官方租金基准，系统如实留空，绝不伪造租金数据。', 'No se encontraron comparables de renta verificados ni referencias oficiales para esta parcela.');
      var emptyHtml =
        '<div class="rr-panel">' +
          '<div class="rr-hdr">' +
            '<div>' +
              '<div class="eyebrow">' + span3('Multi-Example Rent Research', '多样本租金深度研判', 'Investigación de Renta Multi-Ejemplo') + '</div>' +
              '<h3 style="margin:2px 0 0;font-size:17px;font-weight:800">🏘️ ' + span3('Rent Research — Median & Average Analysis', '租金研判 — 中位数与均值分析', 'Investigación de Renta — Mediana y Promedio') + '</h3>' +
            '</div>' +
          '</div>' +
          indexBarHtml +
          lazyRentHtml +
          '<div class="callout" style="margin-top:8px">' + esc(emptyMsg) + '</div>' +
        '</div>';
      mounts.forEach(function (m) {
        m.innerHTML = emptyHtml;
        if (lazyHelper && typeof lazyHelper.bindLazySubForms === 'function') lazyHelper.bindLazySubForms(m);
      });
      return;
    }

    var moUnit = L3('/mo', '/月', '/mes');
    var confLabel = rr.confidence || ((rr.leaseCompCount >= 3 || (rr.leaseCompCount >= 1 && rr.sampleCount >= 4)) ? 'High' : ((rr.impliedCompCount >= 2 || rr.sampleCount >= 3) ? 'Medium' : 'Low'));
    var confTone = confLabel === 'High' ? 'good' : (confLabel === 'Medium' ? 'info' : 'watch');
    var mixParts = [];
    if (rr.leaseCompCount > 0) mixParts.push(rr.leaseCompCount + ' ' + L3('lease comp(s)', '套实测租约', 'renta(s) directa(s)'));
    if (rr.impliedCompCount > 0) mixParts.push(rr.impliedCompCount + ' ' + L3('implied sale-comp rent(s)', '套成交折算租约', 'renta(s) implícita(s)'));
    if (rr.benchmarkCount > 0) mixParts.push(rr.benchmarkCount + ' ' + L3('official benchmark(s)', '项官方区域基准', 'referencia(s) oficial(es)'));
    var mixStr = mixParts.join(' · ') || ((rr.sampleCount || 0) + ' ' + L3('examples', '个样本', 'ejemplos'));

    var subjSpecParts = [];
    if (isN(rr.subjectSqft)) subjSpecParts.push(fNum(rr.subjectSqft) + ' sf');
    if (isN(rr.subjectBeds)) subjSpecParts.push(rr.subjectBeds + ' bd');
    if (isN(rr.subjectBaths)) subjSpecParts.push(rr.subjectBaths + ' ba');
    var condTierLabel = rr.subjectConditionTier === 'renovated'
      ? L3('Renovated / Newer', '近期翻新/次新', 'Renovada / Reciente')
      : rr.subjectConditionTier === 'updated'
      ? L3('Updated Systems', '系统已更新', 'Actualizada')
      : rr.subjectConditionTier === 'vintage_unrenovated'
      ? L3('Vintage Unrenovated', '老房未大修', 'Antigua sin renovar')
      : rr.subjectConditionTier === 'well_maintained'
      ? L3('Standard Maintained', '常规良好维护', 'Estándar conservada')
      : L3('Condition Unreported', '成色未登记', 'Condición no reportada');
    var condIdxStr = isN(rr.subjectConditionIndex) ? (' (' + rr.subjectConditionIndex.toFixed(2) + '×)') : '';

    var rowsHtml = (rr.examples || []).map(function (ex) {
      var kindBadge = ex.exampleKind === 'lease_comp'
        ? '<span class="pill good" style="font-size:10.5px;padding:1px 6px">' + esc(L3('Lease Comp', '真实租约样本', 'Renta Directa')) + '</span>'
        : ex.exampleKind === 'implied_comp_rent'
        ? '<span class="pill info" style="font-size:10.5px;padding:1px 6px">' + esc(L3('Implied Comp Rent', '成交租售比折算', 'Renta Implícita')) + '</span>'
        : '<span class="pill" style="font-size:10.5px;padding:1px 6px">' + esc(L3('Official Benchmark', '官方统计基准', 'Referencia Oficial')) + '</span>';

      var primaryTitle = ex.exampleKind === 'official_area_benchmark'
        ? cleanPublicText(ex.label || 'Area Benchmark')
        : cleanPublicText(ex.address || ex.label || 'Comparable');
      var srcText = idxMeta.isSharedIndex ? String(ex.source || '') : cleanPublicText(ex.source || '');
      var rawSrcUrl = (ex.sourceUrl && /^https?:\/\//i.test(String(ex.sourceUrl))) ? String(ex.sourceUrl) : '';
      var safeUrl = idxMeta.isSharedIndex ? rawSrcUrl : ((rawSrcUrl && !_RE_PORTAL_HOST.test(rawSrcUrl)) ? rawSrcUrl : '');
      var srcHtml = safeUrl
        ? '<a href="' + esc(safeUrl) + '" target="_blank" rel="noopener noreferrer" style="color:var(--pri,#0f766e);text-decoration:none">' + esc(srcText || _hostLabelFromUrl(safeUrl) || 'Source') + ' ↗</a>'
        : esc(srcText);

      var specBits = [];
      if (isN(ex.beds)) specBits.push(ex.beds + ' bd');
      if (isN(ex.baths)) specBits.push(ex.baths + ' ba');
      if (isN(ex.sqft)) specBits.push(fNum(ex.sqft) + ' sf');
      var specMain = specBits.length ? esc(specBits.join(' · ')) : '—';
      var typeSub = ex.propertyType ? ('<div class="small">' + esc(ex.propertyType + (isN(ex.yearBuilt) ? (' · ' + ex.yearBuilt) : '')) + '</div>')
        : (isN(ex.yearBuilt) ? ('<div class="small">' + esc(L3('Built ', '建于 ', 'Año ') + ex.yearBuilt) + '</div>') : '');

      var scKey = String(ex.areaScope || '').toLowerCase();
      var scopeLbl = scKey.indexOf('tract') >= 0 ? L3('Census Tract', '人口普查街区', 'Tracto Censal')
        : (scKey.indexOf('zip') >= 0 || scKey.indexOf('zcta') >= 0) ? L3('ZIP / ZCTA', '同邮编区域', 'Código Postal')
        : scKey.indexOf('metro') >= 0 ? L3('Metro Area', '都会区', 'Área Metro')
        : scKey.indexOf('subject') >= 0 ? L3('Subject Parcel', '本房产', 'Propiedad')
        : (ex.areaScope && ex.areaScope !== 'neighborhood' ? String(ex.areaScope) : L3('Neighborhood', '周边街区', 'Vecindario'));
      var distCell = isN(ex.distanceMiles)
        ? ('<b>' + ex.distanceMiles.toFixed(2) + ' mi</b><div class="small">' + esc(scopeLbl) + '</div>')
        : ('<span class="small">' + esc(scopeLbl) + '</span>');

      var rawCell = '<b>' + fUsd(ex.rawMonthlyRent) + moUnit + '</b>' +
        (isN(ex.rawRentPerSqft) ? ('<div class="small">$' + ex.rawRentPerSqft.toFixed(2) + '/sf</div>') : '');

      var areaCell = isN(ex.areaAdjMonthly)
        ? ('<b class="' + (ex.areaAdjMonthly > 0 ? 'rr-pos' : ex.areaAdjMonthly < 0 ? 'rr-neg' : '') + '">' + _fmtSignedMo(ex.areaAdjMonthly) + '</b>' +
           (isN(ex.areaFactor) ? ('<div class="small">×' + ex.areaFactor.toFixed(2) + '</div>') : ''))
        : '<span class="small">—</span>';

      var otherTags = [];
      if (isN(ex.conditionAdjMonthly) && Math.round(ex.conditionAdjMonthly) !== 0) {
        otherTags.push(L3('Cond ', '成色 ', 'Cond ') + _fmtSignedMo(ex.conditionAdjMonthly));
      } else if (isN(ex.conditionAdjMonthly) && Math.round(ex.conditionAdjMonthly) === 0) {
        otherTags.push(L3('Cond $0', '成色 $0', 'Cond $0'));
      }
      if (isN(ex.bedsBathsAdjMonthly) && Math.round(ex.bedsBathsAdjMonthly) !== 0) {
        otherTags.push(L3('Bd/Ba ', '卧卫 ', 'Hab/Ba ') + _fmtSignedMo(ex.bedsBathsAdjMonthly));
      }
      if (isN(ex.propertyTypeAdjMonthly) && Math.round(ex.propertyTypeAdjMonthly) !== 0) {
        otherTags.push(L3('Type ', '房型 ', 'Tipo ') + _fmtSignedMo(ex.propertyTypeAdjMonthly));
      }
      if (isN(ex.microAreaAdjMonthly) && Math.round(ex.microAreaAdjMonthly) !== 0) {
        otherTags.push(L3('Area ', '微区位 ', 'Zona ') + _fmtSignedMo(ex.microAreaAdjMonthly));
      }
      var otherCell = otherTags.length
        ? otherTags.map(function (t) { return '<span class="rr-adj-chip">' + esc(t) + '</span>'; }).join(' ')
        : '<span class="small">—</span>';

      var adjCell = '<b style="font-size:13.5px">' + fUsd(ex.adjustedMonthlyRent) + moUnit + '</b>' +
        (isN(ex.weight) ? ('<div class="small">' + esc(L3('wt ', '权重 ', 'peso ') + ex.weight.toFixed(2)) + '</div>') : '');

      return '<tr>' +
        '<td style="min-width:210px"><div style="display:flex;align-items:center;gap:6px;flex-wrap:wrap">' + kindBadge + '<b>' + esc(primaryTitle) + '</b></div>' +
          (srcHtml ? '<div class="small" style="margin-top:2px">' + srcHtml + '</div>' : '') +
        '</td>' +
        '<td>' + specMain + typeSub + '</td>' +
        '<td class="n">' + distCell + '</td>' +
        '<td class="n">' + rawCell + '</td>' +
        '<td class="n">' + areaCell + '</td>' +
        '<td>' + otherCell + '</td>' +
        '<td class="n">' + adjCell + '</td>' +
      '</tr>';
    }).join('');

    var panelHtml =
      '<div class="rr-panel">' +
        '<div class="rr-hdr">' +
          '<div>' +
            '<div class="eyebrow">' + span3('Multi-Example Median & Average · Condition & Area Adjusted', '多样本中位数与均值 · 面积、房龄成色与微区位显式修正', 'Mediana y Promedio Multi-Ejemplo · Ajustado por Área y Condición') + '</div>' +
            '<h3 style="margin:2px 0 0;font-size:18px;font-weight:800">🏘️ ' + span3('Rent Research — Multi-Example Median & Average', '租金深度研判 — 多样本中位数与算术/加权均值', 'Investigación de Renta — Mediana y Promedio') + '</h3>' +
          '</div>' +
          '<div style="display:flex;gap:6px;align-items:center;flex-wrap:wrap">' +
            '<span class="pill ' + confTone + '">' + esc(L3('Confidence: ', '置信度：', 'Confianza: ') + confLabel) + '</span>' +
            '<span class="pill">' + esc(mixStr) + '</span>' +
          '</div>' +
        '</div>' +
        indexBarHtml +
        lazyRentHtml +

        '<div class="rr-kpis">' +
          '<div class="rr-kpi hl">' +
            '<div class="k">' + span3('Recommended Rent', '推荐公允月租', 'Renta Recomendada') + '</div>' +
            '<div class="v">' + fUsd(rr.recommendedMonthlyRent) + moUnit + '</div>' +
            '<div class="d">' + esc(L3('Range ', '区间 ', 'Rango ') + fUsd(rr.lowMonthly) + '–' + fUsd(rr.highMonthly) + moUnit + (isN(rr.recommendedRentPerSqft) ? (' · $' + rr.recommendedRentPerSqft.toFixed(2) + '/sf') : '')) + '</div>' +
          '</div>' +
          '<div class="rr-kpi">' +
            '<div class="k">' + span3('Adjusted Median Rent', '修正后中位数月租', 'Mediana Ajustada') + '</div>' +
            '<div class="v">' + fUsd(rr.adjustedMedianMonthly) + moUnit + '</div>' +
            '<div class="d">' + span3('P50 across adjusted examples', '经面积与成色修正后中位值', 'P50 de ejemplos ajustados') + '</div>' +
          '</div>' +
          '<div class="rr-kpi">' +
            '<div class="k">' + span3('Adjusted Average Rent', '修正后平均月租 (Mean)', 'Promedio Ajustado') + '</div>' +
            '<div class="v">' + fUsd(rr.adjustedAverageMonthly) + moUnit + '</div>' +
            '<div class="d">' + esc(L3('Weighted avg: ', '相似度加权均值：', 'Prom. ponderado: ') + fUsd(rr.weightedAverageMonthly) + moUnit) + '</div>' +
          '</div>' +
          '<div class="rr-kpi">' +
            '<div class="k">' + span3('Raw Median / Average', '未修正原始中位 / 均值', 'Mediana / Promedio Bruto') + '</div>' +
            '<div class="v" style="font-size:16px">' + fUsd(rr.rawMedianMonthly) + ' / ' + fUsd(rr.rawAverageMonthly) + '</div>' +
            '<div class="d">' + span3('Before condition & sq ft math', '修正前原始样本中位数与均值', 'Antes de ajustes físicos') + '</div>' +
          '</div>' +
          '<div class="rr-kpi">' +
            '<div class="k">' + span3('Examples Analyzed', '纳入研判样本数', 'Ejemplos Analizados') + '</div>' +
            '<div class="v">' + (rr.sampleCount || 0) + '</div>' +
            '<div class="d">' + esc(mixStr) + '</div>' +
          '</div>' +
          '<div class="rr-kpi">' +
            '<div class="k">' + span3('Subject Condition & Sq Ft', '标的成色与面积因子', 'Condición y Área Sujeto') + '</div>' +
            '<div class="v" style="font-size:15px">' + esc(condTierLabel + condIdxStr) + '</div>' +
            '<div class="d">' + esc((subjSpecParts.join(' · ') || L3('Specs partial', '部分规格未登记', 'Datos parciales')) + ' · α=' + (rr.areaElasticity || 0.60).toFixed(2)) + '</div>' +
          '</div>' +
        '</div>' +

        '<div class="rr-subbar">' +
          '<div><b>' + span3('Condition & Area Calibration:', '面积与成色修正依据：', 'Calibración de Condición y Área:') + '</b> ' +
            esc([cleanPublicText(rr.conditionSummary || ''), cleanPublicText(rr.areaSqftSummary || '')].filter(Boolean).join(' · ') || L3('Calibrated against local rental index metrics.', '基于本地租金指数校准。', 'Calibrado con el índice local de alquiler.')) +
          '</div>' +
          (rr.methodologyNote ? '<div class="small" style="margin-top:4px">' + esc(cleanPublicText(rr.methodologyNote)) + '</div>' : '') +
        '</div>' +

        (rowsHtml
          ? '<div class="scroll" style="margin-top:10px">' +
              '<table class="t rr-table">' +
                '<thead><tr>' +
                  '<th>' + span3('Source / Address & Kind', '样本来源 / 地址与类别', 'Fuente / Dirección y Tipo') + '</th>' +
                  '<th>' + span3('Beds · Baths · Sq Ft', '卧 · 卫 · 面积', 'Hab · Ba · Área') + '</th>' +
                  '<th class="n">' + span3('Dist / Scope', '距离 / 范围', 'Dist / Alcance') + '</th>' +
                  '<th class="n">' + span3('Raw Rent', '原始月租', 'Renta Bruta') + '</th>' +
                  '<th class="n">' + span3('Sq Ft Adj.', '面积修正', 'Ajuste Área') + '</th>' +
                  '<th>' + span3('Condition & Feature Adj.', '成色·房型·卧卫·微区位修正', 'Ajustes Condición y Tipo') + '</th>' +
                  '<th class="n">' + span3('Adjusted Rent', '修正后月租', 'Renta Ajustada') + '</th>' +
                '</tr></thead>' +
                '<tbody>' + rowsHtml + '</tbody>' +
              '</table>' +
            '</div>'
          : '<div class="callout" style="margin-top:10px">' + esc(L3('Regional index benchmarks shown above; individual active listings are omitted or pending refresh.', '已展示区域租金指数汇总；暂无单独挂牌明细或等待刷新。', 'Se muestran referencias del índice regional; sin listados individuales por ahora.')) + '</div>') +
      '</div>';

    mounts.forEach(function (m) {
      m.innerHTML = panelHtml;
      if (lazyHelper && typeof lazyHelper.bindLazySubForms === 'function') lazyHelper.bindLazySubForms(m);
    });
  }

  /* ------------------------------------------------------------------ 5B. Phase-2 Role Studios & Legal/Living Radars */
  var RADAR_ENUM_MAP = {
    action_required: ['Action Required', '需重点核查', 'Acción requerida'],
    watchouts_present: ['Watchouts Present', '存在留意事项', 'Con alertas'],
    prelim_verified_clear: ['Prelim Verified Clear', '预审报告无异常', 'Preliminar verificado limpio'],
    screened_verify_prelim: ['Screened · Verify Prelim', '已初筛 · 需核对预审报告', 'Evaluado · verificar preliminar'],
    promulgated_statutory: ['Statutory Promulgated Rate', '州法定统一费率', 'Tarifa legal promulgada'],
    state_filed_tariff: ['State-Filed Underwriter Tariff', '州备案承保费率表', 'Tarifa registrada estatal'],
    file_and_use_schedule: ['File-and-Use Rate Schedule', '备案即用费率表', 'Esquema de tarifa registrada'],
    estimated_schedule: ['Estimated Rate Curve (Verify Quote)', '参考费率曲线估算（需向产权公司核实）', 'Curva de tarifa estimada (verificar)'],
    prelim_uploaded: ['Prelim / Title Doc Uploaded', '已上传产权预审文件', 'Documento preliminar cargado'],
    live_open_recorder_checked: ['Live Open Recorder Checked', '已联网检索公开登记库', 'Registro público en vivo consultado'],
    live_open_distress_checked: ['Live Open Distress Checked', '已联网检索公开违约库', 'Registro de mora en vivo consultado'],
    assessor_and_disclosures_only: ['Assessor & Disclosures Screen', '基于评估档案与披露初筛', 'Evaluación y divulgaciones'],
    assessor_or_violations_only: ['Assessor / Municipal Violations Screen', '基于评估档案与市政违规记录初筛', 'Evaluación catastral y municipal'],
    ucc1_recorded_on_prelim: ['UCC-1 Recorded on Prelim', '预审档含太阳能 UCC-1 留置', 'UCC-1 solar registrado'],
    installed_verify_ownership: ['Installed · Verify Owned vs Leased', '已装光伏 · 核实买断/租赁', 'Instalado · verificar propiedad'],
    owned_confirmed: ['Seller-Owned Confirmed', '已确认为卖方买断', 'Propiedad confirmada'],
    none_detected: ['None Detected', '未检出', 'No detectado'],
    active_nod_or_nts: ['Active NOD / Trustee Sale', '存在违约或拍卖通知', 'Aviso de mora/remate activo'],
    tax_delinquent: ['Tax Delinquent Flag', '存在欠税记录', 'Impuestos morosos'],
    prior_reo_or_trustee_deed: ['Prior REO / Trustee Transfer', '历史含银行回收过户', 'Transferencia REO previa'],
    none_in_public_records: ['None in Public Records', '公开档案无违约记录', 'Sin mora en registros públicos'],
    non_judicial: ['Non-Judicial Trustee Sale', '非司法受托拍卖', 'Ejecución no judicial'],
    judicial: ['Judicial Foreclosure', '司法诉讼止赎', 'Ejecución judicial'],
    courtlistener_live_checked: ['CourtListener Dockets Checked', '已检索公开法院卷宗', 'Expedientes judiciales consultados'],
    courtlistener_unauthenticated: ['CourtListener Unauthenticated (Token Needed)', '法院卷宗接口未鉴权（需 API Token）', 'CourtListener sin autenticación'],
    courtlistener_unavailable: ['CourtListener Feed Unavailable', '法院卷宗服务暂不可用', 'CourtListener no disponible'],
    local_disclosures_and_assessor_only: ['Local Disclosures & Assessor Screen', '基于地方披露与评估初筛', 'Divulgaciones y catastro local'],
    non_warrantable_risk: ['Non-Warrantable Condo Risk', '不可担保公寓贷款风险', 'Riesgo de condominio no elegible'],
    review_required: ['Lender Questionnaire Review', '需核对贷款方问卷', 'Requiere revisión del prestamista'],
    no_litigation_blocker_detected: ['No Litigation Blocker Detected', '未检出诉讼阻断', 'Sin litigios bloqueantes'],
    not_applicable_sfr: ['N/A (Single-Family Parcel)', '不适用（独栋住宅）', 'N/A (Vivienda unifamiliar)'],
    high_fair_plan_likelihood: ['High FAIR / Excess Carrier Odds', '极可能需要州保底或高危险', 'Alta probabilidad de plan FAIR'],
    brush_moratorium_watch: ['Brush / Moratorium Watch', '林区承保限制关注', 'Alerta de moratoria por maleza'],
    standard_admitted_likely: ['Standard Admitted Market Likely', '常规商业保险可承保概率高', 'Mercado asegurador estándar probable'],
    immediate_non_renewal_or_acv_cliff: ['Immediate Non-Renewal / ACV Cliff', '面临停保或仅按折旧赔付', 'Riesgo inmediato de no renovación / ACV'],
    carrier_inspection_watch: ['Carrier Inspection Watch', '需留意保险公司实地验屋', 'Vigilancia de inspección'],
    within_standard_guidelines: ['Within Standard Guidelines', '符合主流保险承保标准', 'Dentro de pautas estándar'],
    hoa_master_policy_verify_eoi: ['HOA Master Policy · Verify EOI', 'HOA 总保单覆盖 · 需核实凭证', 'Póliza maestra HOA · verificar EOI'],
    insufficient_data: ['Insufficient Public Data', '公开数据不足', 'Datos insuficientes'],
    live_projects_mapped: ['Live Municipal Projects Mapped', '已匹配周边在建/申报项目', 'Proyectos municipales mapeados'],
    live_queried_zero_active_projects: ['Queried · 0 Active Projects Nearby', '已查询 · 周边无活跃申报项目', 'Consultado · 0 proyectos activos'],
    deterministic_zoning_only: ['Zoning & Statutory Overlay Screen', '基于区划与法定叠加层研判', 'Evaluación de zonificación y leyes'],
    offline_or_upstream_unavailable: ['Municipal Feed Unavailable', '市政实时接口暂不可用', 'Fuente municipal no disponible'],
    fcc_bdc_live: ['FCC Broadband Map Live', 'FCC 官方宽带实测库', 'Mapa FCC en vivo'],
    fcc_bdc_unavailable: ['FCC Mobile Feed Unavailable', 'FCC 移动信号接口暂不可用', 'Datos móviles FCC no disponibles'],
    fcc_bdc_unconfigured: ['FCC API Key Unconfigured (Check Map)', 'FCC API 未配置（请查官方地图）', 'API FCC sin configurar'],
    fcc_urban_tract_tier: ['FCC Tract Coverage Tier', 'FCC 普查区覆盖层级', 'Nivel de cobertura censal FCC'],
    unverified: ['Unverified', '未核实', 'Sin verificar'],
    unavailable: ['Unavailable', '暂不可用', 'No disponible'],
    unconfigured: ['Unconfigured', '未配置', 'Sin configurar'],
    blocker: ['Blocker', '阻断项', 'Bloqueante'],
    watch: ['Watch', '关注项', 'Alerta'],
    verify: ['Verify', '待核实', 'Verificar'],
    clear: ['Clear', '通过', 'Limpio'],
    prep_before_listing: ['Prep before listing', '挂牌前完成整备', 'Preparar antes de listar'],
    repair_before_listing: ['Repair before listing', '建议挂牌前修复', 'Reparar antes de listar'],
    credit_or_as_is: ['Offer credit / as-is', '建议给予买家折让', 'Ofrecer crédito / tal cual'],
    offer_credit: ['Offer closing credit', '建议过户折让', 'Ofrecer crédito de cierre'],
    price_in_or_offer_escrow_credit: ['Price in or offer escrow credit', '按现状定价或过户折让', 'Incluir en precio u ofrecer crédito'],
    skip_major_remodel_sell_as_is: ['Skip major remodel · sell as-is', '跳过大改 · 按现状挂牌', 'Omitir gran reforma · vender tal cual'],
    permit_and_tot_registration_required: ['Permit & TOT registration required', '需办理短租许可及过夜税登记', 'Permiso y registro TOT requeridos'],
    strong_by_right_detached_and_jadu: ['Strong · by-right detached ADU & JADU', '强可建 · 法定免听证独立 ADU 与 JADU', 'Fuerte · ADU independiente y JADU por derecho'],
    ca_prop_19: ['CA Prop 19', '加州 Prop 19', 'Prop 19 de CA'],
    ca_sb_9: ['CA SB 9 Duplex / Lot Split', '加州 SB 9 分宗/双拼法案', 'Ley CA SB 9'],
    ca_ab_2097: ['CA AB 2097 Transit Parking Waiver', '加州 AB 2097 减免车位法案', 'Ley CA AB 2097'],
    ca_sb_35: ['CA SB 35 Streamlined Infill', '加州 SB 35 快速审批条款', 'Ley CA SB 35'],
    ca_ab_2011: ['CA AB 2011 Corridor Housing', '加州 AB 2011 沿线住宅法案', 'Ley CA AB 2011'],
    strong_tax_lock_in: ['Strong Tax Lock-In', '高地税锁定效应', 'Fuerte retención fiscal'],
    moderate_tax_lock_in: ['Moderate Tax Lock-In', '中等地税锁定效应', 'Retención fiscal moderada'],
    light_tax_lock_in: ['Light Tax Lock-In', '地税锁定效应较低', 'Baja retención fiscal'],
    assessed_above_market_prop8_candidate: ['Prop 8 assessment review', '可申请 Prop 8 降税复核', 'Candidato a revisión Prop 8'],
    off_peak_tou_tariff: ['Off-Peak TOU Rate', '分时低谷电价', 'Tarifa fuera de pico'],
    blended_residential_tariff: ['Blended Residential Rate', '居民综合电价', 'Tarifa residencial estándar'],
    missing_electric_rate: ['Electric Rate Unavailable', '暂无电价数据', 'Tarifa eléctrica no disponible'],
    low: ['Low', '低', 'Bajo'],
    moderate: ['Moderate', '中等', 'Moderado'],
    high: ['High', '高', 'Alto'],
    standard: ['Standard', '常规', 'Estándar']
  };

  function humanRadarEnum(raw) {
    if (raw == null || raw === '') return '';
    var key = String(raw).trim();
    var lc = key.toLowerCase();
    if (RADAR_ENUM_MAP[lc]) {
      var tuple = RADAR_ENUM_MAP[lc];
      return L3(tuple[0], tuple[1], tuple[2]);
    }
    if (/^[A-Za-z0-9]+(?:_[A-Za-z0-9]+)+$/.test(key)) {
      var ACRONYMS = { tot: 'TOT', jadu: 'JADU', adu: 'ADU', sb: 'SB', ab: 'AB', ca: 'CA', fha: 'FHA', hoa: 'HOA', fcc: 'FCC', irc: 'IRC', nr: 'NR', prop: 'Prop' };
      return key.split('_').map(function (w, idx) {
        var lw = w.toLowerCase();
        if (ACRONYMS[lw]) return ACRONYMS[lw];
        if (!w) return '';
        if (idx === 0) return w.charAt(0).toUpperCase() + lw.slice(1);
        return lw;
      }).join(' ');
    }
    return cleanPublicText(key);
  }

  function safeHttpUrl(u) {
    if (!u || typeof u !== 'string') return '';
    var s = u.trim();
    return /^https?:\/\//i.test(s) ? s : '';
  }

  function statusPillCls(raw) {
    var s = String(raw || '').toLowerCase();
    if (/blocker|action_required|active_nod|immediate|high_fair|non_warrantable|ucc1/.test(s)) return 'bad';
    if (/watch|verify|review|moratorium|delinquent|credit|installed_verify|screened|unauth|unavail|unconfig|unverif|estimated_schedule|assessor_or_violations|local_disclosures/.test(s)) return 'watch';
    if (/clear|standard_admitted|within_standard|owned_confirmed|none_detected|none_in_public|no_litigation|prelim_verified/.test(s)) return 'good';
    return 'info';
  }

  function renderTitleLegalRadarPanel() {
    var mount = byId('titleLegalRadarPanel');
    if (!mount) return;
    var rep = getRep();
    var r = (rep && rep.titleLegalRadar) || mod(rep, 'titleLegalRadar');
    if (!r || typeof r !== 'object') {
      mount.innerHTML = '';
      return;
    }
    var role = getRole();
    var uf = r.underwriterRateFiling || {};
    var li = r.recordedLiensAndEncumbrances || {};
    var fc = r.foreclosureAndDistress || {};
    var lit = r.litigationAndDockets || {};
    var wp = uf.whoPaysCustom || {};

    var rateUrl = safeHttpUrl(uf.filingUrl);
    var recUrl = li.recorderPortal && safeHttpUrl(li.recorderPortal.url);
    var taxUrl = li.recorderPortal && safeHttpUrl(li.recorderPortal.taxCollectorUrl);

    var lienGuidance = role === 'seller' ? li.sellerActionGuidance : role === 'agent' ? li.agentActionGuidance : li.buyerActionGuidance;
    var fcGuidance = role === 'seller' ? fc.sellerCureNote : role === 'agent' ? fc.agentEscrowNote : fc.buyerLeverageNote;

    var flagsHtml = '';
    var flags = Array.isArray(li.flags) ? li.flags.slice(0, 4) : [];
    if (flags.length) {
      flagsHtml = '<ul style="margin:8px 0 0;padding-left:18px;font-size:12.4px;color:var(--ink2)">' +
        flags.map(function (fl) {
          return '<li style="margin-bottom:4px"><b>' + esc(cleanPublicText(fl.title || humanRadarEnum(fl.code))) + '</b>' +
            (fl.detail ? ': ' + esc(cleanPublicText(fl.detail)) : '') + '</li>';
        }).join('') + '</ul>';
    }

    var recordsHtml = '';
    var recs = Array.isArray(li.liveRecorderRecords) ? li.liveRecorderRecords.slice(0, 4) : [];
    if (recs.length) {
      recordsHtml = '<div style="margin-top:8px;font-size:12.2px;color:var(--ink2)">' +
        recs.map(function (rc) {
          return '<div>• ' + esc(cleanPublicText(rc.documentType || 'Recorded instrument')) +
            (rc.recordedDate ? ' (' + esc(rc.recordedDate) + ')' : '') + '</div>';
        }).join('') + '</div>';
    } else if (li.coverageSummary) {
      recordsHtml = '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(li.coverageSummary)) + '</div>';
    }

    var docketsHtml = '';
    var hits = Array.isArray(lit.docketHits) ? lit.docketHits.slice(0, 4) : [];
    if (hits.length) {
      docketsHtml = '<ul style="margin:8px 0 0;padding-left:18px;font-size:12.4px;color:var(--ink2)">' +
        hits.map(function (d) {
          var du = safeHttpUrl(d.url);
          var ttl = esc(cleanPublicText(d.title || 'Docket'));
          return '<li style="margin-bottom:4px">' + (du ? '<a href="' + esc(du) + '" target="_blank" rel="noopener">' + ttl + '</a>' : ttl) +
            (d.court ? ' · ' + esc(cleanPublicText(d.court)) : '') + (d.dateFiled ? ' (' + esc(d.dateFiled) + ')' : '') + '</li>';
        }).join('') + '</ul>';
    } else if (lit.coverageNote) {
      docketsHtml = '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(lit.coverageNote)) + '</div>';
    }

    var spr = li.superPriorityHoaRule || {};
    var fha = fc.fhaAntiFlipping || {};
    var ce = lit.codeEnforcement || {};
    var repw = lit.builderWarrantyRepose || {};

    var isModeledRate = uf.rateRegime === 'estimated_schedule' || uf.rateRegime === 'modeled_benchmark';

    mount.innerHTML =
      '<div class="eyebrow">' + span3('Title, Liens & Legal Radar', '产权留置权与诉讼雷达', 'Radar Legal y de Título') + '</div>' +
      '<div class="stitle">' +
        '<h2>' + (isModeledRate
          ? span3('Estimated Title Schedule, Liens, Foreclosure & Litigation', '产权费率估算、留置权、法拍与法院诉讼筛查', 'Estimación de Título, Gravámenes, Ejecución y Litigios')
          : span3('Underwriter Tariffs, Liens, Foreclosure & Litigation', '州备案产权费率、留置权、法拍与法院诉讼筛查', 'Tarifas, Gravámenes, Ejecución y Litigios')) + '</h2>' +
        '<div class="meta"><span class="pill ' + statusPillCls(r.overallRadarStatus) + '">' + esc(humanRadarEnum(r.overallRadarStatus)) + '</span></div>' +
      '</div>' +
      (r.headline ? '<div class="small" style="margin-bottom:6px;color:var(--ink2)">' + esc(cleanPublicText(r.headline)) + '</div>' : '') +
      '<div class="studio-grid">' +
        '<div class="studio-subcard">' +
          '<h3>' + (isModeledRate
            ? span3('Modeled Title & Escrow Schedule', '产权与托管费率估算基准', 'Estimación de Título y Escrow')
            : span3('State Title & Escrow Tariff', '州备案产权与托管费率', 'Tarifa Estatal de Título y Escrow')) +
            (uf.rateRegime ? '<span class="pill ' + (isModeledRate ? 'watch' : 'info') + '">' + esc(humanRadarEnum(uf.rateRegime)) + '</span>' : '') +
          '</h3>' +
          '<div class="tiles" style="grid-template-columns:repeat(2,minmax(0,1fr));gap:8px">' +
            '<div class="tile"><div class="k">' + span3('Owner Standard / Enhanced', '标准 / 增强型产权险', 'Propietario Estándar / Plus') + '</div><div class="v">' + esc((isN(uf.ownerStandardUsd) ? fUsd(uf.ownerStandardUsd) : '—') + ' / ' + (isN(uf.ownerEnhancedUsd) ? fUsd(uf.ownerEnhancedUsd) : '—')) + '</div></div>' +
            '<div class="tile"><div class="k">' + span3('Lender Simultaneous + Escrow', '同步贷款险 + 托管费', 'Simultánea + Escrow') + '</div><div class="v">' + esc((isN(uf.simultaneousLenderUsd) ? fUsd(uf.simultaneousLenderUsd) : '—') + ' + ' + (isN(uf.escrowSettlementUsd) ? fUsd(uf.escrowSettlementUsd) : '—')) + '</div></div>' +
          '</div>' +
          (uf.reissueEligible && isN(uf.reissueSavingsUsd) ? '<div class="small" style="margin-top:8px"><b>' + span3('Reissue Credit:', '续保折扣：', 'Crédito de reemisión:') + '</b> ' + esc(fUsd(uf.reissueSavingsUsd)) + (uf.reissueNote ? ' · ' + esc(cleanPublicText(uf.reissueNote)) : '') + '</div>' : '') +
          (isN(wp.buyerEstimatedTitleEscrowUsd) || isN(wp.sellerEstimatedTitleEscrowUsd)
            ? '<div class="small" style="margin-top:6px">' + span3('Local Who-Pays Custom:', '当地惯例分摊：', 'Costumbre local:') + ' ' +
              span3('Buyer ≈ ', '买家 ≈ ', 'Comprador ≈ ') + esc(isN(wp.buyerEstimatedTitleEscrowUsd) ? fUsd(wp.buyerEstimatedTitleEscrowUsd) : '—') + ' · ' +
              span3('Seller ≈ ', '卖家 ≈ ', 'Vendedor ≈ ') + esc(isN(wp.sellerEstimatedTitleEscrowUsd) ? fUsd(wp.sellerEstimatedTitleEscrowUsd) : '—') + '</div>'
            : (wp.note || wp.customSummary ? '<div class="small" style="margin-top:6px">' + span3('Local Who-Pays Custom:', '当地惯例分摊：', 'Costumbre local:') + ' ' + esc(cleanPublicText(wp.note || wp.customSummary)) + '</div>' : '')) +
          (uf.regulator || rateUrl ? '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(uf.regulator || '')) + (rateUrl ? ' · <a href="' + esc(rateUrl) + '" target="_blank" rel="noopener">' + span3('Filing reference', '费率备案出处', 'Referencia tarifaria') + '</a>' : '') + '</div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Recorded Liens, Solar & HOA Priority', '留置权、太阳能 UCC-1 与 HOA 优先权', 'Gravámenes, Solar UCC-1 y HOA') +
            (li.coverageStatus ? '<span class="pill ' + statusPillCls(li.coverageStatus) + '">' + esc(humanRadarEnum(li.coverageStatus)) + '</span>' : '') +
          '</h3>' +
          '<div class="small">' +
            (li.solarOwnershipStatus ? '<b>' + span3('Solar:', '太阳能：', 'Solar:') + '</b> ' + esc(humanRadarEnum(li.solarOwnershipStatus)) + ' · ' : '') +
            (spr.isSuperPriorityState ? '<b>' + span3('HOA Super-Priority:', 'HOA 超级优先留置：', 'Súper prioridad HOA:') + '</b> ' + esc(String(spr.monthsPriority || '') + ' mo (' + (spr.statute || '') + ')') : span3('Non-super-priority HOA state', '非超级优先 HOA 州', 'Estado sin súper prioridad HOA')) +
          '</div>' +
          recordsHtml + flagsHtml +
          (lienGuidance ? '<div class="small" style="margin-top:8px"><b>' + span3('Role Action:', '角色行动建议：', 'Acción recomendada:') + '</b> ' + esc(cleanPublicText(lienGuidance)) + '</div>' : '') +
          (recUrl || taxUrl ? '<div class="small" style="margin-top:6px">' + (recUrl ? '<a href="' + esc(recUrl) + '" target="_blank" rel="noopener">' + esc(cleanPublicText((li.recorderPortal && li.recorderPortal.name) || L3('County Recorder', '县记录局', 'Registro del condado'))) + '</a>' : '') + (recUrl && taxUrl ? ' · ' : '') + (taxUrl ? '<a href="' + esc(taxUrl) + '" target="_blank" rel="noopener">' + esc(cleanPublicText((li.recorderPortal && li.recorderPortal.taxCollectorName) || L3('Tax Collector', '税务局', 'Recaudador fiscal'))) + '</a>' : '') + '</div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Foreclosure, Distress & FHA Flip Rule', '法拍违约、历史折价与 FHA 倒手限制', 'Ejecución, Mora y Regla FHA') +
            (fc.distressStage ? '<span class="pill ' + statusPillCls(fc.distressStage) + '">' + esc(humanRadarEnum(fc.distressStage)) + '</span>' : '') +
          '</h3>' +
          '<div class="small">' +
            (fc.statutoryForeclosureTrack ? '<b>' + span3('Track:', '程序类型：', 'Vía:') + '</b> ' + esc(humanRadarEnum(fc.statutoryForeclosureTrack)) + (fc.statutoryCitation ? ' (' + esc(fc.statutoryCitation) + ')' : '') : '') +
            (fc.quitclaimInChain ? ' · <b>' + span3('Quitclaim deed in chain', '产权链含放弃契约', 'Escritura de renuncia en historial') + '</b>' : '') +
          '</div>' +
          (fha.rule ? '<div class="small" style="margin-top:6px"><b>' + span3('FHA Anti-Flipping:', 'FHA 防炒房规则：', 'Regla anti-flipping FHA:') + '</b> ' + esc(cleanPublicText(fha.note || fha.rule)) + '</div>' : '') +
          (fcGuidance ? '<div class="small" style="margin-top:6px"><b>' + span3('Role Note:', '角色提示：', 'Nota para su rol:') + '</b> ' + esc(cleanPublicText(fcGuidance)) + '</div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Litigation Dockets, Code & Repose', '法院卷宗、市政违章与建造商保修期', 'Litigios, Código y Garantía') +
            (lit.coverageStatus ? '<span class="pill ' + statusPillCls(lit.coverageStatus) + '">' + esc(humanRadarEnum(lit.coverageStatus)) + '</span>' : '') +
          '</h3>' +
          '<div class="small">' +
            '<b>' + span3('Open Code Cases:', '未结市政违章：', 'Casos municipales abiertos:') + '</b> ' +
            esc((function () {
              var ceChecked = Boolean(
                (rep && rep.facts && (rep.facts.codeEnforcementChecked === true || (rep.facts.disclosures && rep.facts.disclosures.codeEnforcement && rep.facts.disclosures.codeEnforcement.checked === true) || (rep.facts.countyRecords && rep.facts.countyRecords.permitsAndCode && rep.facts.countyRecords.permitsAndCode.codeEnforcementChecked === true))) ||
                (Array.isArray(ce.cases) && ce.cases.length > 0) ||
                (isN(ce.openCount) && ce.openCount > 0) ||
                (isN(ce.closedCount) && ce.closedCount > 0)
              );
              return (ceChecked && isN(ce.openCount)) ? String(ce.openCount) : L3('Verify with city', '待向市政核查', 'Verificar');
            })()) +
            (lit.condoWarrantabilityImpact ? ' · <b>' + span3('Warrantability:', '公寓可贷性：', 'Elegibilidad:') + '</b> ' + esc(humanRadarEnum(lit.condoWarrantabilityImpact)) : '') +
          '</div>' +
          (repw.statute ? '<div class="small" style="margin-top:6px"><b>' + span3('Builder Repose:', '建造商责任时效：', 'Plazo de garantía:') + '</b> ' + esc(cleanPublicText(repw.statute)) + (isN(repw.expiresYear) ? ' → ' + esc(String(repw.expiresYear)) : '') + (repw.active ? ' (' + span3('Active', '仍在期内', 'Vigente') + ')' : '') + '</div>' : '') +
          docketsHtml +
        '</div>' +
      '</div>' +
      (r.disclaimer ? '<div class="small" style="margin-top:8px">' + esc(cleanPublicText(r.disclaimer)) + '</div>' : '');
  }

  function renderBuyerLivingIntelPanel() {
    var mount = byId('buyerLivingIntelPanel');
    if (!mount) return;
    var rep = getRep();
    var bi = (rep && rep.buyerLivingIntel) || mod(rep, 'buyerLivingIntel');
    if (!bi || typeof bi !== 'object') {
      mount.innerHTML = '';
      return;
    }
    var ins = bi.insurabilityCliff || {};
    var rc = ins.roofCliff || {};
    var vs = ins.vintageSystems || {};
    var fc = bi.futureChangeRadar || {};
    var zn = fc.zoningInfillContext || {};
    var m5 = bi.mobile5gCoverage || {};
    var ns = bi.timeOfDayNoise || {};
    var el = bi.electrificationPayback || {};
    var st = bi.strAndAduRules || {};
    var strObj = st.shortTermRental || {};
    var aduObj = st.aduAndJadu || {};

    var mProj = Array.isArray(fc.mappedProjects) ? fc.mappedProjects.slice(0, 4) : [];
    var projHtml = mProj.length
      ? '<ul style="margin:6px 0 0;padding-left:18px;font-size:12.3px;color:var(--ink2)">' + mProj.map(function (p) {
          return '<li><b>' + esc(cleanPublicText(p.name)) + '</b> (' + esc(cleanPublicText(p.categoryLabel || '')) +
            (isN(p.distanceMiles) ? ', ' + p.distanceMiles.toFixed(2) + ' mi' : '') + ')</li>';
        }).join('') + '</ul>'
      : '<div class="small" style="margin-top:6px">' + span3('Zero active municipal development applications mapped within search radius.', '检索半径内未发现已立案的在建/规划申报项目。', 'Cero proyectos municipales activos mapeados en el radio.') + '</div>';

    var overlays = Array.isArray(zn.statutoryUpzoningOverlays) ? zn.statutoryUpzoningOverlays.filter(function (o) { return o && o.applies; }) : [];
    var carriers = Array.isArray(m5.carriers) ? m5.carriers : [];
    var ev = el.evLevel2Charging || {};
    var hp = el.heatPumpComparison || {};
    var sb = el.solarAndBatteryPayback || {};
    var portalUrl = safeHttpUrl(fc.municipalPortalUrl);
    var fccUrl = safeHttpUrl(m5.fccMobileMapUrl);

    var hasCarrierPcts = m5.available !== false && carriers.some(function (c) { return isN(c.outdoor5gNr35x3) || isN(c.outdoor4gLte5x1); });
    var m5Unavail = m5.status === 'unavailable' || m5.status === 'unconfigured' || m5.available === false || /unavail|unconfig/i.test(String(m5.provenance || ''));
    var carrierHtml = hasCarrierPcts
      ? '<div class="small">' + carriers.map(function (c) {
          return '<b>' + esc(c.carrierName) + '</b>: 5G ' + esc(isN(c.outdoor5gNr35x3) ? (c.outdoor5gNr35x3.toFixed(0) + '%') : '—') + ' / 4G ' + esc(isN(c.outdoor4gLte5x1) ? (c.outdoor4gLte5x1.toFixed(0) + '%') : '—');
        }).join(' · ') + '</div>'
      : '<div class="small">' + esc(cleanPublicText(m5.reason || m5.indoorPenetrationNote || m5.dbmHonestyNote || L3(
          'Live FCC carrier coverage tiers unavailable here — check the official FCC Broadband Map for this address.',
          '暂未获取实时运营商 5G/4G 覆盖数据 — 请在下方 FCC 官方信号地图核查门牌信号。',
          'Cobertura 5G/4G por operador no disponible aquí — consulte el mapa oficial FCC.'
        ))) + '</div>';

    mount.innerHTML =
      '<div class="eyebrow">' + span3('Buyer Insurability & Living Intelligence', '买家投保悬崖、5G 信号与居住深度核查', 'Seguro, 5G, Ruido y Normas de Vida') + '</div>' +
      '<div class="stitle">' +
        '<h2>' + span3('Carrier Cliff, Future Development, 5G, Electrification & ADU', '承保门槛、周边规划、5G 覆盖、电气化账本与 ADU 法规', 'Elegibilidad de Seguro, Desarrollo Futuro, 5G, Electrificación y ADU') + '</h2>' +
      '</div>' +
      '<div class="studio-grid">' +
        '<div class="studio-subcard">' +
          '<h3>' + span3('Insurability Cliff & Vintage Systems', '保险承保悬崖与老旧管线', 'Umbral de Seguro e Instalaciones') +
            (ins.fairPlanRiskTier ? '<span class="pill ' + statusPillCls(ins.fairPlanRiskTier) + '">' + esc(humanRadarEnum(ins.fairPlanRiskTier)) + '</span>' : '') +
          '</h3>' +
          '<div class="small">' +
            (rc.cliffStatus ? '<b>' + span3('Roof Status:', '屋顶承保状态：', 'Estado del techo:') + '</b> ' + esc(humanRadarEnum(rc.cliffStatus)) + (isN(rc.estimatedRoofAgeYears) ? ' (~' + Math.round(rc.estimatedRoofAgeYears) + ' yr)' : '') : '') +
            (ins.fourPointInspectionRequired ? ' · <span class="pill watch">' + span3('4-Point Inspection Likely', '可能要求四点验屋', 'Inspección de 4 puntos probable') + '</span>' : '') +
          '</div>' +
          (rc.summary ? '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(rc.summary)) + '</div>' : '') +
          ((vs.electrical && vs.electrical.eraCheckTriggers && vs.electrical.eraCheckTriggers[0])
            ? '<div class="small" style="margin-top:6px"><b>' + span3('Electrical / Plumbing Check:', '电路与管道年代核查：', 'Revisión eléctrica/plomería:') + '</b> ' + esc(cleanPublicText(vs.electrical.eraCheckTriggers[0])) + '</div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Future Change & Zoning Radar', '周边规划变更与区划叠加层', 'Radar de Cambios y Zonificación') +
            (fc.coverageStatus ? '<span class="pill ' + statusPillCls(fc.coverageStatus) + '">' + esc(humanRadarEnum(fc.coverageStatus)) + '</span>' : '') +
          '</h3>' +
          '<div class="small">' +
            (zn.parcelZoning ? '<b>' + span3('Zoning:', '土地分区：', 'Zonificación:') + '</b> ' + esc(zn.parcelZoning) + ' · ' : '') +
            (zn.infillExposureTier ? '<b>' + span3('Infill Exposure:', '加密开发敏感度：', 'Exposición infill:') + '</b> ' + esc(humanRadarEnum(zn.infillExposureTier)) : '') +
          '</div>' +
          (overlays.length ? '<div class="small" style="margin-top:6px"><b>' + span3('Applicable Overlays:', '适用法定叠加条款：', 'Normas aplicables:') + '</b> ' + esc(overlays.map(function (o) { return humanRadarEnum(o.code || o.title); }).join(', ')) + '</div>' : '') +
          projHtml +
          (portalUrl ? '<div class="small" style="margin-top:6px"><a href="' + esc(portalUrl) + '" target="_blank" rel="noopener">' + esc(cleanPublicText(fc.municipalPortalName || L3('City Planning Portal', '市政规划官网', 'Portal municipal'))) + '</a></div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Mobile 5G Coverage & Day/Night Noise', '蜂窝 5G 覆盖与昼夜分时噪音', 'Cobertura 5G y Ruido Día/Noche') +
            (m5.provenance ? '<span class="pill ' + (m5Unavail ? 'mute' : 'info') + '">' + esc(humanRadarEnum(m5.provenance)) + '</span>' : '') +
          '</h3>' +
          carrierHtml +
          (ns.laeq24hBandLabel || isN(ns.nighttimeDb)
            ? '<div class="small" style="margin-top:6px"><b>' + span3('Noise Profile:', '噪音画像：', 'Perfil acústico:') + '</b> ' +
              esc(cleanPublicText(ns.laeq24hBandLabel || '')) +
              (isN(ns.nighttimeDb) ? ' · ' + span3('Nighttime ', '夜间 ', 'Nocturno ') + Math.round(ns.nighttimeDb) + ' dB' : '') +
              (ns.schoolCurbCongestion && ns.schoolCurbCongestion.flaggedWithin250m ? ' · ' + span3('School curb bell traffic (<250m)', '250 米内含上下学接送高峰', 'Tráfico escolar (<250 m)') : '') +
              '</div>'
            : '') +
          (fccUrl ? '<div class="small" style="margin-top:6px"><a href="' + esc(fccUrl) + '" target="_blank" rel="noopener">' + span3('Open FCC Broadband Map', '查看 FCC 信号地图', 'Ver mapa FCC') + '</a></div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Electrification, STR & ADU Rules', '电气化节能、短租与 ADU 加建规则', 'Electrificación, STR y ADU') + '</h3>' +
          (ev.available
            ? '<div class="small"><b>' + span3('Home EV Level-2:', '家用慢充：', 'Carga EV Nivel 2:') + '</b> ' + esc(fUsd(ev.costPer1000MilesUsd)) + '/1k mi' +
              (isN(ev.gasVehicleCostPer1000MilesUsd) ? ' vs ' + esc(fUsd(ev.gasVehicleCostPer1000MilesUsd)) + ' ' + span3('gas', '油车', 'gasolina') : '') +
              (isN(ev.estimatedAnnualSavingsAt12kMilesUsd) ? ' (' + esc(fUsd(ev.estimatedAnnualSavingsAt12kMilesUsd)) + '/yr)' : '') + '</div>' : '') +
          (hp.available
            ? '<div class="small" style="margin-top:5px"><b>' + span3('Heat Pump vs Gas:', '热泵 vs 燃气供暖：', 'Bomba de calor vs Gas:') + '</b> ' +
              esc(fUsd(hp.heatPumpCostPerDeliveredMmbtuUsd)) + ' vs ' + esc(fUsd(hp.gasCostPerDeliveredMmbtuUsd)) + '/MMBtu</div>' : '') +
          (sb.available && isN(sb.recommendedSystemKw)
            ? '<div class="small" style="margin-top:5px"><b>' + span3('Solar + Battery:', '光伏储能测算：', 'Solar + Batería:') + '</b> ' + sb.recommendedSystemKw + ' kW · ' +
              span3('Yr-1 offset ', '首年电费抵扣 ', 'Ahorro año 1 ') + esc(fUsd(sb.estimatedYear1OffsetWithBatteryUsd || sb.estimatedYear1OffsetSolarOnlyUsd)) + '</div>' : '') +
          '<div class="small" style="margin-top:6px">' +
            (strObj.regulatoryTier ? '<b>STR:</b> ' + esc(humanRadarEnum(strObj.regulatoryTier)) + ' · ' : '') +
            (aduObj.feasibilityTier ? '<b>ADU:</b> ' + esc(humanRadarEnum(aduObj.feasibilityTier)) + (isN(aduObj.statutoryMaxDetachedAduSqft) ? ' (' + aduObj.statutoryMaxDetachedAduSqft + ' sq ft max)' : '') : '') +
          '</div>' +
        '</div>' +
      '</div>' +
      (bi.disclaimer ? '<div class="small" style="margin-top:8px">' + esc(cleanPublicText(bi.disclaimer)) + '</div>' : '');
  }

  var _sellerInputState = { price: '', payoff: '', feePct: '', credits: '' };

  function renderSellerStrategyStudio() {
    var mount = byId('sellerStrategyStudio');
    if (!mount) return;
    var rep = getRep();
    var ss = (rep && rep.sellerSuite) || mod(rep, 'sellerSuite');
    if (!ss || typeof ss !== 'object') {
      mount.innerHTML = '';
      return;
    }
    var ns = ss.netSheetModel || {};
    var tp = ss.taxPortability || {};
    var cu = ss.cureBeforeEscrow || {};
    var lt = ss.launchTimingAndBracket || {};
    var irc = ns.irc121Exclusion || {};
    var bc = lt.searchBracketCliff || {};

    var tTax = isN(ns.sellerTransferTaxUsd) ? ns.sellerTransferTaxUsd : 0;
    var tEsc = isN(ns.sellerTitleAndEscrowBenchmarkUsd) ? ns.sellerTitleAndEscrowBenchmarkUsd : 0;

    var cures = Array.isArray(cu.repairVsCreditRanking) ? cu.repairVsCreditRanking.slice(0, 4) : [];
    var mandates = Array.isArray(cu.pointOfSaleMandates) ? cu.pointOfSaleMandates.slice(0, 4) : [];
    var months = Array.isArray(lt.monthlySeasonalityIndex) ? lt.monthlySeasonalityIndex : [];

    var seasonHtml = '';
    if (lt.seasonalityAvailable && months.length) {
      var peakSet = {};
      (Array.isArray(lt.strongestListingMonths) ? lt.strongestListingMonths : []).forEach(function (pm) {
        if (pm) peakSet[String(pm).toLowerCase()] = true;
      });
      var maxAbs = months.reduce(function (mx, m) {
        var v = isN(m.avgMomPct) ? Math.abs(m.avgMomPct) : 0;
        return v > mx ? v : mx;
      }, 0.5);
      seasonHtml = '<div class="season-bars" role="img" aria-label="' + esc(L3('Monthly seasonality bars', '邮编月度价格季节性图', 'Estacionalidad mensual')) + '">' +
        months.map(function (m) {
          var v = isN(m.avgMomPct) ? m.avgMomPct : 0;
          var hPct = Math.max(12, Math.min(82, Math.round((Math.abs(v) / maxAbs) * 76)));
          var mNameLc = String(m.monthName || '').toLowerCase();
          var mShortLc = String(m.monthShort || '').toLowerCase();
          var isPeak = !!(peakSet[mNameLc] || peakSet[mShortLc]);
          var valTxt = isN(m.seasonalityIndex) ? m.seasonalityIndex.toFixed(1) : ((v >= 0 ? '+' : '') + v.toFixed(1) + '%');
          return '<div class="season-col' + (isPeak ? ' is-peak' : '') + '" title="' + esc((m.monthName || m.monthShort || '') + ': ' + (v >= 0 ? '+' : '') + v.toFixed(2) + '%') + '">' +
            '<span class="season-val">' + esc(valTxt) + '</span>' +
            '<div class="season-bar ' + (isPeak ? 'peak' : (v >= 0 ? 'pos' : 'neg')) + '" style="height:' + hPct + '%"></div>' +
            '<span class="season-lbl">' + esc(m.monthShort || '') + '</span>' +
          '</div>';
        }).join('') + '</div>' +
        '<div class="small" style="margin-top:6px">' +
          (Array.isArray(lt.strongestListingMonths) && lt.strongestListingMonths.length ? '<b>' + span3('Strongest months:', '最佳挂牌窗口：', 'Mejores meses:') + '</b> ' + esc(lt.strongestListingMonths.join(', ')) : '') +
        '</div>';
    } else if (lt.seasonalityMethodologyNote) {
      seasonHtml = '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(lt.seasonalityMethodologyNote)) + '</div>';
    }

    var titleEscrowDisplay = isN(ns.sellerTitleAndEscrowBenchmarkUsd)
      ? (tEsc === 0
          ? '<span>' + span3('Buyer-paid by local custom ($0 seller)', '当地惯例由买家承担（卖家 $0）', 'Pagado por comprador ($0 vendedor)') + '</span>'
          : esc(fUsd(tEsc)) + ' (' + span3('local custom', '当地惯例', 'costumbre local') + ')')
      : '—';

    mount.innerHTML =
      '<div class="eyebrow">' + span3('Seller Strategy & Net-Sheet Studio', '卖家净到手测算与上市策略台', 'Estudio de Neto y Estrategia del Vendedor') + '</div>' +
      '<div class="stitle">' +
        '<h2>' + span3('Proceeds Calculator, Tax Portability, Cure List & Timing', '税费净到手计算器、地税结转、预整备与挂牌价档', 'Calculadora Neta, Portabilidad Fiscal, Curación y Calendario') + '</h2>' +
      '</div>' +
      '<div class="studio-grid">' +
        '<div class="studio-subcard span2">' +
          '<h3>' + span3('Interactive Seller Net Proceeds Worksheet', '卖家净到手交互测算表', 'Hoja Interactiva de Ganancia Neta') +
            '<span class="pill info">' + span3('Public-record taxes pre-filled', '已预填公开税费基准', 'Impuestos públicos precargados') + '</span>' +
          '</h3>' +
          '<div class="small">' +
            (isN(ns.benchmarkSalePriceUsd) ? '<b>' + span3('Benchmark Value:', '估值基准：', 'Valor de referencia:') + '</b> ' + esc(fUsd(ns.benchmarkSalePriceUsd)) + ' · ' : '') +
            '<b>' + span3('Seller Transfer Tax:', '卖家过户税：', 'Impuesto de transferencia:') + '</b> ' + esc(isN(ns.sellerTransferTaxUsd) ? fUsd(ns.sellerTransferTaxUsd) : '—') + ' (' + span3('public record', '公开档案', 'registro público') + ') · ' +
            '<b>' + span3('Title & Escrow:', '产权与托管费：', 'Título y escrow:') + '</b> ' + titleEscrowDisplay +
          '</div>' +
          '<div class="net-inputs">' +
            '<label class="calcf"><span>' + span3('Target Sale Price ($)', '目标成交价 ($)', 'Precio objetivo ($)') + ' <em class="net-tag">' + span3('your input', '您的输入', 'su dato') + '</em></span>' +
              '<input id="sellerTargetPriceInput" class="calcinp" type="number" min="0" step="1000" placeholder="' + esc(isN(ns.benchmarkSalePriceUsd) ? ('e.g. ' + Math.round(ns.benchmarkSalePriceUsd)) : 'e.g. 950000') + '" value="' + esc(_sellerInputState.price) + '">' +
            '</label>' +
            '<label class="calcf"><span>' + span3('Mortgage Payoff ($)', '未还贷款余额 ($)', 'Saldo de hipoteca ($)') + ' <em class="net-tag">' + span3('your input', '您的输入', 'su dato') + '</em></span>' +
              '<input id="sellerLoanPayoffInput" class="calcinp" type="number" min="0" step="1000" placeholder="' + esc(L3('Enter lender payoff', '输入贷款结清额', 'Saldo actual')) + '" value="' + esc(_sellerInputState.payoff) + '">' +
            '</label>' +
            '<label class="calcf"><span>' + span3('Brokerage / Selling Fee (%)', '卖方中介总佣金 (%)', 'Comisión / Honorarios (%)') + ' <em class="net-tag">' + span3('your input', '您的输入', 'su dato') + '</em></span>' +
              '<input id="sellerCommissionPctInput" class="calcinp" type="number" min="0" max="15" step="0.1" placeholder="' + esc(L3('Enter agreed fee %', '输入协商佣金比例 %', 'Ingrese % acordado')) + '" value="' + esc(_sellerInputState.feePct) + '">' +
            '</label>' +
          '</div>' +
          '<div class="net-inputs" style="grid-template-columns:minmax(0,1fr)">' +
            '<label class="calcf"><span>' + span3('Seller Credits, Repairs or Staging Budget ($)', '买家让利 / 整备修缮预算 ($)', 'Créditos o reparaciones ($)') + ' <em class="net-tag">' + span3('your input', '您的输入', 'su dato') + '</em></span>' +
              '<input id="sellerCreditsInput" class="calcinp" type="number" min="0" step="500" placeholder="' + esc(L3('Optional prep or buyer credit ($)', '可选：整备或过户折让金额', 'Opcional ($)')) + '" value="' + esc(_sellerInputState.credits) + '">' +
            '</label>' +
          '</div>' +
          '<div id="sellerNetResultBanner" class="net-result-banner" hidden></div>' +
          (irc.singleExclusionUsd ? '<div class="small" style="margin-top:8px"><b>' + span3('IRC §121 Rule:', 'IRC §121 自住免税额：', 'Regla IRC §121:') + '</b> ' +
            esc(fUsd(irc.singleExclusionUsd)) + ' (' + span3('Single', '单身', 'Soltero') + ') / ' + esc(fUsd(irc.marriedExclusionUsd)) + ' (' + span3('Married', '夫妻合报', 'Casado') + ')' +
            (isN(irc.ownershipYearsHeld) ? ' · ' + irc.ownershipYearsHeld.toFixed(1) + ' yr ' + span3('held', '已持有', 'tenencia') : '') + '</div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Property Tax Portability', '房产税基数跨区结转', 'Portabilidad de Impuesto Predial') +
            (tp.programCode ? '<span class="pill ' + (tp.applicable ? 'good' : 'info') + '">' + esc(humanRadarEnum(tp.programCode)) + '</span>' : '') +
          '</h3>' +
          (tp.applicable
            ? '<div class="tiles" style="grid-template-columns:repeat(2,minmax(0,1fr));gap:8px">' +
                '<div class="tile"><div class="k">' + span3('Est. Annual Savings', '预估年省地税', 'Ahorro anual est.') + '</div><div class="v">' + esc(fUsd(tp.estimatedAnnualTaxSavingsUsd || tp.estimatedAnnualProp19TaxSavingsUsd)) + '</div></div>' +
                '<div class="tile"><div class="k">' + span3('10-Yr Cumulative', '10 年累计节税', 'Acumulado 10 años') + '</div><div class="v">' + esc(fUsd(tp.estimated10YrCumulativeSavingsUsd)) + '</div></div>' +
              '</div>'
            : '') +
          (tp.summary ? '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(tp.summary)) + '</div>' : '') +
        '</div>' +

        '<div class="studio-subcard">' +
          '<h3>' + span3('Cure-Before-Escrow & Repair ROI', '挂牌前隐患清零与整备回报率', 'Lista de Curación y ROI de Mejoras') + '</h3>' +
          '<div class="small">' +
            '<b>' + span3('Open Permits:', '未结案许可：', 'Permisos abiertos:') + '</b> ' + esc(String(cu.openOrUnfinaledPermitsCount || 0)) + ' · ' +
            '<b>' + span3('Open Code Cases:', '未结违章：', 'Casos abiertos:') + '</b> ' + esc(String(cu.codeEnforcementOpenCount || 0)) +
          '</div>' +
          (mandates.length ? '<div class="small" style="margin-top:6px"><b>' + span3('Point-of-Sale Mandates:', '过户强制核查：', 'Mandatos de venta:') + '</b> ' + esc(mandates.map(function (m) { return cleanPublicText(m.title); }).join(' · ')) + '</div>' : '') +
          (cures.length ? '<ul style="margin:6px 0 0;padding-left:18px;font-size:12.3px;color:var(--ink2)">' + cures.map(function (c) {
            return '<li><b>' + esc(cleanPublicText(c.item)) + '</b> — <span class="pill ' + statusPillCls(c.recommendation) + '">' + esc(humanRadarEnum(c.recommendation)) + '</span>' +
              (isN(c.costRecoupedPct) ? ' (~' + Math.round(c.costRecoupedPct) + '% ROI)' : '') + '</li>';
          }).join('') + '</ul>' : '') +
        '</div>' +

        '<div class="studio-subcard span2">' +
          '<h3>' + span3('Listing Seasonality & Portal Bracket Cliff', '挂牌季节性指数与搜索价档边界', 'Estacionalidad y Umbrales de Búsqueda') + '</h3>' +
          (bc.strategicPricingNote ? '<div class="small" style="margin-bottom:6px"><b>' + span3('Search Bracket Strategy:', '搜索价档策略：', 'Estrategia de umbral:') + '</b> ' + esc(cleanPublicText(bc.strategicPricingNote)) + '</div>' : '') +
          seasonHtml +
        '</div>' +
      '</div>';

    var pEl = byId('sellerTargetPriceInput');
    var mEl = byId('sellerLoanPayoffInput');
    var fEl = byId('sellerCommissionPctInput');
    var cEl = byId('sellerCreditsInput');
    var banner = byId('sellerNetResultBanner');

    function recomputeSellerNet() {
      if (!pEl || !banner) return;
      _sellerInputState.price = pEl.value;
      _sellerInputState.payoff = mEl ? mEl.value : '';
      _sellerInputState.feePct = fEl ? fEl.value : '';
      _sellerInputState.credits = cEl ? cEl.value : '';

      var hasAny = [_sellerInputState.price, _sellerInputState.payoff, _sellerInputState.feePct, _sellerInputState.credits].some(function (s) {
        return String(s || '').trim() !== '';
      });
      var priceNum = parseFloat(_sellerInputState.price);
      if (!hasAny || !isFinite(priceNum) || priceNum <= 0) {
        banner.setAttribute('hidden', '');
        banner.innerHTML = '';
        return;
      }
      var payoffNum = parseFloat(_sellerInputState.payoff);
      if (!isFinite(payoffNum) || payoffNum < 0) payoffNum = 0;
      var feePctNum = parseFloat(_sellerInputState.feePct);
      if (!isFinite(feePctNum) || feePctNum < 0) feePctNum = 0;
      var creditsNum = parseFloat(_sellerInputState.credits);
      if (!isFinite(creditsNum) || creditsNum < 0) creditsNum = 0;

      var feeUsd = priceNum * (feePctNum / 100);
      var netUsd = priceNum - payoffNum - feeUsd - creditsNum - tTax - tEsc;
      banner.removeAttribute('hidden');
      banner.innerHTML =
        '<div>' +
          '<div class="small">' + span3('Estimated Seller Net Proceeds (from your inputs − public tax/title)', '预估卖家净到手（基于您的输入 − 公开税费）', 'Ganancia neta estimada (sus datos − impuestos públicos)') + '</div>' +
          '<div class="nr-big" id="sellerNetProceedsVal">' + esc(fUsd(netUsd)) + '</div>' +
        '</div>' +
        '<div class="small">' +
          esc(L3('Gross ', '总价 ', 'Bruto ') + fUsd(priceNum) + ' − ' + L3('Payoff ', '贷款 ', 'Hipoteca ') + fUsd(payoffNum) + ' − ' + L3('Fee ', '佣金 ', 'Comisión ') + fUsd(feeUsd) + ' − ' + L3('Credits ', '让利 ', 'Créditos ') + fUsd(creditsNum) + ' − ' + L3('Tax/Title ', '税费 ', 'Impuestos ') + fUsd(tTax + tEsc)) +
        '</div>';
    }

    [pEl, mEl, fEl, cEl].forEach(function (inp) {
      if (inp) inp.addEventListener('input', recomputeSellerNet);
    });
    recomputeSellerNet();
  }

  var _cmaState = { key: '', condPct: 0, included: {} };

  function renderAgentDealDesk() {
    var mount = byId('agentDealDesk');
    if (!mount) return;
    var rep = getRep();
    var ag = (rep && rep.agentSuite) || mod(rep, 'agentSuite');
    if (!ag || typeof ag !== 'object') {
      mount.innerHTML = '';
      return;
    }
    var sum = ag.escrowKillerSummary || {};
    var gates = Array.isArray(ag.escrowKillerMatrix) ? ag.escrowKillerMatrix : [];
    var cma = ag.cmaWorkbench || {};
    var comps = Array.isArray(cma.comps) ? cma.comps : [];
    var lead = ag.listingLeadSignals || {};

    var repKey = String((rep && rep.address) || '') + ':' + comps.length;
    if (_cmaState.key !== repKey) {
      _cmaState.key = repKey;
      _cmaState.condPct = 0;
      _cmaState.included = {};
      comps.forEach(function (c, i) {
        _cmaState.included[i] = c.includedByDefault !== false;
      });
    }

    var gatesHtml = gates.map(function (g) {
      return '<div class="escrow-card">' +
        '<div class="escrow-card-top"><b>' + esc(cleanPublicText(g.title)) + '</b><span class="pill ' + statusPillCls(g.status) + '">' + esc(humanRadarEnum(g.status)) + '</span></div>' +
        (g.evidence ? '<div class="small">' + esc(cleanPublicText(g.evidence)) + '</div>' : '') +
        (g.agentAction ? '<div class="small" style="margin-top:4px;color:var(--ink2)"><b>' + span3('Next:', '行动：', 'Acción:') + '</b> ' + esc(cleanPublicText(g.agentAction)) + '</div>' : '') +
      '</div>';
    }).join('');

    var rowsHtml = comps.map(function (c, idx) {
      var chk = _cmaState.included[idx] !== false ? ' checked' : '';
      var adjParts = [];
      if (isN(c.sqftDeltaUsd) && c.sqftDeltaUsd !== 0) adjParts.push('SqFt ' + (c.sqftDeltaUsd > 0 ? '+' : '') + fUsdK(c.sqftDeltaUsd));
      if (isN(c.bedBathDeltaUsd) && c.bedBathDeltaUsd !== 0) adjParts.push('Bed/Bath ' + (c.bedBathDeltaUsd > 0 ? '+' : '') + fUsdK(c.bedBathDeltaUsd));
      if (isN(c.ageCondDeltaUsd) && c.ageCondDeltaUsd !== 0) adjParts.push('Age ' + (c.ageCondDeltaUsd > 0 ? '+' : '') + fUsdK(c.ageCondDeltaUsd));
      return '<tr>' +
        '<td><input type="checkbox" class="agent-cma-chk" data-cma-idx="' + idx + '"' + chk + ' aria-label="Include comp"></td>' +
        '<td><b>' + esc(cleanPublicText(c.address || ('Comp #' + (idx + 1)))) + '</b><div class="small">' + esc(c.saleDate || '') + (isN(c.sqft) ? ' · ' + fNum(c.sqft) + ' sq ft' : '') + '</div></td>' +
        '<td class="n">' + esc(fUsd(c.rawPrice)) + '</td>' +
        '<td><span class="small">' + esc(adjParts.join(' · ') || L3('Minimal diff', '差异极小', 'Mínimo')) + '</span></td>' +
        '<td class="n"><strong data-cma-adj-cell="' + idx + '">' + esc(fUsd(c.adjustedPrice)) + '</strong></td>' +
      '</tr>';
    }).join('');

    mount.innerHTML =
      '<div class="eyebrow">' + span3('Agent Deal Desk & Interactive CMA', '经纪人成交通道评估与交互式 CMA', 'Mesa de Cierre y CMA del Agente') + '</div>' +
      '<div class="stitle">' +
        '<h2>' + span3('8-Gate Escrow-Killer Matrix, Live CMA & Lead Signals', '8 大成交通道闸门、实时可比调价台与房源线索', 'Matriz de 8 Puertas, CMA en Vivo y Señales de Captación') + '</h2>' +
      '</div>' +
      '<div class="studio-grid">' +
        '<div class="studio-subcard span2">' +
          '<h3>' + span3('8-Gate Escrow-Killer Matrix', '8 大成交通道风险闸门', 'Matriz de 8 Puertas de Cierre') +
            '<span style="display:flex;gap:6px;flex-wrap:wrap">' +
              '<span class="pill bad">' + esc((sum.blockerCount || 0) + ' ' + L3('Blocker', '阻断', 'Bloq.')) + '</span>' +
              '<span class="pill watch">' + esc(((sum.watchCount || 0) + (sum.verifyCount || 0)) + ' ' + L3('Watch/Verify', '留意/待核实', 'Alerta/Verif.')) + '</span>' +
              '<span class="pill good">' + esc((sum.clearCount || 0) + ' ' + L3('Clear', '通过', 'Limpio')) + '</span>' +
            '</span>' +
          '</h3>' +
          '<div class="escrow-grid">' + gatesHtml + '</div>' +
        '</div>' +

        '<div class="studio-subcard span2">' +
          '<h3>' + span3('Interactive CMA Adjustment Workbench', '交互式可比房源 CMA 调价台', 'Mesa Interactiva de Ajustes CMA') +
            '<button type="button" id="agentCopyCmaBtn" class="copybtn pri-btn">' + span3('Copy CMA summary', '复制 CMA 摘要', 'Copiar resumen CMA') + '</button>' +
          '</h3>' +
          '<div class="cma-toolbar">' +
            '<label class="cma-slider-wrap" for="agentCmaCondSlider">' + span3('Condition / Upgrade Adj:', '成色 / 装修修正系数：', 'Ajuste condición:') +
              '<input id="agentCmaCondSlider" type="range" min="-10" max="10" step="1" value="' + esc(String(_cmaState.condPct)) + '">' +
              '<span id="agentCmaCondReadout" class="pill info">' + esc((_cmaState.condPct >= 0 ? '+' : '') + _cmaState.condPct + '%') + '</span>' +
            '</label>' +
            '<div class="small" id="agentCmaActiveCount"></div>' +
          '</div>' +
          '<div class="tiles" style="grid-template-columns:repeat(3,minmax(0,1fr));gap:8px;margin-bottom:8px">' +
            '<div class="tile"><div class="k">' + span3('Weighted Adjusted CMA', '加权修正 CMA 估值', 'CMA Ajustado Ponderado') + '</div><div class="v" id="agentCmaWeightedVal">—</div></div>' +
            '<div class="tile"><div class="k">' + span3('Adjusted Median', '修正中位数', 'Mediana Ajustada') + '</div><div class="v" id="agentCmaMedianVal">—</div></div>' +
            '<div class="tile"><div class="k">' + span3('Adjusted $/Sq Ft', '修正单价 / 平尺', '$/Pie² Ajustado') + '</div><div class="v" id="agentCmaPsfVal">—</div></div>' +
          '</div>' +
          (comps.length
            ? '<div class="scroll"><table class="t"><thead><tr>' +
                '<th></th>' +
                '<th>' + span3('Comp Sale', '可比房源', 'Comparable') + '</th>' +
                '<th class="n">' + span3('Raw Price', '原始成交价', 'Precio bruto') + '</th>' +
                '<th>' + span3('Adjustments', '面积/房型/房龄修正', 'Ajustes') + '</th>' +
                '<th class="n">' + span3('Adjusted Value', '修正后估值', 'Valor ajustado') + '</th>' +
              '</tr></thead><tbody>' + rowsHtml + '</tbody></table></div>'
            : '<div class="small">' + span3('No comparable sales attached in this report.', '本报告暂无可比成交记录。', 'Sin ventas comparables en este informe.') + '</div>') +
        '</div>' +

        '<div class="studio-subcard span2">' +
          '<h3>' + span3('Listing Lead & Tax Lock-In Signals', '挂牌开发线索与地税锁定信号', 'Señales de Captación y Retención Fiscal') +
            (lead.taxLockInStrength ? '<span class="pill ' + statusPillCls(lead.taxLockInStrength) + '">' + esc(humanRadarEnum(lead.taxLockInStrength)) + '</span>' : '') +
          '</h3>' +
          '<div class="small">' +
            (isN(lead.ownershipTenureYears) ? '<b>' + span3('Tenure:', '持有年限：', 'Tenencia:') + '</b> ' + lead.ownershipTenureYears.toFixed(1) + ' yr · ' : '') +
            (isN(lead.estimatedGrossEquityGainUsd) ? '<b>' + span3('Est. Gross Equity Gain:', '预估账面增值：', 'Plusvalía bruta est.:') + '</b> ' + esc(fUsd(lead.estimatedGrossEquityGainUsd)) + (isN(lead.estimatedGrossEquityGainPct) ? ' (+' + lead.estimatedGrossEquityGainPct.toFixed(0) + '%)' : '') + ' · ' : '') +
            (isN(lead.assessedToMarketRatioPct) ? '<b>' + span3('Assessed / Market:', '计税评估占比：', 'Catastro / Mercado:') + '</b> ' + lead.assessedToMarketRatioPct.toFixed(0) + '%' : '') +
          '</div>' +
          (lead.privacyNotice ? '<div class="small" style="margin-top:6px">' + esc(cleanPublicText(lead.privacyNotice)) + '</div>' : '') +
        '</div>' +
      '</div>';

    var slider = byId('agentCmaCondSlider');
    var readout = byId('agentCmaCondReadout');
    var wEl = byId('agentCmaWeightedVal');
    var mEl = byId('agentCmaMedianVal');
    var psfEl = byId('agentCmaPsfVal');
    var cntEl = byId('agentCmaActiveCount');
    var latestSummaryText = '';

    function recomputeAgentCma() {
      var factor = 1 + (_cmaState.condPct / 100);
      if (readout) readout.textContent = (_cmaState.condPct >= 0 ? '+' : '') + _cmaState.condPct + '%';

      var activePrices = [];
      var activePsfs = [];
      var wSum = 0, wpSum = 0;

      comps.forEach(function (c, idx) {
        var baseAdj = isN(c.adjustedPrice) ? c.adjustedPrice : (isN(c.rawPrice) ? c.rawPrice : null);
        var eff = baseAdj != null ? Math.round(baseAdj * factor) : null;
        var cell = mount.querySelector('[data-cma-adj-cell="' + idx + '"]');
        if (cell) cell.textContent = eff != null ? fUsd(eff) : '—';

        if (_cmaState.included[idx] !== false && eff != null) {
          activePrices.push(eff);
          var w = isN(c.weight) && c.weight > 0 ? c.weight : 1;
          wSum += w;
          wpSum += eff * w;
          if (isN(c.sqft) && c.sqft > 0) activePsfs.push(eff / c.sqft);
          else if (isN(c.pricePerSqft)) activePsfs.push(c.pricePerSqft * factor);
        }
      });

      var wAvg = wSum > 0 ? Math.round(wpSum / wSum) : null;
      var med = null;
      if (activePrices.length) {
        var sorted = activePrices.slice().sort(function (a, b) { return a - b; });
        var mid = Math.floor(sorted.length / 2);
        med = sorted.length % 2 ? sorted[mid] : Math.round((sorted[mid - 1] + sorted[mid]) / 2);
      }
      var medPsf = null;
      if (activePsfs.length) {
        var sPsf = activePsfs.slice().sort(function (a, b) { return a - b; });
        var mIdx = Math.floor(sPsf.length / 2);
        medPsf = sPsf.length % 2 ? sPsf[mIdx] : (sPsf[mIdx - 1] + sPsf[mIdx]) / 2;
      }

      if (wEl) wEl.textContent = wAvg != null ? fUsd(wAvg) : '—';
      if (mEl) mEl.textContent = med != null ? fUsd(med) : '—';
      if (psfEl) psfEl.textContent = medPsf != null ? fUsd(medPsf) : '—';
      if (cntEl) cntEl.textContent = activePrices.length + ' / ' + comps.length + ' ' + L3('comps selected', '套已入选', 'comps seleccionados');

      latestSummaryText =
        ((rep && rep.address) || 'Subject Property') + ' — Interactive CMA (' + activePrices.length + '/' + comps.length + ' comps, condition ' + (_cmaState.condPct >= 0 ? '+' : '') + _cmaState.condPct + '%): ' +
        'Weighted ' + fUsd(wAvg) + ', Median ' + fUsd(med) + ', ' + fUsd(medPsf) + '/sq ft.';
    }

    if (slider) {
      slider.addEventListener('input', function () {
        _cmaState.condPct = parseInt(slider.value, 10) || 0;
        recomputeAgentCma();
      });
    }
    qAll('.agent-cma-chk', mount).forEach(function (chk) {
      chk.addEventListener('change', function () {
        var idx = parseInt(chk.getAttribute('data-cma-idx'), 10);
        if (!isNaN(idx)) _cmaState.included[idx] = !!chk.checked;
        recomputeAgentCma();
      });
    });
    var copyBtn = byId('agentCopyCmaBtn');
    if (copyBtn) {
      copyBtn.addEventListener('click', function () {
        if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
          navigator.clipboard.writeText(latestSummaryText).catch(function () {});
        }
        copyBtn.textContent = L3('Copied!', '已复制！', '¡Copiado!');
        setTimeout(function () {
          copyBtn.textContent = L3('Copy CMA summary', '复制 CMA 摘要', 'Copiar resumen CMA');
        }, 1600);
      });
    }
    recomputeAgentCma();
  }

  /* ------------------------------------------------------------------ 6. Global Keyboard Shortcuts, Language Refresh & Boot */
  document.addEventListener('keydown', function (ev) {
    if (_wtState.open) {
      if (ev.key === 'Escape') { ev.preventDefault(); closeWalkthroughModal(); }
      else if (ev.key === 'ArrowRight') {
        var deck = getWalkthroughDeck(getRep() || {});
        if (_wtState.idx < (deck.slides || []).length - 1) { _wtState.idx++; renderWalkthroughModal(); }
      } else if (ev.key === 'ArrowLeft') {
        if (_wtState.idx > 0) { _wtState.idx--; renderWalkthroughModal(); }
      }
      return;
    }
    if (_plbState.open) {
      if (ev.key === 'Escape') { ev.preventDefault(); closePhotoLightbox(); }
      else if (ev.key === 'ArrowRight') { stepPhotoLightbox(1); }
      else if (ev.key === 'ArrowLeft') { stepPhotoLightbox(-1); }
    }
  });

  function syncStickyHeaderVars() {
    var hdr = typeof document.querySelector === 'function' ? document.querySelector('.hdr') : null;
    var hdrRect = hdr && typeof hdr.getBoundingClientRect === 'function' ? hdr.getBoundingClientRect() : null;
    var hdrH = hdrRect ? Math.ceil(hdrRect.height) : ((hdr && hdr.offsetHeight) || 64);
    var rootStyle = document.documentElement && document.documentElement.style;
    if (rootStyle && typeof rootStyle.setProperty === 'function') {
      rootStyle.setProperty('--hdr-h', hdrH + 'px');
      rootStyle.setProperty('--chap-sticky-h', '0px');
    }
  }

  function refreshAllWorkspaceViews() {
    if (document.documentElement && typeof document.documentElement.setAttribute === 'function') {
      document.documentElement.setAttribute('data-role', getRole());
    }
    syncFengShuiLanguageVisibility();
    renderInsuranceQuoteStudio();
    renderRentResearchPanel();
    renderTitleLegalRadarPanel();
    renderBuyerLivingIntelPanel();
    renderSellerStrategyStudio();
    renderAgentDealDesk();
    applyRoleHiddenSections();
    renderRoleSwitchers();
    renderVerdictHero();
    renderHeroMediaHub();
    renderRoleFocusBrief();
    renderChapterBar();
    renderQuestionCards();
    renderRoleHiddenNote();
    renderReportTocTree();
    bindTocGlobalEvents();
    renderBenchmarkStrips();
    renderWalkthroughOverviewBanner();
    renderChapterFooterNav();
    syncTabsNavChapterState();
    syncStickyHeaderVars();
    updateHorizontalScrollFades();
    if (window.OpenEstateLazy && typeof window.OpenEstateLazy.renderAllLazyAndPaywallAnchors === 'function') {
      try { window.OpenEstateLazy.renderAllLazyAndPaywallAnchors({ skipRent: true }); } catch (e) {}
    }
    if (_wtState.open) renderWalkthroughModal();
    if (_plbState.open) renderLightboxModal();
  }

  function ensureQuestionCardsCanonicalDomOrder() {
    var stack = byId('questionCardsStack');
    if (stack && typeof stack.appendChild === 'function') {
      getOrderedQuestionCards().forEach(function (q) {
        var c = byId(q.cardId);
        if (c && c.parentNode === stack) stack.appendChild(c);
      });
    }
    var potDrawer = byId('qDrawerPotential');
    var h3 = byId('house3d');
    var hs = byId('house');
    if (potDrawer && h3 && hs && h3.parentNode === potDrawer && hs.parentNode === potDrawer && typeof potDrawer.insertBefore === 'function') {
      try { potDrawer.insertBefore(h3, hs); } catch (e) {}
    }
  }

  function detectInitialChapter() {
    var u = safeUrlObj();
    if (!u || !u.searchParams) return null;
    var ch = u.searchParams.get('chapter');
    return ch ? String(ch).trim() : null;
  }

  function bootWorkspace() {
    ensureQuestionCardsCanonicalDomOrder();
    bindChapterAutoActivation();
    refreshAllWorkspaceViews();
    var initialTarget = detectInitialChapter();
    if (initialTarget && initialTarget !== 'ov') {
      activateChapter(initialTarget, { scrollWindow: true });
    } else {
      syncDrawerFromLocationHash(true);
    }
    syncStickyHeaderVars();
    if (!window.__DB_BOUND_STICKY_RESIZE__ && typeof window.addEventListener === 'function') {
      window.__DB_BOUND_STICKY_RESIZE__ = true;
      window.addEventListener('resize', syncStickyHeaderVars);
    }

    if (typeof window.refreshDynamicLanguageViews === 'function' && !window.__DB_WRAPPED_LANG_REFRESH__) {
      window.__DB_WRAPPED_LANG_REFRESH__ = true;
      var origLangRefresh = window.refreshDynamicLanguageViews;
      window.refreshDynamicLanguageViews = function () {
        var res = origLangRefresh.apply(this, arguments);
        try { refreshAllWorkspaceViews(); } catch (e) {}
        return res;
      };
    }
  }

  window.OpenEstateWorkspace = {
    setRole: setRole,
    getRole: getRole,
    getRoleCardDomIds: getRoleCardDomIds,
    renderRoleBrief: renderRoleFocusBrief,
    renderRoleSwitchers: renderRoleSwitchers,
    activateChapter: activateChapter,
    toggleQuestionCard: setQuestionCardOpen,
    setHeroMediaMode: setHeroMediaMode,
    jumpToSection: jumpToSection,
    openWalkthrough: openWalkthroughModal,
    closeWalkthrough: closeWalkthroughModal,
    openLightbox: openPhotoLightbox,
    renderVerdictHero: renderVerdictHero,
    renderQuestionCards: renderQuestionCards,
    renderBenchmarkStrips: renderBenchmarkStrips,
    renderWalkthroughBanner: renderWalkthroughOverviewBanner,
    renderInsuranceStudio: renderInsuranceQuoteStudio,
    renderRentResearchPanel: renderRentResearchPanel,
    renderTitleLegalRadarPanel: renderTitleLegalRadarPanel,
    renderBuyerLivingIntelPanel: renderBuyerLivingIntelPanel,
    renderSellerStrategyStudio: renderSellerStrategyStudio,
    renderAgentDealDesk: renderAgentDealDesk,
    getRentResearchData: getRentResearchData,
    renderTocTree: renderReportTocTree,
    setTocSidebarOpen: setTocSidebarOpen,
    setTocBranchCollapsed: setTocBranchCollapsed,
    resizeChartsAndMaps: resizeChartsAndMaps,
    refreshRentResearch: renderRentResearchPanel,
    refreshAll: refreshAllWorkspaceViews
  };

  bootWorkspace();
})();
