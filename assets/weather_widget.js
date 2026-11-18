/* Open Estate Weather Widget — live NOAA/NWS forecast, alerts, station observation & regional Leaflet radar/weather map.
 *
 * Self-contained standalone module for `<section id="weather" class="sec"></section>`.
 * Theme-native via report CSS custom properties (`--surface`, `--surface-2`, `--soft`, `--ink`, `--ink2`,
 * `--muted`, `--line`, `--line-strong`, `--brand`, `--brand-solid`, `--goodbg`, `--watchbg`, `--badbg`).
 * Never fabricates weather data when offline or when upstream requests fail.
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_WEATHER_INIT__) return;
  window.__OPEN_ESTATE_WEATHER_INIT__ = true;

  var SECTION_ID = 'weather';
  var MAP_ID = 'weatherMap';
  var CHIPS_ID = 'weatherChips';
  var STALE_MS = 30 * 60 * 1000;
  var REFRESH_COOLDOWN_MS = 30 * 1000;
  var RADAR_FRAMES = ['-m50m', '-m45m', '-m40m', '-m35m', '-m30m', '-m25m', '-m20m', '-m15m', '-m10m', '-m05m', ''];

  var st = {
    sec: null,
    rep: null,
    lat: null,
    lon: null,
    wx: null,
    fetching: false,
    fetchErr: null,
    lastFetchAtMs: 0,
    showFull7d: false,
    radarFrameIdx: RADAR_FRAMES.length - 1,
    radarPlaying: false,
    radarTimer: null,
    radarTileLayer: null,
    mapMounted: false,
    mountedStation: null,
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
      if (typeof LANG === 'string' && LANG) return LANG === 'zh' ? 'zh' : LANG === 'es' ? 'es' : 'en';
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

  function getModData(rep, key) {
    try {
      if (typeof D === 'function') return D(key);
    } catch (e) {}
    if (window.OpenEstateCore && typeof window.OpenEstateCore.mod === 'function') {
      return window.OpenEstateCore.mod(rep, key);
    }
    var m = rep && rep.modules && rep.modules[key];
    return (m && m.data) || (rep && rep[key]) || null;
  }

  function isUsCountry(country) {
    var c = String(country || '').trim().toUpperCase();
    return !c || c === 'US' || c === 'USA' || c === 'UNITED STATES' || c === 'UNITED STATES OF AMERICA';
  }

  function areRefLinksVisible() {
    try {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.isDisplayLinksEnabled === 'function') {
        return Boolean(window.OpenEstateCore.isDisplayLinksEnabled());
      }
      if (document.documentElement && document.documentElement.dataset) {
        if (document.documentElement.dataset.displayLinks === '1') return true;
        if (document.documentElement.dataset.displayLinks === '0') return false;
      }
    } catch (e) {}
    return false;
  }

  function wxRefChipHtml(rawUrl, label) {
    var href = rawUrl;
    try {
      if (typeof window.toReferenceRedirectUrl === 'function') {
        href = window.toReferenceRedirectUrl(rawUrl, { section: 'weather', name: label }) || rawUrl;
      }
    } catch (e) {}
    return '<a class="wx-src-chip src-lnk" href="' + escH(href) + '" data-direct-url="' + escH(rawUrl) + '" target="_blank" rel="noopener noreferrer">' + escH(label) + ' ↗</a>';
  }

  function injectCss() {
    if (document.getElementById('oeWeatherCss')) return;
    var css = [
      '#weather[hidden]{display:none!important}',
      '#weather .wx-hdr-meta{display:inline-flex;align-items:center;gap:8px;flex-wrap:wrap;justify-content:flex-end}',
      '#weather .wx-btn{display:inline-flex;align-items:center;gap:5px;border:1px solid var(--line-strong,var(--line));background:var(--surface-2,var(--soft));color:var(--ink2);border-radius:999px;padding:4px 10px;font:700 11.5px var(--font);cursor:pointer;transition:border-color .15s,background .15s,color .15s}',
      '#weather .wx-btn:hover:not(:disabled){border-color:var(--brand);color:var(--brand);background:var(--brandsoft)}',
      '#weather .wx-btn:disabled{opacity:.6;cursor:wait}',
      '#weather .wx-btn.on{background:var(--brand-solid);border-color:var(--brand-solid);color:var(--chat-msg-u-fg,var(--surface))}',
      '#weather .wx-alerts{display:flex;flex-direction:column;gap:8px;margin:10px 0 12px}',
      '#weather .wx-alert-card{border-radius:12px;padding:10px 13px;border:1px solid var(--line);display:flex;flex-direction:column;gap:4px;font-size:13px;line-height:1.45}',
      '#weather .wx-alert-card.bad{background:var(--badbg);border-color:var(--bad);color:var(--ink)}',
      '#weather .wx-alert-card.watch{background:var(--watchbg);border-color:var(--watch);color:var(--ink)}',
      '#weather .wx-alert-top{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;font-weight:800}',
      '#weather .wx-alert-meta{font-size:11.5px;color:var(--muted);font-weight:600}',
      '#weather .wx-risks{display:flex;flex-wrap:wrap;gap:6px;margin:8px 0 12px}',
      '#weather .wx-obs-tiles{margin:10px 0 14px}',
      '#weather .wx-obs-tiles .tile{background:var(--soft);border-color:var(--line)}',
      '#weather .wx-obs-tiles .tile .v{color:var(--ink)}',
      '#weather .wx-map-bar{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;margin:14px 0 8px}',
      '#weather .wx-radar-ctrl{display:inline-flex;align-items:center;gap:5px;flex-wrap:wrap;background:var(--soft);border:1px solid var(--line);border-radius:999px;padding:3px 6px}',
      '#weather .wx-radar-lbl{font-size:11.5px;font-weight:700;color:var(--ink2);min-width:44px;text-align:center;font-variant-numeric:tabular-nums}',
      '#weather #weatherChips .chip{background:var(--surface-2,var(--soft));border-color:var(--line-strong,var(--line));color:var(--ink2)}',
      '#weather #weatherChips .chip.on{background:var(--brand-solid);border-color:var(--brand-solid);color:var(--chat-msg-u-fg,var(--surface))}',
      '#weather #weatherMap{height:390px;background:var(--surface-2,var(--soft));border-color:var(--line)}',
      '#weather .wx-fc-hdr{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;margin:16px 0 8px}',
      '#weather .wx-periods{display:grid;grid-template-columns:repeat(auto-fit,minmax(145px,1fr));gap:9px}',
      '#weather .wx-pcard{background:var(--soft);border:1px solid var(--line);border-radius:12px;padding:10px 12px;display:flex;flex-direction:column;gap:4px;min-width:0}',
      '#weather .wx-pcard.day{background:var(--surface-2,var(--soft))}',
      '#weather .wx-pcard-top{display:flex;align-items:baseline;justify-content:space-between;gap:6px}',
      '#weather .wx-pname{font-size:12px;font-weight:800;color:var(--ink2);line-height:1.25}',
      '#weather .wx-ptemp{font-size:19px;font-weight:800;color:var(--brand);letter-spacing:-.4px;font-variant-numeric:tabular-nums;white-space:nowrap}',
      '#weather .wx-pdesc{font-size:12.5px;font-weight:600;color:var(--ink);line-height:1.35}',
      '#weather .wx-pmeta{font-size:11.3px;color:var(--muted);line-height:1.35;margin-top:auto}',
      '#weather .wx-src-bar{display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin-top:14px;padding-top:10px;border-top:1px solid var(--line);font-size:12px;color:var(--muted)}',
      '#weather .wx-src-chip{display:inline-flex;align-items:center;gap:4px;padding:3px 9px;border-radius:999px;border:1px solid var(--line);background:var(--surface-2,var(--soft));color:var(--ink2);font-weight:600;font-size:11.5px;text-decoration:none}',
      '#weather .wx-src-chip:hover{border-color:var(--brand);color:var(--brand);background:var(--brandsoft);text-decoration:none}'
    ].join('\n');
    var style = document.createElement('style');
    style.id = 'oeWeatherCss';
    style.textContent = css;
    (document.head || document.documentElement).appendChild(style);
  }

  function compass8(deg) {
    var dirs = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'];
    return dirs[Math.round((((deg % 360) + 360) % 360) / 45) % 8];
  }

  function formatRelAge(isoStr) {
    if (!isoStr) return '';
    var ts = Date.parse(isoStr);
    if (!isFinite(ts)) return '';
    var mins = Math.max(0, Math.round((Date.now() - ts) / 60000));
    if (mins < 1) return P('just now', '刚刚', 'ahora');
    if (mins < 60) return P(mins + 'm ago', mins + ' 分钟前', 'hace ' + mins + 'm');
    var hrs = Math.round(mins / 60);
    return P(hrs + 'h ago', hrs + ' 小时前', 'hace ' + hrs + 'h');
  }

  function isWxStale(wx) {
    if (!wx || !wx.fetchedAt) return true;
    var ts = Date.parse(wx.fetchedAt);
    if (!isFinite(ts)) return true;
    return (Date.now() - ts) > STALE_MS;
  }

  function maxWindFromText(txt) {
    var m = String(txt || '').match(/\d+(?:\.\d+)?/g);
    if (!m || !m.length) return null;
    var best = 0;
    for (var i = 0; i < m.length; i++) {
      var v = parseFloat(m[i]);
      if (isFinite(v) && v > best) best = v;
    }
    return best > 0 ? best : null;
  }

  function deriveRiskWindowsClient(periods) {
    var out = [];
    var seen = Object.create(null);
    function push(kind, period, summary) {
      if (out.length >= 6) return;
      var k = kind + '|' + period;
      if (seen[k]) return;
      seen[k] = 1;
      out.push({ kind: kind, period: period, summary: summary });
    }
    for (var i = 0; i < (periods || []).length; i++) {
      var p = periods[i] || {};
      var name = String(p.name || '').trim();
      if (!name) continue;
      var sf = String(p.shortForecast || '').trim();
      var df = String(p.detailedForecast || '').trim();
      var hay = (sf + ' ' + df).toLowerCase();
      var kw = null;
      if (hay.indexOf('tornado') >= 0) kw = 'Tornado risk';
      else if (hay.indexOf('hurricane') >= 0 || hay.indexOf('tropical storm') >= 0) kw = 'Tropical storm risk';
      else if (hay.indexOf('red flag') >= 0 || hay.indexOf('fire weather') >= 0) kw = 'Fire weather / Red Flag';
      else if (hay.indexOf('blizzard') >= 0 || hay.indexOf('ice storm') >= 0) kw = 'Severe winter storm';
      else if (hay.indexOf('flash flood') >= 0 || hay.indexOf('flood') >= 0) kw = 'Flood potential';
      else if (hay.indexOf('thunder') >= 0 || hay.indexOf('t-storm') >= 0 || hay.indexOf('severe') >= 0) kw = 'Storm / thunder risk';
      if (kw) push('storm', name, kw + ' (' + name + ': ' + sf + ')');

      if (isNum(p.temperature)) {
        var tf = String(p.temperatureUnit || 'F').toUpperCase() === 'C' ? (p.temperature * 9 / 5 + 32) : p.temperature;
        if (tf >= 95) push('heat', name, 'Extreme heat ' + Math.round(tf) + '°F (' + name + ')');
        else if (tf <= 32) push('freeze', name, 'Freeze risk ' + Math.round(tf) + '°F (' + name + ')');
      }
      if (isNum(p.precipPct) && p.precipPct >= 60) {
        push('precip', name, 'High precip chance ' + p.precipPct + '% (' + name + ': ' + sf + ')');
      }
      var maxW = maxWindFromText(p.windSpeed);
      if (isNum(maxW) && maxW >= 25) {
        push('wind', name, 'High wind up to ' + Math.round(maxW) + ' mph (' + name + ')');
      }
    }
    return out;
  }

  function parseClientObservation(obsJson, stationId, stationName) {
    var props = obsJson && obsJson.properties;
    if (!props) return null;
    var tVal = props.temperature && props.temperature.value;
    var tUnit = String((props.temperature && props.temperature.unitCode) || '').toLowerCase();
    var tempF = isNum(tVal) ? Math.round(tUnit.indexOf('degf') >= 0 ? tVal : (tVal * 9 / 5 + 32)) : null;

    var rhVal = props.relativeHumidity && props.relativeHumidity.value;
    var rhPct = isNum(rhVal) && rhVal >= 0 && rhVal <= 100 ? Math.round(rhVal) : null;

    var wVal = props.windSpeed && props.windSpeed.value;
    var wUnit = String((props.windSpeed && props.windSpeed.unitCode) || '').toLowerCase();
    var windMph = null;
    if (isNum(wVal) && wVal >= 0) {
      if (wUnit.indexOf('mi_h') >= 0 || wUnit.indexOf('mph') >= 0) windMph = Math.round(wVal);
      else if (wUnit.indexOf('m_s') >= 0) windMph = Math.round(wVal * 2.236936);
      else if (wUnit.indexOf('kt') >= 0 || wUnit.indexOf('knot') >= 0) windMph = Math.round(wVal * 1.150779);
      else windMph = Math.round(wVal * 0.621371);
    }

    var wdVal = props.windDirection && props.windDirection.value;
    var windDir = isNum(wdVal) ? compass8(wdVal) : null;
    var desc = String(props.textDescription || '').trim() || null;
    var ts = String(props.timestamp || '').trim() || null;
    if (tempF == null && rhPct == null && windMph == null && !desc) return null;
    return {
      stationId: stationId,
      stationName: stationName || null,
      tempF: tempF,
      relativeHumidityPct: rhPct,
      windMph: windMph,
      windDirection: windDir,
      textDescription: desc,
      observedAt: ts
    };
  }

  function fetchJsonNoUa(url, signal) {
    return fetch(url, {
      headers: { Accept: 'application/geo+json' },
      signal: signal
    }).then(function (r) {
      if (!r.ok) throw new Error('NWS HTTP ' + r.status);
      return r.json();
    });
  }

  function fetchLiveNws(lat, lon, manual) {
    if (typeof fetch !== 'function' || location.protocol === 'file:') return Promise.resolve(null);
    if (st.fetching) return Promise.resolve(null);
    var now = Date.now();
    if (manual && (now - st.lastFetchAtMs) < REFRESH_COOLDOWN_MS) return Promise.resolve(null);

    st.fetching = true;
    st.fetchErr = null;
    renderLivePanels();

    var hadStation = Boolean(st.wx && st.wx.radarStation);
    var ctrl = typeof AbortController === 'function' ? new AbortController() : null;
    var timer = ctrl ? setTimeout(function () { try { ctrl.abort(); } catch (e) {} }, 9000) : null;
    var sig = ctrl ? ctrl.signal : undefined;

    var la = Number(lat).toFixed(4);
    var lo = Number(lon).toFixed(4);
    var pointsUrl = 'https://api.weather.gov/points/' + la + ',' + lo;

    return fetchJsonNoUa(pointsUrl, sig)
      .then(function (pts) {
        var props = (pts && pts.properties) || {};
        var fcUrl = props.forecast;
        var stnsUrl = props.observationStations;
        var alUrl = 'https://api.weather.gov/alerts/active?point=' + la + ',' + lo;

        var fcP = fcUrl ? fetchJsonNoUa(fcUrl, sig).catch(function () { return null; }) : Promise.resolve(null);
        var alP = fetchJsonNoUa(alUrl, sig).catch(function () { return null; });
        var obsP = stnsUrl
          ? fetchJsonNoUa(stnsUrl, sig).then(function (stns) {
              var feats = (stns && stns.features) || [];
              function tryStation(idx) {
                if (idx >= Math.min(2, feats.length)) return Promise.resolve(null);
                var f = feats[idx] || {};
                var fp = f.properties || {};
                var sid = fp.stationIdentifier || String(f.id || '').split('/').pop() || '';
                if (!sid) return tryStation(idx + 1);
                var sUrl = (f.id && String(f.id).indexOf('http') === 0 ? f.id : ('https://api.weather.gov/stations/' + encodeURIComponent(sid))) + '/observations/latest';
                return fetchJsonNoUa(sUrl, sig).then(function (oj) {
                  var parsed = parseClientObservation(oj, sid, fp.name);
                  return parsed || tryStation(idx + 1);
                }).catch(function () { return tryStation(idx + 1); });
              }
              return tryStation(0);
            }).catch(function () { return null; })
          : Promise.resolve(null);

        return Promise.all([fcP, alP, obsP]).then(function (res) {
          var fcJson = res[0], alJson = res[1], obs = res[2];
          var rawPeriods = (fcJson && fcJson.properties && fcJson.properties.periods) || [];
          var periods = rawPeriods.slice(0, 14).map(function (p) {
            var pop = p.probabilityOfPrecipitation && p.probabilityOfPrecipitation.value;
            return {
              name: p.name,
              temperature: isNum(p.temperature) ? Math.round(p.temperature) : null,
              temperatureUnit: p.temperatureUnit || 'F',
              shortForecast: p.shortForecast || '',
              detailedForecast: p.detailedForecast || null,
              windSpeed: p.windSpeed || null,
              windDirection: p.windDirection || null,
              precipPct: isNum(pop) ? Math.round(pop) : null,
              isDaytime: typeof p.isDaytime === 'boolean' ? p.isDaytime : null,
              startTime: p.startTime || null
            };
          });
          var rawAlerts = (alJson && alJson.features) || [];
          var alerts = rawAlerts.slice(0, 10).map(function (f) {
            var ap = (f && f.properties) || {};
            return {
              id: ap.id || null,
              event: ap.event || '',
              severity: ap.severity || null,
              urgency: ap.urgency || null,
              certainty: ap.certainty || null,
              headline: ap.headline || null,
              description: ap.description ? String(ap.description).slice(0, 360) : null,
              effective: ap.effective || null,
              expires: ap.expires || null,
              senderName: ap.senderName || null
            };
          }).filter(function (a) { return a.event; });

          if (!periods.length && !obs && !alJson) {
            return null;
          }
          var fz = props.forecastZone ? String(props.forecastZone).split('/').pop() : null;
          var cz = props.county ? String(props.county).split('/').pop() : null;
          var rStn = props.radarStation ? String(props.radarStation).trim().toUpperCase() : null;
          var nextWx = {
            office: props.cwa || props.gridId || null,
            gridId: props.gridId || null,
            gridX: isNum(props.gridX) ? props.gridX : null,
            gridY: isNum(props.gridY) ? props.gridY : null,
            radarStation: rStn,
            forecastZone: fz,
            countyZone: cz,
            fetchedAt: new Date().toISOString(),
            forecastUrl: 'https://forecast.weather.gov/MapClick.php?lat=' + la + '&lon=' + lo,
            observation: obs,
            activeAlerts: alerts,
            riskWindows: deriveRiskWindowsClient(periods),
            periods: periods
          };
          st.wx = nextWx;
          st.lastFetchAtMs = Date.now();
          if (st.rep && st.rep.modules && st.rep.modules.hazards && st.rep.modules.hazards.data) {
            st.rep.modules.hazards.data.weather = nextWx;
          }
          return nextWx;
        });
      })
      .catch(function (err) {
        st.fetchErr = (err && err.message) || 'NWS fetch failed';
        return null;
      })
      .then(function (updated) {
        if (timer) clearTimeout(timer);
        st.fetching = false;
        renderLivePanels();
        if (updated && !hadStation && updated.radarStation) {
          var mapEl = document.getElementById(MAP_ID);
          if (mapEl && !mapEl._map) {
            mountMap();
          }
        }
        return updated;
      });
  }

  function radarUrlForFrame(frame) {
    return 'https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/nexrad-n0q-900913' + (frame || '') + '/{z}/{x}/{y}.png';
  }

  function makeArcgisExport(url, layers, opts) {
    if (typeof arcgisExport === 'function') {
      return arcgisExport(url, layers, opts);
    }
    var hiDpi = typeof window !== 'undefined' && (window.devicePixelRatio || 1) > 1;
    var px = hiDpi ? 512 : 256, dpi = hiDpi ? 192 : 96;
    var Layer = window.L.TileLayer.extend({
      getTileUrl: function (c) {
        var z = this._getZoomForUrl(), n = Math.pow(2, z), o = Math.PI * 6378137, res = (2 * o) / (256 * n);
        var x0 = -o + c.x * 256 * res, y1 = o - c.y * 256 * res;
        return url + '/export?bbox=' + x0 + ',' + (y1 - 256 * res) + ',' + (x0 + 256 * res) + ',' + y1 +
          '&bboxSR=3857&imageSR=3857&size=' + px + ',' + px + '&dpi=' + dpi +
          '&format=png32&transparent=true&layers=show:' + layers + '&f=image';
      }
    });
    return new Layer('', Object.assign({ opacity: 0.5, tileSize: 256, maxZoom: 21, maxNativeZoom: 12 }, opts || {}));
  }

  function buildOverlayDefs() {
    var wx = st.wx;
    var lon = Number(st.lon);
    var goesLayer = lon < -100 ? 'goes_west' : 'goes_east';
    var defs = [
      {
        key: 'nexrad',
        label: P('Live NEXRAD Radar', '实时 NEXRAD 气象雷达', 'Radar NEXRAD en vivo'),
        color: '#16a34a',
        on: true,
        make: function (opts) {
          var lyr = window.L.tileLayer(radarUrlForFrame(RADAR_FRAMES[st.radarFrameIdx]), {
            opacity: 0.65,
            maxZoom: 18,
            maxNativeZoom: 12,
            attribution: 'NEXRAD: Iowa State Mesonet / NOAA'
          });
          if (!opts || !opts.forModal) st.radarTileLayer = lyr;
          return lyr;
        }
      },
      {
        key: 'wwa',
        label: P('NWS Watches & Warnings', 'NWS 气象警报图层', 'Alertas y avisos NWS'),
        color: '#dc2626',
        on: true,
        make: function () {
          return window.L.tileLayer.wms('https://opengeo.ncep.noaa.gov/geoserver/wwa/hazards/ows', {
            layers: 'hazards',
            format: 'image/png',
            transparent: true,
            opacity: 0.52,
            attribution: 'NOAA/NWS Watches, Warnings & Advisories'
          });
        }
      }
    ];

    var rStn = wx && wx.radarStation ? String(wx.radarStation).trim().toUpperCase() : '';
    if (/^[A-Z0-9]{4}$/.test(rStn)) {
      var stnLc = rStn.toLowerCase();
      defs.push({
        key: 'local-radar',
        label: P('Local Site Radar (' + rStn + ')', '本地站点雷达 (' + rStn + ')', 'Radar local (' + rStn + ')'),
        color: '#0284c7',
        on: false,
        make: function () {
          return window.L.tileLayer.wms('https://opengeo.ncep.noaa.gov/geoserver/' + stnLc + '/' + stnLc + '_sr_bref/ows', {
            layers: stnLc + '_sr_bref',
            format: 'image/png',
            transparent: true,
            opacity: 0.65,
            attribution: 'NOAA NEXRAD ' + rStn
          });
        }
      });
    }

    defs.push(
      {
        key: 'qpf',
        label: P('24h Forecast Precip (QPF)', '未来24小时降水预报', 'Precipitación 24h (QPF)'),
        color: '#2563eb',
        on: false,
        make: function () {
          return makeArcgisExport('https://mapservices.weather.noaa.gov/vector/rest/services/precip/wpc_qpf/MapServer', '1', {
            opacity: 0.5,
            maxNativeZoom: 12,
            attribution: 'NOAA WPC QPF'
          });
        }
      },
      {
        key: 'temp',
        label: P('Forecast Temp (°F, NDFD)', '预报气温 (°F, NDFD)', 'Temp. pronóstico (°F, NDFD)'),
        color: '#d97706',
        on: false,
        make: function () {
          return makeArcgisExport('https://mapservices.weather.noaa.gov/raster/rest/services/NDFD/NDFD_temp/MapServer', '0', {
            opacity: 0.45,
            maxNativeZoom: 12,
            attribution: 'NOAA NDFD'
          });
        }
      },
      {
        key: 'goes',
        label: P('GOES Satellite Cloud/IR', 'GOES 卫星云图', 'Satélite GOES nubes/IR'),
        color: '#6d28d9',
        on: false,
        make: function () {
          return window.L.tileLayer('https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/' + goesLayer + '/{z}/{x}/{y}.png', {
            opacity: 0.45,
            maxZoom: 18,
            maxNativeZoom: 10,
            attribution: 'GOES Satellite: NOAA / IEM'
          });
        }
      }
    );
    return defs;
  }

  function frameLabel(frame) {
    return frame ? frame.replace(/^-m/, '-') : P('Live', '实时', 'Vivo');
  }

  function syncRadarUi() {
    var playBtn = document.getElementById('wxRadarPlay');
    var lbl = document.getElementById('wxRadarLbl');
    if (playBtn) {
      playBtn.classList.toggle('on', st.radarPlaying);
      playBtn.textContent = st.radarPlaying
        ? P('⏸ Pause 50m', '⏸ 暂停回放', '⏸ Pausa')
        : P('▶ Loop 50m', '▶ 50分钟雷达回放', '▶ Bucle 50m');
    }
    if (lbl) {
      lbl.textContent = frameLabel(RADAR_FRAMES[st.radarFrameIdx]);
    }
  }

  function setRadarFrame(idx) {
    var len = RADAR_FRAMES.length;
    st.radarFrameIdx = ((idx % len) + len) % len;
    if (st.radarTileLayer && typeof st.radarTileLayer.setUrl === 'function') {
      st.radarTileLayer.setUrl(radarUrlForFrame(RADAR_FRAMES[st.radarFrameIdx]));
    }
    syncRadarUi();
  }

  function stopRadarLoop() {
    if (st.radarTimer) {
      clearInterval(st.radarTimer);
      st.radarTimer = null;
    }
    st.radarPlaying = false;
    syncRadarUi();
  }

  function ensureNexradChipOn() {
    var chip = document.querySelector('#' + CHIPS_ID + ' .chip[data-i="0"]');
    if (chip && !chip.classList.contains('on') && typeof chip.click === 'function') {
      chip.click();
    }
  }

  function toggleRadarLoop() {
    if (st.radarPlaying) {
      stopRadarLoop();
      return;
    }
    ensureNexradChipOn();
    st.radarPlaying = true;
    syncRadarUi();
    st.radarTimer = setInterval(function () {
      if (!document.body.contains(document.getElementById(MAP_ID))) {
        stopRadarLoop();
        return;
      }
      setRadarFrame(st.radarFrameIdx + 1);
    }, 650);
  }

  function mountMap() {
    if (typeof window.L === 'undefined' || typeof sectionMap !== 'function') return;
    var mapEl = document.getElementById(MAP_ID);
    if (!mapEl) return;
    st.radarTileLayer = null;
    var defs = buildOverlayDefs();
    sectionMap(MAP_ID, CHIPS_ID, {
      base: 'light',
      zoom: 9,
      layers: defs
    });
    st.mapMounted = true;
    st.mountedStation = (st.wx && st.wx.radarStation) || null;
  }

  function refreshChipLabels() {
    var defs = buildOverlayDefs();
    var chHost = document.getElementById(CHIPS_ID);
    if (!chHost) return;
    var chips = chHost.querySelectorAll('.chip[data-i]');
    if (chips.length !== defs.length) return;
    chips.forEach(function (ch) {
      var idx = Number(ch.getAttribute('data-i'));
      var def = defs[idx];
      if (!def) return;
      var dot = ch.querySelector('i');
      ch.textContent = '';
      if (dot) ch.appendChild(dot);
      ch.appendChild(document.createTextNode(def.label));
    });
  }

  function renderHeader() {
    var hdrEl = document.getElementById('wxHeader');
    if (!hdrEl) return;
    var wx = st.wx;
    var parts = [];
    if (wx && wx.office) parts.push('NWS ' + wx.office);
    if (wx && wx.radarStation) parts.push(P('Radar ', '雷达 ', 'Radar ') + wx.radarStation);
    var age = wx && wx.fetchedAt ? formatRelAge(wx.fetchedAt) : '';
    if (age) parts.push(P('Updated ', '更新于 ', 'Actualizado ') + age);
    if (!parts.length) parts.push('NOAA / api.weather.gov');

    var canLive = location.protocol !== 'file:';
    var btnHtml = canLive
      ? '<button type="button" class="wx-btn" id="wxRefreshBtn"' + (st.fetching ? ' disabled' : '') + '>' +
        escH(st.fetching
          ? P('Refreshing…', '刷新中…', 'Actualizando…')
          : P('↻ Live NWS', '↻ 刷新实时气象', '↻ NWS en vivo')) +
        '</button>'
      : '';

    hdrEl.innerHTML =
      '<div class="eyebrow">' + escH(P('Live Weather & Radar', '实时气象与雷达', 'Clima y radar en vivo')) + '</div>' +
      '<div class="stitle">' +
        '<h2>' + escH(P('Regional Weather, Alerts & Radar', '实时气象预警与区域雷达图', 'Clima regional, alertas y radar')) + '</h2>' +
        '<div class="meta wx-hdr-meta"><span>' + escH(parts.join(' · ')) + '</span>' + btnHtml + '</div>' +
      '</div>';

    var btn = document.getElementById('wxRefreshBtn');
    if (btn) {
      btn.addEventListener('click', function () {
        fetchLiveNws(st.lat, st.lon, true);
      });
    }
  }

  function renderLivePanels() {
    renderHeader();
    var topBox = document.getElementById('wxTopPanels');
    var botBox = document.getElementById('wxBottomPanels');
    var mapHdr = document.getElementById('wxMapHdr');
    if (!topBox || !botBox) return;

    var wx = st.wx;
    var obs = wx && wx.observation;
    var alerts = (wx && wx.activeAlerts) || [];
    var rws = (wx && wx.riskWindows) || [];
    var allPeriods = (wx && wx.periods) || [];
    var shownPeriods = st.showFull7d ? allPeriods : allPeriods.slice(0, 6);

    var topHtml = '';
    if (wx) {
      if (alerts.length) {
        topHtml += '<div class="wx-alerts">' + alerts.map(function (a) {
          var sev = String(a.severity || '').toLowerCase();
          var cls = (sev === 'extreme' || sev === 'severe') ? 'bad' : 'watch';
          var exp = a.expires ? (P('Expires: ', '截止：', 'Expira: ') + a.expires) : '';
          return '<div class="wx-alert-card ' + cls + '">' +
            '<div class="wx-alert-top">' +
              '<span>⚠ ' + escH(a.event) + (a.severity ? ' (' + escH(a.severity) + ')' : '') + '</span>' +
              (exp ? '<span class="wx-alert-meta">' + escH(exp) + '</span>' : '') +
            '</div>' +
            (a.headline ? '<div class="small" style="color:var(--ink2)">' + escH(a.headline) + '</div>' : '') +
            '</div>';
        }).join('') + '</div>';
      } else {
        topHtml += '<div class="wx-risks"><span class="pill good">' +
          escH(P('✓ 0 active NWS weather alerts', '✓ 当前无 NWS 生效气象预警', '✓ 0 alertas NWS activas')) +
          '</span>' +
          (rws.length
            ? rws.slice(0, 4).map(function (w) { return '<span class="pill watch">◆ ' + escH(w.summary) + '</span>'; }).join('')
            : '<span class="pill mute">' + escH(P('No extreme heat, freeze, wind or storm windows in 7-day forecast', '未来7天预报无极端高温、冰冻、大风或风暴窗口', 'Sin ventanas extremas en pronóstico de 7 días')) + '</span>') +
          '</div>';
      }
      if (alerts.length && rws.length) {
        topHtml += '<div class="wx-risks">' + rws.slice(0, 4).map(function (w) {
          return '<span class="pill watch">◆ ' + escH(w.summary) + '</span>';
        }).join('') + '</div>';
      }

      if (obs) {
        var obsAge = obs.observedAt ? formatRelAge(obs.observedAt) : '';
        var windVal = obs.windMph != null
          ? (obs.windMph + ' mph' + (obs.windDirection ? ' ' + obs.windDirection : ''))
          : '—';
        topHtml += '<div class="tiles wx-obs-tiles">' +
          '<div class="tile"><div class="k">' + escH(P('Current Conditions', '当前实况温度', 'Condiciones actuales')) + '</div>' +
            '<div class="v">' + (obs.tempF != null ? escH(obs.tempF + '°F') : '—') + '</div>' +
            '<div class="d">' + escH(obs.textDescription || P('Station observation', '气象站实测', 'Observación')) + '</div></div>' +
          '<div class="tile"><div class="k">' + escH(P('Wind', '实时风速', 'Viento')) + '</div>' +
            '<div class="v">' + escH(windVal) + '</div>' +
            '<div class="d">' + escH(obsAge || P('Latest METAR', '最新地面观测', 'Último METAR')) + '</div></div>' +
          '<div class="tile"><div class="k">' + escH(P('Relative Humidity', '相对湿度', 'Humedad relativa')) + '</div>' +
            '<div class="v">' + (obs.relativeHumidityPct != null ? escH(obs.relativeHumidityPct + '%') : '—') + '</div>' +
            '<div class="d">' + escH(wx.forecastZone ? P('Zone ', '预报区 ', 'Zona ') + wx.forecastZone : '') + '</div></div>' +
          '<div class="tile"><div class="k">' + escH(P('Station & Radar', '气象站与雷达站', 'Estación y radar')) + '</div>' +
            '<div class="v">' + escH(obs.stationId || wx.radarStation || '—') + '</div>' +
            '<div class="d">' + escH([obs.stationName, wx.radarStation ? ('NEXRAD ' + wx.radarStation) : ''].filter(Boolean).join(' · ')) + '</div></div>' +
          '</div>';
      }
    } else if (st.fetching) {
      topHtml += '<div class="hint">' + escH(P('Loading live NOAA / NWS forecast, alerts & nearest observation…', '正在从 NOAA / NWS 加载实时预报、预警与观测站实况…', 'Cargando pronóstico, alertas y observación de NOAA / NWS…')) + '</div>';
    } else if (st.fetchErr) {
      topHtml += '<div class="empty">' + escH(P('NOAA / NWS point feed unavailable right now; live regional radar map layers below remain active.', 'NOAA / NWS 站点预报暂不可用；下方实时区域雷达图层仍可正常查看。', 'Fuente puntual NOAA / NWS no disponible ahora; las capas del mapa radar siguen activas.')) + '</div>';
    } else {
      topHtml += '<div class="hint">' + escH(P('Regional radar and NOAA weather overlays are active below.', '下方已加载实时区域气象雷达与 NOAA 预警图层。', 'El radar regional y las capas meteorológicas NOAA están activos abajo.')) + '</div>';
    }
    topBox.innerHTML = topHtml;

    if (mapHdr) {
      mapHdr.innerHTML =
        '<div class="sub" style="margin:0">' +
          escH(P('Regional Weather & Radar Map', '区域气象雷达与预警地图', 'Mapa regional de clima y radar')) +
          ' <small>' + escH(P('click chips to toggle overlays · zoom out for CONUS', '点击标签切换图层 · 缩小可查看全美态势', 'clic para activar capas · aleje para vista nacional')) + '</small>' +
        '</div>' +
        '<div class="wx-radar-ctrl" role="group" aria-label="' + escH(P('Radar loop controls', '雷达回放控制', 'Controles de radar')) + '">' +
          '<button type="button" class="wx-btn" id="wxRadarPrev" title="' + escH(P('Previous 5m frame', '上一帧 (-5分钟)', 'Cuadro anterior')) + '">◀</button>' +
          '<button type="button" class="wx-btn' + (st.radarPlaying ? ' on' : '') + '" id="wxRadarPlay"></button>' +
          '<button type="button" class="wx-btn" id="wxRadarNext" title="' + escH(P('Next 5m frame', '下一帧 (+5分钟)', 'Cuadro siguiente')) + '">▶</button>' +
          '<span class="wx-radar-lbl" id="wxRadarLbl"></span>' +
        '</div>';
      syncRadarUi();
      var prevB = document.getElementById('wxRadarPrev');
      var playB = document.getElementById('wxRadarPlay');
      var nextB = document.getElementById('wxRadarNext');
      if (prevB) prevB.addEventListener('click', function () { stopRadarLoop(); ensureNexradChipOn(); setRadarFrame(st.radarFrameIdx - 1); });
      if (playB) playB.addEventListener('click', toggleRadarLoop);
      if (nextB) nextB.addEventListener('click', function () { stopRadarLoop(); ensureNexradChipOn(); setRadarFrame(st.radarFrameIdx + 1); });
    }

    var botHtml = '';
    if (allPeriods.length) {
      botHtml += '<div class="wx-fc-hdr">' +
        '<div class="sub" style="margin:0">' + escH(P('7-Day NWS Forecast', 'NWS 未来 7 天天气预报', 'Pronóstico NWS a 7 días')) + ' <small>' + escH(wx.gridId ? (wx.gridId + ' ' + (wx.gridX ?? '') + ',' + (wx.gridY ?? '')) : 'api.weather.gov') + '</small></div>' +
        (allPeriods.length > 6
          ? '<button type="button" class="wx-btn" id="wxToggle7d">' + escH(st.showFull7d
              ? P('Show next 3 days', '仅看近 3 天', 'Ver próximos 3 días')
              : P('Show full 7 days (' + allPeriods.length + ' periods)', '展开完整 7 天（' + allPeriods.length + ' 时段）', 'Ver 7 días (' + allPeriods.length + ')')) + '</button>'
          : '') +
        '</div>';
      botHtml += '<div class="wx-periods">' + shownPeriods.map(function (p) {
        var metaBits = [];
        if (p.windSpeed) metaBits.push((p.windDirection ? p.windDirection + ' ' : '') + p.windSpeed);
        if (p.precipPct != null) metaBits.push(p.precipPct + '% ' + P('precip', '降水', 'precip.'));
        return '<div class="wx-pcard' + (p.isDaytime ? ' day' : '') + '">' +
          '<div class="wx-pcard-top">' +
            '<span class="wx-pname">' + escH(p.name) + '</span>' +
            '<span class="wx-ptemp">' + (p.temperature != null ? escH(p.temperature + '°' + (p.temperatureUnit || 'F')) : '—') + '</span>' +
          '</div>' +
          '<div class="wx-pdesc" title="' + escH(p.detailedForecast || p.shortForecast || '') + '">' + escH(p.shortForecast || '') + '</div>' +
          (metaBits.length ? '<div class="wx-pmeta">' + escH(metaBits.join(' · ')) + '</div>' : '') +
          '</div>';
      }).join('') + '</div>';
    }

    if (areRefLinksVisible()) {
      var fcLink = (wx && wx.forecastUrl) || ('https://forecast.weather.gov/MapClick.php?lat=' + Number(st.lat).toFixed(4) + '&lon=' + Number(st.lon).toFixed(4));
      botHtml += '<div class="wx-src-bar">' +
        '<span>🔗 ' + escH(P('Weather sources:', '气象数据源：', 'Fuentes meteorológicas:')) + '</span>' +
        wxRefChipHtml(fcLink, 'NOAA / NWS Forecast') +
        wxRefChipHtml('https://api.weather.gov/', 'NWS API') +
        wxRefChipHtml('https://opengeo.ncep.noaa.gov/', 'NOAA NCEP WMS') +
        wxRefChipHtml('https://mesonet.agron.iastate.edu/ogc/', 'Iowa State NEXRAD / GOES') +
        '</div>';
    }

    botBox.innerHTML = botHtml;

    var toggleBtn = document.getElementById('wxToggle7d');
    if (toggleBtn) {
      toggleBtn.addEventListener('click', function () {
        st.showFull7d = !st.showFull7d;
        renderLivePanels();
      });
    }
    refreshChipLabels();
  }

  function mount() {
    var sec = document.getElementById(SECTION_ID);
    if (!sec) return;
    st.sec = sec;
    var rep = getReport();
    st.rep = rep;

    var g = (rep && rep.geo) || {};
    var lat = g.refined && isNum(g.refined.lat) ? g.refined.lat : g.lat;
    var lon = g.refined && isNum(g.refined.lon) ? g.refined.lon : g.lon;
    var hazardsMod = rep && rep.modules && rep.modules.hazards;
    var hazardsData = getModData(rep, 'hazards');
    var wx = hazardsData && hazardsData.weather ? hazardsData.weather : null;
    var enabledMods = rep && rep.enabledModules;
    var hazardsDisabled = Boolean(
      (rep && rep.nwsWeatherDisabled === true) ||
      (Array.isArray(enabledMods) && enabledMods.indexOf('natural_hazards') < 0) ||
      (hazardsMod && hazardsMod.disabled === true)
    );

    if (!isNum(lat) || !isNum(lon) || !isUsCountry(g.country) || hazardsDisabled || (!hazardsData && !wx)) {
      stopRadarLoop();
      sec.hidden = true;
      sec.innerHTML = '';
      if (window.OpenEstateWorkspace && typeof window.OpenEstateWorkspace.renderTocTree === 'function') {
        try { window.OpenEstateWorkspace.renderTocTree(); } catch (e) {}
      }
      return;
    }

    sec.hidden = false;
    st.lat = lat;
    st.lon = lon;
    if (wx && !st.wx) st.wx = wx;
    injectCss();

    if (sec.getAttribute('data-weather-init') !== '1') {
      sec.setAttribute('data-weather-init', '1');
      sec.innerHTML =
        '<div id="wxHeader"></div>' +
        '<div id="wxTopPanels"></div>' +
        '<div class="wx-map-bar" id="wxMapHdr"></div>' +
        '<div class="chips" id="' + CHIPS_ID + '"></div>' +
        '<div id="' + MAP_ID + '" class="map"></div>' +
        '<div id="wxBottomPanels"></div>';
    }

    renderLivePanels();
    if (!st.mapMounted) {
      mountMap();
    }
    if (location.protocol !== 'file:' && isWxStale(st.wx)) {
      fetchLiveNws(lat, lon, false);
    }

    if (!window.__OE_WEATHER_LANG_OBS__ && typeof MutationObserver !== 'undefined' && document.documentElement) {
      window.__OE_WEATHER_LANG_OBS__ = true;
      new MutationObserver(function () {
        if (st.sec && !st.sec.hidden) renderLivePanels();
      }).observe(document.documentElement, { attributes: true, attributeFilter: ['lang', 'data-display-links'] });
    }

    if (window.OpenEstateWorkspace && typeof window.OpenEstateWorkspace.renderTocTree === 'function') {
      try { window.OpenEstateWorkspace.renderTocTree(); } catch (e2) {}
    }
  }

  window.OpenEstateWeather = {
    mount: mount,
    refresh: function () { return st.lat != null && st.lon != null ? fetchLiveNws(st.lat, st.lon, true) : Promise.resolve(null); },
    _state: st
  };

  try {
    if (document.readyState === 'loading' && document.addEventListener) {
      document.addEventListener('DOMContentLoaded', mount);
    } else {
      mount();
    }
  } catch (err) {
    if (window.console && console.warn) console.warn('weather_widget: mount failed', err);
  }
})();
