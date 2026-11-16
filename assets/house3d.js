/* Open Estate House 3D — interactive exterior model of the home and its lot, plus embedded listing tours.
 *
 * Self-contained: reads the report embedded in the page, draws an instant model from the mapped
 * footprints, then upgrades it with measured roofs, terrain and trees from GET /api/house3d/:slug
 * (USGS 3DEP LiDAR). three.js is loaded on demand from a CDN the first time the section scrolls
 * into view. All text is trilingual through data-en / data-zh / data-es spans, which the report
 * page's language switch already swaps.
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_HOUSE3D_INIT__) return;
  window.__OPEN_ESTATE_HOUSE3D_INIT__ = true;

  var Core = window.OpenEstateCore || null;
  var THREE_URLS = (Core && Array.isArray(Core.THREE_URLS)) ? Core.THREE_URLS.slice() : [
    'https://cdn.jsdelivr.net/npm/three@0.170.0/build/three.module.min.js',
    'https://unpkg.com/three@0.170.0/build/three.module.min.js',
  ];
  var USGS_TILE_TMPL = 'https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/{z}/{y}/{x}';
  var SCC_SJ_TILE_TMPL = 'https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/tile/{z}/{y}/{x}';
  var KINGCO_TILE_TMPL = 'https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer/tile/{z}/{y}/{x}';
  var ESRI_TILE = 'https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/';
  var IMG_ZOOM = 19;
  var EARTH_R = 6371008.8;
  var FT = 0.3048;
  var STOREY_M = 2.9;
  var SECTION_ID = 'house3d';

  /* ------------------------------------------------------------------ language + formatting */
  var curLang = function () {
    if (Core && typeof Core.langCode === 'function') return Core.langCode();
    var l = String((document.documentElement && document.documentElement.lang) || 'en').toLowerCase();
    return l.indexOf('zh') === 0 ? 'zh' : l.indexOf('es') === 0 ? 'es' : 'en';
  };
  var escH = function (s) {
    if (Core && typeof Core.esc === 'function') return Core.esc(s);
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  };
  /** Plain string in the active language. */
  var P = function (en, zh, es) {
    var l = curLang();
    return l === 'zh' ? (zh || en) : l === 'es' ? (es || en) : en;
  };
  /** Trilingual span that the page's language switch keeps in sync. */
  var T = function (en, zh, es) {
    return '<span data-en="' + escH(en) + '" data-zh="' + escH(zh || en) + '" data-es="' + escH(es || en) + '">' + escH(P(en, zh, es)) + '</span>';
  };
  var isNum = function (x) { return typeof x === 'number' && isFinite(x); };
  var nf = function (v, d) {
    if (Core && typeof Core.fNum === 'function' && isNum(v)) return Core.fNum(v, d);
    try {
      return new Intl.NumberFormat(curLang() === 'zh' ? 'zh-CN' : curLang() === 'es' ? 'es-US' : 'en-US', { maximumFractionDigits: d || 0, minimumFractionDigits: 0 }).format(v);
    } catch (e) { return String(Math.round(v)); }
  };
  /** Length shown the way US buyers read it, with metres alongside. */
  var ftm = function (m) { return isNum(m) ? nf(m / FT, 0) + ' ft (' + nf(m, 1) + ' m)' : '—'; };
  var sqftm = function (m2) { return isNum(m2) ? nf(m2 * 10.7639104, 0) + ' sq ft (' + nf(m2, 0) + ' m²)' : '—'; };
  var COMPASS = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'];
  var COMPASS_ZH = { N: '北', NE: '东北', E: '东', SE: '东南', S: '南', SW: '西南', W: '西', NW: '西北' };
  var COMPASS_ES = { N: 'N', NE: 'NE', E: 'E', SE: 'SE', S: 'S', SW: 'SO', W: 'O', NW: 'NO' };
  var compass8 = function (deg) { return COMPASS[Math.round((((deg % 360) + 360) % 360) / 45) % 8]; };
  var compassT = function (c) { return T(c, COMPASS_ZH[c] || c, COMPASS_ES[c] || c); };

  /* ------------------------------------------------------------------ report access */
  function getReport() {
    if (Core && typeof Core.getRep === 'function') return Core.getRep();
    try { if (typeof R !== 'undefined' && R && typeof R === 'object') return R; } catch (e) { /* not on a report page */ }
    try {
      var el = document.getElementById('report-data');
      var txt = el && el.textContent;
      return txt ? JSON.parse(txt) : null;
    } catch (e2) { return null; }
  }
  function modData(rep, key) {
    if (Core && typeof Core.mod === 'function') return Core.mod(rep, key);
    var m = rep && rep.modules && rep.modules[key];
    return (m && m.data) || (rep && rep[key]) || null;
  }

  /* ------------------------------------------------------------------ planar geometry */
  function ringArea(r) {
    var s = 0;
    for (var i = 0, n = r.length; i < n; i++) { var a = r[i], b = r[(i + 1) % n]; s += a[0] * b[1] - b[0] * a[1]; }
    return s / 2;
  }
  function normRing(raw) {
    var out = [];
    for (var i = 0; i < (raw || []).length; i++) {
      var p = raw[i];
      if (!p || !isNum(+p[0]) || !isNum(+p[1])) continue;
      var q = out[out.length - 1];
      if (q && Math.abs(q[0] - p[0]) < 1e-6 && Math.abs(q[1] - p[1]) < 1e-6) continue;
      out.push([+p[0], +p[1]]);
    }
    if (out.length >= 2) {
      var f = out[0], l = out[out.length - 1];
      if (Math.abs(f[0] - l[0]) < 1e-6 && Math.abs(f[1] - l[1]) < 1e-6) out.pop();
    }
    if (ringArea(out) < 0) out.reverse();
    return out;
  }
  function ringCentroid(r) {
    var a = ringArea(r), cx = 0, cy = 0, n = r.length;
    if (Math.abs(a) < 1e-9) {
      for (var k = 0; k < n; k++) { cx += r[k][0]; cy += r[k][1]; }
      return [cx / Math.max(1, n), cy / Math.max(1, n)];
    }
    for (var i = 0; i < n; i++) {
      var p = r[i], q = r[(i + 1) % n], f = p[0] * q[1] - q[0] * p[1];
      cx += (p[0] + q[0]) * f; cy += (p[1] + q[1]) * f;
    }
    return [cx / (6 * a), cy / (6 * a)];
  }
  function ringBBox(r) {
    var b = [Infinity, Infinity, -Infinity, -Infinity];
    for (var i = 0; i < r.length; i++) {
      b[0] = Math.min(b[0], r[i][0]); b[1] = Math.min(b[1], r[i][1]);
      b[2] = Math.max(b[2], r[i][0]); b[3] = Math.max(b[3], r[i][1]);
    }
    return b;
  }
  function pointInRing(x, y, r) {
    var inside = false;
    for (var i = 0, j = r.length - 1; i < r.length; j = i++) {
      var xi = r[i][0], yi = r[i][1], xj = r[j][0], yj = r[j][1];
      if (((yi > y) !== (yj > y)) && (x < ((xj - xi) * (y - yi)) / (yj - yi) + xi)) inside = !inside;
    }
    return inside;
  }
  function distToRing(x, y, r) {
    var best = Infinity;
    for (var i = 0, n = r.length; i < n; i++) {
      var a = r[i], b = r[(i + 1) % n], dx = b[0] - a[0], dy = b[1] - a[1], l2 = dx * dx + dy * dy;
      var t = l2 < 1e-12 ? 0 : Math.max(0, Math.min(1, ((x - a[0]) * dx + (y - a[1]) * dy) / l2));
      var qx = a[0] + t * dx, qy = a[1] + t * dy, d = Math.hypot(x - qx, y - qy);
      if (d < best) best = d;
    }
    return best;
  }
  /** Dominant wall direction (radians CCW from east, folded into (-45deg, 45deg]). */
  function dominantAxis(r) {
    var c = 0, s = 0;
    for (var i = 0, n = r.length; i < n; i++) {
      var a = r[i], b = r[(i + 1) % n], dx = b[0] - a[0], dy = b[1] - a[1], len = Math.hypot(dx, dy);
      if (len < 0.3) continue;
      var th = Math.atan2(dy, dx), w = len * len;
      c += w * Math.cos(4 * th); s += w * Math.sin(4 * th);
    }
    return (Math.abs(c) < 1e-9 && Math.abs(s) < 1e-9) ? 0 : Math.atan2(s, c) / 4;
  }
  function rot2(x, y, a) { var sn = Math.sin(a), cs = Math.cos(a); return [x * cs - y * sn, x * sn + y * cs]; }

  /* ------------------------------------------------------------------ scene from the embedded report */
  function localFrame(lat0, lon0) {
    var ky = (Math.PI / 180) * EARTH_R, kx = Math.max(1e-6, Math.abs(Math.cos((lat0 * Math.PI) / 180))) * ky;
    return {
      lat0: lat0, lon0: lon0,
      fwd: function (lat, lon) { return [(lon - lon0) * kx, (lat - lat0) * ky]; },
      inv: function (x, y) { return [lat0 + y / ky, lon0 + x / kx]; },
    };
  }
  function assumedHeights(role, mappedHeight, levels) {
    if (isNum(mappedHeight) && mappedHeight > 1.5 && mappedHeight < 400) {
      var rise = Math.max(0.6, Math.min(2.4, mappedHeight * 0.22));
      return { eave: Math.max(2.2, mappedHeight - rise), ridge: mappedHeight, src: 'osm' };
    }
    if (isNum(levels) && levels >= 1 && levels < 120) {
      var e1 = levels * STOREY_M + 0.3;
      return { eave: e1, ridge: e1 + 1.5, src: 'levels' };
    }
    return { eave: null, ridge: null, src: 'unavailable' };
  }
  /** Mirrors `modules::house3d` (no LiDAR): footprints with mapped/storey heights, or 2D footprint when height is unavailable. */
  function sceneFromReport(rep) {
    if (!rep) return null;
    var fs = modData(rep, 'fengshui') || {}, house = fs.house || {}, dia = fs.diagram || {}, geo = rep.geo || {}, facts = rep.facts || {};
    var lat = null, lon = null, houseFrame = false;
    if (isNum(house.lat) && isNum(house.lon)) { lat = house.lat; lon = house.lon; houseFrame = true; }
    else if (geo.refined && isNum(geo.refined.lat) && isNum(geo.refined.lon)) { lat = geo.refined.lat; lon = geo.refined.lon; }
    else if (isNum(geo.lat) && isNum(geo.lon)) { lat = geo.lat; lon = geo.lon; }
    if (lat == null) return null;
    var frame = localFrame(lat, lon);
    var llRing = function (arr) {
      if (!Array.isArray(arr)) return null;
      var r = normRing(arr.filter(function (p) { return p && isNum(+p[0]) && isNum(+p[1]); }).map(function (p) { return frame.fwd(+p[0], +p[1]); }));
      return r.length >= 3 ? r : null;
    };
    var locRing = function (arr) {
      if (!Array.isArray(arr)) return null;
      var r = normRing(arr);
      return r.length >= 3 ? r : null;
    };
    var stories = isNum(+facts.stories) && +facts.stories >= 1 ? +facts.stories : null;
    var buildings = [];
    var pushB = function (ring, role, label, mapped, levels) {
      if (!ring || Math.abs(ringArea(ring)) < 3) return;
      var c = ringCentroid(ring), a = Math.abs(ringArea(ring));
      for (var i = 0; i < buildings.length; i++) {
        var bc = ringCentroid(buildings[i].ring), ba = buildings[i].areaM2;
        if (Math.hypot(bc[0] - c[0], bc[1] - c[1]) < 1.5 && Math.abs(ba - a) <= 0.25 * Math.max(ba, a)) return;
      }
      var hgt = assumedHeights(role, mapped, levels);
      buildings.push({
        role: role, label: label || null, ring: ring, areaM2: a, outline: 'osm', base: 0,
        heightSource: hgt.src, eave: hgt.eave, ridge: hgt.ridge,
        roofShape: hgt.src === 'unavailable' ? null : (a > 600 ? 'flat' : 'hip'),
        axisDeg: (dominantAxis(ring) * 180) / Math.PI,
      });
    };
    var subj = (houseFrame && locRing(dia.subject)) || llRing(house.footprint);
    pushB(subj, 'subject', null, +house.heightM, isNum(+house.levels) && +house.levels >= 1 ? +house.levels : stories);
    var lot = (houseFrame && locRing(dia.lot)) || llRing(house.lotPolygon) || llRing(facts.parcelPolygon) || llRing(facts.buildingRegister && facts.buildingRegister.parcelPolygon);
    (Array.isArray(house.additionalStructures) ? house.additionalStructures : []).forEach(function (st) {
      pushB(llRing(st && st.footprint), 'accessory', st && st.label);
    });
    if (houseFrame) {
      (Array.isArray(dia.additional) ? dia.additional : []).forEach(function (r) { pushB(locRing(r), 'accessory'); });
      (Array.isArray(dia.buildings) ? dia.buildings : []).forEach(function (r) {
        var ring = locRing(r);
        if (!ring) return;
        // Same rule as the report maps: an unlabelled structure inside the parcel belongs to this home.
        var c = ringCentroid(ring), onLot = lot && pointInRing(c[0], c[1], lot) && Math.abs(ringArea(ring)) >= 5.5;
        pushB(ring, onLot ? 'accessory' : 'neighbor');
      });
    }
    var streets = [];
    if (houseFrame) {
      (Array.isArray(dia.streets) ? dia.streets : []).forEach(function (s) {
        var pts = (s && Array.isArray(s.pts) ? s.pts : []).filter(function (p) { return p && isNum(+p[0]) && isNum(+p[1]); }).map(function (p) { return [+p[0], +p[1]]; });
        if (pts.length >= 2) streets.push({ name: s.name || null, kind: s.highway || 'residential', pts: pts });
      });
    }
    var subjIdx = -1;
    for (var k = 0; k < buildings.length; k++) if (buildings[k].role === 'subject') { subjIdx = k; break; }
    var sb = subjIdx >= 0 ? buildings[subjIdx] : null;
    var facing = fs.orientation && isNum(+fs.orientation.facingTrue) ? +fs.orientation.facingTrue : null;
    return {
      version: 1,
      quality: sb ? 'footprint' : 'none',
      origin: { lat: lat, lon: lon, elevM: fs.terrain && isNum(+fs.terrain.center) ? +fs.terrain.center : null },
      facingDeg: facing,
      lidar: null, terrain: null, trees: [],
      buildings: buildings,
      subject: sb ? { index: subjIdx, areaM2: sb.areaM2, heightSource: sb.heightSource, eaveM: sb.eave, ridgeM: sb.ridge, roofShape: null, pitchDeg: null, roofPlanes: null, outline: 'osm' } : null,
      lot: lot || null,
      streets: streets,
      metrics: {},
      local: true,
    };
  }

  /* ------------------------------------------------------------------ sun position + property time zone */
  var RAD = Math.PI / 180;
  function sunPosition(ms, lat, lon) {
    var d = ms / 86400000 - 0.5 + 2440588 - 2451545;
    var M = RAD * (357.5291 + 0.98560028 * d);
    var C = RAD * (1.9148 * Math.sin(M) + 0.02 * Math.sin(2 * M) + 0.0003 * Math.sin(3 * M));
    var L = M + C + RAD * 102.9372 + Math.PI;
    var e = RAD * 23.4397;
    var dec = Math.asin(Math.sin(e) * Math.sin(L));
    var ra = Math.atan2(Math.sin(L) * Math.cos(e), Math.cos(L));
    var lw = RAD * -lon, phi = RAD * lat;
    var H = RAD * (280.16 + 360.9856235 * d) - lw - ra;
    var az = Math.atan2(Math.sin(H), Math.cos(H) * Math.sin(phi) - Math.tan(dec) * Math.cos(phi));
    var alt = Math.asin(Math.sin(phi) * Math.sin(dec) + Math.cos(phi) * Math.cos(dec) * Math.cos(H));
    // az above is measured from south toward west; return a compass bearing instead.
    return { altitude: alt, bearing: (((az / RAD + 180) % 360) + 360) % 360 };
  }
  /** IANA zone for a US address. State-level with longitude/latitude splits for the states that
   *  straddle two zones; good to the hour everywhere except within a few miles of a zone line. */
  function usTimeZone(state, lat, lon) {
    var st = String(state || '').toUpperCase();
    var E = 'America/New_York', Cn = 'America/Chicago', Mt = 'America/Denver', Pc = 'America/Los_Angeles';
    var fixed = {
      CT: E, DE: E, DC: E, GA: E, ME: E, MD: E, MA: E, NH: E, NJ: E, NY: E, NC: E, OH: E, PA: E, RI: E, SC: E, VT: E, VA: E, WV: E,
      AL: Cn, AR: Cn, IL: Cn, IA: Cn, LA: Cn, MN: Cn, MS: Cn, MO: Cn, OK: Cn, WI: Cn,
      CO: Mt, MT: Mt, NM: Mt, UT: Mt, WY: Mt, AZ: 'America/Phoenix',
      CA: Pc, NV: Pc, WA: Pc, HI: 'Pacific/Honolulu', AK: 'America/Anchorage', PR: 'America/Puerto_Rico', VI: 'America/Puerto_Rico', GU: 'Pacific/Guam',
    };
    if (fixed[st]) return fixed[st];
    if (st === 'FL') return lon < -85.0 && lat > 29.3 ? Cn : E;
    if (st === 'IN') return (lon < -86.9 && (lat > 40.9 || lat < 38.3)) ? Cn : 'America/Indiana/Indianapolis';
    if (st === 'KY') return lon < -86.0 ? Cn : E;
    if (st === 'TN') return lon < -85.2 ? Cn : E;
    if (st === 'MI') return lon < -87.6 && lat > 45.0 ? Cn : 'America/Detroit';
    if (st === 'TX') return lon < -104.9 ? Mt : Cn;
    if (st === 'KS') return lon < -101.5 ? Mt : Cn;
    if (st === 'NE') return lon < -101.0 ? Mt : Cn;
    if (st === 'SD') return lon < -100.4 ? Mt : Cn;
    if (st === 'ND') return lon < -101.6 && lat < 47.6 ? Mt : Cn;
    if (st === 'ID') return lat > 45.5 ? Pc : 'America/Boise';
    if (st === 'OR') return lon > -117.9 && lat < 44.5 ? 'America/Boise' : Pc;
    // Unknown state: nearest standard meridian.
    var h = Math.round(lon / 15);
    return h <= -8 ? Pc : h === -7 ? Mt : h === -6 ? Cn : E;
  }
  var DTF_CACHE = {};
  function tzOffsetMin(tz, utcMs) {
    try {
      var dtf = DTF_CACHE[tz] || (DTF_CACHE[tz] = new Intl.DateTimeFormat('en-US', { timeZone: tz, hourCycle: 'h23', year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit' }));
      var o = {};
      dtf.formatToParts(new Date(utcMs)).forEach(function (p) { o[p.type] = p.value; });
      var asUtc = Date.UTC(+o.year, +o.month - 1, +o.day, +o.hour % 24, +o.minute, +o.second);
      return Math.round((asUtc - utcMs) / 60000);
    } catch (e) { return null; }
  }
  /** UTC instant of a wall-clock time at the property. */
  function localToUtc(tz, lon, y, m, d, minutes) {
    var guess = Date.UTC(y, m, d, 0, minutes);
    var off = tzOffsetMin(tz, guess);
    if (off == null) return guess - Math.round(lon / 15) * 3600000;
    var ms = guess - off * 60000, off2 = tzOffsetMin(tz, ms);
    return off2 != null && off2 !== off ? guess - off2 * 60000 : ms;
  }
  function localParts(tz, lon, utcMs) {
    var off = tzOffsetMin(tz, utcMs);
    if (off == null) off = Math.round(lon / 15) * 60;
    var dt = new Date(utcMs + off * 60000);
    return { y: dt.getUTCFullYear(), m: dt.getUTCMonth(), d: dt.getUTCDate(), minutes: dt.getUTCHours() * 60 + dt.getUTCMinutes() };
  }
  function fmtClock(minutes) {
    var tot = ((Math.round(minutes) % 1440) + 1440) % 1440, h = Math.floor(tot / 60), mm = tot % 60;
    if (curLang() === 'en') return ((h + 11) % 12 + 1) + ':' + (mm < 10 ? '0' : '') + mm + (h < 12 ? ' AM' : ' PM');
    return (h < 10 ? '0' : '') + h + ':' + (mm < 10 ? '0' : '') + mm;
  }

  /* ------------------------------------------------------------------ listing tours (official embeds only) */
  var ID_RE = /^[A-Za-z0-9_-]{4,64}$/;
  /** Recognises a listing-tour link and rebuilds the host's own embed URL from its validated id.
   *  Unknown hosts return null and are never framed. */
  function parseTourUrl(raw) {
    var txt = String(raw || '').trim();
    if (!txt || txt.length > 600) return null;
    if (/^\/\//.test(txt)) txt = 'https:' + txt;
    if (!/^https?:\/\//i.test(txt)) txt = 'https://' + txt;
    var u;
    try { u = new URL(txt); } catch (e) { return null; }
    if (u.protocol !== 'https:' && u.protocol !== 'http:') return null;
    if (u.username || u.password) return null;
    var host = u.hostname.toLowerCase().replace(/^www\./, ''), path = u.pathname, m;
    var mk = function (vendor, embedUrl, openUrl, kind) { return { vendor: vendor, embedUrl: embedUrl, openUrl: openUrl, kind: kind || '3d', generic: vendor === 'Listing 3D tour' }; };
    if (host === 'my.matterport.com' || host === 'matterport.com') {
      var id = u.searchParams.get('m') || ((m = path.match(/\/(?:show|models|discover\/space)\/([A-Za-z0-9]{6,24})/)) && m[1]);
      if (id && /^[A-Za-z0-9]{6,24}$/.test(id)) return mk('Matterport', 'https://my.matterport.com/show/?m=' + id + '&play=1', 'https://my.matterport.com/show/?m=' + id);
      return null;
    }
    if (host === 'zillow.com') {
      m = path.match(/^\/(?:view-imx|view-3d-home)\/([0-9a-fA-F-]{16,48})/);
      if (m) return mk('Listing 3D tour', 'https://www.zillow.com/view-imx/' + m[1] + '?setAttribution=mls&wl=true&initialViewType=pano', 'https://www.zillow.com/view-imx/' + m[1] + '?setAttribution=mls&wl=true');
      return null;
    }
    if (host === 'youriguide.com') {
      m = path.match(/^\/(?:embed\/)?([A-Za-z0-9_-]{4,120})\/?$/);
      if (m && m[1] !== 'embed') return mk('iGUIDE', 'https://youriguide.com/embed/' + m[1] + '?unbranded=1', 'https://youriguide.com/' + m[1]);
      return null;
    }
    if (host === 'kuula.co') {
      m = path.match(/^\/share\/(collection\/)?([A-Za-z0-9_-]{3,40})\/?$/);
      if (m) { var ku = 'https://kuula.co/share/' + (m[1] || '') + m[2]; return mk('Kuula', ku + '?logo=0&info=0&fs=1&vr=0&sd=1&thumbs=1', ku); }
      return null;
    }
    if (host === 'app.cloudpano.com') {
      m = path.match(/^\/tours\/([A-Za-z0-9_-]{4,64})\/?$/);
      return m ? mk('CloudPano', 'https://app.cloudpano.com/tours/' + m[1], 'https://app.cloudpano.com/tours/' + m[1]) : null;
    }
    if (host === 'ricoh360.com') {
      m = path.match(/^\/tours\/([A-Za-z0-9_-]{4,64})\/?$/);
      return m ? mk('RICOH360 Tours', 'https://www.ricoh360.com/tours/' + m[1], 'https://www.ricoh360.com/tours/' + m[1]) : null;
    }
    if (host === 'tour.giraffe360.com') {
      m = path.match(/^\/([A-Za-z0-9_-]{4,64})\/?$/);
      return m ? mk('Giraffe360', 'https://tour.giraffe360.com/' + m[1] + '/', 'https://tour.giraffe360.com/' + m[1] + '/') : null;
    }
    if (host === 'players.cupix.com') {
      m = path.match(/^\/p\/([A-Za-z0-9_-]{4,64})\/?$/);
      return m ? mk('Cupix', 'https://players.cupix.com/p/' + m[1], 'https://players.cupix.com/p/' + m[1]) : null;
    }
    if (host === 'youtube.com' || host === 'm.youtube.com' || host === 'youtu.be' || host === 'youtube-nocookie.com') {
      var vid = host === 'youtu.be' ? path.slice(1).split('/')[0] : (u.searchParams.get('v') || ((m = path.match(/^\/(?:embed|shorts|live)\/([A-Za-z0-9_-]{6,20})/)) && m[1]));
      if (vid && /^[A-Za-z0-9_-]{6,20}$/.test(vid)) return mk('YouTube', 'https://www.youtube-nocookie.com/embed/' + vid, 'https://www.youtube.com/watch?v=' + vid, 'video');
      return null;
    }
    if (host === 'vimeo.com' || host === 'player.vimeo.com') {
      m = path.match(/(?:^|\/)(\d{6,12})(?:\/|$)/);
      return m ? mk('Vimeo', 'https://player.vimeo.com/video/' + m[1], 'https://vimeo.com/' + m[1], 'video') : null;
    }
    return null;
  }
  /** Tour links recorded with the listing (licensed feed or researched facts). */
  function toursFromReport(rep) {
    var facts = (rep && rep.facts) || {}, found = [], seen = {};
    var add = function (v, label) {
      if (!v) return;
      if (Array.isArray(v)) { v.forEach(function (x) { add(x, label); }); return; }
      if (typeof v === 'object') { add(v.url || v.href || v.link || v.embedUrl, v.label || v.name || label); return; }
      var raw = String(v).trim();
      if (!/^https?:\/\//i.test(raw) || seen[raw]) return;
      seen[raw] = 1;
      var t = parseTourUrl(raw);
      if (t) { t.label = label || null; t.fromListing = true; found.push(t); }
      else found.push({ vendor: null, embedUrl: null, openUrl: raw, kind: 'link', label: label || null, fromListing: true });
    };
    var ml = facts.mlsListing || {}, li = facts.listing || {};
    [facts.virtualTours, facts.virtualTourUrl, facts.virtualTour, facts.tour3dUrl, facts.tourUrl,
      ml.virtualTours, ml.virtualTourUrl, ml.virtualTourUrlUnbranded, ml.virtualTourUrlBranded,
      li.virtualTours, li.virtualTourUrl].forEach(function (v) { add(v, null); });
    // An external link is only safe to offer when it is plain https.
    return found.filter(function (t) { return t.embedUrl || /^https:\/\//i.test(t.openUrl); }).slice(0, 6);
  }

  function resolveSatelliteConfig(rep, lat, lon) {
    var sat = rep && rep.meta && rep.meta.satellite ? rep.meta.satellite : null;
    var isFile = typeof location !== 'undefined' && location.protocol === 'file:';
    var fallbackUrl = (sat && sat.fallbackTileUrl) || USGS_TILE_TMPL;
    var fallbackZoom = (sat && Number(sat.fallbackMaxNativeZoom)) || 16;
    var fallbackAttr = (sat && sat.fallbackAttribution) || 'Imagery © USGS The National Map / USDA NAIP (public domain)';
    if (sat && sat.tileUrl && !(isFile && sat.proxied)) {
      return {
        source: sat.source || 'configured',
        tileUrl: sat.tileUrl,
        maxNativeZoom: Number(sat.maxNativeZoom) || 16,
        tileSize: Number(sat.tileSize) || 256,
        attribution: sat.attribution || fallbackAttr,
        fallbackTileUrl: fallbackUrl,
        fallbackMaxNativeZoom: fallbackZoom,
        fallbackAttribution: fallbackAttr,
      };
    }
    if (typeof lat === 'number' && typeof lon === 'number' && isFinite(lat) && isFinite(lon)) {
      var inScc = (lat >= 37.10 && lat <= 37.49 && lon >= -122.20 && lon <= -121.55)
        || (lat >= 36.95 && lat <= 37.15 && lon >= -121.78 && lon <= -121.45);
      if (inScc) {
        return {
          source: 'scc_sj_ortho_2025',
          tileUrl: SCC_SJ_TILE_TMPL,
          maxNativeZoom: 21,
          tileSize: 256,
          attribution: 'Aerial © City of San José / Santa Clara Valley (2025 Ortho, 0.1m)',
          fallbackTileUrl: fallbackUrl,
          fallbackMaxNativeZoom: fallbackZoom,
          fallbackAttribution: fallbackAttr,
        };
      }
      if (lat >= 47.28 && lat <= 47.78 && lon >= -122.54 && lon <= -121.10) {
        return {
          source: 'kingco_ortho_2023',
          tileUrl: KINGCO_TILE_TMPL,
          maxNativeZoom: 20,
          tileSize: 256,
          attribution: 'Aerial © King County GIS (2023 Ortho)',
          fallbackTileUrl: fallbackUrl,
          fallbackMaxNativeZoom: fallbackZoom,
          fallbackAttribution: fallbackAttr,
        };
      }
    }
    return {
      source: 'usgs_imagery_only',
      tileUrl: USGS_TILE_TMPL,
      maxNativeZoom: 16,
      tileSize: 256,
      attribution: fallbackAttr,
      fallbackTileUrl: USGS_TILE_TMPL,
      fallbackMaxNativeZoom: 16,
      fallbackAttribution: fallbackAttr,
    };
  }

  function formatTileUrl(tmpl, z, y, x) {
    var s = String(tmpl || '');
    if (s.indexOf('{z}') !== -1) {
      return s.replace('{z}', String(z)).replace('{y}', String(y)).replace('{x}', String(x));
    }
    return s + z + '/' + y + '/' + x;
  }

  // Shared with the viewer and UI halves of this file.
  var H3 = {
    THREE_URLS: THREE_URLS, ESRI_TILE: ESRI_TILE, IMG_ZOOM: IMG_ZOOM, EARTH_R: EARTH_R, FT: FT, SECTION_ID: SECTION_ID,
    resolveSatelliteConfig: resolveSatelliteConfig, formatTileUrl: formatTileUrl,
    curLang: curLang, escH: escH, P: P, T: T, isNum: isNum, nf: nf, ftm: ftm, sqftm: sqftm, compass8: compass8, compassT: compassT,
    getReport: getReport, modData: modData,
    ringArea: ringArea, normRing: normRing, ringCentroid: ringCentroid, ringBBox: ringBBox, pointInRing: pointInRing, distToRing: distToRing,
    dominantAxis: dominantAxis, rot2: rot2, localFrame: localFrame, sceneFromReport: sceneFromReport,
    sunPosition: sunPosition, usTimeZone: usTimeZone, tzOffsetMin: tzOffsetMin, localToUtc: localToUtc, localParts: localParts, fmtClock: fmtClock,
    parseTourUrl: parseTourUrl, toursFromReport: toursFromReport,
  };
  window.OpenEstateHouse3D = H3;
})();
/* Open Estate House 3D — WebGL viewer (geometry, lighting, camera). Extends window.OpenEstateHouse3D. */
(function () {
  'use strict';
  if (typeof window === 'undefined') return;
  var H3 = window.OpenEstateHouse3D;
  if (!H3 || H3.createViewer) return;
  var isNum = H3.isNum, RAD = Math.PI / 180;
  var GROUND_HALF = 120;
  /** Outline colours shared with the report's 2D satellite map legend (main house / garage-ADU). */
  var OUTLINE = { subject: 0xfacc15, subjectEdge: 0xb45309, accessory: 0xe879f9, accessoryEdge: 0x86198f };
  H3.OUTLINE = OUTLINE;
  var STREET_W = { motorway: 15, trunk: 14, primary: 12.5, secondary: 11, tertiary: 9.5, residential: 7.5, unclassified: 7.5, living_street: 6.5, service: 4, footway: 1.6, path: 1.6, cycleway: 2, steps: 1.6, pedestrian: 4, track: 3 };
  var clamp = function (v, a, b) { return Math.max(a, Math.min(b, v)); };
  /** Clutter rules used by roof-measurement reports: no label on an edge under 5 ft or a face under 20 sq ft. */
  var MIN_EDGE_M = 1.524, MIN_FACE_M2 = 1.858;
  /** A tree standing between the camera and the home is drawn at this opacity (it keeps its shadow). */
  var FADE_ALPHA = 0.2;
  H3.MEASURE_RULES = { minEdgeM: MIN_EDGE_M, minFaceM2: MIN_FACE_M2, fadeAlpha: FADE_ALPHA };

  /* ---------------------------------------------------------------- terrain sampler */
  function terrainSampler(scene) {
    var t = scene && scene.terrain;
    if (!t || !Array.isArray(t.z) || !(t.n > 1)) return { has: false, half: 0, h: function () { return 0; } };
    var half = t.half, step = t.step, n = t.n, z = t.z;
    return {
      has: true, half: half,
      h: function (e, nn) {
        var fx = clamp((e + half) / step, 0, n - 1), fy = clamp((nn + half) / step, 0, n - 1);
        var x0 = Math.floor(fx), y0 = Math.floor(fy), x1 = Math.min(n - 1, x0 + 1), y1 = Math.min(n - 1, y0 + 1);
        var tx = fx - x0, ty = fy - y0;
        return ((z[y0 * n + x0] * (1 - tx) + z[y0 * n + x1] * tx) * (1 - ty) + (z[y1 * n + x0] * (1 - tx) + z[y1 * n + x1] * tx) * ty) / 100;
      },
    };
  }

  /* ---------------------------------------------------------------- roof model */
  /** Height of the flat-topped block drawn when a roof has no fitted planes: the mean measured
   *  roof height when the survey covers the building, midway between mapped/storey eave and ridge
   *  when available, or a flush 2D footprint pad (0.06m) when height data is unavailable. */
  function blockHeight(b) {
    if (b.heightSource === 'lidar' && isNum(b.meanHeight)) {
      var eL = isNum(b.eave) ? b.eave : b.meanHeight, rL = isNum(b.ridge) ? Math.max(b.ridge, eL) : b.meanHeight;
      return clamp(b.meanHeight, eL, rL);
    }
    if (!isNum(b.eave) && !isNum(b.ridge)) return 0.06;
    var eave = isNum(b.eave) ? b.eave : b.ridge, ridge = isNum(b.ridge) ? Math.max(b.ridge, eave) : eave;
    return (eave + ridge) / 2;
  }
  /** Roof of one building as flat faces in the building's own lattice frame.
   *  With `roofFit` from the server: the fitted planes, the plane each lattice node belongs to
   *  and how touching planes meet (ridge/hip = lower plane wins, valley = higher plane wins).
   *  Without it: one horizontal plane (a flat-topped block, labelled "simplified" in the UI).
   *  The raw surveyed surface is never drawn. */
  function roofModel(b) {
    var ring = b.ring, fit = b.roofFit, r = b.roof, M = { fitted: false, rules: {} };
    if (fit && r && Array.isArray(fit.planes) && fit.planes.length && Array.isArray(fit.labels) && r.w > 1 && r.h > 1 && isNum(r.step) && r.step > 0) {
      var lab = new Int16Array(r.w * r.h), k = 0;
      for (var q = 0; q + 1 < fit.labels.length; q += 2) {
        var v = fit.labels[q] >= 0 && fit.labels[q] < fit.planes.length ? fit.labels[q] : -1;
        for (var c = 0; c < fit.labels[q + 1] && k < lab.length; c++) lab[k++] = v;
      }
      while (k < lab.length) lab[k++] = -1;
      // Nodes just outside the surveyed rim take a neighbouring face.
      for (var pass = 0; pass < 2; pass++) {
        var prev = lab.slice();
        for (var j = 0; j < r.h; j++) for (var i = 0; i < r.w; i++) {
          if (prev[j * r.w + i] >= 0) continue;
          for (var dj = -1; dj <= 1 && lab[j * r.w + i] < 0; dj++) for (var di = -1; di <= 1; di++) {
            var ii = i + di, jj = j + dj;
            if (ii >= 0 && jj >= 0 && ii < r.w && jj < r.h && prev[jj * r.w + ii] >= 0) { lab[j * r.w + i] = prev[jj * r.w + ii]; break; }
          }
        }
      }
      M.fitted = true; M.theta = (r.rot || 0) * RAD; M.ox = r.ox; M.oy = r.oy; M.step = r.step; M.w = r.w; M.h = r.h; M.lab = lab;
      M.planes = fit.planes.map(function (p) { return { a: +p.a || 0, b: +p.b || 0, c: +p.c || 0, src: p }; });
      (Array.isArray(fit.pairs) ? fit.pairs : []).forEach(function (p) { M.rules[p[0] + '|' + p[1]] = p[2]; M.rules[p[1] + '|' + p[0]] = p[2]; });
    } else {
      var c0 = H3.ringCentroid(ring);
      M.theta = 0; M.ox = c0[0]; M.oy = c0[1]; M.step = 1; M.w = 1; M.h = 1; M.lab = new Int16Array(1);
      M.planes = [{ a: 0, b: 0, c: blockHeight(b), src: null }];
    }
    M.world = function (fu, fv) { var w2 = H3.rot2(fu * M.step, fv * M.step, M.theta); return [M.ox + w2[0], M.oy + w2[1]]; };
    M.ringF = ring.map(function (p) { var q3 = H3.rot2(p[0] - M.ox, p[1] - M.oy, -M.theta); return [q3[0] / M.step, q3[1] / M.step]; });
    M.lattice = function (e, n) { var q4 = H3.rot2(e - M.ox, n - M.oy, -M.theta); return [q4[0] / M.step, q4[1] / M.step]; };
    M.hp = function (p, x, y) { var pl = M.planes[p]; return pl.a * x + pl.b * y + pl.c; };
    M.rule = function (a, c2) { var v2 = M.rules[a + '|' + c2]; return v2 == null ? null : v2; };
    M.labelAt = function (i2, j2) { var v3 = M.lab[clamp(j2, 0, M.h - 1) * M.w + clamp(i2, 0, M.w - 1)]; return v3 < 0 ? 0 : v3; };
    /** Planes that can own part of the cell around node (i, j): its own and the creased neighbours. */
    M.cands = function (i3, j3) {
      var own = M.labelAt(i3, j3), cs = [own];
      if (M.fitted) for (var dj2 = -1; dj2 <= 1; dj2++) for (var di2 = -1; di2 <= 1; di2++) {
        var i4 = i3 + di2, j4 = j3 + dj2;
        if (i4 < 0 || j4 < 0 || i4 >= M.w || j4 >= M.h) continue;
        var m = M.lab[j4 * M.w + i4];
        if (m >= 0 && cs.indexOf(m) < 0 && M.rule(own, m) != null) cs.push(m);
      }
      cs.sort(function (x, y) { return x - y; });
      cs.own = own;
      return cs;
    };
    /** Same rule as `crease_winner` on the server: every creased pair plays once, most wins
     *  takes the point, the cell's own plane on a tie. */
    M.winner = function (cs, own, x, y) {
      if (cs.length < 2) return own;
      var wins = [], k2, a, c3;
      for (k2 = 0; k2 < cs.length; k2++) wins.push(0);
      for (a = 0; a < cs.length; a++) for (c3 = a + 1; c3 < cs.length; c3++) {
        var rule = M.rule(cs[a], cs[c3]);
        if (rule == null) continue;
        var ha = M.hp(cs[a], x, y), hc = M.hp(cs[c3], x, y);
        wins[(rule === 0 ? ha <= hc : ha >= hc) ? a : c3]++;
      }
      var best = Math.max(0, cs.indexOf(own));
      for (k2 = 0; k2 < cs.length; k2++) if (wins[k2] > wins[best]) best = k2;
      return cs[best];
    };
    /** Roof height (above the building base) and owning plane at lattice point (x, y). */
    M.surf = function (x, y) {
      var cs = M.cands(clamp(Math.round(x), 0, M.w - 1), clamp(Math.round(y), 0, M.h - 1)), p = M.winner(cs, cs.own, x, y);
      return { p: p, h: M.hp(p, x, y) };
    };
    return M;
  }

  /** Sutherland-Hodgman clip of a polygon (lattice units) against the unit cell at (i, j). */
  function clipToCell(poly, i, j) {
    var planes = [[1, 0, i], [-1, 0, -(i + 1)], [0, 1, j], [0, -1, -(j + 1)]];
    var out = poly;
    for (var k = 0; k < 4 && out.length; k++) {
      var a = planes[k][0], b = planes[k][1], c = planes[k][2], inp = out;
      out = [];
      for (var m = 0; m < inp.length; m++) {
        var p = inp[m], q = inp[(m + 1) % inp.length];
        var dp = a * p[0] + b * p[1] - c, dq = a * q[0] + b * q[1] - c;
        if (dp >= 0) out.push(p);
        if ((dp >= 0) !== (dq >= 0)) { var t = dp / (dp - dq); out.push([p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t]); }
      }
    }
    // Drop repeated points left by the clip.
    var clean = [];
    for (var n = 0; n < out.length; n++) {
      var pr = clean[clean.length - 1];
      if (!pr || Math.abs(pr[0] - out[n][0]) > 1e-7 || Math.abs(pr[1] - out[n][1]) > 1e-7) clean.push(out[n]);
    }
    if (clean.length > 1 && Math.abs(clean[0][0] - clean[clean.length - 1][0]) < 1e-7 && Math.abs(clean[0][1] - clean[clean.length - 1][1]) < 1e-7) clean.pop();
    return clean;
  }
  /** Part of a polygon on one side of the line A·x + B·y + C = 0 (`sign` +1 or -1). */
  function clipHalf(poly, A, B, C, sign) {
    var out = [];
    for (var m = 0; m < poly.length; m++) {
      var p = poly[m], q = poly[(m + 1) % poly.length];
      var dp = sign * (A * p[0] + B * p[1] + C), dq = sign * (A * q[0] + B * q[1] + C);
      if (dp >= 0) out.push(p);
      if ((dp >= 0) !== (dq >= 0)) { var t = dp / (dp - dq); out.push([p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t]); }
    }
    return out;
  }

  /** Parameters (0..1, sorted, deduplicated) where the segment a->b meets a lattice line, plus its ends. */
  function edgeParams(a, b) {
    var ts = [0, 1];
    var add = function (va, vb) {
      if (Math.abs(vb - va) < 1e-9) return;
      var lo = Math.min(va, vb), hi = Math.max(va, vb);
      for (var m = Math.ceil(lo + 1e-9); m <= Math.floor(hi - 1e-9); m++) ts.push((m - va) / (vb - va));
    };
    add(a[0], b[0]); add(a[1], b[1]);
    ts.sort(function (x, y) { return x - y; });
    var out = [ts[0]];
    for (var i = 1; i < ts.length; i++) if (ts[i] - out[out.length - 1] > 1e-7) out.push(ts[i]);
    return out;
  }

  /** Roof + wall triangles for one building.
   *  The roof is cut into flat faces: each lattice cell is drawn on its plane and, where two
   *  planes meet, split on their exact intersection line, so ridges, hips and valleys come out
   *  as straight creases (`crease` carries them as line segments). Steps between roof levels
   *  get a vertical face. Walls rise from just below the terrain to the roof edge. `triPlane`
   *  gives the plane of every roof triangle (for picking a face). */
  function buildingArrays(THREE, b, terr, uvOf) {
    var M = roofModel(b), ringF = M.ringF, W = M.w, Hn = M.h, base = isNum(b.base) ? b.base : 0, rn = ringF.length;
    var roofPos = [], roofNor = [], roofUv = [], triPlane = [], crease = [], top = -Infinity;
    var wallPos = [], wallNor = [], wallCol = [];
    var planeN = M.planes.map(function (pl) {
      var gu = pl.a / M.step, gv = pl.b / M.step, g = H3.rot2(-gu, -gv, M.theta), inv = 1 / Math.sqrt(gu * gu + gv * gv + 1);
      return [g[0] * inv, inv, -g[1] * inv];
    });
    var P3 = function (p, x, y) { var w = M.world(x, y); return [w[0], base + M.hp(p, x, y), -w[1]]; };
    var vert = function (p, x, y) {
      var w = M.world(x, y), yy = base + M.hp(p, x, y), n = planeN[p], uv = uvOf(w[0], w[1]);
      if (yy > top) top = yy;
      roofPos.push(w[0], yy, -w[1]); roofNor.push(n[0], n[1], n[2]); roofUv.push(uv[0], uv[1]);
    };
    var tri = function (p, a, c, d) {
      // Keep counter-clockwise order seen from above so the faces look up.
      var cr = (c[0] - a[0]) * (d[1] - a[1]) - (c[1] - a[1]) * (d[0] - a[0]);
      if (Math.abs(cr) < 1e-9) return;
      vert(p, a[0], a[1]);
      if (cr > 0) { vert(p, c[0], c[1]); vert(p, d[0], d[1]); } else { vert(p, d[0], d[1]); vert(p, c[0], c[1]); }
      triPlane.push(p);
    };
    var emitPoly = function (p, poly, convex) {
      if (poly.length < 3) return;
      if (convex || poly.length === 3) { for (var k = 1; k < poly.length - 1; k++) tri(p, poly[0], poly[k], poly[k + 1]); return; }
      var tris = THREE.ShapeUtils.triangulateShape(poly.map(function (q) { return new THREE.Vector2(q[0], q[1]); }), []);
      for (var t = 0; t < tris.length; t++) tri(p, poly[tris[t][0]], poly[tris[t][1]], poly[tris[t][2]]);
    };
    var seg = function (p, a, c) { var A = P3(p, a[0], a[1]), C = P3(p, c[0], c[1]); crease.push(A[0], A[1] + 0.012, A[2], C[0], C[1] + 0.012, C[2]); };
    var pixCands = M.fitted ? new Array(W * Hn) : null;
    var candsOf = function (i, j) {
      if (!pixCands) return M.cands(0, 0);
      i = clamp(i, 0, W - 1); j = clamp(j, 0, Hn - 1);
      var k = j * W + i, c = pixCands[k];
      if (!c) c = pixCands[k] = M.cands(i, j);
      return c;
    };
    var surfIn = function (i, j, x, y) { var cs = candsOf(i, j), p = cs.length > 1 ? M.winner(cs, cs.own, x, y) : cs[0]; return { p: p, h: M.hp(p, x, y) }; };

    if (M.fitted) {
      // Cell (i, j) is the unit square centred on lattice node (i, j).
      var ringS = ringF.map(function (p) { return [p[0] + 0.5, p[1] + 0.5]; });
      var drawn = new Uint8Array(W * Hn), touch = new Uint8Array(W * Hn);
      var mark = function (cx, cy) { if (cx >= 0 && cy >= 0 && cx < W && cy < Hn) touch[cy * W + cx] = 1; };
      for (var e = 0; e < rn; e++) {
        var p0 = ringS[e], p1 = ringS[(e + 1) % rn], ts0 = edgeParams(p0, p1);
        for (var s = 0; s < ts0.length - 1; s++) {
          var tm = (ts0[s] + ts0[s + 1]) / 2, fx = p0[0] + (p1[0] - p0[0]) * tm, fy = p0[1] + (p1[1] - p0[1]) * tm;
          var rx = Math.round(fx), ry = Math.round(fy), onU = Math.abs(fx - rx) < 1e-6, onV = Math.abs(fy - ry) < 1e-6;
          var xs = onU ? [rx - 1, rx] : [Math.floor(fx)], ys = onV ? [ry - 1, ry] : [Math.floor(fy)];
          for (var xi = 0; xi < xs.length; xi++) for (var yi = 0; yi < ys.length; yi++) mark(xs[xi], ys[yi]);
        }
      }
      var emitCell = function (i, j, poly, convex) {
        var cs = candsOf(i, j);
        if (cs.length === 1) { emitPoly(cs[0], poly, convex); return; }
        var lines = [], pieces = [poly];
        for (var a = 0; a < cs.length; a++) for (var c = a + 1; c < cs.length; c++) {
          if (M.rule(cs[a], cs[c]) == null) continue;
          var pa = M.planes[cs[a]], pc = M.planes[cs[c]], L = [pa.a - pc.a, pa.b - pc.b, pa.c - pc.c, cs[a], cs[c]];
          if (Math.abs(L[0]) + Math.abs(L[1]) < 1e-9) continue;
          lines.push(L);
          var next = [];
          for (var q = 0; q < pieces.length; q++) {
            var neg = clipHalf(pieces[q], L[0], L[1], L[2], 1), pos = clipHalf(pieces[q], L[0], L[1], L[2], -1);
            if (neg.length >= 3) next.push(neg);
            if (pos.length >= 3) next.push(pos);
          }
          pieces = next;
        }
        pieces.forEach(function (pc3) {
          var cx = 0, cy = 0;
          pc3.forEach(function (q2) { cx += q2[0]; cy += q2[1]; });
          cx /= pc3.length; cy /= pc3.length;
          var p = M.winner(cs, cs.own, cx, cy);
          emitPoly(p, pc3, convex);
          for (var k = 0; k < pc3.length; k++) {
            var q0 = pc3[k], q1 = pc3[(k + 1) % pc3.length], dx = q1[0] - q0[0], dy = q1[1] - q0[1], len = Math.hypot(dx, dy);
            if (len < 1e-6) continue;
            for (var li = 0; li < lines.length; li++) {
              var Ln = lines[li];
              if (Ln[3] !== p && Ln[4] !== p) continue;
              if (Math.abs(Ln[0] * q0[0] + Ln[1] * q0[1] + Ln[2]) > 1e-6 || Math.abs(Ln[0] * q1[0] + Ln[1] * q1[1] + Ln[2]) > 1e-6) continue;
              // Which plane lies across this edge? A different one makes it a ridge, hip or valley.
              var mx = (q0[0] + q1[0]) / 2, my = (q0[1] + q1[1]) / 2, nx = -dy / len, ny = dx / len;
              if ((cx - mx) * nx + (cy - my) * ny > 0) { nx = -nx; ny = -ny; }
              var tx = mx + nx * 0.02, ty = my + ny * 0.02;
              if (Math.abs(tx - i) > 0.5 || Math.abs(ty - j) > 0.5) break;
              var p2 = M.winner(cs, cs.own, tx, ty);
              if (p2 !== p && p < p2) seg(p, q0, q1);
              break;
            }
          }
        });
      };
      for (var cj = 0; cj < Hn; cj++) for (var ci = 0; ci < W; ci++) {
        var poly, convex = true;
        if (touch[cj * W + ci]) {
          poly = clipToCell(ringS, ci, cj);
          if (poly.length < 3) continue;
          poly = poly.map(function (q) { return [q[0] - 0.5, q[1] - 0.5]; });
          convex = false;
        } else if (H3.pointInRing(ci, cj, ringF)) {
          poly = [[ci - 0.5, cj - 0.5], [ci + 0.5, cj - 0.5], [ci + 0.5, cj + 0.5], [ci - 0.5, cj + 0.5]];
        } else continue;
        drawn[cj * W + ci] = 1;
        emitCell(ci, cj, poly, convex);
      }
      // Cell borders: a height jump is a step between roof levels (vertical face); two planes
      // meeting at the same height is a crease that happens to run along the lattice.
      var border = function (i, j, i2, j2, A, B) {
        var c1 = candsOf(i, j), c2 = candsOf(i2, j2);
        if (c1.length === 1 && c2.length === 1 && c1[0] === c2[0]) return;
        var partial = touch[j * W + i] || touch[j2 * W + i2];
        for (var hf = 0; hf < 2; hf++) {
          var a = [A[0] + (B[0] - A[0]) * hf * 0.5, A[1] + (B[1] - A[1]) * hf * 0.5], c = [A[0] + (B[0] - A[0]) * (hf + 1) * 0.5, A[1] + (B[1] - A[1]) * (hf + 1) * 0.5];
          var m = [(a[0] + c[0]) / 2, (a[1] + c[1]) / 2];
          if (partial && !H3.pointInRing(m[0], m[1], ringF)) continue;
          var l0 = surfIn(i, j, a[0], a[1]), l1 = surfIn(i, j, c[0], c[1]), r0 = surfIn(i2, j2, a[0], a[1]), r1 = surfIn(i2, j2, c[0], c[1]);
          var d0 = l0.h - r0.h, d1 = l1.h - r1.h;
          if (Math.abs(d0) > 0.03 || Math.abs(d1) > 0.03) {
            var wa = M.world(a[0], a[1]), wc = M.world(c[0], c[1]);
            var q = [[wa[0], base + l0.h, -wa[1]], [wc[0], base + l1.h, -wc[1]], [wc[0], base + r1.h, -wc[1]], [wa[0], base + r0.h, -wa[1]]];
            var ex = wc[0] - wa[0], ez = wa[1] - wc[1], el = Math.hypot(ex, ez) || 1, nx = -ez / el, nz = ex / el;
            // The face looks toward the lower side.
            var w2 = M.world(i2, j2), toward2 = ((w2[0] - wa[0]) * nx + (-w2[1] + wa[1]) * nz) > 0 ? 1 : -1, sg = (d0 + d1 > 0 ? 1 : -1) * toward2;
            [q[0], q[1], q[2], q[0], q[2], q[3]].forEach(function (v) { wallPos.push(v[0], v[1], v[2]); wallNor.push(nx * sg, 0, nz * sg); wallCol.push(1, 1, 1); });
            crease.push(q[0][0], q[0][1] + 0.012, q[0][2], q[1][0], q[1][1] + 0.012, q[1][2], q[3][0], q[3][1] + 0.012, q[3][2], q[2][0], q[2][1] + 0.012, q[2][2]);
          } else {
            var pl = surfIn(i, j, m[0], m[1]).p, pr = surfIn(i2, j2, m[0], m[1]).p;
            if (pl !== pr) seg(pl, a, c);
          }
        }
      };
      for (var bj = 0; bj < Hn; bj++) for (var bi = 0; bi < W; bi++) {
        var bk = bj * W + bi;
        if (!drawn[bk]) continue;
        if (bi + 1 < W && drawn[bk + 1]) border(bi, bj, bi + 1, bj, [bi + 0.5, bj - 0.5], [bi + 0.5, bj + 0.5]);
        if (bj + 1 < Hn && drawn[bk + W]) border(bi, bj, bi, bj + 1, [bi - 0.5, bj + 0.5], [bi + 0.5, bj + 0.5]);
      }
    } else {
      emitPoly(0, ringF, false);
    }

    // Wall pieces along one footprint edge: cut at every cell border and at every crease the
    // edge crosses, so a gable wall follows the roof exactly up to its peak.
    var wallParams = function (a0, b0) {
      if (!M.fitted) return [0, 1];
      var ts = edgeParams([a0[0] + 0.5, a0[1] + 0.5], [b0[0] + 0.5, b0[1] + 0.5]), extra = [];
      for (var s2 = 0; s2 < ts.length - 1; s2++) {
        var tm2 = (ts[s2] + ts[s2 + 1]) / 2, cs = candsOf(Math.round(a0[0] + (b0[0] - a0[0]) * tm2), Math.round(a0[1] + (b0[1] - a0[1]) * tm2));
        if (cs.length < 2) continue;
        for (var a = 0; a < cs.length; a++) for (var c = a + 1; c < cs.length; c++) {
          if (M.rule(cs[a], cs[c]) == null) continue;
          var pa = M.planes[cs[a]], pc = M.planes[cs[c]];
          var f0 = (pa.a - pc.a) * a0[0] + (pa.b - pc.b) * a0[1] + (pa.c - pc.c), f1 = (pa.a - pc.a) * b0[0] + (pa.b - pc.b) * b0[1] + (pa.c - pc.c);
          if (Math.abs(f1 - f0) < 1e-12) continue;
          var t = f0 / (f0 - f1);
          if (t > ts[s2] + 1e-6 && t < ts[s2 + 1] - 1e-6) extra.push(t);
        }
      }
      return extra.length ? ts.concat(extra).sort(function (x, y) { return x - y; }) : ts;
    };
    // Walls, a thin roof-edge band and the ground-contact collar. For this home and its
    // garage/ADU the top face of the roof-edge band is kept apart (`capPos`): it is drawn as the
    // coloured roof-edge outline, the same legend as the 2D satellite map.
    var trimPos = [], trimNor = [], collarPos = [], capPos = [], capNor = [], eaveLine = [], corners = [];
    var sgn = H3.ringArea(b.ring) >= 0 ? 1 : -1, strong = b.role !== 'neighbor';
    var eNor = [], mVec = [];
    for (var ek = 0; ek < rn; ek++) {
      var ea = M.world(ringF[ek][0], ringF[ek][1]), eb = M.world(ringF[(ek + 1) % rn][0], ringF[(ek + 1) % rn][1]);
      var edx = eb[0] - ea[0], edy = eb[1] - ea[1], edl = Math.hypot(edx, edy) || 1;
      eNor.push([(edy / edl) * sgn, (edx / edl) * sgn]);
    }
    for (var vk = 0; vk < rn; vk++) {
      var n0 = eNor[(vk + rn - 1) % rn], n1 = eNor[vk];
      var sx = n0[0] + n1[0], sz = n0[1] + n1[1], sl = Math.hypot(sx, sz);
      if (sl < 1e-4) { mVec.push(n1); } else {
        var ux = sx / sl, uz = sz / sl, sc = clamp(1 / Math.max(0.56, ux * n1[0] + uz * n1[1]), 0.85, 1.75);
        mVec.push([ux * sc, uz * sc]);
      }
    }
    var pushQuad = function (P, N, C, q0, q1, q2, q3, nx, ny, nz, c0, c1, c2, c3) {
      if (sgn >= 0) {
        P.push(q0[0], q0[1], q0[2], q1[0], q1[1], q1[2], q2[0], q2[1], q2[2], q0[0], q0[1], q0[2], q2[0], q2[1], q2[2], q3[0], q3[1], q3[2]);
        if (C) C.push(c0, c0, c0, c1, c1, c1, c2, c2, c2, c0, c0, c0, c2, c2, c2, c3, c3, c3);
      } else {
        P.push(q0[0], q0[1], q0[2], q2[0], q2[1], q2[2], q1[0], q1[1], q1[2], q0[0], q0[1], q0[2], q3[0], q3[1], q3[2], q2[0], q2[1], q2[2]);
        if (C) C.push(c0, c0, c0, c2, c2, c2, c1, c1, c1, c0, c0, c0, c3, c3, c3, c2, c2, c2);
      }
      if (N) for (var r = 0; r < 6; r++) N.push(nx, ny, nz);
    };
    var oh = strong ? 0.2 : 0.1, fh = strong ? 0.13 : 0.09, po = 0.04, cw = 0.34;
    for (var k = 0; k < rn; k++) {
      var a0 = ringF[k], b0 = ringF[(k + 1) % rn], ts = wallParams(a0, b0), pv = ringF[(k + rn - 1) % rn];
      var nxw = eNor[k][0], nzw = eNor[k][1], m0 = mVec[k], m1 = mVec[(k + 1) % rn];
      var ax = a0[0] - pv[0], ay = a0[1] - pv[1], bx = b0[0] - a0[0], by = b0[1] - a0[1];
      var turn = Math.abs(Math.atan2(ax * by - ay * bx, ax * bx + ay * by));
      for (var tt = 0; tt < ts.length - 1; tt++) {
        var tmid = (ts[tt] + ts[tt + 1]) / 2, pi = Math.round(a0[0] + (b0[0] - a0[0]) * tmid), pj = Math.round(a0[1] + (b0[1] - a0[1]) * tmid);
        var at = function (t) {
          var fu = a0[0] + (b0[0] - a0[0]) * t, fv = a0[1] + (b0[1] - a0[1]) * t, w = M.world(fu, fv), gh = terr.h(w[0], w[1]);
          return { x: w[0], z: -w[1], gh: gh, top: base + surfIn(pi, pj, fu, fv).h, bot: Math.min(gh, base) - 0.35, ox: m0[0] * (1 - t) + m1[0] * t, oz: m0[1] * (1 - t) + m1[1] * t };
        };
        var prev = at(ts[tt]), cur = at(ts[tt + 1]);
        if (prev.top > top) top = prev.top;
        if (cur.top > top) top = cur.top;
        // Soft ambient shading baked into the wall: darker at the ground contact and under the roof edge.
        var yLo0 = Math.min(prev.top - 0.4, prev.gh + 0.45), yLo1 = Math.min(cur.top - 0.4, cur.gh + 0.45);
        var yHi0 = Math.max(yLo0 + 0.05, prev.top - 0.36), yHi1 = Math.max(yLo1 + 0.05, cur.top - 0.36);
        pushQuad(wallPos, wallNor, wallCol, [prev.x, prev.bot, prev.z], [cur.x, cur.bot, cur.z], [cur.x, yLo1, cur.z], [prev.x, yLo0, prev.z], nxw, 0, nzw, 0.84, 0.84, 1.0, 1.0);
        pushQuad(wallPos, wallNor, wallCol, [prev.x, yLo0, prev.z], [cur.x, yLo1, cur.z], [cur.x, yHi1, cur.z], [prev.x, yHi0, prev.z], nxw, 0, nzw, 1.0, 1.0, 1.0, 1.0);
        pushQuad(wallPos, wallNor, wallCol, [prev.x, yHi0, prev.z], [cur.x, yHi1, cur.z], [cur.x, cur.top, cur.z], [prev.x, prev.top, prev.z], nxw, 0, nzw, 1.0, 1.0, 0.86, 0.86);
        // Roof-edge band: outer face, top face (the outline) and underside.
        var pInT = [prev.x, prev.top + 0.015, prev.z], cInT = [cur.x, cur.top + 0.015, cur.z];
        var pInB = [prev.x, prev.top - fh, prev.z], cInB = [cur.x, cur.top - fh, cur.z];
        var pOutT = [prev.x + prev.ox * oh, prev.top + 0.02, prev.z + prev.oz * oh], cOutT = [cur.x + cur.ox * oh, cur.top + 0.02, cur.z + cur.oz * oh];
        var pOutB = [prev.x + prev.ox * oh, prev.top - fh, prev.z + prev.oz * oh], cOutB = [cur.x + cur.ox * oh, cur.top - fh, cur.z + cur.oz * oh];
        pushQuad(trimPos, trimNor, null, pOutB, cOutB, cOutT, pOutT, nxw, 0, nzw);
        if (strong) pushQuad(capPos, capNor, null, pOutT, cOutT, cInT, pInT, 0, 1, 0);
        else pushQuad(trimPos, trimNor, null, pOutT, cOutT, cInT, pInT, 0, 1, 0);
        pushQuad(trimPos, trimNor, null, pInB, cInB, cOutB, pOutB, 0, -1, 0);
        if (strong) {
          // Gold / orchid ground-contact collar matching the 2D map's outline colours.
          pushQuad(collarPos, null, null,
            [prev.x + prev.ox * (po + cw), prev.gh + 0.045, prev.z + prev.oz * (po + cw)],
            [cur.x + cur.ox * (po + cw), cur.gh + 0.045, cur.z + cur.oz * (po + cw)],
            [cur.x + cur.ox * po, cur.gh + 0.055, cur.z + cur.oz * po],
            [prev.x + prev.ox * po, prev.gh + 0.055, prev.z + prev.oz * po], 0, 1, 0);
        }
        eaveLine.push(pOutT[0], pOutT[1] + 0.01, pOutT[2], cOutT[0], cOutT[1] + 0.01, cOutT[2]);
        if (tt === 0 && turn > 0.35) corners.push(prev.x, prev.gh, prev.z, prev.x, prev.top - fh, prev.z);
      }
    }
    return {
      roofPos: roofPos, roofNor: roofNor, roofUv: roofUv, triPlane: triPlane, crease: crease,
      wallPos: wallPos, wallNor: wallNor, wallCol: wallCol,
      trimPos: trimPos, trimNor: trimNor, collarPos: collarPos, capPos: capPos, capNor: capNor,
      eaveLine: eaveLine, corners: corners, top: top, fitted: M.fitted, model: M,
    };
  }

  /* ---------------------------------------------------------------- tree symbols */
  function mergeParts(THREE, parts) {
    var posLen = 0, list = [];
    parts.forEach(function (g) {
      var gi = g.index ? g.toNonIndexed() : g;
      list.push({ g: g, gi: gi, p: gi.attributes.position.array, n: gi.attributes.normal.array, c: gi.attributes.color ? gi.attributes.color.array : null });
      posLen += gi.attributes.position.array.length;
    });
    var pos = new Float32Array(posLen), nor = new Float32Array(posLen), col = new Float32Array(posLen), off = 0;
    list.forEach(function (it) {
      pos.set(it.p, off); nor.set(it.n, off);
      if (it.c) col.set(it.c, off); else for (var k = 0; k < it.p.length; k++) col[off + k] = 1;
      off += it.p.length;
      if (it.gi !== it.g) it.gi.dispose();
      it.g.dispose();
    });
    var out = new THREE.BufferGeometry();
    out.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    out.setAttribute('normal', new THREE.BufferAttribute(nor, 3));
    out.setAttribute('color', new THREE.BufferAttribute(col, 3));
    return out;
  }
  /** Unit tree symbol: crown radius 1, height 1. Each instance is scaled by the tree's measured
   *  crown radius (x, z) and measured height (y), so the symbol's size is the measurement.
   *  `kind` only varies the outline a little (0 round, 1 flatter underside, 2 tapering for a
   *  crown measured tall and narrow); it is a drawing style, not a species. */
  function treeSymbol(THREE, kind) {
    var paint = function (g, r, gr, bl) {
      var n = g.attributes.position.count, cols = new Float32Array(n * 3);
      for (var i = 0; i < n; i++) { cols[i * 3] = r; cols[i * 3 + 1] = gr; cols[i * 3 + 2] = bl; }
      g.setAttribute('color', new THREE.BufferAttribute(cols, 3));
      return g;
    };
    var crown;
    if (kind === 2) {
      crown = new THREE.CylinderGeometry(0.16, 1, 0.8, 7, 1);
      crown.translate(0, 0.6, 0);
    } else {
      crown = new THREE.IcosahedronGeometry(1, 1);
      var pa = crown.attributes.position;
      for (var i = 0; i < pa.count; i++) {
        var x = pa.getX(i), y = pa.getY(i), z = pa.getZ(i);
        var wob = kind === 1 ? 1 + 0.09 * Math.sin(x * 4.1 + z * 2.3) : 1 + 0.06 * Math.cos(x * 3.3 - z * 3.7 + y * 2);
        var yy = kind === 1 && y < 0 ? y * 0.72 : y;
        pa.setXYZ(i, x * wob, 0.62 + yy * 0.38, z * wob);
      }
      crown.computeVertexNormals();
    }
    var trunk = new THREE.CylinderGeometry(0.06, 0.08, 0.44, 6, 1);
    trunk.translate(0, 0.22, 0);
    return mergeParts(THREE, [paint(crown, 0.62, 0.74, 0.58), paint(trunk, 0.66, 0.6, 0.54)]);
  }
  /** Squared distance between segments p1-q1 and p2-q2. */
  function segSegDist2(p1x, p1y, p1z, q1x, q1y, q1z, p2x, p2y, p2z, q2x, q2y, q2z) {
    var d1x = q1x - p1x, d1y = q1y - p1y, d1z = q1z - p1z, d2x = q2x - p2x, d2y = q2y - p2y, d2z = q2z - p2z;
    var rx = p1x - p2x, ry = p1y - p2y, rz = p1z - p2z;
    var a = d1x * d1x + d1y * d1y + d1z * d1z, e = d2x * d2x + d2y * d2y + d2z * d2z, f = d2x * rx + d2y * ry + d2z * rz, s, t;
    if (a <= 1e-12 && e <= 1e-12) return rx * rx + ry * ry + rz * rz;
    if (a <= 1e-12) { s = 0; t = clamp(f / e, 0, 1); } else {
      var c = d1x * rx + d1y * ry + d1z * rz;
      if (e <= 1e-12) { t = 0; s = clamp(-c / a, 0, 1); } else {
        var bb = d1x * d2x + d1y * d2y + d1z * d2z, den = a * e - bb * bb;
        s = den > 1e-12 ? clamp((bb * f - c * e) / den, 0, 1) : 0;
        t = (bb * s + f) / e;
        if (t < 0) { t = 0; s = clamp(-c / a, 0, 1); } else if (t > 1) { t = 1; s = clamp((bb - c) / a, 0, 1); }
      }
    }
    var dx = rx + d1x * s - d2x * t, dy = ry + d1y * s - d2y * t, dz = rz + d1z * s - d2z * t;
    return dx * dx + dy * dy + dz * dz;
  }
  H3._segSegDist2 = segSegDist2;
  /** Vertical capsule that stands in for a tree when testing whether it hides the home:
   *  `[e, n, height, crownRadius, groundZ]` -> { x, z, g (ground), y0..y1 (crown axis), r }. */
  function treeCapsule(t, groundZ) {
    var h = t[2], r = isNum(t[3]) && t[3] > 0 ? Math.max(0.5, t[3]) : Math.max(0.5, h * 0.25), rr = Math.min(r, 0.38 * h);
    var y0 = groundZ + 0.24 * h + rr, y1 = groundZ + h - rr;
    if (y1 < y0) { y0 = y1 = groundZ + 0.62 * h; }
    return { x: t[0], z: -t[1], g: groundZ, y0: y0, y1: y1, r: r };
  }
  /** True when the tree stands on the sight line from the camera to any of the home's sample points. */
  function treeHides(cap, cam, pts) {
    var r2 = cap.r * cap.r * 1.16;
    for (var k = 0; k < pts.length; k += 3) {
      if (segSegDist2(cam[0], cam[1], cam[2], pts[k], pts[k + 1], pts[k + 2], cap.x, cap.y0, cap.z, cap.x, cap.y1, cap.z) < r2) return true;
      if (segSegDist2(cam[0], cam[1], cam[2], pts[k], pts[k + 1], pts[k + 2], cap.x, cap.g, cap.z, cap.x, cap.y0, cap.z) < 0.16) return true;
    }
    return false;
  }
  H3._treeCapsule = treeCapsule;
  H3._treeHides = treeHides;

  function makeHorizonTex(THREE) {
    // Grayscale alphaMap (Three.js reads the green channel: white = opaque, black = fully transparent).
    var cv = document.createElement('canvas');
    cv.width = 256; cv.height = 256;
    var ctx = cv.getContext && cv.getContext('2d');
    if (ctx) {
      ctx.fillStyle = '#000000';
      ctx.fillRect(0, 0, 256, 256);
      if (ctx.createRadialGradient) {
        var gr = ctx.createRadialGradient(128, 128, 18, 128, 128, 124);
        gr.addColorStop(0, '#f0f0f0');
        gr.addColorStop(0.35, '#969696');
        gr.addColorStop(0.68, '#282828');
        gr.addColorStop(1, '#000000');
        ctx.fillStyle = gr;
        ctx.fillRect(0, 0, 256, 256);
      }
    }
    var tex = new THREE.CanvasTexture(cv);
    return tex;
  }
  /** Simplifies a ring for dimensioning: drops vertices where the line barely turns. */
  function straightenRing(ring, maxTurnDeg) {
    var out = ring.slice(), changed = true, lim = (maxTurnDeg || 8) * RAD;
    while (changed && out.length > 3) {
      changed = false;
      for (var i = 0; i < out.length && out.length > 3; i++) {
        var p = out[(i + out.length - 1) % out.length], q = out[i], r = out[(i + 1) % out.length];
        var ax = q[0] - p[0], ay = q[1] - p[1], bx = r[0] - q[0], by = r[1] - q[1];
        if (Math.hypot(ax, ay) < 0.05 || Math.hypot(bx, by) < 0.05 || Math.abs(Math.atan2(ax * by - ay * bx, ax * bx + ay * by)) < lim) { out.splice(i, 1); changed = true; i--; }
      }
    }
    return out;
  }
  /** Closest pair of points between two rings (exact for rings that do not cross). */
  function ringGap(r1, r2) {
    var best = { d: Infinity, a: null, b: null };
    var scan = function (pts, ring, flip) {
      for (var i = 0; i < pts.length; i++) {
        var x = pts[i][0], y = pts[i][1];
        for (var k = 0, n = ring.length; k < n; k++) {
          var a = ring[k], b = ring[(k + 1) % n], dx = b[0] - a[0], dy = b[1] - a[1], l2 = dx * dx + dy * dy;
          var t = l2 < 1e-12 ? 0 : clamp(((x - a[0]) * dx + (y - a[1]) * dy) / l2, 0, 1), qx = a[0] + t * dx, qy = a[1] + t * dy, d = Math.hypot(x - qx, y - qy);
          if (d < best.d) best = flip ? { d: d, a: [qx, qy], b: [x, y] } : { d: d, a: [x, y], b: [qx, qy] };
        }
      }
    };
    scan(r1, r2, false); scan(r2, r1, true);
    return best;
  }
  H3._straightenRing = straightenRing;
  H3._ringGap = ringGap;

  /* ---------------------------------------------------------------- viewer */
  H3.createViewer = function (THREE, els, opts) {
    opts = opts || {};
    var renderer = new THREE.WebGLRenderer({ canvas: els.canvas, antialias: true, alpha: true, powerPreference: 'high-performance' });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    renderer.outputColorSpace = THREE.SRGBColorSpace;
    // Neutral tone mapping keeps the white model white; older engines fall back to filmic.
    if (THREE.NeutralToneMapping != null) renderer.toneMapping = THREE.NeutralToneMapping;
    else if (THREE.ACESFilmicToneMapping != null) renderer.toneMapping = THREE.ACESFilmicToneMapping;
    renderer.toneMappingExposure = 1.0;
    renderer.shadowMap.enabled = true;
    renderer.shadowMap.type = THREE.PCFSoftShadowMap;
    // Shadows only change with the sun or the scene, not with the camera.
    renderer.shadowMap.autoUpdate = false;
    renderer.setClearColor(0x000000, 0);
    var scene3 = new THREE.Scene();
    scene3.fog = new THREE.FogExp2(0xdce9f6, 0.0023);
    var camera = new THREE.PerspectiveCamera(35, 1, 1, 2400);
    var hemi = new THREE.HemisphereLight(0xffffff, 0xe4dfd6, 1.9);
    scene3.add(hemi);
    var fill = new THREE.DirectionalLight(0xe6eefa, 0.5);
    scene3.add(fill);
    var sun = new THREE.DirectionalLight(0xfff6e8, 3.0);
    sun.castShadow = true;
    sun.shadow.mapSize.set(4096, 4096);
    sun.shadow.camera.near = 20; sun.shadow.camera.far = 680;
    sun.shadow.camera.left = -78; sun.shadow.camera.right = 78; sun.shadow.camera.top = 78; sun.shadow.camera.bottom = -78;
    sun.shadow.bias = -0.00035; sun.shadow.normalBias = 0.038;
    if ('radius' in sun.shadow) sun.shadow.radius = 2.4;
    scene3.add(sun); scene3.add(sun.target);
    var root = new THREE.Group();
    scene3.add(root);

    var data = null, terr = terrainSampler(null), groups = {}, labels = [], pickables = [], disposables = [];
    var layers = { aerial: true, roofphoto: false, trees: true, neighbors: true, shadows: true, labels: true, lot: true, outlines: true };
    var aerial = null, aerialKey = '', aerialState = 'none';
    var sunMs = Date.now(), sunInfo = { altitude: 0.8, bearing: 180 };
    var cam = { b: 150, p: 58, r: 70, tx: 0, ty: 2, tz: 0 }, goal = { b: 150, p: 58, r: 70, tx: 0, ty: 2, tz: 0 }, home = null;
    var dirty = true, raf = 0, disposed = false, size = { w: 0, h: 0 }, userMoved = false, inView = true, orbitT0 = 0;
    var mats = {}, horizonTex = null, contactTex = null;
    var subject = null, homePts = [], treeSets = [], measure = 'none', mInfo = {}, hiMesh = null, hiKey = '';
    var ZERO = new THREE.Matrix4().makeScale(0, 0, 0);

    function track(o) { disposables.push(o); return o; }
    function requestRender() {
      dirty = true;
      if (!raf && !disposed) raf = window.requestAnimationFrame(frame);
    }
    function shadowsDirty() { renderer.shadowMap.needsUpdate = true; }

    /* ---- aerial imagery ---- */
    function ensureAerial(origin) {
      var cfg = H3.resolveSatelliteConfig(H3.getReport(), origin.lat, origin.lon);
      var z = Math.min(cfg.maxNativeZoom || 16, H3.IMG_ZOOM);
      var key = (cfg.tileUrl || '') + '@' + z + ':' + origin.lat.toFixed(6) + ',' + origin.lon.toFixed(6);
      if (aerial && aerialKey === key) return aerial;
      if (aerial) { aerial.tex.dispose(); aerial.texMuted.dispose(); }
      aerialKey = key;
      var n2 = Math.pow(2, z), frame2 = H3.localFrame(origin.lat, origin.lon);
      var tilePx = cfg.tileSize === 512 ? 512 : 256;
      var px = function (lon) { return ((lon + 180) / 360) * n2 * 256; };
      var py = function (lat) { var s = Math.tan(lat * RAD) + 1 / Math.cos(lat * RAD); return ((1 - Math.log(s) / Math.PI) / 2) * n2 * 256; };
      var nw = frame2.inv(-GROUND_HALF, GROUND_HALF), se = frame2.inv(GROUND_HALF, -GROUND_HALF);
      var x0 = Math.floor(px(nw[1]) / 256), x1 = Math.floor(px(se[1]) / 256), y0 = Math.floor(py(nw[0]) / 256), y1 = Math.floor(py(se[0]) / 256);
      var cols = x1 - x0 + 1, rows = y1 - y0 + 1;
      var cv = document.createElement('canvas');
      cv.width = cols * tilePx; cv.height = rows * tilePx;
      var ctx = cv.getContext('2d');
      ctx.fillStyle = '#d6dcd1'; ctx.fillRect(0, 0, cv.width, cv.height);
      var tex = new THREE.CanvasTexture(cv);
      tex.colorSpace = THREE.SRGBColorSpace;
      tex.anisotropy = Math.min(16, renderer.capabilities.getMaxAnisotropy());
      // Muted copy for neighbouring roofs when the photo is draped on roofs.
      var cvMuted = document.createElement('canvas');
      cvMuted.width = cv.width; cvMuted.height = cv.height;
      var ctxM = cvMuted.getContext('2d');
      ctxM.fillStyle = '#dde0e3'; ctxM.fillRect(0, 0, cv.width, cv.height);
      var texMuted = new THREE.CanvasTexture(cvMuted);
      texMuted.colorSpace = THREE.SRGBColorSpace;
      texMuted.anisotropy = tex.anisotropy;
      var total = cols * rows, ok = 0, bad = 0, usedFallback = false;
      aerialState = 'loading';
      var settle = function () {
        if (ok + bad < total) return;
        aerialState = ok > 0 ? 'ready' : 'failed';
        var activeAttr = usedFallback && ok > 0 ? (cfg.fallbackAttribution || cfg.attribution) : cfg.attribution;
        if (opts.onAerial) opts.onAerial(aerialState, activeAttr);
        if (aerialState === 'failed' && layers.aerial) { layers.aerial = false; applyStyle(); }
      };
      var drawTile = function (img, tx2, ty2, sub) {
        var dx = (tx2 - x0) * tilePx, dy = (ty2 - y0) * tilePx;
        if (sub) {
          ctx.drawImage(img, sub.sx, sub.sy, sub.sw, sub.sh, dx, dy, tilePx, tilePx);
          ctxM.globalAlpha = 1;
          ctxM.drawImage(img, sub.sx, sub.sy, sub.sw, sub.sh, dx, dy, tilePx, tilePx);
        } else {
          ctx.drawImage(img, dx, dy, tilePx, tilePx);
          ctxM.globalAlpha = 1;
          ctxM.drawImage(img, dx, dy, tilePx, tilePx);
        }
        // The ground is a backdrop for the model: slightly lifted and desaturated.
        ctx.globalAlpha = 0.16; ctx.fillStyle = '#f1f3ee'; ctx.fillRect(dx, dy, tilePx, tilePx); ctx.globalAlpha = 1;
        ctxM.globalAlpha = 0.38; ctxM.fillStyle = '#e5e7ea'; ctxM.fillRect(dx, dy, tilePx, tilePx); ctxM.globalAlpha = 1;
        tex.needsUpdate = true; texMuted.needsUpdate = true;
      };
      for (var ty = y0; ty <= y1; ty++) for (var tx = x0; tx <= x1; tx++) {
        (function (tx2, ty2) {
          var loadTile = function (url, sub, onFail) {
            var img = new Image();
            img.crossOrigin = 'anonymous';
            img.onload = function () {
              if (aerialKey !== key || disposed) return;
              try {
                if (sub) {
                  var natW = img.naturalWidth || img.width || 256;
                  var natH = img.naturalHeight || img.height || 256;
                  sub.sx = sub.rx * (natW / sub.div);
                  sub.sy = sub.ry * (natH / sub.div);
                  sub.sw = natW / sub.div;
                  sub.sh = natH / sub.div;
                }
                drawTile(img, tx2, ty2, sub);
                ok++;
              } catch (e) { bad++; }
              settle(); requestRender();
            };
            img.onerror = onFail;
            img.src = url;
          };
          loadTile(H3.formatTileUrl(cfg.tileUrl, z, ty2, tx2), null, function () {
            if (aerialKey !== key || disposed) return;
            if (cfg.fallbackTileUrl && cfg.fallbackTileUrl !== cfg.tileUrl) {
              var fz = Math.min(z, cfg.fallbackMaxNativeZoom || 16);
              var dz = Math.max(0, z - fz);
              var div = Math.pow(2, dz);
              var fx = Math.floor(tx2 / div), fy = Math.floor(ty2 / div);
              var sub = dz > 0 ? { div: div, rx: tx2 % div, ry: ty2 % div, sx: 0, sy: 0, sw: 256, sh: 256 } : null;
              usedFallback = true;
              loadTile(H3.formatTileUrl(cfg.fallbackTileUrl, fz, fy, fx), sub, function () {
                bad++; settle();
              });
            } else {
              bad++; settle();
            }
          });
        })(tx, ty);
      }
      aerial = {
        tex: tex, texMuted: texMuted,
        uv: function (e, n) { var ll = frame2.inv(e, n); return [(px(ll[1]) - x0 * 256) / (cols * 256), 1 - (py(ll[0]) - y0 * 256) / (rows * 256)]; },
      };
      return aerial;
    }

    /* ---- materials: a white "clay" model on the aerial ground ---- */
    function makeMaterials() {
      Object.keys(mats).forEach(function (k) { mats[k].dispose(); });
      if (!horizonTex) horizonTex = makeHorizonTex(THREE);
      var std = function (o) { return new THREE.MeshStandardMaterial(Object.assign({ roughness: 0.94, metalness: 0 }, o)); };
      var dbl = THREE.DoubleSide;
      var ghost = std({ color: 0xffffff, vertexColors: true, flatShading: true, roughness: 1, transparent: true, depthWrite: false });
      // Per-tree opacity for the trees that stand in front of the home.
      ghost.onBeforeCompile = function (sh) {
        sh.vertexShader = 'attribute float instAlpha;\nvarying float vInstAlpha;\n' + sh.vertexShader.replace('#include <begin_vertex>', '#include <begin_vertex>\n\tvInstAlpha = instAlpha;');
        sh.fragmentShader = 'varying float vInstAlpha;\n' + sh.fragmentShader.replace('#include <color_fragment>', '#include <color_fragment>\n\tdiffuseColor.a *= vInstAlpha;');
      };
      ghost.customProgramCacheKey = function () { return 'h3d-tree-ghost'; };
      mats = {
        groundAerial: std({ map: aerial ? aerial.tex : null, color: 0xffffff, roughness: 0.98, vertexColors: true }),
        groundPlain: std({ color: 0xe9ebe4, roughness: 1, vertexColors: true }),
        roofAerial: std({ map: aerial ? aerial.tex : null, color: 0xffffff, roughness: 0.85 }),
        roofAerialDim: std({ map: aerial ? aerial.texMuted : null, color: 0xf8fafc, roughness: 0.9 }),
        // This home in a restrained colour, everything else matte white.
        roofSubject: std({ color: 0x8fb4ad }),
        roofAccessory: std({ color: 0xc8bcd8 }),
        roofNeighbor: std({ color: 0xf6f6f3 }),
        wallSubject: std({ color: 0xfcf9f1, vertexColors: true, side: dbl }),
        wallAccessory: std({ color: 0xf8f4fa, vertexColors: true, side: dbl }),
        wallNeighbor: std({ color: 0xfafaf8, vertexColors: true, side: dbl }),
        trimSubject: std({ color: 0x6d958d, side: dbl }),
        trimAccessory: std({ color: 0xa594bd, side: dbl }),
        trimNeighbor: std({ color: 0xe9eae6, side: dbl }),
        collarSubject: new THREE.MeshBasicMaterial({ color: 0xf59e0b, transparent: true, opacity: 0.72, depthWrite: false, side: dbl }),
        collarAccessory: new THREE.MeshBasicMaterial({ color: 0xa855f7, transparent: true, opacity: 0.72, depthWrite: false, side: dbl }),
        street: std({ color: 0xd3d7dc, roughness: 0.96 }),
        path: std({ color: 0xe2ded3, roughness: 0.96 }),
        tree: std({ color: 0xffffff, vertexColors: true, flatShading: true, roughness: 1 }),
        treeGhost: ghost,
        // Invisible copy that only casts the shadow, so faded trees keep the sun study correct.
        treeCaster: new THREE.MeshBasicMaterial({ colorWrite: false, depthWrite: false }),
        contact: new THREE.MeshBasicMaterial({ map: contactTex, transparent: true, depthWrite: false, polygonOffset: true, polygonOffsetFactor: -1, polygonOffsetUnits: -1 }),
        lotCase: new THREE.MeshBasicMaterial({ color: 0x0c4a6e, transparent: true, opacity: 0.45, depthWrite: false, side: dbl }),
        lotDash: new THREE.MeshBasicMaterial({ color: 0x38bdf8, transparent: true, opacity: 0.95, depthWrite: false, side: dbl }),
        // Roof-edge outlines: yellow for this home, magenta for a garage / ADU on the lot — the
        // same legend as the satellite map at the top of the report. Unlit and not tone-mapped
        // so they read in shade and at dusk.
        outlineSubject: new THREE.MeshBasicMaterial({ color: OUTLINE.subject, side: dbl, toneMapped: false, polygonOffset: true, polygonOffsetFactor: -2, polygonOffsetUnits: -2 }),
        outlineAccessory: new THREE.MeshBasicMaterial({ color: OUTLINE.accessory, side: dbl, toneMapped: false, polygonOffset: true, polygonOffsetFactor: -2, polygonOffsetUnits: -2 }),
        outlineLineSubject: new THREE.LineBasicMaterial({ color: OUTLINE.subjectEdge, toneMapped: false }),
        outlineLineAccessory: new THREE.LineBasicMaterial({ color: OUTLINE.accessoryEdge, toneMapped: false }),
        // Thin edge lines: ridges, hips, valleys, roof edges and wall corners.
        edge: new THREE.LineBasicMaterial({ color: 0x334155, transparent: true, opacity: 0.72 }),
        edgeSoft: new THREE.LineBasicMaterial({ color: 0x64748b, transparent: true, opacity: 0.42 }),
        horizon: new THREE.MeshBasicMaterial({ color: 0xdce4da, alphaMap: horizonTex, transparent: true, depthWrite: false }),
        arrowHalo: new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.88, depthWrite: false, side: dbl }),
        arrow: new THREE.MeshBasicMaterial({ color: 0x0f766e, transparent: true, opacity: 0.94, depthWrite: false, side: dbl }),
        dim: new THREE.MeshBasicMaterial({ color: 0x0f172a, transparent: true, depthTest: false, depthWrite: false, toneMapped: false }),
        dimHalo: new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.85, depthTest: false, depthWrite: false, toneMapped: false }),
        faceHi: new THREE.MeshBasicMaterial({ color: 0x14b8a6, transparent: true, opacity: 0.45, depthWrite: false, side: dbl, polygonOffset: true, polygonOffsetFactor: -3, polygonOffsetUnits: -3 }),
      };
    }
    function roofMaterial(role) {
      if (layers.roofphoto && layers.aerial && aerial) return role === 'neighbor' ? mats.roofAerialDim : mats.roofAerial;
      return role === 'subject' ? mats.roofSubject : role === 'accessory' ? mats.roofAccessory : mats.roofNeighbor;
    }
    function applyStyle() {
      if (!groups.ground) return;
      groups.ground.children.forEach(function (m) { if (m.userData.kind === 'ground') m.material = layers.aerial && aerial ? mats.groundAerial : mats.groundPlain; });
      (groups.buildings ? groups.buildings.children : []).forEach(function (g) {
        g.children.forEach(function (m) {
          var kind = m.userData.kind;
          if (kind === 'roof') m.material = roofMaterial(g.userData.role);
          else if (kind === 'outline-cap' || kind === 'outline-line') m.material = layers.outlines ? m.userData.on : m.userData.off;
          else if (kind === 'collar') m.visible = layers.outlines;
        });
        if (g.userData.role === 'neighbor') g.visible = layers.neighbors;
      });
      if (groups.streets) groups.streets.visible = !(layers.aerial && aerial);
      if (groups.trees) groups.trees.visible = layers.trees;
      if (groups.lot) groups.lot.visible = layers.lot;
      sun.castShadow = layers.shadows && sunInfo.altitude > 0.02;
      els.labels.style.display = layers.labels ? '' : 'none';
      shadowsDirty();
      requestRender();
    }

    /* ---- scene construction ---- */
    function clearScene() {
      clearMeasure();
      setHighlight(null);
      Object.keys(groups).forEach(function (k) { root.remove(groups[k]); });
      disposables.forEach(function (o) { try { o.dispose(); } catch (e) { /* already gone */ } });
      disposables = []; groups = {}; pickables = []; treeSets = []; homePts = []; subject = null;
      labels.forEach(function (l) { if (l.el.parentNode) l.el.parentNode.removeChild(l.el); });
      labels = [];
    }
    function geomFrom(pos, nor, uv, col) {
      var g = track(new THREE.BufferGeometry());
      g.setAttribute('position', new THREE.Float32BufferAttribute(pos, 3));
      if (nor) g.setAttribute('normal', new THREE.Float32BufferAttribute(nor, 3));
      if (uv) g.setAttribute('uv', new THREE.Float32BufferAttribute(uv, 2));
      if (col) g.setAttribute('color', new THREE.Float32BufferAttribute(col, 3));
      return g;
    }
    function addLabel(text, e, n, y, cls) {
      var el = document.createElement('div');
      el.className = 'h3d-label ' + (cls || '');
      el.innerHTML = text;
      els.labels.appendChild(el);
      var l = { el: el, v: new THREE.Vector3(e, y, -n) };
      labels.push(l);
      return l;
    }
    function buildGround() {
      var grp = new THREE.Group(), step = terr.has ? 2 : 8, n = Math.round((2 * GROUND_HALF) / step) + 1;
      var pos = new Float32Array(n * n * 3), uv = new Float32Array(n * n * 2), col = new Float32Array(n * n * 3), idx = [];
      var lo = Infinity, fadeStart = GROUND_HALF - 28;
      for (var j = 0; j < n; j++) for (var i = 0; i < n; i++) {
        var e = -GROUND_HALF + i * step, nn = -GROUND_HALF + j * step, h = terr.h(e, nn), k = j * n + i;
        if (h < lo) lo = h;
        var edge = clamp((Math.max(Math.abs(e), Math.abs(nn)) - fadeStart) / 28, 0, 1);
        var s = edge * edge * (3 - 2 * edge);
        pos[k * 3] = e; pos[k * 3 + 1] = h - s * 0.45; pos[k * 3 + 2] = -nn;
        var t = aerial ? aerial.uv(e, nn) : [0, 0];
        uv[k * 2] = t[0]; uv[k * 2 + 1] = t[1];
        col[k * 3] = 1 - s * 0.12; col[k * 3 + 1] = 1 - s * 0.09; col[k * 3 + 2] = 1 - s * 0.11;
      }
      for (var jj = 0; jj < n - 1; jj++) for (var ii = 0; ii < n - 1; ii++) {
        var a = jj * n + ii, b = a + 1, c = a + n, d = c + 1;
        idx.push(a, b, d, a, d, c);
      }
      var g = track(new THREE.BufferGeometry());
      g.setAttribute('position', new THREE.BufferAttribute(pos, 3));
      g.setAttribute('uv', new THREE.BufferAttribute(uv, 2));
      g.setAttribute('color', new THREE.BufferAttribute(col, 3));
      g.setIndex(idx);
      g.computeVertexNormals();
      var mesh = new THREE.Mesh(g, layers.aerial && aerial ? mats.groundAerial : mats.groundPlain);
      mesh.receiveShadow = true; mesh.userData.kind = 'ground';
      grp.add(mesh);
      // Soft radial-alpha horizon skirt (560 m radius) that dissolves into the sky gradient without a hard seam.
      var far = new THREE.Mesh(track(new THREE.CircleGeometry(560, 64)), mats.horizon);
      far.rotation.x = -Math.PI / 2; far.position.y = lo - 0.5;
      grp.add(far);
      // Soft contact shadow: a blurred copy of the footprints laid on the ground under the model.
      if (contactTex) {
        var half = 64, cn = 33, cpos = [], cuv = [], cidx = [];
        for (var cj = 0; cj < cn; cj++) for (var ci = 0; ci < cn; ci++) {
          var ce = -half + (ci / (cn - 1)) * 2 * half, cnn = -half + (cj / (cn - 1)) * 2 * half;
          cpos.push(ce, terr.h(ce, cnn) + 0.04, -cnn); cuv.push((ce + half) / (2 * half), (cnn + half) / (2 * half));
        }
        for (var qj = 0; qj < cn - 1; qj++) for (var qi = 0; qi < cn - 1; qi++) { var qa = qj * cn + qi; cidx.push(qa, qa + 1, qa + cn + 1, qa, qa + cn + 1, qa + cn); }
        var cg = geomFrom(cpos, null, cuv);
        cg.setIndex(cidx);
        var cm = new THREE.Mesh(cg, mats.contact);
        cm.renderOrder = 1;
        grp.add(cm);
      }
      return grp;
    }
    function buildContactTex() {
      if (contactTex) { contactTex.dispose(); contactTex = null; }
      try {
        var cv = document.createElement('canvas'), S = 1024, half = 64, k = S / (2 * half);
        cv.width = S; cv.height = S;
        var ctx = cv.getContext && cv.getContext('2d');
        if (!ctx || !ctx.beginPath) return;
        // Only the blurred shadow is drawn: the shapes themselves sit far off the canvas.
        var far = 4096;
        ctx.shadowOffsetX = far; ctx.shadowOffsetY = 0; ctx.shadowBlur = 1.5 * k; ctx.shadowColor = 'rgba(15,23,42,0.5)';
        ctx.fillStyle = '#000';
        (data.buildings || []).forEach(function (b) {
          if (!Array.isArray(b.ring) || b.ring.length < 3) return;
          ctx.beginPath();
          b.ring.forEach(function (p, i) { var x = (p[0] + half) * k - far, y = (half - p[1]) * k; if (i) ctx.lineTo(x, y); else ctx.moveTo(x, y); });
          ctx.closePath(); ctx.fill();
        });
        contactTex = new THREE.CanvasTexture(cv);
        contactTex.colorSpace = THREE.SRGBColorSpace;
      } catch (e) { contactTex = null; }
    }
    /** Points on the home's roof and walls; a tree on the sight line to any of them is faded. */
    function homeSamples(b, M) {
      var pts = [], base = b.base || 0, ring = b.ring, bb = H3.ringBBox(ring), f;
      for (var k = 0; k < ring.length; k++) {
        var a = ring[k], c = ring[(k + 1) % ring.length], n = Math.max(1, Math.ceil(Math.hypot(c[0] - a[0], c[1] - a[1]) / 1.2));
        for (var s = 0; s < n; s++) {
          var x = a[0] + ((c[0] - a[0]) * s) / n, y = a[1] + ((c[1] - a[1]) * s) / n, g = terr.h(x, y);
          f = M.lattice(x, y);
          var top = base + M.surf(f[0], f[1]).h;
          pts.push(x, top, -y, x, g + 0.4, -y, x, (top + g) / 2, -y);
        }
      }
      for (var yy = bb[1]; yy <= bb[3]; yy += 1.5) for (var xx = bb[0]; xx <= bb[2]; xx += 1.5) {
        if (!H3.pointInRing(xx, yy, ring)) continue;
        f = M.lattice(xx, yy);
        pts.push(xx, base + M.surf(f[0], f[1]).h, -yy);
      }
      return pts;
    }
    function buildBuildings() {
      var grp = new THREE.Group();
      (data.buildings || []).forEach(function (b, bi) {
        if (!Array.isArray(b.ring) || b.ring.length < 3) return;
        var arr;
        try { arr = buildingArrays(THREE, b, terr, aerial ? aerial.uv : function () { return [0, 0]; }); } catch (e) { return; }
        if (!arr.roofPos.length) return;
        var g = new THREE.Group(), strong = b.role !== 'neighbor', isSubj = b.role === 'subject';
        g.userData.role = b.role; g.userData.index = bi;
        var pickInfo = { type: 'building', b: b, tri: arr.triPlane, model: arr.model };
        var hasHeight = isNum(b.eave) || isNum(b.ridge) || b.heightSource === 'lidar';
        var roof = new THREE.Mesh(geomFrom(arr.roofPos, arr.roofNor, arr.roofUv), roofMaterial(b.role));
        roof.castShadow = hasHeight; roof.receiveShadow = true; roof.userData = { kind: 'roof', pick: pickInfo, roof: true };
        var wmat = isSubj ? mats.wallSubject : b.role === 'accessory' ? mats.wallAccessory : mats.wallNeighbor;
        var wall = new THREE.Mesh(geomFrom(arr.wallPos, arr.wallNor, null, arr.wallCol), wmat);
        wall.castShadow = hasHeight; wall.receiveShadow = true; wall.userData = { kind: 'wall', pick: pickInfo };
        g.add(roof); g.add(wall);
        pickables.push(roof, wall);
        if (arr.trimPos.length) {
          var tmat = isSubj ? mats.trimSubject : b.role === 'accessory' ? mats.trimAccessory : mats.trimNeighbor;
          var trim = new THREE.Mesh(geomFrom(arr.trimPos, arr.trimNor), tmat);
          trim.castShadow = true; trim.receiveShadow = true; trim.userData = { kind: 'trim', pick: pickInfo };
          g.add(trim); pickables.push(trim);
        }
        if (arr.collarPos.length && strong) {
          var collar = new THREE.Mesh(geomFrom(arr.collarPos), isSubj ? mats.collarSubject : mats.collarAccessory);
          collar.userData = { kind: 'collar' };
          collar.visible = layers.outlines;
          g.add(collar);
        }
        if (strong && arr.capPos.length) {
          // Coloured roof-edge band (the top face of the roof edge) in the legend colour.
          var capOn = isSubj ? mats.outlineSubject : mats.outlineAccessory, capOff = isSubj ? mats.trimSubject : mats.trimAccessory;
          var cap = new THREE.Mesh(geomFrom(arr.capPos, arr.capNor), layers.outlines ? capOn : capOff);
          cap.userData = { kind: 'outline-cap', on: capOn, off: capOff, pick: pickInfo };
          g.add(cap); pickables.push(cap);
        }
        // Thin edge lines: roof edge, creases (ridge / hip / valley / step) and wall corners.
        var lineMat = strong ? mats.edge : mats.edgeSoft;
        if (arr.eaveLine.length) {
          var eOn = !strong ? lineMat : isSubj ? mats.outlineLineSubject : mats.outlineLineAccessory;
          var eave = new THREE.LineSegments(geomFrom(arr.eaveLine), layers.outlines ? eOn : lineMat);
          eave.userData = { kind: 'outline-line', on: eOn, off: lineMat };
          g.add(eave);
        }
        if (arr.crease.length) g.add(new THREE.LineSegments(geomFrom(arr.crease), lineMat));
        if (arr.corners.length) g.add(new THREE.LineSegments(geomFrom(arr.corners), lineMat));
        if (b.role === 'neighbor') g.visible = layers.neighbors;
        b._top = arr.top; b._model = arr.model; b._roofPos = arr.roofPos; b._tri = arr.triPlane;
        if (isSubj && !subject) { subject = b; homePts = homeSamples(b, arr.model); }
        grp.add(g);
      });
      return grp;
    }
    function buildTrees() {
      var grp = new THREE.Group(), list = (data.trees || []).filter(function (t) { return Array.isArray(t) && isNum(t[0]) && isNum(t[1]) && isNum(t[2]) && t[2] > 1.5; });
      if (!list.length) return grp;
      var denseLidar = !data.lidar || !isNum(data.lidar.densityPerM2) || data.lidar.densityPerM2 >= 5;
      var kinds = [[], [], []];
      list.forEach(function (t) {
        var h = t[2], r0 = isNum(t[3]) && t[3] > 0 ? t[3] : null;
        var seed = Math.abs(Math.sin(t[0] * 12.9898 + t[1] * 78.233) * 43758.5453) % 1;
        // The symbol follows the measurement: a crown measured tall and narrow tapers.
        if (denseLidar && r0 != null && h > 13.5 && r0 >= 1.5 && h / (r0 * 2) >= 2.75) kinds[2].push([t, seed]);
        else kinds[seed < 0.5 ? 0 : 1].push([t, seed]);
      });
      var q = new THREE.Quaternion(), col = new THREE.Color(), up = new THREE.Vector3(0, 1, 0);
      kinds.forEach(function (sub, kind) {
        if (!sub.length) return;
        var geom = track(treeSymbol(THREE, kind)), n = sub.length;
        var ghostGeom = track(geom.clone()), alpha = new Float32Array(n), attr = new THREE.InstancedBufferAttribute(alpha, 1);
        ghostGeom.setAttribute('instAlpha', attr);
        var solid = new THREE.InstancedMesh(geom, mats.tree, n), ghost = new THREE.InstancedMesh(ghostGeom, mats.treeGhost, n), caster = new THREE.InstancedMesh(geom, mats.treeCaster, n);
        var S = { list: [], caps: [], mats: [], alpha: alpha, attr: attr, ghosted: new Uint8Array(n), solid: solid, ghost: ghost };
        sub.forEach(function (it, i) {
          var t = it[0], seed = it[1], e = t[0], nn = t[1], h = t[2], zg = isNum(t[4]) ? t[4] : terr.h(e, nn);
          var r = isNum(t[3]) && t[3] > 0 ? Math.max(0.5, t[3]) : Math.max(0.5, h * 0.25);
          q.setFromAxisAngle(up, seed * 6.283);
          var m = new THREE.Matrix4().compose(new THREE.Vector3(e, zg - 0.05, -nn), q, new THREE.Vector3(r, h + 0.05, r));
          solid.setMatrixAt(i, m); caster.setMatrixAt(i, m); ghost.setMatrixAt(i, ZERO);
          var v = 0.93 + seed * 0.14;
          col.setRGB(v * (0.97 + ((seed * 7) % 1) * 0.06), v, v * (0.94 + ((seed * 13) % 1) * 0.08));
          solid.setColorAt(i, col); ghost.setColorAt(i, col);
          alpha[i] = 1;
          S.list.push(t); S.mats.push(m); S.caps.push(treeCapsule(t, zg));
        });
        [solid, ghost, caster].forEach(function (mesh) { mesh.instanceMatrix.needsUpdate = true; mesh.frustumCulled = false; if (mesh.instanceColor) mesh.instanceColor.needsUpdate = true; });
        solid.receiveShadow = true;
        caster.castShadow = true;
        ghost.renderOrder = 4;
        solid.userData = { pick: { type: 'tree', list: S.list } };
        pickables.push(solid);
        grp.add(caster); grp.add(solid); grp.add(ghost);
        treeSets.push(S);
      });
      return grp;
    }
    /** Fades every tree that stands between the camera and the home (or over its roof in the
     *  top view) to FADE_ALPHA, and brings it back when it no longer does. Returns true while a
     *  fade is still animating. */
    function updateTreeFade() {
      if (!treeSets.length || !homePts.length || !layers.trees) return false;
      var cp = [camera.position.x, camera.position.y, camera.position.z], anim = false;
      treeSets.forEach(function (S) {
        var mChanged = false, aChanged = false;
        for (var i = 0; i < S.caps.length; i++) {
          var target = treeHides(S.caps[i], cp, homePts) ? FADE_ALPHA : 1, cur = S.alpha[i];
          if (cur !== target) {
            cur = opts.reducedMotion ? target : cur + (target - cur) * 0.24;
            if (Math.abs(target - cur) < 0.02) cur = target; else anim = true;
            S.alpha[i] = cur; aChanged = true;
          }
          var g = cur < 0.995 ? 1 : 0;
          if (g !== S.ghosted[i]) {
            S.ghosted[i] = g;
            S.solid.setMatrixAt(i, g ? ZERO : S.mats[i]); S.ghost.setMatrixAt(i, g ? S.mats[i] : ZERO);
            mChanged = true;
          }
        }
        if (aChanged) S.attr.needsUpdate = true;
        if (mChanged) { S.solid.instanceMatrix.needsUpdate = true; S.ghost.instanceMatrix.needsUpdate = true; S.solid.boundingSphere = null; S.solid.boundingBox = null; }
      });
      return anim;
    }
    function buildStreets() {
      var grp = new THREE.Group(), named = {};
      (data.streets || []).forEach(function (s) {
        var pts = s.pts || [];
        if (pts.length < 2) return;
        var w = STREET_W[s.kind] || 6.5, isPath = w < 3, pos = [], nor = [];
        for (var i = 0; i < pts.length - 1; i++) {
          var a = pts[i], b = pts[i + 1], dx = b[0] - a[0], dy = b[1] - a[1], len = Math.hypot(dx, dy);
          if (len < 0.05) continue;
          var ox = (-dy / len) * w / 2, oy = (dx / len) * w / 2, segs = Math.max(1, Math.ceil(len / 6));
          for (var k = 0; k < segs; k++) {
            var t0 = k / segs, t1 = (k + 1) / segs;
            var p = [a[0] + dx * t0, a[1] + dy * t0], q = [a[0] + dx * t1, a[1] + dy * t1];
            var c = [[p[0] - ox, p[1] - oy], [q[0] - ox, q[1] - oy], [q[0] + ox, q[1] + oy], [p[0] + ox, p[1] + oy]].map(function (v) {
              return [v[0], terr.h(v[0], v[1]) + (isPath ? 0.07 : 0.05), -v[1]];
            });
            [c[0], c[1], c[2], c[0], c[2], c[3]].forEach(function (v) { pos.push(v[0], v[1], v[2]); nor.push(0, 1, 0); });
          }
        }
        if (pos.length) {
          var mesh = new THREE.Mesh(geomFrom(pos, nor), isPath ? mats.path : mats.street);
          mesh.receiveShadow = true;
          grp.add(mesh);
        }
        // One name per street, at its closest approach that is still clear of the house.
        if (s.name && !named[s.name] && !isPath) {
          var best = null;
          for (var j = 0; j < pts.length - 1; j++) for (var u = 0; u <= 4; u++) {
            var x = pts[j][0] + (pts[j + 1][0] - pts[j][0]) * (u / 4), y = pts[j][1] + (pts[j + 1][1] - pts[j][1]) * (u / 4), d = Math.hypot(x, y);
            if (d >= 14 && d <= 105 && (!best || d < best.d)) best = { x: x, y: y, d: d };
          }
          if (best) { named[s.name] = 1; streetLabels.push({ name: s.name, e: best.x, n: best.y }); }
        }
      });
      return grp;
    }
    var streetLabels = [];
    function buildLot() {
      var grp = new THREE.Group(), lot = data.lot;
      if (!Array.isArray(lot) || lot.length < 3) return grp;
      // 3D terrain-draped cased sky-blue parcel ribbon (WebGL 1px lines are nearly invisible over aerial grass).
      var casePos = [], dashPos = [], acc = 0, period = 2.0, dashLen = 1.25;
      var quadRibbon = function (out, p0, p1, hw, yOff) {
        var dx = p1[0] - p0[0], dy = p1[1] - p0[1], l = Math.hypot(dx, dy);
        if (l < 1e-4) return;
        var ox = (-dy / l) * hw, oy = (dx / l) * hw;
        var c = [[p0[0] - ox, p0[1] - oy], [p1[0] - ox, p1[1] - oy], [p1[0] + ox, p1[1] + oy], [p0[0] + ox, p0[1] + oy]].map(function (v) {
          return [v[0], terr.h(v[0], v[1]) + yOff, -v[1]];
        });
        [c[0], c[1], c[2], c[0], c[2], c[3]].forEach(function (v) { out.push(v[0], v[1], v[2]); });
      };
      for (var i = 0; i < lot.length; i++) {
        var a = lot[i], b = lot[(i + 1) % lot.length], len = Math.hypot(b[0] - a[0], b[1] - a[1]);
        if (len < 0.05) continue;
        var segs = Math.max(1, Math.ceil(len / 0.5));
        for (var k = 0; k < segs; k++) {
          var t0 = k / segs, t1 = (k + 1) / segs, segL = len / segs;
          var q0 = [a[0] + (b[0] - a[0]) * t0, a[1] + (b[1] - a[1]) * t0], q1 = [a[0] + (b[0] - a[0]) * t1, a[1] + (b[1] - a[1]) * t1];
          quadRibbon(casePos, q0, q1, 0.16, 0.09);
          if ((acc % period) < dashLen) quadRibbon(dashPos, q0, q1, 0.10, 0.105);
          acc += segL;
        }
      }
      if (casePos.length) grp.add(new THREE.Mesh(geomFrom(casePos), mats.lotCase));
      if (dashPos.length) grp.add(new THREE.Mesh(geomFrom(dashPos), mats.lotDash));
      return grp;
    }
    function buildFacingArrow(sb) {
      var grp = new THREE.Group();
      if (!sb || !isNum(data.facingDeg)) return grp;
      var br = data.facingDeg * RAD, dir = [Math.sin(br), Math.cos(br)], c = H3.ringCentroid(sb.ring), d = 0;
      while (d < 60 && H3.pointInRing(c[0] + dir[0] * d, c[1] + dir[1] * d, sb.ring)) d += 0.5;
      var s0 = d + 1.6, len = 4.6, side = [dir[1], -dir[0]];
      var arrowGeom = function (pad, yOff) {
        var pt = function (along, across) { var x = c[0] + dir[0] * along + side[0] * across, y = c[1] + dir[1] * along + side[1] * across; return [x, terr.h(x, y) + yOff, -y]; };
        var tip = pt(s0 + len + pad * 1.4, 0), l = pt(s0 + len - 2.2 - pad * 0.4, 1.25 + pad), r = pt(s0 + len - 2.2 - pad * 0.4, -1.25 - pad);
        var sl = pt(s0 - pad, 0.42 + pad), sr = pt(s0 - pad, -0.42 - pad), nl = pt(s0 + len - 2.2, 0.42 + pad), nr = pt(s0 + len - 2.2, -0.42 - pad);
        return geomFrom([].concat(tip, l, r, sl, nl, nr, sl, nr, sr));
      };
      grp.add(new THREE.Mesh(arrowGeom(0.14, 0.145), mats.arrowHalo));
      grp.add(new THREE.Mesh(arrowGeom(0, 0.16), mats.arrow));
      var tx = c[0] + dir[0] * (s0 + len + 0.6), ty = c[1] + dir[1] * (s0 + len + 0.6);
      grp.userData.tip = [tx, terr.h(tx, ty) + 0.16, -ty];
      return grp;
    }

    /* ---- measurements drawn on the model, one kind at a time ---- */
    /** What can be dimensioned in this scene. Everything comes from the scene data; a kind
     *  with nothing to show is reported as unavailable so the UI leaves its button out. */
    function measureFacts() {
      var info = { heights: false, roof: false, lot: false, gap: false, tree: false }, sb = subject, m = (data && data.metrics) || {};
      if (!data || !sb) return info;
      var c = H3.ringCentroid(sb.ring);
      if (isNum(sb.eave) && isNum(sb.ridge)) info.heights = heightPole(home ? home.b : (isNum(data.facingDeg) ? data.facingDeg : 160), null);
      var faces = [];
      (data.buildings || []).forEach(function (b) {
        if (b.role === 'neighbor' || !b._model || !b._model.fitted) return;
        b._model.planes.forEach(function (pl, pi) { if (pl.src && isNum(pl.src.areaM2) && pl.src.areaM2 >= MIN_FACE_M2 && isNum(pl.src.cu) && isNum(pl.src.cv)) faces.push({ b: b, pi: pi }); });
      });
      if (faces.length) {
        faces.sort(function(a, b) {
          var aa = a.b._model.planes[a.pi].src.areaM2 || 0, bb = b.b._model.planes[b.pi].src.areaM2 || 0;
          return bb - aa;
        });
        info.roof = { faces: faces };
      }
      if (Array.isArray(data.lot) && data.lot.length >= 3) {
        var ring = straightenRing(data.lot, 8), edges = [];
        for (var i = 0; i < ring.length; i++) {
          var a = ring[i], b2 = ring[(i + 1) % ring.length], len = Math.hypot(b2[0] - a[0], b2[1] - a[1]);
          if (len >= MIN_EDGE_M) edges.push({ a: a, b: b2, len: len });
        }
        if (edges.length) {
          info.lot = { edges: edges };
          if (isNum(m.lotSlopePct)) {
            // Somewhere on the lot that is clear of the house.
            var spot = null;
            ring.forEach(function (p) {
              var q = [c[0] + (p[0] - c[0]) * 0.72, c[1] + (p[1] - c[1]) * 0.72];
              if (H3.pointInRing(q[0], q[1], sb.ring)) return;
              var d = H3.distToRing(q[0], q[1], sb.ring);
              if (!spot || d > spot.d) spot = { d: d, p: q };
            });
            if (spot) info.lot.slopeAt = spot.p;
          }
        }
      }
      var gap = null;
      (data.buildings || []).forEach(function (b) {
        if (b.role !== 'neighbor' || !Array.isArray(b.ring) || b.ring.length < 3) return;
        var g = ringGap(sb.ring, b.ring);
        if (g.a && (!gap || g.d < gap.d)) gap = g;
      });
      if (gap && gap.d > 0.05) info.gap = { a: gap.a, b: gap.b, m: isNum(m.nearestNeighborM) ? m.nearestNeighborM : gap.d };
      if (isNum(m.tallestTreeM)) {
        var tall = null;
        (data.trees || []).forEach(function (t) {
          if (!Array.isArray(t) || !isNum(t[2]) || Math.abs(t[2] - m.tallestTreeM) > 0.06) return;
          var d = H3.pointInRing(t[0], t[1], sb.ring) ? 0 : H3.distToRing(t[0], t[1], sb.ring);
          if (d <= 31 && (!tall || d < tall.d)) tall = { t: t, d: d };
        });
        if (tall) info.tree = { t: tall.t, distM: isNum(m.tallestTreeDistM) ? m.tallestTreeDistM : null };
      }
      return info;
    }
    /** Where the height dimension stands: the footprint corner on the near right of the view,
     *  so it stays beside the home rather than behind it as the camera turns. */
    function heightPole(camBearing, current) {
      var sb = subject, c = H3.ringCentroid(sb.ring), hb = (camBearing - 55) * RAD, dir = [Math.sin(hb), Math.cos(hb)], best = null, cur = null;
      sb.ring.forEach(function (p, i) {
        var s = (p[0] - c[0]) * dir[0] + (p[1] - c[1]) * dir[1];
        if (!best || s > best.s) best = { s: s, p: p, i: i };
        if (current && i === current.idx) cur = s;
      });
      // Keep the corner in use unless another one is clearly better placed.
      if (current && cur != null && best.s - cur < 1.5) return current;
      var ox = best.p[0] - c[0], oy = best.p[1] - c[1], ol = Math.hypot(ox, oy) || 1;
      return { idx: best.i, corner: best.p, pole: [best.p[0] + (ox / ol) * 1.1, best.p[1] + (oy / ol) * 1.1] };
    }
    function clearMeasure() {
      if (groups.measure) {
        root.remove(groups.measure);
        groups.measure.traverse(function (o) { if (o.geometry) o.geometry.dispose(); });
        delete groups.measure;
      }
      labels = labels.filter(function (l) { if (!l.m) return true; if (l.el.parentNode) l.el.parentNode.removeChild(l.el); return false; });
    }
    function buildMeasure() {
      clearMeasure();
      var info = mInfo[measure];
      if (!data || !info) { requestRender(); return; }
      var grp = new THREE.Group(), sb = subject, m = data.metrics || {}, T = H3.T, yAxis = new THREE.Vector3(0, 1, 0);
      var ft = function (v) { return '<b>' + H3.escH(H3.nf(v / H3.FT, 0)) + ' ft</b>'; };
      var est = sb && sb.heightSource !== 'lidar' ? ' <i>' + T('est.', '估算', 'est.') + '</i>' : '';
      // A dimension line: a thin dark bar with a light halo so it reads over the photo and the model.
      var bar = function (a, c, rad) {
        var A = new THREE.Vector3(a[0], a[1], a[2]), C = new THREE.Vector3(c[0], c[1], c[2]), len = A.distanceTo(C);
        if (len < 1e-4) return;
        var mid = A.clone().add(C).multiplyScalar(0.5), dir = C.clone().sub(A).normalize(), qt = new THREE.Quaternion().setFromUnitVectors(yAxis, dir);
        [[rad || 0.045, mats.dim, 21], [(rad || 0.045) * 2.1, mats.dimHalo, 20]].forEach(function (s) {
          var mesh = new THREE.Mesh(new THREE.CylinderGeometry(s[0], s[0], len, 6, 1, true), s[1]);
          mesh.position.copy(mid); mesh.quaternion.copy(qt); mesh.renderOrder = s[2];
          grp.add(mesh);
        });
      };
      var tag = function (html, p, cls) { addLabel(html, p[0], -p[2], p[1], 'h3d-label-dim ' + (cls || '')).m = true; };
      var G = function (e, n, up) { return [e, terr.h(e, n) + (up || 0), -n]; };
      if (measure === 'heights') {
        var pe = info.pole[0], pn = info.pole[1], base = sb.base || 0, g0 = Math.min(terr.h(pe, pn), base), eaveY = base + sb.eave, ridgeY = base + sb.ridge;
        bar([pe, g0, -pn], [pe, ridgeY, -pn]);
        [g0, eaveY, ridgeY].forEach(function (y) { bar([pe, y, -pn], [info.corner[0], y, -info.corner[1]], 0.035); });
        tag(T('Ridge', '屋脊', 'Cumbrera') + ' ' + ft(sb.ridge) + est, [pe, ridgeY, -pn], 'h3d-dim-up');
        tag(T('Eave', '檐口', 'Alero') + ' ' + ft(sb.eave) + est, [pe, eaveY, -pn], 'h3d-dim-side');
      } else if (measure === 'roof') {
        info.faces.forEach(function (f) {
          var M = f.b._model, pl = M.planes[f.pi], s = pl.src, base = f.b.base || 0;
          var P = function (x, y) { var w = M.world(x, y); return [w[0], base + M.hp(f.pi, x, y) + 0.07, -w[1]]; };
          var txt = '<b>' + H3.escH(H3.nf(s.areaM2 * 10.7639104, 0)) + ' sq ft</b>', g = Math.hypot(pl.a, pl.b);
          if (s.aspect && isNum(s.pitchDeg) && g > 1e-6) {
            txt += ' · ' + Math.round(Math.tan(s.pitchDeg * RAD) * 12) + ':12';
            // Downslope arrow lying on the face.
            var dx = -pl.a / g, dy = -pl.b / g, len = clamp(Math.sqrt(s.areaM2) * 0.3, 0.9, 2.6) / M.step, hd = 0.5 / M.step;
            var x0 = s.cu + dx * 0.25 * len, y0 = s.cv + dy * 0.25 * len, x1 = x0 + dx * len, y1 = y0 + dy * len;
            bar(P(x0, y0), P(x1, y1), 0.04);
            [0.5, -0.5].forEach(function (ang) {
              var bx = -dx * Math.cos(ang) + dy * Math.sin(ang), by = -dx * Math.sin(ang) - dy * Math.cos(ang);
              bar(P(x1, y1), P(x1 + bx * hd, y1 + by * hd), 0.04);
            });
          } else txt += ' · ' + T('flat', '平屋面', 'plano');
          tag(txt, P(s.cu, s.cv), 'h3d-dim-face');
        });
      } else if (measure === 'lot') {
        info.edges.forEach(function (ed) {
          var steps = Math.max(1, Math.ceil(ed.len / 4));
          for (var k = 0; k < steps; k++) {
            var t0 = k / steps, t1 = (k + 1) / steps;
            bar(G(ed.a[0] + (ed.b[0] - ed.a[0]) * t0, ed.a[1] + (ed.b[1] - ed.a[1]) * t0, 0.2), G(ed.a[0] + (ed.b[0] - ed.a[0]) * t1, ed.a[1] + (ed.b[1] - ed.a[1]) * t1, 0.2), 0.035);
          }
          bar(G(ed.a[0], ed.a[1], 0), G(ed.a[0], ed.a[1], 0.7), 0.035);
          bar(G(ed.b[0], ed.b[1], 0), G(ed.b[0], ed.b[1], 0.7), 0.035);
          tag(ft(ed.len), G((ed.a[0] + ed.b[0]) / 2, (ed.a[1] + ed.b[1]) / 2, 0.45), 'h3d-dim-lot');
        });
        if (info.slopeAt && isNum(m.lotSlopePct)) {
          var sp = info.slopeAt, basis = m.slopeBasis === 'parcel' ? T('Lot slope', '地块坡度', 'Pendiente del lote') : T('Ground slope', '地面坡度', 'Pendiente del terreno');
          var pct = '<b>' + H3.escH(H3.nf(m.lotSlopePct, 1)) + '%</b>';
          if (m.lotSlopePct >= 1 && isNum(m.lotDownhillBearing)) {
            var sbr = m.lotDownhillBearing * RAD, sd = [Math.sin(sbr), Math.cos(sbr)], tip = [sp[0] + sd[0] * 3.2, sp[1] + sd[1] * 3.2];
            bar(G(sp[0], sp[1], 0.25), G(tip[0], tip[1], 0.25));
            [0.5, -0.5].forEach(function (ang) {
              var bx = -sd[0] * Math.cos(ang) + sd[1] * Math.sin(ang), by = -sd[0] * Math.sin(ang) - sd[1] * Math.cos(ang);
              bar(G(tip[0], tip[1], 0.25), G(tip[0] + bx * 0.9, tip[1] + by * 0.9, 0.25));
            });
            tag(basis + ' ' + pct + ' ↓ ' + H3.compassT(H3.compass8(m.lotDownhillBearing)), G(sp[0], sp[1], 0.5), 'h3d-dim-note');
          } else {
            tag(basis + ' ' + pct + ' · ' + T('essentially level', '基本平整', 'prácticamente plano'), G(sp[0], sp[1], 0.5), 'h3d-dim-note');
          }
        }
      } else if (measure === 'gap') {
        var A = G(info.a[0], info.a[1], 1.1), B = G(info.b[0], info.b[1], 1.1);
        B[1] = A[1] = Math.max(A[1], B[1]);
        bar(A, B, 0.05);
        [A, B].forEach(function (p) { bar([p[0], p[1] - 0.55, p[2]], [p[0], p[1] + 0.55, p[2]], 0.04); });
        tag(T('Gap to nearest neighbor', '最近邻栋间距', 'Separación del vecino') + ' ' + ft(info.m), [(A[0] + B[0]) / 2, A[1] + 0.6, (A[2] + B[2]) / 2], 'h3d-dim-up');
      } else if (measure === 'tree') {
        var t = info.t, r = isNum(t[3]) && t[3] > 0 ? t[3] : 1, zg = isNum(t[4]) ? t[4] : terr.h(t[0], t[1]), xe = t[0] + r + 0.5;
        bar([xe, zg, -t[1]], [xe, zg + t[2], -t[1]]);
        [zg, zg + t[2]].forEach(function (y) { bar([xe, y, -t[1]], [t[0], y, -t[1]], 0.035); });
        tag(T('Tallest tree', '最高树木', 'Árbol más alto') + ' ' + ft(t[2]), [xe, zg + t[2] * 0.6, -t[1]], 'h3d-dim-up');
        if (isNum(info.distM) && info.distM >= 0.5) {
          // Line from the trunk to the nearest point of the house, labelled with the reported distance.
          var best = null;
          for (var k = 0, n = sb.ring.length; k < n; k++) {
            var a = sb.ring[k], b = sb.ring[(k + 1) % n], dx = b[0] - a[0], dy = b[1] - a[1], l2 = dx * dx + dy * dy;
            var tt = l2 < 1e-12 ? 0 : clamp(((t[0] - a[0]) * dx + (t[1] - a[1]) * dy) / l2, 0, 1), qx = a[0] + tt * dx, qy = a[1] + tt * dy, d = Math.hypot(t[0] - qx, t[1] - qy);
            if (!best || d < best.d) best = { d: d, x: qx, y: qy };
          }
          var y1 = zg + 1.2;
          bar([t[0], y1, -t[1]], [best.x, y1, -best.y], 0.04);
          tag(ft(info.distM) + ' ' + T('to the house', '距房屋', 'a la casa'), [(t[0] + best.x) / 2, y1 + 0.3, -(t[1] + best.y) / 2], 'h3d-dim-note');
        } else if (isNum(info.distM)) {
          tag(T('Crown reaches over the roof', '树冠伸至屋顶上方', 'La copa llega sobre el techo'), [t[0], zg + t[2] * 0.55, -t[1]], 'h3d-dim-note');
        }
      }
      groups.measure = grp;
      root.add(grp);
      requestRender();
    }
    /** Tints one roof face (the one under the pointer or last tapped). */
    function setHighlight(b, pi) {
      var key = b ? (data.buildings || []).indexOf(b) + ':' + pi : '';
      if (key === hiKey) return;
      hiKey = key;
      if (hiMesh) { root.remove(hiMesh); hiMesh.geometry.dispose(); hiMesh = null; }
      if (b && b._roofPos && b._tri) {
        var pos = [];
        for (var t = 0; t < b._tri.length; t++) if (b._tri[t] === pi) for (var k = 0; k < 9; k++) pos.push(b._roofPos[t * 9 + k] + (k % 3 === 1 ? 0.03 : 0));
        if (pos.length) {
          var g = new THREE.BufferGeometry();
          g.setAttribute('position', new THREE.Float32BufferAttribute(pos, 3));
          hiMesh = new THREE.Mesh(g, mats.faceHi);
          hiMesh.renderOrder = 3;
          root.add(hiMesh);
        }
      }
      requestRender();
    }

    function rebuild(keepCamera) {
      clearScene();
      if (!data) { requestRender(); return; }
      terr = terrainSampler(data);
      if (data.origin && isNum(data.origin.lat)) ensureAerial(data.origin);
      buildContactTex();
      makeMaterials();
      streetLabels = [];
      groups.ground = buildGround();
      groups.buildings = buildBuildings();
      groups.trees = buildTrees();
      groups.streets = buildStreets();
      groups.lot = buildLot();
      var sb = subject, accs = [];
      (data.buildings || []).forEach(function (b) { if (b.role === 'accessory' && Array.isArray(b.ring) && b.ring.length >= 3) accs.push(b); });
      groups.arrow = buildFacingArrow(sb);
      Object.keys(groups).forEach(function (k) { root.add(groups[k]); });
      // Labels
      if (sb) {
        var c = H3.ringCentroid(sb.ring);
        addLabel(H3.T('This home', '本房', 'Esta casa'), c[0], c[1], (isNum(sb._top) ? sb._top : (sb.base || 0) + (isNum(sb.ridge) ? sb.ridge : 0.2)) + 1.4, 'h3d-label-home');
        if (groups.arrow.userData.tip && isNum(data.facingDeg)) {
          var tip = groups.arrow.userData.tip, cmp = H3.compass8(data.facingDeg);
          addLabel(H3.T('Front', '正面', 'Frente') + ' · ' + H3.compassT(cmp), tip[0], -tip[2], tip[1] + 0.4, 'h3d-label-front').frontOf = data.facingDeg;
        }
      }
      accs.slice(0, 2).forEach(function (ab) {
        var ac = H3.ringCentroid(ab.ring);
        addLabel(H3.escH(ab.label || '') || H3.T('ADU / Garage', '附属建筑', 'Anexo / Garaje'), ac[0], ac[1], (isNum(ab._top) ? ab._top : (ab.base || 0) + (isNum(ab.ridge) ? ab.ridge : 0.2)) + 1.1, 'h3d-label-acc');
      });
      streetLabels.slice(0, 5).forEach(function (s) { addLabel(H3.escH(s.name), s.e, s.n, terr.h(s.e, s.n) + 0.6, 'h3d-label-street'); });
      // Camera framing: a three-quarter view from the street side, on the sunlit side when there is one.
      var diag = 24, ty = 0.5;
      if (sb) {
        var bb = H3.ringBBox(sb.ring);
        diag = Math.hypot(bb[2] - bb[0], bb[3] - bb[1]);
        ty = (sb.base || 0) + (isNum(sb.ridge) ? sb.ridge * 0.42 : 0.25);
      }
      var facing = isNum(data.facingDeg) ? data.facingDeg : 160;
      var sp = data.origin && isNum(data.origin.lat) ? H3.sunPosition(sunMs, data.origin.lat, data.origin.lon) : null;
      var best = clearestBearing(sb, facing, diag, ty, [35, -35, 62, -62], 30, sp && sp.altitude > 0.05 ? sp.bearing : null);
      var frontBest = clearestBearing(sb, facing, diag, ty, [0, 18, -18], 26, null);
      home = { b: best.b, frontB: frontBest.b, p: 58, r: clamp(diag * 1.6 + 14, 36, 120), tx: 0, ty: ty, tz: 0, facing: facing, diag: diag };
      if (!keepCamera) {
        Object.assign(goal, { b: home.b, p: home.p, r: home.r, tx: 0, ty: ty, tz: 0 });
        Object.assign(cam, goal, opts.reducedMotion ? {} : { p: 34, r: home.r * 1.35, b: home.b - 40 });
        orbitT0 = 0;
      } else if (!userMoved) {
        // Nobody has touched the view yet: glide to the opening angle of the refined scene.
        Object.assign(goal, { b: home.b, p: home.p, r: home.r, tx: 0, ty: ty, tz: 0 });
        orbitT0 = 0;
      } else {
        goal.ty = ty;
      }
      mInfo = measureFacts();
      if (opts.onMeasure) opts.onMeasure(measureAvailable());
      buildMeasure();
      applySun();
      applyStyle();
      requestRender();
    }
    function measureAvailable() {
      var out = {};
      ['heights', 'roof', 'lot', 'gap', 'tree'].forEach(function (k) { out[k] = Boolean(mInfo[k]); });
      return out;
    }

    /** Three-quarter (or street-front) bearing with the fewest buildings in the way. Trees are
     *  not counted: they fade when they stand in front of the home. With `sunBearing` the sunlit
     *  side is preferred. Candidates start on the street side. */
    function clearestBearing(sb, facing, diag, ty, cands, elevDeg, sunBearing) {
      var out = { b: (facing + cands[0] + 360) % 360, blocked: 0 };
      if (!sb) return out;
      var c = H3.ringCentroid(sb.ring), halfW = Math.max(4, diag * 0.32), tanEl = Math.tan((elevDeg || 30) * RAD), bestScore = Infinity;
      var blockers = [];
      (data.buildings || []).forEach(function (b) {
        if (b.role === 'subject' || !Array.isArray(b.ring) || !isNum(b.ridge)) return;
        var bc = H3.ringCentroid(b.ring);
        blockers.push({ x: bc[0] - c[0], y: bc[1] - c[1], h: (b.base || 0) + b.ridge, r: Math.sqrt(Math.abs(H3.ringArea(b.ring))) * 0.5 });
      });
      cands.forEach(function (off, i) {
        var br = (facing + off) * RAD, dx = Math.sin(br), dy = Math.cos(br), score = 0;
        blockers.forEach(function (k) {
          var along = k.x * dx + k.y * dy, across = Math.abs(k.x * dy - k.y * dx);
          if (along < halfW * 0.55 || along > 75 || across > halfW + k.r) return;
          var sight = ty + Math.max(0, along - halfW * 0.5) * tanEl;
          if (k.h > sight) score += Math.min(k.r * 2, 2 * halfW) * Math.min(1, (k.h - sight) / 4);
        });
        var blocked = score;
        score += i * 0.9;
        if (sunBearing != null) score += (Math.abs(((facing + off - sunBearing + 540) % 360) - 180) / 180) * 3;
        if (score < bestScore) { bestScore = score; out = { b: (facing + off + 360) % 360, blocked: blocked }; }
      });
      return out;
    }

    /* ---- sun ---- */
    function applySun() {
      if (!data || !data.origin || !mats.horizon) return;
      sunInfo = H3.sunPosition(sunMs, data.origin.lat, data.origin.lon);
      var alt = sunInfo.altitude, br = sunInfo.bearing * RAD, ca = Math.cos(Math.max(alt, 0.03));
      var dir = new THREE.Vector3(Math.sin(br) * ca, Math.sin(Math.max(alt, 0.03)), -Math.cos(br) * ca).normalize();
      sun.target.position.set(0, 0, 0);
      sun.position.copy(dir.clone().multiplyScalar(320));
      fill.position.set(-dir.x * 200, 140, -dir.z * 200);
      var day = clamp((alt + 0.035) / 0.24, 0, 1), low = clamp(1 - alt / 0.45, 0, 1);
      sun.intensity = 3.0 * day;
      sun.color.setRGB(1, 0.97 - 0.2 * low, 0.92 - 0.42 * low);
      hemi.intensity = 0.5 + 1.4 * day;
      fill.intensity = 0.15 + 0.35 * day;
      sun.castShadow = layers.shadows && alt > 0.02;
      var fogDay = new THREE.Color(0xdce9f6), fogDusk = new THREE.Color(0xe8d5c2), fogNight = new THREE.Color(0x1b263f);
      var fc = fogNight.clone().lerp(fogDusk.clone().lerp(fogDay, clamp((alt - 0.02) / 0.3, 0, 1)), day);
      scene3.fog.color.copy(fc);
      mats.horizon.color.copy(fc);
      if (mats.contact) mats.contact.opacity = 0.35 + 0.65 * day;
      shadowsDirty();
      if (opts.onSun) opts.onSun({ altitudeDeg: alt / RAD, bearing: sunInfo.bearing, day: day, low: low });
    }

    /* ---- camera + frame ---- */
    function placeCamera() {
      var p = cam.p * RAD, b = cam.b * RAD, sp = Math.sin(p);
      camera.position.set(cam.tx + cam.r * sp * Math.sin(b), cam.ty + cam.r * Math.cos(p), cam.tz - cam.r * sp * Math.cos(b));
      camera.lookAt(cam.tx, cam.ty, cam.tz);
    }
    function frame(ts) {
      raf = 0;
      if (disposed) return;
      var moving = false;
      // Until the first touch the view sways slowly around the opening angle.
      if (home && !userMoved && !opts.reducedMotion && opts.autoOrbit !== false && inView && !Object.keys(pointers).length) {
        var now = isNum(ts) ? ts : Date.now();
        if (!orbitT0) orbitT0 = now;
        goal.b = (home.b + 13 * Math.sin(((now - orbitT0) / 1000) * 0.26) + 360) % 360;
        moving = true;
      }
      ['p', 'r', 'tx', 'ty', 'tz'].forEach(function (k) {
        var d = goal[k] - cam[k];
        if (Math.abs(d) > 0.002) { cam[k] += d * 0.18; moving = true; } else cam[k] = goal[k];
      });
      var db = ((goal.b - cam.b + 540) % 360) - 180;
      if (Math.abs(db) > 0.01) { cam.b += db * 0.18; moving = true; } else cam.b = goal.b;
      if (!dirty && !moving) return;
      dirty = false;
      placeCamera();
      if (updateTreeFade()) moving = true;
      if (measure === 'heights' && mInfo.heights && subject) {
        var hp = heightPole(cam.b, mInfo.heights);
        if (hp !== mInfo.heights) { mInfo.heights = hp; buildMeasure(); dirty = false; }
      }
      renderer.render(scene3, camera);
      var v = new THREE.Vector3(), rects = [];
      // Sort labels by area descending if they are faces, so larger faces get priority? 
      // Actually they are in array order. 'This home' is usually first? Wait, 'This home' is added last!
      // Let's just do a normal loop, but we collect rects.
      labels.forEach(function (l) {
        v.copy(l.v).project(camera);
        var vis = v.z > -1 && v.z < 1 && Math.abs(v.x) < 1.05 && Math.abs(v.y) < 1.05;
        if (vis && l.frontOf != null) vis = Math.abs(((cam.b - l.frontOf + 540) % 360) - 180) < 100;
        var x = (v.x * 0.5 + 0.5) * size.w, y = (-v.y * 0.5 + 0.5) * size.h;
        if (vis) {
          var isFace = l.el.className.indexOf('h3d-dim-face') >= 0;
          var isSide = l.el.className.indexOf('h3d-dim-side') >= 0;
          if (isSide) {
            if (x > size.w / 2) l.el.style.marginLeft = '-46px';
            else l.el.style.marginLeft = '46px';
          }
          var hw = isFace ? 60 : (l.el.offsetWidth ? l.el.offsetWidth / 2 : 40), hh = l.el.offsetHeight || 26;
          var cx = x, cy = y;
          if (isSide) cx += (l.el.style.marginLeft === '46px' ? 46 : -46);
          
          var hudT = size.h - (size.w < 640 ? 100 : 60);
          var compL = size.w - 60, compT = hudT - 50;
          
          if (cx - hw < 4) cx = hw + 4;
          if (cx + hw > size.w - 4) cx = size.w - hw - 4;
          if (cy - hh < 4) cy = hh + 4;
          if (cy > hudT - 4) cy = hudT - 4;
          if (cx + hw > compL && cy > compT) cy = compT;
          
          if (isSide) { x = cx - (l.el.style.marginLeft === '46px' ? 46 : -46); y = cy; }
          else { x = cx; y = cy; }
          
          var rect = { l: cx - hw - 2, r: cx + hw + 2, t: cy - hh - 2, b: cy + 2 };
          for (var i = 0; i < rects.length; i++) {
            if (rect.l < rects[i].r && rect.r > rects[i].l && rect.t < rects[i].b && rect.b > rects[i].t) {
              if (isFace || l.el.className.indexOf('h3d-label-home') >= 0) vis = false;
              break;
            }
          }
          if (vis) rects.push(rect);
        }
        l.el.style.display = vis ? '' : 'none';
        if (vis) l.el.style.transform = 'translate(-50%,-100%) translate(' + x.toFixed(1) + 'px,' + y.toFixed(1) + 'px)';
      });
      if (opts.onCamera) opts.onCamera({ heading: (cam.b + 180) % 360, tilt: cam.p });
      if (moving && !raf) raf = window.requestAnimationFrame(frame);
    }
    function resize() {
      var w = els.stage.clientWidth || 800, h = els.stage.clientHeight || 500;
      if (w === size.w && h === size.h) return;
      size = { w: w, h: h };
      renderer.setSize(w, h, false);
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      requestRender();
    }

    /* ---- interaction ---- */
    var pointers = {}, gesture = null, active = false, downAt = null, hoverRaf = 0, hoverPt = null, lastHoverXY = null, wheelArmedUntil = 0;
    function limit() {
      goal.p = clamp(goal.p, 3, 87);
      goal.r = clamp(goal.r, 14, 320);
      goal.tx = clamp(goal.tx, -70, 70); goal.tz = clamp(goal.tz, -70, 70);
    }
    function pan(dx, dy) {
      var b = cam.b * RAD, k = cam.r * 0.0016;
      // Screen-right and screen-up projected on the ground.
      var rx = -Math.cos(b), rz = -Math.sin(b), fx = -Math.sin(b), fz = Math.cos(b);
      goal.tx += (-dx * rx + dy * fx) * k; goal.tz += (-dx * rz + dy * fz) * k;
      limit();
    }
    function touched() { if (!userMoved) { userMoved = true; goal.b = cam.b; } }
    function onDown(ev) {
      els.canvas.setPointerCapture && els.canvas.setPointerCapture(ev.pointerId);
      pointers[ev.pointerId] = { x: ev.clientX, y: ev.clientY, type: ev.pointerType, button: ev.button };
      active = true; touched(); wheelArmedUntil = Date.now() + 5000;
      downAt = { x: ev.clientX, y: ev.clientY, t: Date.now() };
      var ids = Object.keys(pointers);
      gesture = ids.length >= 2 ? { d: Math.hypot(pointers[ids[0]].x - pointers[ids[1]].x, pointers[ids[0]].y - pointers[ids[1]].y), y: (pointers[ids[0]].y + pointers[ids[1]].y) / 2 } : null;
      if (opts.onInteract) opts.onInteract('down');
    }
    function onMove(ev) {
      var p = pointers[ev.pointerId];
      if (!p) {
        if (ev.pointerType === 'mouse') {
          if (lastHoverXY && Math.hypot(ev.clientX - lastHoverXY[0], ev.clientY - lastHoverXY[1]) > 5) wheelArmedUntil = Date.now() + 1800;
          lastHoverXY = [ev.clientX, ev.clientY];
          hoverPt = [ev.clientX, ev.clientY];
          if (!hoverRaf && !disposed) {
            hoverRaf = window.requestAnimationFrame(function () {
              hoverRaf = 0;
              if (hoverPt && !Object.keys(pointers).length) pick(hoverPt[0], hoverPt[1]);
            });
          }
        }
        return;
      }
      var dx = ev.clientX - p.x, dy = ev.clientY - p.y;
      p.x = ev.clientX; p.y = ev.clientY;
      var ids = Object.keys(pointers);
      if (ids.length >= 2 && gesture) {
        var a = pointers[ids[0]], b = pointers[ids[1]], d = Math.hypot(a.x - b.x, a.y - b.y), my = (a.y + b.y) / 2;
        if (d > 0 && gesture.d > 0) goal.r *= gesture.d / d;
        goal.p -= (my - gesture.y) * 0.25;
        gesture.d = d; gesture.y = my;
      } else if (p.button === 2 || p.button === 1 || ev.shiftKey) {
        pan(dx, dy);
      } else {
        goal.b = (goal.b + dx * 0.32 + 360) % 360;
        goal.p -= dy * 0.26;
      }
      limit(); requestRender();
    }
    function onUp(ev) {
      var p = pointers[ev.pointerId];
      delete pointers[ev.pointerId];
      gesture = null;
      if (p && downAt && Math.hypot(ev.clientX - downAt.x, ev.clientY - downAt.y) < 5 && Date.now() - downAt.t < 450) pick(ev.clientX, ev.clientY);
    }
    function onWheel(ev) {
      var armed = active || Date.now() < wheelArmedUntil || ev.ctrlKey || ev.metaKey || document.fullscreenElement === els.stage;
      if (!armed) { if (opts.onInteract) opts.onInteract('wheel-idle'); return; }
      ev.preventDefault(); touched(); wheelArmedUntil = Date.now() + 1800;
      goal.r *= Math.exp(clamp(ev.deltaY, -240, 240) * 0.0014);
      limit(); requestRender();
    }
    function onKey(ev) {
      if (ev.target && ev.target.tagName && /^(INPUT|SELECT|TEXTAREA|BUTTON)$/i.test(ev.target.tagName)) return;
      var k = ev.key, used = true;
      if (k === 'ArrowLeft' || k === 'ArrowRight' || k === 'ArrowUp' || k === 'ArrowDown' || k === '+' || k === '=' || k === '-' || k === '_') touched();
      if (k === 'ArrowLeft') goal.b = (goal.b + 8) % 360;
      else if (k === 'ArrowRight') goal.b = (goal.b + 352) % 360;
      else if (k === 'ArrowUp') goal.p -= 5;
      else if (k === 'ArrowDown') goal.p += 5;
      else if (k === '+' || k === '=') goal.r *= 0.85;
      else if (k === '-' || k === '_') goal.r *= 1.18;
      else if (k === '0' || k === 'Home') setView('orbit');
      else used = false;
      if (used) { ev.preventDefault(); limit(); requestRender(); }
    }
    function pick(cx, cy) {
      if (!opts.onPick) return;
      var rect = els.canvas.getBoundingClientRect();
      if (!rect.width || !rect.height) return;
      var ndc = new THREE.Vector2(((cx - rect.left) / rect.width) * 2 - 1, -((cy - rect.top) / rect.height) * 2 + 1);
      var rc = new THREE.Raycaster();
      rc.setFromCamera(ndc, camera);
      var hits = rc.intersectObjects(pickables.filter(function (o) { return o.visible && (!o.parent || o.parent.visible) && (!o.parent || !o.parent.parent || o.parent.parent.visible); }), false);
      if (!hits.length || !hits[0].object.userData.pick) {
        if (els.canvas.style) els.canvas.style.cursor = '';
        setHighlight(null);
        opts.onPick(null);
        return;
      }
      var h = hits[0], info = h.object.userData.pick;
      if (els.canvas.style) els.canvas.style.cursor = 'pointer';
      if (info.type === 'tree') { setHighlight(null); opts.onPick({ type: 'tree', tree: info.list[h.instanceId] }, cx - rect.left, cy - rect.top); return; }
      // A roof face of this home (or its garage / ADU) answers with that face's own numbers.
      var pi = h.object.userData.roof && info.model && info.model.fitted && info.b.role !== 'neighbor' && h.faceIndex != null ? info.tri[h.faceIndex] : null;
      var src = pi != null && info.model.planes[pi] ? info.model.planes[pi].src : null;
      if (src) { setHighlight(info.b, pi); opts.onPick({ type: 'face', b: info.b, face: src }, cx - rect.left, cy - rect.top); return; }
      setHighlight(null);
      opts.onPick({ type: 'building', b: info.b, simplified: !(info.model && info.model.fitted) }, cx - rect.left, cy - rect.top);
    }
    var cvs = els.canvas, listeners = [
      [cvs, 'pointerdown', onDown], [cvs, 'pointermove', onMove], [cvs, 'pointerup', onUp], [cvs, 'pointercancel', onUp],
      [cvs, 'wheel', onWheel, { passive: false }], [cvs, 'contextmenu', function (e) { e.preventDefault(); }],
      [cvs, 'dblclick', function () { setView('orbit'); }],
      [els.stage, 'keydown', onKey],
      [els.stage, 'pointerleave', function () { if (!Object.keys(pointers).length) { active = false; wheelArmedUntil = 0; lastHoverXY = null; if (els.canvas.style) els.canvas.style.cursor = ''; if (opts.onPick) { setHighlight(null); opts.onPick(null); } } }],
    ];
    listeners.forEach(function (l) { l[0].addEventListener(l[1], l[2], l[3]); });
    var ro = null;
    if (typeof ResizeObserver !== 'undefined') { ro = new ResizeObserver(resize); ro.observe(els.stage); }
    window.addEventListener('resize', resize);

    function setView(name) {
      if (!home) return;
      userMoved = name !== 'orbit';
      orbitT0 = 0;
      if (name === 'top') Object.assign(goal, { b: 180, p: 3, r: clamp(home.diag * 2.6 + 30, 60, 170), tx: 0, tz: 0 });
      else if (name === 'front') Object.assign(goal, { b: isNum(home.frontB) ? home.frontB : home.facing, p: 64, r: clamp(home.diag * 1.45 + 14, 32, 86), tx: 0, tz: 0 });
      else if (name === 'back') Object.assign(goal, { b: (home.facing + 180) % 360, p: 62, r: clamp(home.diag * 1.9 + 16, 38, 110), tx: 0, tz: 0 });
      else Object.assign(goal, { b: home.b, p: home.p, r: home.r, tx: 0, tz: 0 });
      limit(); requestRender();
    }

    resize();
    return {
      setScene: function (scene, keepCamera) { data = scene; rebuild(Boolean(keepCamera)); },
      setLayer: function (name, on) { layers[name] = Boolean(on); applySun(); applyStyle(); },
      layers: function () { return Object.assign({}, layers); },
      /** Shows one kind of measurement on the model ('heights', 'roof', 'lot', 'gap', 'tree') or none. */
      setMeasure: function (name) { measure = mInfo[name] ? name : 'none'; buildMeasure(); return measure; },
      measure: function () { return measure; },
      measureAvailable: measureAvailable,
      /** Whether the stage is on screen; the idle sway only runs while it is. */
      setInView: function (on) { inView = Boolean(on); if (inView) requestRender(); },
      aerialState: function () { return aerialState; },
      setView: setView,
      view: function () { return { bearing: cam.b, tilt: cam.p, range: cam.r, home: home ? { bearing: home.b, facing: home.facing } : null, userMoved: userMoved }; },
      /** Trees currently faded because they stand in front of the home, and all trees drawn. */
      fadedTrees: function () { var f = 0, n = 0; treeSets.forEach(function (S) { for (var i = 0; i < S.alpha.length; i++) { n++; if (S.alpha[i] < 0.995) f++; } }); return { faded: f, total: n }; },
      faceNorth: function () { touched(); goal.b = 180; requestRender(); },
      zoomBy: function (f) { touched(); goal.r *= f; limit(); requestRender(); },
      rotateBy: function (deg) { touched(); goal.b = (goal.b + deg + 360) % 360; requestRender(); },
      setSunTime: function (ms) { sunMs = ms; applySun(); requestRender(); },
      sun: function () { return { altitudeDeg: sunInfo.altitude / RAD, bearing: sunInfo.bearing }; },
      resize: resize,
      render: requestRender,
      snapshot: function () { try { placeCamera(); renderer.render(scene3, camera); return els.canvas.toDataURL('image/jpeg', 0.86); } catch (e) { return null; } },
      dispose: function () {
        disposed = true;
        if (raf) window.cancelAnimationFrame(raf);
        if (hoverRaf) window.cancelAnimationFrame(hoverRaf);
        listeners.forEach(function (l) { l[0].removeEventListener(l[1], l[2], l[3]); });
        window.removeEventListener('resize', resize);
        if (ro) ro.disconnect();
        clearScene();
        Object.keys(mats).forEach(function (k) { mats[k].dispose(); });
        if (horizonTex) horizonTex.dispose();
        if (contactTex) contactTex.dispose();
        if (aerial) { aerial.tex.dispose(); aerial.texMuted.dispose(); }
        renderer.dispose();
      },
    };
  };
  H3._roofModel = roofModel;
  H3._buildingArrays = buildingArrays;
  H3._blockHeight = blockHeight;
  H3._clipToCell = clipToCell;
  H3._terrainSampler = terrainSampler;
})();
/* Open Estate House 3D — section UI: lazy start, measurements, sun study, layers, listing tours. */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  var Core = window.OpenEstateCore || null;
  var H3 = window.OpenEstateHouse3D;
  if (!H3 || H3.mount) return;
  var T = H3.T, P = H3.P, escH = H3.escH, isNum = H3.isNum, ftm = H3.ftm, sqftm = H3.sqftm, nf = H3.nf;
  var TOUR_KEY = 'openestate:house3d:tour:v1:';

  var CSS = [
    '.h3d-stage{position:relative;width:100%;aspect-ratio:4/3;min-height:360px;max-height:min(820px,86vh);border-radius:14px;border:1px solid var(--line,#e5e7eb);overflow:hidden;background:linear-gradient(180deg,#89bef0 0%,#c4dff7 26%,#dce9f6 48%,#dce9f6 100%);box-shadow:0 12px 32px rgba(15,23,42,.08);outline:none;user-select:none;-webkit-user-select:none}',
    '.h3d-stage:focus-visible{box-shadow:0 0 0 3px rgba(20,184,166,.3)}',
    '.h3d-stage:fullscreen{max-height:none;border-radius:0;border:0;aspect-ratio:auto;width:100vw;height:100vh}',
    '.h3d-canvas{position:absolute;inset:0;width:100%;height:100%;display:block;touch-action:pan-y;cursor:grab}',
    '.h3d-canvas:active{cursor:grabbing}',
    '.h3d-stage:fullscreen .h3d-canvas{touch-action:none}',
    '.h3d-labels{position:absolute;inset:0;pointer-events:none;overflow:hidden}',
    '.h3d-label{position:absolute;left:0;top:0;white-space:nowrap;font:700 11.5px/1.2 var(--font,system-ui,sans-serif);will-change:transform}',
    '.h3d-label-home{background:linear-gradient(135deg,#0f766e,#0d9488);color:#fff;padding:4px 10px;border-radius:999px;border:1px solid rgba(255,255,255,.32);box-shadow:0 4px 12px rgba(15,23,42,.3)}',
    '.h3d-label-home:after{content:"";position:absolute;left:50%;top:100%;margin-left:-4px;border:4px solid transparent;border-top-color:#0d9488}',
    '.h3d-label-acc{background:linear-gradient(135deg,#6b21a8,#9333ea);color:#fff;padding:3px 9px;border-radius:999px;border:1px solid rgba(255,255,255,.32);box-shadow:0 3px 10px rgba(15,23,42,.26);font-size:11px}',
    '.h3d-label-acc:after{content:"";position:absolute;left:50%;top:100%;margin-left:-4px;border:4px solid transparent;border-top-color:#7e22ce}',
    '.h3d-label-front{color:#0f766e;background:rgba(255,255,255,.92);backdrop-filter:blur(6px);-webkit-backdrop-filter:blur(6px);padding:2.5px 8px;border-radius:999px;border:1px solid #99e0d3;box-shadow:0 2px 8px rgba(15,23,42,.12)}',
    '.h3d-label-street{color:#1e293b;font-weight:700;font-size:11px;letter-spacing:.01em;text-shadow:0 0 4px #fff,0 0 4px #fff,0 1px 6px #fff}',
    '.h3d-label-dim{background:rgba(255,255,255,.95);color:#0f172a;border:1px solid rgba(15,23,42,.2);border-radius:6px;padding:2px 7px;font-weight:600;font-size:11.5px;box-shadow:0 2px 8px rgba(15,23,42,.18)}',
    '.h3d-label-dim b{font-weight:800}.h3d-label-dim i{font-style:normal;font-weight:700;color:#b45309}',
    '.h3d-dim-up{margin-top:-7px}.h3d-dim-side{margin-left:46px;margin-top:11px}.h3d-dim-face{margin-top:-5px}.h3d-dim-note{border-color:#99e0d3;color:#0f5f58}',
    '.h3d-measure{display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin:10px 0 0}',
    '.h3d-mlbl{font:700 12px/1.3 var(--font,system-ui,sans-serif);color:var(--muted,#6b7280);margin-right:2px}',
    '.h3d-chip.light{color:#92400e;border-color:#fde68a}',
    '.h3d-hud{position:absolute;display:flex;gap:6px;align-items:center;z-index:3}',
    '.h3d-badge{left:10px;top:10px;flex-wrap:wrap;max-width:62%}',
    '.h3d-chip{background:rgba(255,255,255,.9);backdrop-filter:blur(10px);-webkit-backdrop-filter:blur(10px);border:1px solid rgba(17,24,39,.1);border-radius:999px;padding:4px 10px;font:700 11.5px/1.3 var(--font,system-ui,sans-serif);color:#1f2937;box-shadow:0 4px 12px rgba(15,23,42,.1);display:inline-flex;align-items:center;gap:6px}',
    '.h3d-chip.ok{color:#15803d;border-color:#bbf7d0}.h3d-chip.est{color:#b45309;border-color:#fde68a}.h3d-chip.busy{color:#1d4ed8;border-color:#bfdbfe}',
    '.h3d-dot{width:8px;height:8px;border-radius:50%;background:currentColor;display:inline-block}',
    '.h3d-chip.busy .h3d-dot{animation:h3dpulse 1.1s ease-in-out infinite}',
    '@keyframes h3dpulse{0%,100%{opacity:.25}50%{opacity:1}}',
    '.h3d-tools{right:10px;top:10px;flex-direction:column;align-items:flex-end}',
    '.h3d-btnrow{display:inline-flex;background:rgba(255,255,255,.9);backdrop-filter:blur(10px);-webkit-backdrop-filter:blur(10px);border:1px solid rgba(17,24,39,.1);border-radius:10px;box-shadow:0 4px 12px rgba(15,23,42,.1);overflow:hidden}',
    '.h3d-btnrow button{border:0;background:transparent;font:700 12px/1 var(--font,system-ui,sans-serif);color:#374151;padding:8px 10px;cursor:pointer;min-width:34px}',
    '.h3d-btnrow button+button{border-left:1px solid rgba(17,24,39,.08)}',
    '.h3d-btnrow button:hover{background:#f3f4f6;color:#111827}.h3d-btnrow button.on{background:#0f766e;color:#fff}',
    '.h3d-compass{right:12px;bottom:58px;width:44px;height:44px;border-radius:50%;background:rgba(255,255,255,.9);backdrop-filter:blur(10px);-webkit-backdrop-filter:blur(10px);border:1px solid rgba(17,24,39,.1);box-shadow:0 4px 12px rgba(15,23,42,.14);justify-content:center;cursor:pointer}',
    '.h3d-needle{position:absolute;inset:0;--h3d-a:0deg;transform:rotate(var(--h3d-a))}',
    '.h3d-needle:before{content:"";position:absolute;left:50%;top:3px;margin-left:-5px;border:5px solid transparent;border-bottom:12px solid #dc2626}',
    '.h3d-needle:after{content:"N";position:absolute;left:50%;top:19px;width:12px;margin-left:-6px;text-align:center;font:800 10.5px/1 var(--font,system-ui,sans-serif);color:#111827;transform:rotate(calc(-1 * var(--h3d-a)))}',
    '.h3d-sun{left:10px;right:10px;bottom:10px;box-sizing:border-box;display:grid;grid-template-columns:16px 154px minmax(0,1fr) 284px auto 34px;align-items:center;gap:8px;background:rgba(255,255,255,.92);backdrop-filter:blur(10px);-webkit-backdrop-filter:blur(10px);border:1px solid rgba(17,24,39,.1);border-radius:12px;box-shadow:0 4px 14px rgba(15,23,42,.12);padding:6px 10px;font:600 12px/1.2 var(--font,system-ui,sans-serif);color:#374151;user-select:none;-webkit-user-select:none}',
    '.h3d-sun>span[aria-hidden]{width:16px;text-align:center;line-height:1}',
    '.h3d-sun select{box-sizing:border-box;width:100%;min-width:0;max-width:none;height:28px;font:600 12px var(--font,system-ui,sans-serif);border:1px solid #e5e7eb;border-radius:8px;padding:0 6px;background:#fff;color:#111827;cursor:pointer}',
    '.h3d-sun input[type=range]{box-sizing:border-box;width:100%;min-width:0;height:24px;margin:0;padding:0;accent-color:#d97706;cursor:pointer;touch-action:none}',
    '.h3d-sun button{box-sizing:border-box;width:34px;height:28px;padding:0;display:inline-flex;align-items:center;justify-content:center;border:1px solid #e5e7eb;background:#fff;border-radius:8px;font:700 12px/1 var(--font,system-ui,sans-serif);cursor:pointer;color:#374151}',
    '.h3d-sun button:hover{background:#f9fafb;border-color:#d1d5db}',
    '.h3d-sun button.h3d-now{width:auto;padding:0 9px;white-space:nowrap}',
    '.h3d-sun button.on{background:#d97706;border-color:#d97706;color:#fff}',
    '.h3d-sunread{box-sizing:border-box;width:100%;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-variant-numeric:tabular-nums;text-align:right}',
    '.h3d-tip{position:absolute;z-index:4;max-width:250px;background:rgba(17,24,39,.94);color:#fff;border-radius:10px;padding:8px 11px;font:500 12px/1.45 var(--font,system-ui,sans-serif);pointer-events:none;box-shadow:0 8px 24px rgba(0,0,0,.3);display:none}',
    '.h3d-tip b{display:block;font-size:12.5px;margin-bottom:2px}',
    '.h3d-hint{left:50%;bottom:58px;transform:translateX(-50%);background:rgba(17,24,39,.78);color:#fff;border-radius:999px;padding:5px 12px;font:600 11.5px/1.3 var(--font,system-ui,sans-serif);pointer-events:none;transition:opacity .4s;white-space:nowrap;max-width:92%;overflow:hidden;text-overflow:ellipsis}',
    '.h3d-attr{position:absolute;left:12px;bottom:58px;z-index:2;font:500 10px/1.2 var(--font,system-ui,sans-serif);color:rgba(17,24,39,.72);text-shadow:0 0 3px #fff,0 0 3px #fff;pointer-events:none;max-width:55%}',
    '.h3d-cover{position:absolute;inset:0;z-index:5;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:10px;text-align:center;padding:20px;background:linear-gradient(#cfe2f4,#eef3f8);color:#1f2937;font:600 13.5px/1.5 var(--font,system-ui,sans-serif)}',
    '.h3d-cover small{font-weight:500;color:#6b7280;max-width:440px}',
    '.h3d-cover button{border:0;background:#0f766e;color:#fff;border-radius:999px;font:700 13px var(--font,system-ui,sans-serif);padding:9px 18px;cursor:pointer}',
    '.h3d-spin{width:26px;height:26px;border-radius:50%;border:3px solid rgba(15,118,110,.2);border-top-color:#0f766e;animation:h3dspin .8s linear infinite}',
    '@keyframes h3dspin{to{transform:rotate(360deg)}}',
    '.h3d-printimg{display:none;width:100%;border-radius:10px;border:1px solid #d1d5db}',
    '.h3d-layers{margin:10px 0 12px}',
    '.h3d-tiles .tile .v{font-size:17px}',
    '.h3d-warn{background:var(--watchbg,#fffbeb);color:var(--watch,#b45309);border:1px solid #fde68a;border-radius:10px;padding:7px 11px;font-size:12.5px;margin:8px 0}',
    '.h3d-src{font-size:12px;color:var(--muted,#6b7280);margin:8px 0 0;line-height:1.5}',
    '.h3d-tour{margin-top:18px;border-top:1px solid var(--line,#e5e7eb);padding-top:4px}',
    '.h3d-tourframe{position:relative;width:100%;aspect-ratio:16/10;min-height:300px;max-height:640px;border-radius:14px;border:1px solid var(--line,#e5e7eb);overflow:hidden;background:#0f172a}',
    '.h3d-tourframe iframe{position:absolute;inset:0;width:100%;height:100%;border:0}',
    '.h3d-tourposter{position:absolute;inset:0;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:10px;color:#e5e7eb;text-align:center;padding:18px;font:600 13.5px/1.5 var(--font,system-ui,sans-serif);background:radial-gradient(circle at 50% 35%,#1e3a5f,#0f172a)}',
    '.h3d-tourposter button{border:0;background:#14b8a6;color:#062a26;border-radius:999px;font:800 13px var(--font,system-ui,sans-serif);padding:10px 20px;cursor:pointer}',
    '.h3d-tourposter small{color:#94a3b8;font-weight:500;max-width:460px}',
    '.h3d-paste{display:flex;gap:8px;flex-wrap:wrap;margin:10px 0 6px}',
    '.h3d-paste input{flex:1;min-width:220px;border:1px solid var(--line,#e5e7eb);border-radius:10px;padding:8px 11px;font:500 13px var(--font,system-ui,sans-serif);outline:none}',
    '.h3d-paste input:focus{border-color:#0f766e;box-shadow:0 0 0 3px rgba(20,184,166,.16)}',
    '.h3d-paste button{border:0;background:#0f766e;color:#fff;border-radius:10px;font:700 12.5px var(--font,system-ui,sans-serif);padding:8px 14px;cursor:pointer}',
    '.h3d-paste button.ghost{background:#fff;color:#374151;border:1px solid var(--line,#e5e7eb)}',
    '.h3d-msg{font-size:12.5px;margin:4px 0;color:#b42318}',
    '.h3d-short{display:none}',
    '@media (max-width:1220px){.h3d-sun{grid-template-columns:16px 148px minmax(0,1fr) 160px auto 34px}.h3d-sun-rs{display:none}}',
    '@media (max-width:640px){.h3d-stage{aspect-ratio:4/5;min-height:400px}.h3d-long{display:none}.h3d-short{display:inline}.h3d-badge{max-width:34%}.h3d-hide-sm{display:none!important}' +
      '.h3d-sun{grid-template-columns:136px minmax(0,1fr) auto 34px;gap:6px 8px;padding:7px 10px}.h3d-sun>span[aria-hidden]{display:none}.h3d-sun select{grid-column:1/2;grid-row:1}.h3d-sunread{grid-column:2/5;grid-row:1;text-align:right}.h3d-sun input[type=range]{grid-column:1/3;grid-row:2}.h3d-sun button.h3d-now{grid-column:3/4;grid-row:2}.h3d-sun #h3dPlay{grid-column:4/5;grid-row:2}' +
      '.h3d-hint{display:none}.h3d-compass,.h3d-attr{bottom:96px}.h3d-attr{max-width:68%}.h3d-btnrow button{padding:8px 9px;min-width:32px}}',
    '@media print{.h3d-stage,.h3d-layers,.h3d-measure,.h3d-paste,.h3d-tourframe{display:none!important}.h3d-printimg.ready{display:block!important}}',
  ].join('\n');

  var st = {
    rep: null, slug: '', sec: null, viewer: null, scene: null, serverTried: false, started: false, failed: false,
    lat: null, lon: null, tz: 'America/Chicago', sun: { preset: 'today', minutes: 630, mode: 'morning', playing: false, raf: 0 },
    measure: 'heights', busy: false,
    warnings: [], hintTimer: 0, tours: [], tourIdx: 0, tourLoaded: false,
  };
  var $ = function (sel) { return st.sec && st.sec.querySelector ? st.sec.querySelector(sel) : null; };
  var setHtml = function (sel, html) { var el = $(sel); if (el) el.innerHTML = html; return el; };

  function injectCss() {
    if (document.getElementById('h3d-css')) return;
    var el = document.createElement('style');
    el.id = 'h3d-css';
    el.textContent = CSS;
    var host = document.head || document.body;
    if (host && host.appendChild) host.appendChild(el);
  }

  /* ------------------------------------------------------------------ shell */
  function shellHtml(hasPlace) {
    var views = [['orbit', '3D', '3D', '3D'], ['top', 'Top', '俯视', 'Planta'], ['front', 'Front', '正面', 'Frente'], ['back', 'Back', '背面', 'Atrás']];
    var layers = [['outlines', 'Outlines', '轮廓线', 'Contornos'], ['aerial', 'Aerial imagery', '航拍影像', 'Imagen aérea'], ['roofphoto', 'Photo on roofs', '屋顶贴航拍', 'Foto en techos'], ['trees', 'Trees', '树木', 'Árboles'], ['neighbors', 'Neighbors', '邻近建筑', 'Vecinos'], ['shadows', 'Shadows', '日照阴影', 'Sombras'], ['labels', 'Labels', '标注', 'Etiquetas'], ['lot', 'Lot line', '地块红线', 'Límite del lote']];
    // Measurements drawn on the model, one kind at a time.
    var measures = [['heights', 'Heights', '高度', 'Alturas'], ['roof', 'Roof faces', '屋面', 'Faldones'], ['lot', 'Lot edges', '地块边长', 'Lados del lote'], ['gap', 'Neighbor gap', '邻栋间距', 'Separación del vecino'], ['tree', 'Tallest tree', '最高树木', 'Árbol más alto']];
    var hex = function (n) { return '#' + ('00000' + n.toString(16)).slice(-6); };
    var OL = H3.OUTLINE || { subject: 0xfacc15, accessory: 0xe879f9 };
    // Legend swatches, matching the satellite map: home, garage / ADU, lot line.
    var swatch = { outlines: [hex(OL.subject), hex(OL.accessory)], lot: ['#38bdf8'] };
    var stage = !hasPlace ? '' :
      '<div class="h3d-stage" id="h3dStage" tabindex="0" role="application" aria-label="Interactive 3D model of the home">' +
        '<canvas class="h3d-canvas" id="h3dCanvas"></canvas>' +
        '<div class="h3d-labels" id="h3dLabels"></div>' +
        '<div class="h3d-hud h3d-badge" id="h3dBadge"></div>' +
        '<div class="h3d-hud h3d-tools">' +
          '<div class="h3d-btnrow" id="h3dViews">' + views.map(function (v, i) { return '<button type="button" data-view="' + v[0] + '" class="' + (i === 0 ? 'on' : '') + '">' + T(v[1], v[2], v[3]) + '</button>'; }).join('') + '</div>' +
          '<div class="h3d-btnrow"><button type="button" class="h3d-hide-sm" data-act="rotl" title="Rotate left">⟲</button><button type="button" class="h3d-hide-sm" data-act="rotr" title="Rotate right">⟳</button><button type="button" data-act="zin" title="Zoom in">+</button><button type="button" data-act="zout" title="Zoom out">−</button><button type="button" data-act="full" title="Full screen">⛶</button></div>' +
        '</div>' +
        '<div class="h3d-hud h3d-compass" id="h3dCompass" data-act="north" role="button" tabindex="0" title="North" aria-label="Face north"><div class="h3d-needle" id="h3dNeedle"></div></div>' +
        '<div class="h3d-attr" id="h3dAttr"></div>' +
        '<div class="h3d-hud h3d-hint" id="h3dHint"></div>' +
        '<div class="h3d-hud h3d-sun" id="h3dSun">' +
          '<span aria-hidden="true">☀</span>' +
          '<select id="h3dDate" aria-label="Date"></select>' +
          '<input type="range" id="h3dTime" min="0" max="1439" step="1" value="840" aria-label="Time of day">' +
          '<span class="h3d-sunread" id="h3dSunRead"></span>' +
          '<button type="button" class="h3d-now" id="h3dNow" data-act="now" title="Sun at the current time">' + T('Now', '现在', 'Ahora') + '</button>' +
          '<button type="button" id="h3dPlay" data-act="play" aria-pressed="false" aria-label="Play the day" title="Play the day">▶</button>' +
        '</div>' +
        '<div class="h3d-tip" id="h3dTip"></div>' +
        '<div class="h3d-cover" id="h3dCover"></div>' +
      '</div>' +
      '<img class="h3d-printimg" id="h3dPrint" alt="3D model snapshot">' +
      '<div class="chips h3d-layers" id="h3dLayers">' + layers.map(function (l) { return '<span class="chip' + (l[0] === 'roofphoto' ? '' : ' on') + '" data-layer="' + l[0] + '">' + (swatch[l[0]] || []).map(function (c) { return '<i style="background:' + c + '"></i>'; }).join('') + T(l[1], l[2], l[3]) + '</span>'; }).join('') + '</div>';
    if (hasPlace) stage = stage.replace('<div class="chips h3d-layers"', '<div class="h3d-measure" id="h3dMeasure"><span class="h3d-mlbl">' + T('Measure on the model:', '在模型上标注：', 'Medir sobre el modelo:') + '</span>' +
      measures.map(function (m) { return '<span class="chip' + (m[0] === st.measure ? ' on' : '') + '" data-measure="' + m[0] + '">' + T(m[1], m[2], m[3]) + '</span>'; }).join('') + '</div><div class="chips h3d-layers"');
    return '<div class="eyebrow">' + T('Interactive 3D', '三维实景', '3D interactivo') + '</div>' +
      '<div class="stitle"><h2>' + T('3D model of the home & lot', '房屋与地块三维模型', 'Modelo 3D de la casa y el lote') + '</h2>' +
      '<div class="meta">' + T('Built automatically from public data · USGS 3DEP LiDAR · OpenStreetMap · aerial imagery on the ground', '由公开数据自动生成 · USGS 3DEP 激光雷达 · OpenStreetMap · 航拍影像', 'Generado con datos públicos · LiDAR USGS 3DEP · OpenStreetMap · imagen aérea') + '</div></div>' +
      stage +
      '<div id="h3dWarn"></div>' +
      '<div class="tiles h3d-tiles" id="h3dTiles"></div>' +
      '<div class="h3d-src" id="h3dSrc"></div>' +
      '<div class="h3d-tour" id="h3dTour"></div>';
  }

  /* ------------------------------------------------------------------ measurements */
  var SHAPE = {
    gable: ['Gable', '双坡屋顶', 'A dos aguas'], hip: ['Hip', '四坡屋顶', 'A cuatro aguas'], flat: ['Flat / low-slope', '平屋顶', 'Plano'],
    shed: ['Shed (single slope)', '单坡屋顶', 'A un agua'], complex: ['Multi-plane', '多坡组合屋顶', 'Varios planos'], unknown: ['—', '—', '—'],
  };
  function sourceLabel(src, year) {
    if (src === 'lidar') return T('Measured · LiDAR' + (year ? ' ' + year : ''), '实测 · 激光雷达' + (year ? ' ' + year : ''), 'Medido · LiDAR' + (year ? ' ' + year : ''));
    if (src === 'osm') return T('Mapped height · OpenStreetMap', '地图标注高度 · OpenStreetMap', 'Altura cartografiada · OpenStreetMap');
    if (src === 'levels') return T('Estimated from storey count', '按楼层数估算', 'Estimado por número de pisos');
    return T('Height unavailable · 2D footprint only', '高度数据缺失 · 仅显示二维轮廓', 'Altura no disponible · solo huella 2D');
  }
  function tile(kHtml, v, dHtml, hl) {
    return '<div class="tile' + (hl ? ' hl' : '') + '"><div class="k">' + kHtml + '</div><div class="v">' + v + '</div>' + (dHtml ? '<div class="d">' + dHtml + '</div>' : '') + '</div>';
  }
  function pitchRise(deg) { return isNum(deg) ? Math.round(Math.tan((deg * Math.PI) / 180) * 12) : null; }
  function renderTiles() {
    var sc = st.scene, out = [];
    if (!sc) { setHtml('#h3dTiles', ''); return; }
    var sb = sc.subject, m = sc.metrics || {}, li = sc.lidar, year = li && li.surveyYear;
    if (sb) {
      var measured = sb.heightSource === 'lidar';
      if (isNum(sb.ridgeM)) {
        out.push(tile(T('Roof ridge height', '屋脊高度', 'Altura de cumbrera'), escH(nf(sb.ridgeM / H3.FT, 0)) + ' ft', escH(nf(sb.ridgeM, 1)) + ' m · ' + sourceLabel(sb.heightSource, year), measured));
      }
      if (isNum(sb.eaveM)) {
        out.push(tile(T('Eave (wall-top) height', '檐口高度', 'Altura de alero'), escH(nf(sb.eaveM / H3.FT, 0)) + ' ft', escH(nf(sb.eaveM, 1)) + ' m · ' + sourceLabel(sb.heightSource, year)));
      }
      if (!isNum(sb.ridgeM) && !isNum(sb.eaveM)) {
        out.push(tile(T('Building height', '建筑高度', 'Altura del edificio'), '—', sourceLabel('unavailable', year)));
      }
      if (measured && sb.roofShape) {
        var shp = SHAPE[sb.roofShape] || SHAPE.unknown, rise = pitchRise(sb.pitchDeg);
        out.push(tile(T('Roof form', '屋顶形式', 'Forma del techo'), T(shp[0], shp[1], shp[2]), isNum(sb.pitchDeg) && sb.roofShape !== 'flat' ? T('Typical pitch', '典型坡度', 'Pendiente típica') + ' ' + escH(nf(sb.pitchDeg, 0)) + '° (' + rise + ':12)' : ''));
      }
      if (measured && Array.isArray(sb.roofPlanes) && sb.roofPlanes.length) {
        var sunny = sb.roofPlanes.filter(function (p) { return ['S', 'SE', 'SW', 'E', 'W'].indexOf(p.aspect) >= 0; }).sort(function (a, b) { return b.areaM2 - a.areaM2; })[0];
        var top = sb.roofPlanes.slice(0, 3).map(function (p) { return H3.compassT(p.aspect) + ' ' + escH(nf(p.areaM2 * 10.7639104, 0)) + ' sq ft'; }).join(' · ');
        out.push(tile(T('Largest sun-facing roof face', '最大向阳屋面', 'Mayor faldón soleado'), sunny ? T('Faces', '朝', 'Mira al') + ' ' + H3.compassT(sunny.aspect) : '—',
          (sunny ? escH(nf(sunny.areaM2 * 10.7639104, 0)) + ' sq ft · ' + escH(nf(sunny.pitchDeg, 0)) + '° · ' : T('No face looks south, east or west', '没有朝南、东或西的屋面', 'Ningún faldón mira al sur, este u oeste') + ' · ') + T('all faces:', '全部屋面：', 'todos:') + ' ' + top));
      }
      out.push(tile(T('Building footprint', '建筑占地轮廓', 'Huella del edificio'), escH(nf(sb.areaM2 * 10.7639104, 0)) + ' sq ft', escH(nf(sb.areaM2, 0)) + ' m² · ' + (sb.outline === 'lidar' ? T('outline traced from LiDAR', '轮廓由激光雷达提取', 'contorno derivado de LiDAR') : T('outline from OpenStreetMap', '轮廓来自 OpenStreetMap', 'contorno de OpenStreetMap'))));
    }
    if (isNum(m.lotSlopePct)) {
      var dn = isNum(m.lotDownhillBearing) ? H3.compass8(m.lotDownhillBearing) : null;
      out.push(tile(m.slopeBasis === 'parcel' ? T('Lot slope', '地块坡度', 'Pendiente del lote') : T('Ground slope around the home', '房屋周边地面坡度', 'Pendiente junto a la casa'), escH(nf(m.lotSlopePct, 1)) + '%',
        (m.lotSlopePct < 1 ? T('Essentially level', '基本平整', 'Prácticamente plano') : T('Falls toward', '向下倾斜方向', 'Desciende hacia') + ' ' + (dn ? H3.compassT(dn) : '')) + (isNum(m.lotReliefM) ? ' · ' + escH(nf(m.lotReliefM / H3.FT, 0)) + ' ft ' + T('relief', '高差', 'de desnivel') : '')));
    }
    if (isNum(m.tallestTreeM)) {
      out.push(tile(T('Tallest tree within 100 ft', '100 英尺内最高树木', 'Árbol más alto a 30 m'), escH(nf(m.tallestTreeM / H3.FT, 0)) + ' ft',
        escH(nf(m.tallestTreeM, 1)) + ' m · ' + (m.tallestTreeDistM < 0.5 ? T('overhangs the roof', '树冠覆盖屋顶', 'sobre el techo') : escH(nf(m.tallestTreeDistM / H3.FT, 0)) + ' ft ' + T('from the house', '（距房屋）', 'de la casa')) + (m.canopyNearRoof ? ' · ' + T('canopy within 6 ft of the roof', '树冠距屋顶不足 6 英尺', 'copa a menos de 2 m del techo') : '')));
    }
    if (isNum(m.nearestNeighborM)) {
      out.push(tile(T('Nearest neighboring building', '最近邻栋间距', 'Edificio vecino más cercano'), escH(nf(m.nearestNeighborM / H3.FT, 0)) + ' ft', escH(nf(m.nearestNeighborM, 1)) + ' m · ' + T('wall to wall', '外墙净距', 'de muro a muro')));
    }
    if (li && sc.origin && isNum(sc.origin.elevM)) {
      out.push(tile(T('Ground elevation', '地面高程', 'Elevación del terreno'), escH(nf(sc.origin.elevM / H3.FT, 0)) + ' ft', escH(nf(sc.origin.elevM, 1)) + ' m · NAVD88'));
    }
    setHtml('#h3dTiles', out.join(''));
    // Provenance
    var src = [];
    if (li) src.push(T('Roofs, terrain and trees: USGS 3DEP airborne LiDAR', '屋顶、地形与树木：USGS 3DEP 机载激光雷达', 'Techos, terreno y árboles: LiDAR aéreo USGS 3DEP') + ' (' + escH(li.project || '') + (li.surveyYear ? ', ' + li.surveyYear : '') + (isNum(li.densityPerM2) ? ', ' + escH(nf(li.densityPerM2, 0)) + ' pts/m²' : '') + ')');
    var satCfg = H3.resolveSatelliteConfig(st.rep, sc && sc.origin && sc.origin.lat, sc && sc.origin && sc.origin.lon);
    src.push(T('Footprints and streets: © OpenStreetMap contributors', '建筑轮廓与道路：© OpenStreetMap 贡献者', 'Huellas y calles: © colaboradores de OpenStreetMap'));
    src.push(T('Ground imagery: ', '地面影像：', 'Imagen: ') + escH(satCfg.attribution));
    src.push(T('Roofs are drawn as flat faces fitted to the survey; a building whose roof could not be fitted, or that has no survey, is a simplified flat-topped block', '屋顶按测绘数据拟合为平整屋面绘制；无法拟合或没有测绘数据的建筑以简化平顶体块表示', 'Los techos se dibujan como faldones planos ajustados al levantamiento; un edificio sin ajuste o sin levantamiento es un bloque simplificado de techo plano'));
    setHtml('#h3dSrc', src.join(' · ') + '. ' + T('Exterior massing only — an automated model, not a survey; interiors are not modelled. Verify dimensions on site.', '仅为外部体量示意——自动生成模型，并非测绘成果；不含室内。尺寸请以现场核实为准。', 'Solo volumen exterior: modelo automático, no un levantamiento; no incluye interiores. Verifique medidas en sitio.'));
    setHtml('#h3dWarn', (st.warnings || []).map(function (w) { return '<div class="h3d-warn">' + escH(w) + '</div>'; }).join(''));
  }
  function subjectSimplified(sc) {
    var b = sc && sc.subject && Array.isArray(sc.buildings) ? sc.buildings[sc.subject.index] : null;
    return Boolean(b) && b.heightSource !== 'unavailable' && !(b.roofFit && Array.isArray(b.roofFit.planes) && b.roofFit.planes.length);
  }
  function renderBadge(busy) {
    if (busy !== undefined) st.busy = Boolean(busy);
    busy = st.busy;
    var sc = st.scene, html = '';
    // Each chip carries a short form for narrow screens.
    var chip = function (cls, longHtml, shortHtml) { return '<span class="h3d-chip ' + cls + '"><i class="h3d-dot"></i><span class="h3d-long">' + longHtml + '</span><span class="h3d-short">' + shortHtml + '</span></span>'; };
    var yr = sc && sc.lidar && sc.lidar.surveyYear ? ' ' + sc.lidar.surveyYear : '';
    if (sc && sc.quality === 'lidar' && sc.subject && sc.subject.heightSource === 'lidar') {
      html = chip('ok', T('Measured · USGS LiDAR', '实测 · USGS 激光雷达', 'Medido · LiDAR USGS') + yr, 'LiDAR' + yr);
    } else if (sc && sc.quality === 'lidar' && sc.subject && sc.subject.heightSource === 'unavailable') {
      html = chip('est', T('Surroundings measured · 2D footprint (height unavailable)', '周边为实测 · 本房仅二维轮廓（高度缺失）', 'Entorno medido · huella 2D (altura no disponible)'), T('2D footprint', '二维轮廓', 'Huella 2D'));
    } else if (sc && sc.quality === 'lidar') {
      html = chip('est', T('Surroundings measured · this roof estimated', '周边为实测 · 本房屋顶为估算', 'Entorno medido · este techo estimado'), T('Roof estimated', '屋顶为估算', 'Techo estimado'));
    } else if (sc && sc.subject && sc.subject.heightSource === 'unavailable') {
      html = chip('est', T('2D footprint · building height unavailable', '二维轮廓 · 未获取建筑高度', 'Huella 2D · altura de edificio no disponible'), T('2D footprint', '二维轮廓', 'Huella 2D'));
    } else if (sc && sc.subject) {
      html = chip('est', T('Footprint model · heights from storey/map data', '轮廓模型 · 高度来自楼层/地图数据', 'Modelo de huella · alturas por pisos/mapa'), T('Storey/map height', '楼层/地图高度', 'Altura pisos/mapa'));
    } else {
      html = chip('est', T('No building outline found here', '此处未找到建筑轮廓', 'Sin contorno de edificio'), T('No outline', '无轮廓', 'Sin contorno'));
    }
    if (subjectSimplified(sc)) html += chip('est', T('Simplified block · roof shape not modelled', '简化体块 · 未建屋顶形状', 'Bloque simplificado · forma del techo no modelada'), T('Simplified', '简化', 'Simplificado'));
    if (st.sun.mode === 'morning') html += chip('light', T('Late-morning light · today', '上午光照 · 今天', 'Luz de media mañana · hoy'), T('10:30 AM light', '上午光照', 'Luz 10:30'));
    else if (st.sun.mode === 'now') html += chip('light', T('Sun as of now', '当前时刻光照', 'Sol de ahora'), T('Now', '现在', 'Ahora'));
    if (busy) html += chip('busy', T('Refining with USGS LiDAR…', '正在用 USGS 激光雷达精化…', 'Refinando con LiDAR USGS…'), 'LiDAR…');
    setHtml('#h3dBadge', html);
  }

  /* ------------------------------------------------------------------ sun study */
  function todayParts() { return H3.localParts(st.tz, st.lon, Date.now()); }
  function presetDate(preset) {
    var now = todayParts();
    if (preset === 'jun') return { y: now.y, m: 5, d: 21 };
    if (preset === 'sep') return { y: now.y, m: 8, d: 22 };
    if (preset === 'dec') return { y: now.y, m: 11, d: 21 };
    if (preset === 'mar') return { y: now.y, m: 2, d: 20 };
    return { y: now.y, m: now.m, d: now.d };
  }
  function sunMsNow() { var d = presetDate(st.sun.preset); return H3.localToUtc(st.tz, st.lon, d.y, d.m, d.d, st.sun.minutes); }
  function daylightRange() {
    var key = st.sun.preset + '|' + st.tz + '|' + st.lat + '|' + st.lon;
    if (st.sun.dlCache && st.sun.dlCache.key === key) return st.sun.dlCache;
    var d = presetDate(st.sun.preset), rise = null, set = null, prev = null;
    for (var mm = 0; mm <= 1440; mm += 10) {
      var alt = H3.sunPosition(H3.localToUtc(st.tz, st.lon, d.y, d.m, d.d, mm), st.lat, st.lon).altitude;
      if (prev != null) {
        if (prev <= -0.0145 && alt > -0.0145 && rise == null) rise = mm - 5;
        if (prev > -0.0145 && alt <= -0.0145) set = mm - 5;
      }
      prev = alt;
    }
    st.sun.dlCache = { key: key, rise: rise == null ? 360 : rise, set: set == null ? 1080 : set };
    return st.sun.dlCache;
  }
  function fillDateOptions() {
    var sel = $('#h3dDate');
    if (!sel) return;
    var opts = [['today', P('Today', '今天', 'Hoy')], ['jun', P('Jun 21 · longest day', '6月21日 · 夏至', '21 jun · día más largo')], ['sep', P('Sep 22 · equinox', '9月22日 · 秋分', '22 sep · equinoccio')], ['dec', P('Dec 21 · shortest day', '12月21日 · 冬至', '21 dic · día más corto')], ['mar', P('Mar 20 · equinox', '3月20日 · 春分', '20 mar · equinoccio')]];
    sel.innerHTML = opts.map(function (o) { return '<option value="' + o[0] + '"' + (o[0] === st.sun.preset ? ' selected' : '') + '>' + escH(o[1]) + '</option>'; }).join('');
  }
  function syncPlayBtn() {
    var btn = $('#h3dPlay'), on = Boolean(st.sun.playing);
    if (!btn) return;
    btn.textContent = on ? '❚❚' : '▶';
    if (btn.classList) btn.classList.toggle('on', on);
    if (btn.setAttribute) {
      var lbl = on ? P('Pause sun animation', '暂停日照动画', 'Pausar animación solar') : P('Play the day', '播放全天日照', 'Reproducir el día');
      btn.setAttribute('aria-pressed', on ? 'true' : 'false');
      btn.setAttribute('aria-label', lbl);
      btn.setAttribute('title', lbl);
    }
  }
  function updateSun(fromSlider) {
    if (!st.viewer) return;
    st.viewer.setSunTime(sunMsNow());
    var s = st.viewer.sun(), el = $('#h3dSunRead'), dl = daylightRange();
    if (el) {
      var rsTxt = '↑' + H3.fmtClock(dl.rise) + ' ↓' + H3.fmtClock(dl.set);
      if (el.setAttribute) el.setAttribute('title', rsTxt);
      el.innerHTML = '<b>' + escH(H3.fmtClock(st.sun.minutes)) + '</b> · ' + (s.altitudeDeg > 0
        ? T('sun', '太阳', 'sol') + ' ' + escH(nf(s.altitudeDeg, 0)) + '° ' + H3.compassT(H3.compass8(s.bearing))
        : T('sun is down', '太阳已落', 'sin sol')) +
        '<span class="h3d-sun-rs h3d-hide-sm"> · ' + escH(rsTxt) + '</span>';
    }
    if (!fromSlider) {
      var tr = $('#h3dTime'), want = String(Math.max(0, Math.min(1439, Math.round(st.sun.minutes))));
      if (tr && tr.value !== want) tr.value = want;
    }
  }
  function togglePlay(force) {
    var on = force != null ? Boolean(force) : !st.sun.playing;
    st.sun.playing = on;
    syncPlayBtn();
    if (st.sun.raf) { window.cancelAnimationFrame(st.sun.raf); st.sun.raf = 0; }
    if (!on) { st.sun.playWhenReady = false; return; }
    if (!st.viewer) {
      st.sun.playing = false;
      st.sun.playWhenReady = true;
      syncPlayBtn();
      start();
      return;
    }
    var dl = daylightRange(), lo = Math.max(0, dl.rise - 20), hi = Math.min(1439, dl.set + 20), span = Math.max(60, hi - lo), last = 0;
    if (st.sun.minutes < lo || st.sun.minutes >= hi) st.sun.minutes = lo;
    updateSun(false);
    var tick = function (ts) {
      if (!st.sun.playing) return;
      if (!last) { last = ts; st.sun.raf = window.requestAnimationFrame(tick); return; }
      var dt = Math.min(0.1, Math.max(0, (ts - last) / 1000));
      last = ts;
      // A full day in about twelve seconds.
      st.sun.minutes += dt * (span / 12);
      if (st.sun.minutes >= hi) st.sun.minutes = lo + ((st.sun.minutes - hi) % span);
      updateSun(false);
      st.sun.raf = window.requestAnimationFrame(tick);
    };
    st.sun.raf = window.requestAnimationFrame(tick);
  }
  function skyFor(info) {
    var stage = $('#h3dStage');
    if (!stage || !stage.style) return;
    var mix = function (a, b, t) { return a.map(function (v, i) { return Math.round(v + (b[i] - v) * t); }); };
    var css = function (c) { return 'rgb(' + c.join(',') + ')'; };
    var dayTop = [137, 190, 240], dayMid = [196, 223, 247], dayBot = [220, 233, 246];
    var duskTop = [88, 124, 184], duskMid = [196, 174, 196], duskBot = [232, 213, 194];
    var nightTop = [12, 20, 38], nightMid = [22, 34, 58], nightBot = [27, 38, 63];
    var t = Math.max(0, Math.min(1, 1 - info.low * 1.15 + 0.15));
    var top = mix(nightTop, mix(duskTop, dayTop, t), info.day);
    var mid = mix(nightMid, mix(duskMid, dayMid, t), info.day);
    var bot = mix(nightBot, mix(duskBot, dayBot, t), info.day);
    stage.style.background = 'linear-gradient(180deg,' + css(top) + ' 0%,' + css(mid) + ' 26%,' + css(bot) + ' 46%,' + css(bot) + ' 100%)';
  }

  /* ------------------------------------------------------------------ picking tooltip */
  function showTip(info, x, y) {
    var tip = $('#h3dTip');
    if (!tip || !tip.style) return;
    if (!info) { tip.style.display = 'none'; return; }
    var year = st.scene && st.scene.lidar && st.scene.lidar.surveyYear, html = '';
    if (info.type === 'tree') {
      var t = info.tree;
      html = '<b>' + T('Tree', '树木', 'Árbol') + '</b>' + T('About', '约', 'Aprox.') + ' ' + escH(ftm(t[2])) + ' ' + T('tall', '高', 'de alto') + '<br>' + T('Crown about', '树冠约', 'Copa aprox.') + ' ' + escH(nf((t[3] * 2) / H3.FT, 0)) + ' ft ' + T('wide', '宽', 'de ancho') + '<br><span style="opacity:.7">' + T('Measured · LiDAR', '实测 · 激光雷达', 'Medido · LiDAR') + (year ? ' ' + year : '') + '</span>';
    } else if (info.type === 'face') {
      var f = info.face, rise = pitchRise(f.pitchDeg);
      html = '<b>' + T('Roof face', '屋面', 'Faldón') + (f.aspect ? ' · ' + T('faces', '朝', 'mira al') + ' ' + H3.compassT(f.aspect) : '') + '</b>' +
        T('Area', '面积', 'Área') + ' ' + escH(sqftm(f.areaM2)) + '<br>' +
        (f.aspect && isNum(f.pitchDeg) ? T('Pitch', '坡度', 'Pendiente') + ' ' + rise + ':12 (' + escH(nf(f.pitchDeg, 0)) + '°)' : T('Flat / low-slope', '平屋面', 'Plano')) +
        '<br><span style="opacity:.7">' + T('Measured · LiDAR', '实测 · 激光雷达', 'Medido · LiDAR') + (year ? ' ' + year : '') + '</span>';
    } else {
      var b = info.b, title = b.role === 'subject' ? T('This home', '本房', 'Esta casa') : b.role === 'accessory' ? T(b.label || 'Structure on this lot', '本地块附属建筑', 'Estructura en este lote') : T('Neighboring building', '邻近建筑', 'Edificio vecino');
      var hgtLines = (isNum(b.ridge) || isNum(b.eave))
        ? (T('Roof ridge', '屋脊', 'Cumbrera') + ' ' + escH(ftm(b.ridge)) + '<br>' + T('Eave', '檐口', 'Alero') + ' ' + escH(ftm(b.eave)) + '<br>')
        : '';
      html = '<b>' + title + '</b>' + hgtLines + T('Footprint', '占地', 'Huella') + ' ' + escH(sqftm(b.areaM2)) + '<br><span style="opacity:.7">' + sourceLabel(b.heightSource, year) + (info.simplified && (isNum(b.ridge) || isNum(b.eave)) ? ' · ' + T('drawn as a simplified block', '以简化体块绘制', 'dibujado como bloque simplificado') : '') + '</span>';
    }
    tip.innerHTML = html;
    tip.style.display = 'block';
    var stage = $('#h3dStage'), w = (stage && stage.clientWidth) || 600, h = (stage && stage.clientHeight) || 400;
    tip.style.left = Math.max(8, Math.min(w - 260, x + 14)) + 'px';
    tip.style.top = Math.max(8, Math.min(h - 150, y + 14)) + 'px';
  }

  /* ------------------------------------------------------------------ start-up */
  function showCover(kind) {
    var c = $('#h3dCover');
    if (!c || !c.style) return;
    if (kind === 'hide') { c.style.display = 'none'; return; }
    c.style.display = 'flex';
    if (kind === 'idle') {
      c.innerHTML = '<div>' + T('Interactive 3D model', '可交互三维模型', 'Modelo 3D interactivo') + '</div><small>' + T('Rotate the home, see measured roof heights and trees, and play the sun across the day.', '可旋转查看房屋，了解实测屋顶高度与树木，并模拟全天日照。', 'Gire la casa, vea alturas medidas y árboles, y simule el sol durante el día.') + '</small><button type="button" data-act="start">' + T('Load 3D model', '加载三维模型', 'Cargar modelo 3D') + '</button>';
    } else if (kind === 'loading') {
      c.innerHTML = '<div class="h3d-spin"></div><div>' + T('Building the 3D model…', '正在生成三维模型…', 'Generando el modelo 3D…') + '</div>';
    } else {
      c.innerHTML = '<div>' + T('The 3D view could not start in this browser', '当前浏览器无法启动三维视图', 'La vista 3D no pudo iniciarse en este navegador') + '</div><small>' + T('It needs WebGL and an internet connection to load the 3D engine. The measurements below are still available.', '需要 WebGL 以及联网加载三维引擎。下方测量数据仍可查看。', 'Requiere WebGL y conexión para cargar el motor 3D. Las medidas siguen disponibles abajo.') + '</small><button type="button" data-act="start">' + T('Try again', '重试', 'Reintentar') + '</button>';
    }
  }
  function webglOk() {
    try {
      var c = document.createElement('canvas');
      return Boolean(c && c.getContext && (c.getContext('webgl2') || c.getContext('webgl')));
    } catch (e) { return false; }
  }
  var threePromise = null;
  function loadThree() {
    if (Core && typeof Core.loadThreeJs === 'function') return Core.loadThreeJs(H3.THREE_URLS);
    if (window.THREE && window.THREE.WebGLRenderer) return Promise.resolve(window.THREE);
    if (threePromise) return threePromise;
    var attempt = function (i) {
      if (i >= H3.THREE_URLS.length) return Promise.reject(new Error('3D engine unavailable'));
      return import(H3.THREE_URLS[i]).then(function (mod) {
        if (!window.THREE && mod && mod.WebGLRenderer) {
          try { window.THREE = mod; } catch (e0) {}
        }
        return mod;
      }, function () { return attempt(i + 1); });
    };
    threePromise = attempt(0).catch(function (e) { threePromise = null; throw e; });
    return threePromise;
  }
  /* One request per page, shared by this section and the hero's 3D view (`H3.requestScene`,
     `H3.onScene`). It starts as soon as the page is idle, long before anyone scrolls to the 3D
     section, so the measured roofs are usually in hand when the viewer opens. The server answers
     202 {pending} while it is still downloading the survey; keep polling for up to two minutes
     (the build carries on server-side and lands in its cache either way). */
  var scenePromise = null, sceneListeners = [];
  function applyServerScene(j) {
    if (!(j && j.ok && j.scene && Array.isArray(j.scene.buildings))) return false;
    st.scene = j.scene;
    st.warnings = Array.isArray(j.warnings) ? j.warnings : [];
    if (st.viewer) st.viewer.setScene(st.scene, true);
    syncLotChip();
    renderTiles();
    sceneListeners.forEach(function (cb) { try { cb(j.scene, j); } catch (e) { /* listener's own problem */ } });
    return true;
  }
  function requestServerScene() {
    if (scenePromise) return scenePromise;
    if (!st.slug || typeof fetch !== 'function') return Promise.resolve(null);
    if (typeof location === 'undefined' || !/^https?:$/.test(location.protocol)) return Promise.resolve(null);
    st.serverTried = true;
    renderBadge(true);
    var t0 = Date.now();
    var once = function () {
      var ctrl = typeof AbortController !== 'undefined' ? new AbortController() : null;
      var timer = setTimeout(function () { if (ctrl) ctrl.abort(); }, 30000);
      // No query string is forwarded: a share token must only be counted by the page view itself.
      return fetch('/api/house3d/' + encodeURIComponent(st.slug), { headers: { Accept: 'application/json' }, credentials: 'same-origin', signal: ctrl ? ctrl.signal : undefined })
        .then(function (r) {
          clearTimeout(timer);
          if (r.status === 202) return r.json().then(function (j) { return { pending: true, wait: j && j.retryAfterMs }; }, function () { return { pending: true }; });
          return r.ok ? r.json() : null;
        }, function (e) { clearTimeout(timer); throw e; });
    };
    var loop = function () {
      return once().then(function (res) {
        if (res && res.pending) {
          if (Date.now() - t0 > 120000) return null;
          var wait = Math.max(1500, Math.min(8000, Number(res.wait) || 2500));
          return new Promise(function (done) { setTimeout(done, wait); }).then(loop);
        }
        return res;
      });
    };
    scenePromise = loop().then(function (j) {
      var ok = applyServerScene(j);
      renderBadge(false);
      // Nothing usable (offline, server error): let a later start of the viewer try again.
      if (!ok) scenePromise = null;
      return ok ? j : null;
    }, function () { renderBadge(false); scenePromise = null; return null; });
    return scenePromise;
  }
  function fetchServerScene() { return requestServerScene(); }
  H3.requestScene = requestServerScene;
  /** Subscribes to measured scenes from the server; fires at once when one is already loaded. */
  H3.onScene = function (cb) {
    if (typeof cb !== 'function') return;
    sceneListeners.push(cb);
    if (st.scene && !st.scene.local) { try { cb(st.scene, null); } catch (e) { /* ignore */ } }
  };
  function syncLotChip() {
    var chip = $('#h3dLayers [data-layer="lot"]');
    if (chip && chip.style) chip.style.display = st.scene && Array.isArray(st.scene.lot) && st.scene.lot.length >= 3 ? '' : 'none';
  }
  /** Shows only the measurement buttons this scene has data for, and keeps one of them active. */
  function syncMeasure(avail) {
    avail = avail || (st.viewer ? st.viewer.measureAvailable() : {});
    if (st.measure !== 'none' && !avail[st.measure]) st.measure = avail.heights ? 'heights' : 'none';
    if (st.viewer && st.viewer.measure() !== st.measure) st.measure = st.viewer.setMeasure(st.measure);
    var row = $('#h3dMeasure'), any = false;
    if (!row || !row.querySelectorAll) return;
    Array.prototype.forEach.call(row.querySelectorAll('[data-measure]'), function (c) {
      var k = c.getAttribute('data-measure');
      if (avail[k]) any = true;
      if (c.style) c.style.display = avail[k] ? '' : 'none';
      if (c.classList) c.classList.toggle('on', k === st.measure);
    });
    if (row.style) row.style.display = any ? '' : 'none';
  }
  function setSunMode(mode) {
    if (st.sun.mode === mode) return;
    st.sun.mode = mode;
    renderBadge();
  }
  function setHint(html, ms) {
    var el = $('#h3dHint');
    if (!el || !el.style) return;
    el.innerHTML = html; el.style.opacity = '1';
    if (st.hintTimer) clearTimeout(st.hintTimer);
    st.hintTimer = setTimeout(function () { el.style.opacity = '0'; }, ms || 5000);
  }
  function start() {
    if (st.started) return;
    st.started = true;
    if (!webglOk()) { st.started = false; st.failed = true; showCover('failed'); return; }
    showCover('loading');
    fetchServerScene();
    loadThree().then(function (THREE) {
      var stage = $('#h3dStage'), canvas = $('#h3dCanvas'), labels = $('#h3dLabels');
      var reduced = Boolean(window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches);
      var initialSat = H3.resolveSatelliteConfig(st.rep, st.scene && st.scene.origin && st.scene.origin.lat, st.scene && st.scene.origin && st.scene.origin.lon);
      st.viewer = H3.createViewer(THREE, { stage: stage, canvas: canvas, labels: labels }, {
        reducedMotion: reduced,
        autoOrbit: !reduced,
        onMeasure: syncMeasure,
        onPick: showTip,
        onSun: skyFor,
        onCamera: function (c) { var n = $('#h3dNeedle'); if (n && n.style && n.style.setProperty) n.style.setProperty('--h3d-a', (-c.heading).toFixed(1) + 'deg'); },
        onAerial: function (state, attrText) {
          var liveAttr = attrText || initialSat.attribution;
          setHtml('#h3dAttr', state === 'failed' ? '© OpenStreetMap · USGS 3DEP' : escH(liveAttr) + ' · © OpenStreetMap · USGS 3DEP');
          var chip = $('#h3dLayers [data-layer="aerial"]');
          if (state === 'failed' && chip && chip.classList) chip.classList.remove('on');
        },
        onInteract: function (kind) {
          if (kind === 'wheel-idle') setHint(T('Move pointer inside the 3D view, then scroll to zoom', '将鼠标移入三维视图内即可滚轮缩放', 'Mueva el cursor dentro de la vista 3D y use la rueda'), 2400);
          else { var el = $('#h3dHint'); if (el && el.style) el.style.opacity = '0'; showTip(null); }
        },
      });
      setHtml('#h3dAttr', escH(initialSat.attribution) + ' · © OpenStreetMap · USGS 3DEP');
      // Open in late-morning light on today's date (labelled on the model); "Now" is one tap away.
      if (st.sun.mode === 'morning') { st.sun.preset = 'today'; st.sun.minutes = 630; }
      st.viewer.setSunTime(sunMsNow());
      st.viewer.setScene(st.scene, false);
      if (typeof IntersectionObserver !== 'undefined' && stage) {
        var seen = true, sync = function () { if (st.viewer) st.viewer.setInView(seen && !document.hidden); };
        new IntersectionObserver(function (ents) { seen = ents.some(function (e) { return e.isIntersecting; }); sync(); }, { threshold: 0.02 }).observe(stage);
        if (document.addEventListener) document.addEventListener('visibilitychange', sync);
      }
      renderBadge();
      fillDateOptions();
      syncPlayBtn();
      updateSun(false);
      showCover('hide');
      if (st.sun.playWhenReady) { st.sun.playWhenReady = false; togglePlay(true); }
      setHint(T('Drag to rotate · scroll or pinch to zoom · tap a roof face, building or tree for its numbers', '拖动旋转 · 滚轮或双指缩放 · 点击屋面、建筑或树木查看数据', 'Arrastre para girar · rueda o pellizco para acercar · toque un faldón, edificio o árbol para ver sus medidas'), 6500);
      fetchServerScene();
    }).catch(function (err) {
      if (window.console && console.warn) console.warn('house3d: the 3D view could not start', err);
      st.started = false; st.failed = true;
      showCover('failed');
      // Measurements do not need WebGL.
      fetchServerScene();
    });
  }

  /* ------------------------------------------------------------------ listing tours */
  function savedTour() { try { return (window.localStorage && localStorage.getItem(TOUR_KEY + st.slug)) || ''; } catch (e) { return ''; } }
  function saveTour(url) { try { if (window.localStorage) localStorage.setItem(TOUR_KEY + st.slug, url || ''); } catch (e) { /* private mode */ } }
  function collectTours() {
    var list = H3.toursFromReport(st.rep), saved = savedTour();
    if (saved) {
      var t = H3.parseTourUrl(saved);
      if (t && !list.some(function (x) { return x.embedUrl === t.embedUrl; })) { t.mine = true; list.push(t); }
    }
    st.tours = list;
    if (st.tourIdx >= list.length) st.tourIdx = 0;
  }
  function renderTour(msg) {
    var box = $('#h3dTour');
    if (!box) return;
    collectTours();
    var addr = (st.rep && st.rep.meta && st.rep.meta.displayAddress) || '', q = encodeURIComponent(addr);
    var embeds = st.tours.filter(function (t) { return t.embedUrl; }), links = st.tours.filter(function (t) { return !t.embedUrl; });
    var cur = embeds[Math.min(st.tourIdx, Math.max(0, embeds.length - 1))];
    var html = '<div class="sub">' + T('Interior 3D tour', '室内三维漫游', 'Recorrido 3D interior') + ' <small>' + T('shown live from the tour host when the listing has one', '房源提供时，由漫游托管方实时呈现', 'en vivo desde el servicio del recorrido cuando el anuncio lo incluye') + '</small></div>';
    if (cur) {
      if (embeds.length > 1) {
        html += '<div class="chips">' + embeds.map(function (t, i) { return '<span class="chip' + (t === cur ? ' on' : '') + '" data-tour="' + i + '">' + escH(t.vendor) + (t.mine ? ' · ' + T('added by you', '您添加的', 'añadido por usted') : '') + '</span>'; }).join('') + '</div>';
      }
      html += '<div class="h3d-tourframe" id="h3dTourFrame">' + (st.tourLoaded ? tourIframe(cur) :
        '<div class="h3d-tourposter"><div>' + escH(cur.vendor) + ' · ' + (cur.kind === 'video' ? T('video tour', '视频看房', 'recorrido en video') : T('interactive 3D walkthrough', '可交互三维漫游', 'recorrido 3D interactivo')) + '</div>' +
        '<small>' + T('Loads the tour straight from ' + (cur.generic ? 'its host' : cur.vendor) + '. Walk through rooms, switch to the dollhouse or floor-plan view, and measure where the host allows it.', '直接从' + (cur.generic ? '其托管方' : ' ' + cur.vendor + ' ') + '加载。可逐间浏览、切换立体剖面或平面图视图，并在托管方支持时测量。', 'Se carga directamente desde ' + (cur.generic ? 'su servicio' : cur.vendor) + '. Recorra las habitaciones, cambie a la vista de maqueta o plano y mida donde el servicio lo permita.') + '</small>' +
        '<button type="button" data-act="tourload">' + T('Load interactive tour', '加载交互式漫游', 'Cargar recorrido interactivo') + '</button></div>') + '</div>' +
        '<div class="h3d-src">' + (cur.mine ? T('Link added by you — stored only in this browser.', '此链接由您添加，仅保存在本浏览器中。', 'Enlace añadido por usted; solo se guarda en este navegador.') : T('Tour link from the listing record.', '漫游链接来自房源记录。', 'Enlace del recorrido tomado del anuncio.')) +
        ' <a href="' + escH(cur.openUrl) + '" target="_blank" rel="noopener noreferrer">' + T('Open in a new tab ↗', '在新标签页打开 ↗', 'Abrir en pestaña nueva ↗') + '</a>' +
        (cur.mine ? ' · <a href="#" data-act="tourclear">' + T('Remove', '移除', 'Quitar') + '</a>' : '') + '</div>';
    } else {
      html += '<div class="hint">' + T('No 3D tour link is on record for this home. If the listing has a Matterport, iGUIDE or similar walkthrough, paste its link to explore it here next to the exterior model.', '此房源记录中暂无三维漫游链接。如果房源提供 Matterport、iGUIDE 等漫游，可粘贴链接，在此与外部模型一并查看。', 'No hay enlace de recorrido 3D registrado para esta casa. Si el anuncio tiene un recorrido Matterport, iGUIDE o similar, pegue su enlace para explorarlo aquí junto al modelo exterior.') + '</div>';
    }
    links.forEach(function (t) {
      html += '<div class="h3d-src"><a href="' + escH(t.openUrl) + '" target="_blank" rel="noopener noreferrer">' + T('Open the listing’s virtual tour ↗', '打开房源虚拟看房 ↗', 'Abrir el recorrido virtual del anuncio ↗') + '</a></div>';
    });
    var bn3d = (typeof window !== 'undefined' && window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) || ((typeof R !== 'undefined' && R && R.brand && R.brand.name) ? String(R.brand.name) : 'Open Estate');
    html += '<div class="h3d-paste"><input type="url" id="h3dTourUrl" inputmode="url" autocomplete="off" spellcheck="false" placeholder="' + escH(P('Paste a 3D tour link (Matterport, iGUIDE, YouTube…)', '粘贴三维漫游链接（Matterport、iGUIDE、YouTube…）', 'Pegue un enlace de recorrido 3D (Matterport, iGUIDE, YouTube…)')) + '">' +
      '<button type="button" data-act="touradd">' + T('Show tour', '显示漫游', 'Mostrar recorrido') + '</button>' +
      (addr ? '<a class="h3d-src" style="align-self:center;margin:0" href="https://www.google.com/search?q=' + q + '%203D%20tour" target="_blank" rel="noopener noreferrer">' + T('Search for this home’s tour ↗', '搜索此房的三维漫游 ↗', 'Buscar el recorrido de esta casa ↗') + '</a>' : '') + '</div>' +
      (msg ? '<div class="h3d-msg">' + msg + '</div>' : '') +
      '<div class="h3d-src">' + T('Tours play through the host’s own embedded player; ' + bn3d + ' does not copy, store or alter tour content, which belongs to the listing’s owner.', '漫游通过托管方自带的嵌入式播放器呈现；' + bn3d + ' 不复制、不存储、不修改漫游内容，内容版权归房源方所有。', 'Los recorridos se reproducen con el reproductor del propio servicio; ' + bn3d + ' no copia, almacena ni altera su contenido, que pertenece al titular del anuncio.') + '</div>';
    box.innerHTML = html;
  }
  function tourIframe(t) {
    return '<iframe src="' + escH(t.embedUrl) + '" title="' + escH(t.vendor) + '" loading="lazy" allowfullscreen allow="fullscreen; xr-spatial-tracking; gyroscope; accelerometer" referrerpolicy="strict-origin-when-cross-origin" sandbox="allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms allow-presentation"></iframe>';
  }

  /* ------------------------------------------------------------------ events */
  function onClick(ev) {
    var t = ev.target, el = t && t.closest ? t.closest('[data-act],[data-view],[data-layer],[data-tour],[data-measure]') : null;
    if (!el) return;
    var act = el.getAttribute('data-act'), view = el.getAttribute('data-view'), layer = el.getAttribute('data-layer'), tour = el.getAttribute('data-tour');
    if (act === 'start') { start(); return; }
    if (act === 'play') { setSunMode('custom'); togglePlay(); return; }
    if (act === 'now') {
      if (st.sun.playing) togglePlay(false);
      st.sun.preset = 'today';
      st.sun.minutes = Math.max(0, Math.min(1439, Math.round(todayParts().minutes)));
      setSunMode('now');
      fillDateOptions();
      if (!st.viewer) { start(); return; }
      updateSun(false);
      return;
    }
    if (tour != null) { st.tourIdx = Number(tour) || 0; st.tourLoaded = false; renderTour(); return; }
    if (act === 'tourload') { st.tourLoaded = true; renderTour(); return; }
    if (act === 'tourclear') { ev.preventDefault(); saveTour(''); st.tourLoaded = false; renderTour(); return; }
    if (act === 'touradd') {
      var inp = $('#h3dTourUrl'), raw = inp ? String(inp.value || '').trim() : '', parsed = H3.parseTourUrl(raw);
      if (!raw) return;
      var bn3d2 = (typeof window !== 'undefined' && window.__OPEN_ESTATE_BRAND__ && window.__OPEN_ESTATE_BRAND__.name) || ((typeof R !== 'undefined' && R && R.brand && R.brand.name) ? String(R.brand.name) : 'Open Estate');
      if (!parsed) { renderTour(T('That link is not from a tour service ' + bn3d2 + ' can embed. Supported: Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360, Cupix, YouTube and Vimeo.', '该链接不属于可嵌入的漫游服务。支持：Matterport、iGUIDE、Kuula、CloudPano、RICOH360、Giraffe360、Cupix、YouTube 与 Vimeo。', 'Ese enlace no es de un servicio de recorridos que se pueda incrustar. Compatibles: Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360, Cupix, YouTube y Vimeo.')); return; }
      saveTour(parsed.openUrl);
      collectTours();
      var embeds = st.tours.filter(function (x) { return x.embedUrl; });
      st.tourIdx = Math.max(0, embeds.length - 1); st.tourLoaded = true;
      renderTour();
      return;
    }
    if (!st.viewer) return;
    if (view) {
      st.viewer.setView(view);
      var row = $('#h3dViews');
      if (row && row.querySelectorAll) Array.prototype.forEach.call(row.querySelectorAll('button'), function (b) { b.classList.toggle('on', b === el); });
      return;
    }
    var mk = el.getAttribute('data-measure');
    if (mk) {
      st.measure = st.viewer.setMeasure(st.measure === mk ? 'none' : mk);
      syncMeasure();
      return;
    }
    if (layer) {
      var on = !el.classList.contains('on');
      el.classList.toggle('on', on);
      st.viewer.setLayer(layer, on);
      return;
    }
    if (act === 'zin') st.viewer.zoomBy(0.8);
    else if (act === 'zout') st.viewer.zoomBy(1.25);
    else if (act === 'rotl') st.viewer.rotateBy(30);
    else if (act === 'rotr') st.viewer.rotateBy(-30);
    else if (act === 'north') st.viewer.faceNorth();
    else if (act === 'full') {
      var stage = $('#h3dStage');
      if (document.fullscreenElement) { if (document.exitFullscreen) document.exitFullscreen(); }
      else if (stage && stage.requestFullscreen) stage.requestFullscreen().catch(function () { /* not allowed */ });
    }
  }
  function onInput(ev) {
    var t = ev.target;
    if (!t || t.id !== 'h3dTime') return;
    if (st.sun.playing) togglePlay(false);
    st.sun.minutes = Math.max(0, Math.min(1439, Number(t.value) || 0));
    setSunMode('custom');
    if (!st.viewer) { start(); return; }
    updateSun(true);
  }
  function onChange(ev) {
    var t = ev.target;
    if (!t || !t.id) return;
    if (t.id === 'h3dDate') {
      var wasPlaying = st.sun.playing;
      if (wasPlaying) togglePlay(false);
      st.sun.preset = t.value;
      setSunMode('custom');
      updateSun(false);
      if (wasPlaying) togglePlay(true);
    } else if (t.id === 'h3dTime') {
      var v = Math.max(0, Math.min(1439, Number(t.value) || 0));
      if (Math.abs(v - st.sun.minutes) >= 0.5) {
        if (st.sun.playing) togglePlay(false);
        st.sun.minutes = v;
        setSunMode('custom');
        updateSun(true);
      }
    }
  }
  function onLang() {
    fillDateOptions();
    syncPlayBtn();
    if (st.viewer) updateSun(false);
    var inp = $('#h3dTourUrl');
    if (inp && inp.setAttribute) inp.setAttribute('placeholder', P('Paste a 3D tour link (Matterport, iGUIDE, YouTube…)', '粘贴三维漫游链接（Matterport、iGUIDE、YouTube…）', 'Pegue un enlace de recorrido 3D (Matterport, iGUIDE, YouTube…)'));
  }

  /* ------------------------------------------------------------------ mount */
  function mount() {
    var sec = document.getElementById(H3.SECTION_ID);
    if (!sec || sec.getAttribute && sec.getAttribute('data-h3d') === '1') return;
    st.sec = sec;
    if (sec.setAttribute) sec.setAttribute('data-h3d', '1');
    st.rep = H3.getReport();
    st.slug = String((st.rep && st.rep.meta && st.rep.meta.slug) || '').replace(/[^a-z0-9-]/gi, '').slice(0, 160);
    st.scene = H3.sceneFromReport(st.rep);
    injectCss();
    var hasPlace = Boolean(st.scene && st.scene.origin);
    sec.innerHTML = shellHtml(hasPlace);
    if (sec.addEventListener) {
      sec.addEventListener('click', onClick);
      sec.addEventListener('input', onInput);
      sec.addEventListener('change', onChange);
      sec.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' && ev.target && ev.target.id === 'h3dTourUrl') { var b = $('[data-act="touradd"]'); if (b && b.click) b.click(); }
        else if ((ev.key === 'Enter' || ev.key === ' ') && ev.target && ev.target.id === 'h3dCompass') { ev.preventDefault(); if (st.viewer) st.viewer.faceNorth(); }
      });
    }
    renderTour();
    if (!hasPlace) {
      setHtml('#h3dSrc', T('This report has no coordinates for the home, so a 3D model cannot be built.', '本报告缺少房屋坐标，无法生成三维模型。', 'Este informe no tiene coordenadas de la casa, por lo que no se puede generar un modelo 3D.'));
      return;
    }
    st.lat = st.scene.origin.lat; st.lon = st.scene.origin.lon;
    st.tz = H3.usTimeZone(st.rep && st.rep.geo && st.rep.geo.state, st.lat, st.lon);
    renderBadge(false);
    renderTiles();
    syncLotChip();
    fillDateOptions();
    showCover('idle');
    // Ask for the measured scene once the page is idle (cached server-side, so usually instant;
    // otherwise this starts the survey download well before the 3D section is opened).
    var whenIdle = typeof window.requestIdleCallback === 'function'
      ? function (fn) { window.requestIdleCallback(fn, { timeout: 2500 }); }
      : function (fn) { setTimeout(fn, 1200); };
    whenIdle(function () { requestServerScene(); });
    var stage = $('#h3dStage');
    if (typeof IntersectionObserver !== 'undefined' && stage) {
      var io = new IntersectionObserver(function (ents) {
        if (ents.some(function (e) { return e.isIntersecting; })) { io.disconnect(); start(); }
      }, { rootMargin: '350px' });
      io.observe(stage);
    }
    document.addEventListener && document.addEventListener('fullscreenchange', function () { if (st.viewer) { st.viewer.resize(); st.viewer.render(); } });
    if (window.addEventListener) {
      window.addEventListener('beforeprint', function () {
        var img = $('#h3dPrint'), url = st.viewer && st.viewer.snapshot();
        if (img && url) { img.src = url; if (img.classList) img.classList.add('ready'); }
      });
    }
    if (typeof MutationObserver !== 'undefined' && document.documentElement && document.documentElement.nodeType === 1) {
      new MutationObserver(onLang).observe(document.documentElement, { attributes: true, attributeFilter: ['lang'] });
    }
  }
  H3.mount = mount;
  H3._state = st;
  try {
    if (document.readyState === 'loading' && document.addEventListener) document.addEventListener('DOMContentLoaded', mount);
    else mount();
  } catch (e) {
    if (window.console && console.warn) console.warn('house3d: mount failed', e);
  }
})();
