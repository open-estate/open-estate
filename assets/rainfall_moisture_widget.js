/* Open Estate Rainfall Climatology, Design Storms & USDA Soil Moisture / Shrink-Swell Widget.
 *
 * Standalone self-contained enhancer for `<section id="risk" class="sec">`:
 * 1. Registers 4 toggleable map overlay chips on `#hzMap` (`#hzChips` + `#mapExpandModal`):
 *    - `72h observed rain (NOAA QPE)` (`obs/rfc_qpe/MapServer` layer 37)
 *    - `7-day rain forecast (WPC QPF)` (`precip/wpc_qpf/MapServer` layer 11)
 *    - `Soil saturation (NOAA NWM)` (`obs/NWM_Land_Analysis/MapServer` layer 0)
 *    - `USDA SSURGO soil units` (UC Davis / USDA-NRCS SoilWeb SSURGO tile layer)
 * 2. Renders a 2-column Rainfall/Design-Storms + Soil-Drainage/Shrink-Swell/Water-Table/Drought
 *    sub-panel inside `#risk`, including a live theme-reactive 12-month dual-axis ECharts
 *    Monthly Normal Rainfall + NASA Soil Wetness chart.
 * 3. Strictly uses report CSS custom properties (`var(--surface)`, `var(--soft)`, `var(--ink)`,
 *    `var(--callz-*)`, etc.), EN/中文/ES i18n, `window.OpenEstateCore?.isDisplayLinksEnabled?.()`
 *    reference link gating, and strict data honesty (never invents missing numbers).
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_RAINFALL_MOISTURE_INIT__) return;
  window.__OPEN_ESTATE_RAINFALL_MOISTURE_INIT__ = true;

  var PANEL_ID = 'oeRainSoilPanel';
  var CHART_ID = 'oeRainWetnessChart';
  var STYLE_ID = 'oeRainSoilCss';

  var st = {
    chipFlags: {
      'noaa-qpe-72h': false,
      'wpc-qpf-7d': false,
      'nwm-soil-sat': false,
      'usda-ssurgo': false
    },
    activeMapLayers: Object.create(null),
    boundMapInstance: null,
    chartInstance: null,
    rendering: false,
    riskObserver: null,
    hzMapObserver: null
  };

  function isNum(x) {
    return typeof x === 'number' && isFinite(x);
  }

  function curLang() {
    try {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.langCode === 'function') {
        return window.OpenEstateCore.langCode();
      }
    } catch (e) {}
    try {
      if (typeof LANG === 'string' && LANG) {
        return LANG === 'zh' ? 'zh' : LANG === 'es' ? 'es' : 'en';
      }
    } catch (e2) {}
    var l = String((document.documentElement && document.documentElement.lang) || 'en').toLowerCase();
    return l.indexOf('zh') === 0 ? 'zh' : l.indexOf('es') === 0 ? 'es' : 'en';
  }

  function P(en, zh, es) {
    var l = curLang();
    return l === 'zh' ? (zh || en) : l === 'es' ? (es || en) : en;
  }

  function escH(s) {
    try {
      if (typeof esc === 'function') return esc(s);
    } catch (e) {}
    if (window.OpenEstateCore && typeof window.OpenEstateCore.esc === 'function') {
      return window.OpenEstateCore.esc(s);
    }
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  function getReport() {
    try {
      if (typeof R !== 'undefined' && R && typeof R === 'object') return R;
    } catch (e) {}
    if (window.OpenEstateCore && typeof window.OpenEstateCore.getRep === 'function') {
      return window.OpenEstateCore.getRep();
    }
    try {
      var el = document.getElementById('report-data');
      var txt = el && el.textContent;
      return txt ? JSON.parse(txt) : null;
    } catch (e2) {
      return null;
    }
  }

  function getHazardsData(rep) {
    try {
      if (typeof D === 'function') {
        var d = D('hazards');
        if (d && typeof d === 'object') return d;
      }
    } catch (e) {}
    if (window.OpenEstateCore && typeof window.OpenEstateCore.mod === 'function') {
      var m = window.OpenEstateCore.mod(rep, 'hazards');
      if (m && typeof m === 'object') return m;
    }
    var modObj = rep && rep.modules && rep.modules.hazards;
    return (modObj && modObj.data) || (rep && rep.hazards) || null;
  }

  function areRefLinksVisible() {
    try {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.isDisplayLinksEnabled === 'function') {
        return Boolean(window.OpenEstateCore.isDisplayLinksEnabled());
      }
      if (typeof window.isDisplayLinksEnabled === 'function') {
        return Boolean(window.isDisplayLinksEnabled());
      }
      if (document.documentElement && document.documentElement.dataset) {
        if (document.documentElement.dataset.displayLinks === '1') return true;
        if (document.documentElement.dataset.displayLinks === '0') return false;
      }
      if (typeof document.cookie === 'string' && document.cookie) {
        return /(?:^|;\s*)display_link=1(?:;|$)/.test(document.cookie);
      }
    } catch (e) {}
    return false;
  }

  function renderRefLink(url, name, label, extId) {
    if (!areRefLinksVisible() || !url) return '';
    try {
      if (typeof factSourceLink === 'function') {
        return factSourceLink(
          { url: url, name: name, extensionId: extId },
          label || '↗',
          name,
          { section: 'risk', dataId: extId }
        );
      }
    } catch (e) {}
    var href = url;
    try {
      if (typeof window.toReferenceRedirectUrl === 'function') {
        href = window.toReferenceRedirectUrl(url, {
          dataId: extId,
          section: 'risk',
          name: name
        }) || url;
      }
    } catch (e2) {}
    return '<a class="src-lnk" href="' + escH(href) + '" data-direct-url="' + escH(url) +
      '" target="_blank" rel="noopener noreferrer" title="' + escH(name || '') + '">' +
      escH(label || '↗') + '</a>';
  }

  function cssVar(name, fallback) {
    try {
      var v = getComputedStyle(document.documentElement).getPropertyValue(name);
      if (v && v.trim()) return v.trim();
    } catch (e) {}
    return fallback;
  }

  function injectCss() {
    if (document.getElementById(STYLE_ID)) return;
    var css = [
      '#' + PANEL_ID + '{margin:14px 0 12px}',
      '#' + PANEL_ID + ' .rm-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:14px;align-items:start}',
      '@media(max-width:920px){#' + PANEL_ID + ' .rm-grid{grid-template-columns:1fr}}',
      '#' + PANEL_ID + ' .rm-col{background:var(--surface-2,var(--soft));border:1px solid var(--line);border-radius:14px;padding:13px 14px;min-width:0}',
      '#' + PANEL_ID + ' .rm-col-hdr{display:flex;align-items:baseline;justify-content:space-between;gap:8px;flex-wrap:wrap;margin-bottom:9px}',
      '#' + PANEL_ID + ' .rm-col-title{font-size:13.5px;font-weight:800;color:var(--ink);display:inline-flex;align-items:center;gap:6px}',
      '#' + PANEL_ID + ' .rm-col-sub{font-size:11.5px;color:var(--muted);font-weight:600}',
      '#' + PANEL_ID + ' .rm-kpis{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:8px;margin-bottom:10px}',
      '@media(max-width:520px){#' + PANEL_ID + ' .rm-kpis{grid-template-columns:1fr}}',
      '#' + PANEL_ID + ' .rm-kpi{background:var(--surface);border:1px solid var(--line);border-radius:11px;padding:9px 11px;min-width:0;display:flex;flex-direction:column;gap:2px}',
      '#' + PANEL_ID + ' .rm-kpi .rk{font-size:11px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.35px;display:flex;align-items:center;justify-content:space-between;gap:4px}',
      '#' + PANEL_ID + ' .rm-kpi .rv{font-size:16.5px;font-weight:800;color:var(--ink);letter-spacing:-.25px;line-height:1.25;font-variant-numeric:tabular-nums;display:flex;align-items:center;gap:6px;flex-wrap:wrap}',
      '#' + PANEL_ID + ' .rm-kpi .rd{font-size:11.5px;color:var(--ink2);line-height:1.35}',
      '#' + PANEL_ID + ' .rm-chart-lbl{font-size:11.8px;font-weight:700;color:var(--ink2);margin:6px 0 4px;display:flex;align-items:center;justify-content:space-between;gap:6px;flex-wrap:wrap}',
      '#' + PANEL_ID + ' #' + CHART_ID + '{width:100%;height:198px}',
      '#' + PANEL_ID + ' .rm-callout{margin-top:9px;padding:10px 12px;border-radius:11px;background:var(--callz-bg);border:1px solid var(--callz-bd);color:var(--callz-fg);font-size:12.3px;line-height:1.48}',
      '#' + PANEL_ID + ' .rm-callout b{font-weight:800;color:var(--ink)}',
      '#' + PANEL_ID + ' .rm-callout ul{margin:5px 0 0;padding-left:17px}',
      '#' + PANEL_ID + ' .rm-callout li{margin:3px 0}',
      '#hzChips .oe-rm-chip{cursor:pointer;user-select:none}'
    ].join('\n');
    var style = document.createElement('style');
    style.id = STYLE_ID;
    style.textContent = css;
    (document.head || document.documentElement).appendChild(style);
  }

  function makeArcgisExportLayer(url, layers, opts) {
    try {
      if (typeof arcgisExport === 'function') {
        return arcgisExport(url, layers, opts);
      }
    } catch (e) {}
    var hiDpi = typeof window !== 'undefined' && (window.devicePixelRatio || 1) > 1;
    var px = hiDpi ? 512 : 256;
    var dpi = hiDpi ? 192 : 96;
    var Layer = window.L.TileLayer.extend({
      getTileUrl: function (c) {
        var z = this._getZoomForUrl();
        var n = Math.pow(2, z);
        var o = Math.PI * 6378137;
        var res = (2 * o) / (256 * n);
        var x0 = -o + c.x * 256 * res;
        var y1 = o - c.y * 256 * res;
        return url + '/export?bbox=' + x0 + ',' + (y1 - 256 * res) + ',' + (x0 + 256 * res) + ',' + y1 +
          '&bboxSR=3857&imageSR=3857&size=' + px + ',' + px + '&dpi=' + dpi +
          '&format=png32&transparent=true&layers=show:' + layers + '&f=image';
      }
    });
    return new Layer('', Object.assign({ opacity: 0.5, tileSize: 256, maxZoom: 21, maxNativeZoom: 12 }, opts || {}));
  }

  function buildHzMapOverlayDefs(hasRain, hasSoil) {
    var defs = [];
    if (hasRain) {
      defs.push(
        {
          key: 'noaa-qpe-72h',
          label: P('72h observed rain (NOAA QPE)', '近72小时实测降雨 (NOAA QPE)', 'Lluvia observada 72h (NOAA QPE)'),
          color: 'var(--blue)',
          on: Boolean(st.chipFlags['noaa-qpe-72h']),
          make: function () {
            if (typeof window.L === 'undefined') return null;
            return makeArcgisExportLayer(
              'https://mapservices.weather.noaa.gov/raster/rest/services/obs/rfc_qpe/MapServer',
              '37',
              { opacity: 0.5, maxNativeZoom: 12, attribution: 'NOAA RFC Multi-Sensor QPE' }
            );
          }
        },
        {
          key: 'wpc-qpf-7d',
          label: P('7-day rain forecast (WPC QPF)', '未来7天降雨预报 (WPC QPF)', 'Pronóstico lluvia 7 días (WPC QPF)'),
          color: 'var(--violet)',
          on: Boolean(st.chipFlags['wpc-qpf-7d']),
          make: function () {
            if (typeof window.L === 'undefined') return null;
            return makeArcgisExportLayer(
              'https://mapservices.weather.noaa.gov/vector/rest/services/precip/wpc_qpf/MapServer',
              '11',
              { opacity: 0.48, maxNativeZoom: 12, attribution: 'NOAA WPC 7-Day QPF' }
            );
          }
        }
      );
    }
    if (hasSoil) {
      defs.push(
        {
          key: 'nwm-soil-sat',
          label: P('Soil saturation (NOAA NWM)', '土壤含水饱和度 (NOAA NWM)', 'Saturación del suelo (NOAA NWM)'),
          color: 'var(--brand)',
          on: Boolean(st.chipFlags['nwm-soil-sat']),
          make: function () {
            if (typeof window.L === 'undefined') return null;
            return makeArcgisExportLayer(
              'https://mapservices.weather.noaa.gov/raster/rest/services/obs/NWM_Land_Analysis/MapServer',
              '0',
              { opacity: 0.45, maxNativeZoom: 12, attribution: 'NOAA National Water Model' }
            );
          }
        },
        {
          key: 'usda-ssurgo',
          label: P('USDA SSURGO soil units', 'USDA SSURGO 土壤分区图', 'Unidades de suelo USDA SSURGO'),
          color: 'var(--amber)',
          on: Boolean(st.chipFlags['usda-ssurgo']),
          make: function () {
            if (typeof window.L === 'undefined') return null;
            return window.L.tileLayer(
              'https://casoilresource.lawr.ucdavis.edu/cgi-bin/mapserv?map=gmap-wms&layers=ssurgo&layers=ssa&mode=tile&tilemode=gmap&tile={x}+{y}+{z}&theme=dark&label=musym',
              {
                opacity: 0.65,
                maxZoom: 19,
                maxNativeZoom: 16,
                attribution: 'USDA-NRCS / UC Davis SoilWeb SSURGO'
              }
            );
          }
        }
      );
    }
    return defs;
  }

  function syncLayersToLeafletMap(defs) {
    var mapEl = document.getElementById('hzMap');
    var map = mapEl && mapEl._map ? mapEl._map : null;
    if (!map) return;

    if (st.boundMapInstance !== map) {
      st.activeMapLayers = Object.create(null);
      st.boundMapInstance = map;
    }

    for (var i = 0; i < defs.length; i++) {
      var d = defs[i];
      var wantOn = Boolean(st.chipFlags[d.key]);
      var existing = st.activeMapLayers[d.key];
      if (wantOn) {
        if (!existing) {
          existing = d.make();
          st.activeMapLayers[d.key] = existing;
        }
        if (existing && !map.hasLayer(existing)) {
          existing.addTo(map);
        }
      } else if (existing && map.hasLayer(existing)) {
        map.removeLayer(existing);
      }
    }
  }

  function watchHzMapLazyInit(defs) {
    var mapEl = document.getElementById('hzMap');
    if (!mapEl) return;
    if (mapEl._map) {
      syncLayersToLeafletMap(defs);
      return;
    }
    if (st.hzMapObserver) {
      try { st.hzMapObserver.disconnect(); } catch (e) {}
      st.hzMapObserver = null;
    }
    if (typeof MutationObserver !== 'undefined') {
      st.hzMapObserver = new MutationObserver(function () {
        if (mapEl._map) {
          if (st.hzMapObserver) {
            try { st.hzMapObserver.disconnect(); } catch (e2) {}
            st.hzMapObserver = null;
          }
          syncLayersToLeafletMap(defs);
        }
      });
      st.hzMapObserver.observe(mapEl, { childList: true });
    }
  }

  function mountHzMapChips(hasRain, hasSoil) {
    var chipsHost = document.getElementById('hzChips');
    if (!chipsHost) return;
    var defs = buildHzMapOverlayDefs(hasRain, hasSoil);

    var oldChips = chipsHost.querySelectorAll('.oe-rm-chip');
    for (var k = 0; k < oldChips.length; k++) {
      oldChips[k].remove();
    }
    if (!defs.length) return;

    defs.forEach(function (def) {
      var chip = document.createElement('span');
      chip.className = 'chip oe-rm-chip' + (st.chipFlags[def.key] ? ' on' : '');
      chip.setAttribute('data-rm-key', def.key);
      var dot = document.createElement('i');
      dot.style.background = def.color;
      chip.appendChild(dot);
      chip.appendChild(document.createTextNode(def.label));
      chip.addEventListener('click', function (ev) {
        ev.stopPropagation();
        st.chipFlags[def.key] = !st.chipFlags[def.key];
        chip.classList.toggle('on', Boolean(st.chipFlags[def.key]));
        syncLayersToLeafletMap(defs);
      });
      chipsHost.appendChild(chip);
    });

    watchHzMapLazyInit(defs);
  }

  function hookMapExpandModal() {
    if (window.__OE_RM_EXPAND_HOOKED__) return;
    if (!window.OpenEstateMapExpand || typeof window.OpenEstateMapExpand.register !== 'function') return;
    window.__OE_RM_EXPAND_HOOKED__ = true;
    var origRegister = window.OpenEstateMapExpand.register.bind(window.OpenEstateMapExpand);
    window.OpenEstateMapExpand.register = function (id, spec) {
      if (id === 'hzMap' && spec && Array.isArray(spec.layers)) {
        var rep = getReport();
        var hz = getHazardsData(rep);
        var allowRain = Boolean(hz && !(rep && rep.rainfallClimatologyDisabled));
        var allowSoil = Boolean(hz && !(rep && rep.usdaSoilMoistureDisabled));
        var extraDefs = buildHzMapOverlayDefs(allowRain, allowSoil);
        extraDefs.forEach(function (d) {
          var exists = spec.layers.some(function (x) { return x && x.key === d.key; });
          if (!exists) {
            spec.layers.push({
              key: d.key,
              label: d.label,
              color: d.color,
              on: Boolean(st.chipFlags[d.key]),
              make: d.make
            });
          }
        });
      }
      return origRegister(id, spec);
    };
  }

  function fmtIn(v, digits) {
    if (!isNum(v)) return '—';
    return v.toFixed(digits != null ? digits : 2) + '"';
  }

  function shrinkSwellPillClass(risk) {
    var r = String(risk || '').toLowerCase();
    if (r === 'very high' || r === 'high') return 'bad';
    if (r === 'moderate') return 'watch';
    if (r === 'low') return 'good';
    return 'mute';
  }

  function localizeShrinkSwellRisk(risk) {
    var r = String(risk || '').toLowerCase();
    if (r === 'very high') return P('Very High', '极高膨胀性', 'Muy alto');
    if (r === 'high') return P('High', '高膨胀性', 'Alto');
    if (r === 'moderate') return P('Moderate', '中等膨胀性', 'Moderado');
    if (r === 'low') return P('Low', '低膨胀性', 'Bajo');
    return risk || '—';
  }

  function localizeSuitability(val) {
    var s = String(val || '').trim();
    var low = s.toLowerCase();
    if (low === 'not limited') return P('Not limited', '无限制 (适宜)', 'Sin limitación');
    if (low === 'somewhat limited') return P('Somewhat limited', '轻度受限', 'Algo limitado');
    if (low === 'very limited') return P('Very limited', '明显受限', 'Muy limitado');
    if (low === 'severely limited') return P('Severely limited', '严重受限', 'Severamente limitado');
    return s;
  }

  function buildDiligenceItems(rain, soil) {
    var items = [];
    var lep = soil && isNum(soil.maxLepPct) ? soil.maxLepPct : null;
    var swing = soil && soil.nasaWetness && isNum(soil.nasaWetness.seasonalSwing) ? soil.nasaWetness.seasonalSwing : null;
    var wtCm = soil && isNum(soil.minWaterTableCm) ? soil.minWaterTableCm : null;
    var infil = String((soil && soil.infiltrationSuitability) || '').toLowerCase();
    var ksatIn = soil && isNum(soil.limitingKsatInHr) ? soil.limitingKsatInHr : null;
    var corCon = String((soil && soil.concreteCorrosion) || '').toLowerCase();
    var corStl = String((soil && soil.steelCorrosion) || '').toLowerCase();
    var hr1Yr10 = rain && rain.atlas14 && isNum(rain.atlas14.hr1Yr10In) ? rain.atlas14.hr1Yr10In : null;
    var hr24Yr100 = rain && rain.atlas14 && isNum(rain.atlas14.hr24Yr100In) ? rain.atlas14.hr24Yr100In : null;

    if ((lep != null && lep >= 3.0) || (swing != null && swing >= 0.45)) {
      var why = [];
      if (lep != null) why.push('LEP ' + lep.toFixed(1) + '%');
      if (swing != null) why.push('Δ ' + Math.round(swing * 100) + '% ' + P('wet/dry swing', '干湿季节差', 'oscilación estacional'));
      items.push(P(
        'Perimeter shrink-swell control (' + why.join(' · ') + '): keep soil moisture even around footings, extend downspouts 4–6 ft away from the foundation, and inspect stucco/drywall corners for seasonal movement.',
        '地基周边干湿胀缩防护（' + why.join(' · ') + '）：保持地基四周土壤湿度均衡，落水管排水口延伸至距墙基 4–6 英尺外，并在验房时核查外墙灰泥与室内门窗角斜向裂纹。',
        'Control de expansión/contracción perimetral (' + why.join(' · ') + '): mantenga humedad uniforme junto a cimientos, extienda bajantes 4–6 pies y revise grietas diagonales.'
      ));
    }

    if (wtCm != null && wtCm < 150) {
      var wtFt = (wtCm / 30.48).toFixed(1);
      items.push(P(
        'Shallow seasonal high water table (' + Math.round(wtCm) + ' cm / ' + wtFt + ' ft): verify crawlspace/basement vapor barrier, sump pump + battery backup, and sub-slab capillary break.',
        '季节性浅层地下水位（' + Math.round(wtCm) + ' 厘米 / ' + wtFt + ' 英尺）：重点核查爬行空间或地下室防潮膜、集水坑排水泵（含备用电源）及墙角返潮痕迹。',
        'Nivel freático estacional somero (' + Math.round(wtCm) + ' cm / ' + wtFt + ' pies): verifique barrera de vapor, bomba de achique con respaldo y drenaje perimetral.'
      ));
    }

    if (infil.indexOf('severely limited') >= 0 || infil.indexOf('very limited') >= 0 || (ksatIn != null && ksatIn < 0.35)) {
      var kTxt = ksatIn != null ? (ksatIn.toFixed(2) + ' in/hr') : localizeSuitability(soil.infiltrationSuitability);
      items.push(P(
        'Slow subsoil percolation (' + kTxt + '): unlined dry wells or passive French drains can saturate in winter storms — route roof & patio drains via solid tightline pipe to street/storm drain.',
        '底土下渗受限（' + kTxt + '）：冬季强降雨期无衬砌渗井或盲沟易饱和积水，建议屋面与庭院雨水采用硬管直排市政雨水口或街边排水沟。',
        'Percolación lenta del subsuelo (' + kTxt + '): los pozos secos pueden saturarse; conduzca el agua pluvial por tubería cerrada hacia el drenaje pluvial.'
      ));
    }

    if (corCon === 'high' || corStl === 'high') {
      items.push(P(
        'Soil corrosivity (' + [corCon === 'high' ? 'high concrete' : '', corStl === 'high' ? 'high uncoated steel' : ''].filter(Boolean).join(' / ') + '): inspect foundation concrete spalling, anchor bolts, and buried metallic water/gas lines.',
        '土壤腐蚀性偏高：验房时留意地基混凝土剥落、锚栓锈蚀及地下金属给水/燃气管线防护。',
        'Corrosividad del suelo alta: revise desconchados en concreto, pernos de anclaje y tuberías metálicas enterradas.'
      ));
    }

    if (!items.length && (hr1Yr10 != null || hr24Yr100 != null)) {
      items.push(P(
        'Routine storm-drainage check: size gutters/downspouts for ' + (hr1Yr10 != null ? hr1Yr10.toFixed(2) + '"/hr (10-yr 1-hr)' : 'local design storms') +
          (hr24Yr100 != null ? ' and verify positive 5% yard grading for a ' + hr24Yr100.toFixed(2) + '" 100-yr 24-hr storm.' : '.'),
        '常规防洪排水核查：建议按 ' + (hr1Yr10 != null ? hr1Yr10.toFixed(2) + ' 英寸/小时（10年一遇1小时暴雨）' : '当地暴雨标准') +
          ' 核查天沟落水管通畅度' + (hr24Yr100 != null ? '，并确认庭院具备向外排水坡度（100年一遇24小时雨量 ' + hr24Yr100.toFixed(2) + ' 英寸）。' : '。'),
        'Revisión de drenaje pluvial: verifique canaletas para ' + (hr1Yr10 != null ? hr1Yr10.toFixed(2) + '"/h (10 años)' : 'tormentas de diseño') +
          (hr24Yr100 != null ? ' y pendiente positiva del terreno (' + hr24Yr100.toFixed(2) + '" en 24h a 100 años).' : '.')
      ));
    }
    return items;
  }

  function renderRainColumn(rain) {
    var a14 = (rain && rain.atlas14) || {};
    var hmax = (rain && rain.historicalMax) || {};
    var qpe = (rain && rain.observedQpe) || {};
    var ffg = (rain && rain.flashFloodGuidance) || {};

    var refHtml = renderRefLink(
      (rain && rain.referenceUrl) || 'https://hdsc.nws.noaa.gov/pfds/',
      'NOAA Atlas 14 PFDS / RCC-ACIS / NASA POWER',
      'NOAA / NASA ↗',
      'rainfall_climatology'
    );

    var annVal = isNum(rain.annualNormalIn) ? (rain.annualNormalIn.toFixed(1) + ' in/yr') : '—';
    var annSub = [];
    if (isNum(rain.wetDaysPerYr)) annSub.push(Math.round(rain.wetDaysPerYr) + ' ' + P('wet days/yr', '雨日/年', 'días lluvia/año'));
    if (isNum(rain.heavyRainDaysGe1In)) annSub.push(rain.heavyRainDaysGe1In.toFixed(1) + 'd ≥1"');
    if (!annSub.length && isNum(rain.stationAnnualNormalIn)) {
      annSub.push(P('Station normal ', '气象站常年 ', 'Normal estación ') + rain.stationAnnualNormalIn.toFixed(1) + '"');
    }

    var qpe72 = isNum(qpe.h72In) ? fmtIn(qpe.h72In, 2) : null;
    var qpf7 = isNum(rain.forecastQpf7dIn) ? fmtIn(rain.forecastQpf7dIn, 2) : null;
    var liveVal = [
      qpe72 != null ? (qpe72 + ' ' + P('72h', '近72h', '72h')) : null,
      qpf7 != null ? (qpf7 + ' ' + P('7d fcst', '未来7d', 'pron. 7d')) : null
    ].filter(Boolean).join(' · ') || '—';
    var liveSub = [];
    if (isNum(qpe.h24In)) liveSub.push('24h ' + fmtIn(qpe.h24In, 2));
    if (isNum(qpe.d30In)) liveSub.push('30d ' + fmtIn(qpe.d30In, 2));
    if (isNum(qpe.wytdIn)) liveSub.push('WYTD ' + fmtIn(qpe.wytdIn, 2));

    var stormVal = '—';
    var stormSub = [];
    if (a14.available && (isNum(a14.hr24Yr10In) || isNum(a14.hr24Yr100In))) {
      stormVal = (isNum(a14.hr24Yr10In) ? ('10y ' + fmtIn(a14.hr24Yr10In, 2)) : '') +
        (isNum(a14.hr24Yr10In) && isNum(a14.hr24Yr100In) ? ' · ' : '') +
        (isNum(a14.hr24Yr100In) ? ('100y ' + fmtIn(a14.hr24Yr100In, 2)) : '');
      if (isNum(a14.day3Yr100In)) stormSub.push(P('3-day 100y ', '3日百年 ', '3d 100a ') + fmtIn(a14.day3Yr100In, 2));
      if (isNum(hmax.max1DayIn)) {
        stormSub.push(P('Max 1d ', '历史单日极值 ', 'Máx 1d ') + fmtIn(hmax.max1DayIn, 2) + (hmax.max1DayDate ? ' (' + hmax.max1DayDate.slice(0, 4) + ')' : ''));
      }
    } else if (isNum(hmax.max1DayIn)) {
      stormVal = P('Max 1d ', '单日极值 ', 'Máx 1d ') + fmtIn(hmax.max1DayIn, 2);
      if (hmax.max1DayDate) stormSub.push(hmax.max1DayDate);
      if (isNum(hmax.max3DayIn)) stormSub.push(P('3d max ', '3日极值 ', 'Máx 3d ') + fmtIn(hmax.max3DayIn, 2));
      stormSub.push(P('RCC-ACIS observed', 'RCC-ACIS 实测极值', 'Observado RCC-ACIS'));
    }

    var gutVal = '—';
    var gutSub = [];
    if (isNum(a14.hr1Yr10In) || isNum(a14.hr1Yr100In)) {
      gutVal = (isNum(a14.hr1Yr10In) ? (a14.hr1Yr10In.toFixed(2) + '"/hr') : '') +
        (isNum(a14.hr1Yr100In) ? (' (100y ' + a14.hr1Yr100In.toFixed(2) + '")') : '');
      gutSub.push(P('10-yr / 100-yr 60-min roof gutter sizing', '10年/100年一遇60分钟屋面天沟标准', 'Intensidad 60 min para canaletas'));
    } else if (isNum(ffg.hr1In)) {
      gutVal = ffg.hr1In.toFixed(2) + '"/1h';
    }
    if (isNum(ffg.hr1In) || isNum(ffg.hr3In)) {
      var ffgBits = [];
      if (isNum(ffg.hr1In)) ffgBits.push('1h ' + ffg.hr1In.toFixed(2) + '"');
      if (isNum(ffg.hr3In)) ffgBits.push('3h ' + ffg.hr3In.toFixed(2) + '"');
      gutSub = [P('Flash flood guidance: ', '山洪雨强阈值：', 'Guía inundación repentina: ') + ffgBits.join(' · ')];
    }

    var hasMonthly = (Array.isArray(rain.monthlyNormalsIn) && rain.monthlyNormalsIn.length === 12) ||
      (Array.isArray(rain.stationMonthlyNormalsIn) && rain.stationMonthlyNormalsIn.length === 12);

    return '<div class="rm-col">' +
      '<div class="rm-col-hdr">' +
        '<span class="rm-col-title">🌧️ ' + escH(P('Rainfall, Design Storms & Lot Drainage', '降雨气候、设计暴雨与排水容量', 'Lluvia, tormentas de diseño y drenaje')) + ' ' + refHtml + '</span>' +
        '<span class="rm-col-sub">' + escH(a14.volume || rain.annualNormalSource || 'NOAA · NASA POWER') + '</span>' +
      '</div>' +
      '<div class="rm-kpis">' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('Annual Normal', '年均正常降雨量', 'Normal anual')) + '</div>' +
          '<div class="rv">' + escH(annVal) + '</div>' +
          '<div class="rd">' + escH(annSub.join(' · ') || (rain.annualNormalSource || '')) + '</div></div>' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('Observed QPE & 7d QPF', '近期实测与未来7天预报', 'QPE obs. y QPF 7d')) + '</div>' +
          '<div class="rv">' + escH(liveVal) + '</div>' +
          '<div class="rd">' + escH(liveSub.join(' · ') || P('NOAA RFC radar+gauge & WPC', 'NOAA 多传感器实测与预报', 'Radar+pluviómetro NOAA RFC')) + '</div></div>' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('24-Hr Design Storm', '24小时设计暴雨深度', 'Tormenta diseño 24h')) + '</div>' +
          '<div class="rv">' + escH(stormVal) + '</div>' +
          '<div class="rd">' + escH(stormSub.join(' · ')) + '</div></div>' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('1-Hr Gutter / Flash Flood', '1小时天沟雨强与山洪阈值', 'Intensidad 1h / Inundación')) + '</div>' +
          '<div class="rv">' + escH(gutVal) + '</div>' +
          '<div class="rd">' + escH(gutSub.join(' · ')) + '</div></div>' +
      '</div>' +
      (hasMonthly
        ? '<div class="rm-chart-lbl"><span>' + escH(P('12-Month Normal Rainfall & Seasonal Soil Wetness Cycle', '12个月降雨均值与土壤干湿季节循环（地基胀缩驱动）', 'Ciclo mensual de lluvia normal y humedad del suelo')) + '</span></div>' +
          '<div id="' + CHART_ID + '"></div>'
        : '') +
      '</div>';
  }

  function renderSoilColumn(soil, rain, hasChartInLeftCol) {
    var refHtml = renderRefLink(
      (soil && soil.referenceUrl) || 'https://websoilsurvey.nrcs.usda.gov/',
      'USDA NRCS SSURGO / UC Davis SoilWeb',
      'USDA SSURGO ↗',
      'usda_soil_moisture'
    );

    var seriesBits = [];
    if (soil.dominantSoilSeries) {
      seriesBits.push(soil.dominantSoilSeries + (isNum(soil.componentPct) ? ' (' + Math.round(soil.componentPct) + '%)' : ''));
    }
    if (isNum(soil.urbanLandPct) && soil.urbanLandPct > 0) {
      seriesBits.push(Math.round(soil.urbanLandPct) + '% ' + P('urban complex', '城市建成复合土', 'complejo urbano'));
    }
    var subHdr = seriesBits.join(' · ') || soil.muname || 'USDA NRCS SSURGO';

    var drVal = soil.drainageClass || '—';
    var drSub = [];
    if (soil.hydrologicGroup) drSub.push('HSG ' + soil.hydrologicGroup);
    if (isNum(soil.availableWaterStorage0To100Cm)) {
      drSub.push('AWS ' + (soil.availableWaterStorage0To100Cm / 2.54).toFixed(1) + '" (0–100cm)');
    }

    var lepValHtml = isNum(soil.maxLepPct) ? escH(soil.maxLepPct.toFixed(1) + '% LEP') : '—';
    if (soil.shrinkSwellRisk) {
      lepValHtml += ' <span class="pill ' + shrinkSwellPillClass(soil.shrinkSwellRisk) + '">' +
        escH(localizeShrinkSwellRisk(soil.shrinkSwellRisk)) + '</span>';
    }
    var lepSub = [];
    if (isNum(soil.maxClayPct)) lepSub.push(P('Max clay ', '最高黏粒 ', 'Arcilla máx ') + Math.round(soil.maxClayPct) + '%');
    if (soil.nasaWetness && isNum(soil.nasaWetness.seasonalSwing)) {
      lepSub.push(P('Surface Δ ', '表土季节振幅 ', 'Oscilación Δ ') + Math.round(soil.nasaWetness.seasonalSwing * 100) + '%');
    }

    var wtVal = '—';
    var wtSub = '';
    if (isNum(soil.minWaterTableCm)) {
      var wtFt = (soil.minWaterTableCm / 30.48).toFixed(1);
      wtVal = Math.round(soil.minWaterTableCm) + ' cm (' + wtFt + ' ft)';
      wtSub = soil.minWaterTableCm < 150
        ? P('Shallow seasonal high water table — check sump/vapor barrier', '浅层季节性潜水位，需核查防潮与排水泵', 'Nivel freático somero — revisar barrera/bomba')
        : P('Moderate depth seasonal water table', '中等埋深季节性潜水位', 'Profundidad moderada');
    } else if (soil.mukey || soil.muname) {
      wtVal = P('> 200 cm (> 6.5 ft)', '> 200 厘米 (> 6.5 英尺)', '> 200 cm (> 6.5 pies)');
      wtSub = P('No shallow water table mapped in SSURGO (0–200 cm)', 'SSURGO 0–200厘米内无浅层地下水记录', 'Sin nivel freático somero en SSURGO');
    }

    var ksatVal = isNum(soil.limitingKsatInHr)
      ? (soil.limitingKsatInHr.toFixed(2) + ' in/hr')
      : (soil.infiltrationSuitability ? localizeSuitability(soil.infiltrationSuitability) : '—');
    var ksatSub = [];
    if (soil.infiltrationSuitability) {
      ksatSub.push(P('Dry-well/French drain: ', '渗井/盲沟适宜性：', 'Infiltración: ') + localizeSuitability(soil.infiltrationSuitability));
    }
    if (soil.basementSuitability) {
      ksatSub.push(P('Basement: ', '地下室：', 'Sótano: ') + localizeSuitability(soil.basementSuitability));
    } else if (soil.concreteCorrosion) {
      ksatSub.push(P('Concrete corrosion: ', '混凝土腐蚀性：', 'Corrosión concreto: ') + soil.concreteCorrosion);
    }

    var drObj = soil.drought || {};
    var nwmSat = isNum(soil.liveNwmSaturationPct) ? (soil.liveNwmSaturationPct.toFixed(0) + '% ' + P('sat.', '饱和度', 'sat.')) : null;
    var drCat = drObj.currentCategory || null;
    var liveMoistureVal = [nwmSat, drCat ? (drCat === 'None' ? P('No drought', '无干旱', 'Sin sequía') : drCat) : null]
      .filter(Boolean)
      .join(' · ') || '—';
    var drSubBits = [];
    if (drObj.cpcSeasonalOutlook) drSubBits.push('CPC: ' + drObj.cpcSeasonalOutlook);
    if (soil.nasaWetness && isNum(soil.nasaWetness.rh2mAnnual)) {
      drSubBits.push('RH ' + soil.nasaWetness.rh2mAnnual.toFixed(0) + '%' +
        (isNum(soil.nasaWetness.dewPointCAnnual) ? (' · Dew ' + soil.nasaWetness.dewPointCAnnual.toFixed(1) + '°C') : ''));
    }

    var diligenceItems = buildDiligenceItems(rain, soil);
    var calloutHtml = diligenceItems.length
      ? '<div class="rm-callout"><b>🧱 ' + escH(P('Foundation & Lot Drainage Buyer Checklist', '地基胀缩与庭院排水买方核查清单', 'Lista de verificación de cimientos y drenaje')) + '</b>' +
        '<ul>' + diligenceItems.map(function (it) { return '<li>' + escH(it) + '</li>'; }).join('') + '</ul></div>'
      : '';

    var soloWetnessChart = (!hasChartInLeftCol && soil.nasaWetness &&
      (Array.isArray(soil.nasaWetness.surfaceMonthly) || Array.isArray(soil.nasaWetness.rootZoneMonthly)))
      ? '<div class="rm-chart-lbl"><span>' + escH(P('12-Month NASA Seasonal Soil Wetness (%)', '12个月土壤含水饱和度季节曲线 (%)', 'Humedad mensual del suelo NASA (%)')) + '</span></div>' +
        '<div id="' + CHART_ID + '"></div>'
      : '';

    return '<div class="rm-col">' +
      '<div class="rm-col-hdr">' +
        '<span class="rm-col-title">🌱 ' + escH(P('Soil Drainage, Shrink-Swell & Moisture', '土壤排水、黏土胀缩与含水态势', 'Drenaje del suelo, expansión y humedad')) + ' ' + refHtml + '</span>' +
        '<span class="rm-col-sub" title="' + escH(soil.muname || '') + '">' + escH(subHdr) + '</span>' +
      '</div>' +
      '<div class="rm-kpis">' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('Drainage & Hydro Group', '天然排水等级与水文组', 'Drenaje y grupo hidrológico')) + '</div>' +
          '<div class="rv">' + escH(drVal) + '</div>' +
          '<div class="rd">' + escH(drSub.join(' · ')) + '</div></div>' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('Expansive Clay (LEP)', '膨胀性黏土指标 (LEP)', 'Arcilla expansiva (LEP)')) + '</div>' +
          '<div class="rv">' + lepValHtml + '</div>' +
          '<div class="rd">' + escH(lepSub.join(' · ')) + '</div></div>' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('Seasonal Water Table', '季节性最高地下水位', 'Nivel freático estacional')) + '</div>' +
          '<div class="rv">' + escH(wtVal) + '</div>' +
          '<div class="rd">' + escH(wtSub) + '</div></div>' +
        '<div class="rm-kpi"><div class="rk">' + escH(P('Subsoil Percolation (Ksat)', '底土渗透率与盲沟排水', 'Percolación subsuelo (Ksat)')) + '</div>' +
          '<div class="rv">' + escH(ksatVal) + '</div>' +
          '<div class="rd">' + escH(ksatSub.join(' · ')) + '</div></div>' +
      '</div>' +
      (liveMoistureVal !== '—'
        ? '<div class="rm-kpi" style="margin-bottom:8px"><div class="rk">' + escH(P('Live NOAA NWM Top-Soil Saturation & USDM Drought', 'NOAA 实时表土含水饱和度与全美干旱监测', 'Saturación actual NOAA NWM y sequía USDM')) + '</div>' +
            '<div class="rv" style="font-size:14.5px">' + escH(liveMoistureVal) + '</div>' +
            (drSubBits.length ? '<div class="rd">' + escH(drSubBits.join(' · ')) + '</div>' : '') +
          '</div>'
        : '') +
      soloWetnessChart +
      calloutHtml +
      '</div>';
  }

  function buildDualAxisChartOption(rain, soil) {
    var months = curLang() === 'zh'
      ? ['1月', '2月', '3月', '4月', '5月', '6月', '7月', '8月', '9月', '10月', '11月', '12月']
      : curLang() === 'es'
        ? ['Ene', 'Feb', 'Mar', 'Abr', 'May', 'Jun', 'Jul', 'Ago', 'Sep', 'Oct', 'Nov', 'Dic']
        : ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    var rainArr = (rain && Array.isArray(rain.monthlyNormalsIn) && rain.monthlyNormalsIn.length === 12)
      ? rain.monthlyNormalsIn
      : ((rain && Array.isArray(rain.stationMonthlyNormalsIn) && rain.stationMonthlyNormalsIn.length === 12)
        ? rain.stationMonthlyNormalsIn
        : null);

    var nw = (soil && soil.nasaWetness) || {};
    var surfArr = Array.isArray(nw.surfaceMonthly) && nw.surfaceMonthly.length === 12
      ? nw.surfaceMonthly.map(function (v) { return isNum(v) ? Math.round(v * 1000) / 10 : null; })
      : null;
    var rootArr = Array.isArray(nw.rootZoneMonthly) && nw.rootZoneMonthly.length === 12
      ? nw.rootZoneMonthly.map(function (v) { return isNum(v) ? Math.round(v * 1000) / 10 : null; })
      : null;

    if (!rainArr && !surfArr && !rootArr) return null;

    var globalC = (typeof C === 'object' && C) ? C : null;
    var ink2 = (globalC && globalC.ink2) || cssVar('--ink2', 'rgba(148,163,184,0.92)');
    var axisCol = (globalC && globalC.axis) || cssVar('--muted', 'rgba(148,163,184,0.78)');
    var gridCol = (globalC && globalC.grid) || cssVar('--line', 'rgba(148,163,184,0.22)');
    var blueCol = (globalC && globalC.blue) || cssVar('--blue', 'rgb(37,99,235)');
    var brandCol = (globalC && globalC.teal) || cssVar('--brand', 'rgb(20,184,166)');
    var amberCol = (globalC && globalC.amber) || cssVar('--amber', 'rgb(217,119,6)');

    var hasWetness = Boolean(surfArr || rootArr);
    var yAxes = [];
    var series = [];

    if (rainArr) {
      yAxes.push({
        type: 'value',
        position: 'left',
        axisLabel: {
          color: axisCol,
          fontSize: 10.5,
          formatter: function (v) { return v + '"'; }
        },
        splitLine: { lineStyle: { color: gridCol } }
      });
      series.push({
        name: P('Normal Rain (in)', '月均降雨 (英寸)', 'Lluvia normal (in)'),
        type: 'bar',
        yAxisIndex: 0,
        barMaxWidth: 16,
        data: rainArr,
        itemStyle: { color: blueCol, borderRadius: [3, 3, 0, 0] }
      });
    }

    if (hasWetness) {
      var wetAxisIdx = yAxes.length;
      yAxes.push({
        type: 'value',
        position: rainArr ? 'right' : 'left',
        min: 0,
        max: 100,
        axisLabel: {
          color: axisCol,
          fontSize: 10.5,
          formatter: function (v) { return v + '%'; }
        },
        splitLine: { show: !rainArr, lineStyle: { color: gridCol } }
      });
      if (surfArr) {
        series.push({
          name: P('Surface Soil Wetness (%)', '表层土壤湿度 (%)', 'Humedad superficial (%)'),
          type: 'line',
          yAxisIndex: wetAxisIdx,
          smooth: true,
          symbolSize: 5,
          data: surfArr,
          lineStyle: { color: brandCol, width: 2.2 },
          itemStyle: { color: brandCol }
        });
      }
      if (rootArr) {
        series.push({
          name: P('Root-Zone Wetness (%)', '根系层湿度 (%)', 'Humedad zona radicular (%)'),
          type: 'line',
          yAxisIndex: wetAxisIdx,
          smooth: true,
          showSymbol: false,
          data: rootArr,
          lineStyle: { color: amberCol, width: 1.8, type: 'dashed' },
          itemStyle: { color: amberCol }
        });
      }
    }

    return {
      grid: { left: 6, right: hasWetness && rainArr ? 8 : 10, top: 28, bottom: 20, containLabel: true },
      legend: {
        top: 0,
        itemWidth: 11,
        itemHeight: 8,
        textStyle: { fontSize: 10.8, color: ink2 }
      },
      tooltip: { trigger: 'axis' },
      xAxis: {
        type: 'category',
        data: months,
        axisLabel: { color: axisCol, fontSize: 10.2 },
        axisTick: { show: false },
        axisLine: { lineStyle: { color: gridCol } }
      },
      yAxis: yAxes,
      series: series
    };
  }

  function mountDualAxisChart(rain, soil) {
    var el = document.getElementById(CHART_ID);
    if (!el || typeof window.echarts === 'undefined') return;
    var opt = buildDualAxisChartOption(rain, soil);
    if (!opt) return;

    try {
      if (typeof chart === 'function') {
        st.chartInstance = chart(el, opt);
        return;
      }
    } catch (e) {}

    var prev = typeof window.echarts.getInstanceByDom === 'function'
      ? window.echarts.getInstanceByDom(el)
      : st.chartInstance;
    if (prev && typeof prev.dispose === 'function') {
      try { prev.dispose(); } catch (e2) {}
    }
    var inst = window.echarts.init(el, null, { renderer: 'canvas' });
    inst.setOption(opt);
    st.chartInstance = inst;
  }

  function refreshChartTheme() {
    var el = document.getElementById(CHART_ID);
    if (!el || typeof window.echarts === 'undefined') return;
    var inst = typeof window.echarts.getInstanceByDom === 'function'
      ? window.echarts.getInstanceByDom(el)
      : st.chartInstance;
    if (!inst || (typeof inst.isDisposed === 'function' && inst.isDisposed())) return;
    var rep = getReport();
    var hz = getHazardsData(rep);
    if (!hz) return;
    var rain = !(rep && rep.rainfallClimatologyDisabled) ? hz.rainfall : null;
    var soil = !(rep && rep.usdaSoilMoistureDisabled) ? hz.soilMoisture : null;
    var opt = buildDualAxisChartOption(rain, soil);
    if (opt) {
      try { inst.setOption(opt, true); } catch (e) {}
    }
  }

  function findInsertionAnchor(riskSec) {
    var subs = riskSec.querySelectorAll(':scope > .sub');
    for (var i = 0; i < subs.length; i++) {
      var txt = String(subs[i].textContent || '').toLowerCase();
      if (txt.indexOf('key findings') >= 0 || txt.indexOf('核心风险发现') >= 0 || txt.indexOf('hallazgos clave') >= 0) {
        return subs[i];
      }
    }
    return riskSec.querySelector(':scope > .more-flat') || riskSec.querySelector(':scope > .src-strip') || null;
  }

  function enhanceRiskSection() {
    if (st.rendering) return;
    var riskSec = document.getElementById('risk');
    if (!riskSec) return;

    hookMapExpandModal();
    injectCss();

    var rep = getReport();
    var enabledMods = rep && rep.enabledModules;
    var hazardsMod = rep && rep.modules && rep.modules.hazards;
    if (
      (Array.isArray(enabledMods) && enabledMods.indexOf('natural_hazards') < 0) ||
      (hazardsMod && hazardsMod.disabled === true)
    ) {
      var existingDisabled = document.getElementById(PANEL_ID);
      if (existingDisabled) existingDisabled.remove();
      return;
    }

    var hz = getHazardsData(rep);
    observeRiskSection(riskSec);
    if (!hz) return;

    var rainAllowed = !(rep && rep.rainfallClimatologyDisabled);
    var soilAllowed = !(rep && rep.usdaSoilMoistureDisabled);

    var rain = rainAllowed && hz.rainfall && typeof hz.rainfall === 'object'
      ? hz.rainfall
      : null;
    var soil = soilAllowed && hz.soilMoisture && typeof hz.soilMoisture === 'object'
      ? hz.soilMoisture
      : null;

    var hasRain = Boolean(rain);
    var hasSoil = Boolean(soil);

    st.rendering = true;
    try {
      mountHzMapChips(rainAllowed, soilAllowed);

      var prevPanel = document.getElementById(PANEL_ID);
      if (!hasRain && !hasSoil) {
        if (prevPanel) prevPanel.remove();
        return;
      }

      var hasChartInLeftCol = Boolean(
        rain && (
          (Array.isArray(rain.monthlyNormalsIn) && rain.monthlyNormalsIn.length === 12) ||
          (Array.isArray(rain.stationMonthlyNormalsIn) && rain.stationMonthlyNormalsIn.length === 12)
        )
      );

      var panel = prevPanel || document.createElement('div');
      panel.id = PANEL_ID;
      var colsHtml = '';
      if (hasRain && hasSoil) {
        colsHtml = '<div class="rm-grid">' +
          renderRainColumn(rain) +
          renderSoilColumn(soil, rain, hasChartInLeftCol) +
          '</div>';
      } else if (hasRain) {
        colsHtml = renderRainColumn(rain);
      } else if (hasSoil) {
        colsHtml = renderSoilColumn(soil, null, false);
      }
      panel.innerHTML = colsHtml;

      if (!prevPanel) {
        var anchor = findInsertionAnchor(riskSec);
        if (anchor && anchor.parentNode === riskSec) {
          riskSec.insertBefore(panel, anchor);
        } else {
          riskSec.appendChild(panel);
        }
      }

      mountDualAxisChart(rain, soil);
    } finally {
      st.rendering = false;
    }

    observeRiskSection(riskSec);
  }

  function observeRiskSection(riskSec) {
    if (st.riskObserver || typeof MutationObserver === 'undefined' || !riskSec) return;
    st.riskObserver = new MutationObserver(function (mutations) {
      if (st.rendering) return;
      var needsRemount = false;
      for (var i = 0; i < mutations.length; i++) {
        var m = mutations[i];
        if (m.type === 'childList') {
          if (!document.getElementById(PANEL_ID) || !riskSec.querySelector('#hzChips .oe-rm-chip')) {
            needsRemount = true;
            break;
          }
        }
      }
      if (needsRemount) {
        enhanceRiskSection();
      }
    });
    st.riskObserver.observe(riskSec, { childList: true });
  }

  function boot() {
    enhanceRiskSection();

    if (typeof MutationObserver !== 'undefined' && document.documentElement) {
      new MutationObserver(function (muts) {
        for (var i = 0; i < muts.length; i++) {
          var attr = muts[i].attributeName;
          if (attr === 'data-theme') {
            refreshChartTheme();
          } else if (attr === 'lang' || attr === 'data-display-links') {
            enhanceRiskSection();
          }
        }
      }).observe(document.documentElement, {
        attributes: true,
        attributeFilter: ['lang', 'data-theme', 'data-display-links']
      });
    }

    window.addEventListener('openestate:themechange', refreshChartTheme);
    if (window.OpenEstateCore && typeof window.OpenEstateCore.onSystemColorSchemeChange === 'function') {
      window.OpenEstateCore.onSystemColorSchemeChange(refreshChartTheme);
    } else if (typeof window.matchMedia === 'function') {
      var mq = window.matchMedia('(prefers-color-scheme: dark)');
      if (mq && typeof mq.addEventListener === 'function') {
        mq.addEventListener('change', refreshChartTheme);
      }
    }

    window.addEventListener('resize', function () {
      if (st.chartInstance && typeof st.chartInstance.resize === 'function') {
        try { st.chartInstance.resize(); } catch (e) {}
      }
    });
  }

  window.OpenEstateRainfallMoisture = {
    mount: enhanceRiskSection,
    refreshTheme: refreshChartTheme,
    _state: st
  };

  try {
    if (document.readyState === 'loading' && document.addEventListener) {
      document.addEventListener('DOMContentLoaded', boot);
    } else {
      boot();
    }
  } catch (err) {
    if (window.console && console.warn) {
      console.warn('rainfall_moisture_widget: init failed', err);
    }
  }
})();
