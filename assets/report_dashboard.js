/* Open Estate Buyer Command Center Dashboard & Interactive AI Artifact Studio
 * Single-screen desktop Bento layout (390px 1-col / 820px 2-col / 1440px 3-col),
 * zero-lag live financing simulator, and AI re-charting studio (`POST /api/report/:slug/artifact`).
 */
(function initOpenEstateCommandCenter(globalObj) {
  'use strict';

  var g = globalObj || (typeof window !== 'undefined' ? window : globalThis);
  if (!g || typeof document === 'undefined' || !document.documentElement) {
    return;
  }

  var STYLE_ID = 'dbCmdDashboardStyles';
  var BTN_ID = 'toggleCommandCenterBtn';
  var SHELL_ID = 'buyerCommandCenter';
  var MAX_HISTORY = 8;

  var I18N = {
    en: {
      toggleLabel: 'Command Center',
      backToReport: 'Back to Full Report',
      hdrSubtitle: 'Single-Screen Buyer Command Center',
      zoneATitle: 'Live Deal & Cashflow Simulator',
      zoneABadge: '0ms Real-Time',
      offerPrice: 'Offer Price',
      downPayment: 'Down Payment',
      interestRate: 'Interest Rate (30yr)',
      quakeToggle: 'CEA Earthquake Insurance',
      deductible: 'Deductible',
      aduRentOffset: 'ADU / Suite Rent Offset',
      netMonthly: 'Net Monthly Outlay',
      grossLabel: 'Gross PITI+M',
      cashToClose: 'Cash to Close',
      fiveYrEquity: '5-Yr Est. Equity',
      capRate: 'Gross / Net Yield',
      resetSim: 'Reset Baseline',
      chartDealBtn: 'Chart This Scenario →',
      allCashTag: 'All-Cash',
      piSeg: 'P&I',
      taxSeg: 'Tax',
      insSeg: 'Ins+Eq',
      maintSeg: 'Maint/HOA',
      zoneBTitle: 'Multi-Source Intelligence Bento',
      zoneBBadge: '6 Pillars',
      deepDive: 'Deep Dive →',
      cardValTitle: 'Valuation vs Third-Party',
      cardLotTitle: 'Reconciled Lot & ADU / BCR',
      cardRiskTitle: 'Natural & Seismic Risk',
      cardEnvTitle: 'Environmental & Noise',
      cardSchoolTitle: 'Schools & Walkability',
      cardTaxTitle: 'County Tax & Deed',
      zoneCTitle: 'Ask AI to Visualize',
      studioSub: 'Interactive chart, KPI strip & structured data table',
      copyCsv: 'Copy CSV',
      copiedCsv: 'Copied!',
      expand: 'Expand',
      collapse: 'Exit Full',
      promptPlaceholder: 'Ask to chart anything: e.g. 20% down vs all-cash, 10-yr equity, seismic risk, comps $/sqft...',
      visualizeBtn: 'Visualize',
      generating: 'Charting...',
      modeInstant: 'Instant Grounded',
      modeAi: 'AI Enhanced',
      artifactPrefix: 'Artifact',
      watchoutLabel: 'Due Diligence Watchout',
      insightsLabel: 'Key Takeaways',
      presets: [
        { key: 'fin', label: '20% Down vs All-Cash + ADU', prompt: '20% Down vs All-Cash + ADU Rent' },
        { key: 'eq', label: '10-Yr Equity vs Renting', prompt: '10-Year Equity P10/P50/P90 vs Renting' },
        { key: 'risk', label: '{YEAR} CapEx & Seismic Risk', prompt: '{YEAR} CapEx, Seismic & Insurance Risk Breakdown' },
        { key: 'comp', label: 'Comps $/sqft vs Lot & Area', prompt: 'Comps $/sqft vs Lot & Living Area' }
      ],
      errRequest: 'Live sync paused; showing local grounded visualization.'
    },
    zh: {
      toggleLabel: '买房指挥舱',
      backToReport: '返回完整分章报告',
      hdrSubtitle: '单屏买房决策指挥舱与交互式AI图表工作室',
      zoneATitle: '实时出价与月供现金流测算',
      zoneABadge: '零延迟演算',
      offerPrice: '模拟出价',
      downPayment: '首付比例',
      interestRate: '30年按揭利率',
      quakeToggle: '加州地震险开关 (CEA)',
      deductible: '免赔额档位',
      aduRentOffset: 'ADU / 副屋月租抵扣',
      netMonthly: '扣除租金后净月支出',
      grossLabel: '月总支出 PITI+维护',
      cashToClose: '首付及过户资金',
      fiveYrEquity: '5年预计房屋净值',
      capRate: '毛/净租售比 (Cap)',
      resetSim: '恢复基准参数',
      chartDealBtn: '将此方案生成对比图 →',
      allCashTag: '全款方案',
      piSeg: '本息',
      taxSeg: '房产税',
      insSeg: '保险+地震',
      maintSeg: '维护/HOA',
      zoneBTitle: '六维尽调情报卡片',
      zoneBBadge: '一键深挖',
      deepDive: '深入章节 →',
      cardValTitle: '估值与三方估价对比',
      cardLotTitle: '核准占地与ADU覆盖率',
      cardRiskTitle: '自然灾害与抗震安全',
      cardEnvTitle: '环境污染与交通噪音',
      cardSchoolTitle: '周边学校与步行指数',
      cardTaxTitle: '县府税单与过户产权',
      zoneCTitle: 'AI 实时绘图与数据工作台',
      studioSub: '根据本房源数据动态生成图表、核心指标与对比表',
      copyCsv: '复制 CSV',
      copiedCsv: '已复制!',
      expand: '全屏展开',
      collapse: '退出全屏',
      promptPlaceholder: '输入任何图表分析请求，例如：对比20%首付和全款加ADU、十年净值预测、抗震与修缮风险...',
      visualizeBtn: '生成图表',
      generating: '生成中...',
      modeInstant: '即时精准测算',
      modeAi: 'AI 智能增强',
      artifactPrefix: '图表',
      watchoutLabel: '重点风控提示',
      insightsLabel: '核心结论',
      presets: [
        { key: 'fin', label: '20%首付 vs 全款+ADU', prompt: '对比20%首付与全款及ADU月租抵扣后的净月供' },
        { key: 'eq', label: '十年净值 vs 租房成本', prompt: '10-Year Equity P10/P50/P90 vs Renting 中文输出' },
        { key: 'risk', label: '{YEAR}修缮与抗震风险', prompt: '{YEAR} CapEx、地震抗震与房屋保险风险拆解' },
        { key: 'comp', label: '周边可比单价与面积', prompt: '对比周边可比成交每平方英尺单价与占地室内面积' }
      ],
      errRequest: '当前已无缝切换至本地精准数据图表。'
    },
    es: {
      toggleLabel: 'Centro de Mando',
      backToReport: 'Volver al Informe',
      hdrSubtitle: 'Panel Ejecutivo de Compra y Estudio de Gráficos IA',
      zoneATitle: 'Simulador en Vivo de Oferta y Flujo',
      zoneABadge: 'Tiempo Real',
      offerPrice: 'Precio de Oferta',
      downPayment: 'Enganche (%)',
      interestRate: 'Tasa Hipotecaria (30a)',
      quakeToggle: 'Seguro Sísmico (CEA)',
      deductible: 'Deducible',
      aduRentOffset: 'Renta ADU / Habitación',
      netMonthly: 'Gasto Mensual Neto',
      grossLabel: 'Bruto PITI+Mant',
      cashToClose: 'Efectivo al Cierre',
      fiveYrEquity: 'Equidad Est. 5 Años',
      capRate: 'Rendimiento / Cap Rate',
      resetSim: 'Restablecer',
      chartDealBtn: 'Graficar Escenario →',
      allCashTag: 'Contado',
      piSeg: 'Capital/Int.',
      taxSeg: 'Predial',
      insSeg: 'Seg+Sismo',
      maintSeg: 'Mant/HOA',
      zoneBTitle: 'Inteligencia Multifuente',
      zoneBBadge: '6 Pilares',
      deepDive: 'Ver Detalle →',
      cardValTitle: 'Valuación vs Referencias',
      cardLotTitle: 'Lote Conciliado y ADU',
      cardRiskTitle: 'Riesgo Natural y Sísmico',
      cardEnvTitle: 'Entorno y Ruido',
      cardSchoolTitle: 'Escuelas y Caminabilidad',
      cardTaxTitle: 'Impuestos y Escritura',
      zoneCTitle: 'Estudio Visual con IA',
      studioSub: 'Gráficos interactivos, KPIs y tabla CSV en segundos',
      copyCsv: 'Copiar CSV',
      copiedCsv: '¡Copiado!',
      expand: 'Ampliar',
      collapse: 'Contraer',
      promptPlaceholder: 'Pide cualquier gráfico: ej. 20% enganche vs contado, equidad 10 años, riesgo sísmico...',
      visualizeBtn: 'Visualizar',
      generating: 'Creando...',
      modeInstant: 'Cálculo Instantáneo',
      modeAi: 'Mejorado con IA',
      artifactPrefix: 'Vista',
      watchoutLabel: 'Alerta de Diligencia',
      insightsLabel: 'Puntos Clave',
      presets: [
        { key: 'fin', label: '20% vs Contado + ADU', prompt: 'Comparar 20% de enganche vs contado y renta ADU mensual' },
        { key: 'eq', label: 'Equidad 10 Años vs Renta', prompt: 'Equidad a 10 años P10/P50/P90 vs alquilar' },
        { key: 'risk', label: 'Riesgo {YEAR} y Sismo', prompt: 'Desglose de CapEx {YEAR}, riesgo sísmico y seguros' },
        { key: 'comp', label: 'Comparables $/sqft y Lote', prompt: 'Comparables $/sqft frente al lote y área habitable' }
      ],
      errRequest: 'Mostrando visualización analítica local con datos verificados.'
    }
  };

  var Core = g.OpenEstateCore || null;
  var _parsedDomReport = null;

  function getReportData() {
    if (Core && typeof Core.getRep === 'function') {
      var cr = Core.getRep();
      if (cr && (cr.meta || cr.facts || cr.modules)) return cr;
    }
    if (typeof R !== 'undefined' && R && (R.meta || R.facts || R.modules)) return R;
    if (g.R && (g.R.meta || g.R.facts || g.R.modules)) return g.R;
    if (g.__REPORT_DATA__ && (g.__REPORT_DATA__.meta || g.__REPORT_DATA__.facts)) return g.__REPORT_DATA__;
    if (_parsedDomReport && (_parsedDomReport.meta || _parsedDomReport.facts || _parsedDomReport.modules)) {
      return _parsedDomReport;
    }
    try {
      var dataEl = document.getElementById && document.getElementById('report-data');
      var txt = dataEl && dataEl.textContent ? String(dataEl.textContent).trim() : '';
      if (txt && txt.charAt(0) === '{') {
        _parsedDomReport = JSON.parse(txt);
        return _parsedDomReport;
      }
    } catch (_e) { /* ignore malformed script JSON */ }
    return {};
  }

  function currentLang() {
    if (Core && typeof Core.langCode === 'function') return Core.langCode();
    try {
      if (g.i18next && typeof g.i18next.language === 'string' && g.i18next.language) {
        var il = g.i18next.language.toLowerCase();
        if (il.indexOf('zh') === 0 || il === 'cn') return 'zh';
        if (il.indexOf('es') === 0) return 'es';
        if (il.indexOf('en') === 0) return 'en';
      }
    } catch (_e0) { /* ignore */ }
    var de = document.documentElement;
    var attr = (de && ((typeof de.getAttribute === 'function' && de.getAttribute('lang')) || de.lang)) || '';
    var saved = '';
    try {
      saved = (g.localStorage && g.localStorage.getItem('open_estate_lang')) || '';
    } catch (_e) { /* ignore */ }
    if (!saved) {
      try {
        var ck = String((document && document.cookie) || '');
        var m = ck.match(/(?:^|;\s*)open_estate_lang=([^;]+)/);
        if (m && m[1]) saved = decodeURIComponent(m[1].trim());
      } catch (_e2) { /* ignore */ }
    }
    var raw = String(attr || saved || 'en').toLowerCase();
    if (raw.indexOf('zh') === 0 || raw === 'cn') return 'zh';
    if (raw.indexOf('es') === 0) return 'es';
    return 'en';
  }

  function t(key) {
    var lang = currentLang();
    var dict = I18N[lang] || I18N.en;
    return Object.prototype.hasOwnProperty.call(dict, key) ? dict[key] : (I18N.en[key] || key);
  }

  function esc(str) {
    if (Core && typeof Core.esc === 'function') return Core.esc(str);
    return String(str == null ? '' : str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function optNum(val) {
    if (typeof val === 'number' && isFinite(val)) return val;
    if (typeof val === 'string' && val.trim() !== '') {
      var cleaned = val.replace(/[^0-9.-]/g, '');
      if (cleaned !== '' && cleaned !== '-' && cleaned !== '.') {
        var parsed = parseFloat(cleaned);
        if (isFinite(parsed)) return parsed;
      }
    }
    return null;
  }

  function posNum(val) {
    var n = optNum(val);
    return n != null && n > 0 ? n : null;
  }

  function num(val, fallback) {
    var n = optNum(val);
    if (n != null) return n;
    return fallback !== undefined ? fallback : 0;
  }

  function fmtUsd(v) {
    if (!isFinite(v)) return '$0';
    var neg = v < 0;
    var abs = Math.round(Math.abs(v));
    var s = abs.toLocaleString ? abs.toLocaleString('en-US') : String(abs);
    return (neg ? '-$' : '$') + s;
  }

  function fmtCompactUsd(v) {
    if (!isFinite(v) || Math.abs(v) < 1) return '$0';
    var neg = v < 0 ? '-' : '';
    var abs = Math.abs(v);
    if (abs >= 1000000) return neg + '$' + (abs / 1000000).toFixed(2) + 'M';
    if (abs >= 10000) return neg + '$' + Math.round(abs / 1000) + 'k';
    return fmtUsd(v);
  }

  function fmtInt(v) {
    var n = Math.round(num(v, 0));
    return n.toLocaleString ? n.toLocaleString('en-US') : String(n);
  }

  function calcMonthlyPI(principal, ratePct, years) {
    if (principal <= 0) return 0;
    var n = (years || 30) * 12;
    var r = Math.max(0, ratePct) / 100 / 12;
    if (r <= 1e-7) return principal / n;
    var p = Math.pow(1 + r, n);
    return (principal * r * p) / (p - 1);
  }

  function calcRemainingBalance(principal, ratePct, years, monthsPaid) {
    if (principal <= 0) return 0;
    var n = (years || 30) * 12;
    var k = Math.min(n, Math.max(0, monthsPaid || 0));
    var r = Math.max(0, ratePct) / 100 / 12;
    if (r <= 1e-7) return Math.max(0, principal * (1 - k / n));
    var pn = Math.pow(1 + r, n);
    var pk = Math.pow(1 + r, k);
    return Math.max(0, (principal * (pn - pk)) / (pn - 1));
  }

  function getBaselineModel() {
    var r = getReportData() || {};
    var meta = r.meta || {};
    var geo = r.geo || {};
    var facts = r.facts || {};
    var valMod = (r.modules && r.modules.valuation && r.modules.valuation.data) || {};
    var hazMod = (r.modules && r.modules.hazards && r.modules.hazards.data) || {};
    var envMod = (r.modules && r.modules.environment && r.modules.environment.data) || {};
    var civMod = (r.modules && r.modules.civic && r.modules.civic.data) || {};
    var own = valMod.ownership || {};
    var mo = own.monthly || {};
    var est = valMod.estimate || {};
    var df = r.decisionFactors || {};
    var lotAdu = df.lotAndAdu || {};
    var recon = (facts.countyRecords && facts.countyRecords.lotReconciliation) || {};
    var insQ = r.insuranceQuotes || r.insurance || {};
    var scores = (r.summary && r.summary.scores) || {};

    var estVal = posNum(est.value) || posNum(facts.avms && facts.avms.zillow && facts.avms.zillow.value) || posNum(facts.lastSale && facts.lastSale.price);
    var estLow = posNum(est.low);
    var estHigh = posNum(est.high);
    var sqft = posNum(facts.sqft);
    var reconLot = posNum(recon.selectedSqft) || posNum(facts.lotSqft) || posNum(lotAdu.lotSqft);
    var footprint = posNum(lotAdu.footprintSqft);
    var bcrPct = posNum(lotAdu.lotCoveragePct) || (reconLot && footprint ? Math.round((footprint / reconLot) * 1000) / 10 : null);
    var yearBuilt = facts.yearBuilt ? parseInt(facts.yearBuilt, 10) : null;
    if (!yearBuilt || isNaN(yearBuilt) || yearBuilt < 1750 || yearBuilt > 2035) yearBuilt = null;

    var taxRatePct = posNum(own.taxRatePct);
    var rate30 = posNum(own.rate30);
    var propTaxMonthly = optNum(mo.propertyTax) != null
      ? optNum(mo.propertyTax)
      : (estVal != null && taxRatePct != null ? Math.round((estVal * (taxRatePct / 100)) / 12) : null);
    var insMonthly = posNum(insQ.monthlyTotal) || posNum(mo.insurance);
    var quakeAddOn = optNum(insQ.specialtyAddOns && insQ.specialtyAddOns.earthquake && insQ.specialtyAddOns.earthquake.monthlyEst);
    var quakeMonthlyBase = quakeAddOn != null ? quakeAddOn : optNum(mo.earthquakeInsurance);
    var maintMonthly = optNum(mo.maintenance);
    var hoaMonthly = optNum(mo.hoa) != null ? optNum(mo.hoa) : optNum(facts.hoaMonthly);
    var rentEst = posNum(own.rentEstimate) || posNum(facts.avms && facts.avms.rentcast && facts.avms.rentcast.rent);
    var aduRent = posNum(lotAdu.estRentMonthly);

    function resolveHorizonDollar(h, dollarKey, pctKey) {
      var dv = posNum(h[dollarKey]);
      if (dv != null && dv > 1000) return dv;
      if (h[pctKey] !== undefined && h[pctKey] !== null) {
        var pv = optNum(h[pctKey]);
        if (pv != null) {
          if (Math.abs(pv) >= 1000) return pv;
          if (estVal != null && estVal > 0) return Math.max(0, Math.round(estVal * (1 + pv / 100)));
        }
      }
      return null;
    }

    var p50Growth5y = null;
    var p10_1 = null, p50_1 = null, p90_1 = null;
    var p10_3 = null, p50_3 = null, p90_3 = null;
    var p10_5 = null, p50_5 = null, p90_5 = null;
    var p10_10 = null, p50_10 = null, p90_10 = null;
    var lossProb5y = null;
    var horizons = (valMod.scenarios && valMod.scenarios.horizons) || [];
    for (var i = 0; i < horizons.length; i++) {
      var h = horizons[i];
      if (!h) continue;
      if (h.years === 1) {
        p10_1 = resolveHorizonDollar(h, 'bear', 'p10');
        p50_1 = resolveHorizonDollar(h, 'base', 'p50');
        p90_1 = resolveHorizonDollar(h, 'bull', 'p90');
      } else if (h.years === 3) {
        p10_3 = resolveHorizonDollar(h, 'bear', 'p10');
        p50_3 = resolveHorizonDollar(h, 'base', 'p50');
        p90_3 = resolveHorizonDollar(h, 'bull', 'p90');
      } else if (h.years === 5) {
        p10_5 = resolveHorizonDollar(h, 'bear', 'p10');
        p50_5 = resolveHorizonDollar(h, 'base', 'p50');
        p90_5 = resolveHorizonDollar(h, 'bull', 'p90');
        if (estVal != null && estVal > 0 && p50_5 != null && p50_5 > 0) p50Growth5y = p50_5 / estVal;
        lossProb5y = optNum(h.lossProbability);
      } else if (h.years === 10) {
        p10_10 = resolveHorizonDollar(h, 'bear', 'p10');
        p50_10 = resolveHorizonDollar(h, 'base', 'p50');
        p90_10 = resolveHorizonDollar(h, 'bull', 'p90');
      }
    }

    var compsRaw = valMod.comps || facts.comps || [];
    var comps = [];
    for (var cIdx = 0; cIdx < Math.min(6, compsRaw.length); cIdx++) {
      var cp = compsRaw[cIdx];
      if (!cp || cp.isSynthetic) continue;
      var cSq = posNum(cp.sqft);
      var cPrice = posNum(cp.price) || posNum(cp.salePrice);
      var cPpsf = posNum(cp.pricePerSqft) || posNum(cp.perSqft) || (cSq && cPrice ? Math.round(cPrice / cSq) : null);
      var cAddr = String(cp.address || ('Comp ' + (cIdx + 1))).split(',')[0].slice(0, 20);
      if (cPpsf != null || cPrice != null) {
        comps.push({ addr: cAddr, ppsf: cPpsf, sqft: cSq, lot: posNum(cp.lotSqft), price: cPrice });
      }
    }

    var hazScores = hazMod.scores || {};
    var envScores = envMod.scores || {};
    var civScores = civMod.scores || {};
    var schObj = civMod.schools || facts.schools || {};
    var schools = Array.isArray(schObj)
      ? schObj
      : (Array.isArray(facts.schools && facts.schools.assigned)
          ? facts.schools.assigned
          : (Array.isArray(schObj.assignedOrNearest)
              ? schObj.assignedOrNearest
              : (Array.isArray(schObj.public) ? schObj.public : [])));
    var schoolGrade = (civMod.schools && civMod.schools.summaryGrade) ? String(civMod.schools.summaryGrade) : null;
    var safetyScore = optNum(scores.safety) != null ? optNum(scores.safety) : optNum(hazScores.overall);
    var envScore = optNum(scores.environment) != null ? optNum(scores.environment) : optNum(envScores.overall);
    var walkScore = optNum(civScores.walkability) != null ? optNum(civScores.walkability) : optNum(facts.walkScore);
    var perSqft = posNum(est.perSqft) || (sqft && estVal ? Math.round(estVal / sqft) : null);

    return {
      slug: String(meta.slug || ''),
      address: String(meta.displayAddress || facts.address || 'Subject Property'),
      cityStateZip: [geo.city, geo.state, geo.zip].filter(Boolean).join(', '),
      beds: facts.beds != null ? facts.beds : null,
      baths: facts.baths != null ? facts.baths : null,
      sqft: sqft,
      lotSqft: reconLot,
      bcrPct: bcrPct,
      yearBuilt: yearBuilt,
      estVal: estVal,
      estLow: estLow,
      estHigh: estHigh,
      zestimate: posNum(facts.avms && facts.avms.zillow && facts.avms.zillow.value),
      perSqft: perSqft,
      rate30: rate30,
      taxRatePct: taxRatePct,
      propTaxMonthly: propTaxMonthly,
      insMonthly: insMonthly,
      quakeMonthlyBase: quakeMonthlyBase,
      maintMonthly: maintMonthly,
      hoaMonthly: hoaMonthly,
      rentEst: rentEst,
      aduRent: aduRent,
      aduFeasibility: lotAdu.feasibility ? String(lotAdu.feasibility) : null,
      p50Growth5y: p50Growth5y,
      p10_1: p10_1,
      p50_1: p50_1,
      p90_1: p90_1,
      p10_3: p10_3,
      p50_3: p50_3,
      p90_3: p90_3,
      p10_5: p10_5,
      p50_5: p50_5,
      p90_5: p90_5,
      p10_10: p10_10,
      p50_10: p50_10,
      p90_10: p90_10,
      lossProb5y: lossProb5y,
      overallScore: optNum(scores.overall),
      invScore: optNum(scores.investment),
      safetyScore: safetyScore,
      envScore: envScore,
      livScore: optNum(scores.livability),
      floodScore: optNum(hazScores.flood),
      quakeScore: optNum(hazScores.earthquake),
      fireScore: optNum(hazScores.wildfire),
      floodZone: (hazMod.flood && hazMod.flood.zone) ? String(hazMod.flood.zone) : null,
      sdc: (hazMod.earthquake && hazMod.earthquake.designMaps && hazMod.earthquake.designMaps.sdc) ? String(hazMod.earthquake.designMaps.sdc) : null,
      noiseLabel: (envMod.noise && envMod.noise.atProperty && envMod.noise.atProperty.all && envMod.noise.atProperty.all.label) ? String(envMod.noise.atProperty.all.label) : null,
      envCleanScore: optNum(envScores.contamination),
      airScore: optNum(envScores.air),
      waterScore: optNum(envScores.water),
      walkScore: walkScore,
      schoolsCount: schools.length || 0,
      schoolGrade: schoolGrade,
      topSchool: (schools[0] && schools[0].name) ? (schools[0].name + (schools[0].rating ? (' (' + schools[0].rating + '/10)') : '')) : null,
      apn: facts.apn ? String(facts.apn) : null,
      lastSalePrice: posNum(facts.lastSale && facts.lastSale.price),
      lastSaleDate: (facts.lastSale && facts.lastSale.date) ? String(facts.lastSale.date) : null,
      comps: comps,
      raw: r
    };
  }

  function unavailableArtifact(id, title, address, note) {
    return {
      id: id,
      title: title,
      subtitle: address,
      chartType: 'bar',
      unit: '',
      categories: [],
      series: [],
      kpis: [],
      tableHeaders: [],
      tableRows: [],
      insights: [note],
      watchout: '',
      synthesisMode: 'instant'
    };
  }

  // ---- Local Instant Artifact Fallback & Seed Engine ----
  function buildLocalArtifact(prompt) {
    var d = getBaselineModel();
    var p = String(prompt || '').toLowerCase();
    var lang = currentLang();
    var yearStr = d.yearBuilt ? String(d.yearBuilt) : null;

    if (p.indexOf('p10') >= 0 || p.indexOf('equity') >= 0 || p.indexOf('10-year') >= 0 || p.indexOf('净值') >= 0 || p.indexOf('equidad') >= 0) {
      var eqTitle = lang === 'zh' ? '十年房产净值(P10/P50/P90)与累计租金对比' : (lang === 'es' ? 'Equidad a 10 Años P10/P50/P90 vs Renta' : '10-Year Equity P10/P50/P90 vs Renting');
      if (d.estVal == null || d.rate30 == null || d.p50_1 == null || d.p50_3 == null || d.p50_5 == null || d.p50_10 == null) {
        return unavailableArtifact('local-equity', eqTitle, d.address, 'Scenario horizon projections or mortgage rate data are unavailable in this report.');
      }
      var price = d.estVal;
      var loan0 = price * 0.8;
      var down20 = price * 0.2;
      var bal1 = calcRemainingBalance(loan0, d.rate30, 30, 12);
      var bal3 = calcRemainingBalance(loan0, d.rate30, 30, 36);
      var bal5 = calcRemainingBalance(loan0, d.rate30, 30, 60);
      var bal10 = calcRemainingBalance(loan0, d.rate30, 30, 120);
      var eqP50 = [Math.round(down20), Math.round(Math.max(0, d.p50_1 - bal1)), Math.round(Math.max(0, d.p50_3 - bal3)), Math.round(Math.max(0, d.p50_5 - bal5)), Math.round(Math.max(0, d.p50_10 - bal10))];
      var hasP10 = d.p10_1 != null && d.p10_3 != null && d.p10_5 != null && d.p10_10 != null;
      var hasP90 = d.p90_1 != null && d.p90_3 != null && d.p90_5 != null && d.p90_10 != null;
      var eqP10 = hasP10 ? [Math.round(down20), Math.round(Math.max(0, d.p10_1 - bal1)), Math.round(Math.max(0, d.p10_3 - bal3)), Math.round(Math.max(0, d.p10_5 - bal5)), Math.round(Math.max(0, d.p10_10 - bal10))] : null;
      var eqP90 = hasP90 ? [Math.round(down20), Math.round(Math.max(0, d.p90_1 - bal1)), Math.round(Math.max(0, d.p90_3 - bal3)), Math.round(Math.max(0, d.p90_5 - bal5)), Math.round(Math.max(0, d.p90_10 - bal10))] : null;
      var rentSunk = null;
      if (d.rentEst != null) {
        var yrRent = d.rentEst * 12;
        var cumRent = function (yrs) {
          var s = 0;
          for (var y = 0; y < yrs; y++) s += yrRent * Math.pow(1.032, y);
          return Math.round(s);
        };
        rentSunk = [0, cumRent(1), cumRent(3), cumRent(5), cumRent(10)];
      }
      var catsEq = lang === 'zh' ? ['交割日', '第1年', '第3年', '第5年', '第10年'] : (lang === 'es' ? ['Cierre', 'Año 1', 'Año 3', 'Año 5', 'Año 10'] : ['Close', 'Year 1', 'Year 3', 'Year 5', 'Year 10']);
      var eqSeries = [];
      if (eqP90) eqSeries.push({ name: lang === 'zh' ? 'P90 乐观净值' : 'P90 Bull Equity', values: eqP90 });
      eqSeries.push({ name: 'P50 Base Equity', values: eqP50 });
      if (eqP10) eqSeries.push({ name: 'P10 Bear Equity', values: eqP10 });
      if (rentSunk) eqSeries.push({ name: lang === 'zh' ? '累计纯租金支出' : 'Cumulative Sunk Rent', values: rentSunk });
      var eqKpis = [
        { label: lang === 'zh' ? '5年 P50 净值' : 'Yr 5 P50 Equity', value: fmtCompactUsd(eqP50[3]), delta: eqP10 ? ('P10 ' + fmtCompactUsd(eqP10[3])) : '', tone: 'info' },
        { label: lang === 'zh' ? '10年 P50 净值' : 'Yr 10 P50 Equity', value: fmtCompactUsd(eqP50[4]), delta: eqP90 ? ('P90 ' + fmtCompactUsd(eqP90[4])) : '', tone: 'good' }
      ];
      if (rentSunk) {
        eqKpis.push({ label: lang === 'zh' ? '10年沉没租金' : '10-Yr Sunk Rent', value: fmtCompactUsd(rentSunk[4]), delta: fmtUsd(d.rentEst) + '/mo', tone: 'warn' });
      }
      if (d.lossProb5y != null) {
        eqKpis.push({ label: lang === 'zh' ? '5年负收益概率' : '5-Yr Loss Odds', value: d.lossProb5y.toFixed(1) + '%', delta: 'ZIP dist.', tone: d.lossProb5y <= 15 ? 'good' : 'warn' });
      }
      var eqHeaders = [lang === 'zh' ? '时间段' : 'Horizon'];
      if (eqP10) eqHeaders.push('P10');
      eqHeaders.push('P50');
      if (rentSunk) eqHeaders.push(lang === 'zh' ? '累计租金' : 'Sunk Rent');
      var eqRows = catsEq.map(function (c, idx) {
        var row = [c];
        if (eqP10) row.push(fmtUsd(eqP10[idx]));
        row.push(fmtUsd(eqP50[idx]));
        if (rentSunk) row.push(fmtUsd(rentSunk[idx]));
        return row;
      });
      var eqInsights = [
        '20% mortgage principal amortization drops debt from ' + fmtCompactUsd(loan0) + ' to ' + fmtCompactUsd(bal10) + ' by Year 10.'
      ];
      if (rentSunk) {
        eqInsights.unshift('Base P50 equity reaches ' + fmtCompactUsd(eqP50[4]) + ' at Year 10 while renting burns ' + fmtCompactUsd(rentSunk[4]) + '.');
      }
      return {
        id: 'local-equity',
        title: eqTitle,
        subtitle: d.address + ' · 20% Down (' + fmtCompactUsd(down20) + ')',
        chartType: 'line',
        unit: '$',
        categories: catsEq,
        series: eqSeries,
        kpis: eqKpis,
        tableHeaders: eqHeaders,
        tableRows: eqRows,
        insights: eqInsights,
        watchout: 'Holding shorter than 3-5 years faces resale transaction friction.',
        synthesisMode: 'instant'
      };
    }

    if (p.indexOf('capex') >= 0 || p.indexOf('seismic') >= 0 || p.indexOf('risk') >= 0 || p.indexOf('抗震') >= 0 || p.indexOf('riesgo') >= 0) {
      var rkTitle = lang === 'zh' ? ((yearStr ? yearStr + '年' : '') + '修缮、抗震与自然灾害风险拆解') : ((yearStr ? yearStr + ' ' : '') + 'Seismic & Hazard Risk Breakdown');
      var rkCats = [];
      var rkVals = [];
      var rkRows = [];
      if (d.quakeScore != null) {
        var seisIdx = Math.round(Math.max(0, Math.min(100, 100 - d.quakeScore)));
        rkCats.push('Seismic');
        rkVals.push(seisIdx);
        rkRows.push(['Seismic', seisIdx + '/100', Math.round(d.quakeScore) + '/100']);
      }
      if (d.floodScore != null) {
        var floodIdx = Math.round(Math.max(0, Math.min(100, 100 - d.floodScore)));
        rkCats.push('Flood');
        rkVals.push(floodIdx);
        rkRows.push(['Flood', floodIdx + '/100', Math.round(d.floodScore) + '/100']);
      }
      if (d.fireScore != null) {
        var fireIdx = Math.round(Math.max(0, Math.min(100, 100 - d.fireScore)));
        rkCats.push('Wildfire');
        rkVals.push(fireIdx);
        rkRows.push(['Wildfire', fireIdx + '/100', Math.round(d.fireScore) + '/100']);
      }
      if (d.envScore != null) {
        var envIdx = Math.round(Math.max(0, Math.min(100, 100 - d.envScore)));
        rkCats.push('Environment');
        rkVals.push(envIdx);
        rkRows.push(['Environment', envIdx + '/100', Math.round(d.envScore) + '/100']);
      }
      if (!rkCats.length) {
        return unavailableArtifact('local-risk', rkTitle, d.address, 'Natural hazard and environmental scores are unavailable in this report.');
      }
      var rkKpis = [];
      if (d.yearBuilt != null) {
        var age = Math.max(0, new Date().getFullYear() - d.yearBuilt);
        rkKpis.push({ label: 'Built / Age', value: d.yearBuilt + ' (' + age + 'y)', delta: 'Inspect systems', tone: age >= 60 ? 'warn' : 'info' });
      }
      if (d.quakeScore != null) {
        rkKpis.push({ label: 'Seismic Score', value: Math.round(d.quakeScore) + '/100', delta: d.sdc ? ('SDC ' + d.sdc) : '', tone: d.quakeScore < 60 ? 'warn' : 'good' });
      }
      if (d.insMonthly != null) {
        var totInsYr = (d.insMonthly + (d.quakeMonthlyBase || 0)) * 12;
        rkKpis.push({ label: 'Annual Ins' + (d.quakeMonthlyBase ? '+Quake' : ''), value: fmtUsd(totInsYr), delta: d.quakeMonthlyBase ? (fmtUsd(d.quakeMonthlyBase) + '/mo quake') : '', tone: 'info' });
      }
      var rkInsights = [];
      if (yearStr) {
        rkInsights.push('Built ' + yearStr + ': inspect foundation anchor bolting, electrical panel, plumbing, and roof remaining life.');
      }
      if (d.floodScore != null && d.fireScore != null) {
        rkInsights.push('Flood (' + Math.round(d.floodScore) + '/100) and wildfire (' + Math.round(d.fireScore) + '/100) scores reflect report hazard screening.');
      }
      return {
        id: 'local-risk',
        title: rkTitle,
        subtitle: d.address + (d.insMonthly != null ? (' · ' + fmtUsd((d.insMonthly + (d.quakeMonthlyBase || 0)) * 12) + '/yr Ins') : ''),
        chartType: 'horizontal_bar',
        unit: 'Risk Index (0-100)',
        categories: rkCats,
        series: [
          { name: lang === 'zh' ? '本房源风险值' : 'This Property (100 - Safety)', values: rkVals }
        ],
        kpis: rkKpis,
        tableHeaders: ['Factor', 'Risk Index', 'Safety Score'],
        tableRows: rkRows,
        insights: rkInsights,
        watchout: 'Obtain a bindable homeowners insurance quote before removing inspection contingencies.',
        synthesisMode: 'instant'
      };
    }

    if (p.indexOf('comp') >= 0 || p.indexOf('sqft') >= 0 || p.indexOf('可比') >= 0 || p.indexOf('单价') >= 0) {
      var cpTitle = lang === 'zh' ? '可比成交单价 ($/sqft) 与面积对比' : 'Comps $/sqft vs Lot & Living Area';
      var validComps = d.comps.filter(function (c) { return c.ppsf != null && c.ppsf > 0; });
      if (d.perSqft == null && !validComps.length) {
        return unavailableArtifact('local-comps', cpTitle, d.address, 'Comparable sales and $/sqft metrics are unavailable in this report.');
      }
      var catsCp = [];
      var ppsfArr = [];
      if (d.perSqft != null) {
        catsCp.push('Subject');
        ppsfArr.push(Math.round(d.perSqft));
      }
      validComps.forEach(function (c) {
        catsCp.push(c.addr);
        ppsfArr.push(Math.round(c.ppsf));
      });
      if (!validComps.length && d.sqft != null && d.estLow != null && d.estHigh != null) {
        catsCp.push('Est Low', 'Est High');
        ppsfArr.push(Math.round(d.estLow / d.sqft), Math.round(d.estHigh / d.sqft));
      }
      var compMean = validComps.length
        ? Math.round(validComps.reduce(function (acc, x) { return acc + x.ppsf; }, 0) / validComps.length)
        : null;
      var seriesCp = [{ name: '$/sqft', values: ppsfArr }];
      if (compMean != null) {
        seriesCp.push({ name: 'Comps Avg', values: catsCp.map(function () { return compMean; }) });
      }
      var rowsCp = [];
      if (d.perSqft != null || d.estVal != null) {
        rowsCp.push([
          'Subject',
          d.estVal != null ? fmtUsd(d.estVal) : '—',
          d.sqft != null ? fmtInt(d.sqft) : '—',
          d.lotSqft != null ? fmtInt(d.lotSqft) : '—',
          d.perSqft != null ? ('$' + Math.round(d.perSqft)) : '—'
        ]);
      }
      d.comps.forEach(function (c) {
        rowsCp.push([
          c.addr,
          c.price != null ? fmtUsd(c.price) : '—',
          c.sqft != null ? fmtInt(c.sqft) : '—',
          c.lot != null ? fmtInt(c.lot) : '—',
          c.ppsf != null ? ('$' + Math.round(c.ppsf)) : '—'
        ]);
      });
      var cpKpis = [];
      if (d.perSqft != null) {
        cpKpis.push({
          label: 'Subject $/sqft',
          value: '$' + Math.round(d.perSqft),
          delta: compMean != null && compMean > 0 ? ((d.perSqft - compMean >= 0 ? '+' : '') + (((d.perSqft - compMean) / compMean) * 100).toFixed(1) + '%') : '',
          tone: 'info'
        });
      }
      if (compMean != null) {
        cpKpis.push({ label: 'Comps Avg', value: '$' + compMean, delta: validComps.length + ' comps', tone: 'info' });
      }
      if (d.sqft != null) {
        cpKpis.push({ label: 'Living / Lot', value: fmtInt(d.sqft) + ' sqft', delta: d.lotSqft != null ? ('Lot ' + fmtInt(d.lotSqft)) : '', tone: 'good' });
      }
      return {
        id: 'local-comps',
        title: cpTitle,
        subtitle: d.address + (d.perSqft != null ? (' · Subject $' + Math.round(d.perSqft) + '/sqft') : ''),
        chartType: 'bar',
        unit: '$/sqft',
        categories: catsCp,
        series: seriesCp,
        kpis: cpKpis,
        tableHeaders: ['Property', 'Price', 'Living', 'Lot', '$/sqft'],
        tableRows: rowsCp,
        insights: compMean != null && d.perSqft != null
          ? ['Subject prices at $' + Math.round(d.perSqft) + '/sqft vs nearby comp average $' + compMean + '/sqft.']
          : ['Confirm all living area square footage reflects permitted structures on county records.'],
        watchout: 'Confirm all living area square footage reflects permitted structures on county records.',
        synthesisMode: 'instant'
      };
    }

    // Default: Financing 20% Down vs All-Cash + ADU Rent
    var finTitle = lang === 'zh' ? '20%首付与全款 + ADU租金抵扣月供对比' : (lang === 'es' ? '20% Enganche vs Contado + Renta ADU' : '20% Down vs All-Cash + ADU Rent');
    if (d.estVal == null || d.rate30 == null) {
      return unavailableArtifact('local-fin', finTitle, d.address, 'Valuation or mortgage rate data is unavailable in this report.');
    }
    var pBase = d.estVal;
    var carryBase = (d.propTaxMonthly || 0) + (d.insMonthly || 0) + (d.maintMonthly || 0) + (d.hoaMonthly || 0);
    var aduOff = d.aduRent || 0;
    var pi20 = Math.round(calcMonthlyPI(pBase * 0.8, d.rate30, 30));
    var pi30 = Math.round(calcMonthlyPI(pBase * 0.7, d.rate30, 30));
    var g20 = Math.round(pi20 + carryBase);
    var gCash = Math.round(carryBase);
    var n20Adu = Math.max(0, g20 - aduOff);
    var catsFin = [];
    var grossVals = [];
    var netVals = [];
    var finRows = [];

    catsFin.push(lang === 'zh' ? '20%首付' : (lang === 'es' ? '20% Base' : '20% Down Base'));
    grossVals.push(g20);
    netVals.push(g20);
    finRows.push([catsFin[catsFin.length - 1], fmtUsd(pBase * 0.2), fmtUsd(g20), fmtUsd(g20)]);

    if (aduOff > 0) {
      catsFin.push(lang === 'zh' ? '20%+ADU租' : (lang === 'es' ? '20% + ADU' : '20% + ADU Rent'));
      grossVals.push(g20);
      netVals.push(n20Adu);
      finRows.push([catsFin[catsFin.length - 1], fmtUsd(pBase * 0.2), fmtUsd(g20), fmtUsd(n20Adu)]);
    }

    if (d.quakeMonthlyBase != null) {
      var g30q = Math.round(pi30 + carryBase + d.quakeMonthlyBase);
      var n30qAdu = Math.max(0, g30q - aduOff);
      catsFin.push(lang === 'zh' ? '30%+地震险' : (lang === 'es' ? '30%+Sismo' : '30% + Quake'));
      grossVals.push(g30q);
      netVals.push(n30qAdu);
      finRows.push([catsFin[catsFin.length - 1], fmtUsd(pBase * 0.3), fmtUsd(g30q), fmtUsd(n30qAdu)]);
    }

    catsFin.push(lang === 'zh' ? '全款方案' : (lang === 'es' ? 'Contado' : 'All-Cash Offer'));
    grossVals.push(gCash);
    netVals.push(gCash);
    finRows.push([catsFin[catsFin.length - 1], fmtUsd(pBase), fmtUsd(gCash), fmtUsd(gCash)]);

    var finSeries = [{ name: lang === 'zh' ? '月总支出' : 'Gross Outlay', values: grossVals }];
    if (aduOff > 0) {
      finSeries.push({ name: lang === 'zh' ? '净月支出' : 'Net After ADU', values: netVals });
    }
    var finKpis = [
      { label: lang === 'zh' ? '20%首付月支' : '20% Down/Mo', value: fmtUsd(g20), delta: 'Down ' + fmtCompactUsd(pBase * 0.2), tone: 'info' },
      { label: lang === 'zh' ? '全款月持有成本' : 'All-Cash Monthly', value: fmtUsd(gCash), delta: 'No mortgage P&I', tone: 'good' }
    ];
    if (aduOff > 0) {
      finKpis.splice(1, 0, { label: lang === 'zh' ? 'ADU抵扣后净月供' : 'With ADU Offset', value: fmtUsd(n20Adu), delta: '-' + fmtUsd(aduOff) + '/mo', tone: 'good' });
    }

    return {
      id: 'local-fin',
      title: finTitle,
      subtitle: d.address + ' · ' + fmtUsd(pBase) + ' @ ' + d.rate30.toFixed(2) + '%',
      chartType: 'bar',
      unit: '$/mo',
      categories: catsFin,
      series: finSeries,
      kpis: finKpis,
      tableHeaders: ['Strategy', 'Principal Cash', 'Gross $/mo', 'Net $/mo'],
      tableRows: finRows,
      insights: [
        'All-cash removes ' + fmtUsd(pi20) + '/mo of P&I debt service, reducing carrying cost to ' + fmtUsd(gCash) + '/mo.'
      ],
      watchout: 'Confirm city setbacks and utility tie-in budgets before underwriting ADU cashflow.',
      synthesisMode: 'instant'
    };
  }

  var state = {
    active: false,
    initialized: false,
    snapshotParked: null,
    sim: null,
    artifacts: [],
    activeArtifactIdx: 0,
    fullscreen: false,
    busy: false,
    lastLang: currentLang(),
    chartInstance: null,
    observerInstalled: false,
    langWrapped: false
  };

  function resetSimState() {
    var d = getBaselineModel();
    state.sim = {
      offer: d.estVal != null ? Math.round(d.estVal / 5000) * 5000 : null,
      downPct: 20,
      ratePct: d.rate30 != null ? Math.round(d.rate30 * 20) / 20 : null,
      quakeOn: d.quakeMonthlyBase != null && d.quakeMonthlyBase > 0,
      deductiblePct: 15,
      rentOffset: d.aduRent != null ? Math.round(d.aduRent / 50) * 50 : 0
    };
  }

  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return;
    var css = [
      'body.db-cmd-active .layout{max-width:1420px;grid-template-columns:minmax(0,1fr);gap:14px}',
      'body.db-cmd-active .chap-ws-outer{max-width:1420px}',
      'body.db-cmd-active #mediaBento,body.db-cmd-active #gallery,body.db-cmd-active #side,body.db-cmd-active #chapterNextNav,body.db-cmd-active main>section.sec{display:none!important}',
      'body.db-cmd-active #chapterWorkspaceBar .chap-btn.on:not(#toggleCommandCenterBtn){background:var(--card,#fff)!important;color:var(--ink2,#334155)!important;border-color:var(--line,#e2e8f0)!important;box-shadow:none!important}',
      'body.db-cmd-active #chapterWorkspaceBar .chap-btn.on:not(#toggleCommandCenterBtn) .chap-num{background:var(--soft,#f1f5f9)!important;color:var(--ink2,#334155)!important}',
      '.db-cmd-toggle{display:inline-flex;align-items:center;gap:5px;padding:4.5px 10px;border-radius:9px;border:1px solid #0f766e;background:linear-gradient(135deg,#0f766e,#0d9488);color:#fff!important;font-weight:700;font-size:11.4px;cursor:pointer;white-space:nowrap;flex:0 0 auto;box-shadow:0 2px 6px rgba(15,118,110,.22);transition:transform .12s,box-shadow .15s}',
      '.db-cmd-toggle:hover{transform:translateY(-1px);box-shadow:0 4px 10px rgba(15,118,110,.3)}',
      '.db-cmd-toggle.is-active{background:linear-gradient(135deg,#134e4a,#0f766e);box-shadow:0 0 0 2px #99f6e4}',
      '.db-cmd-shell{display:block;width:100%;max-width:100%;box-sizing:border-box;font-family:inherit;color:var(--ink,#0f172a)}',
      '.db-cmd-shell[hidden]{display:none!important}',
      '.db-cmd-topbar{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:10px;padding:12px 16px;margin-bottom:14px;border-radius:14px;background:linear-gradient(135deg,#0f172a 0%,#134e4a 100%);color:#f8fafc;box-shadow:0 6px 20px rgba(15,23,42,.12)}',
      '.db-cmd-top-left{display:flex;flex-direction:column;gap:2px;min-width:0}',
      '.db-cmd-addr{font-size:16px;font-weight:800;letter-spacing:-.01em;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
      '.db-cmd-sub{font-size:12px;color:#cbd5e1}',
      '.db-cmd-top-pills{display:flex;flex-wrap:wrap;align-items:center;gap:8px}',
      '.db-cmd-pill{display:inline-flex;align-items:center;gap:5px;padding:5px 10px;border-radius:999px;background:rgba(255,255,255,.12);font-size:12px;font-weight:700;color:#fff;border:1px solid rgba(255,255,255,.16)}',
      '.db-cmd-back-btn{padding:6px 12px;border-radius:9px;border:1px solid rgba(255,255,255,.35);background:rgba(255,255,255,.12);color:#fff;font-size:12px;font-weight:700;cursor:pointer}',
      '.db-cmd-back-btn:hover{background:rgba(255,255,255,.22)}',
      '.db-cmd-grid{display:grid;grid-template-columns:minmax(0,1fr);gap:14px;align-items:start}',
      '@media(min-width:780px){.db-cmd-grid{grid-template-columns:minmax(290px,360px) minmax(0,1fr)}.db-zone-c{grid-column:1 / -1}}',
      '@media(min-width:1220px){.db-cmd-grid{grid-template-columns:minmax(285px,335px) minmax(350px,1.05fr) minmax(385px,1.25fr)}.db-zone-c{grid-column:auto}}',
      '.db-panel{background:var(--card,#fff);border:1px solid var(--line,#e2e8f0);border-radius:14px;padding:14px;box-shadow:0 2px 10px rgba(15,23,42,.04);min-width:0;box-sizing:border-box}',
      '.db-panel-hd{display:flex;align-items:center;justify-content:space-between;gap:8px;margin-bottom:10px;padding-bottom:8px;border-bottom:1px solid var(--line,#e2e8f0)}',
      '.db-panel-title{font-size:14px;font-weight:800;color:var(--ink,#0f172a);margin:0;display:flex;align-items:center;gap:6px}',
      '.db-badge{font-size:10.5px;font-weight:700;padding:2px 8px;border-radius:999px;background:#f0fdfa;color:#0f766e;border:1px solid #ccfbf1;white-space:nowrap}',
      '.db-sim-hero{padding:11px 12px;border-radius:12px;background:linear-gradient(135deg,#f0fdfa,#ecfeff);border:1px solid #99f6e4;margin-bottom:10px}',
      '.db-sim-hero-lbl{font-size:11px;font-weight:700;text-transform:uppercase;letter-spacing:.04em;color:#0f766e}',
      '.db-sim-hero-val{font-size:26px;font-weight:800;color:#0f172a;line-height:1.15;margin:2px 0}',
      '.db-sim-hero-sub{font-size:11.5px;color:#475569}',
      '.db-sim-stack{display:flex;height:9px;border-radius:999px;overflow:hidden;background:#e2e8f0;margin:8px 0 6px}',
      '.db-sim-seg{height:100%;transition:width .12s ease}',
      '.db-sim-leg{display:flex;flex-wrap:wrap;gap:6px 10px;font-size:11px;color:#475569}',
      '.db-sim-leg i{display:inline-block;width:8px;height:8px;border-radius:2px;margin-right:3px}',
      '.db-sim-ctrl{margin-bottom:9px}',
      '.db-sim-ctrl-row{display:flex;justify-content:space-between;align-items:center;font-size:12px;font-weight:700;margin-bottom:3px;color:#334155}',
      '.db-sim-ctrl-val{color:#0f766e;font-variant-numeric:tabular-nums}',
      '.db-sim-range{width:100%;accent-color:#0f766e;cursor:pointer;margin:0}',
      '.db-sim-toggle-row{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:7px 9px;border-radius:9px;background:#f8fafc;border:1px solid #e2e8f0;margin-bottom:9px;font-size:12px}',
      '.db-sim-toggle-lbl{display:flex;align-items:center;gap:6px;font-weight:700;color:#1e293b;cursor:pointer}',
      '.db-sim-select{font-size:11.5px;padding:3px 6px;border-radius:6px;border:1px solid #cbd5e1;background:#fff;color:#0f172a}',
      '.db-sim-metrics{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:6px;margin:10px 0}',
      '.db-sim-mbox{padding:7px 8px;border-radius:9px;background:#f8fafc;border:1px solid #e2e8f0;min-width:0}',
      '.db-sim-mlbl{font-size:10px;font-weight:700;color:#64748b;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
      '.db-sim-mval{font-size:13px;font-weight:800;color:#0f172a;margin-top:2px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
      '.db-sim-actions{display:flex;gap:8px;flex-wrap:wrap}',
      '.db-btn-sec{padding:6px 10px;border-radius:8px;border:1px solid #cbd5e1;background:#fff;color:#334155;font-size:11.5px;font-weight:700;cursor:pointer}',
      '.db-btn-pri{flex:1;padding:7px 11px;border-radius:8px;border:0;background:#0f766e;color:#fff;font-size:11.5px;font-weight:700;cursor:pointer}',
      '.db-btn-pri:hover{background:#0d9488}',
      '.db-bento-grid{display:grid;grid-template-columns:minmax(0,1fr);gap:9px}',
      '@media(min-width:520px){.db-bento-grid{grid-template-columns:repeat(2,minmax(0,1fr))}}',
      '.db-bcard{display:flex;flex-direction:column;justify-content:space-between;gap:6px;padding:10px 11px;border-radius:11px;border:1px solid #e2e8f0;background:#f8fafc;min-width:0}',
      '.db-bcard-top{display:flex;align-items:flex-start;justify-content:space-between;gap:6px}',
      '.db-bcard-title{font-size:11.5px;font-weight:800;color:#334155}',
      '.db-tone-pill{font-size:10px;font-weight:700;padding:1px 6px;border-radius:999px;white-space:nowrap}',
      '.db-tone-good{background:#dcfce7;color:#166534}',
      '.db-tone-info{background:#e0f2fe;color:#075985}',
      '.db-tone-warn{background:#fef3c7;color:#92400e}',
      '.db-bcard-main{font-size:16px;font-weight:800;color:#0f172a;line-height:1.2}',
      '.db-bcard-desc{font-size:11.5px;color:#475569;line-height:1.38}',
      '.db-bcard-jump{align-self:flex-start;padding:3px 8px;border-radius:7px;border:1px solid #cbd5e1;background:#fff;color:#0f766e;font-size:11px;font-weight:700;cursor:pointer}',
      '.db-bcard-jump:hover{background:#f0fdfa;border-color:#0f766e}',
      '.db-art-head{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:8px;margin-bottom:8px}',
      '.db-art-tools{display:flex;flex-wrap:wrap;align-items:center;gap:6px}',
      '.db-art-presets{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:8px}',
      '.db-chip-btn{padding:5px 9px;border-radius:999px;border:1px solid #cbd5e1;background:#f8fafc;color:#1e293b;font-size:11.5px;font-weight:700;cursor:pointer;white-space:nowrap;max-width:100%;overflow:hidden;text-overflow:ellipsis}',
      '.db-chip-btn:hover,.db-chip-btn.is-selected{border-color:#0f766e;background:#f0fdfa;color:#0f766e}',
      '.db-prompt-row{display:flex;gap:6px;margin-bottom:8px}',
      '.db-prompt-inp{flex:1;min-width:0;padding:8px 10px;border-radius:9px;border:1px solid #cbd5e1;font-size:12.5px;color:#0f172a;background:#fff}',
      '.db-prompt-inp:focus{outline:2px solid #0f766e;outline-offset:1px}',
      '.db-prompt-go{padding:8px 14px;border-radius:9px;border:0;background:#0f766e;color:#fff;font-size:12.5px;font-weight:800;cursor:pointer;white-space:nowrap}',
      '.db-prompt-go:disabled{opacity:.6;cursor:wait}',
      '.db-art-tabs{display:flex;flex-wrap:wrap;gap:5px;margin-bottom:8px;max-width:100%}',
      '.db-tab-pill{padding:4px 9px;border-radius:8px;border:1px solid #e2e8f0;background:#f8fafc;color:#475569;font-size:11px;font-weight:700;cursor:pointer;white-space:nowrap}',
      '.db-tab-pill.is-active{background:#0f172a;color:#fff;border-color:#0f172a}',
      '.db-art-kpis{display:grid;grid-template-columns:repeat(auto-fit,minmax(115px,1fr));gap:6px;margin-bottom:8px}',
      '.db-art-kpi{padding:7px 9px;border-radius:9px;background:#f8fafc;border:1px solid #e2e8f0;min-width:0}',
      '.db-art-kpi-l{font-size:10.5px;color:#64748b;font-weight:700;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
      '.db-art-kpi-v{font-size:14px;font-weight:800;color:#0f172a;margin-top:1px}',
      '.db-art-kpi-d{font-size:10.5px;font-weight:700;color:#0f766e}',
      '.db-art-title{font-size:14px;font-weight:800;color:#0f172a;margin:0 0 2px}',
      '.db-art-sub{font-size:11.5px;color:#64748b;margin:0 0 8px}',
      '.db-echart-box{width:100%;height:255px;min-height:220px;border-radius:10px;background:#fff;border:1px solid #f1f5f9}',
      '.db-art-table-wrap{max-width:100%;overflow-x:auto;margin-top:8px;border:1px solid #e2e8f0;border-radius:9px}',
      '.db-art-table{width:100%;border-collapse:collapse;font-size:11.5px;text-align:left}',
      '.db-art-table th{background:#f8fafc;padding:6px 8px;font-weight:800;color:#334155;border-bottom:1px solid #e2e8f0;white-space:nowrap}',
      '.db-art-table td{padding:5px 8px;border-top:1px solid #f1f5f9;color:#1e293b;white-space:nowrap}',
      '.db-art-notes{margin-top:8px;font-size:11.5px;color:#334155;line-height:1.4}',
      '.db-art-watch{margin-top:6px;padding:7px 9px;border-radius:8px;background:#fffbeb;border:1px solid #fde68a;color:#92400e;font-size:11.5px;font-weight:600}',
      '.db-art-status{font-size:11px;color:#b45309;margin:4px 0}',
      '.db-zone-c.db-art-expanded{position:fixed;inset:14px;z-index:9990;overflow:auto;box-shadow:0 24px 60px rgba(15,23,42,.35)}',
      '.db-zone-c.db-art-expanded .db-echart-box{height:380px}'
    ].join('\n');

    var styleEl = document.createElement('style');
    styleEl.id = STYLE_ID;
    styleEl.textContent = css;
    var target = document.head || (document.getElementsByTagName && document.getElementsByTagName('head')[0]) || document.body || document.documentElement;
    if (target && typeof target.appendChild === 'function') {
      target.appendChild(styleEl);
    }
  }

  function updateQueryViewParam(enabled) {
    try {
      if (!g.history || typeof g.history.replaceState !== 'function' || !g.location) return;
      var url = new URL(String(g.location.href));
      if (enabled) {
        url.searchParams.set('view', 'dashboard');
      } else {
        url.searchParams.delete('view');
      }
      g.history.replaceState(null, '', url.pathname + url.search + url.hash);
    } catch (_e) { /* ignore in file:// or mock env */ }
  }

  function ensureCommandCenterButton() {
    var bar = document.getElementById('chapterWorkspaceBar');
    if (!bar) return null;
    var existing = document.getElementById(BTN_ID);
    if (!existing) {
      existing = document.createElement('button');
      existing.type = 'button';
      existing.id = BTN_ID;
      existing.className = 'chap-btn db-cmd-toggle';
      existing.setAttribute('data-view', 'dashboard');
      var container = (bar.querySelector && (bar.querySelector('.chap-pills') || bar.querySelector('.chap-ws'))) || bar;
      if (typeof container.insertBefore === 'function' && container.firstChild) {
        container.insertBefore(existing, container.firstChild);
      } else if (typeof container.appendChild === 'function') {
        container.appendChild(existing);
      }
      existing.addEventListener('click', function (ev) {
        if (ev && ev.stopPropagation) ev.stopPropagation();
        if (state.active) {
          exitCommandCenter({ restoreSnapshot: true });
        } else {
          enterCommandCenter();
        }
      });
    }
    existing.textContent = '⚡ ' + t('toggleLabel');
    existing.setAttribute('aria-pressed', state.active ? 'true' : 'false');
    if (existing.classList) {
      if (state.active) existing.classList.add('is-active');
      else existing.classList.remove('is-active');
    }
    return existing;
  }

  function ensureShellMounted() {
    var shell = document.getElementById(SHELL_ID);
    if (shell) return shell;
    var mainEl = (document.querySelector && document.querySelector('main')) || document.getElementById('main') || document.body;
    if (!mainEl) return null;
    shell = document.createElement('div');
    shell.id = SHELL_ID;
    shell.className = 'db-cmd-shell';
    shell.hidden = true;
    shell.setAttribute('hidden', '');
    if (typeof mainEl.insertBefore === 'function' && mainEl.firstChild) {
      mainEl.insertBefore(shell, mainEl.firstChild);
    } else if (typeof mainEl.appendChild === 'function') {
      mainEl.appendChild(shell);
    }
    return shell;
  }

  function renderTopBarHtml(d) {
    var specs = [
      d.beds != null ? (d.beds + ' bd') : null,
      d.baths != null ? (d.baths + ' ba') : null,
      d.sqft != null && d.sqft > 0 ? (fmtInt(d.sqft) + ' sqft') : null,
      d.yearBuilt ? ('Built ' + d.yearBuilt) : null
    ].filter(Boolean).join(' · ');

    var pillsHtml = '';
    if (d.estVal != null && d.estVal > 0) {
      pillsHtml += '<span class="db-cmd-pill">' + esc(fmtUsd(d.estVal)) + '</span>';
    }
    if (d.overallScore != null) {
      pillsHtml += '<span class="db-cmd-pill">' + Math.round(d.overallScore) + '/100</span>';
    }

    return (
      '<div class="db-cmd-topbar">' +
        '<div class="db-cmd-top-left">' +
          '<div class="db-cmd-addr">' + esc(d.address) + '</div>' +
          '<div class="db-cmd-sub">' + esc(t('hdrSubtitle')) + (specs ? (' · ' + esc(specs)) : '') + (d.cityStateZip ? (' · ' + esc(d.cityStateZip)) : '') + '</div>' +
        '</div>' +
        '<div class="db-cmd-top-pills">' +
          pillsHtml +
          '<button type="button" class="db-cmd-back-btn" id="cmdBackToReportBtn">' + esc(t('backToReport')) + '</button>' +
        '</div>' +
      '</div>'
    );
  }

  function renderZoneAHtml(d) {
    if (d.estVal == null || d.estVal <= 0) {
      return (
        '<section class="db-panel db-zone-a" id="cmdSimPanel" aria-label="' + esc(t('zoneATitle')) + '">' +
          '<div class="db-panel-hd">' +
            '<h2 class="db-panel-title">🎛 ' + esc(t('zoneATitle')) + '</h2>' +
            '<span class="db-badge">' + esc(t('zoneABadge')) + '</span>' +
          '</div>' +
          '<div class="db-sim-hero">' +
            '<div class="db-sim-hero-lbl">' + esc(t('netMonthly')) + '</div>' +
            '<div class="db-sim-hero-val" id="simNetMonthly">—</div>' +
            '<div class="db-sim-hero-sub" id="simGrossSummary">Valuation data unavailable — simulator requires a verified estimate or sale price.</div>' +
          '</div>' +
        '</section>'
      );
    }

    var s = state.sim;
    var offerBase = s.offer != null ? s.offer : Math.round(d.estVal / 5000) * 5000;
    var minOffer = Math.max(50000, Math.round((d.estVal * 0.7) / 10000) * 10000);
    var maxOffer = Math.round((d.estVal * 1.35) / 10000) * 10000;
    var offerStep = maxOffer > 1500000 ? 10000 : 5000;
    var maxRent = Math.max(5000, Math.round(Math.max((d.rentEst || 0) * 1.1, (d.aduRent || 0) * 1.5, 4500) / 100) * 100);

    var rateCtrlHtml = s.ratePct != null ? (
      '<div class="db-sim-ctrl">' +
        '<div class="db-sim-ctrl-row"><span>' + esc(t('interestRate')) + '</span><span class="db-sim-ctrl-val" id="simRateVal"></span></div>' +
        '<input class="db-sim-range" id="simRateSlider" type="range" min="3.5" max="9.5" step="0.05" value="' + s.ratePct + '">' +
      '</div>'
    ) : '';

    var quakeCtrlHtml = d.quakeMonthlyBase != null ? (
      '<div class="db-sim-toggle-row">' +
        '<label class="db-sim-toggle-lbl" for="simEqToggle"><input type="checkbox" id="simEqToggle"' + (s.quakeOn ? ' checked' : '') + '> <span>' + esc(t('quakeToggle')) + '</span></label>' +
        '<select class="db-sim-select" id="simDedSelect" aria-label="' + esc(t('deductible')) + '">' +
          '<option value="10"' + (s.deductiblePct === 10 ? ' selected' : '') + '>10% Ded.</option>' +
          '<option value="15"' + (s.deductiblePct === 15 ? ' selected' : '') + '>15% Ded.</option>' +
          '<option value="20"' + (s.deductiblePct === 20 ? ' selected' : '') + '>20% Ded.</option>' +
        '</select>' +
      '</div>'
    ) : '';

    return (
      '<section class="db-panel db-zone-a" id="cmdSimPanel" aria-label="' + esc(t('zoneATitle')) + '">' +
        '<div class="db-panel-hd">' +
          '<h2 class="db-panel-title">🎛 ' + esc(t('zoneATitle')) + '</h2>' +
          '<span class="db-badge">' + esc(t('zoneABadge')) + '</span>' +
        '</div>' +
        '<div class="db-sim-hero">' +
          '<div class="db-sim-hero-lbl">' + esc(t('netMonthly')) + '</div>' +
          '<div class="db-sim-hero-val" id="simNetMonthly">—</div>' +
          '<div class="db-sim-hero-sub" id="simGrossSummary"></div>' +
          '<div class="db-sim-stack" id="simStackedBar"></div>' +
          '<div class="db-sim-leg" id="simStackLegend"></div>' +
        '</div>' +
        '<div class="db-sim-ctrl">' +
          '<div class="db-sim-ctrl-row"><span>' + esc(t('offerPrice')) + '</span><span class="db-sim-ctrl-val" id="simOfferVal"></span></div>' +
          '<input class="db-sim-range" id="simOfferSlider" type="range" min="' + minOffer + '" max="' + maxOffer + '" step="' + offerStep + '" value="' + offerBase + '">' +
        '</div>' +
        '<div class="db-sim-ctrl">' +
          '<div class="db-sim-ctrl-row"><span>' + esc(t('downPayment')) + '</span><span class="db-sim-ctrl-val" id="simDownVal"></span></div>' +
          '<input class="db-sim-range" id="simDownSlider" type="range" min="0" max="100" step="5" value="' + s.downPct + '">' +
        '</div>' +
        rateCtrlHtml +
        quakeCtrlHtml +
        '<div class="db-sim-ctrl">' +
          '<div class="db-sim-ctrl-row"><span>' + esc(t('aduRentOffset')) + '</span><span class="db-sim-ctrl-val" id="simRentOffsetVal"></span></div>' +
          '<input class="db-sim-range" id="simRentOffsetSlider" type="range" min="0" max="' + maxRent + '" step="50" value="' + s.rentOffset + '">' +
        '</div>' +
        '<div class="db-sim-metrics">' +
          '<div class="db-sim-mbox"><div class="db-sim-mlbl">' + esc(t('cashToClose')) + '</div><div class="db-sim-mval" id="simCashToClose">—</div></div>' +
          '<div class="db-sim-mbox"><div class="db-sim-mlbl">' + esc(t('fiveYrEquity')) + '</div><div class="db-sim-mval" id="simFiveYrEquity">—</div></div>' +
          '<div class="db-sim-mbox"><div class="db-sim-mlbl">' + esc(t('capRate')) + '</div><div class="db-sim-mval" id="simCapRate">—</div></div>' +
        '</div>' +
        '<div class="db-sim-actions">' +
          '<button type="button" class="db-btn-sec" id="simResetBtn">' + esc(t('resetSim')) + '</button>' +
          '<button type="button" class="db-btn-pri" id="simSendToStudioBtn">' + esc(t('chartDealBtn')) + '</button>' +
        '</div>' +
      '</section>'
    );
  }

  function renderZoneBHtml(d) {
    var cards = [];
    var tpLbl = currentLang() === 'zh' ? '第三方估值' : currentLang() === 'es' ? 'Estimación de terceros' : 'Third-party estimate';

    if (d.estVal != null && d.estVal > 0) {
      var zDeltaPct = d.zestimate != null && d.zestimate > 0 ? (((d.estVal - d.zestimate) / d.zestimate) * 100).toFixed(1) : null;
      var valParts = [];
      if (d.estLow != null && d.estHigh != null) {
        valParts.push('Range ' + fmtCompactUsd(d.estLow) + '–' + fmtCompactUsd(d.estHigh));
      }
      if (d.zestimate != null && d.zestimate > 0 && zDeltaPct != null) {
        valParts.push(tpLbl + ' ' + fmtCompactUsd(d.zestimate) + ' (' + (Number(zDeltaPct) >= 0 ? '+' : '') + zDeltaPct + '%)');
      }
      cards.push({
        title: t('cardValTitle'),
        tone: 'info',
        pill: d.perSqft != null ? ('$' + Math.round(d.perSqft) + '/sf') : '',
        mainHtml: esc(fmtCompactUsd(d.estVal)),
        desc: esc(valParts.join(' · ')),
        jump: 'value'
      });
    }

    if (d.lotSqft != null || d.bcrPct != null || d.aduFeasibility || d.aduRent != null) {
      var lotMainHtml = d.lotSqft != null
        ? ('<span data-lot-hover="1" tabindex="0" class="lot-trig">' + esc(fmtInt(d.lotSqft)) + ' sqft</span>')
        : 'Lot Record';
      var lotParts = [];
      if (d.bcrPct != null) lotParts.push('Coverage ' + d.bcrPct.toFixed(1) + '%');
      if (d.aduFeasibility) lotParts.push(d.aduFeasibility);
      if (d.aduRent != null) lotParts.push('Est ADU ' + fmtUsd(d.aduRent) + '/mo');
      cards.push({
        title: t('cardLotTitle'),
        tone: d.lotSqft != null && d.lotSqft >= 6000 ? 'good' : 'info',
        pill: d.bcrPct != null ? (d.bcrPct.toFixed(0) + '% BCR') : 'Lot/GIS',
        mainHtml: lotMainHtml,
        desc: esc(lotParts.join(' · ')),
        jump: 'house'
      });
    }

    if (d.safetyScore != null || d.floodZone || d.sdc || d.quakeScore != null || d.floodScore != null || d.fireScore != null) {
      var riskTone = d.safetyScore != null ? (d.safetyScore >= 72 ? 'good' : (d.safetyScore >= 58 ? 'info' : 'warn')) : 'info';
      var riskHdr = [
        d.floodZone ? ('Flood ' + d.floodZone) : null,
        d.sdc ? ('SDC ' + d.sdc) : null
      ].filter(Boolean).join(' · ') || (d.safetyScore != null ? (Math.round(d.safetyScore) + '/100') : 'Hazards');
      var riskSub = [
        d.quakeScore != null ? ('Quake ' + Math.round(d.quakeScore) + '/100') : null,
        d.floodScore != null ? ('Flood ' + Math.round(d.floodScore) + '/100') : null,
        d.fireScore != null ? ('Fire ' + Math.round(d.fireScore) + '/100') : null
      ].filter(Boolean).join(' · ');
      cards.push({
        title: t('cardRiskTitle'),
        tone: riskTone,
        pill: d.safetyScore != null ? ('Safety ' + Math.round(d.safetyScore) + '/100') : '',
        mainHtml: esc(riskHdr),
        desc: esc(riskSub),
        jump: 'risk'
      });
    }

    if (d.envScore != null || d.noiseLabel || d.envCleanScore != null || d.airScore != null || d.waterScore != null) {
      var envSub = [
        d.envCleanScore != null ? ('Clean ' + Math.round(d.envCleanScore) + '/100') : null,
        d.airScore != null ? ('Air ' + Math.round(d.airScore) + '/100') : null,
        d.waterScore != null ? ('Water ' + Math.round(d.waterScore) + '/100') : null
      ].filter(Boolean).join(' · ');
      cards.push({
        title: t('cardEnvTitle'),
        tone: d.envScore != null && d.envScore >= 72 ? 'good' : 'info',
        pill: d.envScore != null ? ('Env ' + Math.round(d.envScore) + '/100') : '',
        mainHtml: esc(d.noiseLabel ? ('Noise: ' + d.noiseLabel) : (d.envScore != null ? (Math.round(d.envScore) + '/100') : 'Environment')),
        desc: esc(envSub),
        jump: 'environment'
      });
    }

    if (d.schoolsCount > 0 || d.walkScore != null || d.livScore != null) {
      var schSub = [
        d.topSchool ? ('Top: ' + d.topSchool) : null,
        d.livScore != null ? ('Livability ' + Math.round(d.livScore) + '/100') : null
      ].filter(Boolean).join(' · ');
      var schPill = [
        d.walkScore != null ? ('Walk ' + Math.round(d.walkScore)) : null,
        d.schoolGrade ? ('Grade ' + d.schoolGrade) : null
      ].filter(Boolean).join(' · ');
      cards.push({
        title: t('cardSchoolTitle'),
        tone: d.livScore != null && d.livScore >= 72 ? 'good' : 'info',
        pill: schPill,
        mainHtml: esc(d.schoolsCount > 0 ? (d.schoolsCount + ' Assigned Schools') : (d.walkScore != null ? ('Walk ' + Math.round(d.walkScore)) : 'Livability')),
        desc: esc(schSub),
        jump: 'schools'
      });
    }

    if (d.taxRatePct != null || d.apn || d.lastSalePrice != null) {
      var taxEstYr = d.estVal != null && d.taxRatePct != null ? Math.round(d.estVal * (d.taxRatePct / 100)) : null;
      var taxSub = [
        d.apn ? ('APN ' + d.apn) : null,
        d.lastSalePrice != null ? ('Last ' + fmtCompactUsd(d.lastSalePrice) + (d.lastSaleDate ? (' (' + d.lastSaleDate + ')') : '')) : null
      ].filter(Boolean).join(' · ');
      cards.push({
        title: t('cardTaxTitle'),
        tone: 'info',
        pill: d.taxRatePct != null ? (d.taxRatePct.toFixed(2) + '% Tax') : '',
        mainHtml: esc(taxEstYr != null ? (fmtUsd(taxEstYr) + '/yr') : (d.apn ? ('APN ' + d.apn) : 'County Record')),
        desc: esc(taxSub),
        jump: 'county'
      });
    }

    var html = cards.map(function (c) {
      return (
        '<article class="db-bcard">' +
          '<div>' +
            '<div class="db-bcard-top"><span class="db-bcard-title">' + esc(c.title) + '</span>' + (c.pill ? ('<span class="db-tone-pill db-tone-' + c.tone + '">' + esc(c.pill) + '</span>') : '') + '</div>' +
            '<div class="db-bcard-main">' + c.mainHtml + '</div>' +
            '<div class="db-bcard-desc">' + c.desc + '</div>' +
          '</div>' +
          '<button type="button" class="db-bcard-jump" data-cmd-jump="' + esc(c.jump) + '">' + esc(t('deepDive')) + '</button>' +
        '</article>'
      );
    }).join('');

    return (
      '<section class="db-panel db-zone-b" id="cmdBentoCards" aria-label="' + esc(t('zoneBTitle')) + '">' +
        '<div class="db-panel-hd">' +
          '<h2 class="db-panel-title">🧩 ' + esc(t('zoneBTitle')) + '</h2>' +
          (cards.length ? ('<span class="db-badge">' + cards.length + ' Pillars</span>') : '') +
        '</div>' +
        '<div class="db-bento-grid">' + html + '</div>' +
      '</section>'
    );
  }

  function renderZoneCHtml(d) {
    var yrStr = d.yearBuilt ? String(d.yearBuilt) : 'Home';
    var presets = t('presets') || I18N.en.presets;
    var chipsHtml = presets.map(function (pr) {
      var lbl = pr.label.replace('{YEAR}', yrStr);
      var prm = pr.prompt.replace('{YEAR}', yrStr);
      return '<button type="button" class="db-chip-btn" data-art-prompt="' + esc(prm) + '">' + esc(lbl) + '</button>';
    }).join('');

    return (
      '<section class="db-panel db-zone-c' + (state.fullscreen ? ' db-art-expanded' : '') + '" id="artifactStudioPane" aria-label="' + esc(t('zoneCTitle')) + '">' +
        '<div class="db-art-head">' +
          '<div>' +
            '<h2 class="db-panel-title">✨ ' + esc(t('zoneCTitle')) + '</h2>' +
            '<div class="db-art-sub" id="artifactStudioSub">' + esc(t('studioSub')) + '</div>' +
          '</div>' +
          '<div class="db-art-tools">' +
            '<span class="db-badge" id="artifactModeBadge">' + esc(t('modeInstant')) + '</span>' +
            '<button type="button" class="db-btn-sec" id="artifactCopyCsvBtn">' + esc(t('copyCsv')) + '</button>' +
            '<button type="button" class="db-btn-sec" id="artifactFullscreenBtn">' + esc(state.fullscreen ? t('collapse') : t('expand')) + '</button>' +
          '</div>' +
        '</div>' +
        '<div class="db-art-presets" id="artifactPresetChips">' + chipsHtml + '</div>' +
        '<form class="db-prompt-row" id="artifactPromptForm">' +
          '<input class="db-prompt-inp" id="artifactPromptInput" type="text" maxlength="500" placeholder="' + esc(t('promptPlaceholder')) + '">' +
          '<button class="db-prompt-go" id="artifactGenerateBtn" type="submit">' + esc(t('visualizeBtn')) + '</button>' +
        '</form>' +
        '<div class="db-art-status" id="artifactStatusMsg" hidden></div>' +
        '<div class="db-art-tabs" id="artifactHistoryTabs"></div>' +
        '<h3 class="db-art-title" id="artifactTitle"></h3>' +
        '<p class="db-art-sub" id="artifactSubtitle"></p>' +
        '<div class="db-art-kpis" id="artifactKpiStrip"></div>' +
        '<div class="db-echart-box" id="artifactEchartBox"></div>' +
        '<div class="db-art-table-wrap" id="artifactTableWrap"></div>' +
        '<div class="db-art-notes" id="artifactInsights"></div>' +
        '<div class="db-art-watch" id="artifactWatchout"></div>' +
      '</section>'
    );
  }

  function updateSimulatorOutputs() {
    if (!state.sim) resetSimState();
    var d = getBaselineModel();
    if (d.estVal == null || d.estVal <= 0) return;
    var s = state.sim;

    var offer = optNum(s.offer) != null ? optNum(s.offer) : d.estVal;
    var downPct = Math.max(0, Math.min(100, num(s.downPct, 20)));
    var ratePct = optNum(s.ratePct) != null ? optNum(s.ratePct) : d.rate30;
    var loan = offer * (1 - downPct / 100);
    var pi = (downPct < 99.9 && ratePct != null) ? Math.round(calcMonthlyPI(loan, ratePct, 30)) : (downPct >= 99.9 ? 0 : null);
    var insRatio = offer / d.estVal;
    var propTax = d.taxRatePct != null
      ? Math.round((offer * (d.taxRatePct / 100)) / 12)
      : (d.propTaxMonthly != null ? Math.round(d.propTaxMonthly * insRatio) : 0);
    var homeIns = d.insMonthly != null ? Math.round(d.insMonthly * insRatio) : 0;
    var dedMult = s.deductiblePct === 10 ? 1.25 : (s.deductiblePct === 20 ? 0.78 : 1.0);
    var quakeIns = (s.quakeOn && d.quakeMonthlyBase != null) ? Math.round(d.quakeMonthlyBase * insRatio * dedMult) : 0;
    var maint = d.maintMonthly != null ? Math.round(d.maintMonthly * insRatio) : 0;
    var hoa = d.hoaMonthly != null ? Math.round(d.hoaMonthly) : 0;
    var rentOff = Math.max(0, num(s.rentOffset, 0));

    var gross = pi != null ? (pi + propTax + homeIns + quakeIns + maint + hoa) : null;
    var net = gross != null ? (gross - rentOff) : null;

    var cashToClose = Math.round(offer * (downPct / 100));
    var equity5 = null;
    if (d.p50Growth5y != null && (downPct >= 99.9 || ratePct != null)) {
      var bal5 = downPct >= 99.9 ? 0 : calcRemainingBalance(loan, ratePct, 30, 60);
      equity5 = Math.round(Math.max(0, offer * d.p50Growth5y - bal5));
    }

    var netCapPct = null;
    if (d.rentEst != null && offer > 0) {
      var potentialAnnualRent = (d.rentEst + rentOff) * 12;
      var annualOpEx = (propTax + homeIns + quakeIns + maint + hoa) * 12;
      netCapPct = ((potentialAnnualRent - annualOpEx) / offer) * 100;
    }

    function setTxt(id, txt) {
      var el = document.getElementById(id);
      if (el) el.textContent = txt;
    }

    setTxt('simOfferVal', fmtUsd(offer));
    setTxt('simDownVal', downPct >= 99.5 ? ('100% (' + t('allCashTag') + ')') : (downPct + '% (' + fmtCompactUsd(offer * (downPct / 100)) + ')'));
    if (ratePct != null) setTxt('simRateVal', ratePct.toFixed(2) + '%');
    setTxt('simRentOffsetVal', '-' + fmtUsd(rentOff) + '/mo');
    setTxt('simNetMonthly', net != null ? (fmtUsd(net) + '/mo') : '—');
    setTxt('simGrossSummary', gross != null
      ? (t('grossLabel') + ': ' + fmtUsd(gross) + '/mo · ' + t('piSeg') + ' ' + fmtUsd(pi) + ' · ' + t('taxSeg') + ' ' + fmtUsd(propTax))
      : 'Mortgage rate unavailable in report');
    setTxt('simCashToClose', fmtCompactUsd(cashToClose));
    setTxt('simFiveYrEquity', equity5 != null ? fmtCompactUsd(equity5) : '—');
    setTxt('simCapRate', netCapPct != null ? (netCapPct.toFixed(1) + '%') : '—');

    var stackEl = document.getElementById('simStackedBar');
    var legEl = document.getElementById('simStackLegend');
    if (stackEl && gross != null && gross > 0) {
      var segs = [
        { k: t('piSeg'), v: pi, c: '#0f766e' },
        { k: t('taxSeg'), v: propTax, c: '#0284c7' },
        { k: t('insSeg'), v: homeIns + quakeIns, c: '#f59e0b' },
        { k: t('maintSeg'), v: maint + hoa, c: '#64748b' }
      ].filter(function (sg) { return sg.v > 0; });
      stackEl.innerHTML = segs.map(function (sg) {
        var pct = Math.max(0, (sg.v / gross) * 100).toFixed(1);
        return '<span class="db-sim-seg" style="width:' + pct + '%;background:' + sg.c + '"></span>';
      }).join('');
      if (legEl) {
        legEl.innerHTML = segs.map(function (sg) {
          return '<span><i style="background:' + sg.c + '"></i>' + esc(sg.k) + ' ' + esc(fmtUsd(sg.v)) + '</span>';
        }).join('');
      }
    }
  }

  function formatChartAxisVal(v, unit) {
    if (!isFinite(v)) return '0';
    var u = String(unit || '');
    if (u.indexOf('$') >= 0) return fmtCompactUsd(v);
    if (u.indexOf('%') >= 0) return Number(v).toFixed(1) + '%';
    return String(Math.round(v * 10) / 10);
  }

  function buildEChartsOption(art) {
    var cats = Array.isArray(art.categories) ? art.categories : [];
    var series = Array.isArray(art.series) ? art.series : [];
    var cType = art.chartType || 'bar';
    var unit = art.unit || '';
    var palette = ['#0f766e', '#0284c7', '#f59e0b', '#dc2626', '#7c3aed'];

    if (cType === 'donut') {
      var s0 = series[0] && Array.isArray(series[0].values) ? series[0].values : [];
      var pieData = cats.map(function (name, idx) {
        return { name: name, value: num(s0[idx], 0) };
      });
      return {
        color: palette,
        tooltip: { trigger: 'item', formatter: '{b}: {c} ({d}%)' },
        legend: { bottom: 0, type: 'scroll', textStyle: { fontSize: 11 } },
        series: [{
          name: (series[0] && series[0].name) || unit,
          type: 'pie',
          radius: ['42%', '68%'],
          center: ['50%', '44%'],
          label: { show: true, fontSize: 11, formatter: '{b}\n{d}%' },
          data: pieData
        }]
      };
    }

    if (cType === 'radar') {
      var indicator = cats.map(function (name) { return { name: name, max: 100 }; });
      var radarData = series.map(function (s) {
        return { name: s.name, value: (s.values || []).map(function (x) { return num(x, 0); }) };
      });
      return {
        color: palette,
        tooltip: { trigger: 'item' },
        legend: { bottom: 0, textStyle: { fontSize: 11 } },
        radar: { indicator: indicator, radius: '60%', center: ['50%', '45%'] },
        series: [{ type: 'radar', data: radarData, areaStyle: { opacity: 0.16 } }]
      };
    }

    if (cType === 'horizontal_bar') {
      return {
        color: palette,
        tooltip: { trigger: 'axis', axisPointer: { type: 'shadow' } },
        legend: { top: 2, type: 'scroll', textStyle: { fontSize: 11 } },
        grid: { left: 105, right: 20, top: 32, bottom: 26 },
        xAxis: {
          type: 'value',
          axisLabel: { fontSize: 10, formatter: function (v) { return formatChartAxisVal(v, unit); } }
        },
        yAxis: {
          type: 'category',
          data: cats,
          inverse: true,
          axisLabel: { fontSize: 10.5, width: 96, overflow: 'truncate' }
        },
        series: series.map(function (s) {
          return {
            name: s.name,
            type: 'bar',
            barMaxWidth: 18,
            data: (s.values || []).map(function (x) { return num(x, 0); })
          };
        })
      };
    }

    var isLine = cType === 'line';
    return {
      color: palette,
      tooltip: { trigger: 'axis' },
      legend: { top: 2, type: 'scroll', textStyle: { fontSize: 11 } },
      grid: { left: 56, right: 16, top: 34, bottom: 32 },
      xAxis: {
        type: 'category',
        data: cats,
        axisLabel: { fontSize: 10.5, interval: 0, width: 72, overflow: 'truncate' }
      },
      yAxis: {
        type: 'value',
        axisLabel: { fontSize: 10, formatter: function (v) { return formatChartAxisVal(v, unit); } }
      },
      series: series.map(function (s) {
        return {
          name: s.name,
          type: isLine ? 'line' : 'bar',
          smooth: isLine,
          barMaxWidth: 26,
          data: (s.values || []).map(function (x) { return num(x, 0); })
        };
      })
    };
  }

  function renderActiveArtifact() {
    if (!state.artifacts.length) return;
    var idx = Math.max(0, Math.min(state.artifacts.length - 1, state.activeArtifactIdx));
    state.activeArtifactIdx = idx;
    var art = state.artifacts[idx];
    if (!art) return;

    var tabsEl = document.getElementById('artifactHistoryTabs');
    if (tabsEl) {
      tabsEl.innerHTML = state.artifacts.map(function (a, i) {
        var cls = 'db-tab-pill' + (i === idx ? ' is-active' : '');
        var shortTitle = String(a.title || (t('artifactPrefix') + ' ' + (i + 1))).slice(0, 22);
        return '<button type="button" class="' + cls + '" data-art-tab="' + i + '">' + esc(t('artifactPrefix') + ' ' + (i + 1) + ': ' + shortTitle) + '</button>';
      }).join('');
    }

    var badgeEl = document.getElementById('artifactModeBadge');
    if (badgeEl) {
      badgeEl.textContent = art.synthesisMode === 'ai_enhanced' ? t('modeAi') : t('modeInstant');
    }

    var titleEl = document.getElementById('artifactTitle');
    if (titleEl) titleEl.textContent = art.title || '';
    var subEl = document.getElementById('artifactSubtitle');
    if (subEl) subEl.textContent = art.subtitle || '';

    var kpiEl = document.getElementById('artifactKpiStrip');
    if (kpiEl) {
      var kpis = Array.isArray(art.kpis) ? art.kpis : [];
      kpiEl.innerHTML = kpis.map(function (k) {
        return (
          '<div class="db-art-kpi">' +
            '<div class="db-art-kpi-l">' + esc(k.label) + '</div>' +
            '<div class="db-art-kpi-v">' + esc(k.value) + '</div>' +
            '<div class="db-art-kpi-d">' + esc(k.delta) + '</div>' +
          '</div>'
        );
      }).join('');
    }

    var chartBox = document.getElementById('artifactEchartBox');
    if (chartBox) {
      if (!Array.isArray(art.series) || !art.series.length) {
        if (state.chartInstance && typeof state.chartInstance.clear === 'function') {
          try { state.chartInstance.clear(); } catch (_e0) {}
        }
        var emptyNote = (Array.isArray(art.insights) && art.insights[0]) || 'Chart data unavailable for this property.';
        chartBox.innerHTML = '<div style="padding:18px;font-size:12.5px;color:#475569">' + esc(emptyNote) + '</div>';
      } else if (typeof g.echarts !== 'undefined' && g.echarts && typeof g.echarts.init === 'function') {
        try {
          var inst = (g.echarts.getInstanceByDom && g.echarts.getInstanceByDom(chartBox)) || g.echarts.init(chartBox);
          state.chartInstance = inst;
          inst.setOption(buildEChartsOption(art), true);
          if (typeof inst.resize === 'function') {
            setTimeout(function () { try { inst.resize(); } catch (_e) {} }, 20);
          }
        } catch (_err) {
          chartBox.innerHTML = '<div style="padding:14px;font-size:12px;color:#475569">' + esc(art.title || '') + '</div>';
        }
      } else {
        // Lightweight non-ECharts fallback (for DOM environments without canvas/echarts)
        var s0 = (art.series && art.series[0]) || { values: [] };
        chartBox.innerHTML = '<div style="padding:10px;font-size:12px">' + (art.categories || []).map(function (cat, i) {
          return '<div><strong>' + esc(cat) + '</strong>: ' + esc(formatChartAxisVal(num(s0.values[i], 0), art.unit)) + '</div>';
        }).join('') + '</div>';
      }
    }

    var tblWrap = document.getElementById('artifactTableWrap');
    if (tblWrap) {
      var headers = Array.isArray(art.tableHeaders) ? art.tableHeaders : [];
      var rows = Array.isArray(art.tableRows) ? art.tableRows : [];
      if (headers.length && rows.length) {
        var th = '<thead><tr>' + headers.map(function (h) { return '<th>' + esc(h) + '</th>'; }).join('') + '</tr></thead>';
        var tb = '<tbody>' + rows.map(function (r) {
          return '<tr>' + r.map(function (c) { return '<td>' + esc(c) + '</td>'; }).join('') + '</tr>';
        }).join('') + '</tbody>';
        tblWrap.innerHTML = '<table class="db-art-table">' + th + tb + '</table>';
      } else {
        tblWrap.innerHTML = '';
      }
    }

    var notesEl = document.getElementById('artifactInsights');
    if (notesEl) {
      var ins = Array.isArray(art.insights) ? art.insights : [];
      notesEl.innerHTML = ins.length
        ? ('<strong>' + esc(t('insightsLabel')) + ':</strong> ' + ins.map(function (x) { return '• ' + esc(x); }).join(' '))
        : '';
    }

    var watchEl = document.getElementById('artifactWatchout');
    if (watchEl) {
      if (art.watchout) {
        watchEl.hidden = false;
        watchEl.textContent = t('watchoutLabel') + ': ' + art.watchout;
      } else {
        watchEl.hidden = true;
      }
    }
  }

  function pushArtifact(art) {
    if (!art) return;
    state.artifacts.push(art);
    if (state.artifacts.length > MAX_HISTORY) {
      state.artifacts.shift();
    }
    state.activeArtifactIdx = state.artifacts.length - 1;
    renderActiveArtifact();
  }

  function setStatusMsg(msg) {
    var el = document.getElementById('artifactStatusMsg');
    if (!el) return;
    if (!msg) {
      el.hidden = true;
      el.textContent = '';
    } else {
      el.hidden = false;
      el.textContent = msg;
    }
  }

  function requestArtifact(promptText) {
    var clean = String(promptText || '').trim();
    if (!clean || state.busy) return;
    var inp = document.getElementById('artifactPromptInput');
    if (inp && inp.value !== clean) inp.value = clean;

    var d = getBaselineModel();
    var slug = d.slug;
    var goBtn = document.getElementById('artifactGenerateBtn');

    if (!slug || typeof g.fetch !== 'function' || (g.location && g.location.protocol === 'file:')) {
      setStatusMsg('');
      pushArtifact(buildLocalArtifact(clean));
      return;
    }

    state.busy = true;
    setStatusMsg('');
    if (goBtn) {
      goBtn.disabled = true;
      goBtn.textContent = t('generating');
    }

    var shareTok = '';
    try {
      var sp = new URLSearchParams(g.location && g.location.search ? g.location.search : '');
      shareTok = sp.get('share_token') || sp.get('shareToken') || '';
    } catch (_e) { /* ignore */ }
    var endpoint = '/api/report/' + encodeURIComponent(slug) + '/artifact' + (shareTok ? ('?share_token=' + encodeURIComponent(shareTok)) : '');

    g.fetch(endpoint, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ prompt: clean, lang: currentLang() })
    }).then(function (resp) {
      return resp.json().then(function (data) {
        return { ok: resp.ok, status: resp.status, data: data };
      });
    }).then(function (res) {
      if (res && res.ok && res.data && res.data.ok && res.data.artifact) {
        pushArtifact(res.data.artifact);
      } else if (res && res.status === 400 && res.data && res.data.error) {
        setStatusMsg(res.data.error);
      } else {
        setStatusMsg(t('errRequest'));
        pushArtifact(buildLocalArtifact(clean));
      }
    }).catch(function () {
      setStatusMsg(t('errRequest'));
      pushArtifact(buildLocalArtifact(clean));
    }).finally(function () {
      state.busy = false;
      if (goBtn) {
        goBtn.disabled = false;
        goBtn.textContent = t('visualizeBtn');
      }
    });
  }

  function exportActiveCsv() {
    var art = state.artifacts[state.activeArtifactIdx];
    if (!art) return;
    var headers = Array.isArray(art.tableHeaders) ? art.tableHeaders : [];
    var rows = Array.isArray(art.tableRows) ? art.tableRows : [];
    var lines = [];
    function csvRow(arr) {
      return arr.map(function (cell) {
        return '"' + String(cell == null ? '' : cell).replace(/"/g, '""') + '"';
      }).join(',');
    }
    if (headers.length) lines.push(csvRow(headers));
    for (var i = 0; i < rows.length; i++) lines.push(csvRow(rows[i]));
    var csv = lines.join('\n');

    var btn = document.getElementById('artifactCopyCsvBtn');
    function markCopied() {
      if (!btn) return;
      btn.textContent = t('copiedCsv');
      setTimeout(function () { if (btn) btn.textContent = t('copyCsv'); }, 1500);
    }

    if (typeof navigator !== 'undefined' && navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
      navigator.clipboard.writeText(csv).then(markCopied).catch(markCopied);
    } else {
      markCopied();
    }
  }

  function jumpToDeepDive(targetId) {
    exitCommandCenter({ restoreSnapshot: false });
    if (g.OpenEstateWorkspace && typeof g.OpenEstateWorkspace.jumpToSection === 'function') {
      g.OpenEstateWorkspace.jumpToSection(targetId);
      return;
    }
    var target = document.getElementById(targetId);
    if (target && typeof target.scrollIntoView === 'function') {
      target.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }
  }

  function bindShellEvents(shell) {
    var backBtn = document.getElementById('cmdBackToReportBtn');
    if (backBtn) {
      backBtn.addEventListener('click', function () {
        exitCommandCenter({ restoreSnapshot: true });
      });
    }

    function bindSlider(id, key, isFloat) {
      var el = document.getElementById(id);
      if (!el) return;
      var handler = function () {
        state.sim[key] = isFloat ? parseFloat(el.value) : parseInt(el.value, 10);
        updateSimulatorOutputs();
      };
      el.addEventListener('input', handler);
      el.addEventListener('change', handler);
    }

    bindSlider('simOfferSlider', 'offer', false);
    bindSlider('simDownSlider', 'downPct', false);
    bindSlider('simRateSlider', 'ratePct', true);
    bindSlider('simRentOffsetSlider', 'rentOffset', false);

    var eqTog = document.getElementById('simEqToggle');
    if (eqTog) {
      eqTog.addEventListener('change', function () {
        state.sim.quakeOn = !!eqTog.checked;
        updateSimulatorOutputs();
      });
    }
    var dedSel = document.getElementById('simDedSelect');
    if (dedSel) {
      dedSel.addEventListener('change', function () {
        state.sim.deductiblePct = parseInt(dedSel.value, 10) || 15;
        updateSimulatorOutputs();
      });
    }

    var resetBtn = document.getElementById('simResetBtn');
    if (resetBtn) {
      resetBtn.addEventListener('click', function () {
        resetSimState();
        renderShellContents();
      });
    }

    var chartDealBtn = document.getElementById('simSendToStudioBtn');
    if (chartDealBtn) {
      chartDealBtn.addEventListener('click', function () {
        var s = state.sim || {};
        if (s.offer == null) return;
        var ratePart = s.ratePct != null ? (' (' + Number(s.ratePct).toFixed(2) + '%)') : '';
        var promptStr = 'Compare ' + (s.downPct != null ? s.downPct : 20) + '% down at ' + fmtUsd(s.offer) + ratePart + ' vs All-Cash with ' + fmtUsd(s.rentOffset || 0) + '/mo ADU rent offset';
        requestArtifact(promptStr);
      });
    }

    var form = document.getElementById('artifactPromptForm');
    if (form) {
      form.addEventListener('submit', function (ev) {
        if (ev && ev.preventDefault) ev.preventDefault();
        var inp = document.getElementById('artifactPromptInput');
        if (inp) requestArtifact(inp.value);
      });
    }

    var copyBtn = document.getElementById('artifactCopyCsvBtn');
    if (copyBtn) {
      copyBtn.addEventListener('click', exportActiveCsv);
    }

    var fullBtn = document.getElementById('artifactFullscreenBtn');
    if (fullBtn) {
      fullBtn.addEventListener('click', function () {
        state.fullscreen = !state.fullscreen;
        var pane = document.getElementById('artifactStudioPane');
        if (pane && pane.classList) {
          if (state.fullscreen) pane.classList.add('db-art-expanded');
          else pane.classList.remove('db-art-expanded');
        }
        fullBtn.textContent = state.fullscreen ? t('collapse') : t('expand');
        if (state.chartInstance && typeof state.chartInstance.resize === 'function') {
          setTimeout(function () { try { state.chartInstance.resize(); } catch (_e) {} }, 30);
        }
      });
    }

    shell.addEventListener('click', function (ev) {
      var target = ev.target;
      while (target && target !== shell) {
        if (target.getAttribute) {
          var jumpId = target.getAttribute('data-cmd-jump');
          if (jumpId) {
            jumpToDeepDive(jumpId);
            return;
          }
          var artPrompt = target.getAttribute('data-art-prompt');
          if (artPrompt) {
            requestArtifact(artPrompt);
            return;
          }
          var tabIdx = target.getAttribute('data-art-tab');
          if (tabIdx !== null && tabIdx !== '') {
            state.activeArtifactIdx = parseInt(tabIdx, 10) || 0;
            renderActiveArtifact();
            return;
          }
        }
        target = target.parentNode;
      }
    });
  }

  function renderShellContents() {
    var shell = ensureShellMounted();
    if (!shell) return;
    if (!state.sim) resetSimState();
    var d = getBaselineModel();
    shell.innerHTML = (
      renderTopBarHtml(d) +
      '<div class="db-cmd-grid">' +
        renderZoneAHtml(d) +
        renderZoneBHtml(d) +
        renderZoneCHtml(d) +
      '</div>'
    );
    bindShellEvents(shell);
    updateSimulatorOutputs();
    if (!state.artifacts.length) {
      var seedPrompt = (t('presets')[0] && t('presets')[0].prompt) || '20% Down vs All-Cash + ADU Rent';
      state.artifacts.push(buildLocalArtifact(seedPrompt));
      state.activeArtifactIdx = 0;
    }
    renderActiveArtifact();
  }

  function snapshotAndParkSections() {
    var sections = document.querySelectorAll ? document.querySelectorAll('main > section.sec') : [];
    var snap = [];
    for (var i = 0; i < sections.length; i++) {
      var sec = sections[i];
      var wasParked = !!(sec.classList && sec.classList.contains('ch-parked'));
      snap.push({ id: sec.id || '', wasParked: wasParked });
      if (sec.classList) sec.classList.add('ch-parked');
    }
    state.snapshotParked = snap;
  }

  function restoreSectionsSnapshot() {
    if (!state.snapshotParked) return;
    var sections = document.querySelectorAll ? document.querySelectorAll('main > section.sec') : [];
    for (var i = 0; i < sections.length; i++) {
      var sec = sections[i];
      var rec = state.snapshotParked[i];
      if (!rec || !sec.classList) continue;
      if (rec.wasParked) sec.classList.add('ch-parked');
      else sec.classList.remove('ch-parked');
    }
    state.snapshotParked = null;
  }

  function enterCommandCenter() {
    injectStyles();
    if (!state.sim) resetSimState();
    state.active = true;
    state.lastLang = currentLang();
    snapshotAndParkSections();
    if (document.body && document.body.classList) {
      document.body.classList.add('db-cmd-active');
    }
    var shell = ensureShellMounted();
    renderShellContents();
    if (shell) {
      shell.hidden = false;
      if (typeof shell.removeAttribute === 'function') shell.removeAttribute('hidden');
    }
    ensureCommandCenterButton();
    updateQueryViewParam(true);
    if (state.chartInstance && typeof state.chartInstance.resize === 'function') {
      setTimeout(function () { try { state.chartInstance.resize(); } catch (_e) {} }, 40);
    }
  }

  function exitCommandCenter(opts) {
    var options = opts || {};
    state.active = false;
    state.fullscreen = false;
    if (document.body && document.body.classList) {
      document.body.classList.remove('db-cmd-active');
    }
    var shell = document.getElementById(SHELL_ID);
    if (shell) {
      shell.hidden = true;
      if (typeof shell.setAttribute === 'function') shell.setAttribute('hidden', '');
    }
    if (options.restoreSnapshot) {
      restoreSectionsSnapshot();
    } else {
      state.snapshotParked = null;
    }
    ensureCommandCenterButton();
    updateQueryViewParam(false);
  }

  function installObserversAndHooks() {
    if (!state.observerInstalled) {
      state.observerInstalled = true;
      var bar = document.getElementById('chapterWorkspaceBar');
      if (bar && typeof g.MutationObserver === 'function') {
        try {
          var obs = new g.MutationObserver(function () {
            if (!document.getElementById(BTN_ID)) {
              ensureCommandCenterButton();
            }
          });
          obs.observe(bar, { childList: true });
        } catch (_e) { /* ignore */ }
      }

      // Capture-phase exit when user clicks any chapter tab, All Sections, Next/Prev chapter, or anchor link
      document.addEventListener('click', function (ev) {
        if (!state.active) return;
        var node = ev.target;
        while (node && node !== document) {
          if (node.id === BTN_ID) return;
          if (node.getAttribute) {
            var hasCh = node.getAttribute('data-chapter') !== null;
            var hasGo = node.getAttribute('data-go-chap') !== null;
            var isTabAnchor = node.tagName === 'A' && String(node.getAttribute('href') || '').charAt(0) === '#';
            if (hasCh || hasGo || isTabAnchor) {
              exitCommandCenter({ restoreSnapshot: false });
              return;
            }
          }
          node = node.parentNode;
        }
      }, true);

      if (typeof g.addEventListener === 'function') {
        g.addEventListener('hashchange', function () {
          if (state.active) exitCommandCenter({ restoreSnapshot: false });
        });
        g.addEventListener('beforeprint', function () {
          if (state.active) exitCommandCenter({ restoreSnapshot: true });
        });
        g.addEventListener('resize', function () {
          if (state.active && state.chartInstance && typeof state.chartInstance.resize === 'function') {
            try { state.chartInstance.resize(); } catch (_e) {}
          }
        });
        g.addEventListener('keydown', function (ev) {
          if (ev && ev.key === 'Escape' && state.fullscreen) {
            state.fullscreen = false;
            var pane = document.getElementById('artifactStudioPane');
            if (pane && pane.classList) pane.classList.remove('db-art-expanded');
            var fb = document.getElementById('artifactFullscreenBtn');
            if (fb) fb.textContent = t('expand');
          }
        });
      }
    }

    if (!state.langWrapped) {
      state.langWrapped = true;
      var origLangFn = g.refreshDynamicLanguageViews;
      g.refreshDynamicLanguageViews = function () {
        if (typeof origLangFn === 'function') {
          try { origLangFn.apply(this, arguments); } catch (_e) {}
        }
        var nl = currentLang();
        if (nl !== state.lastLang) {
          state.lastLang = nl;
          if (state.artifacts.length === 1 && state.artifacts[0] && state.artifacts[0].id && state.artifacts[0].id.indexOf('local-') === 0) {
            var p0 = (t('presets')[0] && t('presets')[0].prompt) || '20% Down vs All-Cash + ADU Rent';
            state.artifacts[0] = buildLocalArtifact(p0);
          }
        }
        ensureCommandCenterButton();
        if (state.active) renderShellContents();
      };
    }
  }

  function initCommandCenter() {
    if (state.initialized) {
      ensureCommandCenterButton();
      return;
    }
    state.initialized = true;
    injectStyles();
    resetSimState();
    ensureCommandCenterButton();
    installObserversAndHooks();

    try {
      var sp = new URLSearchParams(g.location && g.location.search ? g.location.search : '');
      var view = String(sp.get('view') || '').toLowerCase();
      if (view === 'dashboard' || view === 'command') {
        enterCommandCenter();
      }
    } catch (_e) { /* ignore */ }
  }

  g.OpenEstateCommandCenter = {
    init: initCommandCenter,
    enter: enterCommandCenter,
    exit: exitCommandCenter,
    requestArtifact: requestArtifact,
    isActive: function () { return !!state.active; },
    getState: function () { return state; }
  };

  if (document.readyState === 'loading' && typeof document.addEventListener === 'function') {
    document.addEventListener('DOMContentLoaded', initCommandCenter);
  } else {
    initCommandCenter();
  }
})(typeof window !== 'undefined' ? window : globalThis);
