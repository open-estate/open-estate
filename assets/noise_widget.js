/* Open Estate Noise & Route Corridors Widget — standalone themed enhancer for `#environment` & `#envMap`.
 *
 * Renders per-extension acoustic dBA & schedule cards (`faa_flight_noise`, `highway_traffic_noise`,
 * `rail_transit_noise`, `military_airspace_noise`) and registers USDOT BTS 2022 aviation/road/rail
 * noise `L.tileLayer` chips plus rich GeoJSON flight/highway/rail/crossing/SUA layers onto `#envMap`.
 *
 * Strictly uses ONLY report theme CSS variables (`--surface`, `--surface-2`, `--soft`, `--ink`,
 * `--ink2`, `--muted`, `--line`, `--line-strong`, `--brand`, `--goodbg`, `--watchbg`, `--badbg`),
 * respects `window.OpenEstateCore?.isDisplayLinksEnabled?.()`, supports EN/中文/ES, and never
 * fabricates missing numbers.
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_NOISE_WIDGET_INIT__) return;
  window.__OPEN_ESTATE_NOISE_WIDGET_INIT__ = true;

  var PANEL_ID = 'oeNoiseExtPanel';
  var CSS_ID = 'oeNoiseWidgetCss';

  var st = {
    mutatingDom: false,
    activeFlags: Object.create(null),
    mountedMapInstance: null,
    mountedLeafletLayers: Object.create(null),
    envMapObserver: null
  };

  function isNum(x) {
    return typeof x === 'number' && isFinite(x);
  }

  function fmtInt(n) {
    if (!isNum(n)) return '—';
    try {
      return Math.round(n).toLocaleString();
    } catch (e) {
      return String(Math.round(n));
    }
  }

  function fmt1(n) {
    return isNum(n) ? n.toFixed(1) : '—';
  }

  function curLang() {
    try {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.langCode === 'function') {
        return window.OpenEstateCore.langCode();
      }
    } catch (e) {}
    var l = String((document.documentElement && document.documentElement.lang) || 'en').toLowerCase();
    return l.indexOf('zh') === 0 ? 'zh' : l.indexOf('es') === 0 ? 'es' : 'en';
  }

  function P(en, zh, es) {
    var l = curLang();
    return l === 'zh' ? (zh || en) : l === 'es' ? (es || en) : en;
  }

  function escH(s) {
    if (window.OpenEstateCore && typeof window.OpenEstateCore.esc === 'function') {
      return window.OpenEstateCore.esc(s);
    }
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  function getReport() {
    if (window.OpenEstateCore && typeof window.OpenEstateCore.getRep === 'function') {
      var r = window.OpenEstateCore.getRep();
      if (r) return r;
    }
    try {
      if (window.__REPORT__ && typeof window.__REPORT__ === 'object') return window.__REPORT__;
    } catch (e) {}
    try {
      var el = document.getElementById('report-data');
      var txt = el && el.textContent;
      return txt ? JSON.parse(txt) : null;
    } catch (e2) {
      return null;
    }
  }

  function getModData(rep, key) {
    if (window.OpenEstateCore && typeof window.OpenEstateCore.mod === 'function') {
      var d = window.OpenEstateCore.mod(rep, key);
      if (d) return d;
    }
    var m = rep && rep.modules && rep.modules[key];
    return (m && m.data) || (rep && rep[key]) || null;
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
      if (typeof document.cookie === 'string' && /(?:^|;\s*)display_link=1(?:;|$)/.test(document.cookie)) {
        return true;
      }
    } catch (e) {}
    return false;
  }

  function cleanRefHref(rawUrl, label) {
    if (!areRefLinksVisible() || !rawUrl) return null;
    var s = String(rawUrl).trim();
    if (typeof window.safeHttpUrl === 'function') {
      s = window.safeHttpUrl(s) || '';
    } else if (s.indexOf('https://') !== 0) {
      return null;
    }
    if (!s) return null;
    if (typeof window.toReferenceRedirectUrl === 'function') {
      return window.toReferenceRedirectUrl(s, { label: label || 'Source' }) || s;
    }
    if (window.OpenEstateCore && typeof window.OpenEstateCore.toReferenceRedirectUrl === 'function') {
      return window.OpenEstateCore.toReferenceRedirectUrl(s, { label: label || 'Source' }) || s;
    }
    return s;
  }

  function refLinkHtml(rawUrl, text) {
    var href = cleanRefHref(rawUrl, text);
    if (!href) return '';
    return '<a class="oe-nz-link src-lnk" href="' + escH(href) + '" data-direct-url="' + escH(rawUrl) + '" target="_blank" rel="noopener noreferrer">' + escH(text) + ' ↗</a>';
  }

  function dbaBadgeTone(db) {
    if (!isNum(db)) return 'soft';
    if (db >= 65) return 'bad';
    if (db >= 55) return 'watch';
    return 'good';
  }

  function injectCss() {
    if (document.getElementById(CSS_ID)) return;
    var css = [
      '#' + PANEL_ID + '{margin:14px 0 6px;padding:14px;border-radius:14px;background:var(--surface);border:1px solid var(--line);color:var(--ink)}',
      '#' + PANEL_ID + ' .oe-nz-top{display:flex;align-items:flex-start;justify-content:space-between;gap:10px;flex-wrap:wrap;margin-bottom:12px}',
      '#' + PANEL_ID + ' .oe-nz-title{font-size:14.5px;font-weight:800;color:var(--ink);line-height:1.3}',
      '#' + PANEL_ID + ' .oe-nz-sub{font-size:12px;color:var(--muted);margin-top:2px}',
      '#' + PANEL_ID + ' .oe-nz-kpis{display:flex;align-items:center;gap:6px;flex-wrap:wrap}',
      '#' + PANEL_ID + ' .oe-nz-badge{display:inline-flex;align-items:center;gap:5px;padding:3px 9px;border-radius:999px;font-size:11.5px;font-weight:700;border:1px solid var(--line);color:var(--ink);font-variant-numeric:tabular-nums}',
      '#' + PANEL_ID + ' .oe-nz-badge.good{background:var(--goodbg);border-color:var(--line-strong)}',
      '#' + PANEL_ID + ' .oe-nz-badge.watch{background:var(--watchbg);border-color:var(--line-strong)}',
      '#' + PANEL_ID + ' .oe-nz-badge.bad{background:var(--badbg);border-color:var(--line-strong)}',
      '#' + PANEL_ID + ' .oe-nz-badge.soft{background:var(--surface-2);color:var(--ink2)}',
      '#' + PANEL_ID + ' .oe-nz-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(265px,1fr));gap:10px}',
      '#' + PANEL_ID + ' .oe-nz-card{background:var(--surface-2);border:1px solid var(--line);border-radius:12px;padding:11px 12px;display:flex;flex-direction:column;gap:8px;min-width:0}',
      '#' + PANEL_ID + ' .oe-nz-card-hd{display:flex;align-items:flex-start;justify-content:space-between;gap:8px}',
      '#' + PANEL_ID + ' .oe-nz-card-ttl{font-size:13px;font-weight:800;color:var(--ink);line-height:1.3}',
      '#' + PANEL_ID + ' .oe-nz-card-mod{font-size:11px;color:var(--muted);margin-top:1px}',
      '#' + PANEL_ID + ' .oe-nz-metrics{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:6px}',
      '#' + PANEL_ID + ' .oe-nz-met{background:var(--soft);border:1px solid var(--line);border-radius:9px;padding:6px 8px}',
      '#' + PANEL_ID + ' .oe-nz-met .k{font-size:10.5px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.02em}',
      '#' + PANEL_ID + ' .oe-nz-met .v{font-size:15px;font-weight:800;color:var(--ink);font-variant-numeric:tabular-nums;margin-top:1px}',
      '#' + PANEL_ID + ' .oe-nz-met .d{font-size:11px;color:var(--ink2);margin-top:1px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
      '#' + PANEL_ID + ' .oe-nz-list{display:flex;flex-direction:column;gap:5px}',
      '#' + PANEL_ID + ' .oe-nz-row{background:var(--surface);border:1px solid var(--line);border-radius:8px;padding:6px 8px;font-size:11.8px;line-height:1.4;color:var(--ink2)}',
      '#' + PANEL_ID + ' .oe-nz-row-top{display:flex;align-items:center;justify-content:space-between;gap:6px;font-weight:700;color:var(--ink)}',
      '#' + PANEL_ID + ' .oe-nz-row-meta{font-size:11px;color:var(--muted);margin-top:2px}',
      '#' + PANEL_ID + ' .oe-nz-pills{display:flex;flex-wrap:wrap;gap:5px}',
      '#' + PANEL_ID + ' .oe-nz-pill{display:inline-flex;align-items:center;gap:4px;padding:2px 7px;border-radius:999px;font-size:11px;font-weight:600;background:var(--soft);border:1px solid var(--line);color:var(--ink2)}',
      '#' + PANEL_ID + ' .oe-nz-foot{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;margin-top:auto;padding-top:6px;border-top:1px solid var(--line);font-size:11px;color:var(--muted)}',
      '#' + PANEL_ID + ' .oe-nz-link{color:var(--brand);font-weight:700;text-decoration:none;font-size:11px}',
      '#' + PANEL_ID + ' .oe-nz-link:hover{text-decoration:underline}',
      '#envChips .chip.oe-nz-chip{background:var(--surface-2);border-color:var(--line-strong);color:var(--ink2)}',
      '#envChips .chip.oe-nz-chip.on{background:var(--brand);border-color:var(--brand);color:var(--surface)}',
      '.oe-nz-pop{color:var(--ink);font-size:12px;line-height:1.45;min-width:190px}',
      '.oe-nz-pop-ttl{font-weight:800;font-size:12.5px;color:var(--ink);margin-bottom:3px}',
      '.oe-nz-pop-sub{color:var(--ink2);font-size:11.5px}',
      '.oe-nz-pop-meta{color:var(--muted);font-size:11px;margin-top:3px}'
    ].join('\n');
    var style = document.createElement('style');
    style.id = CSS_ID;
    style.textContent = css;
    (document.head || document.documentElement).appendChild(style);
  }

  function renderFlightCard(fl) {
    if (!fl || typeof fl !== 'object') return '';
    var ldn = fl.estLdnDba;
    var lmax = fl.peakFlyoverLmaxDba;
    var nc = fl.nearestCorridor && typeof fl.nearestCorridor === 'object' ? fl.nearestCorridor : null;
    var corridors = Array.isArray(fl.runwayCorridors) ? fl.runwayCorridors : [];
    var shelves = Array.isArray(fl.airspaceShelves) ? fl.airspaceShelves : [];
    var adsb = fl.liveAdsb && typeof fl.liveAdsb === 'object' ? fl.liveAdsb : null;
    var aircraft = (adsb && Array.isArray(adsb.aircraft)) ? adsb.aircraft : [];

    var rowsHtml = '';
    if (corridors.length) {
      rowsHtml += '<div class="oe-nz-list">' + corridors.slice(0, 3).map(function (c) {
        var rwyLbl = (c.airportId || '') + ' ' + P('Rwy ', '跑道 ', 'Pista ') + (c.runwayEndId || '') + (c.ilsType ? ' · ' + c.ilsType : '');
        var altTxt = isNum(c.estGlidepathAltAglFt) ? (fmtInt(c.estGlidepathAltAglFt) + ' ft AGL') : '';
        var meta = [];
        if (isNum(c.crossTrackMi)) meta.push(c.crossTrackMi.toFixed(2) + ' mi ' + P('cross-track', '横向偏距', 'lateral'));
        if (isNum(c.alongTrackMi)) meta.push(c.alongTrackMi.toFixed(1) + ' mi ' + P('from threshold', '距跑道端', 'del umbral'));
        if (isNum(c.glidepathAngleDeg)) meta.push(c.glidepathAngleDeg.toFixed(1) + '° GPA');
        if (isNum(c.estLdnDba)) meta.push(fmt1(c.estLdnDba) + ' dBA Ldn');
        return '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>🛫 ' + escH(rwyLbl) + '</span><span>' + escH(altTxt) + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH(meta.join(' · ')) + '</div>' +
        '</div>';
      }).join('') + '</div>';
    } else if (Array.isArray(fl.airports) && fl.airports.length) {
      rowsHtml += '<div class="oe-nz-list">' + fl.airports.slice(0, 2).map(function (ap) {
        return '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>✈ ' + escH((ap.name || ap.ident) + ' (' + ap.ident + ')') + '</span><span>' + escH(isNum(ap.distanceMi) ? ap.distanceMi + ' mi' : '') + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH([ap.typeCode === 'HP' ? P('Heliport', '直升机场', 'Helipuerto') : P('Airport', '机场', 'Aeropuerto'), ap.direction || '', isNum(ap.elevationFt) ? ('Elev ' + fmtInt(ap.elevationFt) + ' ft') : ''].filter(Boolean).join(' · ')) + '</div>' +
        '</div>';
      }).join('') + '</div>';
    }

    var pills = [];
    shelves.slice(0, 2).forEach(function (s) {
      var lbl = (s.name || s.airspaceClass || 'Airspace') + (s.floorFtMsl || s.ceilingFtMsl ? ' (' + (s.floorFtMsl || 'SFC') + '–' + (s.ceilingFtMsl || '') + ' ft)' : '');
      pills.push('<span class="oe-nz-pill">' + escH(lbl) + '</span>');
    });
    aircraft.slice(0, 3).forEach(function (ac) {
      var cs = ac.callsign || ac.hex || 'ADS-B';
      var bits = [cs];
      if (ac.aircraftType) bits.push(ac.aircraftType);
      if (isNum(ac.altitudeFt)) bits.push(fmtInt(ac.altitudeFt) + ' ft');
      if (isNum(ac.distanceNm)) bits.push(ac.distanceNm.toFixed(1) + ' NM');
      pills.push('<span class="oe-nz-pill">✈ ' + escH(bits.join(' · ')) + '</span>');
    });

    var adsbMeta = adsb && adsb.available
      ? P(
          fmtInt(adsb.airborneCount) + ' live aircraft ≤15 NM (' + fmtInt(adsb.within5NmCount) + ' ≤5 NM)',
          '15海里内实时航班 ' + fmtInt(adsb.airborneCount) + ' 架（5海里内 ' + fmtInt(adsb.within5NmCount) + ' 架）',
          fmtInt(adsb.airborneCount) + ' aeronaves en vivo ≤15 NM (' + fmtInt(adsb.within5NmCount) + ' ≤5 NM)'
        )
      : P('FAA AIS + BTS runway alignments', 'FAA AIS 与 BTS 跑道航向数据', 'Alineaciones de pista FAA AIS + BTS');

    return '<div class="oe-nz-card" data-noise-card="faa_flight_noise">' +
      '<div class="oe-nz-card-hd">' +
        '<div>' +
          '<div class="oe-nz-card-ttl">' + escH(P('Commercial & GA Flight Paths', '民航与通航航线及下滑道', 'Rutas de vuelo comercial y general')) + '</div>' +
          '<div class="oe-nz-card-mod">' + escH(adsbMeta) + '</div>' +
        '</div>' +
        '<span class="oe-nz-badge ' + dbaBadgeTone(ldn) + '">' + escH(isNum(ldn) ? (fmt1(ldn) + ' dBA Ldn') : P('Screened', '已筛查', 'Evaluado')) + '</span>' +
      '</div>' +
      '<div class="oe-nz-metrics">' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Glidepath Alt', '最近航道高度', 'Altitud senda')) + '</div>' +
          '<div class="v">' + escH(nc && isNum(nc.estGlidepathAltAglFt) ? (fmtInt(nc.estGlidepathAltAglFt) + ' ft') : '—') + '</div>' +
          '<div class="d">' + escH(nc ? ((nc.airportId || '') + ' Rwy ' + (nc.runwayEndId || '')) : P('No aligned approach ≤6.5 mi', '6.5英里内无对准进近航道', 'Sin aproximación ≤6.5 mi')) + '</div></div>' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Peak Flyover', '单架飞越峰值', 'Pico sobrevuelo')) + '</div>' +
          '<div class="v">' + escH(isNum(lmax) ? (fmt1(lmax) + ' dBA') : '—') + '</div>' +
          '<div class="d">' + escH(P('FAA AEDT Lmax screening', 'FAA AEDT 最大声级估算', 'Estimación Lmax FAA AEDT')) + '</div></div>' +
      '</div>' +
      rowsHtml +
      (pills.length ? '<div class="oe-nz-pills">' + pills.join('') + '</div>' : '') +
      '<div class="oe-nz-foot"><span>FAA AIS · BTS · ADS-B</span>' + refLinkHtml(fl.faaReferenceUrl || fl.referenceUrl, 'FAA Aero Data') + '</div>' +
    '</div>';
  }

  function renderHighwayCard(hw) {
    if (!hw || typeof hw !== 'object') return '';
    var ldn = hw.estLdnDba;
    var pkLeq = hw.peakHourLeqDba;
    var corridors = Array.isArray(hw.corridors) ? hw.corridors : [];
    var ctPeaks = Array.isArray(hw.caltransPeakHours) ? hw.caltransPeakHours : [];

    var rowsHtml = '';
    if (corridors.length) {
      rowsHtml += '<div class="oe-nz-list">' + corridors.slice(0, 3).map(function (c) {
        var meta = [];
        if (isNum(c.aadt)) meta.push('AADT ' + fmtInt(c.aadt));
        if (isNum(c.heavyTruckPct)) meta.push(c.heavyTruckPct.toFixed(1) + '% ' + P('trucks', '货车', 'camiones'));
        if (isNum(c.speedLimitMph)) meta.push(fmtInt(c.speedLimitMph) + ' mph');
        if (isNum(c.distanceM)) meta.push(fmtInt(c.distanceM) + ' m');
        return '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>🛣 ' + escH(c.routeId || 'Corridor') + '</span><span>' + escH(isNum(c.estLdnDba) ? (fmt1(c.estLdnDba) + ' dBA') : '') + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH(meta.join(' · ')) + '</div>' +
        '</div>';
      }).join('') + '</div>';
    }

    var pills = ctPeaks.slice(0, 2).map(function (cp) {
      return '<span class="oe-nz-pill">' + escH('SR-' + (cp.route || '') + ' ' + P('Peak Hr ', '高峰小时 ', 'Hora pico ') + fmtInt(cp.peakHourVolume)) + '</span>';
    });

    return '<div class="oe-nz-card" data-noise-card="highway_traffic_noise">' +
      '<div class="oe-nz-card-hd">' +
        '<div>' +
          '<div class="oe-nz-card-ttl">' + escH(P('Highway AADT & Heavy-Truck Noise', '高速公路车流量与重卡噪音', 'Tráfico AADT y camiones pesados')) + '</div>' +
          '<div class="oe-nz-card-mod">' + escH(P('FHWA HPMS traffic counts + TNM 2.5 line-source', 'FHWA HPMS 实测车流与 TNM 2.5 线声源模型', 'Conteos FHWA HPMS + modelo TNM 2.5')) + '</div>' +
        '</div>' +
        '<span class="oe-nz-badge ' + dbaBadgeTone(ldn) + '">' + escH(isNum(ldn) ? (fmt1(ldn) + ' dBA Ldn') : P('Screened', '已筛查', 'Evaluado')) + '</span>' +
      '</div>' +
      '<div class="oe-nz-metrics">' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Peak-Hour Leq', '高峰小时等效声级', 'Leq hora pico')) + '</div>' +
          '<div class="v">' + escH(isNum(pkLeq) ? (fmt1(pkLeq) + ' dBA') : '—') + '</div>' +
          '<div class="d">' + escH(P('Rush-hour corridor traffic', '通勤高峰时段噪音', 'Hora punta')) + '</div></div>' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Truck Pass-by', '重卡经过峰值', 'Pico de camión')) + '</div>' +
          '<div class="v">' + escH(isNum(hw.peakTruckLmaxDba) ? (fmt1(hw.peakTruckLmaxDba) + ' dBA') : '—') + '</div>' +
          '<div class="d">' + escH(corridors[0] && isNum(corridors[0].heavyTruckPct) ? (corridors[0].heavyTruckPct.toFixed(1) + '% ' + P('truck share', '重卡占比', '% camiones')) : 'FHWA TNM 2.5') + '</div></div>' +
      '</div>' +
      rowsHtml +
      (pills.length ? '<div class="oe-nz-pills">' + pills.join('') + '</div>' : '') +
      '<div class="oe-nz-foot"><span>USDOT FHWA HPMS</span>' + refLinkHtml(hw.fhwaReferenceUrl || hw.referenceUrl, 'FHWA HPMS') + '</div>' +
    '</div>';
  }

  function renderRailCard(rl) {
    if (!rl || typeof rl !== 'object') return '';
    var ldn = rl.estLdnDba;
    var hornLmax = rl.peakHornLmaxDba;
    var qzCnt = rl.quietZoneCrossingCount || 0;
    var hornCnt = rl.activeHornCrossingCount || 0;
    var xings = Array.isArray(rl.crossings) ? rl.crossings : [];
    var routes = Array.isArray(rl.transitRoutes) ? rl.transitRoutes : [];

    var zoneBadgeTone = hornCnt > 0 ? (isNum(hornLmax) && hornLmax >= 75 ? 'bad' : 'watch') : (qzCnt > 0 ? 'good' : 'soft');
    var zoneBadgeTxt = hornCnt > 0
      ? P(hornCnt + ' Active Horn Crossing(s)', hornCnt + ' 处鸣笛道口', hornCnt + ' cruce(s) con bocina')
      : (qzCnt > 0
        ? P(qzCnt + ' Quiet Zone (24h Horn Ban)', qzCnt + ' 处 24h 禁鸣静音区', qzCnt + ' Zona Silenciosa 24h')
        : (isNum(ldn) ? (fmt1(ldn) + ' dBA Ldn') : P('No Active Crossings', '周边无活跃平交道口', 'Sin cruces activos')));

    var rowsHtml = '';
    if (xings.length) {
      rowsHtml += '<div class="oe-nz-list">' + xings.slice(0, 3).map(function (x) {
        var tag = !x.isAtGrade
          ? (x.crossingPosition || P('Grade-separated', '立交分离', 'Desnivel'))
          : (x.isQuietZone
            ? P('24h Quiet Zone', '24h 禁鸣区', 'Zona Silenciosa 24h')
            : P('Active Horn', '火车鸣笛', 'Bocina activa'));
        var meta = [
          fmtInt(x.daylightThruTrains || 0) + ' ' + P('day', '昼间', 'día') + ' / ' + fmtInt(x.nighttimeThruTrains || 0) + ' ' + P('night trains', '夜间班次', 'trenes noct.'),
          isNum(x.maxSpeedMph) ? (fmtInt(x.maxSpeedMph) + ' mph') : '',
          isNum(x.distanceMi) ? (x.distanceMi.toFixed(2) + ' mi') : ''
        ].filter(Boolean);
        return '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>🚆 ' + escH((x.street || 'Crossing') + (x.railroadCode ? ' (' + x.railroadCode + ')' : '')) + '</span><span>' + escH(tag) + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH(meta.join(' · ')) + '</div>' +
        '</div>';
      }).join('') + '</div>';
    }

    var pills = routes.slice(0, 3).map(function (tr) {
      return '<span class="oe-nz-pill">' + escH((tr.agency || '') + ' ' + (tr.routeName || '')) + '</span>';
    });

    return '<div class="oe-nz-card" data-noise-card="rail_transit_noise">' +
      '<div class="oe-nz-card-hd">' +
        '<div>' +
          '<div class="oe-nz-card-ttl">' + escH(P('Rail Schedules, Horns & Quiet Zones', '铁路班次、火车鸣笛与静音区', 'Horarios de tren, bocinas y zonas silenciosas')) + '</div>' +
          '<div class="oe-nz-card-mod">' + escH(P('FRA Crossing Inventory + 49 CFR 222 horn rules', 'FRA 道口名录与 49 CFR 222 禁鸣规则', 'Inventario FRA + regla de bocinas 49 CFR 222')) + '</div>' +
        '</div>' +
        '<span class="oe-nz-badge ' + zoneBadgeTone + '">' + escH(zoneBadgeTxt) + '</span>' +
      '</div>' +
      '<div class="oe-nz-metrics">' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Wayside Rail Ldn', '轨道沿线等效声级', 'Ldn vía férrea')) + '</div>' +
          '<div class="v">' + escH(isNum(ldn) ? (fmt1(ldn) + ' dBA') : '—') + '</div>' +
          '<div class="d">' + escH(P('FTA 2018 wayside model', 'FTA 轨道噪声模型', 'Modelo FTA 2018')) + '</div></div>' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Horn Impulse Lmax', '道口鸣笛峰值', 'Pico de bocina')) + '</div>' +
          '<div class="v">' + escH(isNum(hornLmax) ? (fmt1(hornLmax) + ' dBA') : (qzCnt > 0 ? P('Suppressed', '已禁鸣抑制', 'Suprimida') : '—')) + '</div>' +
          '<div class="d">' + escH(qzCnt > 0 && !isNum(hornLmax) ? P('24-hr Quiet Zone active', '24小时静音区生效', 'Zona silenciosa 24h') : P('At-grade horn screening', '平交道口鸣笛评估', 'Bocina en cruce')) + '</div></div>' +
      '</div>' +
      rowsHtml +
      (pills.length ? '<div class="oe-nz-pills">' + pills.join('') + '</div>' : '') +
      '<div class="oe-nz-foot"><span>USDOT FRA · FTA · BTS</span>' + refLinkHtml(rl.referenceUrl, 'BTS Transit Map') + '</div>' +
    '</div>';
  }

  function renderMilitaryCard(mil) {
    if (!mil || typeof mil !== 'object') return '';
    var ldn = mil.estLdnDba;
    var lmax = mil.peakJetLmaxDba;
    var suas = Array.isArray(mil.specialUseAirspace) ? mil.specialUseAirspace : [];
    var mtrs = Array.isArray(mil.militaryTrainingRoutes) ? mil.militaryTrainingRoutes : [];
    var insts = Array.isArray(mil.installations) ? mil.installations : [];
    var afs = Array.isArray(mil.federalAirfieldCorridors) ? mil.federalAirfieldCorridors : [];

    var rowsHtml = '';
    var rowItems = [];
    afs.slice(0, 2).forEach(function (af) {
      rowItems.push(
        '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>🛩 ' + escH((af.airportName || af.airportId) + ' Rwy ' + (af.runwayEndId || '')) + '</span><span>' + escH(isNum(af.estGlidepathAltAglFt) ? (fmtInt(af.estGlidepathAltAglFt) + ' ft AGL') : '') + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH([isNum(af.crossTrackMi) ? (af.crossTrackMi.toFixed(2) + ' mi ' + P('cross-track', '横向偏距', 'lateral')) : '', isNum(af.peakLmaxDba) ? ('~' + fmtInt(af.peakLmaxDba) + ' dBA ' + P('peak', '峰值', 'pico')) : ''].filter(Boolean).join(' · ')) + '</div>' +
        '</div>'
      );
    });
    suas.slice(0, 2).forEach(function (s) {
      var locTxt = s.inside ? P('Overhead', '位于空域下方', 'Sobre la propiedad') : (isNum(s.distanceMi) ? (s.distanceMi.toFixed(1) + ' mi') : '');
      rowItems.push(
        '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>🛡 ' + escH((s.name || 'SUA') + ' (' + (s.typeCode || 'SUA') + ')') + '</span><span>' + escH(locTxt) + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH([
            P('Schedule: ', '启用时段：', 'Horario: ') + (s.timesOfUse || P('By NOTAM', '按航行通告 NOTAM', 'Por NOTAM')),
            (s.floor || 'SFC') + '–' + (s.ceiling || '') + ' ft'
          ].join(' · ')) + '</div>' +
        '</div>'
      );
    });
    mtrs.slice(0, 2).forEach(function (m) {
      if (rowItems.length >= 3) return;
      rowItems.push(
        '<div class="oe-nz-row">' +
          '<div class="oe-nz-row-top"><span>⚡ ' + escH(m.routeId || 'MTR') + '</span><span>' + escH(isNum(m.distanceNm) ? (m.distanceNm.toFixed(1) + ' NM') : '') + '</span></div>' +
          '<div class="oe-nz-row-meta">' + escH([
            P('Floor ', '底高 ', 'Piso ') + fmtInt(m.floorFt) + ' ft',
            '±' + fmt1(m.corridorHalfWidthNm) + ' NM ' + P('corridor', '走廊半宽', 'corredor')
          ].join(' · ')) + '</div>' +
        '</div>'
      );
    });
    if (rowItems.length) {
      rowsHtml = '<div class="oe-nz-list">' + rowItems.slice(0, 3).join('') + '</div>';
    }

    var pills = insts.slice(0, 3).map(function (b) {
      return '<span class="oe-nz-pill">' + escH((b.name || 'DoD') + (isNum(b.distanceMi) ? (' · ' + b.distanceMi.toFixed(1) + ' mi') : '')) + '</span>';
    });

    return '<div class="oe-nz-card" data-noise-card="military_airspace_noise">' +
      '<div class="oe-nz-card-hd">' +
        '<div>' +
          '<div class="oe-nz-card-ttl">' + escH(P('Military SUA Schedules & MTR Corridors', '军用特殊空域时刻表与低空走廊', 'Horarios de espacio aéreo militar y rutas MTR')) + '</div>' +
          '<div class="oe-nz-card-mod">' + escH(P('FAA Special Use Airspace + DoD AICUZ screening', 'FAA 特殊空域 TIMESOFUSE 与国防部 AICUZ 评估', 'FAA SUA TIMESOFUSE + evaluación DoD AICUZ')) + '</div>' +
        '</div>' +
        '<span class="oe-nz-badge ' + dbaBadgeTone(ldn) + '">' + escH(isNum(ldn) ? (fmt1(ldn) + ' dBA Ldn') : P('Screened', '已筛查', 'Evaluado')) + '</span>' +
      '</div>' +
      '<div class="oe-nz-metrics">' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Nearby SUA / MTR', '周边特殊空域 / 航线', 'SUA / MTR cercanos')) + '</div>' +
          '<div class="v">' + escH(fmtInt(suas.length) + ' / ' + fmtInt(mtrs.length)) + '</div>' +
          '<div class="d">' + escH(P('SUA zones · MTR routes', '特殊空域 · 军训低空航线', 'Zonas SUA · Rutas MTR')) + '</div></div>' +
        '<div class="oe-nz-met"><div class="k">' + escH(P('Peak Overflight', '军机飞越峰值', 'Pico militar')) + '</div>' +
          '<div class="v">' + escH(isNum(lmax) ? (fmt1(lmax) + ' dBA') : '—') + '</div>' +
          '<div class="d">' + escH(P('AICUZ / MR_NMAP Lmax', 'AICUZ / MR_NMAP 峰值', 'AICUZ / MR_NMAP Lmax')) + '</div></div>' +
      '</div>' +
      rowsHtml +
      (pills.length ? '<div class="oe-nz-pills">' + pills.join('') + '</div>' : '') +
      '<div class="oe-nz-foot"><span>FAA SUA · MTR · NTAD DoD</span>' + refLinkHtml(mil.referenceUrl, 'FAA SUA') + '</div>' +
    '</div>';
  }

  function renderExtensionPanel(envEl, nz) {
    var existing = document.getElementById(PANEL_ID);
    var exts = (nz && nz.extensions && typeof nz.extensions === 'object') ? nz.extensions : null;
    if (!exts) {
      if (existing) existing.remove();
      return;
    }

    var cardsHtml = [
      renderFlightCard(exts.faaFlight),
      renderHighwayCard(exts.highwayTraffic),
      renderRailCard(exts.railTransit),
      renderMilitaryCard(exts.militaryAirspace)
    ].filter(Boolean).join('');

    if (!cardsHtml) {
      if (existing) existing.remove();
      return;
    }

    var kpis = [];
    if (isNum(nz.estLdnDb)) {
      kpis.push('<span class="oe-nz-badge ' + dbaBadgeTone(nz.estLdnDb) + '">' + escH(P('Composite ', '综合等效 ', 'Compuesto ') + fmt1(nz.estLdnDb) + ' dBA Ldn') + '</span>');
    }
    if (isNum(nz.peakLmaxDb)) {
      kpis.push('<span class="oe-nz-badge ' + dbaBadgeTone(nz.peakLmaxDb - 12) + '">' + escH(P('Peak Event ', '单次事件峰值 ', 'Evento pico ') + fmt1(nz.peakLmaxDb) + ' dBA Lmax') + '</span>');
    }

    var html =
      '<div class="oe-nz-top">' +
        '<div>' +
          '<div class="oe-nz-title">' + escH(P('Multi-Source Acoustic & Corridor Intelligence', '多源航线、车流、铁路禁鸣区与空域噪音精算', 'Inteligencia acústica de vuelos, tráfico, trenes y espacio aéreo')) + '</div>' +
          '<div class="oe-nz-sub">' + escH(P('Modeled from official FAA AIS/BTS runways, FHWA HPMS truck volumes, FRA grade-crossing Quiet Zones, and Special Use Airspace schedules.', '基于 FAA 跑道下滑道、FHWA 实测重卡车流、FRA 平交道口 24h 禁鸣区及军用特殊空域时刻表建模。', 'Modelado con pistas FAA/BTS, volúmenes de camiones FHWA HPMS, Zonas Silenciosas FRA y horarios SUA.')) + '</div>' +
        '</div>' +
        (kpis.length ? '<div class="oe-nz-kpis">' + kpis.join('') + '</div>' : '') +
      '</div>' +
      '<div class="oe-nz-grid">' + cardsHtml + '</div>';

    var panel = existing || document.createElement('div');
    panel.id = PANEL_ID;
    panel.innerHTML = html;

    if (!existing) {
      var mapEl = document.getElementById('envMap');
      if (mapEl && mapEl.parentNode === envEl) {
        mapEl.insertAdjacentElement('afterend', panel);
      } else {
        envEl.appendChild(panel);
      }
    }
  }

  function findEnvLayer(envData, id) {
    var arr = (envData && Array.isArray(envData.mapLayers)) ? envData.mapLayers : [];
    for (var i = 0; i < arr.length; i++) {
      if (arr[i] && arr[i].id === id && Array.isArray(arr[i].features) && arr[i].features.length) {
        return arr[i];
      }
    }
    return null;
  }

  function buildPopupHtml(p, fallbackTitle) {
    var title = p.name || fallbackTitle || '';
    var sub = p.popup || '';
    var extra = [];
    if (p.featureKind === 'grade_crossing') {
      extra.push(
        (p.isQuietZone
          ? P('✓ 24-hr Quiet Zone (Horns Suppressed)', '✓ 24小时禁鸣静音区（禁止常规鸣笛）', '✓ Zona Silenciosa 24h (Bocina suprimida)')
          : P('⚠ Active Horn Crossing', '⚠ 火车常规鸣笛道口', '⚠ Cruce con bocina activa'))
      );
    } else if (p.featureKind === 'sua' && p.timesOfUse) {
      extra.push(P('Schedule: ', '启用时段：', 'Horario: ') + p.timesOfUse);
    } else if (p.featureKind === 'aircraft') {
      var acBits = [];
      if (p.aircraftType) acBits.push(p.aircraftType);
      if (isNum(p.groundSpeedKt)) acBits.push(fmtInt(p.groundSpeedKt) + ' kt');
      if (isNum(p.verticalRateFpm) && p.verticalRateFpm !== 0) {
        acBits.push((p.verticalRateFpm > 0 ? '+' : '') + fmtInt(p.verticalRateFpm) + ' fpm');
      }
      if (acBits.length) extra.push(acBits.join(' · '));
    }
    var link = p.url ? refLinkHtml(p.url, P('Official Record', '官方档案', 'Registro oficial')) : '';
    return '<div class="oe-nz-pop">' +
      '<div class="oe-nz-pop-ttl">' + escH(title) + '</div>' +
      (sub ? '<div class="oe-nz-pop-sub">' + escH(sub) + '</div>' : '') +
      (extra.length ? '<div class="oe-nz-pop-meta">' + escH(extra.join(' · ')) + '</div>' : '') +
      (link ? '<div style="margin-top:4px">' + link + '</div>' : '') +
    '</div>';
  }

  function makeRichGeoLayer(lyr, defaultRadius) {
    if (typeof window.L === 'undefined' || !lyr) return null;
    var baseColor = lyr.color || '#0284c7';
    var pinRing = 'currentColor';
    try {
      var surf = getComputedStyle(document.documentElement).getPropertyValue('--surface').trim();
      if (surf) pinRing = surf;
    } catch (e) {}
    return window.L.geoJSON({ type: 'FeatureCollection', features: lyr.features || [] }, {
      pointToLayer: function (f, ll) {
        var p = (f && f.properties) || {};
        var col = p.color || baseColor;
        var rad = p.featureKind === 'aircraft' ? 6.5 : (p.featureKind === 'grade_crossing' ? 7 : (defaultRadius || 6));
        return window.L.circleMarker(ll, {
          radius: rad,
          color: pinRing,
          weight: 1.8,
          fillColor: col,
          fillOpacity: 0.94
        });
      },
      style: function (f) {
        var gType = f && f.geometry && f.geometry.type;
        if (gType === 'Point') return {};
        var p = (f && f.properties) || {};
        var col = p.color || baseColor;
        var isPoly = gType === 'Polygon' || gType === 'MultiPolygon';
        return {
          color: col,
          weight: isNum(p.weight) ? p.weight : (isPoly ? 1.6 : 3),
          opacity: isPoly ? 0.78 : 0.9,
          fillColor: col,
          fillOpacity: p.featureKind === 'glidepath_cone' ? 0.12 : (isPoly ? 0.14 : 0),
          dashArray: p.dash || null
        };
      },
      onEachFeature: function (f, layer) {
        var p = (f && f.properties) || {};
        var popHtml = buildPopupHtml(p, lyr.title);
        if (typeof layer.bindPopup === 'function') {
          layer.bindPopup(popHtml);
        }
        if (typeof layer.bindTooltip === 'function') {
          var tipHtml = '<b>' + escH(p.name || lyr.title || '') + '</b>' + (p.popup ? '<span> ' + escH(p.popup) + '</span>' : '');
          layer.bindTooltip(tipHtml, {
            sticky: (f && f.geometry && f.geometry.type) !== 'Point',
            direction: 'top',
            offset: [0, -8],
            opacity: 1,
            className: 'atip'
          });
        }
      }
    });
  }

  function resolveBtsTileUrl(nz, state, mode) {
    var fromPayload = nz && nz.btsTiles && nz.btsTiles[mode] && nz.btsTiles[mode].tileUrl;
    if (fromPayload) return fromPayload;
    var stUp = String(state || '').trim().toUpperCase();
    var region = stUp === 'AK' ? 'Alaska' : (stUp === 'HI' ? 'Hawaii' : 'CONUS');
    if (region === 'Hawaii' && mode === 'rail') return null;
    return 'https://tiles.arcgis.com/tiles/xOi1kZaI0eWDREZv/arcgis/rest/services/NTAD_Noise_2022_' + region + '_' + mode + '/MapServer/tile/{z}/{y}/{x}';
  }

  function buildOverlayDefs(envData, nz) {
    var defs = [];
    var state = (envData && envData.state) || '';

    var flLyr = findEnvLayer(envData, 'noise-flight-corridors');
    if (flLyr) {
      defs.push({
        key: 'noise-flight-corridors',
        label: P('Flight corridors & ADS-B', '航线走廊与实时航班', 'Rutas de vuelo y ADS-B'),
        color: '#0284c7',
        count: flLyr.features.length,
        defaultOn: true,
        make: function () { return makeRichGeoLayer(flLyr, 6); }
      });
    }

    var hwLyr = findEnvLayer(envData, 'noise-highway-aadt');
    if (hwLyr) {
      defs.push({
        key: 'noise-highway-aadt',
        label: P('Highway AADT & trucks', '道路实测车流与重卡', 'Tráfico AADT y camiones'),
        color: '#ea580c',
        count: hwLyr.features.length,
        defaultOn: true,
        make: function () { return makeRichGeoLayer(hwLyr, 6); }
      });
    }

    var rlLines = findEnvLayer(envData, 'noise-rail-lines');
    if (rlLines) {
      defs.push({
        key: 'noise-rail-lines',
        label: P('Rail & transit lines', '铁路与城轨线路', 'Líneas de tren y tránsito'),
        color: '#7c3aed',
        count: rlLines.features.length,
        defaultOn: true,
        make: function () { return makeRichGeoLayer(rlLines, 6); }
      });
    }

    var rlXings = findEnvLayer(envData, 'noise-rail-crossings');
    if (rlXings) {
      defs.push({
        key: 'noise-rail-crossings',
        label: P('Rail crossings (Horn / Quiet Zone)', '铁路道口（鸣笛 / 禁鸣区）', 'Cruces de tren (Bocina / Silencio)'),
        color: '#0d9488',
        count: rlXings.features.length,
        defaultOn: true,
        make: function () { return makeRichGeoLayer(rlXings, 7); }
      });
    }

    var milLyr = findEnvLayer(envData, 'noise-military-airspace');
    if (milLyr) {
      defs.push({
        key: 'noise-military-airspace',
        label: P('Military SUA & MTR', '军用特殊空域与走廊', 'Espacio militar SUA y MTR'),
        color: '#9333ea',
        count: milLyr.features.length,
        defaultOn: false,
        make: function () { return makeRichGeoLayer(milLyr, 6); }
      });
    }

    if (nz && nz.checked !== false) {
      var avTile = resolveBtsTileUrl(nz, state, 'aviation');
      if (avTile) {
        defs.push({
          key: 'bts-tile-aviation',
          label: P('BTS Aviation Noise', 'BTS 航空噪音热力层', 'Ruido aéreo BTS'),
          color: '#0284c7',
          count: null,
          defaultOn: false,
          make: function () {
            return window.L.tileLayer(avTile, {
              opacity: 0.44,
              maxZoom: 19,
              maxNativeZoom: 12,
              attribution: 'US DOT/BTS Aviation Noise 2022'
            });
          }
        });
      }
      var roadTile = resolveBtsTileUrl(nz, state, 'road');
      if (roadTile) {
        defs.push({
          key: 'bts-tile-road',
          label: P('BTS Road Noise', 'BTS 道路噪音热力层', 'Ruido vial BTS'),
          color: '#ea580c',
          count: null,
          defaultOn: false,
          make: function () {
            return window.L.tileLayer(roadTile, {
              opacity: 0.44,
              maxZoom: 19,
              maxNativeZoom: 12,
              attribution: 'US DOT/BTS Road Noise 2022'
            });
          }
        });
      }
      var railTile = resolveBtsTileUrl(nz, state, 'rail');
      if (railTile) {
        defs.push({
          key: 'bts-tile-rail',
          label: P('BTS Rail Noise', 'BTS 铁路噪音热力层', 'Ruido ferroviario BTS'),
          color: '#7c3aed',
          count: null,
          defaultOn: false,
          make: function () {
            return window.L.tileLayer(railTile, {
              opacity: 0.48,
              maxZoom: 19,
              maxNativeZoom: 12,
              attribution: 'US DOT/BTS Rail Noise 2022'
            });
          }
        });
      }
    }

    return defs;
  }

  function syncLayersToLeafletMap(map, defs) {
    if (!map || typeof window.L === 'undefined') return;
    if (st.mountedMapInstance !== map) {
      st.mountedMapInstance = map;
      st.mountedLeafletLayers = Object.create(null);
    }
    for (var i = 0; i < defs.length; i++) {
      var d = defs[i];
      var isOn = st.activeFlags[d.key] !== undefined ? st.activeFlags[d.key] : d.defaultOn;
      var existingLayer = st.mountedLeafletLayers[d.key];
      if (isOn) {
        if (!existingLayer) {
          existingLayer = d.make();
          st.mountedLeafletLayers[d.key] = existingLayer;
        }
        if (existingLayer && !map.hasLayer(existingLayer)) {
          existingLayer.addTo(map);
        }
      } else if (existingLayer && map.hasLayer(existingLayer)) {
        map.removeLayer(existingLayer);
      }
    }
  }

  function enhanceEnvMapAndChips(envData, nz) {
    var chipsHost = document.getElementById('envChips');
    var mapEl = document.getElementById('envMap');
    if (!chipsHost || !mapEl) return;

    var defs = buildOverlayDefs(envData, nz);
    chipsHost.querySelectorAll('.chip.oe-nz-chip').forEach(function (n) { n.remove(); });
    if (!defs.length) return;

    defs.forEach(function (d) {
      if (st.activeFlags[d.key] === undefined) {
        st.activeFlags[d.key] = Boolean(d.defaultOn);
      }
      var chip = document.createElement('span');
      chip.className = 'chip oe-nz-chip' + (st.activeFlags[d.key] ? ' on' : '');
      chip.setAttribute('data-noise-key', d.key);
      chip.innerHTML = '<i style="background:' + escH(d.color) + '"></i>' +
        escH(d.label) +
        (d.count != null ? ' <em>' + fmtInt(d.count) + '</em>' : '');
      chip.addEventListener('click', function () {
        var nextOn = !st.activeFlags[d.key];
        st.activeFlags[d.key] = nextOn;
        chip.classList.toggle('on', nextOn);
        if (mapEl._map) {
          syncLayersToLeafletMap(mapEl._map, defs);
        }
      });
      chipsHost.appendChild(chip);
    });

    if (mapEl._map) {
      syncLayersToLeafletMap(mapEl._map, defs);
    }

    if (st.envMapObserver) {
      try { st.envMapObserver.disconnect(); } catch (e) {}
      st.envMapObserver = null;
    }
    if (typeof MutationObserver === 'function') {
      st.envMapObserver = new MutationObserver(function () {
        if (mapEl._map && st.mountedMapInstance !== mapEl._map) {
          syncLayersToLeafletMap(mapEl._map, defs);
        }
      });
      st.envMapObserver.observe(mapEl, { childList: true });
    }
  }

  function wrapMapExpandRegister() {
    if (!window.OpenEstateMapExpand || typeof window.OpenEstateMapExpand.register !== 'function') return;
    if (window.OpenEstateMapExpand.__oeNoiseWrapped) return;
    var origReg = window.OpenEstateMapExpand.register;
    window.OpenEstateMapExpand.register = function (id, spec) {
      if (id === 'envMap' && spec && Array.isArray(spec.layers)) {
        try {
          var rep = getReport();
          var envData = getModData(rep, 'environment');
          var nz = envData && envData.noise;
          var defs = buildOverlayDefs(envData, nz);
          var existingLabels = Object.create(null);
          spec.layers.forEach(function (l) { if (l && l.label) existingLabels[l.label] = 1; });
          defs.forEach(function (d) {
            if (!existingLabels[d.label]) {
              spec.layers.push({
                key: d.key,
                label: d.label,
                color: d.color,
                count: d.count,
                on: st.activeFlags[d.key] !== undefined ? st.activeFlags[d.key] : d.defaultOn,
                make: d.make
              });
            }
          });
        } catch (e) {}
      }
      return origReg.call(window.OpenEstateMapExpand, id, spec);
    };
    window.OpenEstateMapExpand.__oeNoiseWrapped = true;
  }

  function enhanceEnvironmentSection() {
    if (st.mutatingDom) return;
    var envEl = document.getElementById('environment');
    if (!envEl) return;
    var rep = getReport();
    if (!rep) return;
    var envData = getModData(rep, 'environment');
    if (!envData) return;
    var nz = envData.noise;
    if (!nz) return;

    injectCss();
    wrapMapExpandRegister();

    st.mutatingDom = true;
    try {
      renderExtensionPanel(envEl, nz);
      enhanceEnvMapAndChips(envData, nz);
    } finally {
      st.mutatingDom = false;
    }
  }

  function bindObservers() {
    enhanceEnvironmentSection();

    var envEl = document.getElementById('environment');
    if (envEl && typeof MutationObserver === 'function') {
      var envMo = new MutationObserver(function (records) {
        if (st.mutatingDom) return;
        var externalChange = records.some(function (rec) {
          if (rec.target && (rec.target.id === PANEL_ID || (rec.target.closest && rec.target.closest('#' + PANEL_ID)))) {
            return false;
          }
          if (rec.target && rec.target.id === 'envChips') {
            for (var i = 0; i < rec.addedNodes.length; i++) {
              var n = rec.addedNodes[i];
              if (n && n.classList && n.classList.contains('oe-nz-chip')) return false;
            }
          }
          return true;
        });
        if (externalChange) {
          enhanceEnvironmentSection();
        }
      });
      envMo.observe(envEl, { childList: true });
    }

    if (typeof MutationObserver === 'function' && document.documentElement) {
      var langMo = new MutationObserver(function () {
        enhanceEnvironmentSection();
      });
      langMo.observe(document.documentElement, {
        attributes: true,
        attributeFilter: ['lang', 'data-display-links']
      });
    }
  }

  window.OpenEstateNoiseWidget = {
    refresh: enhanceEnvironmentSection
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bindObservers);
  } else {
    bindObservers();
  }
})();
