/* Open Estate Property Video — a ~50-second cinematic walkthrough of the report, drawn on <canvas>
 * (played in the page, exportable as .webm).
 *
 * Story: fly in over real aerial imagery -> trace the lot, the main house and any garage / ADU on
 * it -> value -> monthly cost -> risks & neighborhood -> verdict and what to verify before an
 * offer. The imagery is Esri World Imagery requested in Web Mercator (EPSG:3857) with a known
 * extent, so every outline, marker and label lands on its true ground position, in the same
 * colours as the report's satellite map (lot sky-blue dashed, house yellow, garage / ADU magenta).
 * Headlines and captions follow the report's walkthrough deck. Numbers are the report's own;
 * anything the report does not have is left out, never invented.
 *
 * Self-contained IIFE + runtime <style id="dbMotionStyles">, Node DOM-mock safe, trilingual
 * (EN / 中文 / ES). Public API: window.OpenEstateMotion.
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_MOTION_INIT__) return;
  window.__OPEN_ESTATE_MOTION_INIT__ = true;

  function brandInfo() {
    var r = (typeof R !== 'undefined' && R) || (typeof window !== 'undefined' && window.__OPEN_ESTATE_REPORT__) || null;
    var wb = (typeof window !== 'undefined' && window.__OPEN_ESTATE_BRAND__) || {};
    var rb = (r && r.brand) || {};
    var name = (wb.name && String(wb.name).trim()) || (rb.name && String(rb.name).trim()) || 'Open Estate';
    var domain = (wb.domain && String(wb.domain).trim()) || (rb.domain && String(rb.domain).trim()) || 'open-estate.local';
    var parts = name.replace(/([a-z])([A-Z])/g, '$1 $2').split(/[\s_-]+/).filter(Boolean);
    var monogram = parts.length >= 2
      ? (parts[0][0] + parts[1][0]).toUpperCase()
      : name.slice(0, 2).toUpperCase();
    return { name: name, domain: domain, upper: name.toUpperCase(), monogram: monogram };
  }

  /* ------------------------------------------------------------------ Scene plan */
  // Pacing follows the content: the fly-in is short, the lot and the risk overview get the most time.
  var SCENES = [
    { key: 'arrival', dur: 7 },
    { key: 'lot', dur: 9 },
    { key: 'value', dur: 8 },
    { key: 'cost', dur: 8 },
    { key: 'risk', dur: 10 },
    { key: 'verdict', dur: 8 }
  ];
  var SCENE_COUNT = SCENES.length;
  var SCENE_START = [];
  var TOTAL_DURATION = 0;
  for (var si0 = 0; si0 < SCENE_COUNT; si0++) {
    SCENE_START.push(TOTAL_DURATION);
    TOTAL_DURATION += SCENES[si0].dur;
  }
  /** Seconds over which each scene dissolves in over the previous one. */
  var TRANS = 0.6;

  function sceneAt(t) {
    var w = ((t % TOTAL_DURATION) + TOTAL_DURATION) % TOTAL_DURATION;
    for (var i = SCENE_COUNT - 1; i >= 0; i--) {
      if (w >= SCENE_START[i]) return { idx: i, st: w - SCENE_START[i], wrapped: w };
    }
    return { idx: 0, st: w, wrapped: w };
  }

  var PAL = {
    ink: '#f8fafc',
    soft: '#e2e8f0',
    muted: '#cbd5e1',
    dim: '#94a3b8',
    teal: '#14b8a6',
    tealBright: '#2dd4bf',
    sky: '#38bdf8',
    blue: '#3b82f6',
    amber: '#f59e0b',
    yellow: '#facc15',
    magenta: '#e879f9',
    purple: '#a855f7',
    green: '#22c55e',
    red: '#ef4444',
    slate: '#64748b',
    night: '#040914'
  };
  /** Same legend as the report's satellite map (`addPropertyOutlines`). */
  var OUTLINE = { lot: '#38bdf8', house: '#facc15', acc: '#e879f9' };
  var LEVEL_COL = { good: '#22c55e', watch: '#f59e0b', bad: '#ef4444', info: '#94a3b8' };
  /**
   * The two looks of the video. The active one (`TH`) follows the viewer's system setting
   * (`prefers-color-scheme`) and switches live; text colors in `PAL` are swapped with it.
   */
  var THEMES = {
    dark: {
      mode: 'dark', ink: '#f8fafc', soft: '#e2e8f0', muted: '#cbd5e1', dim: '#94a3b8', night: '#040914',
      wash: '3, 8, 20', washK: 1, vignette: '0, 0, 0', vignetteK: 1, hudA: 0.55,
      glass: 'rgba(7, 14, 28, 0.74)', glassStrong: 'rgba(6, 12, 24, 0.84)', glassTile: 'rgba(7, 14, 28, 0.82)',
      glassEdge: 'rgba(255, 255, 255, 0.12)', glassShadow: 'rgba(0, 0, 0, 0.45)', textShadow: 'rgba(0, 0, 0, 0.6)',
      chipBg: 'rgba(15, 23, 42, 0.62)', chipEdge: 'rgba(255, 255, 255, 0.28)',
      accent: '#5eead4', accentSoft: 'rgba(20, 184, 166, 0.28)', accentEdge: 'rgba(45, 212, 191, 0.8)', accentInk: '#ccfbf1',
      amberInk: '#fcd34d', purpleInk: '#e9d5ff', greenInk: '#bbf7d0', goodInk: '#86efac',
      track: 'rgba(255, 255, 255, 0.08)', trackEdge: 'rgba(255, 255, 255, 0.12)', axis: 'rgba(255, 255, 255, 0.28)',
      leader: 'rgba(255, 255, 255, 0.9)', tick: 'rgba(255, 255, 255, 0.55)', ringTrack: 'rgba(255, 255, 255, 0.14)',
      caption: 'rgba(2, 6, 23, 0.74)', captionEdge: 'rgba(255, 255, 255, 0.10)',
      hudPill: 'rgba(6, 12, 24, 0.66)', hudPillEdge: 'rgba(45, 212, 191, 0.55)', hudPillInk: '#99f6e4',
      watchBg: 'rgba(120, 53, 15, 0.72)', watchEdge: 'rgba(251, 191, 36, 0.75)', watchInk: '#fef3c7',
      checkFill: 'rgba(251, 191, 36, 0.2)', checkEdge: '#fbbf24', checkInk: '#fde68a',
      end: 'rgb(4, 9, 20)', endGlow: 'rgba(20, 184, 166, 0.16)', loading: 'rgba(4, 9, 20, 0.55)',
      progressTrack: 'rgba(148, 163, 184, 0.2)', backdrop: ['#06101f', '#0b1a30', '#050d1a'], grid: 'rgba(148, 163, 184, 0.06)',
      attribution: 'rgba(226, 232, 240, 0.6)'
    },
    light: {
      mode: 'light', ink: '#0f172a', soft: '#1e293b', muted: '#475569', dim: '#64748b', night: '#e2e8f0',
      // Light scrims wash the imagery toward white instead of darkening it.
      wash: '248, 250, 252', washK: 0.9, vignette: '15, 23, 42', vignetteK: 0.22, hudA: 0.78,
      glass: 'rgba(255, 255, 255, 0.86)', glassStrong: 'rgba(255, 255, 255, 0.93)', glassTile: 'rgba(255, 255, 255, 0.9)',
      glassEdge: 'rgba(15, 23, 42, 0.10)', glassShadow: 'rgba(15, 23, 42, 0.18)', textShadow: 'rgba(255, 255, 255, 0.9)',
      chipBg: 'rgba(255, 255, 255, 0.88)', chipEdge: 'rgba(15, 23, 42, 0.14)',
      accent: '#0f766e', accentSoft: 'rgba(20, 184, 166, 0.14)', accentEdge: 'rgba(13, 148, 136, 0.65)', accentInk: '#115e59',
      amberInk: '#b45309', purpleInk: '#7e22ce', greenInk: '#15803d', goodInk: '#15803d',
      track: 'rgba(15, 23, 42, 0.06)', trackEdge: 'rgba(15, 23, 42, 0.12)', axis: 'rgba(15, 23, 42, 0.25)',
      leader: 'rgba(15, 23, 42, 0.65)', tick: 'rgba(15, 23, 42, 0.4)', ringTrack: 'rgba(15, 23, 42, 0.10)',
      caption: 'rgba(255, 255, 255, 0.93)', captionEdge: 'rgba(15, 23, 42, 0.10)',
      hudPill: 'rgba(255, 255, 255, 0.88)', hudPillEdge: 'rgba(13, 148, 136, 0.5)', hudPillInk: '#0f766e',
      watchBg: 'rgba(255, 251, 235, 0.96)', watchEdge: 'rgba(217, 119, 6, 0.55)', watchInk: '#92400e',
      checkFill: 'rgba(245, 158, 11, 0.14)', checkEdge: '#d97706', checkInk: '#92400e',
      end: 'rgb(248, 250, 252)', endGlow: 'rgba(20, 184, 166, 0.14)', loading: 'rgba(248, 250, 252, 0.62)',
      progressTrack: 'rgba(15, 23, 42, 0.12)', backdrop: ['#f1f5f9', '#e2e8f0', '#f8fafc'], grid: 'rgba(15, 23, 42, 0.05)',
      attribution: 'rgba(15, 23, 42, 0.55)'
    }
  };
  var Core = window.OpenEstateCore || null;
  var TH = THEMES.light;
  /** 'light' or 'dark' from the viewer's system setting (light when it cannot be read). */
  function systemTheme() {
    if (Core && typeof Core.systemColorScheme === 'function') return Core.systemColorScheme();
    try {
      return typeof window.matchMedia === 'function' && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    } catch (e) { return 'light'; }
  }
  function applyTheme(mode) {
    TH = THEMES[mode] || THEMES.light;
    PAL.ink = TH.ink; PAL.soft = TH.soft; PAL.muted = TH.muted; PAL.dim = TH.dim; PAL.night = TH.night;
    return TH.mode;
  }
  function wash(a) { return 'rgba(' + TH.wash + ', ' + clamp(a * TH.washK, 0, 1).toFixed(3) + ')'; }
  /** A palette color as text: unchanged on dark, a deeper shade of it on light backgrounds. */
  var DEEP = { '#f59e0b': '#b45309', '#a855f7': '#7e22ce', '#38bdf8': '#0369a1', '#22c55e': '#15803d', '#ef4444': '#b91c1c',
    '#14b8a6': '#0f766e', '#3b82f6': '#1d4ed8', '#e879f9': '#a21caf', '#facc15': '#a16207', '#16a34a': '#166534', '#94a3b8': '#475569' };
  function inkFor(c) { return TH.mode === 'light' && DEEP[c] ? DEEP[c] : c; }
  applyTheme(systemTheme());
  var FONT_STACK = 'Inter, "PingFang SC", "Microsoft YaHei", "Noto Sans SC", system-ui, sans-serif';

  /* ------------------------------------------------------------------ Player state */
  var state = {
    open: false,
    playing: false,
    t: 0,
    speed: 1,
    aspect: (typeof window.innerWidth === 'number' && window.innerWidth <= 640) ? '9:16' : '16:9',
    audioOn: true,
    langOverride: null,
    lastSceneIdx: -1,
    lastRafMs: 0,
    rafId: 0,
    zeroDtStreak: 0,
    recording: false,
    recorder: null,
    recChunks: [],
    recStartedAt: 0,
    data: null,
    dpr: 1,
    /** 'light' / 'dark' to pin the look (tests, previews); null follows the system setting. */
    themeOverride: null,
    waitingSince: 0
  };

  /* ------------------------------------------------------------------ Core & math helpers */
  function byId(id) {
    return document && typeof document.getElementById === 'function' ? document.getElementById(id) : null;
  }
  function qAll(sel, root) {
    var ctx = root || document;
    if (!ctx || typeof ctx.querySelectorAll !== 'function') return [];
    try { return Array.prototype.slice.call(ctx.querySelectorAll(sel) || []); } catch (e) { return []; }
  }
  function isN(x) { return typeof x === 'number' && isFinite(x); }
  function pickNum() {
    for (var i = 0; i < arguments.length; i++) {
      if (isN(arguments[i])) return arguments[i];
    }
    return null;
  }
  function clamp(v, lo, hi) { return v < lo ? lo : v > hi ? hi : v; }
  function lerp(a, b, t) { return a + (b - a) * t; }
  function remap(t, in0, in1) {
    if (in1 <= in0) return t >= in1 ? 1 : 0;
    return clamp((t - in0) / (in1 - in0), 0, 1);
  }
  function easeOutCubic(t) { var x = clamp(t, 0, 1); return 1 - Math.pow(1 - x, 3); }
  function easeOutExpo(t) { var x = clamp(t, 0, 1); return x >= 1 ? 1 : 1 - Math.pow(2, -10 * x); }
  function easeInOutCubic(t) {
    var x = clamp(t, 0, 1);
    return x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2;
  }
  function easeOutBack(t) {
    var x = clamp(t, 0, 1), c1 = 1.35, c3 = c1 + 1;
    return 1 + c3 * Math.pow(x - 1, 3) + c1 * Math.pow(x - 1, 2);
  }
  /** Eased 0 -> 1 entrance that starts at `t0` and lasts `d` seconds. */
  function enter(st, t0, d) { return easeOutCubic(remap(st, t0, t0 + (d || 0.6))); }

  /* ------------------------------------------------------------------ Language & formatting */
  function normLang(raw) {
    var s = String(raw || 'en').toLowerCase();
    return s.indexOf('zh') === 0 ? 'zh' : s.indexOf('es') === 0 ? 'es' : 'en';
  }
  function langCode() {
    if (state.langOverride) return state.langOverride;
    if (Core && typeof Core.langCode === 'function') {
      var cl = Core.langCode();
      if (cl !== 'en') return cl;
    }
    try { if (typeof LANG === 'string' && LANG && normLang(LANG) !== 'en') return normLang(LANG); } catch (e) {}
    if (document.documentElement && document.documentElement.lang) {
      var dl = normLang(document.documentElement.lang);
      if (dl !== 'en') return dl;
    }
    if (Core && typeof Core.langCode === 'function') return Core.langCode();
    try { if (typeof LANG === 'string' && LANG) return normLang(LANG); } catch (e) {}
    return normLang(document.documentElement && document.documentElement.lang);
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
  function numLoc() { var l = langCode(); return l === 'zh' ? 'zh-CN' : l === 'es' ? 'es-US' : 'en-US'; }
  function fUsd(v) {
    if (!isN(v)) return '—';
    try { return new Intl.NumberFormat(numLoc(), { style: 'currency', currency: 'USD', maximumFractionDigits: 0 }).format(Math.round(v)).replace(/US\$\s?/, '$'); }
    catch (e) { return '$' + Math.round(v).toLocaleString(); }
  }
  function fUsdK(v) {
    if (!isN(v)) return '—';
    var a = Math.abs(v);
    var neg = v < 0 ? '−$' : '$';
    if (a >= 1e6) return neg + (a / 1e6).toFixed(2).replace(/\.?0+$/, '') + 'M';
    if (a >= 1e3) return neg + Math.round(a / 1e3) + 'K';
    return fUsd(v);
  }
  function fNum(v, d) {
    if (!isN(v)) return '—';
    try { return new Intl.NumberFormat(numLoc(), { maximumFractionDigits: d || 0, minimumFractionDigits: d || 0 }).format(v); }
    catch (e) { return String(Math.round(v)); }
  }
  function sqftWord() { return L3('sq ft', '平方英尺', 'pies²'); }
  function fmtClock(sec) {
    var s = Math.max(0, Math.floor(sec));
    return Math.floor(s / 60) + ':' + (s % 60 < 10 ? '0' : '') + (s % 60);
  }

  /* ------------------------------------------------------------------ Report access & text */
  function getRep() {
    try { if (typeof R !== 'undefined' && R && typeof R === 'object') return R; } catch (e) {}
    try {
      var el = byId('report-data');
      return el && el.textContent ? JSON.parse(el.textContent) : null;
    } catch (e2) { return null; }
  }
  function mod(rep, k) {
    var m = rep && rep.modules && rep.modules[k];
    return (m && m.data) || (rep && rep[k]) || null;
  }
  function clean(s) { return String(s == null ? '' : s).replace(/\s+/g, ' ').trim(); }
  /** At most `max` characters, cut at a word (or CJK character) boundary. */
  function shorten(s, max) {
    var t = clean(s);
    if (t.length <= max) return t;
    var cut = t.slice(0, Math.max(1, max - 1));
    var sp = cut.lastIndexOf(' ');
    if (sp > max * 0.55) cut = cut.slice(0, sp);
    return cut.replace(/[\s,;:·—–(-]+$/, '') + '…';
  }
  function lowerFirst(s) {
    var t = clean(s);
    return /^[A-ZÁÉÍÓÚÑ][a-záéíóúñ]/.test(t) ? t.charAt(0).toLowerCase() + t.slice(1) : t;
  }
  function stripEnd(s) { return clean(s).replace(/[.。;；!！]+$/, ''); }
  function isCjk(s) { return /[\u3400-\u9fff]/.test(String(s || '')); }
  /**
   * A watch-out short enough for a chip or a caption, never cut mid-thought: the whole text when
   * it fits, else its first sentence / clause, else the detail after a "Heading:", else the longest
   * lead-in that ends at a comma or an "and". Only when none of those fit is it cut with "…".
   */
  function brief(s) {
    // Limits follow the script of the text itself (an untranslated English note in the 中文 cut
    // keeps the English limits).
    var zh = isCjk(s);
    var max = zh ? 32 : 90, min = zh ? 8 : 24;
    var t = stripEnd(s);
    if (t.length <= max) return t;
    var fits = function (x) { x = stripEnd(x); return x.length >= min && x.length <= max ? x : ''; };
    var c = fits(t.split(zh ? /[；;。！]/ : /;\s|[.!]\s/)[0]);
    if (c) return c;
    var m = t.match(zh ? /^[^：:]{2,14}[：:](.+)$/ : /^[^:]{3,40}:\s(.+)$/);
    if (m) {
      t = stripEnd(m[1]);
      if (!zh) t = t.charAt(0).toUpperCase() + t.slice(1);
      if (t.length <= max) return t;
    }
    var re = zh ? /[，、,]|并|且/g : /,\s|\s(?:and|but|or|y|e|pero|o)\s/g;
    var best = '', mm;
    while ((mm = re.exec(t))) {
      if (mm.index > max) break;
      if (mm.index >= min) best = t.slice(0, mm.index);
    }
    return best ? stripEnd(best) + '…' : shorten(t, max);
  }

  /* ------------------------------------------------------------------ Data (report -> video facts) */
  function walkthroughSlides(rep) {
    var ws = (rep && rep.walkthroughSlides) || mod(rep, 'walkthroughSlides') || null;
    return ws && Array.isArray(ws.slides) ? ws.slides : [];
  }
  function slideOf(slides, kind) {
    for (var i = 0; i < slides.length; i++) if (slides[i] && slides[i].visualKind === kind) return slides[i];
    return null;
  }
  /** Localized slide text. `strict`: in 中文 / ES return '' rather than fall back to English. */
  function slideField(sl, key, strict) {
    if (!sl) return '';
    var l = langCode();
    var ov = l === 'zh' ? sl.zh : l === 'es' ? sl.es : null;
    var v = ov && ov[key];
    if (typeof v === 'string' && clean(v)) return clean(v);
    if (strict && l !== 'en') return '';
    return typeof sl[key] === 'string' ? clean(sl[key]) : '';
  }
  function ringOk(r) {
    if (!Array.isArray(r) || r.length < 4) return null;
    var out = [];
    for (var i = 0; i < r.length; i++) {
      var p = r[i];
      if (!Array.isArray(p) || !isN(+p[0]) || !isN(+p[1])) return null;
      out.push([+p[0], +p[1]]);
    }
    return out;
  }
  function ringCentroidLL(r) {
    var n = r.length;
    if (n > 1 && r[0][0] === r[n - 1][0] && r[0][1] === r[n - 1][1]) n -= 1;
    var la = 0, lo = 0;
    for (var i = 0; i < n; i++) { la += r[i][0]; lo += r[i][1]; }
    return [la / Math.max(1, n), lo / Math.max(1, n)];
  }
  /** Ground offset (east, north) in metres of (lat, lon) from (lat0, lon0). */
  function groundOffsetM(lat0, lon0, lat, lon) {
    return [(lon - lon0) * 111320 * Math.cos(lat0 * Math.PI / 180), (lat - lat0) * 110574];
  }
  function shortType(s) {
    var t = clean(String(s || '').replace(/\([^)]*\)/g, '')).replace(/\s+\+\s+/g, ' + ');
    var l = langCode();
    if (l !== 'en') {
      var words = l === 'zh'
        ? [[/single[- ]family/i, '独栋住宅'], [/condo(minium)?/i, '公寓'], [/town ?house/i, '联排别墅'], [/multi[- ]family/i, '多户住宅'], [/permitted adu/i, '合法 ADU'], [/mobile home|manufactured/i, '活动房屋']]
        : [[/single[- ]family/i, 'Unifamiliar'], [/condo(minium)?/i, 'Condominio'], [/town ?house/i, 'Casa adosada'], [/multi[- ]family/i, 'Multifamiliar'], [/permitted adu/i, 'ADU con permiso'], [/mobile home|manufactured/i, 'Casa prefabricada']];
      words.forEach(function (w) { t = t.replace(w[0], w[1]); });
    }
    return shorten(t, 34);
  }
  /** "High / Medium / Low" as the report states it, in the video's language. */
  function confidenceWord(c) {
    var k = clean(c).toLowerCase();
    var map = { high: ['High', '高', 'alta'], medium: ['Medium', '中', 'media'], moderate: ['Moderate', '中', 'moderada'], low: ['Low', '低', 'baja'] };
    return map[k] ? L3(map[k][0], map[k][1], map[k][2]) : clean(c);
  }
  function methodLabel(m) {
    var k = ((m.key || '') + ' ' + (m.label || '')).toLowerCase();
    if (/avm|zestimate|zillow|redfin|realtor|movoto|trulia|homes\.com|compass|opendoor|third[- ]party|第三方|terceros/.test(k) && !/zhvi/.test(k)) {
      return L3('Third-party estimate', '第三方估值', 'Estimación de terceros');
    }
    if (/roll|assess|prop\s?13/.test(k)) return L3('County roll, indexed', '县评估值（指数化）', 'Avalúo del condado, indexado');
    if (/repeat|last.?sale|resale/.test(k)) return L3('Last sale, indexed', '上次成交（指数化）', 'Última venta, indexada');
    if (/comp/.test(k)) return L3('Comparable sales', '可比成交', 'Ventas comparables');
    if (/zhvi|zip|邮编房价指数|código postal/.test(k)) return L3('ZIP home-value index', '邮编房价指数', 'Índice de valor del código postal');
    return shorten(m.label || m.key || L3('Benchmark', '参考值', 'Referencia'), 28);
  }
  var HAZARD_WORDS = [
    [/dam.?failure|dam inundation/i, ['Dam-failure inundation area', '位于溃坝淹没区', 'Zona de inundación por presa']],
    [/liquefaction/i, ['Liquefaction zone', '地震液化区', 'Zona de licuefacción']],
    [/landslide/i, ['Landslide zone', '滑坡区', 'Zona de deslizamientos']],
    [/alquist|fault zone/i, ['Alquist-Priolo fault zone', '活动断层带', 'Zona de falla Alquist-Priolo']],
    [/tsunami/i, ['Tsunami zone', '海啸区', 'Zona de tsunami']],
    [/sea.?level/i, ['Sea-level rise exposure', '海平面上升影响区', 'Exposición al aumento del mar']],
    [/radon/i, ['Elevated radon potential', '氡气偏高', 'Radón elevado']]
  ];

  function extractReelData(rep) {
    rep = rep || getRep() || {};
    var l = langCode();
    var meta = rep.meta || {};
    var g = rep.geo || {};
    var f = rep.facts || {};
    var br = f.buildingRegister || {};
    var v = mod(rep, 'valuation') || {};
    var est = v.estimate || {};
    var own = v.ownership || {};
    var m0 = own.monthly || {};
    var hz = mod(rep, 'hazards') || {};
    var env = mod(rep, 'environment') || {};
    var civ = mod(rep, 'civic') || {};
    var am = mod(rep, 'amenities') || {};
    var fs = mod(rep, 'fengshui') || {};
    var h = fs.house || {};
    var lotAdu = (rep.decisionFactors && rep.decisionFactors.lotAndAdu) || {};
    var summ = rep.summary || {};
    var scores = summ.scores || {};
    var slides = walkthroughSlides(rep);
    var trMap = rep.translations || {};
    var tr = l === 'zh' ? ((trMap.zh && trMap.zh.data) || (trMap['zh-CN'] && trMap['zh-CN'].data) || null)
      : l === 'es' ? ((trMap.es && trMap.es.data) || (trMap['es-ES'] && trMap['es-ES'].data) || (trMap['es-419'] && trMap['es-419'].data) || null)
        : null;

    var fullAddr = clean(meta.displayAddress || meta.inputAddress || '') || L3('Subject property', '目标房产', 'Propiedad');
    var addrParts = fullAddr.split(',');
    var street = clean(addrParts[0] || fullAddr);
    var cityLine = clean(addrParts.slice(1).join(',')) || [g.city, g.state, g.zip].filter(Boolean).join(', ');
    var slug = String(meta.slug || '').replace(/[^a-z0-9-]/gi, '');

    // Geometry: the same selection the report's maps make (`getPropertyGeometries`).
    var lotRing = ('lotPolygon' in h) ? ringOk(h.lotPolygon) : (f.parcelMatch === 'nearest' ? null : ringOk(f.parcelPolygon || br.parcelPolygon));
    var offLot = !!lotRing && h.onLot === false;
    var houseRing = offLot ? null : ringOk(h.footprint);
    var acc = [];
    if (!offLot && Array.isArray(h.additionalStructures)) {
      h.additionalStructures.forEach(function (s) {
        var r = s && ringOk(s.footprint);
        if (r && acc.length < 2) {
          acc.push({ ring: r, label: L3(s.label || 'Garage / ADU', s.labelZh || '车库 / 附属建筑', s.labelEs || 'Garaje / ADU'), areaSqft: isN(s.areaSqft) ? s.areaSqft : null });
        }
      });
    }
    var lotC = lotRing ? ringCentroidLL(lotRing) : null;
    var lat = pickNum(h.lat, g.refined && g.refined.lat, g.lat);
    var lon = pickNum(h.lon, g.refined && g.refined.lon, g.lon);
    if (offLot && lotC) { lat = lotC[0]; lon = lotC[1]; }
    var hasPoint = isN(lat) && isN(lon) && Math.abs(lat) <= 85 && Math.abs(lon) <= 180 && !(lat === 0 && lon === 0);
    var focus = lotC || (hasPoint ? [lat, lon] : null);
    var extentM = 0;
    if (focus) {
      [lotRing, houseRing].concat(acc.map(function (a) { return a.ring; })).forEach(function (r) {
        (r || []).forEach(function (p) {
          var o = groundOffsetM(focus[0], focus[1], p[0], p[1]);
          extentM = Math.max(extentM, Math.abs(o[0]), Math.abs(o[1]));
        });
      });
    }
    var ori = fs.orientation || {};

    var facts = {
      beds: pickNum(f.beds, br.beds),
      baths: pickNum(f.baths, br.baths),
      sqft: pickNum(f.sqft, br.livingSqft),
      yearBuilt: pickNum(f.yearBuilt, br.yearBuilt),
      lotSqft: pickNum(lotAdu.lotSqft, f.lotSqft, br.lotSqft),
      type: shortType(f.propertyType || br.propertyType || '')
    };
    var areas = {
      lotRecord: facts.lotSqft,
      lotOutline: pickNum(h.lotAreaSqft, f.lotGisSqft),
      house: pickNum(h.areaSqft, lotAdu.footprintSqft, br.footprintSqft),
      coveragePct: pickNum(lotAdu.lotCoveragePct, br.lotCoveragePct),
      openLot: pickNum(lotAdu.openLotSqft)
    };

    // Value: the estimate and the report's own method values on one scale.
    var value = null;
    if (isN(est.value) && est.value > 0) {
      var refs = [];
      var methods = Array.isArray(v.methods) ? v.methods : (Array.isArray(est.methods) ? est.methods : []);
      methods.forEach(function (m) {
        if (!m || !isN(m.value) || m.value <= 0 || refs.length >= 3) return;
        var lbl = methodLabel(m);
        if (refs.some(function (r) { return r.label === lbl || Math.abs(r.value - m.value) < 1; })) return;
        refs.push({ label: lbl, value: m.value, color: [PAL.amber, PAL.purple, PAL.sky][refs.length] });
      });
      var ls = f.lastSale;
      if (ls && isN(ls.price) && ls.price > 0 && refs.length < 4) {
        var lsYear = String(ls.date || '').slice(0, 4);
        refs.push({ label: L3('Last sale', '上次成交', 'Última venta') + (lsYear ? ' ' + lsYear : ''), value: ls.price, color: PAL.sky });
      }
      value = {
        est: est.value,
        low: isN(est.low) && est.low > 0 ? est.low : null,
        high: isN(est.high) && est.high > 0 ? est.high : null,
        perSqft: pickNum(est.perSqft, est.pricePerSqft),
        confidence: clean(est.confidence || ''),
        refs: refs
      };
    }

    // Monthly cost: every bar segment comes from the same ownership model as its total.
    var cost = null;
    var parts = [];
    var addPart = function (key, label, val, color) { if (isN(val) && val > 0) parts.push({ key: key, label: label, value: val, color: color }); };
    addPart('pi', L3('Mortgage P&I', '房贷本息', 'Hipoteca'), m0.principalInterest, PAL.teal);
    addPart('tax', L3('Property tax', '房产税', 'Impuesto predial'), m0.propertyTax, PAL.blue);
    addPart('ins', L3('Insurance', '房屋保险', 'Seguro'), m0.insurance, PAL.amber);
    addPart('hoa', 'HOA', m0.hoa, PAL.purple);
    addPart('maint', L3('Maintenance', '维护预留', 'Mantenimiento'), m0.maintenance, '#94a3b8');
    if (parts.length) {
      var sumParts = parts.reduce(function (s, p) { return s + p.value; }, 0);
      var total = isN(m0.total) && m0.total > 0 ? m0.total : sumParts;
      if (total - sumParts > Math.max(25, total * 0.015)) addPart('other', L3('Other costs', '其他费用', 'Otros costos'), total - sumParts, '#475569');
      var rent = isN(own.rentEstimate) && own.rentEstimate > 0 ? own.rentEstimate : null;
      cost = {
        total: total,
        parts: parts,
        quake: isN(m0.earthquakeInsurance) && m0.earthquakeInsurance > 0 ? m0.earthquakeInsurance : null,
        rent: rent,
        ownMinusRent: isN(own.ownMinusRent) ? own.ownMinusRent : (rent ? total - rent : null),
        rate30: isN(own.rate30) ? own.rate30 : null,
        downPct: isN(own.downPaymentPct) ? own.downPaymentPct : null,
        taxRatePct: isN(own.taxRatePct) ? own.taxRatePct : null
      };
    }

    // Risks: hazards first, then everyday-life signals.
    var hazards = [];
    var fl = hz.flood || {};
    var zone = clean(fl.zone || fl.femaZone || '');
    if (zone || typeof fl.sfha === 'boolean') {
      var nsf = fl.nearestSfha || null;
      var nearSfha = !fl.sfha && nsf && isN(nsf.distMi) && nsf.distMi <= 1 ? nsf : null;
      hazards.push({
        key: 'flood', icon: 'flood', title: L3('Flood', '洪水', 'Inundación'),
        level: fl.sfha ? 'bad' : (nearSfha && nearSfha.distMi <= 0.3 ? 'watch' : 'good'),
        fact: [zone ? L3('FEMA Zone ' + zone, 'FEMA ' + zone + ' 区', 'Zona FEMA ' + zone) : '',
          fl.sfha ? L3('high-risk floodplain', '高风险洪泛区', 'zona de alto riesgo') : L3('minimal risk', '低风险', 'riesgo mínimo'),
          nearSfha ? L3('nearest Zone ' + (nearSfha.zone || 'A') + ' ' + fNum(nearSfha.distMi, 2) + ' mi', '最近 ' + (nearSfha.zone || 'A') + ' 区 ' + fNum(nearSfha.distMi, 2) + ' mi', 'Zona ' + (nearSfha.zone || 'A') + ' más cercana a ' + fNum(nearSfha.distMi, 2) + ' mi') : ''
        ].filter(Boolean).join(' · '),
        raw: { sfha: !!fl.sfha, nearMi: nearSfha ? nearSfha.distMi : null }
      });
    }
    var eq = hz.earthquake || {};
    var dm = eq.designMaps || {};
    var pga = pickNum(dm.pgaM, dm.pga, eq.pga);
    var sdc = clean(dm.sdc || eq.sdc || '');
    var fault = (eq.faults && Array.isArray(eq.faults.nearest) && eq.faults.nearest[0]) || (Array.isArray(eq.nearestFaults) && eq.nearestFaults[0]) || null;
    if (isN(pga) || sdc || (fault && fault.name)) {
      var eqBits = [];
      if (isN(pga)) eqBits.push(L3('shaking ', '设计地震动 ', 'sacudida ') + fNum(pga, 2) + ' g');
      if (sdc) eqBits.push('SDC ' + sdc);
      if (fault && fault.name && isN(fault.distMi)) eqBits.push(shorten(fault.name, 22) + ' ' + fNum(fault.distMi, 2) + ' mi');
      hazards.push({
        key: 'quake', icon: 'quake', title: L3('Earthquake', '地震', 'Sismo'), fact: eqBits.join(' · '),
        raw: { pga: isN(pga) ? pga : null, faultName: fault && fault.name ? clean(fault.name) : '', faultMi: fault && isN(fault.distMi) ? fault.distMi : null },
        level: (isN(pga) && pga >= 0.5) || /^[DEF]/i.test(sdc) ? 'bad' : (isN(pga) && pga >= 0.2) ? 'watch' : 'good'
      });
    }
    var wf = hz.wildfire || {};
    var wca = wf.california || {};
    if (wca.fhsz || typeof wca.inFhsz === 'boolean') {
      var inZone = wca.inFhsz === true;
      var nearFire = wca.nearestHighOrVeryHigh;
      hazards.push({
        key: 'fire', icon: 'fire', title: L3('Wildfire', '山火', 'Incendio'),
        level: inZone ? ((wca.fhszClass || 0) >= 3 ? 'bad' : 'watch') : 'good',
        raw: { inZone: inZone, zone: clean(wca.fhsz || '') },
        fact: inZone ? shorten(wca.fhsz, 40) : L3('Outside mapped fire zones', '不在山火危险区内', 'Fuera de zonas de incendio') +
          (nearFire && isN(nearFire.distMi) ? ' · ' + L3('nearest high-hazard zone ' + fNum(nearFire.distMi, 1) + ' mi', '最近高危区 ' + fNum(nearFire.distMi, 1) + ' mi', 'zona de alto riesgo a ' + fNum(nearFire.distMi, 1) + ' mi') : '')
      });
    }
    (Array.isArray(hz.highlights) ? hz.highlights : []).some(function (x) {
      if (!x || x.level !== 'bad' || !x.text) return false;
      var t = String(x.text);
      if (/flood zone|zone x|shaking|fire hazard|wildfire/i.test(t)) return false;
      for (var i = 0; i < HAZARD_WORDS.length; i++) {
        if (HAZARD_WORDS[i][0].test(t)) {
          var w = HAZARD_WORDS[i][1];
          hazards.push({ key: 'other', icon: 'alert', title: L3('Also mapped', '另需注意', 'También'), fact: L3(w[0], w[1], w[2]), level: 'bad', raw: { words: w } });
          return true;
        }
      }
      if (l === 'en') { hazards.push({ key: 'other', icon: 'alert', title: 'Also mapped', fact: shorten(t, 64), level: 'bad' }); return true; }
      return false;
    });
    var living = [];
    var sc = civ.schools || {};
    var assigned = Array.isArray(sc.assignedOrNearest) ? sc.assignedOrNearest.filter(function (s) { return s && s.name; }) : [];
    var schoolScore = pickNum(sc.summaryScore);
    if (assigned.length || isN(schoolScore)) {
      living.push({
        key: 'schools', icon: 'school',
        title: L3('Schools', '学区', 'Escuelas') + (isN(schoolScore) ? ' · ' + fNum(schoolScore, 1) + '/10' : ''),
        fact: assigned.slice(0, 2).map(function (s) { return shorten(s.name, 24) + (isN(s.distanceMi) ? ' ' + fNum(s.distanceMi, 1) + ' mi' : ''); }).join(' · ') || clean(sc.summaryScoreSource || ''),
        level: isN(schoolScore) ? (schoolScore >= 7 ? 'good' : schoolScore >= 5 ? 'watch' : 'bad') : 'info'
      });
    }
    var walk = pickNum(civ.scores && civ.scores.walkability, f.walkScore && f.walkScore.walk);
    var grocery = am.nearest && am.nearest.grocery;
    if (isN(walk) || (grocery && grocery.name)) {
      var gDist = grocery ? pickNum(grocery.distMi, grocery.distanceMi) : null;
      living.push({
        key: 'walk', icon: 'walk',
        title: L3('Walkability', '步行便利', 'Caminabilidad') + (isN(walk) ? ' · ' + Math.round(walk) + '/100' : ''),
        fact: grocery && grocery.name ? shorten(grocery.name, 26) + (isN(gDist) ? ' ' + fNum(gDist, 2) + ' mi' : '') : '',
        level: isN(walk) ? (walk >= 70 ? 'good' : walk >= 50 ? 'watch' : 'bad') : 'info'
      });
    }
    var envS = env.scores || {};
    var envTile = isN(envS.overall) ? {
      key: 'env', icon: 'leaf', title: L3('Environment', '环境', 'Entorno') + ' · ' + Math.round(envS.overall) + '/100',
      fact: isN(envS.air) ? L3('Air quality ', '空气质量 ', 'Calidad del aire ') + Math.round(envS.air) + '/100' : '',
      level: envS.overall >= 70 ? 'good' : envS.overall >= 50 ? 'watch' : 'bad'
    } : null;
    var tiles = hazards.slice(0, 4).concat(living.slice(0, 2));
    if (envTile && tiles.length < 6) tiles.push(envTile);

    var pois = [];
    [['grocery', 'G', PAL.green, L3('Grocery', '超市', 'Súper')], ['park', 'P', '#16a34a', L3('Park', '公园', 'Parque')],
      ['transit', 'T', PAL.blue, L3('Transit', '公交', 'Transporte')], ['cafe', 'C', PAL.amber, L3('Café', '咖啡', 'Café')],
      ['health', '+', PAL.red, L3('Clinic', '医疗', 'Clínica')]].forEach(function (k) {
      var p = am.nearest && am.nearest[k[0]];
      var dist = p ? pickNum(p.distMi, p.distanceMi) : null;
      if (p && isN(p.lat) && isN(p.lon) && isN(dist) && dist <= 1.6) pois.push({ kind: k[0], lat: p.lat, lon: p.lon, letter: k[1], color: k[2], label: k[3], dist: dist, name: clean(p.name || '') });
    });

    // Verdict + what to verify (the deck's watch-outs, localized).
    var verdictText = l === 'en' ? clean(summ.verdict || '') : clean((tr && tr.verdict) || slideField(slideOf(slides, 'verdict'), 'title', true) || summ.verdict || '');
    var hasSubjectData = isN(facts.beds) || isN(facts.baths) || isN(facts.sqft) || isN(facts.yearBuilt) || isN(facts.lotSqft) ||
      isN(areas.house) || isN(areas.lotRecord) || isN(areas.lotOutline) || Boolean(value) || Boolean(cost) ||
      hazards.length > 0 || living.length > 0 || pois.length > 0 || Boolean(verdictText) ||
      Boolean(rep.ai && Array.isArray(rep.ai.verify) && rep.ai.verify.length);
    var wo = function (kind) { return hasSubjectData ? brief(slideField(slideOf(slides, kind), 'watchout', true)) : ''; };
    var watch = {
      lot: wo('property_lot') || wo('remodel_upside'),
      value: wo('zestimate_comps'),
      cost: wo('cost_insurance'),
      risk: wo('risk_climate'),
      verdict: wo('verdict')
    };
    // The checklist on screen has room for two lines per item; captions get the short form.
    var checksFull = [];
    var fullMax = function (t) { return isCjk(t) ? 60 : 140; };
    if (hasSubjectData) {
      ['property_lot', 'cost_insurance', 'risk_climate', 'action_plan', 'remodel_upside', 'zestimate_comps'].forEach(function (k) {
        var w = stripEnd(slideField(slideOf(slides, k), 'watchout', true));
        if (w && checksFull.length < 3 && checksFull.indexOf(shorten(w, fullMax(w))) < 0) checksFull.push(shorten(w, fullMax(w)));
      });
    }
    if (!checksFull.length) {
      var fallback = (tr && Array.isArray(tr.verify) && tr.verify) || (rep.ai && Array.isArray(rep.ai.verify) && rep.ai.verify) || (summ.findings && Array.isArray(summ.findings.watch) && summ.findings.watch) || [];
      fallback.slice(0, 3).forEach(function (x) { var t = stripEnd(typeof x === 'string' ? x : (x.text || '')); if (t) checksFull.push(shorten(t, fullMax(t))); });
    }
    checksFull = checksFull.filter(Boolean);
    var checks = checksFull.map(brief).filter(Boolean);

    var data = {
      slug: slug,
      street: street,
      cityLine: cityLine,
      fullAddr: fullAddr,
      county: clean(g.county || ''),
      lat: hasPoint ? lat : null,
      lon: hasPoint ? lon : null,
      hasPoint: hasPoint,
      k: hasPoint ? 1 / Math.max(0.2, Math.cos(lat * Math.PI / 180)) : 1,
      geo: {
        lot: lotRing, house: houseRing, acc: acc, lotC: lotC, focus: focus, extentM: extentM,
        facingDeg: isN(ori.facingTrue) ? ori.facingTrue : null, facingLabel: clean(ori.facingCompass || '')
      },
      facts: facts,
      areas: areas,
      value: value,
      cost: cost,
      tiles: tiles,
      pois: pois.sort(function (a, b) { return a.dist - b.dist; }).slice(0, 5),
      verdict: { text: verdictText, tone: summ.verdictTone || '', score: isN(scores.overall) ? Math.round(scores.overall) : null },
      watch: watch,
      checks: checks,
      checksFull: checksFull,
      headline: {
        home: slideField(slideOf(slides, 'property_lot'), 'title', true),
        lot: slideField(slideOf(slides, 'remodel_upside'), 'title', true),
        risk: slideField(slideOf(slides, 'risk_climate'), 'title', true)
      },
      sat: hasPoint ? resolveMotionSatellite(rep, lat, lon) : null,
      layers: []
    };
    data.layers = data.hasPoint ? aerialLayers(data.lat, data.lon, data.geo.focus, data.sat) : [];
    data.captions = buildCaptions(data, hazards, living);
    return data;
  }

  /** One burned-in caption (or two) per scene, from the same numbers on screen. */
  function buildCaptions(d, hazards, living) {
    var l = langCode();
    var F = d.facts, A = d.areas, V = d.value, C = d.cost;
    var out = [];
    var sp = l === 'zh' ? '' : ' ';
    var cased = function (t) { return l === 'zh' ? stripEnd(t) : lowerFirst(stripEnd(t)); };
    // 1 · The home
    var bits = [];
    if (isN(F.beds)) bits.push(L3(F.beds + '-bed', F.beds + ' 卧', F.beds + ' rec.'));
    if (isN(F.baths)) bits.push(L3(fNum(F.baths, F.baths % 1 ? 1 : 0) + '-bath', fNum(F.baths, F.baths % 1 ? 1 : 0) + ' 卫', fNum(F.baths, F.baths % 1 ? 1 : 0) + ' baños'));
    if (isN(F.sqft)) bits.push(L3(fNum(F.sqft) + ' sq ft', '室内 ' + fNum(F.sqft) + ' 平方英尺', fNum(F.sqft) + ' pies²'));
    if (!bits.length) out.push(d.street + (d.cityLine ? (l === 'zh' ? '，' : ', ') + d.cityLine : '') + (l === 'zh' ? '。' : '.'));
    else if (l === 'zh') out.push(d.street + '：' + bits.join('，') + (isN(F.yearBuilt) ? '，' + F.yearBuilt + ' 年建' : '') + (isN(F.lotSqft) ? '，占地 ' + fNum(F.lotSqft) + ' 平方英尺' : '') + '。');
    else if (l === 'es') out.push(d.street + ': casa de ' + bits.join(', ') + (isN(F.yearBuilt) ? ', construida en ' + F.yearBuilt : '') + (isN(F.lotSqft) ? ', en un lote de ' + fNum(F.lotSqft) + ' pies²' : '') + '.');
    else out.push(d.street + ': a ' + bits.join(', ') + ' home' + (isN(F.yearBuilt) ? ' built in ' + F.yearBuilt : '') + (isN(F.lotSqft) ? ' on a ' + fNum(F.lotSqft) + ' sq ft lot' : '') + '.');
    // 2 · The lot
    var lot = '';
    if (isN(A.coveragePct) && isN(A.openLot)) {
      lot = L3('The house covers about ' + fNum(A.coveragePct, 0) + '% of the lot, leaving roughly ' + fNum(A.openLot) + ' sq ft of open yard.',
        '主宅约占地块的 ' + fNum(A.coveragePct, 0) + '%，余约 ' + fNum(A.openLot) + ' 平方英尺空地。',
        'La casa ocupa cerca del ' + fNum(A.coveragePct, 0) + '% del lote y deja unos ' + fNum(A.openLot) + ' pies² de patio libre.');
    } else if (isN(A.house) && isN(A.lotRecord || A.lotOutline)) {
      lot = L3('About ' + fNum(A.house) + ' sq ft of roof on a ' + fNum(A.lotRecord || A.lotOutline) + ' sq ft lot.',
        '约 ' + fNum(A.house) + ' 平方英尺屋顶投影，地块 ' + fNum(A.lotRecord || A.lotOutline) + ' 平方英尺。',
        'Unos ' + fNum(A.house) + ' pies² de techo en un lote de ' + fNum(A.lotRecord || A.lotOutline) + ' pies².');
    }
    if (d.geo.acc.length) {
      var a0 = d.geo.acc[0];
      lot += (lot ? sp : '') + L3('A ' + (isN(a0.areaSqft) ? fNum(a0.areaSqft) + ' sq ft ' : '') + 'garage / ADU also sits on the lot.',
        '地块内另有' + (isN(a0.areaSqft) ? '约 ' + fNum(a0.areaSqft) + ' 平方英尺的' : '') + '车库 / 附属建筑。',
        'En el lote también hay un garaje / ADU' + (isN(a0.areaSqft) ? ' de ' + fNum(a0.areaSqft) + ' pies²' : '') + '.');
    }
    out.push(lot);
    // 3 · Value
    var val = '';
    if (V) {
      var bnV = brandInfo().name;
      var range = isN(V.low) && isN(V.high) ? fUsdK(V.low) + '–' + fUsdK(V.high) : '';
      var psf = isN(V.perSqft) && !V.refs.length;
      val = L3(bnV + ' estimates ' + fUsdK(V.est) + (range ? ' (range ' + range + ')' : '') + (psf ? ', about ' + fUsd(V.perSqft) + ' per sq ft' : '') + '.',
        bnV + ' 估值 ' + fUsdK(V.est) + (range ? '（区间 ' + range + '）' : '') + (psf ? '，约 ' + fUsd(V.perSqft) + '/平方英尺' : '') + '。',
        bnV + ' estima ' + fUsdK(V.est) + (range ? ' (rango ' + range + ')' : '') + (psf ? ', unos ' + fUsd(V.perSqft) + ' por pie²' : '') + '.');
      if (V.refs.length) {
        val += sp + V.refs.slice(0, 2).map(function (r, i) { return (i && l !== 'zh' ? lowerFirst(r.label) : r.label) + (l === 'zh' ? ' ' : ': ') + fUsdK(r.value); }).join(l === 'zh' ? '；' : '; ') + (l === 'zh' ? '。' : '.');
      }
    }
    out.push(val);
    // 4 · Cost
    var cst = '';
    if (C) {
      var terms = isN(C.downPct) && isN(C.rate30);
      var omr = C.ownMinusRent;
      if (l === 'zh') {
        cst = '持有成本约每月 ' + fUsd(C.total) + (terms ? '（首付 ' + fNum(C.downPct) + '%，利率 ' + fNum(C.rate30, 2) + '%）' : '') +
          (C.rent && isN(omr) ? (omr > 0 ? '，比租房（每月 ' + fUsd(C.rent) + '）多 ' + fUsd(omr) + '。' : '，低于租房（每月 ' + fUsd(C.rent) + '）。') : '。');
      } else if (l === 'es') {
        cst = 'Ser dueño cuesta unos ' + fUsd(C.total) + ' al mes' + (terms ? ' (' + fNum(C.downPct) + '% de enganche al ' + fNum(C.rate30, 2) + '%)' : '') +
          (C.rent && isN(omr) ? (omr > 0 ? ', ' + fUsd(omr) + ' más que rentar por ' + fUsd(C.rent) + '.' : ', menos que rentar por ' + fUsd(C.rent) + '.') : '.');
      } else {
        cst = 'Owning costs about ' + fUsd(C.total) + ' a month' + (terms ? ' with ' + fNum(C.downPct) + '% down at ' + fNum(C.rate30, 2) + '%' : '') +
          (C.rent && isN(omr) ? (omr > 0 ? ', ' + fUsd(omr) + ' more than renting at ' + fUsd(C.rent) + '.' : ', less than renting at ' + fUsd(C.rent) + '.') : '.');
      }
    }
    out.push(cst);
    // 5 · Risks & neighborhood
    var bad = hazards.filter(function (x) { return x.level === 'bad'; });
    var sentence = function (s) { return clean(s).replace(/\s·\s/g, l === 'zh' ? '，' : ', '); };
    var rk = '';
    if (bad.length) {
      rk = L3('Biggest risk: ' + lowerFirst(bad[0].title) + ' (' + sentence(bad[0].fact) + ').', '最大风险：' + bad[0].title + '（' + sentence(bad[0].fact) + '）。', 'Mayor riesgo: ' + lowerFirst(bad[0].title) + ' (' + sentence(bad[0].fact) + ').');
      if (bad.length > 1) rk += sp + L3('Also mapped: ', '另需注意：', 'También: ') + bad.slice(1).map(function (x) { return l === 'zh' ? sentence(x.fact) : lowerFirst(sentence(x.fact)); }).join(l === 'zh' ? '；' : '; ') + (l === 'zh' ? '。' : '.');
    } else if (hazards.length) {
      rk = L3('No high-risk natural hazards are mapped here.', '此处未见高风险自然灾害。', 'No hay riesgos naturales altos mapeados aquí.');
    }
    if (d.pois.length && bad.length < 2) {
      rk += (rk ? sp : '') + L3('Nearby: ', '周边：', 'Cerca: ') + d.pois.slice(0, 2).map(function (p) { return (l === 'en' ? p.label.toLowerCase() : p.label) + ' ' + fNum(p.dist, 2) + ' mi'; }).join(l === 'zh' ? '，' : ', ') + (l === 'zh' ? '。' : '.');
    }
    out.push(rk);
    // 6 · Verdict
    var vd = '';
    if (d.verdict.text) vd = L3('Verdict: ', '结论：', 'Veredicto: ') + stripEnd(d.verdict.text) + (l === 'zh' ? '。' : '.');
    // As many checks as fit two caption lines.
    // Budget in "visual" characters (a CJK character counts as two).
    var vlen = function (t) { return t.length + (t.match(/[\u3400-\u9fff\u3000-\u303f\uff00-\uffef]/g) || []).length; };
    var picked = [], budget = 165, full = false;
    d.checks.slice(0, 2).forEach(function (c) {
      if (full) return;
      var next = picked.concat([cased(c)]);
      var line = L3('Before you offer: ', '出价前请核实：', 'Antes de ofertar: ') + next.join(l === 'zh' ? '；' : '; ') + '.';
      if ((vd ? vlen(vd) + sp.length : 0) + vlen(line) <= budget) picked = next;
      else full = true;
    });
    if (picked.length) {
      var listed = picked.join(l === 'zh' ? '；' : '; ');
      vd += (vd ? sp : '') + L3('Before you offer: ', '出价前请核实：', 'Antes de ofertar: ') + listed + (/…$/.test(listed) ? '' : (l === 'zh' ? '。' : '.'));
    }
    out.push(vd);
    return out;
  }

  /* ------------------------------------------------------------------ Aerial imagery & camera */
  var R_MERC = 6378137;
  function mercX(lon) { return R_MERC * lon * Math.PI / 180; }
  function mercY(lat) {
    var r = clamp(lat, -85, 85) * Math.PI / 180;
    return R_MERC * Math.log(Math.tan(Math.PI / 4 + r / 2));
  }
  var WORLD_M = 2 * Math.PI * R_MERC;
  /** USGS The National Map public-domain orthoimagery fallback + high-res regional/proxied sources. */
  var TILE_URL = 'https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/';
  var USGS_TILE_TMPL = TILE_URL + '{z}/{y}/{x}';
  var SCC_SJ_TILE_TMPL = 'https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/tile/{z}/{y}/{x}';
  var SCC_SJ_EXPORT = 'https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/export';
  var KINGCO_TILE_TMPL = 'https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer/tile/{z}/{y}/{x}';
  var KINGCO_EXPORT = 'https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer/export';
  var USGS_ATTR = 'Imagery © USGS The National Map / USDA NAIP (public domain)';
  /** Default USGS cached tile ceiling; resolved high-res sources raise `zMax` up to 19. */
  var Z_MAX = 16;
  var HIGH_RES_TILE_CEIL = 19;
  var AERIAL_EXPORT = 'https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export';
  var DETAIL_HALF_M = 80, DETAIL_PX = 1600;

  function mercToLon(x) { return (x / R_MERC) * (180 / Math.PI); }
  function mercToLat(y) { return (2 * Math.atan(Math.exp(y / R_MERC)) - Math.PI / 2) * (180 / Math.PI); }

  function resolveMotionSatellite(rep, lat, lon) {
    var sat = rep && rep.meta && rep.meta.satellite ? rep.meta.satellite : null;
    var isFile = typeof location !== 'undefined' && location.protocol === 'file:';
    var fbTile = (sat && sat.fallbackTileUrl) || USGS_TILE_TMPL;
    var fbZoom = Math.min((sat && Number(sat.fallbackMaxNativeZoom)) || Z_MAX, HIGH_RES_TILE_CEIL);
    var fbAttr = (sat && sat.fallbackAttribution) || USGS_ATTR;
    if (sat && sat.tileUrl && !(isFile && sat.proxied)) {
      return {
        tileUrl: sat.tileUrl,
        exportUrl: sat.exportUrl === null ? null : (sat.exportUrl || AERIAL_EXPORT),
        zMax: Math.min(Number(sat.maxNativeZoom) || Z_MAX, HIGH_RES_TILE_CEIL),
        attribution: sat.attribution || fbAttr,
        proxied: Boolean(sat.proxied),
        fallbackTileUrl: fbTile,
        fallbackMaxNativeZoom: fbZoom,
        fallbackAttribution: fbAttr
      };
    }
    if (typeof lat === 'number' && typeof lon === 'number' && isFinite(lat) && isFinite(lon)) {
      var inScc = (lat >= 37.10 && lat <= 37.49 && lon >= -122.20 && lon <= -121.55)
        || (lat >= 36.95 && lat <= 37.15 && lon >= -121.78 && lon <= -121.45);
      if (inScc) {
        return {
          tileUrl: SCC_SJ_TILE_TMPL,
          exportUrl: SCC_SJ_EXPORT,
          zMax: HIGH_RES_TILE_CEIL,
          attribution: 'Aerial © City of San José / Santa Clara Valley (2025 Ortho, 0.1m)',
          proxied: false,
          fallbackTileUrl: fbTile,
          fallbackMaxNativeZoom: fbZoom,
          fallbackAttribution: fbAttr
        };
      }
      if (lat >= 47.28 && lat <= 47.78 && lon >= -122.54 && lon <= -121.10) {
        return {
          tileUrl: KINGCO_TILE_TMPL,
          exportUrl: KINGCO_EXPORT,
          zMax: HIGH_RES_TILE_CEIL,
          attribution: 'Aerial © King County GIS (2023 Ortho)',
          proxied: false,
          fallbackTileUrl: fbTile,
          fallbackMaxNativeZoom: fbZoom,
          fallbackAttribution: fbAttr
        };
      }
    }
    return {
      tileUrl: USGS_TILE_TMPL,
      exportUrl: AERIAL_EXPORT,
      zMax: Z_MAX,
      attribution: USGS_ATTR,
      proxied: false,
      fallbackTileUrl: USGS_TILE_TMPL,
      fallbackMaxNativeZoom: Z_MAX,
      fallbackAttribution: USGS_ATTR
    };
  }

  function formatTileUrl(tmpl, z, y, x) {
    var s = String(tmpl || TILE_URL);
    if (s.indexOf('{z}') !== -1) {
      return s.replace('{z}', String(z)).replace('{y}', String(y)).replace('{x}', String(x));
    }
    return s + z + '/' + y + '/' + x;
  }

  /**
   * The lot close-up loads one crisp overhead export centred on the parcel so close-up zooming
   * retains sub-quarter-meter structure clarity alongside live Web Mercator tiles.
   */
  function aerialLayers(lat, lon, focus, sat) {
    var cfg = sat || resolveMotionSatellite(getRep(), lat, lon);
    if (!cfg || !cfg.exportUrl) return [];
    var c = focus || [lat, lon];
    var k = 1 / Math.max(0.2, Math.cos(c[0] * Math.PI / 180));
    var cx = mercX(c[1]), cy = mercY(c[0]), hm = DETAIL_HALF_M * k;
    var b = [cx - hm, cy - hm, cx + hm, cy + hm];
    var url;
    if (
      cfg.exportUrl.indexOf('/api/media/satellite/export') === 0 ||
      cfg.exportUrl.indexOf('/api/satellite-export') === 0
    ) {
      var wgs = [mercToLon(b[0]), mercToLat(b[1]), mercToLon(b[2]), mercToLat(b[3])];
      url = cfg.exportUrl + '?bbox=' + wgs.map(function (n) { return n.toFixed(6); }).join(',') +
        '&w=' + DETAIL_PX + '&h=' + DETAIL_PX + '&zoom=' + (cfg.zMax || HIGH_RES_TILE_CEIL);
    } else {
      url = cfg.exportUrl + '?bbox=' + b.map(function (n) { return n.toFixed(1); }).join(',') +
        '&bboxSR=3857&imageSR=3857&size=' + DETAIL_PX + ',' + DETAIL_PX + '&format=jpg&f=image';
    }
    return [{
      idx: 0, minx: b[0], miny: b[1], maxx: b[2], maxy: b[3], res: 2 * hm / DETAIL_PX,
      url: url
    }];
  }
  var _img = {};
  /** A copy of the image whose edges fade out, so it melts into the tiles around it. */
  function featherImage(im) {
    try {
      var w = im.naturalWidth, h = im.naturalHeight;
      var cv = document.createElement('canvas');
      if (!cv || typeof cv.getContext !== 'function') return null;
      cv.width = w;
      cv.height = h;
      var c = cv.getContext('2d');
      if (!c) return null;
      c.drawImage(im, 0, 0);
      c.globalCompositeOperation = 'destination-out';
      var f = Math.round(Math.min(w, h) * 0.08);
      [[0, 0, f, 0, 0, 0, f, h], [w, 0, w - f, 0, w - f, 0, f, h], [0, 0, 0, f, 0, 0, w, f], [0, h, 0, h - f, 0, h - f, w, f]].forEach(function (e) {
        var g = c.createLinearGradient(e[0], e[1], e[2], e[3]);
        g.addColorStop(0, 'rgba(0, 0, 0, 1)');
        g.addColorStop(1, 'rgba(0, 0, 0, 0)');
        c.fillStyle = g;
        c.fillRect(e[4], e[5], e[6], e[7]);
      });
      return cv;
    } catch (e) { return null; }
  }
  function loadImage(url) {
    if (!url || typeof Image !== 'function') return null;
    if (_img[url]) return _img[url];
    var e = _img[url] = { status: 'loading', src: null, readyAt: 0 };
    try {
      var im = new Image();
      im.crossOrigin = 'anonymous';
      im.decoding = 'async';
      im.onload = function () {
        if (!(im.naturalWidth > 0)) { e.status = 'error'; return; }
        e.src = featherImage(im) || im;
        e.readyAt = Date.now();
        e.status = 'ready';
        requestRepaint();
      };
      im.onerror = function () { e.status = 'error'; };
      im.src = url;
    } catch (err) { e.status = 'error'; }
    return e;
  }

  /** A paused frame is redrawn (once per animation frame at most) as its images arrive. */
  function requestRepaint() {
    if (!state.open || state.playing || state.repaintQueued || typeof window.requestAnimationFrame !== 'function') return;
    state.repaintQueued = true;
    window.requestAnimationFrame(function () {
      state.repaintQueued = false;
      if (state.open && !state.playing) renderFrame(state.t);
    });
  }
  // Map tiles: a small LRU cache keyed z/y/x, filled ahead of the playhead.
  var _tiles = {}, _tileN = 0, _tileClock = 0, _tileKeep = 360, _tileSourceKey = '';
  function tileSpan(z) { return WORLD_M / Math.pow(2, z); }
  function syncTileSource(sat) {
    var srcTmpl = (sat && sat.tileUrl) || USGS_TILE_TMPL;
    if (_tileSourceKey && _tileSourceKey !== srcTmpl) {
      _tiles = {};
      _tileN = 0;
    }
    _tileSourceKey = srcTmpl;
    return srcTmpl;
  }
  function getTile(z, x, y, request, sat) {
    var srcTmpl = syncTileSource(sat);
    var key = z + '/' + y + '/' + x, e = _tiles[key];
    if (e) { e.used = ++_tileClock; return e; }
    if (!request || typeof Image !== 'function') return null;
    e = _tiles[key] = { status: 'loading', img: null, used: ++_tileClock };
    _tileN++;
    try {
      var loadUrl = function (url, onErr) {
        var im = new Image();
        im.crossOrigin = 'anonymous';
        im.decoding = 'async';
        im.onload = function () { e.img = im; e.status = im.naturalWidth > 0 ? 'ready' : 'error'; requestRepaint(); };
        im.onerror = onErr;
        im.src = url;
      };
      var fbTmpl = (sat && sat.fallbackTileUrl) || USGS_TILE_TMPL;
      var fbZMax = (sat && sat.fallbackMaxNativeZoom) || Z_MAX;
      loadUrl(formatTileUrl(srcTmpl, z, y, x), function () {
        if (srcTmpl !== fbTmpl && z <= fbZMax) {
          loadUrl(formatTileUrl(fbTmpl, z, y, x), function () { e.status = 'error'; });
        } else {
          e.status = 'error';
        }
      });
    } catch (err) { e.status = 'error'; }
    if (_tileN > _tileKeep) {
      var old = Object.keys(_tiles).filter(function (k) { return _tiles[k].status !== 'loading'; })
        .sort(function (p, q) { return _tiles[p].used - _tiles[q].used; });
      for (var i = 0; i < old.length && _tileN > _tileKeep * 0.85; i++) { delete _tiles[old[i]]; _tileN--; }
    }
    return e;
  }
  /** The tile level drawn for a camera (fractional `zF`, 1 tile pixel per device pixel) and its range. */
  function tileView(c, W, H, dpr, zMax) {
    var P = projector(c, W, H);
    var zF = Math.log(WORLD_M * P.s * (dpr || 1) / 256) / Math.LN2;
    // Tiles may be magnified up to ~1.4x before the next level is used: sharp enough, far fewer tiles.
    var zTop = clamp(Math.ceil(zF - 0.5), 1, zMax || Z_MAX);
    return {
      P: P, zF: zF, zTop: zTop,
      bx0: c.x - W * c.fx / P.s, bx1: c.x + W * (1 - c.fx) / P.s,
      by0: c.y - H * (1 - c.fy) / P.s, by1: c.y + H * c.fy / P.s
    };
  }
  function tileRange(v, z) {
    var span = tileSpan(z), n = Math.pow(2, z);
    return {
      span: span,
      x0: clamp(Math.floor((v.bx0 + WORLD_M / 2) / span), 0, n - 1), x1: clamp(Math.floor((v.bx1 + WORLD_M / 2) / span), 0, n - 1),
      y0: clamp(Math.floor((WORLD_M / 2 - v.by1) / span), 0, n - 1), y1: clamp(Math.floor((WORLD_M / 2 - v.by0) / span), 0, n - 1)
    };
  }
  /**
   * Every tile the video will show, in the order it first appears, for one aspect ratio and pixel
   * density, plus how many of them the opening shot needs.
   */
  function planTiles(d, isP, dpr) {
    var W = isP ? 720 : 1280, H = isP ? 1280 : 720, keys = [], seen = {}, opening = 0;
    if (!d || !d.hasPoint) return { keys: keys, opening: 0 };
    var zMax = (d.sat && d.sat.zMax) || Z_MAX;
    for (var t = 0; t < TOTAL_DURATION; t += 0.25) {
      var s = sceneAt(t), c = SCENE_CAM[s.idx](s.st, d, isP, W, H);
      if (!c) continue;
      var v = tileView(c, W, H, dpr, zMax), r = tileRange(v, v.zTop);
      for (var y = r.y0; y <= r.y1; y++) {
        for (var x = r.x0; x <= r.x1; x++) {
          var key = v.zTop + '/' + y + '/' + x;
          if (!seen[key]) { seen[key] = 1; keys.push([v.zTop, x, y]); }
        }
      }
      if (t <= 1.5) opening = keys.length;
    }
    return { keys: keys, opening: opening };
  }
  /** Starts every download the video needs: the lot close-up, then the tiles in playback order. */
  function preloadImagery(d, dpr) {
    if (!d || !d.hasPoint) return;
    (d.layers || []).forEach(function (Lr) { loadImage(Lr.url); });
    var isP = state.aspect === '9:16', px = dpr || screenDpr();
    var planKey = (isP ? 'p' : 'l') + px;
    if (!d.tilePlans) d.tilePlans = {};
    if (!d.tilePlans[planKey]) d.tilePlans[planKey] = planTiles(d, isP, px);
    var plan = d.tilePlans[planKey];
    d.tilePlan = plan;
    _tileKeep = Math.max(_tileKeep, plan.keys.length + 80);
    plan.keys.forEach(function (k) { getTile(k[0], k[1], k[2], true, d.sat); });
  }
  /** True while the opening shot's tiles (or, with `all`, every planned image) are still loading. */
  function imageryPending(d, all) {
    if (!d || !d.hasPoint) return false;
    var plan = d.tilePlan;
    if (!plan) return true;
    var n = all ? plan.keys.length : plan.opening;
    for (var i = 0; i < n; i++) {
      var k = plan.keys[i], e = _tiles[k[0] + '/' + k[2] + '/' + k[1]];
      if (!e || e.status === 'loading') return true;
    }
    if (all) return (d.layers || []).some(function (Lr) { var e = _img[Lr.url]; return !e || e.status === 'loading'; });
    return false;
  }
  /** Camera: a ground point, the ground half-span shown across the frame's shorter side, and
   *  where on screen (fractions of W / H) that point sits. */
  function mkCam(d, lat, lon, groundHalf, fx, fy) {
    return { x: mercX(lon), y: mercY(lat), half: groundHalf * d.k, fx: fx, fy: fy };
  }
  function projector(c, W, H) {
    var s = Math.min(W, H) / (2 * c.half), ox = W * c.fx, oy = H * c.fy;
    return {
      s: s,
      mx: function (x) { return ox + (x - c.x) * s; },
      my: function (y) { return oy - (y - c.y) * s; },
      ll: function (la, lo) { return [ox + (mercX(lo) - c.x) * s, oy - (mercY(la) - c.y) * s]; }
    };
  }
  /**
   * Draws the imagery under the camera and returns the projector (null without a location): map
   * tiles at the level matching the zoom over coarser ones already loaded (so a missing tile shows
   * its blurrier parent, never a hole), then the sharp lot image once tiles would be magnified.
   */
  function paintAerial(ctx, W, H, c, d) {
    ctx.fillStyle = PAL.night;
    ctx.fillRect(0, 0, W, H);
    if (!c || !d.hasPoint) { drawFallbackBackdrop(ctx, W, H); return null; }
    syncTileSource(d.sat);
    var dpr = state.dpr || 1, zMax = (d.sat && d.sat.zMax) || Z_MAX, v = tileView(c, W, H, dpr, zMax), P = v.P, drew = false;
    var base = ctx.globalAlpha;
    // Start from the finest level whose tiles are all in, so hidden levels are not drawn.
    var zStart = Math.max(1, v.zTop - 5);
    for (var zz = v.zTop - 1; zz >= zStart; zz--) {
      var rr = tileRange(v, zz), full = true;
      for (var yy = rr.y0; yy <= rr.y1 && full; yy++) {
        for (var xx = rr.x0; xx <= rr.x1; xx++) { var te = _tiles[zz + '/' + yy + '/' + xx]; if (!te || te.status !== 'ready') { full = false; break; } }
      }
      if (full) { zStart = zz; break; }
    }
    ctx.save();
    ctx.imageSmoothingEnabled = true;
    try { ctx.imageSmoothingQuality = 'high'; } catch (e0) {}
    for (var z = zStart; z <= v.zTop; z++) {
      var r = tileRange(v, z);
      // The finest level fades in over the first stretch of its zoom range.
      var a = z < v.zTop ? 1 : clamp((v.zF - v.zTop + 0.5) / 0.25, 0, 1);
      for (var y = r.y0; y <= r.y1; y++) {
        for (var x = r.x0; x <= r.x1; x++) {
          var e = getTile(z, x, y, z >= v.zTop - 1, d.sat);
          if (!e || e.status !== 'ready' || a <= 0) continue;
          var X0 = Math.floor(P.mx(x * r.span - WORLD_M / 2)), X1 = Math.ceil(P.mx((x + 1) * r.span - WORLD_M / 2));
          var Y0 = Math.floor(P.my(WORLD_M / 2 - y * r.span)), Y1 = Math.ceil(P.my(WORLD_M / 2 - (y + 1) * r.span));
          ctx.globalAlpha = base * a;
          ctx.drawImage(e.img, X0, Y0, X1 - X0, Y1 - Y0);
          drew = true;
        }
      }
    }
    ctx.restore();
    // The lot close-up, once tiles would be magnified (and faded in when it arrives mid-shot).
    var disp = 1 / (P.s * dpr), tileRes = tileSpan(zMax) / 256, now = Date.now();
    (d.layers || []).forEach(function (Lr) {
      var e = _img[Lr.url];
      if (!e || e.status !== 'ready' || !e.src) return;
      var la = easeInOutCubic(remap(tileRes / disp, 0.9, 1.5)) * clamp((now - e.readyAt) / 600, 0, 1);
      if (!drew) la = 1;
      var x0 = P.mx(Lr.minx), x1 = P.mx(Lr.maxx), y0 = P.my(Lr.maxy), y1 = P.my(Lr.miny);
      if (la <= 0.004 || x1 <= 0 || x0 >= W || y1 <= 0 || y0 >= H) return;
      var vx0 = Math.max(0, x0), vy0 = Math.max(0, y0), vx1 = Math.min(W, x1), vy1 = Math.min(H, y1);
      var iw = e.src.naturalWidth || e.src.width || DETAIL_PX, ih = e.src.naturalHeight || e.src.height || DETAIL_PX;
      var sx = (vx0 - x0) / (x1 - x0) * iw, sy = (vy0 - y0) / (y1 - y0) * ih;
      var sw = (vx1 - vx0) / (x1 - x0) * iw, sh = (vy1 - vy0) / (y1 - y0) * ih;
      if (sw < 0.5 || sh < 0.5) return;
      ctx.save();
      ctx.globalAlpha = base * la;
      ctx.imageSmoothingEnabled = true;
      try { ctx.imageSmoothingQuality = 'high'; } catch (e2) {}
      ctx.drawImage(e.src, sx, sy, sw, sh, vx0, vy0, vx1 - vx0, vy1 - vy0);
      ctx.restore();
      drew = true;
    });
    if (!drew) drawFallbackBackdrop(ctx, W, H);
    return P;
  }
  function drawFallbackBackdrop(ctx, W, H) {
    var grad = ctx.createLinearGradient(0, 0, W, H);
    grad.addColorStop(0, TH.backdrop[0]);
    grad.addColorStop(0.55, TH.backdrop[1]);
    grad.addColorStop(1, TH.backdrop[2]);
    ctx.fillStyle = grad;
    ctx.fillRect(0, 0, W, H);
    ctx.save();
    ctx.strokeStyle = TH.grid;
    ctx.lineWidth = 1;
    var step = Math.max(40, Math.round(Math.min(W, H) / 13));
    for (var gx = 0; gx < W; gx += step) { ctx.beginPath(); ctx.moveTo(gx, 0); ctx.lineTo(gx, H); ctx.stroke(); }
    for (var gy = 0; gy < H; gy += step) { ctx.beginPath(); ctx.moveTo(0, gy); ctx.lineTo(W, gy); ctx.stroke(); }
    ctx.restore();
  }
  /** Wash for legibility (darker in the dark theme, lighter in the light one) + vignette. */
  function grade(ctx, W, H, dim, vig) {
    if (dim > 0) {
      ctx.fillStyle = wash(dim);
      ctx.fillRect(0, 0, W, H);
    }
    var g = ctx.createRadialGradient(W / 2, H / 2, Math.min(W, H) * 0.36, W / 2, H / 2, Math.max(W, H) * 0.78);
    g.addColorStop(0, 'rgba(' + TH.vignette + ', 0)');
    g.addColorStop(1, 'rgba(' + TH.vignette + ', ' + ((vig == null ? 0.45 : vig) * TH.vignetteK).toFixed(3) + ')');
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);
  }
  /** Gradient scrim behind text: 'left', 'right', 'bottom' or 'top'. */
  function scrim(ctx, W, H, side, strength, reach) {
    var r = reach || 0.55, g;
    if (side === 'left') g = ctx.createLinearGradient(0, 0, W * r, 0);
    else if (side === 'right') g = ctx.createLinearGradient(W, 0, W * (1 - r), 0);
    else if (side === 'top') g = ctx.createLinearGradient(0, 0, 0, H * r);
    else g = ctx.createLinearGradient(0, H, 0, H * (1 - r));
    g.addColorStop(0, wash(strength));
    g.addColorStop(1, 'rgba(' + TH.wash + ', 0)');
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);
  }

  /* ------------------------------------------------------------------ Procedural WebAudio Micro-SFX */
  var AudioEngine = {
    ctx: null,
    dest: null,
    master: null,
    ensure: function () {
      if (this.ctx) {
        if (this.ctx.state === 'suspended' && typeof this.ctx.resume === 'function') {
          try { this.ctx.resume(); } catch (e) {}
        }
        return this.ctx;
      }
      var AC = window.AudioContext || window.webkitAudioContext;
      if (typeof AC !== 'function') return null;
      try {
        this.ctx = new AC();
        this.master = this.ctx.createGain();
        this.master.gain.value = 0.8;
        this.master.connect(this.ctx.destination);
        if (typeof this.ctx.createMediaStreamDestination === 'function') {
          this.dest = this.ctx.createMediaStreamDestination();
          this.master.connect(this.dest);
        }
      } catch (e) {
        this.ctx = null;
      }
      return this.ctx;
    },
    /** Soft air swish on scene changes. */
    whoosh: function () {
      if (!state.audioOn) return;
      var ctx = this.ensure();
      if (!ctx || !this.master) return;
      try {
        var now = ctx.currentTime, dur = 0.42;
        var buf = ctx.createBuffer(1, Math.floor(ctx.sampleRate * dur), ctx.sampleRate);
        var ch = buf.getChannelData(0);
        for (var i = 0; i < ch.length; i++) ch[i] = (Math.random() * 2 - 1) * Math.sin(Math.PI * i / ch.length);
        var src = ctx.createBufferSource();
        src.buffer = buf;
        var flt = ctx.createBiquadFilter();
        flt.type = 'lowpass';
        flt.frequency.setValueAtTime(420, now);
        flt.frequency.exponentialRampToValueAtTime(2200, now + dur * 0.6);
        var gn = ctx.createGain();
        gn.gain.setValueAtTime(0.0001, now);
        gn.gain.linearRampToValueAtTime(0.035, now + 0.12);
        gn.gain.exponentialRampToValueAtTime(0.0005, now + dur);
        src.connect(flt); flt.connect(gn); gn.connect(this.master);
        src.start(now); src.stop(now + dur);
      } catch (e) {}
    },
    finaleChord: function () {
      if (!state.audioOn) return;
      var ctx = this.ensure();
      if (!ctx || !this.master) return;
      try {
        var now = ctx.currentTime, self = this;
        [261.63, 329.63, 392.0, 493.88].forEach(function (freq, idx) {
          var osc = ctx.createOscillator(), gn = ctx.createGain();
          osc.type = 'sine';
          osc.frequency.setValueAtTime(freq, now + idx * 0.05);
          gn.gain.setValueAtTime(0.0005, now + idx * 0.05);
          gn.gain.linearRampToValueAtTime(0.022, now + idx * 0.05 + 0.12);
          gn.gain.exponentialRampToValueAtTime(0.0005, now + 1.6);
          osc.connect(gn); gn.connect(self.master);
          osc.start(now + idx * 0.05);
          osc.stop(now + 1.7);
        });
      } catch (e) {}
    }
  };

  /* ------------------------------------------------------------------ Drawing primitives */
  function F(weight, px) { return weight + ' ' + Math.max(1, px).toFixed(1) + 'px ' + FONT_STACK; }
  function layout(W, H, isP) {
    var u = Math.min(W, H) / 720;
    return {
      u: u,
      m: Math.round(44 * u),
      top: Math.round((isP ? 138 : 92) * u),
      capTop: H - Math.round((isP ? 168 : 112) * u)
    };
  }
  function roundRectPath(ctx, x, y, w, h, r) {
    var rad = Math.max(0, Math.min(r || 0, Math.min(w, h) / 2));
    ctx.beginPath();
    ctx.moveTo(x + rad, y);
    ctx.lineTo(x + w - rad, y);
    ctx.quadraticCurveTo(x + w, y, x + w, y + rad);
    ctx.lineTo(x + w, y + h - rad);
    ctx.quadraticCurveTo(x + w, y + h, x + w - rad, y + h);
    ctx.lineTo(x + rad, y + h);
    ctx.quadraticCurveTo(x, y + h, x, y + h - rad);
    ctx.lineTo(x, y + rad);
    ctx.quadraticCurveTo(x, y, x + rad, y);
    ctx.closePath();
  }
  function fillRoundCard(ctx, x, y, w, h, r, fill, stroke, lineW) {
    ctx.save();
    roundRectPath(ctx, x, y, w, h, r);
    if (fill) { ctx.fillStyle = fill; ctx.fill(); }
    if (stroke) { ctx.strokeStyle = stroke; ctx.lineWidth = lineW || 1; ctx.stroke(); }
    ctx.restore();
  }
  /** Frosted dark card used for every panel. */
  function glass(ctx, x, y, w, h, u, tint) {
    ctx.save();
    ctx.shadowColor = TH.glassShadow;
    ctx.shadowBlur = 24 * u;
    ctx.shadowOffsetY = 8 * u;
    fillRoundCard(ctx, x, y, w, h, 16 * u, tint || TH.glass, null);
    ctx.restore();
    fillRoundCard(ctx, x, y, w, h, 16 * u, null, TH.glassEdge, 1);
  }
  /** Draws text; shrinks it to `maxW` when given. Returns the drawn width. */
  function txt(ctx, s, x, y, o) {
    o = o || {};
    var px = o.px || 16;
    ctx.save();
    ctx.font = F(o.w || 700, px);
    if (o.maxW) {
      var minPx = o.minPx || px * 0.6;
      while (px > minPx && ctx.measureText(s).width > o.maxW) { px -= 1; ctx.font = F(o.w || 700, px); }
    }
    ctx.fillStyle = o.color || PAL.ink;
    ctx.textAlign = o.align || 'left';
    ctx.textBaseline = o.base || 'alphabetic';
    if (o.shadow) { ctx.shadowColor = TH.textShadow; ctx.shadowBlur = o.shadow; ctx.shadowOffsetY = TH.mode === 'dark' ? 1 : 0; }
    if (o.spacing && 'letterSpacing' in ctx) ctx.letterSpacing = o.spacing + 'px';
    ctx.fillText(s, x, y);
    var w = ctx.measureText(s).width;
    ctx.restore();
    return w;
  }
  function wrapLines(ctx, text, maxW, maxLines) {
    // A number stays on the same line as its unit ("2.7 mi", "0.75 g", "1,990 sq ft").
    var raw = String(text || '').trim().replace(/(\d) (?=(mi|g|ft|sq|pies²|%)(\s|$|[,.;:)·]))/g, '$1\u00a0');
    if (!raw) return [];
    var cjk = /[\u4e00-\u9fff\u3000-\u303f\uff00-\uffef]/;
    var tokens = [];
    raw.split(/([ \t\r\n]+)/).forEach(function (w) {
      if (!w || /^[ \t\r\n]+$/.test(w)) { if (tokens.length) tokens.push(' '); return; }
      if (cjk.test(w)) { for (var c = 0; c < w.length; c++) tokens.push(w[c]); } else tokens.push(w);
    });
    var lines = [], cur = '';
    for (var k = 0; k < tokens.length; k++) {
      var tok = tokens[k];
      if (tok === ' ' && !cur) continue;
      var cand = cur + tok;
      // Closing punctuation stays on the line it closes (no line starts with "。" or "，").
      if (ctx.measureText(cand.trim()).width <= maxW || !cur.trim() || /^[。，、；：！？）」』》%,.;:!?)]$/.test(tok)) {
        cur = cand;
      } else {
        lines.push(cur.trim());
        cur = tok === ' ' ? '' : tok;
        if (maxLines && lines.length >= maxLines) break;
      }
    }
    if (cur.trim() && (!maxLines || lines.length < maxLines)) lines.push(cur.trim());
    var consumed = lines.join('').replace(/\s+/g, '').length;
    if (maxLines && lines.length && consumed < raw.replace(/\s+/g, '').length) {
      var last = lines[lines.length - 1];
      while (last.length > 2 && ctx.measureText(last + '…').width > maxW) last = last.slice(0, -1);
      lines[lines.length - 1] = last.replace(/[\s,;:·—–]+$/, '') + '…';
    }
    return lines;
  }
  /** Rounded label chip; returns its width. */
  function pill(ctx, x, y, text, o) {
    o = o || {};
    var u = o.u || 1, h = o.h || 30 * u, px = o.px || 14 * u, padX = o.padX || 13 * u;
    ctx.save();
    ctx.font = F(o.w || 700, px);
    var tw = ctx.measureText(text).width;
    var w = Math.min(o.maxW || 9999, tw + padX * 2 + (o.dot ? 14 * u : 0));
    fillRoundCard(ctx, x, y, w, h, h / 2, o.bg || TH.chipBg, o.border || TH.chipEdge, 1);
    var tx = x + padX;
    if (o.dot) {
      ctx.fillStyle = o.dot;
      ctx.beginPath(); ctx.arc(tx + 4 * u, y + h / 2, 4 * u, 0, Math.PI * 2); ctx.fill();
      tx += 14 * u;
    }
    var t = text;
    var room = w - (tx - x) - padX;
    if (tw > room + 0.5) {
      while (t.length > 3 && ctx.measureText(t + '…').width > room) t = t.slice(0, -1);
      t += '…';
    }
    ctx.fillStyle = o.color || PAL.ink;
    ctx.textBaseline = 'middle';
    ctx.fillText(t, tx, y + h / 2 + 0.5);
    ctx.restore();
    return w;
  }
  /** Amber "verify this" chip used for the deck's watch-outs. */
  function watchChip(ctx, x, y, text, maxW, u, a) {
    if (!text || a <= 0) return 0;
    ctx.save();
    ctx.globalAlpha *= a;
    var w = pill(ctx, x, y, '⚠  ' + text, { u: u, h: 34 * u, px: 14.5 * u, maxW: maxW, bg: TH.watchBg, border: TH.watchEdge, color: TH.watchInk });
    ctx.restore();
    return w;
  }
  function polyArea(pts) {
    var s = 0;
    for (var i = 0; i < pts.length; i++) { var a = pts[i], b = pts[(i + 1) % pts.length]; s += a[0] * b[1] - b[0] * a[1]; }
    return s / 2;
  }
  function polyCentroid(pts) {
    var x = 0, y = 0;
    pts.forEach(function (p) { x += p[0]; y += p[1]; });
    return [x / Math.max(1, pts.length), y / Math.max(1, pts.length)];
  }
  /** Small vector glyphs (no emoji fonts needed). */
  function icon(ctx, kind, cx, cy, r, color) {
    ctx.save();
    ctx.strokeStyle = color;
    ctx.fillStyle = color;
    ctx.lineWidth = Math.max(1.5, r * 0.16);
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';
    var i;
    if (kind === 'flood') {
      for (i = -1; i <= 1; i++) {
        ctx.beginPath();
        for (var k = 0; k <= 12; k++) {
          var xx = cx - r * 0.7 + (k / 12) * r * 1.4, yy = cy + i * r * 0.42 + Math.sin(k / 12 * Math.PI * 2) * r * 0.12;
          if (k) ctx.lineTo(xx, yy); else ctx.moveTo(xx, yy);
        }
        ctx.stroke();
      }
    } else if (kind === 'quake') {
      ctx.beginPath();
      ctx.moveTo(cx - r * 0.8, cy);
      [[-0.45, -0.5], [-0.15, 0.55], [0.15, -0.6], [0.42, 0.45], [0.8, 0]].forEach(function (p) { ctx.lineTo(cx + p[0] * r, cy + p[1] * r); });
      ctx.stroke();
    } else if (kind === 'fire') {
      ctx.beginPath();
      ctx.moveTo(cx, cy - r * 0.8);
      ctx.bezierCurveTo(cx + r * 0.75, cy - r * 0.1, cx + r * 0.55, cy + r * 0.75, cx, cy + r * 0.75);
      ctx.bezierCurveTo(cx - r * 0.55, cy + r * 0.75, cx - r * 0.75, cy - r * 0.05, cx, cy - r * 0.8);
      ctx.stroke();
    } else if (kind === 'alert') {
      ctx.beginPath();
      ctx.moveTo(cx, cy - r * 0.75); ctx.lineTo(cx + r * 0.78, cy + r * 0.62); ctx.lineTo(cx - r * 0.78, cy + r * 0.62); ctx.closePath();
      ctx.stroke();
      ctx.beginPath(); ctx.moveTo(cx, cy - r * 0.22); ctx.lineTo(cx, cy + r * 0.2); ctx.stroke();
      ctx.beginPath(); ctx.arc(cx, cy + r * 0.42, r * 0.06, 0, Math.PI * 2); ctx.fill();
    } else if (kind === 'leaf') {
      ctx.beginPath();
      ctx.moveTo(cx - r * 0.6, cy + r * 0.6);
      ctx.quadraticCurveTo(cx - r * 0.6, cy - r * 0.7, cx + r * 0.65, cy - r * 0.65);
      ctx.quadraticCurveTo(cx + r * 0.6, cy + r * 0.6, cx - r * 0.6, cy + r * 0.6);
      ctx.stroke();
      ctx.beginPath(); ctx.moveTo(cx - r * 0.6, cy + r * 0.6); ctx.lineTo(cx + r * 0.2, cy - r * 0.2); ctx.stroke();
    } else if (kind === 'school') {
      ctx.beginPath();
      ctx.moveTo(cx - r * 0.85, cy - r * 0.2); ctx.lineTo(cx, cy - r * 0.62); ctx.lineTo(cx + r * 0.85, cy - r * 0.2); ctx.lineTo(cx, cy + r * 0.2); ctx.closePath();
      ctx.stroke();
      ctx.beginPath(); ctx.moveTo(cx - r * 0.45, cy); ctx.lineTo(cx - r * 0.45, cy + r * 0.48); ctx.quadraticCurveTo(cx, cy + r * 0.78, cx + r * 0.45, cy + r * 0.48); ctx.lineTo(cx + r * 0.45, cy); ctx.stroke();
    } else if (kind === 'walk') {
      ctx.beginPath();
      ctx.arc(cx, cy - r * 0.22, r * 0.42, Math.PI * 0.85, Math.PI * 2.15);
      ctx.lineTo(cx, cy + r * 0.78);
      ctx.closePath();
      ctx.stroke();
      ctx.beginPath(); ctx.arc(cx, cy - r * 0.22, r * 0.14, 0, Math.PI * 2); ctx.fill();
    } else {
      ctx.beginPath(); ctx.arc(cx, cy, r * 0.5, 0, Math.PI * 2); ctx.stroke();
    }
    ctx.restore();
  }

  /* ------------------------------------------------------------------ Map overlays */
  function ringPts(P, ring) {
    var pts = ring.map(function (p) { return P.ll(p[0], p[1]); });
    if (pts.length > 2) {
      var a = pts[0], b = pts[pts.length - 1];
      if (Math.abs(a[0] - b[0]) < 0.01 && Math.abs(a[1] - b[1]) < 0.01) pts.pop();
    }
    return pts;
  }
  function polyPath(ctx, pts) {
    ctx.beginPath();
    pts.forEach(function (p, i) { if (i) ctx.lineTo(p[0], p[1]); else ctx.moveTo(p[0], p[1]); });
    ctx.closePath();
  }
  /** Outline drawn progressively along its perimeter; the fill fades in as it closes. */
  function traceRing(ctx, pts, prog, o) {
    if (!pts || pts.length < 3 || prog <= 0) return;
    var u = o.u || 1;
    ctx.save();
    if (o.fill) {
      var fa = remap(prog, 0.7, 1);
      if (fa > 0) {
        ctx.save();
        ctx.globalAlpha *= fa;
        polyPath(ctx, pts);
        ctx.fillStyle = o.fill;
        ctx.fill();
        ctx.restore();
      }
    }
    var total = 0;
    for (var i = 0; i < pts.length; i++) total += Math.hypot(pts[(i + 1) % pts.length][0] - pts[i][0], pts[(i + 1) % pts.length][1] - pts[i][1]);
    var target = total * clamp(prog, 0, 1), done = 0, tip = pts[0];
    ctx.beginPath();
    ctx.moveTo(pts[0][0], pts[0][1]);
    for (var j = 0; j < pts.length; j++) {
      var a = pts[j], b = pts[(j + 1) % pts.length], len = Math.hypot(b[0] - a[0], b[1] - a[1]);
      if (done + len <= target) { ctx.lineTo(b[0], b[1]); done += len; tip = b; }
      else {
        var fr = (target - done) / Math.max(1e-6, len);
        tip = [lerp(a[0], b[0], fr), lerp(a[1], b[1], fr)];
        ctx.lineTo(tip[0], tip[1]);
        break;
      }
    }
    if (prog >= 0.999) ctx.closePath();
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    if (o.casing) {
      ctx.strokeStyle = o.casing;
      ctx.lineWidth = o.width + 3.5 * u;
      ctx.stroke();
    }
    if (o.dash) { ctx.setLineDash(o.dash); ctx.lineDashOffset = o.dashOffset || 0; }
    if (o.glow) { ctx.shadowColor = o.glow; ctx.shadowBlur = 12 * u; }
    ctx.strokeStyle = o.color;
    ctx.lineWidth = o.width;
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.shadowBlur = 0;
    if (prog > 0.02 && prog < 0.995) {
      ctx.fillStyle = '#ffffff';
      ctx.beginPath(); ctx.arc(tip[0], tip[1], 4.5 * u, 0, Math.PI * 2); ctx.fill();
    }
    ctx.restore();
  }
  /** Lot (sky-blue dashed), main house (yellow) and garage / ADU (magenta), as on the report's map.
   *  `tm` gives each one's [start, end] trace window in scene seconds; omitted = fully drawn. */
  function drawOutlines(ctx, P, d, st, u, tm, alpha) {
    var g = d.geo;
    ctx.save();
    ctx.globalAlpha *= alpha == null ? 1 : alpha;
    var pr = function (w) { return w ? easeInOutCubic(remap(st, w[0], w[1])) : 1; };
    if (g.lot) {
      traceRing(ctx, ringPts(P, g.lot), pr(tm && tm.lot), {
        u: u, color: OUTLINE.lot, width: 3 * u, dash: [11 * u, 7 * u], dashOffset: -st * 14 * u,
        casing: 'rgba(8, 47, 73, 0.6)', fill: 'rgba(56, 189, 248, 0.10)'
      });
    }
    g.acc.forEach(function (a, i) {
      traceRing(ctx, ringPts(P, a.ring), pr(tm && tm.acc && [tm.acc[0] + i * 0.3, tm.acc[1] + i * 0.3]), {
        u: u, color: OUTLINE.acc, width: 3 * u, casing: 'rgba(74, 4, 78, 0.55)', fill: 'rgba(232, 121, 249, 0.24)', glow: 'rgba(232, 121, 249, 0.55)'
      });
    });
    if (g.house) {
      traceRing(ctx, ringPts(P, g.house), pr(tm && tm.house), {
        u: u, color: OUTLINE.house, width: 3.4 * u, casing: 'rgba(66, 32, 6, 0.55)', fill: 'rgba(250, 204, 21, 0.20)', glow: 'rgba(250, 204, 21, 0.6)'
      });
    }
    ctx.restore();
  }
  function pulse(ctx, x, y, st, u, a) {
    if (a <= 0) return;
    ctx.save();
    ctx.globalAlpha *= a;
    for (var k = 0; k < 2; k++) {
      var ph = (st * 0.75 + k * 0.5) % 1;
      ctx.strokeStyle = 'rgba(45, 212, 191, ' + (0.9 * (1 - ph)).toFixed(3) + ')';
      ctx.lineWidth = 2.4 * u;
      ctx.beginPath(); ctx.arc(x, y, (10 + ph * 48) * u, 0, Math.PI * 2); ctx.stroke();
    }
    ctx.shadowColor = 'rgba(0,0,0,0.5)';
    ctx.shadowBlur = 8 * u;
    ctx.fillStyle = PAL.red;
    ctx.strokeStyle = '#ffffff';
    ctx.lineWidth = 2.6 * u;
    ctx.beginPath(); ctx.arc(x, y, 8 * u, 0, Math.PI * 2); ctx.fill(); ctx.stroke();
    ctx.restore();
  }
  /** Leader line from an anchor to a two-line label card. */
  function callout(ctx, ax, ay, lx, ly, title, value, color, a, u, bounds) {
    if (a <= 0 || !title) return;
    ctx.save();
    ctx.globalAlpha *= a;
    ctx.font = F(800, 16 * u);
    var w1 = value ? ctx.measureText(value).width : 0;
    ctx.font = F(700, 11.5 * u);
    var w2 = ctx.measureText(title.toUpperCase()).width;
    var w = Math.max(w1, w2) + 30 * u, h = (value ? 48 : 30) * u;
    var left = lx < ax;
    var x = left ? lx - w : lx, y = ly - h / 2;
    if (bounds) {
      x = clamp(x, bounds[0], bounds[2] - w);
      y = clamp(y, bounds[1], bounds[3] - h);
    }
    var ex = left ? x + w : x;
    ctx.strokeStyle = TH.leader;
    ctx.lineWidth = 1.6 * u;
    ctx.beginPath(); ctx.moveTo(ax, ay); ctx.lineTo(ex, y + h / 2); ctx.stroke();
    ctx.fillStyle = color;
    ctx.beginPath(); ctx.arc(ax, ay, 4.5 * u, 0, Math.PI * 2); ctx.fill();
    glass(ctx, x, y, w, h, u, TH.glassStrong);
    fillRoundCard(ctx, x + 7 * u, y + 8 * u, 4 * u, h - 16 * u, 2 * u, color, null);
    ctx.textBaseline = 'alphabetic';
    ctx.textAlign = 'left';
    ctx.fillStyle = PAL.muted;
    ctx.font = F(700, 11.5 * u);
    ctx.fillText(title.toUpperCase(), x + 18 * u, y + (value ? 19 : 19.5) * u);
    if (value) {
      ctx.fillStyle = PAL.ink;
      ctx.font = F(800, 16 * u);
      ctx.fillText(value, x + 18 * u, y + 39 * u);
    }
    ctx.restore();
  }
  /** Red arrow out of the house's front, as on the report's map. */
  function facingArrow(ctx, P, d, a, u) {
    var g = d.geo;
    if (!g.house || !isN(g.facingDeg) || a <= 0) return;
    var pts = ringPts(P, g.house), c = polyCentroid(pts);
    var r = Math.sqrt(Math.abs(polyArea(pts)) / Math.PI);
    var b = g.facingDeg * Math.PI / 180, dx = Math.sin(b), dy = -Math.cos(b);
    var x0 = c[0] + dx * r * 0.95, y0 = c[1] + dy * r * 0.95;
    var len = 46 * u * easeOutBack(a);
    var x1 = x0 + dx * len, y1 = y0 + dy * len;
    ctx.save();
    ctx.globalAlpha *= clamp(a * 1.4, 0, 1);
    ctx.strokeStyle = '#ef4444';
    ctx.fillStyle = '#ef4444';
    ctx.lineWidth = 4 * u;
    ctx.lineCap = 'round';
    ctx.shadowColor = 'rgba(0,0,0,0.5)';
    ctx.shadowBlur = 6 * u;
    ctx.beginPath(); ctx.moveTo(x0, y0); ctx.lineTo(x1, y1); ctx.stroke();
    var hx = -dy, hy = dx;
    ctx.beginPath();
    ctx.moveTo(x1 + dx * 12 * u, y1 + dy * 12 * u);
    ctx.lineTo(x1 + hx * 8 * u, y1 + hy * 8 * u);
    ctx.lineTo(x1 - hx * 8 * u, y1 - hy * 8 * u);
    ctx.closePath();
    ctx.fill();
    ctx.shadowBlur = 0;
    if (g.facingLabel) {
      var lbl = L3('Faces ', '朝', 'Mira al ') + compassWord(g.facingLabel);
      ctx.font = F(700, 13 * u);
      var tagW = ctx.measureText(lbl).width + 26 * u, tagH = 28 * u;
      // The tag sits beyond the arrow head, along the facing direction, never on top of the arrow.
      var off = 16 * u + Math.abs(dx) * tagW / 2 + Math.abs(dy) * tagH / 2;
      pill(ctx, x1 + dx * off - tagW / 2, y1 + dy * off - tagH / 2, lbl, { u: u, h: tagH, px: 13 * u, bg: 'rgba(185, 28, 28, 0.92)', border: 'rgba(254, 202, 202, 0.7)', color: '#ffffff' });
    }
    ctx.restore();
  }
  function compassWord(c) {
    var map = { N: ['N', '北', 'N'], NE: ['NE', '东北', 'NE'], E: ['E', '东', 'E'], SE: ['SE', '东南', 'SE'], S: ['S', '南', 'S'], SW: ['SW', '西南', 'SO'], W: ['W', '西', 'O'], NW: ['NW', '西北', 'NO'] };
    var e = map[String(c).toUpperCase()];
    return e ? L3(e[0], e[1], e[2]) : c;
  }

  /* ------------------------------------------------------------------ Scene cameras */
  function lotFitHalf(d, isP) {
    var ext = d.geo.extentM > 1 ? d.geo.extentM : 30;
    return clamp(ext * (isP ? 1.3 : 1.32), 24, 110);
  }
  function camArrival(st, d, isP) {
    if (!d.hasPoint) return null;
    var p = easeInOutCubic(remap(st, 0, 6.9));
    return mkCam(d, d.lat, d.lon, lerp(560, 120, p), isP ? 0.5 : 0.6, isP ? 0.36 : 0.45);
  }
  function camLot(st, d, isP) {
    if (!d.hasPoint) return null;
    var p = easeInOutCubic(remap(st, 0, 3.2));
    var f = d.geo.focus || [d.lat, d.lon];
    var gHalf = lerp(120, lotFitHalf(d, isP), p) * (1 - 0.05 * remap(st, 3.2, 9.5));
    return mkCam(d, lerp(d.lat, f[0], p), lerp(d.lon, f[1], p), gHalf,
      isP ? 0.5 : lerp(0.6, 0.675, p), isP ? lerp(0.36, 0.33, p) : lerp(0.45, 0.5, p));
  }
  function camValue(st, d, isP) {
    if (!d.hasPoint) return null;
    var f = d.geo.focus || [d.lat, d.lon], base = lotFitHalf(d, isP);
    return mkCam(d, f[0], f[1], lerp(base * 1.05, base * 1.75, easeInOutCubic(remap(st, 0, 8.6))), 0.5, isP ? 0.42 : 0.5);
  }
  function camCost(st, d) {
    if (!d.hasPoint) return null;
    return mkCam(d, d.lat + 0.00004 * Math.sin(st * 0.2), d.lon, lerp(72, 58, easeInOutCubic(remap(st, 0, 8.6))), 0.5, 0.5);
  }
  /** Risk scene layout: tile grid on one side, the neighborhood map (with its bounds) on the other. */
  function riskLayout(W, H, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m;
    var tileX0 = isP ? m : Math.round(W * 0.5), tileY0 = isP ? Math.round(H * 0.47) : Lt.top + 4 * u;
    return {
      tileX0: tileX0, tileX1: W - m, tileY0: tileY0, tileY1: Lt.capTop - 14 * u,
      mapB: isP ? [m * 0.5, Lt.top, W - m * 0.5, tileY0 - 64 * u] : [m * 0.5, Lt.top, tileX0 - 14 * u, Lt.capTop - 8 * u],
      fx: isP ? 0.5 : 0.27, fy: isP ? 0.27 : 0.53
    };
  }
  function camRisk(st, d, isP, W, H) {
    if (!d.hasPoint) return null;
    var R = riskLayout(W, H, isP);
    var fit = riskFitHalf(d, W, H, R.mapB, R.fx, R.fy);
    return mkCam(d, d.lat, d.lon, lerp(75, fit, easeInOutCubic(remap(st, 0, 3.8))), R.fx, R.fy);
  }
  function camVerdict(st, d, isP) {
    if (!d.hasPoint) return null;
    var f = d.geo.focus || [d.lat, d.lon], base = lotFitHalf(d, isP);
    return mkCam(d, f[0], f[1], lerp(base * 1.2, base * 2.4, easeInOutCubic(remap(st, 0, 8.6))), 0.5, 0.5);
  }

  /* ------------------------------------------------------------------ Scene 1 · The home (fly-in) */
  function sceneArrival(ctx, W, H, st, d, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m;
    var P = paintAerial(ctx, W, H, camArrival(st, d, isP), d);
    grade(ctx, W, H, 0.04, 0.5);
    scrim(ctx, W, H, 'bottom', 0.88, isP ? 0.62 : 0.66);
    if (!isP) scrim(ctx, W, H, 'left', 0.45, 0.6);
    if (P) {
      var hp = P.ll(d.lat, d.lon);
      pulse(ctx, hp[0], hp[1], st, u, enter(st, 0.4, 0.6) * (1 - remap(st, 5.8, 6.8) * 0.6));
      if (d.geo.lot) {
        ctx.save();
        ctx.globalAlpha *= 0.85;
        traceRing(ctx, ringPts(P, d.geo.lot), easeInOutCubic(remap(st, 4.6, 6.6)), { u: u, color: OUTLINE.lot, width: 2.6 * u, dash: [10 * u, 6 * u], casing: 'rgba(8, 47, 73, 0.55)' });
        ctx.restore();
      }
    }
    // Title block, laid out from the bottom up.
    var F0 = d.facts;
    var maxW = isP ? W - 2 * m : W * 0.62;
    var items = [];
    if (F0.type) items.push({ t: F0.type, accent: true });
    if (isN(F0.beds)) items.push({ t: F0.beds + ' ' + L3('bd', '卧', 'rec.') });
    if (isN(F0.baths)) items.push({ t: fNum(F0.baths, F0.baths % 1 ? 1 : 0) + ' ' + L3('ba', '卫', 'baños') });
    if (isN(F0.sqft)) items.push({ t: fNum(F0.sqft) + ' ' + sqftWord() });
    if (isN(F0.yearBuilt)) items.push({ t: L3('Built ' + F0.yearBuilt, F0.yearBuilt + ' 年建', 'Año ' + F0.yearBuilt) });
    if (isN(F0.lotSqft)) items.push({ t: fNum(F0.lotSqft) + ' ' + sqftWord() + ' ' + L3('lot', '地块', 'lote') });
    var ph = (isP ? 40 : 34) * u, gap = 9 * u, px = (isP ? 18 : 15) * u;
    ctx.save();
    ctx.font = F(700, px);
    var rows = [[]], rowW = 0;
    items.forEach(function (it) {
      var w = Math.min(maxW, ctx.measureText(it.t).width + 28 * u);
      if (rowW && rowW + w > maxW) { rows.push([]); rowW = 0; }
      rows[rows.length - 1].push({ t: it.t, w: w, accent: it.accent });
      rowW += w + gap;
    });
    ctx.restore();
    if (!items.length) rows = [];
    var bottom = Lt.capTop - 16 * u;
    var pillsTop = bottom - rows.length * (ph + gap);
    var n = 0;
    rows.forEach(function (row, ri) {
      var x = m;
      row.forEach(function (pc) {
        var a = enter(st, 1.5 + n * 0.12, 0.5);
        n += 1;
        if (a > 0) {
          ctx.save();
          ctx.globalAlpha *= a;
          ctx.translate(0, (1 - a) * 14 * u);
          pill(ctx, x, pillsTop + ri * (ph + gap), pc.t, pc.accent
            ? { u: u, h: ph, px: px, maxW: pc.w, bg: TH.accentSoft, border: TH.accentEdge, color: TH.accentInk }
            : { u: u, h: ph, px: px, maxW: pc.w, bg: TH.chipBg, border: TH.chipEdge });
          ctx.restore();
        }
        x += pc.w + gap;
      });
    });
    var cityY = pillsTop - 18 * u;
    var streetPx = isP ? 58 : 66;
    var streetY = cityY - 40 * u;
    var eyebrowY = streetY - streetPx * u - 10 * u;
    var a1 = enter(st, 0.8, 0.7), a2 = enter(st, 1.1, 0.7), a0 = enter(st, 0.5, 0.6);
    if (a0 > 0) {
      ctx.save();
      ctx.globalAlpha *= a0;
      var eyebrow = d.headline.home || L3('Property walkthrough', '房产视频导览', 'Recorrido de la propiedad');
      txt(ctx, (langCode() === 'en' ? eyebrow.toUpperCase() : eyebrow), m, eyebrowY, { px: 15 * u, w: 800, color: TH.accent, spacing: 1.2 * u, shadow: 10 * u, maxW: maxW });
      ctx.restore();
    }
    if (a1 > 0) {
      ctx.save();
      ctx.globalAlpha *= a1;
      txt(ctx, d.street, m, streetY + (1 - a1) * 26 * u, { px: streetPx * u, w: 800, minPx: 34 * u, maxW: maxW, shadow: 18 * u });
      ctx.restore();
    }
    if (a2 > 0 && d.cityLine) {
      ctx.save();
      ctx.globalAlpha *= a2;
      txt(ctx, d.cityLine, m, cityY + (1 - a2) * 18 * u, { px: (isP ? 24 : 25) * u, w: 600, color: PAL.soft, maxW: maxW, shadow: 12 * u });
      ctx.restore();
    }
  }

  /* ------------------------------------------------------------------ Scene 2 · The lot & buildings */
  function sceneLot(ctx, W, H, st, d, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m;
    var P = paintAerial(ctx, W, H, camLot(st, d, isP), d);
    grade(ctx, W, H, 0.1, 0.42);
    var panelW = isP ? W - 2 * m : Math.round(W * 0.335);
    var panelX = m, panelY = isP ? Math.round(H * 0.56) : Lt.top;
    var panelH = isP ? Lt.capTop - 14 * u - panelY : Lt.capTop - 14 * u - panelY;
    if (isP) scrim(ctx, W, H, 'bottom', 0.9, 0.5); else scrim(ctx, W, H, 'left', 0.82, 0.5);
    var mapBounds = isP ? [m * 0.5, Lt.top - 30 * u, W - m * 0.5, panelY - 10 * u] : [panelX + panelW + 14 * u, Lt.top - 20 * u, W - m * 0.6, Lt.capTop - 10 * u];
    if (P) {
      var g = d.geo;
      var hasGeom = g.lot || g.house || g.acc.length;
      if (hasGeom) {
        drawOutlines(ctx, P, d, st, u, { lot: [0.4, 2.3], house: [1.3, 2.7], acc: [2.0, 3.2] });
        facingArrow(ctx, P, d, enter(st, 3.4, 0.7), u);
        var mid = [(mapBounds[0] + mapBounds[2]) / 2, (mapBounds[1] + mapBounds[3]) / 2];
        if (g.lot) {
          var lp = ringPts(P, g.lot), best = lp[0];
          lp.forEach(function (p) { if (p[0] - p[1] > best[0] - best[1]) best = p; });
          var lotVal = isN(d.areas.lotRecord || d.areas.lotOutline) ? fNum(d.areas.lotRecord || d.areas.lotOutline) + ' ' + sqftWord() : '';
          callout(ctx, best[0], best[1], best[0] + 30 * u, best[1] - 40 * u, L3('Lot', '地块', 'Lote'), lotVal, OUTLINE.lot, enter(st, 2.6, 0.6), u, mapBounds);
        }
        if (g.house) {
          var hpp = ringPts(P, g.house), hc = polyCentroid(hpp);
          var hdx = hc[0] < mid[0] ? -1 : 1;
          callout(ctx, hc[0], hc[1], hc[0] + hdx * 90 * u, hc[1] + 70 * u, L3('Main house', '主宅', 'Casa principal'),
            isN(d.areas.house) ? fNum(d.areas.house) + ' ' + sqftWord() : '', OUTLINE.house, enter(st, 3.0, 0.6), u, mapBounds);
        }
        g.acc.forEach(function (a, i) {
          var ap = ringPts(P, a.ring), ac = polyCentroid(ap);
          var ref = g.house ? polyCentroid(ringPts(P, g.house)) : mid;
          var vx = ac[0] - ref[0], vy = ac[1] - ref[1], vl = Math.hypot(vx, vy) || 1;
          callout(ctx, ac[0], ac[1], ac[0] + (vx / vl) * 80 * u, ac[1] + (vy / vl) * 60 * u - 20 * u, a.label,
            isN(a.areaSqft) ? fNum(a.areaSqft) + ' ' + sqftWord() : '', OUTLINE.acc, enter(st, 3.4 + i * 0.3, 0.6), u, mapBounds);
        });
      } else {
        var hp0 = P.ll(d.lat, d.lon);
        pulse(ctx, hp0[0], hp0[1], st, u, 1);
      }
    }
    // Facts panel.
    var A = d.areas, rows = [];
    if (isN(A.lotRecord) || isN(A.lotOutline)) {
      var both = isN(A.lotRecord) && isN(A.lotOutline) && Math.abs(A.lotRecord - A.lotOutline) / A.lotOutline > 0.0075;
      rows.push({ color: OUTLINE.lot, dash: true, label: L3('Lot', '地块', 'Lote'), value: fNum(A.lotRecord || A.lotOutline) + ' ' + sqftWord(),
        sub: both ? L3('county record · outline ' + fNum(A.lotOutline), '县登记 · 红线实测 ' + fNum(A.lotOutline), 'registro · contorno ' + fNum(A.lotOutline))
          : (isN(A.lotRecord) ? L3('county record', '县登记面积', 'registro del condado') : L3('measured from the parcel outline', '按地块红线实测', 'medido del contorno')) });
    }
    if (isN(A.house)) {
      rows.push({ color: OUTLINE.house, label: L3('Main house', '主宅', 'Casa principal'), value: fNum(A.house) + ' ' + sqftWord(),
        sub: L3('roof footprint', '屋顶投影面积', 'huella del techo') + (isN(d.facts.sqft) ? ' · ' + L3(fNum(d.facts.sqft) + ' sq ft living', '室内 ' + fNum(d.facts.sqft), fNum(d.facts.sqft) + ' pies² habitables') : '') });
    }
    d.geo.acc.forEach(function (a) {
      rows.push({ color: OUTLINE.acc, label: a.label, value: isN(a.areaSqft) ? fNum(a.areaSqft) + ' ' + sqftWord() : L3('on the lot', '位于地块内', 'en el lote'), sub: L3('roof footprint', '屋顶投影面积', 'huella del techo') });
    });
    if (isN(A.openLot) || isN(A.coveragePct)) {
      rows.push({ color: '#4ade80', label: L3('Open yard', '空地', 'Patio libre'), value: isN(A.openLot) ? fNum(A.openLot) + ' ' + sqftWord() : fNum(100 - A.coveragePct, 0) + '%',
        sub: isN(A.coveragePct) ? L3(fNum(A.coveragePct, 0) + '% of the lot is built', '建筑覆盖率 ' + fNum(A.coveragePct, 0) + '%', fNum(A.coveragePct, 0) + '% del lote construido') : '' });
    }
    rows = rows.slice(0, 4);
    var pa = enter(st, 0.6, 0.7);
    if (pa > 0 && (rows.length || d.headline.lot)) {
      ctx.save();
      ctx.globalAlpha *= pa;
      ctx.translate((isP ? 0 : -1) * (1 - pa) * 30 * u, (isP ? 1 : 0) * (1 - pa) * 30 * u);
      glass(ctx, panelX, panelY, panelW, panelH, u);
      var title = d.headline.lot || L3('The lot & buildings', '地块与建筑', 'El lote y las construcciones');
      var ix = panelX + 24 * u, iw = panelW - 48 * u;
      txt(ctx, L3('THE LOT', '地块', 'EL LOTE'), ix, panelY + 34 * u, { px: 12.5 * u, w: 800, color: TH.accent, spacing: 1.4 * u });
      txt(ctx, title, ix, panelY + 68 * u, { px: (isP ? 27 : 26) * u, w: 800, maxW: iw, minPx: 17 * u });
      var cols = isP ? 2 : 1, ts = isP ? 1.2 : 1;
      var rowH = isP ? 90 * u : Math.min(78 * u, (panelH - 150 * u) / Math.max(1, rows.length));
      var colW = iw / cols;
      rows.forEach(function (r, i) {
        var ra = enter(st, 1.0 + i * 0.45, 0.55);
        if (ra <= 0) return;
        var cx = ix + (i % cols) * colW, cy = panelY + 92 * u + Math.floor(i / cols) * rowH;
        ctx.save();
        ctx.globalAlpha *= ra;
        ctx.translate(0, (1 - ra) * 10 * u);
        ctx.strokeStyle = r.color;
        ctx.fillStyle = r.color;
        ctx.lineWidth = 3 * u;
        if (r.dash) ctx.setLineDash([5 * u, 3.5 * u]);
        roundRectPath(ctx, cx, cy + 6 * u, 16 * u * ts, 16 * u * ts, 3 * u);
        ctx.save(); ctx.globalAlpha *= 0.25; ctx.fill(); ctx.restore();
        ctx.stroke();
        ctx.setLineDash([]);
        var tx0 = cx + 28 * u * ts, tw0 = colW - 34 * u * ts;
        txt(ctx, r.label.toUpperCase(), tx0, cy + 19 * u * ts, { px: 12 * u * ts, w: 700, color: PAL.dim, maxW: tw0 });
        txt(ctx, r.value, tx0, cy + 46 * u * ts, { px: 22 * u * ts, w: 800, maxW: tw0, minPx: 14 * u });
        if (r.sub) txt(ctx, r.sub, tx0, cy + 66 * u * ts, { px: 12.5 * u * ts, w: 600, color: PAL.muted, maxW: tw0, minPx: 9 * u });
        ctx.restore();
      });
      watchChip(ctx, ix, panelY + panelH - 52 * u, d.watch.lot, iw, u, enter(st, 5.0, 0.6));
      ctx.restore();
    }
  }

  /* ------------------------------------------------------------------ Scene 3 · Value */
  function sceneValue(ctx, W, H, st, d, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m;
    var P = paintAerial(ctx, W, H, camValue(st, d, isP), d);
    if (P) drawOutlines(ctx, P, d, st, u, null, 0.55);
    grade(ctx, W, H, 0.6, 0.55);
    var V = d.value;
    if (!V) {
      txt(ctx, L3('No value estimate in this report', '本报告暂无估值', 'Este informe no tiene valoración'), W / 2, H / 2, { px: 30 * u, w: 800, align: 'center', base: 'middle', shadow: 14 * u });
      return;
    }
    var x0 = m + (isP ? 0 : 6 * u);
    var yE = Lt.top + (isP ? 90 : 52) * u;
    var aE = enter(st, 0.2, 0.5);
    if (aE > 0) {
      ctx.save();
      ctx.globalAlpha *= aE;
      txt(ctx, L3('ESTIMATED VALUE', '估值', 'VALOR ESTIMADO'), x0, yE, { px: 14 * u, w: 800, color: TH.accent, spacing: 1.5 * u });
      ctx.restore();
    }
    // Counts up over the last stretch only, so the figure on screen is always plausible.
    var count = lerp(V.est * 0.84, V.est, easeOutExpo(remap(st, 0.3, 2.1)));
    var bigPx = (isP ? 84 : 104) * u;
    var aN = enter(st, 0.3, 0.4);
    if (aN > 0) {
      ctx.save();
      ctx.globalAlpha *= aN;
      txt(ctx, fUsd(count), x0 - 4 * u, yE + bigPx * 0.98, { px: bigPx, w: 800, maxW: W - 2 * m, minPx: 48 * u, shadow: 22 * u });
      ctx.restore();
    }
    var subBits = [];
    if (V.confidence) subBits.push(L3(confidenceWord(V.confidence) + ' confidence', '置信度：' + confidenceWord(V.confidence), 'Confianza ' + confidenceWord(V.confidence)));
    if (isN(V.perSqft)) subBits.push(fUsd(V.perSqft) + ' / ' + sqftWord());
    var aS = enter(st, 1.4, 0.6);
    if (aS > 0 && subBits.length) {
      ctx.save();
      ctx.globalAlpha *= aS;
      txt(ctx, subBits.join('  ·  '), x0, yE + bigPx * 0.98 + 40 * u, { px: 21 * u, w: 600, color: PAL.soft, maxW: W - 2 * m });
      ctx.restore();
    }
    // Value axis: estimate, range and the report's own benchmarks on one zoomed scale.
    var vals = [V.est, V.low, V.high].concat(V.refs.map(function (r) { return r.value; })).filter(isN);
    var lo = Math.min.apply(null, vals), hi = Math.max.apply(null, vals);
    var pad = Math.max((hi - lo) * 0.14, V.est * 0.03);
    lo -= pad; hi += pad;
    var ax0 = m + 24 * u, ax1 = W - m - 24 * u, ay = isP ? H * 0.53 : H * 0.62;
    var X = function (val) { return ax0 + (val - lo) / (hi - lo) * (ax1 - ax0); };
    var aT = enter(st, 1.8, 0.6);
    if (aT > 0) {
      ctx.save();
      ctx.globalAlpha *= aT;
      var mx = (ax0 + ax1) / 2, half = (ax1 - ax0) / 2 * aT;
      ctx.strokeStyle = TH.axis;
      ctx.lineWidth = 2 * u;
      ctx.beginPath(); ctx.moveTo(mx - half, ay); ctx.lineTo(mx + half, ay); ctx.stroke();
      ctx.restore();
    }
    var bandP = easeInOutCubic(remap(st, 2.2, 3.2));
    if (bandP > 0 && isN(V.low) && isN(V.high)) {
      var xl = lerp(X(V.est), X(V.low), bandP), xr = lerp(X(V.est), X(V.high), bandP);
      ctx.save();
      var bg = ctx.createLinearGradient(xl, 0, xr, 0);
      bg.addColorStop(0, 'rgba(56, 189, 248, 0.85)');
      bg.addColorStop(0.5, 'rgba(45, 212, 191, 0.95)');
      bg.addColorStop(1, 'rgba(56, 189, 248, 0.85)');
      ctx.shadowColor = 'rgba(45, 212, 191, 0.6)';
      ctx.shadowBlur = 16 * u;
      fillRoundCard(ctx, xl, ay - 8 * u, Math.max(2, xr - xl), 16 * u, 8 * u, bg, null);
      ctx.restore();
      var aLH = enter(st, 3.0, 0.5);
      if (aLH > 0) {
        ctx.save();
        ctx.globalAlpha *= aLH;
        txt(ctx, fUsdK(V.low), X(V.low), ay + 34 * u, { px: 18 * u, w: 800, align: 'center' });
        txt(ctx, L3('LOW', '下限', 'MÍN.'), X(V.low), ay + 52 * u, { px: 11 * u, w: 700, color: PAL.dim, align: 'center', spacing: 1 * u });
        txt(ctx, fUsdK(V.high), X(V.high), ay + 34 * u, { px: 18 * u, w: 800, align: 'center' });
        txt(ctx, L3('HIGH', '上限', 'MÁX.'), X(V.high), ay + 52 * u, { px: 11 * u, w: 700, color: PAL.dim, align: 'center', spacing: 1 * u });
        ctx.restore();
      }
    }
    var aM = enter(st, 2.5, 0.5);
    if (aM > 0) {
      ctx.save();
      ctx.globalAlpha *= aM;
      var ex = X(V.est);
      ctx.fillStyle = '#ffffff';
      ctx.strokeStyle = PAL.teal;
      ctx.lineWidth = 4 * u;
      ctx.beginPath(); ctx.arc(ex, ay, 11 * u * easeOutBack(aM), 0, Math.PI * 2); ctx.fill(); ctx.stroke();
      txt(ctx, fUsdK(V.est), ex, ay - 30 * u, { px: 24 * u, w: 800, align: 'center', shadow: 10 * u });
      txt(ctx, brandInfo().upper, ex, ay - 58 * u, { px: 11.5 * u, w: 800, color: TH.accent, align: 'center', spacing: 1.2 * u });
      ctx.restore();
    }
    // Benchmarks: diamonds on the axis, labels in two rows below so they never collide.
    var refs = V.refs.slice().sort(function (a, b) { return a.value - b.value; });
    var lastRight = [-1e9, -1e9];
    refs.forEach(function (r, i) {
      var a = enter(st, 3.6 + i * 0.45, 0.5);
      if (a <= 0) return;
      var rx = X(r.value);
      var rts = isP ? 1.2 : 1;
      ctx.save();
      ctx.font = F(700, 12.5 * u * rts);
      var lw = Math.max(ctx.measureText(r.label).width, 70 * u) + 20 * u;
      ctx.restore();
      var row = (rx - lw / 2 > lastRight[0] + 8 * u) ? 0 : 1;
      lastRight[row] = rx + lw / 2;
      var ly = ay + (isN(V.low) ? 92 : 50) * u + row * 60 * u * rts;
      ctx.save();
      ctx.globalAlpha *= a;
      ctx.strokeStyle = TH.tick;
      ctx.lineWidth = 1.4 * u;
      ctx.beginPath(); ctx.moveTo(rx, ay + 10 * u); ctx.lineTo(rx, ly - 16 * u); ctx.stroke();
      ctx.fillStyle = r.color;
      ctx.strokeStyle = 'rgba(0, 0, 0, 0.5)';
      ctx.lineWidth = 1.5 * u;
      ctx.beginPath();
      var ds = 9 * u * easeOutBack(a);
      ctx.moveTo(rx, ay - ds); ctx.lineTo(rx + ds, ay); ctx.lineTo(rx, ay + ds); ctx.lineTo(rx - ds, ay); ctx.closePath();
      ctx.fill(); ctx.stroke();
      var tx = clamp(rx, ax0 + lw / 2 - 20 * u, ax1 - lw / 2 + 20 * u);
      txt(ctx, r.label, tx, ly, { px: 12.5 * u * rts, w: 700, color: PAL.muted, align: 'center', shadow: 8 * u });
      txt(ctx, fUsdK(r.value), tx, ly + 23 * u * rts, { px: 18 * u * rts, w: 800, color: inkFor(r.color), align: 'center', shadow: 8 * u });
      ctx.restore();
    });
    watchChip(ctx, x0, Lt.capTop - 54 * u, d.watch.value, W - 2 * m, u, enter(st, 5.4, 0.6));
  }

  /* ------------------------------------------------------------------ Scene 4 · Cost to own */
  function hatch(ctx, x, y, w, h, color, u) {
    ctx.save();
    roundRectPath(ctx, x, y, w, h, 4 * u);
    ctx.clip();
    ctx.fillStyle = 'rgba(168, 85, 247, 0.22)';
    ctx.fillRect(x, y, w, h);
    ctx.strokeStyle = color;
    ctx.lineWidth = 2 * u;
    for (var k = -h; k < w + h; k += 9 * u) { ctx.beginPath(); ctx.moveTo(x + k, y + h); ctx.lineTo(x + k + h, y); ctx.stroke(); }
    ctx.restore();
  }
  function sceneCost(ctx, W, H, st, d, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m;
    var P = paintAerial(ctx, W, H, camCost(st, d, isP), d);
    if (P) drawOutlines(ctx, P, d, st, u, null, 0.5);
    grade(ctx, W, H, 0.64, 0.55);
    var C = d.cost;
    if (!C) {
      txt(ctx, L3('No monthly cost estimate in this report', '本报告暂无月度持有成本', 'Este informe no tiene costo mensual'), W / 2, H / 2, { px: 28 * u, w: 800, align: 'center', base: 'middle', shadow: 14 * u });
      return;
    }
    var x0 = m + (isP ? 0 : 6 * u), w0 = W - 2 * m;
    var yE = Lt.top + (isP ? 90 : 52) * u;
    var aE = enter(st, 0.2, 0.5);
    if (aE > 0) {
      ctx.save();
      ctx.globalAlpha *= aE;
      txt(ctx, L3('COST TO OWN · PER MONTH', '持有成本 · 每月', 'COSTO DE SER DUEÑO · AL MES'), x0, yE, { px: 14 * u, w: 800, color: TH.amberInk, spacing: 1.5 * u });
      ctx.restore();
    }
    var bigPx = (isP ? 80 : 96) * u;
    var aN = enter(st, 0.3, 0.4);
    if (aN > 0) {
      ctx.save();
      ctx.globalAlpha *= aN;
      var bw = txt(ctx, fUsd(lerp(C.total * 0.8, C.total, easeOutExpo(remap(st, 0.3, 1.9)))), x0 - 4 * u, yE + bigPx * 0.98, { px: bigPx, w: 800, shadow: 22 * u, maxW: w0 * 0.8, minPx: 46 * u });
      txt(ctx, L3('/mo', '/月', '/mes'), x0 + bw + 8 * u, yE + bigPx * 0.98, { px: 30 * u, w: 700, color: PAL.muted });
      ctx.restore();
    }
    var terms = [];
    if (isN(C.downPct)) terms.push(L3(fNum(C.downPct) + '% down', '首付 ' + fNum(C.downPct) + '%', fNum(C.downPct) + '% de enganche'));
    if (isN(C.rate30)) terms.push(L3(fNum(C.rate30, 2) + '% 30-yr fixed', '30 年固定 ' + fNum(C.rate30, 2) + '%', fNum(C.rate30, 2) + '% fijo a 30 años'));
    if (isN(C.taxRatePct)) terms.push(L3(fNum(C.taxRatePct, 2) + '% property tax', '房产税 ' + fNum(C.taxRatePct, 2) + '%', fNum(C.taxRatePct, 2) + '% de impuesto'));
    var aS = enter(st, 1.2, 0.6);
    if (aS > 0 && terms.length) {
      ctx.save();
      ctx.globalAlpha *= aS;
      txt(ctx, terms.join('  ·  '), x0, yE + bigPx * 0.98 + 38 * u, { px: 19 * u, w: 600, color: PAL.soft, maxW: w0 });
      ctx.restore();
    }
    // Stacked bar: the parts grow one by one; the optional earthquake cover is hatched past the total.
    var scale = Math.max(C.total + (C.quake || 0), C.rent || 0) * 1.03;
    var by = isP ? H * 0.42 : H * 0.5, bh = (isP ? 46 : 40) * u;
    var BX = function (val) { return x0 + (val / scale) * w0; };
    var aB = enter(st, 1.4, 0.4);
    if (aB > 0) {
      ctx.save();
      ctx.globalAlpha *= aB;
      fillRoundCard(ctx, x0, by, w0, bh, 9 * u, TH.track, TH.trackEdge, 1);
      ctx.restore();
    }
    var acc = 0;
    ctx.save();
    roundRectPath(ctx, x0, by, w0, bh, 9 * u);
    ctx.clip();
    C.parts.forEach(function (p, i) {
      var gp = easeOutCubic(remap(st, 1.6 + i * 0.42, 2.2 + i * 0.42));
      if (gp > 0) {
        var sx = BX(acc), sw = (p.value / scale) * w0 * gp;
        ctx.fillStyle = p.color;
        ctx.fillRect(sx, by, Math.max(0, sw - 2 * u), bh);
      }
      acc += p.value;
    });
    ctx.restore();
    var qStart = 1.6 + C.parts.length * 0.42;
    if (C.quake) {
      var qp = easeOutCubic(remap(st, qStart, qStart + 0.6));
      if (qp > 0) {
        hatch(ctx, BX(acc), by, (C.quake / scale) * w0 * qp, bh, PAL.purple, u);
        ctx.save();
        ctx.globalAlpha *= enter(st, qStart + 0.3, 0.5);
        txt(ctx, '+ ' + fUsd(C.quake) + ' ' + L3('earthquake cover (optional)', '地震险（可选）', 'seguro sísmico (opcional)'), BX(acc + C.quake) , by - 12 * u, { px: 13.5 * u, w: 700, color: TH.purpleInk, align: 'right', shadow: 8 * u });
        ctx.restore();
      }
    }
    // Legend under the bar (one row in landscape; larger type in portrait, which phones show small).
    var ts = isP ? 1.25 : 1;
    var cols = isP ? 2 : Math.min(5, C.parts.length), lgY = by + bh + 26 * u, cw = w0 / cols, lh = 54 * u * ts;
    C.parts.forEach(function (p, i) {
      var a = enter(st, 1.8 + i * 0.42, 0.5);
      if (a <= 0) return;
      var lx = x0 + (i % cols) * cw, ly = lgY + Math.floor(i / cols) * lh;
      ctx.save();
      ctx.globalAlpha *= a;
      fillRoundCard(ctx, lx, ly + 4 * u, 14 * u * ts, 14 * u * ts, 3 * u, p.color, null);
      txt(ctx, p.label, lx + 24 * u * ts, ly + 16 * u * ts, { px: 13.5 * u * ts, w: 700, color: PAL.muted, maxW: cw - 30 * u * ts });
      txt(ctx, fUsd(p.value), lx + 24 * u * ts, ly + 42 * u * ts, { px: 20 * u * ts, w: 800, maxW: cw - 30 * u * ts });
      ctx.restore();
    });
    var legendRows = Math.ceil(C.parts.length / cols);
    // Rent comparison on the same scale (kept clear of the watch-out chip above the caption).
    if (C.rent) {
      var ry = lgY + legendRows * lh + 14 * u;
      var aR = enter(st, qStart + 0.9, 0.6);
      var rowH = (isP ? 92 : 52) * u * ts;
      if (aR > 0 && ry + rowH < Lt.capTop - 60 * u) {
        ctx.save();
        ctx.globalAlpha *= aR;
        txt(ctx, L3('Market rent', '市场租金', 'Renta de mercado'), x0, ry + 14 * u * ts, { px: 13.5 * u * ts, w: 700, color: PAL.muted });
        var rw = (C.rent / scale) * w0 * easeOutCubic(remap(st, qStart + 0.9, qStart + 1.6));
        var barY = ry + 24 * u * ts, barH = 20 * u * ts;
        fillRoundCard(ctx, x0, barY, Math.max(4 * u, rw), barH, 6 * u, '#16a34a', null);
        txt(ctx, fUsd(C.rent) + L3('/mo', '/月', '/mes'), x0 + rw + 12 * u, barY + barH * 0.8, { px: 18 * u * ts, w: 800, color: TH.greenInk });
        if (isN(C.ownMinusRent)) {
          var msg = C.ownMinusRent > 0
            ? L3('Owning costs ' + fUsd(C.ownMinusRent) + '/mo more than renting', '持有比租房每月多 ' + fUsd(C.ownMinusRent), 'Ser dueño cuesta ' + fUsd(C.ownMinusRent) + '/mes más que rentar')
            : L3('Owning costs less than renting', '持有成本低于租房', 'Ser dueño cuesta menos que rentar');
          var mcol = C.ownMinusRent > 0 ? TH.amberInk : TH.goodInk;
          if (isP) txt(ctx, msg, x0, barY + barH + 34 * u * ts, { px: 19 * u * ts, w: 800, color: mcol, maxW: w0, minPx: 13 * u });
          else txt(ctx, msg, W - m, barY + barH * 0.8, { px: 18 * u, w: 800, color: mcol, align: 'right', maxW: w0 * 0.5, minPx: 12 * u });
        }
        ctx.restore();
      }
    }
    watchChip(ctx, x0, Lt.capTop - 54 * u, d.watch.cost, w0, u, enter(st, 6.0, 0.6));
  }

  /* ------------------------------------------------------------------ Scene 5 · Risks & neighborhood */
  /**
   * Ground half-span (m) at which the home and its nearest everyday places (within 1.2 mi) all fit
   * the map area, so the zoom-out ends on a view that shows them.
   */
  function riskFitHalf(d, W, H, mapB, fx, fy) {
    var ox = W * fx, oy = H * fy, pad = 46 * Math.min(W, H) / 720, sMax = Infinity;
    d.pois.filter(function (p) { return p.dist <= 1.2; }).slice(0, 3).forEach(function (p) {
      var o = groundOffsetM(d.lat, d.lon, p.lat, p.lon);
      if (o[0] > 0) sMax = Math.min(sMax, (mapB[2] - pad - ox) / o[0]);
      if (o[0] < 0) sMax = Math.min(sMax, (ox - mapB[0] - pad) / -o[0]);
      if (o[1] > 0) sMax = Math.min(sMax, (oy - mapB[1] - pad) / o[1]);
      if (o[1] < 0) sMax = Math.min(sMax, (mapB[3] - pad - oy) / -o[1]);
    });
    if (!isFinite(sMax) || sMax <= 0) return 320;
    return clamp(Math.min(W, H) / (2 * sMax), 260, 1400);
  }
  function rectsOverlap(a, b) { return a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]; }
  function sceneRisk(ctx, W, H, st, d, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m, ts = isP ? 1.25 : 1;
    var R = riskLayout(W, H, isP);
    var tileX0 = R.tileX0, tileX1 = R.tileX1, tileY0 = R.tileY0, tileY1 = R.tileY1, mapB = R.mapB;
    var P = paintAerial(ctx, W, H, camRisk(st, d, isP, W, H), d);
    grade(ctx, W, H, 0.26, 0.5);
    if (isP) scrim(ctx, W, H, 'bottom', 0.92, 0.6); else scrim(ctx, W, H, 'right', 0.88, 0.6);
    if (P) {
      if (d.geo.lot || d.geo.house) drawOutlines(ctx, P, d, st, u, null, 1 - 0.5 * remap(st, 1, 3.5));
      var hp = P.ll(d.lat, d.lon);
      pulse(ctx, hp[0], hp[1], st, u, 1);
      // Everyday places: a pin where it is on the map, or an arrow at the map's edge pointing to it.
      var inset = [mapB[0] + 26 * u, mapB[1] + 26 * u, mapB[2] - 26 * u, mapB[3] - 26 * u];
      var placed = [[hp[0] - 20 * u, hp[1] - 20 * u, hp[0] + 20 * u, hp[1] + 20 * u]];
      d.pois.forEach(function (p, i) {
        var a = enter(st, 2.6 + i * 0.3, 0.5);
        if (a <= 0) return;
        var q = P.ll(p.lat, p.lon);
        var inside = q[0] >= inset[0] && q[0] <= inset[2] && q[1] >= inset[1] && q[1] <= inset[3];
        var px0 = q[0], py0 = q[1], ang = 0;
        if (!inside) {
          var vx = q[0] - hp[0], vy = q[1] - hp[1];
          var tx = vx > 0 ? (inset[2] - hp[0]) / vx : vx < 0 ? (inset[0] - hp[0]) / vx : Infinity;
          var ty = vy > 0 ? (inset[3] - hp[1]) / vy : vy < 0 ? (inset[1] - hp[1]) / vy : Infinity;
          var tt = Math.max(0, Math.min(tx, ty));
          px0 = hp[0] + vx * tt; py0 = hp[1] + vy * tt; ang = Math.atan2(vy, vx);
        }
        var rr = 13 * u * ts;
        ctx.save();
        ctx.globalAlpha *= a;
        ctx.shadowColor = 'rgba(0, 0, 0, 0.5)';
        ctx.shadowBlur = 8 * u;
        ctx.fillStyle = p.color;
        ctx.strokeStyle = '#ffffff';
        ctx.lineWidth = 2.2 * u;
        if (!inside) {
          // Arrow head just outside the disc, toward the place.
          ctx.beginPath();
          ctx.moveTo(px0 + Math.cos(ang) * (rr + 11 * u), py0 + Math.sin(ang) * (rr + 11 * u));
          ctx.lineTo(px0 + Math.cos(ang + 0.5) * (rr + 1 * u), py0 + Math.sin(ang + 0.5) * (rr + 1 * u));
          ctx.lineTo(px0 + Math.cos(ang - 0.5) * (rr + 1 * u), py0 + Math.sin(ang - 0.5) * (rr + 1 * u));
          ctx.closePath();
          ctx.fill();
        }
        ctx.beginPath(); ctx.arc(px0, py0, rr * easeOutBack(a), 0, Math.PI * 2); ctx.fill(); ctx.stroke();
        ctx.shadowBlur = 0;
        txt(ctx, p.letter, px0, py0 + 0.5, { px: 13 * u * ts, w: 800, align: 'center', base: 'middle', color: '#ffffff' });
        var lbl = p.label + ' · ' + fNum(p.dist, 2) + ' mi';
        var ph = 26 * u * ts, lpx = 12.5 * u * ts;
        var pw = pill(ctx, 0, -9999, lbl, { u: u, h: ph, px: lpx });
        var right = px0 + rr + 8 * u + pw <= mapB[2] - 4 * u && !(!inside && Math.cos(ang) > 0.5);
        var lx = right ? px0 + rr + 8 * u : px0 - rr - 8 * u - pw, ly = py0 - ph / 2;
        for (var k = 0; k < 4; k++) {
          var rect = [lx, ly, lx + pw, ly + ph];
          if (!placed.some(function (r) { return rectsOverlap(r, rect); })) break;
          ly += (py0 > (mapB[1] + mapB[3]) / 2 ? -1 : 1) * (ph + 6 * u);
        }
        placed.push([lx, ly, lx + pw, ly + ph]);
        pill(ctx, lx, ly, lbl, { u: u, h: ph, px: lpx, bg: TH.glassStrong, border: TH.chipEdge });
        ctx.restore();
      });
    }
    var aH = enter(st, 0.3, 0.6);
    var hx = isP ? m : tileX0, hy = isP ? tileY0 - 44 * u : Lt.top + 4 * u;
    if (aH > 0) {
      ctx.save();
      ctx.globalAlpha *= aH;
      txt(ctx, L3('RISKS & NEIGHBORHOOD', '风险与生活圈', 'RIESGOS Y VECINDARIO'), hx, hy, { px: 13.5 * u, w: 800, color: TH.accent, spacing: 1.4 * u, shadow: 8 * u });
      if (d.headline.risk) txt(ctx, d.headline.risk, hx, hy + 30 * u, { px: 22 * u * ts, w: 800, maxW: tileX1 - hx, minPx: 14 * u, shadow: 10 * u });
      ctx.restore();
    }
    var tiles = d.tiles;
    if (!tiles.length) return;
    var gridTop = isP ? tileY0 + 8 * u + (d.headline.risk ? 4 * u : 0) : Lt.top + (d.headline.risk ? 58 : 30) * u;
    var cols = 2, rowsN = Math.ceil(tiles.length / cols), gap = 12 * u;
    var tw = (tileX1 - tileX0 - gap) / cols;
    var th = Math.min((isP ? 160 : 118) * u, (tileY1 - gridTop - gap * (rowsN - 1)) / rowsN);
    tiles.forEach(function (t, i) {
      var a = enter(st, 1.1 + i * 0.32, 0.6);
      if (a <= 0) return;
      var tx = tileX0 + (i % cols) * (tw + gap), ty = gridTop + Math.floor(i / cols) * (th + gap);
      var col = LEVEL_COL[t.level] || LEVEL_COL.info;
      ctx.save();
      ctx.globalAlpha *= a;
      ctx.translate((1 - a) * 34 * u, 0);
      glass(ctx, tx, ty, tw, th, u, TH.glassTile);
      fillRoundCard(ctx, tx, ty, 6 * u, th, 3 * u, col, null);
      var ir = (isP ? 17 : 19) * u, icx = tx + (isP ? 18 : 22) * u + ir, icy = ty + 20 * u + ir * 0.6;
      ctx.save();
      ctx.fillStyle = col;
      ctx.globalAlpha *= 0.2;
      ctx.beginPath(); ctx.arc(icx, icy, ir, 0, Math.PI * 2); ctx.fill();
      ctx.restore();
      icon(ctx, t.icon, icx, icy, ir * 0.78, inkFor(col));
      var tx2 = icx + ir + (isP ? 10 : 14) * u, tw2 = tx + tw - tx2 - 12 * u;
      var titlePx = 18 * u * ts, factPx = 13.5 * u * ts, lineH = factPx * 1.38;
      txt(ctx, t.title, tx2, ty + 22 * u + titlePx * 0.55, { px: titlePx, w: 800, maxW: tw2, minPx: 12 * u });
      var factTop = ty + 36 * u + titlePx;
      var maxLines = Math.max(1, Math.min(isP ? 4 : 3, Math.floor((ty + th - 10 * u - factTop) / lineH) + 1));
      ctx.save();
      ctx.font = F(600, factPx);
      var lines = wrapLines(ctx, t.fact, tw2, maxLines);
      ctx.restore();
      lines.forEach(function (ln, li) { txt(ctx, ln, tx2, factTop + li * lineH, { px: factPx, w: 600, color: PAL.muted }); });
      ctx.restore();
    });
  }

  /* ------------------------------------------------------------------ Scene 6 · Verdict & what to verify */
  function sceneVerdict(ctx, W, H, st, d, isP) {
    var Lt = layout(W, H, isP), u = Lt.u, m = Lt.m;
    var P = paintAerial(ctx, W, H, camVerdict(st, d, isP), d);
    if (P) drawOutlines(ctx, P, d, st, u, null, 0.5);
    grade(ctx, W, H, 0.68, 0.55);
    var V = d.verdict;
    var tone = inkFor(/good|positive|strong|buy/i.test(V.tone) ? PAL.green : /risk|bad|avoid|negative|caution/i.test(V.tone) ? PAL.red : /watch|mixed|neutral/i.test(V.tone) ? PAL.amber : PAL.teal);
    var x0 = m + (isP ? 0 : 6 * u);
    var ringR = (isP ? 60 : 66) * u;
    var textW = isP ? W - 2 * m : W - 2 * m - ringR * 2 - 60 * u;
    var y = Lt.top + (isP ? 70 : 40) * u;
    var aP = enter(st, 0.2, 0.5);
    if (aP > 0) {
      ctx.save();
      ctx.globalAlpha *= aP;
      pill(ctx, x0, y, L3('VERDICT', '结论', 'VEREDICTO'), { u: u, h: 32 * u, px: 13.5 * u, w: 800, bg: TH.chipBg, border: tone, color: tone, dot: tone });
      ctx.restore();
    }
    var headPx = (isP ? 44 : 50) * u;
    var aV = enter(st, 0.45, 0.7);
    var headLines = [];
    if (V.text) {
      ctx.save();
      ctx.font = F(800, headPx);
      headLines = wrapLines(ctx, V.text, textW, 2);
      ctx.restore();
    }
    if (aV > 0) {
      headLines.forEach(function (ln, i) {
        ctx.save();
        ctx.globalAlpha *= aV;
        txt(ctx, ln, x0, y + 32 * u + headPx * 1.12 * (i + 1) + (1 - aV) * 20 * u, { px: headPx, w: 800, shadow: 18 * u });
        ctx.restore();
      });
    }
    if (isN(V.score)) {
      var aR = enter(st, 0.7, 0.6);
      if (aR > 0) {
        var rcx = isP ? W - m - ringR : W - m - ringR - 10 * u, rcy = isP ? y + 16 * u - ringR * 0.1 + ringR : Lt.top + 30 * u + ringR;
        var prog = easeOutCubic(remap(st, 0.8, 2.2));
        var rc = V.score >= 70 ? PAL.green : V.score >= 50 ? PAL.amber : PAL.red;
        ctx.save();
        ctx.globalAlpha *= aR;
        ctx.lineWidth = 10 * u;
        ctx.strokeStyle = TH.ringTrack;
        ctx.beginPath(); ctx.arc(rcx, rcy, ringR, 0, Math.PI * 2); ctx.stroke();
        ctx.strokeStyle = rc;
        ctx.lineCap = 'round';
        ctx.shadowColor = rc;
        ctx.shadowBlur = 14 * u;
        ctx.beginPath(); ctx.arc(rcx, rcy, ringR, -Math.PI / 2, -Math.PI / 2 + Math.PI * 2 * (V.score / 100) * prog); ctx.stroke();
        ctx.shadowBlur = 0;
        txt(ctx, String(Math.round(V.score * prog)), rcx, rcy + 4 * u, { px: 40 * u, w: 800, align: 'center', base: 'middle' });
        txt(ctx, L3('SCORE / 100', '评分 / 100', 'PUNTOS / 100'), rcx, rcy + ringR + 24 * u, { px: 11.5 * u, w: 800, color: PAL.dim, align: 'center', spacing: 1 * u });
        ctx.restore();
      }
    }
    var listY = y + 32 * u + headPx * 1.12 * Math.max(1, headLines.length) + (isP ? 66 : 54) * u;
    if (isP && isN(V.score)) listY = Math.max(listY, Lt.top + 290 * u);
    if (d.checksFull.length) {
      var aL = enter(st, 1.4, 0.5);
      if (aL > 0) {
        ctx.save();
        ctx.globalAlpha *= aL;
        txt(ctx, L3('BEFORE YOU MAKE AN OFFER', '出价前请核实', 'ANTES DE OFERTAR'), x0, listY, { px: 13.5 * u, w: 800, color: TH.amberInk, spacing: 1.4 * u });
        ctx.restore();
      }
      var iy = listY + 22 * u;
      d.checksFull.slice(0, 3).forEach(function (ck, i) {
        var a = enter(st, 1.7 + i * 0.35, 0.5);
        ctx.save();
        var ipx = (isP ? 24 : 21) * u, ilh = ipx * 1.3;
        ctx.font = F(650, ipx);
        var lines = wrapLines(ctx, ck, W - 2 * m - 54 * u, isP ? 3 : 2);
        ctx.restore();
        if (a > 0) {
          ctx.save();
          ctx.globalAlpha *= a;
          ctx.translate((1 - a) * 22 * u, 0);
          ctx.fillStyle = TH.checkFill;
          ctx.strokeStyle = TH.checkEdge;
          ctx.lineWidth = 2 * u;
          ctx.beginPath(); ctx.arc(x0 + 14 * u, iy + 17 * u, 14 * u, 0, Math.PI * 2); ctx.fill(); ctx.stroke();
          txt(ctx, String(i + 1), x0 + 14 * u, iy + 18 * u, { px: 14 * u, w: 800, color: TH.checkInk, align: 'center', base: 'middle' });
          lines.forEach(function (ln, li) { txt(ctx, ln, x0 + 44 * u, iy + 25 * u + li * ilh, { px: ipx, w: 650, shadow: 10 * u }); });
          ctx.restore();
        }
        iy += Math.max(1, lines.length) * ilh + 22 * u;
      });
    }
    // End card.
    var aEnd = enter(st, 5.3, 0.8);
    if (aEnd > 0) {
      ctx.save();
      ctx.globalAlpha *= aEnd;
      ctx.fillStyle = TH.end;
      ctx.fillRect(0, 0, W, H);
      var cy0 = H * (isP ? 0.42 : 0.4);
      var glow = ctx.createRadialGradient(W / 2, cy0, 0, W / 2, cy0, Math.min(W, H) * 0.55);
      glow.addColorStop(0, TH.endGlow);
      glow.addColorStop(1, 'rgba(20, 184, 166, 0)');
      ctx.fillStyle = glow;
      ctx.fillRect(0, 0, W, H);
      var mk = 66 * u;
      ctx.save();
      ctx.shadowColor = 'rgba(45, 212, 191, 0.5)';
      ctx.shadowBlur = 24 * u;
      fillRoundCard(ctx, W / 2 - mk / 2, cy0 - mk, mk, mk, 16 * u, TH.accentSoft, TH.accentEdge, 2 * u);
      ctx.restore();
      var biV = brandInfo();
      txt(ctx, biV.monogram, W / 2, cy0 - mk / 2 + 1, { px: 28 * u, w: 800, color: TH.accent, align: 'center', base: 'middle' });
      txt(ctx, biV.name, W / 2, cy0 + 58 * u, { px: 44 * u, w: 800, align: 'center' });
      txt(ctx, L3('Full report, maps & sources', '完整报告、地图与数据来源', 'Informe completo, mapas y fuentes'), W / 2, cy0 + 100 * u, { px: 20 * u, w: 600, color: PAL.muted, align: 'center' });
      txt(ctx, reportUrl(d), W / 2, cy0 + 138 * u, { px: 22 * u, w: 800, color: TH.accent, align: 'center', maxW: W - 2 * m, minPx: 12 * u });
      txt(ctx, L3('Automated research summary from public data — not an appraisal or inspection.', '基于公开数据的自动研究摘要——非正式估价或验房结论。', 'Resumen automático con datos públicos: no es un avalúo ni una inspección.'),
        W / 2, H - m - 20 * u, { px: 13.5 * u, w: 600, color: PAL.dim, align: 'center', maxW: W - 2 * m, minPx: 9 * u });
      ctx.restore();
    }
  }
  function reportUrl(d) {
    var bi = brandInfo();
    var host = bi.domain;
    try {
      var h = typeof location !== 'undefined' && location.host ? String(location.host) : '';
      if (h && !/^(localhost|127\.0\.0\.1)(:|$)/i.test(h)) host = h.replace(/^www\./i, '');
    } catch (e) {}
    return host + (d.slug ? '/r/' + d.slug : '');
  }

  /* ------------------------------------------------------------------ Global layers */
  var DRAW = { arrival: sceneArrival, lot: sceneLot, value: sceneValue, cost: sceneCost, risk: sceneRisk, verdict: sceneVerdict };
  var SCENE_CAM = [camArrival, camLot, camValue, camCost, camRisk, camVerdict];
  function chapterName(idx) {
    return [
      L3('THE HOME', '房屋', 'LA CASA'),
      L3('THE LOT', '地块', 'EL LOTE'),
      L3('VALUE', '估值', 'VALOR'),
      L3('COST TO OWN', '持有成本', 'COSTO MENSUAL'),
      L3('RISKS & NEIGHBORHOOD', '风险与生活圈', 'RIESGOS Y VECINDARIO'),
      L3('VERDICT', '结论', 'VEREDICTO')
    ][idx] || '';
  }
  function drawHud(ctx, W, H, d, idx, isP, a) {
    if (a <= 0) return;
    var u = Math.min(W, H) / 720, x = Math.round(34 * u), y = Math.round(28 * u);
    ctx.save();
    ctx.globalAlpha *= a;
    var tg = ctx.createLinearGradient(0, 0, 0, 120 * u);
    tg.addColorStop(0, 'rgba(' + TH.wash + ', ' + TH.hudA + ')');
    tg.addColorStop(1, 'rgba(' + TH.wash + ', 0)');
    ctx.fillStyle = tg;
    ctx.fillRect(0, 0, W, 120 * u);
    fillRoundCard(ctx, x, y, 32 * u, 32 * u, 8 * u, TH.accentSoft, TH.accentEdge, 1.5 * u);
    var biH = brandInfo();
    txt(ctx, biH.monogram, x + 16 * u, y + 17 * u, { px: 13.5 * u, w: 800, color: TH.accent, align: 'center', base: 'middle' });
    txt(ctx, biH.name, x + 44 * u, y + 14 * u, { px: 15 * u, w: 800, shadow: 6 * u });
    var addr = d.street + (d.cityLine ? ' · ' + d.cityLine : '');
    txt(ctx, addr, x + 44 * u, y + 32 * u, { px: 12.5 * u, w: 600, color: PAL.muted, maxW: isP ? W - x * 2 - 50 * u : W * 0.45, minPx: 9 * u, shadow: 6 * u });
    var lbl = (idx + 1 < 10 ? '0' : '') + (idx + 1) + ' / 0' + SCENE_COUNT + ' · ' + chapterName(idx);
    ctx.font = F(800, 12.5 * u);
    var pw = ctx.measureText(lbl).width + 30 * u;
    var px = isP ? x : W - x - pw, py = isP ? y + 50 * u : y + 2 * u;
    pill(ctx, px, py, lbl, { u: u, h: 30 * u, px: 12.5 * u, w: 800, bg: TH.hudPill, border: TH.hudPillEdge, color: TH.hudPillInk });
    ctx.restore();
  }
  function drawCaption(ctx, W, H, text, a, isP) {
    if (!text || a <= 0) return;
    var u = Math.min(W, H) / 720, m = 44 * u;
    var px = (isP ? 25 : 22) * u, maxW = isP ? W - 2 * m : Math.min(W - 2 * m, 1000 * u);
    ctx.save();
    ctx.globalAlpha *= a;
    ctx.font = F(600, px);
    var lines = wrapLines(ctx, text, maxW - 44 * u, isP ? 3 : 2);
    var lw = 0;
    lines.forEach(function (ln) { lw = Math.max(lw, ctx.measureText(ln).width); });
    var lh = px * 1.36, bw = lw + 44 * u, bh = lines.length * lh + 22 * u;
    var bx = (W - bw) / 2, by = H - (isP ? 40 : 26) * u - bh;
    fillRoundCard(ctx, bx, by, bw, bh, 14 * u, TH.caption, TH.captionEdge, 1);
    lines.forEach(function (ln, i) { txt(ctx, ln, W / 2, by + 11 * u + lh * (i + 0.5) + 1, { px: px, w: 600, align: 'center', base: 'middle' }); });
    ctx.restore();
  }
  function drawLoading(ctx, W, H, t) {
    var u = Math.min(W, H) / 720;
    ctx.save();
    ctx.fillStyle = TH.loading;
    ctx.fillRect(0, 0, W, H);
    ctx.strokeStyle = 'rgba(45, 212, 191, 0.25)';
    ctx.lineWidth = 4 * u;
    ctx.beginPath(); ctx.arc(W / 2, H / 2 - 20 * u, 22 * u, 0, Math.PI * 2); ctx.stroke();
    ctx.strokeStyle = '#2dd4bf';
    ctx.lineCap = 'round';
    var a0 = (t * 5) % (Math.PI * 2);
    ctx.beginPath(); ctx.arc(W / 2, H / 2 - 20 * u, 22 * u, a0, a0 + Math.PI * 0.6); ctx.stroke();
    txt(ctx, L3('Loading aerial imagery…', '正在加载航拍影像…', 'Cargando imágenes aéreas…'), W / 2, H / 2 + 30 * u, { px: 17 * u, w: 700, color: PAL.soft, align: 'center' });
    ctx.restore();
  }

  /* ------------------------------------------------------------------ Frame rendering */
  function getLogicalDims() {
    return state.aspect === '9:16' ? { W: 720, H: 1280 } : { W: 1280, H: 720 };
  }
  /** Canvas pixel density: the screen's (capped at 1.5); exports are always 1080p. */
  function screenDpr() {
    if (state.recording) return 1.5;
    return typeof window.devicePixelRatio === 'number' ? Math.min(1.5, Math.max(1, window.devicePixelRatio)) : 1;
  }
  function syncCanvasResolution() {
    var canvas = byId('motionReelCanvas');
    if (!canvas) return;
    var dims = getLogicalDims();
    var dpr = screenDpr();
    var pw = Math.round(dims.W * dpr);
    var ph = Math.round(dims.H * dpr);
    if (canvas.width !== pw) canvas.width = pw;
    if (canvas.height !== ph) canvas.height = ph;
    canvas._dpr = dpr;
    state.dpr = dpr;
    syncCanvasDisplaySize();
  }
  function syncCanvasDisplaySize() {
    var canvas = byId('motionReelCanvas');
    var stage = byId('motionReelStage');
    if (!canvas || !stage || typeof stage.getBoundingClientRect !== 'function') return;
    var rect = stage.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0) return;
    var dims = getLogicalDims();
    var availW = Math.max(120, rect.width - 18);
    var availH = Math.max(120, rect.height - 18);
    var scale = Math.min(availW / dims.W, availH / dims.H);
    canvas.style.width = Math.floor(dims.W * scale) + 'px';
    canvas.style.height = Math.floor(dims.H * scale) + 'px';
  }
  function captionAlpha(idx, st) {
    var dur = SCENES[idx].dur;
    var a = enter(st, 0.55, 0.45) * (1 - remap(st, dur - 0.4, dur));
    if (SCENES[idx].key === 'verdict') a *= 1 - remap(st, 4.9, 5.4);
    return a;
  }
  function renderFrame(t) {
    var canvas = byId('motionReelCanvas');
    if (!canvas || typeof canvas.getContext !== 'function') return;
    var ctx = canvas.getContext('2d');
    if (!ctx) return;
    var dims = getLogicalDims();
    var W = dims.W, H = dims.H;
    var dpr = canvas._dpr || 1;
    var isP = state.aspect === '9:16';
    var d = state.data || extractReelData(getRep());
    state.data = d;
    var s = sceneAt(t), idx = s.idx, st = s.st;

    ctx.save();
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.globalAlpha = 1;
    try {
      // Dissolve: the previous scene (already settled) underneath, the new one fading in on top.
      if (st < TRANS && idx > 0) {
        var pi = idx - 1;
        DRAW[SCENES[pi].key](ctx, W, H, SCENES[pi].dur + st, d, isP);
        ctx.globalAlpha = easeInOutCubic(st / TRANS);
      }
      DRAW[SCENES[idx].key](ctx, W, H, st, d, isP);
    } catch (e) {
      if (window.console && console.warn) console.warn('motion video: frame failed', e);
    }
    ctx.globalAlpha = 1;
    var endA = SCENES[idx].key === 'verdict' ? enter(st, 5.3, 0.8) : 0;
    drawCaption(ctx, W, H, d.captions && d.captions[idx], captionAlpha(idx, st), isP);
    drawHud(ctx, W, H, d, idx, isP, 1 - endA);
    if (d.hasPoint && endA < 1) {
      ctx.save();
      ctx.globalAlpha = 1 - endA;
      var satAttr = (d.sat && d.sat.attribution) || USGS_ATTR;
      txt(ctx, satAttr, W - 12 * (Math.min(W, H) / 720), H - 10 * (Math.min(W, H) / 720), { px: 10 * (Math.min(W, H) / 720), w: 600, color: TH.attribution, align: 'right', shadow: 4 });
      ctx.restore();
    }
    var pct = s.wrapped / TOTAL_DURATION;
    var u = Math.min(W, H) / 720;
    ctx.fillStyle = TH.progressTrack;
    ctx.fillRect(0, H - 4 * u, W, 4 * u);
    ctx.fillStyle = PAL.tealBright;
    ctx.fillRect(0, H - 4 * u, W * pct, 4 * u);
    if (state.waitingSince && imageryPending(d) && Date.now() - state.waitingSince <= 3500) drawLoading(ctx, W, H, (Date.now() - state.waitingSince) / 1000);
    ctx.restore();

    if (idx !== state.lastSceneIdx) {
      var prev = state.lastSceneIdx;
      state.lastSceneIdx = idx;
      if (prev !== -1 && state.playing && !state.waitingSince) {
        AudioEngine.whoosh();
        if (SCENES[idx].key === 'verdict') AudioEngine.finaleChord();
      }
      updateModalUiState();
    }
    updateProgressUi(s.wrapped, idx);
  }
  function updateProgressUi(wrappedT, sceneIdx) {
    for (var i = 0; i < SCENE_COUNT; i++) {
      var bar = byId('motionSegFill_' + i);
      if (bar && bar.style) {
        var fill = i < sceneIdx ? 100 : i > sceneIdx ? 0 : clamp(((wrappedT - SCENE_START[i]) / SCENES[i].dur) * 100, 0, 100);
        bar.style.width = fill.toFixed(1) + '%';
      }
      var pillEl = byId('motionScenePill_' + i);
      if (pillEl && pillEl.classList) pillEl.classList.toggle('on', i === sceneIdx);
    }
    var clk = byId('motionTimeBadge');
    if (clk) clk.textContent = fmtClock(wrappedT) + ' / ' + fmtClock(TOTAL_DURATION);
    if (state.recording) {
      var expBtn = byId('motionExportBtn');
      var pct = Math.min(99, Math.round((wrappedT / TOTAL_DURATION) * 100));
      if (expBtn) expBtn.textContent = '● REC ' + pct + '% · ' + L3('Finish', '完成导出', 'Finalizar');
    }
  }
  function tick(nowMs) {
    if (!state.open || !state.playing) {
      state.rafId = 0;
      return;
    }
    // Guard against synchronous, non-advancing requestAnimationFrame in headless / Node DOM mocks.
    if (typeof nowMs !== 'number' || !isFinite(nowMs)) {
      renderFrame(state.t);
      state.rafId = 0;
      return;
    }
    // Hold the first frame (with a loading spinner) until the imagery is in, at most ~3.5 s.
    if (state.waitingSince) {
      if (!imageryPending(state.data) || Date.now() - state.waitingSince > 3500) {
        state.waitingSince = 0;
        state.lastSceneIdx = -1;
        state.lastRafMs = 0;
      }
      renderFrame(state.t);
      if (typeof window.requestAnimationFrame === 'function') state.rafId = window.requestAnimationFrame(tick);
      return;
    }
    if (state.lastRafMs > 0) {
      var rawDt = (nowMs - state.lastRafMs) / 1000;
      if (rawDt <= 0) {
        state.zeroDtStreak = (state.zeroDtStreak || 0) + 1;
        if (state.zeroDtStreak > 4) {
          state.rafId = 0;
          return;
        }
        state.lastRafMs = nowMs;
        if (typeof window.requestAnimationFrame === 'function') state.rafId = window.requestAnimationFrame(tick);
        return;
      }
      state.zeroDtStreak = 0;
      var dt = Math.min(0.1, rawDt);
      state.t += dt * state.speed;
      if (state.t >= TOTAL_DURATION) {
        if (state.recording) {
          state.t = TOTAL_DURATION - 0.01;
          renderFrame(state.t);
          stopExportWebm(true);
          return;
        }
        state.t = state.t % TOTAL_DURATION;
      }
    }
    state.lastRafMs = nowMs;
    renderFrame(state.t);
    if (typeof window.requestAnimationFrame === 'function') state.rafId = window.requestAnimationFrame(tick);
  }
  function startLoop() {
    if (state.rafId && typeof window.cancelAnimationFrame === 'function') {
      try { window.cancelAnimationFrame(state.rafId); } catch (e) {}
      state.rafId = 0;
    }
    state.lastRafMs = 0;
    state.zeroDtStreak = 0;
    if (state.open && state.playing && typeof window.requestAnimationFrame === 'function') {
      state.rafId = window.requestAnimationFrame(tick);
    }
  }

  /* ------------------------------------------------------------------ .webm export (1080p) */
  function pickSupportedMimeType() {
    if (typeof MediaRecorder === 'undefined' || typeof MediaRecorder.isTypeSupported !== 'function') return '';
    var cands = ['video/webm;codecs=vp9,opus', 'video/webm;codecs=vp9', 'video/webm;codecs=vp8,opus', 'video/webm;codecs=vp8', 'video/webm'];
    for (var i = 0; i < cands.length; i++) {
      try { if (MediaRecorder.isTypeSupported(cands[i])) return cands[i]; } catch (e) {}
    }
    return '';
  }
  function startExportWebm() {
    var canvas = byId('motionReelCanvas');
    if (!canvas || typeof canvas.captureStream !== 'function' || typeof MediaRecorder === 'undefined') {
      var stEl = byId('motionExportStatus');
      if (stEl) stEl.textContent = L3('Video export requires Chrome/Edge/Firefox canvas capture.', '当前浏览器暂不支持录制导出 .webm 视频。', 'La exportación requiere Chrome/Edge/Firefox.');
      return;
    }
    if (state.recording) {
      stopExportWebm(true);
      return;
    }
    // Record only once every image of the 1080p cut is in, so the file never shows a loading frame.
    preloadImagery(state.data, 1.5);
    if (imageryPending(state.data, true)) {
      var stEl2 = byId('motionExportStatus');
      if (stEl2) stEl2.textContent = L3('Loading imagery…', '正在加载影像…', 'Cargando imágenes…');
      var t0 = Date.now();
      var wait = function () {
        if (!imageryPending(state.data, true) || Date.now() - t0 > 15000) {
          if (stEl2) stEl2.textContent = '';
          beginRecording(canvas);
        } else setTimeout(wait, 150);
      };
      wait();
      return;
    }
    beginRecording(canvas);
  }
  function beginRecording(canvas) {
    state.recording = true;
    state.recChunks = [];
    state.speed = 1;
    state.t = 0;
    state.lastSceneIdx = -1;
    state.playing = true;
    state.waitingSince = 0;
    syncCanvasResolution();
    renderFrame(0);
    try {
      var stream = canvas.captureStream(60);
      if (state.audioOn && AudioEngine.ensure() && AudioEngine.dest && AudioEngine.dest.stream) {
        try { AudioEngine.dest.stream.getAudioTracks().forEach(function (trk) { stream.addTrack(trk); }); } catch (eAud) {}
      }
      var mime = pickSupportedMimeType();
      var opts = mime ? { mimeType: mime, videoBitsPerSecond: 9000000 } : { videoBitsPerSecond: 9000000 };
      var rec = new MediaRecorder(stream, opts);
      rec.ondataavailable = function (ev) {
        if (ev && ev.data && ev.data.size > 0) state.recChunks.push(ev.data);
      };
      rec.onstop = function () {
        var hadChunks = state.recChunks.length > 0;
        if (hadChunks && typeof Blob === 'function' && typeof URL !== 'undefined' && typeof URL.createObjectURL === 'function') {
          var blob = new Blob(state.recChunks, { type: mime || 'video/webm' });
          var url = URL.createObjectURL(blob);
          var a = document.createElement('a');
          var slug = (state.data && state.data.slug) || 'openestate-property';
          a.href = url;
          a.download = slug + '-video-walkthrough.webm';
          if (document.body && typeof document.body.appendChild === 'function') document.body.appendChild(a);
          if (typeof a.click === 'function') a.click();
          setTimeout(function () {
            try { if (a.parentNode) a.parentNode.removeChild(a); URL.revokeObjectURL(url); } catch (e) {}
          }, 1500);
        }
        state.recChunks = [];
        state.recording = false;
        state.recorder = null;
        syncCanvasResolution();
        updateModalUiState();
      };
      state.recorder = rec;
      state.recStartedAt = Date.now();
      rec.start(250);
      updateModalUiState();
      startLoop();
    } catch (e) {
      state.recording = false;
      state.recorder = null;
      syncCanvasResolution();
      updateModalUiState();
    }
  }
  function stopExportWebm(triggerDownload) {
    if (!state.recording || !state.recorder) {
      state.recording = false;
      return;
    }
    try {
      if (!triggerDownload) state.recChunks = [];
      if (state.recorder.state !== 'inactive') state.recorder.stop();
    } catch (e) {
      state.recording = false;
      state.recorder = null;
      updateModalUiState();
    }
  }

  /* ------------------------------------------------------------------ Modal shell & controls */
  function ensureModalElement() {
    var modal = byId('motionReelModal');
    if (modal) return modal;
    if (!document.body || typeof document.createElement !== 'function') return null;
    modal = document.createElement('div');
    modal.id = 'motionReelModal';
    modal.className = 'db-motion-bg';
    modal.hidden = true;
    document.body.appendChild(modal);
    return modal;
  }
  function sceneTitles() {
    return [
      L3('1 · Home', '1 · 房屋', '1 · Casa'),
      L3('2 · Lot', '2 · 地块', '2 · Lote'),
      L3('3 · Value', '3 · 估值', '3 · Valor'),
      L3('4 · Cost', '4 · 持有成本', '4 · Costo'),
      L3('5 · Risks', '5 · 风险与生活圈', '5 · Riesgos'),
      L3('6 · Verdict', '6 · 结论', '6 · Veredicto')
    ];
  }
  function updateModalUiState() {
    var playBtn = byId('motionPlayBtn');
    if (playBtn) playBtn.textContent = state.playing ? '⏸ ' + L3('Pause', '暂停', 'Pausa') : '▶ ' + L3('Play', '播放', 'Reproducir');
    var spdBtn = byId('motionSpeedBtn');
    if (spdBtn) spdBtn.textContent = state.speed + 'x';
    var aspBtn = byId('motionAspectBtn');
    if (aspBtn) {
      aspBtn.textContent = state.aspect === '9:16'
        ? '📱 9:16 ' + L3('Story', '竖屏短视频', 'Historia')
        : '🖥️ 16:9 ' + L3('Cinema', '宽屏影院', 'Cine');
    }
    var audBtn = byId('motionAudioBtn');
    if (audBtn) {
      audBtn.textContent = state.audioOn ? '🔊 ' + L3('Sound On', '音效开', 'Sonido') : '🔇 ' + L3('Sound Off', '静音', 'Silencio');
      if (audBtn.classList) audBtn.classList.toggle('on', state.audioOn);
    }
    var expBtn = byId('motionExportBtn');
    if (expBtn) {
      if (expBtn.classList) expBtn.classList.toggle('rec', state.recording);
      if (!state.recording) expBtn.textContent = '⬇ ' + L3('Export Video (1080p .webm)', '导出视频（1080p .webm）', 'Exportar Video (1080p .webm)');
    }
    var curL = langCode();
    qAll('[data-motion-lang]').forEach(function (b) {
      if (b.classList) b.classList.toggle('on', b.getAttribute('data-motion-lang') === curL);
    });
  }
  function renderModalChrome() {
    var modal = ensureModalElement();
    if (!modal) return;
    var curL = langCode();
    var titles = sceneTitles();
    var segsHtml = titles.map(function (_, i) {
      return '<div class="db-motion-seg" data-motion-seg="' + i + '" role="button" tabindex="0" aria-label="Jump to scene ' + (i + 1) + '">' +
        '<div class="db-motion-seg-fill" id="motionSegFill_' + i + '"></div>' +
      '</div>';
    }).join('');
    var pillsHtml = titles.map(function (lbl, i) {
      return '<button type="button" class="db-motion-scbtn' + (i === Math.max(0, state.lastSceneIdx) ? ' on' : '') + '" id="motionScenePill_' + i + '" data-motion-scene="' + i + '">' + esc(lbl) + '</button>';
    }).join('');
    var cols = SCENES.map(function (s) { return s.dur + 'fr'; }).join(' ');

    modal.innerHTML =
      '<div class="db-motion-dialog" role="dialog" aria-modal="true" aria-label="Property video walkthrough">' +
        '<div class="db-motion-stories" id="motionStoryBar" style="grid-template-columns:' + cols + '">' + segsHtml + '</div>' +
        '<div class="db-motion-top">' +
          '<div class="db-motion-top-l">' +
            '<span class="db-motion-live">● HD</span>' +
            '<span class="db-motion-time" id="motionTimeBadge">0:00 / ' + fmtClock(TOTAL_DURATION) + '</span>' +
            '<button type="button" class="db-motion-chip" id="motionAspectBtn"></button>' +
          '</div>' +
          '<div class="db-motion-top-r">' +
            '<div class="db-motion-lang">' +
              '<button type="button" data-motion-lang="en" class="' + (curL === 'en' ? 'on' : '') + '">EN</button>' +
              '<button type="button" data-motion-lang="zh" class="' + (curL === 'zh' ? 'on' : '') + '">中文</button>' +
              '<button type="button" data-motion-lang="es" class="' + (curL === 'es' ? 'on' : '') + '">ES</button>' +
            '</div>' +
            '<button type="button" class="db-motion-chip" id="motionAudioBtn"></button>' +
            '<button type="button" class="db-motion-close" id="motionCloseBtn" aria-label="Close video">✕</button>' +
          '</div>' +
        '</div>' +
        '<div class="db-motion-stage" id="motionReelStage">' +
          '<canvas id="motionReelCanvas" class="db-motion-canvas" width="1280" height="720"></canvas>' +
        '</div>' +
        '<div class="db-motion-foot">' +
          '<div class="db-motion-controls">' +
            '<button type="button" class="db-motion-pri" id="motionPlayBtn"></button>' +
            '<button type="button" class="db-motion-chip" id="motionSpeedBtn"></button>' +
            '<div class="db-motion-scenes">' + pillsHtml + '</div>' +
          '</div>' +
          '<div class="db-motion-export-wrap">' +
            '<span class="db-motion-status" id="motionExportStatus"></span>' +
            '<button type="button" class="db-motion-export" id="motionExportBtn"></button>' +
          '</div>' +
        '</div>' +
      '</div>';

    var cBtn = byId('motionCloseBtn');
    if (cBtn) cBtn.addEventListener('click', closeMotionReel);
    var pBtn = byId('motionPlayBtn');
    if (pBtn) pBtn.addEventListener('click', togglePlay);
    var spBtn = byId('motionSpeedBtn');
    if (spBtn) spBtn.addEventListener('click', function () {
      state.speed = state.speed >= 1.5 ? 1 : 1.5;
      updateModalUiState();
    });
    var asBtn = byId('motionAspectBtn');
    if (asBtn) asBtn.addEventListener('click', function () { setAspect(state.aspect === '9:16' ? '16:9' : '9:16'); });
    var auBtn = byId('motionAudioBtn');
    if (auBtn) auBtn.addEventListener('click', function () {
      state.audioOn = !state.audioOn;
      if (state.audioOn) AudioEngine.ensure();
      updateModalUiState();
    });
    var exBtn = byId('motionExportBtn');
    if (exBtn) exBtn.addEventListener('click', startExportWebm);
    qAll('[data-motion-scene]', modal).forEach(function (btn) {
      btn.addEventListener('click', function () { seekScene(+(btn.getAttribute('data-motion-scene') || 0)); });
    });
    qAll('[data-motion-seg]', modal).forEach(function (seg) {
      seg.addEventListener('click', function (ev) {
        var idx = clamp(+(seg.getAttribute('data-motion-seg') || 0), 0, SCENE_COUNT - 1);
        var frac = 0;
        if (ev && typeof seg.getBoundingClientRect === 'function') {
          var r = seg.getBoundingClientRect();
          if (r && r.width > 0) frac = clamp((ev.clientX - r.left) / r.width, 0, 0.96);
        }
        seekTime(SCENE_START[idx] + frac * SCENES[idx].dur);
      });
    });
    qAll('[data-motion-lang]', modal).forEach(function (btn) {
      btn.addEventListener('click', function () { setReelLang(btn.getAttribute('data-motion-lang') || 'en'); });
    });
    modal.onclick = function (ev) { if (ev && ev.target === modal) closeMotionReel(); };
    updateModalUiState();
    syncCanvasResolution();
  }
  /** Applies the system (or pinned) theme to the canvas and the modal chrome. */
  function syncTheme() {
    applyTheme(state.themeOverride || systemTheme());
    var modal = byId('motionReelModal');
    if (modal && modal.classList) {
      modal.classList.toggle('db-motion-light', TH.mode === 'light');
      modal.classList.toggle('db-motion-dark', TH.mode === 'dark');
    }
    if (state.open) renderFrame(state.t);
    return TH.mode;
  }
  function openMotionReel(startSceneIdx) {
    if (window.OpenEstateWorkspace && typeof window.OpenEstateWorkspace.closeWalkthrough === 'function') {
      try { window.OpenEstateWorkspace.closeWalkthrough(); } catch (e) {}
    }
    var plb = byId('photoLightboxModal');
    if (plb && !plb.hidden) {
      plb.hidden = true;
      if (plb.classList) plb.classList.remove('open');
    }
    var modal = ensureModalElement();
    if (!modal) return;
    syncTheme();
    state.data = extractReelData(getRep());
    state.open = true;
    state.playing = true;
    state.t = isN(+startSceneIdx) ? SCENE_START[clamp(+startSceneIdx, 0, SCENE_COUNT - 1)] : 0;
    state.lastSceneIdx = -1;
    state.aspect = (typeof window.innerWidth === 'number' && window.innerWidth <= 640) ? '9:16' : state.aspect || '16:9';
    preloadImagery(state.data);
    state.waitingSince = imageryPending(state.data) ? Date.now() : 0;
    modal.hidden = false;
    if (modal.classList) modal.classList.add('open');
    renderModalChrome();
    AudioEngine.ensure();
    renderFrame(state.t);
    startLoop();
  }
  function closeMotionReel() {
    if (state.recording) stopExportWebm(false);
    state.open = false;
    state.playing = false;
    state.waitingSince = 0;
    if (state.rafId && typeof window.cancelAnimationFrame === 'function') {
      try { window.cancelAnimationFrame(state.rafId); } catch (e) {}
      state.rafId = 0;
    }
    var modal = byId('motionReelModal');
    if (modal) {
      modal.hidden = true;
      if (modal.classList) modal.classList.remove('open');
      modal.innerHTML = '';
    }
  }
  function togglePlay() {
    state.playing = !state.playing;
    if (state.playing) {
      AudioEngine.ensure();
      startLoop();
    }
    updateModalUiState();
  }
  function seekTime(sec) {
    state.t = clamp(+sec || 0, 0, TOTAL_DURATION - 0.05);
    state.lastSceneIdx = -1;
    renderFrame(state.t);
    if (state.open && state.playing && !state.rafId) startLoop();
  }
  function seekScene(idx) {
    var i = clamp(idx, 0, SCENE_COUNT - 1);
    var offset = Math.min(state.playing ? 2.2 : 3.2, SCENES[i].dur - 0.5);
    seekTime(SCENE_START[i] + offset);
  }
  function setAspect(nextAsp) {
    state.aspect = nextAsp === '9:16' ? '9:16' : '16:9';
    syncCanvasResolution();
    if (state.open && state.data) preloadImagery(state.data);
    updateModalUiState();
    if (state.open) renderFrame(state.t);
  }
  function setReelLang(nextL) {
    var norm = normLang(nextL);
    state.langOverride = norm;
    var finish = function () {
      state.langOverride = null;
      state.data = extractReelData(getRep());
      if (state.open) {
        preloadImagery(state.data);
        renderModalChrome();
        renderFrame(state.t);
      }
      injectEntryButtons();
    };
    if (typeof window.setLang === 'function') {
      Promise.resolve().then(function () { return window.setLang(norm); }).then(finish, function () {
        if (document.documentElement) document.documentElement.lang = norm === 'zh' ? 'zh-CN' : norm;
        finish();
      });
    } else {
      if (document.documentElement) document.documentElement.lang = norm === 'zh' ? 'zh-CN' : norm;
      finish();
    }
  }

  /* ------------------------------------------------------------------ Entry points & runtime CSS */
  function ensureStyles() {
    if (byId('dbMotionStyles')) return;
    var host = document.head || document.body || document.documentElement;
    if (!host || typeof document.createElement !== 'function' || typeof host.appendChild !== 'function') return;
    var st = document.createElement('style');
    st.id = 'dbMotionStyles';
    st.textContent =
      '.hm-tab.hm-motion{background:#f0fdfa;border-color:#5eead4;color:#0f766e;font-weight:800}' +
      '.hm-tab.hm-motion:hover{background:#0f766e;color:#fff;border-color:#0f766e}' +
      '.db-chap-actions{display:inline-flex;align-items:center;gap:6px;flex:0 0 auto}' +
      '.db-chap-motion-btn{display:inline-flex;align-items:center;gap:5px;border:1px solid #99f6e4;background:#f0fdfa;color:#0f766e;border-radius:999px;padding:6px 11px;font:800 11.6px var(--font,Inter,system-ui,sans-serif);cursor:pointer;white-space:nowrap;flex:0 0 auto;box-shadow:0 2px 8px -4px rgba(15,118,110,.35)}' +
      '.db-chap-motion-btn:hover{background:#0f766e;color:#fff;border-color:#0f766e}' +
      '.db-wt-actions{display:inline-flex;align-items:center;gap:8px;flex-wrap:wrap}' +
      '.db-banner-motion-btn{border:1px solid #5eead4;background:#fff;color:#0f766e;border-radius:11px;padding:10px 15px;font:800 13px var(--font,Inter,system-ui,sans-serif);cursor:pointer;white-space:nowrap;box-shadow:0 6px 16px -8px rgba(15,118,110,.4)}' +
      '.db-banner-motion-btn:hover{background:#0f766e;color:#fff;border-color:#0f766e}' +
      '.db-wt-motion-btn{border:1px solid #5eead4;background:#f0fdfa;color:#0f766e;border-radius:999px;padding:6px 12px;font:800 12px var(--font,Inter,system-ui,sans-serif);cursor:pointer;min-height:36px;white-space:nowrap}' +
      '.db-wt-motion-btn:hover{background:#0f766e;color:#fff}' +
      '@media (prefers-color-scheme: dark){' +
        'html:not([data-theme="light"]) .db-chap-motion-btn,html:not([data-theme="light"]) .hm-tab.hm-motion,html:not([data-theme="light"]) .db-banner-motion-btn{background:var(--brandsoft,rgba(20,184,166,.16));border-color:rgba(45,212,191,.42);color:#5eead4;box-shadow:0 2px 10px -4px rgba(0,0,0,.5)}' +
        'html:not([data-theme="light"]) .db-chap-motion-btn:hover,html:not([data-theme="light"]) .hm-tab.hm-motion:hover,html:not([data-theme="light"]) .db-banner-motion-btn:hover{background:#0f766e;color:#fff;border-color:#14b8a6}' +
      '}' +
      'html[data-theme="dark"] .db-chap-motion-btn,html[data-theme="dark"] .hm-tab.hm-motion,html[data-theme="dark"] .db-banner-motion-btn{background:var(--brandsoft,rgba(20,184,166,.16));border-color:rgba(45,212,191,.42);color:#5eead4;box-shadow:0 2px 10px -4px rgba(0,0,0,.5)}' +
      'html[data-theme="dark"] .db-chap-motion-btn:hover,html[data-theme="dark"] .hm-tab.hm-motion:hover,html[data-theme="dark"] .db-banner-motion-btn:hover{background:#0f766e;color:#fff;border-color:#14b8a6}' +
      '.db-motion-bg{position:fixed;inset:0;z-index:3600;background:rgba(2,6,23,.88);-webkit-backdrop-filter:blur(8px);backdrop-filter:blur(8px);display:grid;place-items:center;padding:12px}' +
      '.db-motion-bg[hidden]{display:none!important}' +
      '.db-motion-dialog{width:min(1180px,100%);height:min(880px,calc(100dvh - 24px));max-height:calc(100dvh - 24px);background:#070e1c;border:1px solid rgba(148,163,184,.24);border-radius:18px;display:flex;flex-direction:column;overflow:hidden;box-shadow:0 30px 80px -12px rgba(0,0,0,.75);color:#f8fafc}' +
      '.db-motion-stories{display:grid;grid-template-columns:repeat(' + SCENE_COUNT + ',1fr);gap:6px;padding:10px 14px 4px;background:#091224}' +
      '.db-motion-seg{height:5px;border-radius:999px;background:rgba(148,163,184,.22);overflow:hidden;cursor:pointer}' +
      '.db-motion-seg-fill{height:100%;width:0%;background:linear-gradient(90deg,#14b8a6,#38bdf8);border-radius:999px}' +
      '.db-motion-top{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;padding:7px 14px;background:#091224;border-bottom:1px solid rgba(148,163,184,.14)}' +
      '.db-motion-top-l,.db-motion-top-r{display:flex;align-items:center;gap:7px;flex-wrap:wrap}' +
      '.db-motion-live{font:800 10.5px var(--font,Inter,sans-serif);color:#5eead4;background:rgba(20,184,166,.16);border:1px solid rgba(45,212,191,.35);padding:3px 8px;border-radius:999px;letter-spacing:.4px}' +
      '.db-motion-time{font:700 12px ui-monospace,SFMono-Regular,Menlo,monospace;color:#cbd5e1;font-variant-numeric:tabular-nums}' +
      '.db-motion-chip{border:1px solid rgba(148,163,184,.26);background:rgba(255,255,255,.06);color:#e2e8f0;border-radius:999px;padding:5px 10px;font:700 11.5px var(--font,Inter,sans-serif);cursor:pointer;white-space:nowrap}' +
      '.db-motion-chip:hover,.db-motion-chip.on{border-color:#2dd4bf;background:rgba(20,184,166,.2);color:#5eead4}' +
      '.db-motion-lang{display:inline-flex;background:rgba(255,255,255,.07);border-radius:999px;padding:2px;gap:2px}' +
      '.db-motion-lang button{border:0;background:transparent;border-radius:999px;padding:4px 8px;font:700 11px var(--font,Inter,sans-serif);color:#94a3b8;cursor:pointer}' +
      '.db-motion-lang button.on{background:#14b8a6;color:#042f2e}' +
      '.db-motion-close{width:34px;height:34px;border-radius:9px;border:1px solid rgba(148,163,184,.28);background:rgba(255,255,255,.07);color:#f8fafc;font:800 15px sans-serif;cursor:pointer;display:grid;place-items:center}' +
      '.db-motion-stage{position:relative;flex:1 1 auto;min-height:0;display:flex;align-items:center;justify-content:center;background:#040812;padding:8px;overflow:hidden}' +
      '.db-motion-canvas{display:block;max-width:100%;max-height:100%;border-radius:12px;box-shadow:0 20px 50px -12px rgba(0,0,0,.7),0 0 0 1px rgba(148,163,184,.16);background:#060d18}' +
      '.db-motion-foot{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;padding:9px 14px;background:#091224;border-top:1px solid rgba(148,163,184,.14)}' +
      '.db-motion-controls{display:flex;align-items:center;gap:7px;flex-wrap:wrap;min-width:0}' +
      '.db-motion-pri{border:0;background:linear-gradient(135deg,#0f766e,#14b8a6);color:#042f2e;border-radius:999px;padding:7px 14px;font:800 12px var(--font,Inter,sans-serif);cursor:pointer;white-space:nowrap}' +
      '.db-motion-scenes{display:flex;align-items:center;gap:4px;overflow-x:auto;scrollbar-width:none;max-width:100%}' +
      '.db-motion-scenes::-webkit-scrollbar{display:none}' +
      '.db-motion-scbtn{border:1px solid rgba(148,163,184,.2);background:rgba(255,255,255,.04);color:#94a3b8;border-radius:999px;padding:4.5px 9px;font:700 11px var(--font,Inter,sans-serif);cursor:pointer;white-space:nowrap;flex:0 0 auto}' +
      '.db-motion-scbtn.on{background:#14b8a6;border-color:#2dd4bf;color:#042f2e}' +
      '.db-motion-export-wrap{display:flex;align-items:center;gap:8px;flex-wrap:wrap}' +
      '.db-motion-status{font-size:11.5px;color:#fbbf24}' +
      '.db-motion-export{border:1px solid #38bdf8;background:linear-gradient(135deg,#0284c7,#2563eb);color:#fff;border-radius:999px;padding:7px 14px;font:800 12px var(--font,Inter,sans-serif);cursor:pointer;white-space:nowrap}' +
      '.db-motion-export.rec{background:#dc2626;border-color:#fca5a5;animation:dbMotionPulse 1s infinite}' +
      '@keyframes dbMotionPulse{0%,100%{opacity:1}50%{opacity:.78}}' +
      '@media (max-width:760px){' +
        '.db-chap-actions{width:100%}.db-chap-actions>*{flex:1 1 0;justify-content:center}' +
        '.db-wt-actions{width:100%}.db-wt-actions>*{flex:1 1 auto;text-align:center}' +
        '.db-motion-bg{padding:6px}.db-motion-dialog{height:calc(100dvh - 12px);max-height:calc(100dvh - 12px);border-radius:14px}' +
        '.db-motion-top,.db-motion-foot{padding:7px 10px;gap:6px}' +
        '.db-motion-export-wrap{width:100%}.db-motion-export{width:100%;text-align:center}' +
      '}' +
      '.db-motion-bg.db-motion-light{background:rgba(241,245,249,.72);-webkit-backdrop-filter:blur(10px);backdrop-filter:blur(10px)}' +
      '.db-motion-light .db-motion-dialog{background:#fff;border-color:#e2e8f0;color:#0f172a;box-shadow:0 30px 80px -16px rgba(15,23,42,.35)}' +
      '.db-motion-light .db-motion-stories,.db-motion-light .db-motion-top,.db-motion-light .db-motion-foot{background:#f8fafc;border-color:#e2e8f0}' +
      '.db-motion-light .db-motion-seg{background:rgba(15,23,42,.1)}' +
      '.db-motion-light .db-motion-live{color:#0f766e;background:#f0fdfa;border-color:#99f6e4}' +
      '.db-motion-light .db-motion-time{color:#334155}' +
      '.db-motion-light .db-motion-chip{border-color:#e2e8f0;background:#fff;color:#334155}' +
      '.db-motion-light .db-motion-chip:hover,.db-motion-light .db-motion-chip.on{border-color:#5eead4;background:#f0fdfa;color:#0f766e}' +
      '.db-motion-light .db-motion-lang{background:#e2e8f0}' +
      '.db-motion-light .db-motion-lang button{color:#475569}' +
      '.db-motion-light .db-motion-lang button.on{background:#fff;color:#0f172a;box-shadow:0 1px 2px rgba(0,0,0,.12)}' +
      '.db-motion-light .db-motion-close{border-color:#e2e8f0;background:#fff;color:#0f172a}' +
      '.db-motion-light .db-motion-stage{background:#eef2f7}' +
      '.db-motion-light .db-motion-canvas{background:#e2e8f0;box-shadow:0 18px 44px -14px rgba(15,23,42,.35),0 0 0 1px rgba(15,23,42,.08)}' +
      '.db-motion-light .db-motion-pri{color:#fff}' +
      '.db-motion-light .db-motion-scbtn{border-color:#e2e8f0;background:#fff;color:#475569}' +
      '.db-motion-light .db-motion-scbtn.on{background:#0f766e;border-color:#0f766e;color:#fff}' +
      '.db-motion-light .db-motion-status{color:#b45309}' +
      '@media print{.db-motion-bg,.db-chap-motion-btn,.db-banner-motion-btn,.hm-motion,.db-wt-motion-btn{display:none!important}}';
    host.appendChild(st);
  }
  function makeMotionBtn(id, cls, en, zh, es, startScene) {
    var b = document.createElement('button');
    b.type = 'button';
    b.id = id;
    b.className = cls;
    b.setAttribute('data-open-motion-reel', '1');
    b.innerHTML = '🎬 ' + span3(en, zh, es);
    b.addEventListener('click', function () {
      openMotionReel(typeof startScene === 'function' ? startScene() : (startScene || 0));
    });
    // Warm the aerial images as soon as someone reaches for the button.
    var warm = function () {
      try { preloadImagery(state.data || (state.data = extractReelData(getRep()))); } catch (e) {}
    };
    b.addEventListener('pointerenter', warm);
    b.addEventListener('focus', warm);
    return b;
  }
  function injectEntryButtons() {
    ensureStyles();
    var mediaTabs = typeof document.querySelector === 'function' ? document.querySelector('#heroMediaSwitcher .hm-tabs') : null;
    if (mediaTabs && !byId('openMotionReelBtn') && typeof mediaTabs.appendChild === 'function') {
      mediaTabs.appendChild(makeMotionBtn('openMotionReelBtn', 'hm-tab hm-motion', 'Watch 50s Video Tour', '50秒视频导览', 'Video de 50s', 0));
    }
    var chapInner = typeof document.querySelector === 'function' ? document.querySelector('#chapterWorkspaceBar .chap-bar-inner') : null;
    var stickyWt = byId('stickyWalkthroughBtn');
    if (chapInner && stickyWt && !byId('chapterMotionReelBtn') && typeof chapInner.appendChild === 'function') {
      var actWrap = document.createElement('div');
      actWrap.className = 'db-chap-actions';
      actWrap.appendChild(stickyWt);
      actWrap.appendChild(makeMotionBtn('chapterMotionReelBtn', 'db-chap-motion-btn', 'Video Tour', '视频导览', 'Video', 0));
      chapInner.appendChild(actWrap);
    }
    var wtHead = typeof document.querySelector === 'function' ? document.querySelector('#walkthroughBannerMount .wt-banner-head') : null;
    var openWt = byId('openWalkthroughBtn');
    if (wtHead && openWt && !byId('bannerMotionReelBtn') && typeof wtHead.appendChild === 'function') {
      var bWrap = document.createElement('div');
      bWrap.className = 'db-wt-actions';
      bWrap.appendChild(openWt);
      bWrap.appendChild(makeMotionBtn('bannerMotionReelBtn', 'db-banner-motion-btn', 'Watch the 50s Video Tour', '观看 50 秒视频导览', 'Ver el video de 50s', 0));
      wtHead.appendChild(bWrap);
    }
    var wtModal = byId('walkthroughModal');
    var wtRight = wtModal && typeof wtModal.querySelector === 'function' ? wtModal.querySelector('.wt-top-right') : null;
    if (wtRight && !byId('wtMotionReelBtn') && typeof wtRight.insertBefore === 'function') {
      var wtBtn = makeMotionBtn('wtMotionReelBtn', 'db-wt-motion-btn', 'Play as Motion Video', '切换为动态视频播放', 'Reproducir como Video', 0);
      var closeWt = byId('wtCloseBtn');
      wtRight.insertBefore(wtBtn, closeWt || null);
    }
  }
  function wireHooksAndObservers() {
    if (typeof window.refreshDynamicLanguageViews === 'function' && !window.__DB_MOTION_WRAPPED_LANG__) {
      window.__DB_MOTION_WRAPPED_LANG__ = true;
      var origLang = window.refreshDynamicLanguageViews;
      window.refreshDynamicLanguageViews = function () {
        var res = origLang.apply(this, arguments);
        try { injectEntryButtons(); } catch (e) {}
        return res;
      };
    }
    if (window.OpenEstateWorkspace && !window.__DB_MOTION_WRAPPED_WS__) {
      window.__DB_MOTION_WRAPPED_WS__ = true;
      ['refreshAll', 'renderWalkthroughBanner', 'openWalkthrough'].forEach(function (fn) {
        if (typeof window.OpenEstateWorkspace[fn] === 'function') {
          var orig = window.OpenEstateWorkspace[fn];
          window.OpenEstateWorkspace[fn] = function () {
            var res = orig.apply(this, arguments);
            try { injectEntryButtons(); } catch (e) {}
            return res;
          };
        }
      });
    }
    if (typeof MutationObserver === 'function' && !window.__DB_MOTION_OBS__) {
      window.__DB_MOTION_OBS__ = true;
      var targets = ['heroMediaBento', 'chapterWorkspaceBar', 'walkthroughBannerMount', 'walkthroughModal'];
      var pending = false;
      var obs = new MutationObserver(function () {
        if (pending) return;
        pending = true;
        Promise.resolve().then(function () {
          pending = false;
          injectEntryButtons();
        });
      });
      targets.forEach(function (id) {
        var el = byId(id);
        if (el) {
          try { obs.observe(el, { childList: true, subtree: false }); } catch (e) {}
        }
      });
    }
    if (typeof document.addEventListener === 'function' && !window.__DB_MOTION_KEYS__) {
      window.__DB_MOTION_KEYS__ = true;
      document.addEventListener('keydown', function (ev) {
        if (!state.open || !ev) return;
        if (ev.key === 'Escape') {
          if (typeof ev.preventDefault === 'function') ev.preventDefault();
          closeMotionReel();
        } else if (ev.key === ' ' || ev.code === 'Space') {
          var tag = ev.target && ev.target.tagName ? String(ev.target.tagName).toUpperCase() : '';
          if (tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'BUTTON') {
            if (typeof ev.preventDefault === 'function') ev.preventDefault();
            togglePlay();
          }
        } else if (ev.key === 'ArrowRight') {
          seekScene(Math.min(SCENE_COUNT - 1, sceneAt(state.t).idx + 1));
        } else if (ev.key === 'ArrowLeft') {
          seekScene(Math.max(0, sceneAt(state.t).idx - 1));
        }
      });
    }
    // Follow the system light / dark setting live.
    if (!window.__DB_MOTION_THEME__ && typeof window.matchMedia === 'function') {
      window.__DB_MOTION_THEME__ = true;
      try {
        var mq = window.matchMedia('(prefers-color-scheme: dark)');
        window.__DB_MOTION_MQ__ = mq;
        var onTheme = function () { if (!state.themeOverride) syncTheme(); };
        if (mq && typeof mq.addEventListener === 'function') mq.addEventListener('change', onTheme);
        else if (mq && typeof mq.addListener === 'function') mq.addListener(onTheme);
      } catch (e) {}
    }
    if (typeof window.addEventListener === 'function' && !window.__DB_MOTION_RESIZE__) {
      window.__DB_MOTION_RESIZE__ = true;
      window.addEventListener('resize', function () { if (state.open) syncCanvasDisplaySize(); });
    }
  }

  window.OpenEstateMotion = {
    open: openMotionReel,
    close: closeMotionReel,
    play: function () { if (!state.playing) togglePlay(); },
    pause: function () { if (state.playing) togglePlay(); },
    seekScene: seekScene,
    seekTime: seekTime,
    setAspect: setAspect,
    setLang: setReelLang,
    refreshButtons: injectEntryButtons,
    exportWebm: startExportWebm,
    /** 'light' | 'dark' pins the look; anything else follows the system setting again. */
    setTheme: function (m) { state.themeOverride = m === 'light' || m === 'dark' ? m : null; return syncTheme(); },
    duration: TOTAL_DURATION,
    getState: function () {
      return {
        open: state.open,
        playing: state.playing,
        t: state.t,
        aspect: state.aspect,
        speed: state.speed,
        recording: state.recording,
        loading: Boolean(state.waitingSince),
        theme: TH.mode,
        sceneIndex: sceneAt(state.t).idx
      };
    },
    // Exposed for the offline tests.
    _internals: {
      extractReelData: extractReelData,
      resolveMotionSatellite: resolveMotionSatellite,
      aerialLayers: aerialLayers,
      mkCam: mkCam,
      projector: projector,
      mercX: mercX,
      mercY: mercY,
      sceneAt: sceneAt,
      scenes: SCENES,
      renderFrame: renderFrame,
      planTiles: planTiles,
      tick: tick,
      state: state,
      imageStatus: function () {
        var tiles = { ready: 0, loading: 0, error: 0 };
        Object.keys(_tiles).forEach(function (k) { tiles[_tiles[k].status] = (tiles[_tiles[k].status] || 0) + 1; });
        return { detail: Object.keys(_img).map(function (k) { return { url: k, status: _img[k].status }; }), tiles: tiles };
      }
    }
  };

  injectEntryButtons();
  wireHooksAndObservers();
})();
