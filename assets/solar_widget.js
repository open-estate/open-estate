/* Open Estate Sunroof-Equivalent Solar Potential Widget (`assets/solar_widget.js`)
 * Standalone themed interactive rooftop solar visualizer & 20-year financial simulator:
 * - 2D satellite solar flux heatmap (`fluxGrid`) + virtual PV panel polygons (`panels[]`)
 * - Clickable LiDAR/footprint roof facets, monthly electric bill slider, EV & heat-pump load toggles
 * - Solar-Only vs Solar + 13.5 kWh LFP Battery (CA NEM 3.0 peak TOU arbitrage)
 * - Cash / 20-Yr Loan / Lease-PPA tabs + ECharts 20-yr cumulative cashflow & monthly kWh charts
 * - EPA eGRID 2023 carbon offset bar + HOA/condo shared-roof caution + EN/中文/ES i18n
 * - 100% CSS custom properties for automatic light/dark parity (`AGENTS.md`)
 */
(function initOpenEstateSolarWidget(globalObj) {
  'use strict';

  var g = globalObj || (typeof window !== 'undefined' ? window : globalThis);
  if (!g || typeof document === 'undefined' || !document.documentElement) {
    return;
  }

  var STYLE_ID = 'oeSolarWidgetStyles';
  var USGS_SAT_TMPL = 'https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/{z}/{y}/{x}';
  var SCC_SJ_SAT_TMPL = 'https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/tile/{z}/{y}/{x}';
  var OSM_STREETS_TMPL = 'https://tile.openstreetmap.org/{z}/{x}/{y}.png';
  var MONTH_LABELS = {
    en: ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'],
    zh: ['1月', '2月', '3月', '4月', '5月', '6月', '7月', '8月', '9月', '10月', '11月', '12月'],
    es: ['Ene', 'Feb', 'Mar', 'Abr', 'May', 'Jun', 'Jul', 'Ago', 'Sep', 'Oct', 'Nov', 'Dic']
  };
  var SEASON_WEIGHTS = [0.98, 0.92, 0.90, 0.92, 0.98, 1.08, 1.18, 1.18, 1.06, 0.94, 0.92, 0.94];

  var I18N = {
    en: {
      title: 'Rooftop Solar Potential & Savings',
      subTitle: '3D LiDAR roof irradiance, shading, panel layout & 20-year utility payback',
      loading: 'Analyzing 3DEP LiDAR roof planes, tree canopy shading, and NASA POWER solar irradiance...',
      unavailable: 'Solar analysis requires valid property coordinates.',
      noFootprint: 'No building footprint was available for this address. Showing local solar climate irradiance only.',
      provLidar: 'USGS 3DEP LiDAR + NASA POWER + PVGIS',
      provFootprint: 'Building Footprint Estimate + NASA POWER',
      provFallback: 'Clear-Sky Astronomical Fallback',
      suitOptimal: 'Optimal Solar Roof',
      suitGood: 'Good Solar Roof',
      suitShaded: 'Partially Shaded Roof',
      suitHoa: 'HOA / Condo Shared Roof Caution',
      existingSolar: 'Permitted PV Detected',
      kpiSunlight: 'Usable Sunlight',
      kpiRoof: 'Usable Roof Area',
      kpiArray: 'Recommended System',
      kpiOutput: 'Est. Year-1 Output',
      kpiNet20: '20-Yr Net Savings',
      hrsYr: 'hrs/yr',
      sqft: 'sq ft',
      ofRoof: 'of roof',
      panels: 'panels',
      maxPanels: 'max',
      billOffset: 'bill offset',
      paybackYr: 'Payback Yr',
      noPayback: '20+ yrs',
      immediate: 'Year 1',
      mapFluxLayer: 'Solar Flux',
      mapPanelsLayer: 'PV Panels',
      mapFacetsLayer: 'Roof Facets',
      mapSat: 'Satellite',
      mapStreets: 'Streets',
      legendLow: 'Shaded',
      legendPeak: 'Peak Sun',
      cfgTitle: 'Customize Your Home Load & Roof',
      billLabel: 'Avg. Monthly Electric Bill',
      impliedUse: 'Est. Load',
      evToggle: 'Add EV Charging (+3,500 kWh/yr)',
      hpToggle: 'Add Heat Pump (+4,000 kWh/yr)',
      facetsTitle: 'Clickable Roof Facets (Include / Exclude)',
      facetOptimal: 'Optimal',
      facetGood: 'Good',
      facetFair: 'Fair',
      facetShaded: 'Shaded',
      shadeLbl: 'shade',
      storageTitle: 'Storage & CA NEM 3.0 Mode',
      solarOnlyBtn: 'Solar Only',
      solarBattBtn: 'Solar + 13.5 kWh LFP Battery',
      nemBadge: 'CA NEM 3.0 Peak Arbitrage',
      costPerWatt: 'Install Cost ($/W)',
      incentivePct: 'Rebate / Tax Credit (%)',
      escPct: 'Utility Esc. (%/yr)',
      resetBtn: 'Reset',
      finCash: 'Cash Purchase',
      finLoan: '20-Yr Loan (6.99%)',
      finLease: 'Lease / PPA ($0 Down)',
      grossCost: 'System Gross Cost',
      incentives: 'Rebates / Credit',
      netCost: 'Net Upfront / Principal',
      monthlyPmt: 'Est. Monthly Payment',
      yr1Savings: 'Year-1 Bill Savings',
      yr1Net: 'Year-1 Net Cashflow',
      effRate: 'Effective Solar Cost',
      vsUtility: 'vs utility',
      chartCashTitle: '20-Year Cumulative Cashflow Comparison',
      chartMonthlyTitle: 'Monthly Solar Production vs. Home Load (kWh)',
      seriesUtility: 'Utility Bill (No Solar)',
      seriesSolarCost: 'Cumulative Net Solar Cost',
      seriesNetSavings: 'Cumulative Net Savings',
      seriesGen: 'Solar Generation (kWh)',
      seriesLoad: 'Est. Home Usage (kWh)',
      envTitle: 'EPA eGRID Environmental Impact',
      co2Yr: 'CO2 Avoided / Yr',
      co2_20: '20-Yr CO2 Avoided',
      eqMiles: 'Car Miles Avoided (20 Yr)',
      eqTrees: 'Tree Seedlings (10 Yr)',
      metricTons: 'MT',
      milesUnit: 'mi',
      treesUnit: 'trees',
      disclaimer: 'Engineering screening estimate only — not a PE structural audit or contractor bid. Confirm roof condition, 200A service panel capacity, and interconnection rules with a licensed installer.'
    },
    zh: {
      title: '屋顶光伏发电潜力与 20 年收益测算',
      subTitle: '基于 3DEP 激光雷达屋顶倾角、树荫遮挡与当地电价的 Sunroof 级测算',
      loading: '正在解析 USGS 3DEP 激光雷达屋顶坡面、树木遮挡与 NASA POWER 光照数据...',
      unavailable: '缺少有效经纬度坐标，无法计算屋顶光伏潜力。',
      noFootprint: '暂无该地址建筑轮廓，仅展示区域太阳辐射气候指标。',
      provLidar: 'USGS 3DEP 激光雷达 + NASA POWER + PVGIS',
      provFootprint: '建筑轮廓估算 + NASA POWER',
      provFallback: '晴空天文辐射备用估算',
      suitOptimal: '极佳光伏屋顶',
      suitGood: '良好光伏屋顶',
      suitShaded: '部分树荫遮挡',
      suitHoa: 'HOA / 联排公寓共享屋顶提示',
      existingSolar: '已检测到许可光伏',
      kpiSunlight: '年有效等效日照',
      kpiRoof: '可用屋顶面积',
      kpiArray: '推荐装机规模',
      kpiOutput: '首年预计发电量',
      kpiNet20: '20 年净节省电费',
      hrsYr: '小时/年',
      sqft: '平方英尺',
      ofRoof: '占总屋顶',
      panels: '块组件',
      maxPanels: '上限',
      billOffset: '电费抵扣率',
      paybackYr: '回本周期',
      noPayback: '20年以上',
      immediate: '第1年',
      mapFluxLayer: '光照热力图',
      mapPanelsLayer: '光伏板排布',
      mapFacetsLayer: '屋顶坡面',
      mapSat: '卫星航拍',
      mapStreets: '街道图',
      legendLow: '弱光遮挡',
      legendPeak: '强光峰值',
      cfgTitle: '自定义家庭用电与屋顶坡面',
      billLabel: '当前月均电费账单',
      impliedUse: '估算年用电',
      evToggle: '增加电动车充电 (+3,500 kWh/年)',
      hpToggle: '增加热泵空调/热水 (+4,000 kWh/年)',
      facetsTitle: '点击选择屋顶坡面 (纳入 / 排除)',
      facetOptimal: '最优',
      facetGood: '良好',
      facetFair: '一般',
      facetShaded: '遮挡',
      shadeLbl: '遮挡',
      storageTitle: '储能电池与加州 NEM 3.0 模式',
      solarOnlyBtn: '纯光伏方案',
      solarBattBtn: '光伏 + 13.5 kWh 磷酸铁锂电池',
      nemBadge: 'CA NEM 3.0 峰谷套利',
      costPerWatt: '装机单价 ($/W)',
      incentivePct: '补贴 / 税收抵免 (%)',
      escPct: '电价年涨幅 (%/年)',
      resetBtn: '重置默认',
      finCash: '全款购买',
      finLoan: '20 年光伏贷款 (6.99%)',
      finLease: '零首付租赁 / PPA',
      grossCost: '系统总造价',
      incentives: '补贴 / 抵免金额',
      netCost: '净投入 / 贷款本金',
      monthlyPmt: '预计月供 / 月租',
      yr1Savings: '首年电费节省',
      yr1Net: '首年净现金流',
      effRate: '光伏度电成本',
      vsUtility: '对比电网',
      chartCashTitle: '20 年累计用电成本与净收益曲线',
      chartMonthlyTitle: '逐月发电量 vs 家庭用电负荷 (kWh)',
      seriesUtility: '未装光伏累计电费',
      seriesSolarCost: '安装光伏累计净支出',
      seriesNetSavings: '累计净节省金额',
      seriesGen: '光伏发电量 (kWh)',
      seriesLoad: '估算家庭用电 (kWh)',
      envTitle: 'EPA eGRID 减碳环境效益',
      co2Yr: '年减排二氧化碳',
      co2_20: '20 年累计减碳',
      eqMiles: '等效少开里程 (20年)',
      eqTrees: '等效植树 (10年成活)',
      metricTons: '公吨',
      milesUnit: '英里',
      treesUnit: '棵树',
      disclaimer: '本测算为工程级公开数据筛选评估，不可替代结构工程师勘测或施工报价。安装前请核实屋面寿命、200A 配电箱容量及供电公司并网条款。'
    },
    es: {
      title: 'Potencial Solar del Techo y Ahorro',
      subTitle: 'Irradiancia 3D LiDAR, sombras, diseño de paneles y retorno a 20 años',
      loading: 'Analizando planos LiDAR 3DEP, sombras de árboles e irradiancia NASA POWER...',
      unavailable: 'Se requieren coordenadas válidas para analizar el potencial solar.',
      noFootprint: 'No hay huella del edificio disponible; se muestra solo irradiancia climática.',
      provLidar: 'USGS 3DEP LiDAR + NASA POWER + PVGIS',
      provFootprint: 'Estimación por Huella + NASA POWER',
      provFallback: 'Estimación Astronómica de Cielo Despejado',
      suitOptimal: 'Techo Solar Óptimo',
      suitGood: 'Buen Techo Solar',
      suitShaded: 'Parcialmente Sombreado',
      suitHoa: 'Aviso de Techo Compartido (HOA / Condo)',
      existingSolar: 'Solar Permitido Detectado',
      kpiSunlight: 'Sol Útil Anual',
      kpiRoof: 'Área Útil de Techo',
      kpiArray: 'Sistema Recomendado',
      kpiOutput: 'Generación Año 1',
      kpiNet20: 'Ahorro Neto 20 Años',
      hrsYr: 'hrs/año',
      sqft: 'pies²',
      ofRoof: 'del techo',
      panels: 'paneles',
      maxPanels: 'máx',
      billOffset: 'cobertura',
      paybackYr: 'Retorno Año',
      noPayback: '20+ años',
      immediate: 'Año 1',
      mapFluxLayer: 'Flujo Solar',
      mapPanelsLayer: 'Paneles PV',
      mapFacetsLayer: 'Planos del Techo',
      mapSat: 'Satélite',
      mapStreets: 'Calles',
      legendLow: 'Sombra',
      legendPeak: 'Máximo Sol',
      cfgTitle: 'Personaliza Tu Consumo y Techo',
      billLabel: 'Factura Eléctrica Mensual Promedio',
      impliedUse: 'Consumo Est.',
      evToggle: 'Agregar Cargador EV (+3,500 kWh/año)',
      hpToggle: 'Agregar Bomba de Calor (+4,000 kWh/año)',
      facetsTitle: 'Planos del Techo (Incluir / Excluir)',
      facetOptimal: 'Óptimo',
      facetGood: 'Bueno',
      facetFair: 'Regular',
      facetShaded: 'Sombra',
      shadeLbl: 'sombra',
      storageTitle: 'Batería y Modo CA NEM 3.0',
      solarOnlyBtn: 'Solo Solar',
      solarBattBtn: 'Solar + Batería LFP 13.5 kWh',
      nemBadge: 'Arbitraje Pico CA NEM 3.0',
      costPerWatt: 'Costo ($/W)',
      incentivePct: 'Incentivo / Crédito (%)',
      escPct: 'Alza Tarifa (%/año)',
      resetBtn: 'Restablecer',
      finCash: 'Contado',
      finLoan: 'Préstamo 20a (6.99%)',
      finLease: 'Arrendamiento / PPA ($0)',
      grossCost: 'Costo Bruto',
      incentives: 'Incentivos / Crédito',
      netCost: 'Costo Neto / Principal',
      monthlyPmt: 'Pago Mensual Est.',
      yr1Savings: 'Ahorro Año 1',
      yr1Net: 'Flujo Neto Año 1',
      effRate: 'Costo Efectivo Solar',
      vsUtility: 'vs tarifa',
      chartCashTitle: 'Flujo de Caja Acumulado a 20 Años',
      chartMonthlyTitle: 'Generación Solar vs. Consumo Mensual (kWh)',
      seriesUtility: 'Factura sin Solar',
      seriesSolarCost: 'Costo Neto Acumulado Solar',
      seriesNetSavings: 'Ahorro Neto Acumulado',
      seriesGen: 'Generación Solar (kWh)',
      seriesLoad: 'Consumo Estimado (kWh)',
      envTitle: 'Impacto Ambiental EPA eGRID',
      co2Yr: 'CO2 Evitado / Año',
      co2_20: 'CO2 Evitado 20 Años',
      eqMiles: 'Millas Evitadas (20a)',
      eqTrees: 'Árboles Plantados (10a)',
      metricTons: 't',
      milesUnit: 'mi',
      treesUnit: 'árboles',
      disclaimer: 'Estimación preliminar de ingeniería; no sustituye inspección estructural ni cotización final de instalador.'
    }
  };

  function core() {
    return g.OpenEstateCore || null;
  }

  function langCode() {
    var c = core();
    if (c && typeof c.langCode === 'function') return c.langCode();
    var l = String((document.documentElement && document.documentElement.lang) || 'en').toLowerCase();
    if (l.indexOf('zh') === 0) return 'zh';
    if (l.indexOf('es') === 0) return 'es';
    return 'en';
  }

  function tr(key) {
    var l = langCode();
    return (I18N[l] && I18N[l][key]) || I18N.en[key] || key;
  }

  function esc(s) {
    var c = core();
    if (c && typeof c.esc === 'function') return c.esc(s);
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (ch) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch];
    });
  }

  function isN(v) {
    return typeof v === 'number' && isFinite(v);
  }

  function fmtUsd(v) {
    if (!isN(v)) return '—';
    var c = core();
    if (c && typeof c.fUsd === 'function') return c.fUsd(v);
    var r = Math.round(v);
    return (r < 0 ? '-$' : '$') + Math.abs(r).toLocaleString();
  }

  function fmtNum(v, dec) {
    if (!isN(v)) return '—';
    var c = core();
    if (c && typeof c.fNum === 'function') return c.fNum(v, dec || 0);
    return Number(v).toLocaleString(undefined, { maximumFractionDigits: dec || 0 });
  }

  function cssVar(name, fallback) {
    try {
      var val = getComputedStyle(document.documentElement).getPropertyValue(name);
      return (val && val.trim()) ? val.trim() : fallback;
    } catch (e) {
      return fallback;
    }
  }

  function isThemeDark() {
    try {
      var dt = document.documentElement.getAttribute('data-theme');
      if (dt === 'dark') return true;
      if (dt === 'light') return false;
      return typeof g.matchMedia === 'function' && g.matchMedia('(prefers-color-scheme: dark)').matches;
    } catch (e) {
      return false;
    }
  }

  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return;
    var css = [
      '.oe-sol{display:flex;flex-direction:column;gap:14px;color:var(--ink);font-family:var(--font);}',
      '.oe-sol-hdr{display:flex;align-items:flex-start;justify-content:space-between;gap:12px;flex-wrap:wrap;}',
      '.oe-sol-title{margin:0;font-size:20px;font-weight:800;letter-spacing:-.3px;color:var(--ink);}',
      '.oe-sol-sub{margin:2px 0 0;font-size:13px;color:var(--muted);}',
      '.oe-sol-badges{display:flex;flex-wrap:wrap;gap:6px;align-items:center;}',
      '.oe-sol-pill{display:inline-flex;align-items:center;gap:5px;padding:4px 10px;border-radius:999px;font-size:11.5px;font-weight:700;border:1px solid var(--line);background:var(--surface-2);color:var(--ink2);white-space:nowrap;}',
      '.oe-sol-pill.good{background:var(--goodbg);color:var(--good);border-color:color-mix(in srgb,var(--good) 28%,transparent);}',
      '.oe-sol-pill.watch{background:var(--watchbg);color:var(--watch);border-color:color-mix(in srgb,var(--watch) 28%,transparent);}',
      '.oe-sol-pill.brand{background:var(--brandsoft);color:var(--brand);border-color:color-mix(in srgb,var(--brand) 30%,transparent);}',
      '.oe-sol-alert{border:1px solid var(--line-strong);background:var(--watchbg);color:var(--watch);border-radius:12px;padding:10px 13px;font-size:12.5px;line-height:1.45;font-weight:600;}',
      '.oe-sol-kpis{display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:10px;}',
      '@media(max-width:980px){.oe-sol-kpis{grid-template-columns:repeat(2,minmax(0,1fr));}}',
      '.oe-sol-kpi{background:var(--surface-2);border:1px solid var(--line);border-radius:13px;padding:11px 13px;display:flex;flex-direction:column;gap:2px;min-width:0;}',
      '.oe-sol-kpi-k{font-size:11px;font-weight:700;text-transform:uppercase;letter-spacing:.4px;color:var(--muted);}',
      '.oe-sol-kpi-v{font-size:20px;font-weight:800;color:var(--ink);letter-spacing:-.4px;line-height:1.2;}',
      '.oe-sol-kpi-v.good{color:var(--good);}',
      '.oe-sol-kpi-s{font-size:11.5px;color:var(--muted);line-height:1.3;}',
      '.oe-sol-split{display:grid;grid-template-columns:1.15fr 1fr;gap:14px;align-items:stretch;}',
      '@media(max-width:960px){.oe-sol-split{grid-template-columns:1fr;}}',
      '.oe-sol-mapbox{position:relative;border:1px solid var(--line);border-radius:14px;overflow:hidden;background:var(--surface-2);min-height:360px;display:flex;flex-direction:column;}',
      '.oe-sol-mapbar{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;padding:8px 11px;border-bottom:1px solid var(--line);background:var(--surface);}',
      '.oe-sol-chips{display:flex;flex-wrap:wrap;gap:6px;align-items:center;}',
      '.oe-sol-chip{display:inline-flex;align-items:center;gap:5px;border:1px solid var(--line);background:var(--surface-2);color:var(--ink2);border-radius:999px;padding:4px 10px;font:700 11.5px var(--font);cursor:pointer;transition:all .14s;}',
      '.oe-sol-chip.on{background:var(--brandsoft);border-color:var(--brand);color:var(--brand);}',
      '.oe-sol-chip i{width:8px;height:8px;border-radius:50%;display:inline-block;}',
      '.oe-sol-mapcanvas{position:relative;flex:1 1 auto;min-height:300px;width:100%;background:var(--soft);}',
      '.oe-sol-legend{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:7px 12px;border-top:1px solid var(--line);background:var(--surface);font-size:11.5px;color:var(--muted);}',
      '.oe-sol-grad{flex:1;max-width:190px;height:8px;border-radius:999px;background:linear-gradient(90deg,#3b1d6e 0%,#b45309 35%,#f59e0b 70%,#fde047 100%);}',
      '.oe-sol-cfg{background:var(--surface);border:1px solid var(--line);border-radius:14px;padding:14px;display:flex;flex-direction:column;gap:12px;}',
      '.oe-sol-cfg-t{margin:0;font-size:14.5px;font-weight:800;color:var(--ink);}',
      '.oe-sol-slider-row{display:flex;flex-direction:column;gap:5px;background:var(--surface-2);border:1px solid var(--line);border-radius:11px;padding:10px 12px;}',
      '.oe-sol-slider-hdr{display:flex;justify-content:space-between;align-items:baseline;gap:8px;}',
      '.oe-sol-slider-hdr label{font-size:12.5px;font-weight:700;color:var(--ink2);}',
      '.oe-sol-slider-val{font-size:16px;font-weight:800;color:var(--brand);}',
      '.oe-sol-slider-row input[type=range]{width:100%;accent-color:var(--brand);cursor:pointer;margin:2px 0;}',
      '.oe-sol-toggles{display:grid;grid-template-columns:1fr 1fr;gap:8px;}',
      '@media(max-width:520px){.oe-sol-toggles{grid-template-columns:1fr;}}',
      '.oe-sol-chk{display:flex;align-items:center;gap:7px;background:var(--surface-2);border:1px solid var(--line);border-radius:10px;padding:7px 10px;font-size:12px;font-weight:600;color:var(--ink2);cursor:pointer;user-select:none;}',
      '.oe-sol-chk input{accent-color:var(--brand);cursor:pointer;}',
      '.oe-sol-facets{display:flex;flex-wrap:wrap;gap:6px;}',
      '.oe-sol-facet-btn{border:1px solid var(--line);background:var(--surface-2);color:var(--ink2);border-radius:10px;padding:6px 9px;font:600 11.5px/1.3 var(--font);cursor:pointer;text-align:left;transition:all .14s;display:flex;flex-direction:column;gap:1px;}',
      '.oe-sol-facet-btn.on{border-color:var(--brand);background:var(--brandsoft);color:var(--ink);}',
      '.oe-sol-facet-btn:hover{border-color:var(--brand2);}',
      '.oe-sol-storage{display:flex;flex-direction:column;gap:7px;background:var(--surface-2);border:1px solid var(--line);border-radius:11px;padding:10px 12px;}',
      '.oe-sol-seg{display:inline-flex;background:var(--soft);border:1px solid var(--line);border-radius:999px;padding:3px;gap:3px;flex-wrap:wrap;}',
      '.oe-sol-seg button{border:0;background:transparent;color:var(--ink2);font:700 12px var(--font);padding:5px 12px;border-radius:999px;cursor:pointer;transition:all .14s;}',
      '.oe-sol-seg button.on{background:var(--brand-solid);color:#fff;}',
      '.oe-sol-adv{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:8px;}',
      '@media(max-width:600px){.oe-sol-adv{grid-template-columns:1fr;}}',
      '.oe-sol-numbox{display:flex;flex-direction:column;gap:3px;font-size:11px;font-weight:700;color:var(--muted);}',
      '.oe-sol-numbox input{border:1px solid var(--line-strong);background:var(--surface);color:var(--ink);border-radius:8px;padding:5px 8px;font:700 12.5px var(--font);width:100%;box-sizing:border-box;}',
      '.oe-sol-fin{background:var(--surface);border:1px solid var(--line);border-radius:14px;padding:14px;display:flex;flex-direction:column;gap:12px;}',
      '.oe-sol-fin-top{display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap;}',
      '.oe-sol-fin-cards{display:grid;grid-template-columns:repeat(6,minmax(0,1fr));gap:9px;}',
      '@media(max-width:1024px){.oe-sol-fin-cards{grid-template-columns:repeat(3,minmax(0,1fr));}}',
      '@media(max-width:560px){.oe-sol-fin-cards{grid-template-columns:repeat(2,minmax(0,1fr));}}',
      '.oe-sol-fcard{background:var(--surface-2);border:1px solid var(--line);border-radius:11px;padding:9px 11px;}',
      '.oe-sol-fcard-k{font-size:11px;font-weight:700;color:var(--muted);}',
      '.oe-sol-fcard-v{font-size:16px;font-weight:800;color:var(--ink);margin-top:2px;}',
      '.oe-sol-fcard-s{font-size:11px;color:var(--muted);}',
      '.oe-sol-charts{display:grid;grid-template-columns:1fr 1fr;gap:14px;}',
      '@media(max-width:900px){.oe-sol-charts{grid-template-columns:1fr;}}',
      '.oe-sol-chartcard{background:var(--surface-2);border:1px solid var(--line);border-radius:12px;padding:11px 12px;display:flex;flex-direction:column;gap:6px;}',
      '.oe-sol-chart-t{font-size:13px;font-weight:800;color:var(--ink);}',
      '.oe-sol-chartbox{width:100%;height:260px;}',
      '.oe-sol-env{display:grid;grid-template-columns:1.2fr repeat(4,minmax(0,1fr));gap:10px;background:var(--surface-2);border:1px solid var(--line);border-radius:13px;padding:12px 14px;align-items:center;}',
      '@media(max-width:900px){.oe-sol-env{grid-template-columns:repeat(2,minmax(0,1fr));}}',
      '.oe-sol-note{font-size:11.5px;color:var(--muted);line-height:1.45;}',
      '.oe-sol-btn{border:1px solid var(--line-strong);background:var(--surface-2);color:var(--ink2);border-radius:8px;padding:4px 10px;font:700 11.5px var(--font);cursor:pointer;}',
      '.oe-sol-btn:hover{border-color:var(--brand);color:var(--brand);}'
    ].join('');
    var st = document.createElement('style');
    st.id = STYLE_ID;
    st.textContent = css;
    document.head.appendChild(st);
  }

  function fluxColorRgba(norm255) {
    if (!norm255 || norm255 <= 0) return [0, 0, 0, 0];
    var t = Math.max(0, Math.min(1, (norm255 - 1) / 254));
    // Deep shade violet (#3b1d6e) -> warm amber (#d97706) -> gold (#f59e0b) -> sun yellow (#fde047)
    var stops = [
      [0.00, 59, 29, 110, 170],
      [0.35, 194, 65, 12, 195],
      [0.70, 245, 158, 11, 210],
      [1.00, 253, 224, 71, 225]
    ];
    for (var i = 0; i < stops.length - 1; i++) {
      var a = stops[i], b = stops[i + 1];
      if (t >= a[0] && t <= b[0]) {
        var f = (t - a[0]) / Math.max(1e-6, b[0] - a[0]);
        return [
          Math.round(a[1] + (b[1] - a[1]) * f),
          Math.round(a[2] + (b[2] - a[2]) * f),
          Math.round(a[3] + (b[3] - a[3]) * f),
          Math.round(a[4] + (b[4] - a[4]) * f)
        ];
      }
    }
    return [253, 224, 71, 225];
  }

  function renderRotatedFluxDataUrl(fluxGrid, enabledFacets) {
    if (!fluxGrid || !fluxGrid.w || !fluxGrid.h || !Array.isArray(fluxGrid.norm255)) return null;
    var w = fluxGrid.w, h = fluxGrid.h, step = fluxGrid.step || 0.5;
    var ox = fluxGrid.ox || 0, oy = fluxGrid.oy || 0;
    var rotRad = (fluxGrid.rot || 0) * Math.PI / 180;
    var lat0 = (fluxGrid.origin && fluxGrid.origin.lat) || 37.0;
    var lon0 = (fluxGrid.origin && fluxGrid.origin.lon) || -122.0;
    var kx = 111320 * Math.cos(lat0 * Math.PI / 180);
    var ky = 111132.92;
    var cs = Math.cos(rotRad), sn = Math.sin(rotRad);
    var toLL = function (i, j) {
      var lx = i * step, ly = j * step;
      var e = ox + lx * cs - ly * sn;
      var n = oy + lx * sn + ly * cs;
      return [lat0 + n / ky, lon0 + e / kx];
    };
    var corners = [toLL(0, 0), toLL(w, 0), toLL(w, h), toLL(0, h)];
    var minLat = Infinity, maxLat = -Infinity, minLon = Infinity, maxLon = -Infinity;
    corners.forEach(function (c) {
      minLat = Math.min(minLat, c[0]);
      maxLat = Math.max(maxLat, c[0]);
      minLon = Math.min(minLon, c[1]);
      maxLon = Math.max(maxLon, c[1]);
    });

    var scale = 4;
    var cw = Math.max(32, Math.min(512, Math.round(((maxLon - minLon) * kx) / step * scale)));
    var ch = Math.max(32, Math.min(512, Math.round(((maxLat - minLat) * ky) / step * scale)));
    var canvas = document.createElement('canvas');
    canvas.width = cw;
    canvas.height = ch;
    var ctx = canvas.getContext('2d');
    if (!ctx) return null;

    var img = ctx.createImageData(cw, ch);
    var px = img.data;
    var fids = fluxGrid.facetIds || [];
    for (var py = 0; py < ch; py++) {
      var la = maxLat - ((py + 0.5) / ch) * (maxLat - minLat);
      var n = (la - lat0) * ky;
      for (var pxX = 0; pxX < cw; pxX++) {
        var lo = minLon + ((pxX + 0.5) / cw) * (maxLon - minLon);
        var e = (lo - lon0) * kx;
        var de = e - ox, dn = n - oy;
        var gi = Math.round((de * cs + dn * sn) / step);
        var gj = Math.round((-de * sn + dn * cs) / step);
        if (gi < 0 || gj < 0 || gi >= w || gj >= h) continue;
        var idx = gj * w + gi;
        var v = fluxGrid.norm255[idx];
        if (!v) continue;
        var fid = fids[idx];
        var dim = (fid != null && fid >= 0 && enabledFacets && enabledFacets[fid] === false);
        var rgba = fluxColorRgba(v);
        var o = (py * cw + pxX) * 4;
        px[o] = rgba[0];
        px[o + 1] = rgba[1];
        px[o + 2] = rgba[2];
        px[o + 3] = dim ? Math.round(rgba[3] * 0.25) : rgba[3];
      }
    }
    ctx.putImageData(img, 0, 0);
    return {
      url: canvas.toDataURL('image/png'),
      bounds: [[minLat, minLon], [maxLat, maxLon]]
    };
  }

  function isSantaClaraOrthoBounds(lat, lon) {
    return lat >= 36.89 && lat <= 37.49 && lon >= -122.22 && lon <= -121.58;
  }

  function monthlyLoanPayment(principal, aprPct, years) {
    if (principal <= 0) return 0;
    var n = years * 12;
    var r = (aprPct / 100) / 12;
    if (r <= 1e-9) return principal / n;
    return principal * r * Math.pow(1 + r, n) / (Math.pow(1 + r, n) - 1);
  }

  function computeInteractiveSimulation(data, state) {
    var fin = data.financials || {};
    var rate = (isN(fin.ratePerKwhUsd) && fin.ratePerKwhUsd > 0) ? fin.ratePerKwhUsd : 0.28;
    var peakRate = (isN(fin.peakRateKwhUsd) && fin.peakRateKwhUsd > 0) ? fin.peakRateKwhUsd : rate * 1.25;
    var accRate = isN(fin.accExportRateKwhUsd) ? fin.accExportRateKwhUsd : 0.065;
    var isNem3 = fin.tariffRegime === 'ca_nem_3_net_billing';
    var isCaMuni = fin.tariffRegime === 'ca_municipal_solar_schedule';

    var baseMonthlyBill = Math.max(50, Math.min(800, Number(state.monthlyBillUsd) || 220));
    var baseAnnualUsage = (baseMonthlyBill / rate) * 12;
    var extraUsage = (state.addEv ? 3500 : 0) + (state.addHeatPump ? 4000 : 0);
    var annualUsageKwh = baseAnnualUsage + extraUsage;
    var annualUtilityBaseline = annualUsageKwh * rate;

    var monthlyUsage = SEASON_WEIGHTS.map(function (w) {
      return Math.round((annualUsageKwh / 12) * w);
    });

    // Filter panels by enabled facets, sorted by annualKwh desc
    var panels = Array.isArray(data.panels) ? data.panels : [];
    var enabledPanels = panels.filter(function (p) {
      return state.enabledFacets[p.facetId] !== false;
    });
    var selectedPanels = [];
    var sumKwh = 0;
    var monthlyGen = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

    for (var i = 0; i < enabledPanels.length; i++) {
      var p = enabledPanels[i];
      if (selectedPanels.length < 4 || sumKwh < annualUsageKwh * 1.03) {
        selectedPanels.push(p);
        sumKwh += Number(p.annualKwh) || 0;
        var mk = Array.isArray(p.monthlyKwh) ? p.monthlyKwh : [];
        for (var m = 0; m < 12; m++) {
          monthlyGen[m] += Number(mk[m]) || 0;
        }
      }
    }
    var selectedIds = {};
    selectedPanels.forEach(function (p) { selectedIds[p.id] = true; });

    var panelWattsTotal = selectedPanels.reduce(function (s, p) { return s + (Number(p.watts) || 410); }, 0);
    var systemKw = Math.round((panelWattsTotal / 1000) * 100) / 100;
    var annualKwh = Math.round(sumKwh);
    var monthlyGenRounded = monthlyGen.map(function (v) { return Math.round(v); });

    // If no panel array (no footprint), fall back cleanly to roof/climate figures
    if (panels.length === 0) {
      systemKw = Number(data.recommendedSystemKw) || 0;
      annualKwh = Number(data.annualKwh) || 0;
      monthlyGenRounded = Array.isArray(data.monthlyKwh) ? data.monthlyKwh.slice() : monthlyGenRounded;
    }

    var billOffsetPct = annualUsageKwh > 0 ? Math.min(125, Math.round((annualKwh / annualUsageKwh) * 1000) / 10) : 0;

    // Usable roof area from active facets
    var facets = Array.isArray(data.facets) ? data.facets : [];
    var activeUsableSqft = 0;
    var activeTotalSqft = Number(data.roofAreaSqft) || 0;
    var sunSum = 0, sunWeight = 0;
    facets.forEach(function (f) {
      if (state.enabledFacets[f.id] !== false) {
        activeUsableSqft += Number(f.usableAreaSqft) || 0;
        var w = Math.max(1, Number(f.usableAreaSqft) || 0);
        sunSum += (Number(f.sunHoursPerYear) || 0) * w;
        sunWeight += w;
      }
    });
    var usableRoofSqft = facets.length ? Math.round(activeUsableSqft) : data.usableRoofSqft;
    var usableSunHrs = sunWeight > 0 ? Math.round(sunSum / sunWeight) : (Number(data.usableSunHoursPerYear) || 0);

    // Year-1 savings under NEM 3.0 vs Solar + Battery
    var solarOnlyYr1 = 0, withBattYr1 = 0;
    if (isNem3) {
      solarOnlyYr1 = Math.min(annualUtilityBaseline, annualKwh * (0.48 * rate + 0.52 * accRate));
      withBattYr1 = Math.min(annualUtilityBaseline * 0.98, annualKwh * (0.48 * rate + 0.40 * (peakRate * 0.92) + 0.12 * accRate));
    } else if (isCaMuni) {
      solarOnlyYr1 = Math.min(annualUtilityBaseline * 0.96, annualKwh * rate * 0.84);
      withBattYr1 = Math.min(annualUtilityBaseline * 0.98, annualKwh * rate * 0.92);
    } else {
      solarOnlyYr1 = Math.min(annualUtilityBaseline * 0.96, annualKwh * rate * 0.86);
      withBattYr1 = Math.min(annualUtilityBaseline * 0.98, annualKwh * rate * 0.93);
    }

    var useBatt = Boolean(state.withBattery);
    var yr1Sav = useBatt ? withBattYr1 : solarOnlyYr1;
    var cpw = Math.max(1.8, Math.min(6.0, Number(state.costPerWatt) || Number(fin.costPerWattUsd) || 2.95));
    var battCost = useBatt ? (Number(fin.batteryCostUsd) || 11500) : 0;
    var grossCost = Math.round(systemKw * 1000 * cpw) + battCost;
    var incPct = Math.max(0, Math.min(60, Number(state.incentivePct) || 0)) / 100;
    var escRate = Math.max(0, Math.min(12, Number(state.escalationPct) || Number(fin.utilityEscalationPct) || 3.5)) / 100;
    var degRate = (Number(fin.panelDegradationPct) || 0.5) / 100;
    var invRefresh = systemKw > 0 ? 2100 : 0;

    function buildPath(mode) {
      var utilCum = [0], solCum = [], netCum = [];
      var incUsd = Math.round(grossCost * (mode === 'lease' ? 0.30 : incPct));
      var netUsd = Math.max(0, grossCost - incUsd);
      var yrlyCash = 0;
      var loanApr = 6.99;
      var moPmt = 0;
      var down = 0;
      var leaseBaseYr = 0;

      if (mode === 'cash') {
        solCum.push(netUsd);
        netCum.push(-netUsd);
      } else if (mode === 'loan') {
        moPmt = monthlyLoanPayment(netUsd, loanApr, 20);
        yrlyCash = moPmt * 12;
        solCum.push(0);
        netCum.push(0);
      } else {
        leaseBaseYr = Math.max(0, Math.min(yr1Sav * (useBatt ? 0.85 : 0.82), netUsd * (useBatt ? 0.078 : 0.074)));
        moPmt = leaseBaseYr / 12;
        solCum.push(0);
        netCum.push(0);
      }

      var cSol = mode === 'cash' ? netUsd : 0;
      var cUtil = 0;
      var totalKwh20 = 0;
      var payback = mode === 'lease' ? 0 : null;
      var totalLeasePmts = 0;

      for (var y = 1; y <= 20; y++) {
        var deg = Math.pow(1 - degRate, y - 1);
        var esc = Math.pow(1 + escRate, y - 1);
        var ySav = yr1Sav * deg * esc;
        var yUtil = annualUtilityBaseline * esc;
        var yRem = Math.max(0, yUtil - ySav);
        var yMaint = (mode !== 'lease' && y === 12) ? invRefresh : 0;
        var yPmt = mode === 'loan' ? yrlyCash : (mode === 'lease' ? leaseBaseYr * Math.pow(1.029, y - 1) : 0);
        totalLeasePmts += (mode === 'lease' ? yPmt : 0);
        var prevNet = cUtil - cSol;
        cSol += yPmt + yRem + yMaint;
        cUtil += yUtil;
        var curNet = cUtil - cSol;
        if (payback === null && curNet >= 0 && curNet > prevNet) {
          var frac = (-prevNet) / Math.max(1, curNet - prevNet);
          payback = Math.round(((y - 1) + Math.max(0, Math.min(1, frac))) * 10) / 10;
        }
        totalKwh20 += annualKwh * deg;
        utilCum.push(Math.round(cUtil));
        solCum.push(Math.round(cSol));
        netCum.push(Math.round(curNet));
      }

      var effKwh = totalKwh20 > 0
        ? (mode === 'cash' ? (netUsd + invRefresh) / totalKwh20 : mode === 'loan' ? (yrlyCash * 20 + invRefresh) / totalKwh20 : totalLeasePmts / totalKwh20)
        : 0;
      var yr1Out = mode === 'loan' ? yrlyCash : mode === 'lease' ? leaseBaseYr : 0;
      return {
        mode: mode,
        grossCostUsd: grossCost,
        incentiveUsd: incUsd,
        netCostUsd: netUsd,
        downPaymentUsd: mode === 'cash' ? netUsd : down,
        monthlyPaymentUsd: Math.round(moPmt),
        year1BillSavingsUsd: Math.round(yr1Sav),
        year1NetCashflowUsd: Math.round(yr1Sav - yr1Out),
        paybackYears: payback,
        netSavings20yrUsd: netCum[netCum.length - 1] || 0,
        effectiveSolarKwhUsd: Math.round(effKwh * 1000) / 1000,
        cumulative20yrUtilityOutlayUsd: utilCum,
        cumulative20yrSolarOutlayUsd: solCum,
        cumulative20yrNetSavingsUsd: netCum
      };
    }

    var paths = {
      cash: buildPath('cash'),
      loan: buildPath('loan'),
      lease: buildPath('lease')
    };

    // Environmental scaling proportional to annualKwh vs baseline
    var envBase = data.environmental || {};
    var baseKwh = Math.max(1, Number(data.annualKwh) || 1);
    var envRatio = annualKwh / baseKwh;
    var co2Yr = Math.round(((Number(envBase.co2OffsetMetricTonsYr) || 0) * envRatio) * 100) / 100;
    var co2_20 = Math.round(((Number(envBase.co2OffsetMetricTons20yr) || 0) * envRatio) * 10) / 10;
    var miles20 = Math.round((Number(envBase.eqMilesDriven20yr) || 0) * envRatio);
    var trees10 = Math.round((Number(envBase.eqTreeSeedlings10yr) || 0) * envRatio);

    return {
      systemKw: systemKw,
      panelCount: selectedPanels.length || (panels.length === 0 ? (Number(data.recommendedPanelCount) || 0) : 0),
      maxPanelCount: enabledPanels.length,
      selectedIds: selectedIds,
      annualKwh: annualKwh,
      monthlyKwh: monthlyGenRounded,
      monthlyUsageKwh: monthlyUsage,
      annualUsageKwh: Math.round(annualUsageKwh),
      usableRoofSqft: usableRoofSqft,
      usableRoofPct: activeTotalSqft > 0 && usableRoofSqft != null ? Math.round((usableRoofSqft / activeTotalSqft) * 1000) / 10 : data.usableRoofPct,
      usableSunHours: usableSunHrs,
      billOffsetPct: billOffsetPct,
      ratePerKwh: rate,
      solarOnlyYr1: Math.round(solarOnlyYr1),
      withBattYr1: Math.round(withBattYr1),
      isNem3: isNem3,
      paths: paths,
      activePath: paths[state.financeTab] || paths.cash,
      env: {
        subregion: envBase.egridSubregion || 'US',
        fullName: envBase.egridFullName || 'U.S. Grid',
        co2LbPerMwh: envBase.co2LbPerMwh || 767.2,
        co2Yr: co2Yr,
        co2_20: co2_20,
        miles20: miles20,
        trees10: trees10
      }
    };
  }

  function createWidgetController(hostEl, initialData, initialOpts) {
    injectStyles();
    var opts = initialOpts || {};
    var data = initialData || null;
    var mapInst = null;
    var basemapLayer = null;
    var fluxOverlay = null;
    var panelLayer = null;
    var facetLayer = null;
    var cashChart = null;
    var monthChart = null;
    var pollTimer = null;

    var state = {
      basemap: 'satellite',
      showFlux: true,
      showPanels: true,
      showFacets: true,
      financeTab: 'cash',
      monthlyBillUsd: 220,
      addEv: false,
      addHeatPump: false,
      withBattery: false,
      costPerWatt: 2.95,
      incentivePct: 0,
      escalationPct: 3.5,
      enabledFacets: {}
    };

    function seedStateFromData(d) {
      if (!d) return;
      var fin = d.financials || {};
      state.monthlyBillUsd = Math.max(50, Math.min(800, Math.round(Number(fin.monthlyBillUsd) || 220)));
      state.withBattery = Boolean(fin.batteryRecommended);
      state.costPerWatt = Number(fin.costPerWattUsd) || 2.95;
      state.escalationPct = Number(fin.utilityEscalationPct) || 3.5;
      state.incentivePct = 0;
      state.enabledFacets = {};
      (Array.isArray(d.facets) ? d.facets : []).forEach(function (f) {
        state.enabledFacets[f.id] = f.recommended !== false;
      });
      // If all facets were shaded, enable the first facet so user sees output
      var anyOn = Object.keys(state.enabledFacets).some(function (k) { return state.enabledFacets[k]; });
      if (!anyOn && Array.isArray(d.facets) && d.facets.length) {
        state.enabledFacets[d.facets[0].id] = true;
      }
    }

    function teardownMapsAndCharts() {
      if (mapInst) { try { mapInst.remove(); } catch (e) {} mapInst = null; }
      basemapLayer = null;
      fluxOverlay = null;
      panelLayer = null;
      facetLayer = null;
      if (cashChart) { try { cashChart.dispose(); } catch (e) {} cashChart = null; }
      if (monthChart) { try { monthChart.dispose(); } catch (e) {} monthChart = null; }
    }

    function updateMapOverlays(sim) {
      if (!mapInst || typeof g.L === 'undefined' || !data) return;
      if (fluxOverlay) {
        try { mapInst.removeLayer(fluxOverlay); } catch (e) {}
        fluxOverlay = null;
      }
      if (state.showFlux && data.fluxGrid) {
        var imgInfo = renderRotatedFluxDataUrl(data.fluxGrid, state.enabledFacets);
        if (imgInfo) {
          fluxOverlay = g.L.imageOverlay(imgInfo.url, imgInfo.bounds, { opacity: 0.85, interactive: false });
          fluxOverlay.addTo(mapInst);
        }
      }
      if (facetLayer) {
        facetLayer.clearLayers();
        if (state.showFacets && Array.isArray(data.facets)) {
          data.facets.forEach(function (f) {
            if (!Array.isArray(f.polygonLatLon) || f.polygonLatLon.length < 3) return;
            var on = state.enabledFacets[f.id] !== false;
            var poly = g.L.polygon(f.polygonLatLon, {
              color: on ? '#2dd4bf' : '#94a3b8',
              weight: on ? 1.8 : 1.0,
              dashArray: on ? null : '4,4',
              fillColor: on ? '#14b8a6' : '#64748b',
              fillOpacity: on ? 0.08 : 0.14
            });
            poly.bindTooltip(
              '<b>' + esc(f.label) + '</b><br>' +
              fmtNum(f.usableAreaSqft) + ' ' + esc(tr('sqft')) + ' · ' +
              fmtNum(f.annualKwhPerKw) + ' kWh/kW/yr · ' +
              esc(tr('shadeLbl')) + ' ' + fmtNum(f.shadingLossPct, 1) + '%',
              { sticky: true }
            );
            poly.on('click', function () {
              state.enabledFacets[f.id] = !(state.enabledFacets[f.id] !== false);
              refreshDynamicViews();
            });
            facetLayer.addLayer(poly);
          });
        }
      }
      if (panelLayer) {
        panelLayer.clearLayers();
        if (state.showPanels && Array.isArray(data.panels)) {
          data.panels.forEach(function (p) {
            if (!Array.isArray(p.cornersLatLon) || p.cornersLatLon.length < 4) return;
            var facetOn = state.enabledFacets[p.facetId] !== false;
            var sel = Boolean(sim && sim.selectedIds[p.id]);
            var poly = g.L.polygon(p.cornersLatLon, {
              color: sel ? '#7dd3fc' : (facetOn ? '#64748b' : '#475569'),
              weight: sel ? 1.2 : 0.7,
              dashArray: sel ? null : '2,2',
              fillColor: sel ? '#0c2d5e' : '#1e293b',
              fillOpacity: sel ? 0.88 : (facetOn ? 0.35 : 0.15)
            });
            poly.bindTooltip(
              '<b>410W PV #' + (p.id + 1) + '</b><br>' +
              fmtNum(p.annualKwh) + ' kWh/yr · ' + fmtNum(p.sunHours) + ' ' + esc(tr('hrsYr')),
              { sticky: true }
            );
            panelLayer.addLayer(poly);
          });
        }
      }
    }

    function initMap(mapEl, sim) {
      if (!mapEl || !data) return;
      var origin = (data.fluxGrid && data.fluxGrid.origin) || {};
      var lat = origin.lat || (g.HOME && g.HOME[0]);
      var lon = origin.lon || (g.HOME && g.HOME[1]);
      if (!isN(lat) || !isN(lon)) return;

      if (typeof g.L === 'undefined') {
        // Minimal Canvas fallback when Leaflet is absent
        var imgInfo = renderRotatedFluxDataUrl(data.fluxGrid, state.enabledFacets);
        mapEl.innerHTML = imgInfo
          ? '<img src="' + imgInfo.url + '" alt="Solar Flux" style="width:100%;height:100%;object-fit:contain;display:block;padding:12px;">'
          : '<div style="padding:24px;color:var(--muted);text-align:center;">' + esc(tr('noFootprint')) + '</div>';
        return;
      }

      if (mapInst) { try { mapInst.remove(); } catch (e) {} }
      var useScc = isSantaClaraOrthoBounds(lat, lon);
      var satUrl = useScc ? SCC_SJ_SAT_TMPL : USGS_SAT_TMPL;
      var maxZ = useScc ? 21 : 19;

      mapInst = g.L.map(mapEl, {
        scrollWheelZoom: false,
        zoomControl: true,
        minZoom: 15,
        maxZoom: 22
      }).setView([lat, lon], 20);
      if (mapInst.attributionControl && !(core() && typeof core().isDisplayLinksEnabled === 'function' && core().isDisplayLinksEnabled())) {
        mapInst.attributionControl.setPrefix(false);
      }

      function setMapBase(kind) {
        state.basemap = kind;
        if (basemapLayer) { try { mapInst.removeLayer(basemapLayer); } catch (e) {} }
        if (kind === 'streets') {
          basemapLayer = g.L.tileLayer(OSM_STREETS_TMPL, { maxZoom: 22, maxNativeZoom: 19, attribution: '© OpenStreetMap' });
        } else {
          basemapLayer = g.L.tileLayer(satUrl, { maxZoom: 22, maxNativeZoom: maxZ, attribution: useScc ? 'City of San Jose / SCC' : 'USGS Imagery' });
        }
        basemapLayer.addTo(mapInst);
        if (typeof basemapLayer.bringToBack === 'function') basemapLayer.bringToBack();
      }

      setMapBase(state.basemap);
      facetLayer = g.L.layerGroup().addTo(mapInst);
      panelLayer = g.L.layerGroup().addTo(mapInst);
      updateMapOverlays(sim);

      if (data.fluxGrid && Array.isArray(data.fluxGrid.boundsLatLon)) {
        try {
          mapInst.fitBounds(data.fluxGrid.boundsLatLon, { padding: [26, 26], maxZoom: 20 });
        } catch (e) {}
      }
      mapEl._setSolarBase = setMapBase;
    }

    function renderCharts(sim) {
      var cashEl = hostEl.querySelector('#oeSolCashChart');
      var monEl = hostEl.querySelector('#oeSolMonthChart');
      var ink = cssVar('--ink', isThemeDark() ? '#f1f5f9' : '#111827');
      var muted = cssVar('--muted', isThemeDark() ? '#94a3b8' : '#6b7280');
      var line = cssVar('--line', isThemeDark() ? '#1e293b' : '#e5e7eb');
      var surface = cssVar('--surface', isThemeDark() ? '#111827' : '#ffffff');
      var brand = cssVar('--brand', '#14b8a6');
      var amber = cssVar('--amber', '#d97706');
      var red = cssVar('--red', '#dc2626');
      var good = cssVar('--good', '#16a34a');

      var p = sim.activePath;
      var yrLabels = [];
      for (var y = 0; y <= 20; y++) yrLabels.push(langCode() === 'zh' ? (y + '年') : ('Yr ' + y));

      if (typeof g.echarts !== 'undefined' && g.echarts && typeof g.echarts.init === 'function') {
        if (cashEl) {
          cashChart = g.echarts.getInstanceByDom(cashEl) || g.echarts.init(cashEl);
          cashChart.setOption({
            backgroundColor: 'transparent',
            tooltip: { trigger: 'axis', backgroundColor: surface, borderColor: line, textStyle: { color: ink, fontSize: 12 } },
            legend: { top: 0, textStyle: { color: muted, fontSize: 11 } },
            grid: { left: 52, right: 16, top: 34, bottom: 26 },
            xAxis: { type: 'category', data: yrLabels, axisLabel: { color: muted, fontSize: 10 }, axisLine: { lineStyle: { color: line } } },
            yAxis: {
              type: 'value',
              axisLabel: { color: muted, fontSize: 10, formatter: function (v) { return '$' + Math.round(v / 1000) + 'k'; } },
              splitLine: { lineStyle: { color: line, type: 'dashed' } }
            },
            series: [
              { name: tr('seriesUtility'), type: 'line', smooth: true, showSymbol: false, lineStyle: { width: 2, color: red, type: 'dashed' }, itemStyle: { color: red }, data: p.cumulative20yrUtilityOutlayUsd },
              { name: tr('seriesSolarCost'), type: 'line', smooth: true, showSymbol: false, lineStyle: { width: 2.5, color: amber }, itemStyle: { color: amber }, data: p.cumulative20yrSolarOutlayUsd },
              { name: tr('seriesNetSavings'), type: 'bar', barMaxWidth: 12, itemStyle: { color: good, borderRadius: [3, 3, 0, 0] }, data: p.cumulative20yrNetSavingsUsd }
            ]
          }, true);
        }
        if (monEl) {
          monthChart = g.echarts.getInstanceByDom(monEl) || g.echarts.init(monEl);
          var mNames = MONTH_LABELS[langCode()] || MONTH_LABELS.en;
          monthChart.setOption({
            backgroundColor: 'transparent',
            tooltip: { trigger: 'axis', backgroundColor: surface, borderColor: line, textStyle: { color: ink, fontSize: 12 } },
            legend: { top: 0, textStyle: { color: muted, fontSize: 11 } },
            grid: { left: 46, right: 14, top: 34, bottom: 26 },
            xAxis: { type: 'category', data: mNames, axisLabel: { color: muted, fontSize: 10 }, axisLine: { lineStyle: { color: line } } },
            yAxis: { type: 'value', axisLabel: { color: muted, fontSize: 10 }, splitLine: { lineStyle: { color: line, type: 'dashed' } } },
            series: [
              { name: tr('seriesGen'), type: 'bar', itemStyle: { color: brand, borderRadius: [3, 3, 0, 0] }, data: sim.monthlyKwh },
              { name: tr('seriesLoad'), type: 'line', smooth: true, lineStyle: { width: 2.2, color: amber }, itemStyle: { color: amber }, data: sim.monthlyUsageKwh }
            ]
          }, true);
        }
      } else {
        // Lightweight non-ECharts SVG fallback for minimal headless environments
        if (cashEl) {
          cashEl.innerHTML = '<div style="padding:14px;font-size:12px;color:var(--ink2);">' +
            esc(tr('seriesNetSavings')) + ' (20 Yr): <b>' + esc(fmtUsd(p.netSavings20yrUsd)) + '</b></div>';
        }
        if (monEl) {
          monEl.innerHTML = '<div style="padding:14px;font-size:12px;color:var(--ink2);">' +
            esc(tr('kpiOutput')) + ': <b>' + fmtNum(sim.annualKwh) + ' kWh/yr</b></div>';
        }
      }
    }

    function renderDom() {
      if (!data) {
        hostEl.innerHTML = '<div class="oe-sol"><div class="oe-sol-alert">' + esc(tr('loading')) + '</div></div>';
        return;
      }
      var sim = computeInteractiveSimulation(data, state);
      var fin = data.financials || {};
      var p = sim.activePath;
      var provLabel = data.lidarMeasured
        ? tr('provLidar')
        : (data.irradianceSource === 'nasa_power_25yr_climatology' ? tr('provFootprint') : tr('provFallback'));

      var suitText = data.hoaRoofCaution
        ? tr('suitHoa')
        : (data.suitabilityBadge === 'optimal' ? tr('suitOptimal') : (data.suitabilityBadge === 'good' ? tr('suitGood') : tr('suitShaded')));
      var suitCls = (data.hoaRoofCaution || data.suitabilityBadge === 'shaded') ? 'watch' : 'good';

      var html = [];
      html.push('<div class="oe-sol" data-provenance="' + esc(data.provenance || '') + '">');

      // Header
      html.push('<div class="oe-sol-hdr">');
      html.push('<div><h2 class="oe-sol-title">' + esc(tr('title')) + '</h2><p class="oe-sol-sub">' + esc(tr('subTitle')) + '</p></div>');
      html.push('<div class="oe-sol-badges">');
      html.push('<span class="oe-sol-pill brand">' + esc(provLabel) + '</span>');
      html.push('<span class="oe-sol-pill ' + suitCls + '">' + esc(suitText) + '</span>');
      if (data.hasExistingSolar) {
        var exLbl = tr('existingSolar') + (isN(data.existingSolarKw) ? (' (' + data.existingSolarKw + ' kW)') : '');
        html.push('<span class="oe-sol-pill good">' + esc(exLbl) + '</span>');
      }
      html.push('</div></div>');

      if (data.hoaRoofCaution && data.hoaNote) {
        html.push('<div class="oe-sol-alert">' + esc(data.hoaNote) + '</div>');
      }
      if (data.provenance === 'irradiance_only_no_footprint') {
        html.push('<div class="oe-sol-alert">' + esc(tr('noFootprint')) + '</div>');
      }

      // KPI Hero Strip
      html.push('<div class="oe-sol-kpis" id="oeSolKpis">');
      html.push('<div class="oe-sol-kpi"><span class="oe-sol-kpi-k">' + esc(tr('kpiSunlight')) + '</span><span class="oe-sol-kpi-v" data-k="sun">' + fmtNum(sim.usableSunHours) + '</span><span class="oe-sol-kpi-s">' + esc(tr('hrsYr')) + ' · ' + fmtNum(data.sunHoursPerDay, 1) + ' hr/day</span></div>');
      html.push('<div class="oe-sol-kpi"><span class="oe-sol-kpi-k">' + esc(tr('kpiRoof')) + '</span><span class="oe-sol-kpi-v" data-k="roof">' + (sim.usableRoofSqft != null ? fmtNum(sim.usableRoofSqft) : '—') + '</span><span class="oe-sol-kpi-s">' + esc(tr('sqft')) + (sim.usableRoofPct != null ? (' · ' + sim.usableRoofPct + '% ' + esc(tr('ofRoof'))) : '') + '</span></div>');
      html.push('<div class="oe-sol-kpi"><span class="oe-sol-kpi-k">' + esc(tr('kpiArray')) + '</span><span class="oe-sol-kpi-v" data-k="kw">' + fmtNum(sim.systemKw, 2) + ' kW</span><span class="oe-sol-kpi-s">' + fmtNum(sim.panelCount) + ' ' + esc(tr('panels')) + ' (' + esc(tr('maxPanels')) + ' ' + fmtNum(sim.maxPanelCount) + ')</span></div>');
      html.push('<div class="oe-sol-kpi"><span class="oe-sol-kpi-k">' + esc(tr('kpiOutput')) + '</span><span class="oe-sol-kpi-v" data-k="kwh">' + fmtNum(sim.annualKwh) + ' kWh</span><span class="oe-sol-kpi-s">' + fmtNum(sim.billOffsetPct, 0) + '% ' + esc(tr('billOffset')) + '</span></div>');
      html.push('<div class="oe-sol-kpi"><span class="oe-sol-kpi-k">' + esc(tr('kpiNet20')) + '</span><span class="oe-sol-kpi-v ' + (p.netSavings20yrUsd >= 0 ? 'good' : '') + '" data-k="net20">' + esc(fmtUsd(p.netSavings20yrUsd)) + '</span><span class="oe-sol-kpi-s">' + esc(tr('paybackYr')) + ': ' + (p.paybackYears === 0 ? esc(tr('immediate')) : (p.paybackYears != null ? p.paybackYears : esc(tr('noPayback')))) + '</span></div>');
      html.push('</div>');

      // Split Map + Configurator
      html.push('<div class="oe-sol-split">');
      html.push('<div class="oe-sol-mapbox">');
      html.push('<div class="oe-sol-mapbar">');
      html.push('<div class="oe-sol-chips">');
      html.push('<button type="button" class="oe-sol-chip ' + (state.showFlux ? 'on' : '') + '" data-layer="flux"><i style="background:#f59e0b"></i>' + esc(tr('mapFluxLayer')) + '</button>');
      html.push('<button type="button" class="oe-sol-chip ' + (state.showPanels ? 'on' : '') + '" data-layer="panels"><i style="background:#38bdf8"></i>' + esc(tr('mapPanelsLayer')) + '</button>');
      html.push('<button type="button" class="oe-sol-chip ' + (state.showFacets ? 'on' : '') + '" data-layer="facets"><i style="background:#2dd4bf"></i>' + esc(tr('mapFacetsLayer')) + '</button>');
      html.push('</div>');
      html.push('<div class="oe-sol-seg" role="group">');
      html.push('<button type="button" class="' + (state.basemap === 'satellite' ? 'on' : '') + '" data-bm="satellite">' + esc(tr('mapSat')) + '</button>');
      html.push('<button type="button" class="' + (state.basemap === 'streets' ? 'on' : '') + '" data-bm="streets">' + esc(tr('mapStreets')) + '</button>');
      html.push('</div></div>');
      html.push('<div class="oe-sol-mapcanvas" id="oeSolMap"></div>');
      html.push('<div class="oe-sol-legend"><span>' + esc(tr('legendLow')) + '</span><div class="oe-sol-grad"></div><span>' + esc(tr('legendPeak')) + ' (' + fmtNum((data.fluxGrid && data.fluxGrid.maxVal) || sim.usableSunHours) + ' kWh/m²/yr)</span></div>');
      html.push('</div>');

      // Right Configurator
      html.push('<div class="oe-sol-cfg">');
      html.push('<div style="display:flex;justify-content:space-between;align-items:center;gap:8px;"><h3 class="oe-sol-cfg-t">' + esc(tr('cfgTitle')) + '</h3><button type="button" class="oe-sol-btn" id="oeSolResetBtn">' + esc(tr('resetBtn')) + '</button></div>');

      html.push('<div class="oe-sol-slider-row">');
      html.push('<div class="oe-sol-slider-hdr"><label for="oeSolBillRange">' + esc(tr('billLabel')) + '</label><span class="oe-sol-slider-val" id="oeSolBillVal">' + esc(fmtUsd(state.monthlyBillUsd)) + '/mo</span></div>');
      html.push('<input type="range" id="oeSolBillRange" min="50" max="800" step="10" value="' + state.monthlyBillUsd + '">');
      html.push('<div class="oe-sol-note" id="oeSolLoadNote">' + esc(tr('impliedUse')) + ': <b>' + fmtNum(sim.annualUsageKwh) + ' kWh/yr</b> @ $' + sim.ratePerKwh.toFixed(3) + '/kWh' + (fin.carrierName ? (' (' + esc(fin.carrierName) + ')') : '') + '</div>');
      html.push('</div>');

      html.push('<div class="oe-sol-toggles">');
      html.push('<label class="oe-sol-chk"><input type="checkbox" id="oeSolEvChk" ' + (state.addEv ? 'checked' : '') + '><span>' + esc(tr('evToggle')) + '</span></label>');
      html.push('<label class="oe-sol-chk"><input type="checkbox" id="oeSolHpChk" ' + (state.addHeatPump ? 'checked' : '') + '><span>' + esc(tr('hpToggle')) + '</span></label>');
      html.push('</div>');

      if (Array.isArray(data.facets) && data.facets.length) {
        html.push('<div><div class="oe-sol-fcard-k" style="margin-bottom:6px;">' + esc(tr('facetsTitle')) + '</div><div class="oe-sol-facets" id="oeSolFacets">');
        data.facets.forEach(function (f) {
          var on = state.enabledFacets[f.id] !== false;
          var suitTag = f.suitability === 'optimal' ? tr('facetOptimal') : (f.suitability === 'good' ? tr('facetGood') : (f.suitability === 'fair' ? tr('facetFair') : tr('facetShaded')));
          html.push('<button type="button" class="oe-sol-facet-btn ' + (on ? 'on' : '') + '" data-fid="' + f.id + '">' +
            '<b>' + esc(f.label) + ' · ' + esc(suitTag) + '</b>' +
            '<span>' + fmtNum(f.usableAreaSqft) + ' ' + esc(tr('sqft')) + ' · ' + fmtNum(f.panelCapacity) + ' ' + esc(tr('panels')) + ' · ' + fmtNum(f.shadingLossPct, 0) + '% ' + esc(tr('shadeLbl')) + '</span>' +
            '</button>');
        });
        html.push('</div></div>');
      }

      html.push('<div class="oe-sol-storage">');
      html.push('<div style="display:flex;justify-content:space-between;align-items:center;gap:6px;flex-wrap:wrap;"><span class="oe-sol-fcard-k">' + esc(tr('storageTitle')) + '</span>' + (sim.isNem3 ? '<span class="oe-sol-pill brand">' + esc(tr('nemBadge')) + '</span>' : '') + '</div>');
      html.push('<div class="oe-sol-seg" id="oeSolBattSeg" role="group">' +
        '<button type="button" class="' + (!state.withBattery ? 'on' : '') + '" data-batt="0">' + esc(tr('solarOnlyBtn')) + ' (' + esc(fmtUsd(sim.solarOnlyYr1)) + '/yr)</button>' +
        '<button type="button" class="' + (state.withBattery ? 'on' : '') + '" data-batt="1">' + esc(tr('solarBattBtn')) + ' (' + esc(fmtUsd(sim.withBattYr1)) + '/yr)</button>' +
        '</div>');
      if (fin.nem3ArbitrageNote) {
        html.push('<div class="oe-sol-note">' + esc(fin.nem3ArbitrageNote) + '</div>');
      }
      html.push('</div>');

      html.push('<div class="oe-sol-adv">');
      html.push('<label class="oe-sol-numbox"><span>' + esc(tr('costPerWatt')) + '</span><input type="number" id="oeSolCpwInp" min="1.8" max="6.0" step="0.05" value="' + state.costPerWatt.toFixed(2) + '"></label>');
      html.push('<label class="oe-sol-numbox"><span>' + esc(tr('incentivePct')) + '</span><input type="number" id="oeSolIncInp" min="0" max="60" step="5" value="' + state.incentivePct + '"></label>');
      html.push('<label class="oe-sol-numbox"><span>' + esc(tr('escPct')) + '</span><input type="number" id="oeSolEscInp" min="0" max="12" step="0.5" value="' + state.escalationPct + '"></label>');
      html.push('</div>');

      html.push('</div></div>'); // end split

      // Finance Tabs + Cashflow & Monthly Charts
      html.push('<div class="oe-sol-fin">');
      html.push('<div class="oe-sol-fin-top">');
      html.push('<div class="oe-sol-seg" id="oeSolFinTabs" role="tablist">');
      html.push('<button type="button" role="tab" class="' + (state.financeTab === 'cash' ? 'on' : '') + '" data-mode="cash">' + esc(tr('finCash')) + '</button>');
      html.push('<button type="button" role="tab" class="' + (state.financeTab === 'loan' ? 'on' : '') + '" data-mode="loan">' + esc(tr('finLoan')) + '</button>');
      html.push('<button type="button" role="tab" class="' + (state.financeTab === 'lease' ? 'on' : '') + '" data-mode="lease">' + esc(tr('finLease')) + '</button>');
      html.push('</div>');
      html.push('<div class="oe-sol-note">' + esc(fin.taxCreditNote || '') + '</div>');
      html.push('</div>');

      html.push('<div class="oe-sol-fin-cards" id="oeSolFinCards">');
      html.push('<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('grossCost')) + '</div><div class="oe-sol-fcard-v">' + esc(fmtUsd(p.grossCostUsd)) + '</div><div class="oe-sol-fcard-s">$' + state.costPerWatt.toFixed(2) + '/W</div></div>');
      html.push('<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('incentives')) + '</div><div class="oe-sol-fcard-v">' + esc(fmtUsd(p.incentiveUsd)) + '</div><div class="oe-sol-fcard-s">' + (state.financeTab === 'lease' ? '30% §48E' : (state.incentivePct + '%')) + '</div></div>');
      html.push('<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(state.financeTab === 'cash' ? tr('netCost') : tr('monthlyPmt')) + '</div><div class="oe-sol-fcard-v">' + esc(state.financeTab === 'cash' ? fmtUsd(p.netCostUsd) : (fmtUsd(p.monthlyPaymentUsd) + '/mo')) + '</div><div class="oe-sol-fcard-s">' + (state.financeTab === 'loan' ? '6.99% APR · $0 down' : (state.financeTab === 'lease' ? '$0 down · 2.9%/yr' : 'Upfront net')) + '</div></div>');
      html.push('<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('yr1Savings')) + '</div><div class="oe-sol-fcard-v" style="color:var(--good);">' + esc(fmtUsd(p.year1BillSavingsUsd)) + '</div><div class="oe-sol-fcard-s">' + esc(tr('yr1Net')) + ': ' + esc(fmtUsd(p.year1NetCashflowUsd)) + '</div></div>');
      html.push('<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('paybackYr')) + '</div><div class="oe-sol-fcard-v">' + (p.paybackYears === 0 ? esc(tr('immediate')) : (p.paybackYears != null ? ('Yr ' + p.paybackYears) : esc(tr('noPayback')))) + '</div><div class="oe-sol-fcard-s">20-yr: ' + esc(fmtUsd(p.netSavings20yrUsd)) + '</div></div>');
      html.push('<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('effRate')) + '</div><div class="oe-sol-fcard-v">$' + p.effectiveSolarKwhUsd.toFixed(3) + '/kWh</div><div class="oe-sol-fcard-s">' + esc(tr('vsUtility')) + ' $' + sim.ratePerKwh.toFixed(3) + '</div></div>');
      html.push('</div>');

      html.push('<div class="oe-sol-charts">');
      html.push('<div class="oe-sol-chartcard"><div class="oe-sol-chart-t">' + esc(tr('chartCashTitle')) + '</div><div class="oe-sol-chartbox" id="oeSolCashChart"></div></div>');
      html.push('<div class="oe-sol-chartcard"><div class="oe-sol-chart-t">' + esc(tr('chartMonthlyTitle')) + '</div><div class="oe-sol-chartbox" id="oeSolMonthChart"></div></div>');
      html.push('</div></div>'); // end fin

      // EPA eGRID Environmental Impact Bar
      html.push('<div class="oe-sol-env" id="oeSolEnv">');
      html.push('<div><div class="oe-sol-fcard-k">' + esc(tr('envTitle')) + '</div><div style="font-size:14px;font-weight:800;color:var(--ink);">' + esc(sim.env.subregion) + ' · ' + esc(sim.env.fullName) + '</div><div class="oe-sol-note">' + fmtNum(sim.env.co2LbPerMwh, 1) + ' lb CO2/MWh</div></div>');
      html.push('<div><div class="oe-sol-fcard-k">' + esc(tr('co2Yr')) + '</div><div class="oe-sol-fcard-v" style="color:var(--good);">' + fmtNum(sim.env.co2Yr, 2) + ' ' + esc(tr('metricTons')) + '</div></div>');
      html.push('<div><div class="oe-sol-fcard-k">' + esc(tr('co2_20')) + '</div><div class="oe-sol-fcard-v">' + fmtNum(sim.env.co2_20, 1) + ' ' + esc(tr('metricTons')) + '</div></div>');
      html.push('<div><div class="oe-sol-fcard-k">' + esc(tr('eqMiles')) + '</div><div class="oe-sol-fcard-v">' + fmtNum(sim.env.miles20) + ' ' + esc(tr('milesUnit')) + '</div></div>');
      html.push('<div><div class="oe-sol-fcard-k">' + esc(tr('eqTrees')) + '</div><div class="oe-sol-fcard-v">' + fmtNum(sim.env.trees10) + ' ' + esc(tr('treesUnit')) + '</div></div>');
      html.push('</div>');

      html.push('<div class="oe-sol-note">' + esc(tr('disclaimer')) + '</div>');
      html.push('</div>');

      teardownMapsAndCharts();
      hostEl.innerHTML = html.join('');
      bindEvents(sim);
      initMap(hostEl.querySelector('#oeSolMap'), sim);
      renderCharts(sim);
    }

    function refreshDynamicViews() {
      if (!data) return;
      var sim = computeInteractiveSimulation(data, state);
      var p = sim.activePath;

      // Update KPIs in-place (preserving map pan/zoom)
      var kpis = hostEl.querySelector('#oeSolKpis');
      if (kpis) {
        var setK = function (k, txt) {
          var el = kpis.querySelector('[data-k="' + k + '"]');
          if (el) el.textContent = txt;
        };
        setK('sun', fmtNum(sim.usableSunHours));
        setK('roof', sim.usableRoofSqft != null ? fmtNum(sim.usableRoofSqft) : '—');
        setK('kw', fmtNum(sim.systemKw, 2) + ' kW');
        setK('kwh', fmtNum(sim.annualKwh) + ' kWh');
        setK('net20', fmtUsd(p.netSavings20yrUsd));
        var kwSub = kpis.querySelector('[data-k="kw"] + .oe-sol-kpi-s');
        if (kwSub) kwSub.textContent = fmtNum(sim.panelCount) + ' ' + tr('panels') + ' (' + tr('maxPanels') + ' ' + fmtNum(sim.maxPanelCount) + ')';
        var kwhSub = kpis.querySelector('[data-k="kwh"] + .oe-sol-kpi-s');
        if (kwhSub) kwhSub.textContent = fmtNum(sim.billOffsetPct, 0) + '% ' + tr('billOffset');
        var netSub = kpis.querySelector('[data-k="net20"] + .oe-sol-kpi-s');
        if (netSub) netSub.textContent = tr('paybackYr') + ': ' + (p.paybackYears === 0 ? tr('immediate') : (p.paybackYears != null ? p.paybackYears : tr('noPayback')));
      }

      var bVal = hostEl.querySelector('#oeSolBillVal');
      if (bVal) bVal.textContent = fmtUsd(state.monthlyBillUsd) + '/mo';
      var lNote = hostEl.querySelector('#oeSolLoadNote');
      if (lNote) {
        var fin = data.financials || {};
        lNote.innerHTML = esc(tr('impliedUse')) + ': <b>' + fmtNum(sim.annualUsageKwh) + ' kWh/yr</b> @ $' + sim.ratePerKwh.toFixed(3) + '/kWh' + (fin.carrierName ? (' (' + esc(fin.carrierName) + ')') : '');
      }

      var fBtns = hostEl.querySelectorAll('#oeSolFacets .oe-sol-facet-btn');
      fBtns.forEach(function (btn) {
        var fid = Number(btn.getAttribute('data-fid'));
        btn.classList.toggle('on', state.enabledFacets[fid] !== false);
      });

      var battSeg = hostEl.querySelectorAll('#oeSolBattSeg button');
      battSeg.forEach(function (btn) {
        var isB = btn.getAttribute('data-batt') === '1';
        btn.classList.toggle('on', isB === Boolean(state.withBattery));
        btn.textContent = (isB ? tr('solarBattBtn') : tr('solarOnlyBtn')) + ' (' + fmtUsd(isB ? sim.withBattYr1 : sim.solarOnlyYr1) + '/yr)';
      });

      var finTabs = hostEl.querySelectorAll('#oeSolFinTabs button');
      finTabs.forEach(function (btn) {
        btn.classList.toggle('on', btn.getAttribute('data-mode') === state.financeTab);
      });

      var finCards = hostEl.querySelector('#oeSolFinCards');
      if (finCards) {
        finCards.innerHTML = [
          '<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('grossCost')) + '</div><div class="oe-sol-fcard-v">' + esc(fmtUsd(p.grossCostUsd)) + '</div><div class="oe-sol-fcard-s">$' + state.costPerWatt.toFixed(2) + '/W</div></div>',
          '<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('incentives')) + '</div><div class="oe-sol-fcard-v">' + esc(fmtUsd(p.incentiveUsd)) + '</div><div class="oe-sol-fcard-s">' + (state.financeTab === 'lease' ? '30% §48E' : (state.incentivePct + '%')) + '</div></div>',
          '<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(state.financeTab === 'cash' ? tr('netCost') : tr('monthlyPmt')) + '</div><div class="oe-sol-fcard-v">' + esc(state.financeTab === 'cash' ? fmtUsd(p.netCostUsd) : (fmtUsd(p.monthlyPaymentUsd) + '/mo')) + '</div><div class="oe-sol-fcard-s">' + (state.financeTab === 'loan' ? '6.99% APR · $0 down' : (state.financeTab === 'lease' ? '$0 down · 2.9%/yr' : 'Upfront net')) + '</div></div>',
          '<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('yr1Savings')) + '</div><div class="oe-sol-fcard-v" style="color:var(--good);">' + esc(fmtUsd(p.year1BillSavingsUsd)) + '</div><div class="oe-sol-fcard-s">' + esc(tr('yr1Net')) + ': ' + esc(fmtUsd(p.year1NetCashflowUsd)) + '</div></div>',
          '<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('paybackYr')) + '</div><div class="oe-sol-fcard-v">' + (p.paybackYears === 0 ? esc(tr('immediate')) : (p.paybackYears != null ? ('Yr ' + p.paybackYears) : esc(tr('noPayback')))) + '</div><div class="oe-sol-fcard-s">20-yr: ' + esc(fmtUsd(p.netSavings20yrUsd)) + '</div></div>',
          '<div class="oe-sol-fcard"><div class="oe-sol-fcard-k">' + esc(tr('effRate')) + '</div><div class="oe-sol-fcard-v">$' + p.effectiveSolarKwhUsd.toFixed(3) + '/kWh</div><div class="oe-sol-fcard-s">' + esc(tr('vsUtility')) + ' $' + sim.ratePerKwh.toFixed(3) + '</div></div>'
        ].join('');
      }

      var envEl = hostEl.querySelector('#oeSolEnv');
      if (envEl) {
        envEl.innerHTML = [
          '<div><div class="oe-sol-fcard-k">' + esc(tr('envTitle')) + '</div><div style="font-size:14px;font-weight:800;color:var(--ink);">' + esc(sim.env.subregion) + ' · ' + esc(sim.env.fullName) + '</div><div class="oe-sol-note">' + fmtNum(sim.env.co2LbPerMwh, 1) + ' lb CO2/MWh</div></div>',
          '<div><div class="oe-sol-fcard-k">' + esc(tr('co2Yr')) + '</div><div class="oe-sol-fcard-v" style="color:var(--good);">' + fmtNum(sim.env.co2Yr, 2) + ' ' + esc(tr('metricTons')) + '</div></div>',
          '<div><div class="oe-sol-fcard-k">' + esc(tr('co2_20')) + '</div><div class="oe-sol-fcard-v">' + fmtNum(sim.env.co2_20, 1) + ' ' + esc(tr('metricTons')) + '</div></div>',
          '<div><div class="oe-sol-fcard-k">' + esc(tr('eqMiles')) + '</div><div class="oe-sol-fcard-v">' + fmtNum(sim.env.miles20) + ' ' + esc(tr('milesUnit')) + '</div></div>',
          '<div><div class="oe-sol-fcard-k">' + esc(tr('eqTrees')) + '</div><div class="oe-sol-fcard-v">' + fmtNum(sim.env.trees10) + ' ' + esc(tr('treesUnit')) + '</div></div>'
        ].join('');
      }

      updateMapOverlays(sim);
      renderCharts(sim);

      try {
        g.dispatchEvent(new CustomEvent('openestate:solar-update', { detail: { state: state, sim: sim } }));
      } catch (e) {}
    }

    function bindEvents() {
      var billRange = hostEl.querySelector('#oeSolBillRange');
      if (billRange) {
        billRange.addEventListener('input', function () {
          state.monthlyBillUsd = Number(billRange.value) || 220;
          refreshDynamicViews();
        });
      }
      var evChk = hostEl.querySelector('#oeSolEvChk');
      if (evChk) {
        evChk.addEventListener('change', function () {
          state.addEv = Boolean(evChk.checked);
          refreshDynamicViews();
        });
      }
      var hpChk = hostEl.querySelector('#oeSolHpChk');
      if (hpChk) {
        hpChk.addEventListener('change', function () {
          state.addHeatPump = Boolean(hpChk.checked);
          refreshDynamicViews();
        });
      }
      hostEl.querySelectorAll('#oeSolFacets .oe-sol-facet-btn').forEach(function (btn) {
        btn.addEventListener('click', function () {
          var fid = Number(btn.getAttribute('data-fid'));
          state.enabledFacets[fid] = !(state.enabledFacets[fid] !== false);
          refreshDynamicViews();
        });
      });
      hostEl.querySelectorAll('#oeSolBattSeg button').forEach(function (btn) {
        btn.addEventListener('click', function () {
          state.withBattery = btn.getAttribute('data-batt') === '1';
          refreshDynamicViews();
        });
      });
      hostEl.querySelectorAll('#oeSolFinTabs button').forEach(function (btn) {
        btn.addEventListener('click', function () {
          state.financeTab = btn.getAttribute('data-mode') || 'cash';
          refreshDynamicViews();
        });
      });
      var cpwInp = hostEl.querySelector('#oeSolCpwInp');
      if (cpwInp) {
        cpwInp.addEventListener('change', function () {
          state.costPerWatt = Number(cpwInp.value) || 2.95;
          refreshDynamicViews();
        });
      }
      var incInp = hostEl.querySelector('#oeSolIncInp');
      if (incInp) {
        incInp.addEventListener('change', function () {
          state.incentivePct = Number(incInp.value) || 0;
          refreshDynamicViews();
        });
      }
      var escInp = hostEl.querySelector('#oeSolEscInp');
      if (escInp) {
        escInp.addEventListener('change', function () {
          state.escalationPct = Number(escInp.value) || 3.5;
          refreshDynamicViews();
        });
      }
      var resetBtn = hostEl.querySelector('#oeSolResetBtn');
      if (resetBtn) {
        resetBtn.addEventListener('click', function () {
          state.addEv = false;
          state.addHeatPump = false;
          seedStateFromData(data);
          renderDom();
        });
      }
      hostEl.querySelectorAll('.oe-sol-chip[data-layer]').forEach(function (ch) {
        ch.addEventListener('click', function () {
          var lyr = ch.getAttribute('data-layer');
          if (lyr === 'flux') state.showFlux = !state.showFlux;
          if (lyr === 'panels') state.showPanels = !state.showPanels;
          if (lyr === 'facets') state.showFacets = !state.showFacets;
          ch.classList.toggle('on', lyr === 'flux' ? state.showFlux : lyr === 'panels' ? state.showPanels : state.showFacets);
          updateMapOverlays(computeInteractiveSimulation(data, state));
        });
      });
      hostEl.querySelectorAll('.oe-sol-mapbar button[data-bm]').forEach(function (btn) {
        btn.addEventListener('click', function () {
          var next = btn.getAttribute('data-bm') || 'satellite';
          hostEl.querySelectorAll('.oe-sol-mapbar button[data-bm]').forEach(function (b) {
            b.classList.toggle('on', b === btn);
          });
          var mEl = hostEl.querySelector('#oeSolMap');
          if (mEl && typeof mEl._setSolarBase === 'function') mEl._setSolarBase(next);
        });
      });
    }

    function setData(nextData) {
      data = nextData;
      seedStateFromData(data);
      renderDom();
    }

    function fetchForSlug(slug) {
      if (!slug) return;
      if (pollTimer) { clearTimeout(pollTimer); pollTimer = null; }
      fetch('/api/solar/' + encodeURIComponent(slug), { credentials: 'same-origin' })
        .then(function (r) { return r.json(); })
        .then(function (payload) {
          if (payload && payload.solar) {
            setData(payload.solar);
          }
          if (payload && payload.pending) {
            pollTimer = setTimeout(function () { fetchForSlug(slug); }, payload.retryAfterMs || 2500);
          }
        })
        .catch(function () {});
    }

    if (data) {
      seedStateFromData(data);
    }
    renderDom();
    var repForUpgrade = (core() && typeof core().getRep === 'function') ? core().getRep() : (g.R || null);
    var upgradeToLidar = Boolean(
      data &&
      !data.lidarMeasured &&
      opts.slug &&
      !(repForUpgrade && repForUpgrade.house3dDisabled) &&
      typeof location !== 'undefined' &&
      /^https?:$/.test(location.protocol || '')
    );
    if ((!data || upgradeToLidar) && opts.slug) {
      fetchForSlug(opts.slug);
    }

    // Live theme & language reactivity
    var onTheme = function () {
      if (data) renderCharts(computeInteractiveSimulation(data, state));
    };
    g.addEventListener('openestate:themechange', onTheme);
    if (core() && typeof core().onSystemColorSchemeChange === 'function') {
      core().onSystemColorSchemeChange(onTheme);
    }
    if (typeof MutationObserver !== 'undefined') {
      var mo = new MutationObserver(function (muts) {
        var langChanged = muts.some(function (m) { return m.attributeName === 'lang'; });
        var themeChanged = muts.some(function (m) { return m.attributeName === 'data-theme'; });
        if (langChanged) renderDom();
        else if (themeChanged) onTheme();
      });
      mo.observe(document.documentElement, { attributes: true, attributeFilter: ['lang', 'data-theme'] });
    }

    return {
      setData: setData,
      refresh: refreshDynamicViews,
      rerender: renderDom,
      fetchForSlug: fetchForSlug,
      getState: function () { return state; },
      getData: function () { return data; }
    };
  }

  function autoMount() {
    var sec = document.getElementById('solar');
    if (!sec) return null;
    var rep = (core() && typeof core().getRep === 'function') ? core().getRep() : (g.R || null);
    if (rep && (rep.solarPotentialDisabled === true || (rep.modules && rep.modules.solar && rep.modules.solar.disabled === true))) {
      sec.hidden = true;
      sec.innerHTML = '';
      sec._oeSolarCtrl = null;
      return null;
    }
    sec.hidden = false;
    if (sec._oeSolarCtrl) return sec._oeSolarCtrl;
    var initial = rep && (rep.solarPotential || (rep.modules && rep.modules.solar && rep.modules.solar.data) || (rep.modules && rep.modules.solarPotential && rep.modules.solarPotential.data));
    var slug = rep && rep.meta && rep.meta.slug;
    var ctrl = createWidgetController(sec, initial || null, { slug: slug });
    sec._oeSolarCtrl = ctrl;
    return ctrl;
  }

  if (typeof document !== 'undefined') {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', autoMount);
    } else {
      autoMount();
    }
  }

  function mountTarget(targetEl, data, opts) {
    if (!targetEl) return autoMount();
    var el = typeof targetEl === 'string' ? document.querySelector(targetEl) : targetEl;
    if (!el) return null;
    if (el._oeSolarCtrl && !data) return el._oeSolarCtrl;
    var ctrl = createWidgetController(el, data || null, opts || {});
    el._oeSolarCtrl = ctrl;
    return ctrl;
  }

  g.OpenEstateSolar = {
    __v: 1,
    render: mountTarget,
    mount: mountTarget,
    autoMount: autoMount,
    simulate: computeInteractiveSimulation,
    renderFluxCanvas: renderRotatedFluxDataUrl
  };
  g.OpenEstateSolarWidget = g.OpenEstateSolar;
})(typeof window !== 'undefined' ? window : globalThis);
