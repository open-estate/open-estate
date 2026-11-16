/* Open Estate Hero Media Cache, Photo/360° Upload Sheet & Native Three.js 360° Walk-In Viewer.
 * Runs right after assets/report_workspace.js without modifying template.html.
 * Trilingual (EN / 简体中文 / Español), light-card report chrome, zero synthetic interiors.
 */
(function () {
  'use strict';
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (window.__OPEN_ESTATE_WALKIN_MEDIA_INIT__) return;
  window.__OPEN_ESTATE_WALKIN_MEDIA_INIT__ = true;

  var Core = window.OpenEstateCore || null;
  var THREE_CDN_URLS = (Core && Array.isArray(Core.THREE_URLS)) ? Core.THREE_URLS.slice() : [
    'https://cdn.jsdelivr.net/npm/three@0.170.0/build/three.module.min.js',
    'https://unpkg.com/three@0.170.0/build/three.module.min.js'
  ];

  /* ------------------------------------------------------------------ core helpers */
  function byId(id) {
    return document.getElementById ? document.getElementById(id) : null;
  }
  function qAll(sel, root) {
    var ctx = root || document;
    if (!ctx || typeof ctx.querySelectorAll !== 'function') return [];
    try { return Array.prototype.slice.call(ctx.querySelectorAll(sel) || []); } catch (e) { return []; }
  }
  function isN(x) { return typeof x === 'number' && isFinite(x); }
  function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)); }
  function esc(s) {
    if (Core && typeof Core.esc === 'function') return Core.esc(s);
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }
  function curLang() {
    if (Core && typeof Core.langCode === 'function') return Core.langCode();
    try { if (typeof LANG === 'string' && LANG) return LANG.indexOf('zh') === 0 ? 'zh' : LANG.indexOf('es') === 0 ? 'es' : 'en'; } catch (e) {}
    try {
      if (window.i18next && typeof window.i18next.language === 'string' && window.i18next.language) {
        var il = window.i18next.language.toLowerCase();
        return il.indexOf('zh') === 0 ? 'zh' : il.indexOf('es') === 0 ? 'es' : 'en';
      }
    } catch (e2) {}
    var dl = String((document.documentElement && document.documentElement.lang) || 'en').toLowerCase();
    return dl.indexOf('zh') === 0 ? 'zh' : dl.indexOf('es') === 0 ? 'es' : 'en';
  }
  function L3(en, zh, es) {
    var l = curLang();
    return l === 'zh' ? (zh || en) : l === 'es' ? (es || en) : en;
  }
  function span3(en, zh, es) {
    return '<span data-en="' + esc(en) + '" data-zh="' + esc(zh || en) + '" data-es="' + esc(es || en) + '">' + esc(L3(en, zh, es)) + '</span>';
  }

  function getRep() {
    if (Core && typeof Core.getRep === 'function') return Core.getRep();
    var rep = null;
    try { if (typeof R !== 'undefined' && R && typeof R === 'object') rep = R; } catch (e) {}
    if (!rep && window.__REPORT_DATA__ && typeof window.__REPORT_DATA__ === 'object') {
      rep = window.__REPORT_DATA__;
    }
    if (!rep) {
      try {
        var el = byId('report-data');
        if (el && el.textContent) rep = JSON.parse(el.textContent);
      } catch (e2) {}
    }
    if (rep && window.__REPORT_DATA__ !== rep) {
      window.__REPORT_DATA__ = rep;
    }
    return rep;
  }

  function resolveSlug() {
    var rep = getRep();
    var raw = (rep && rep.meta && rep.meta.slug) || '';
    if (!raw && typeof location !== 'undefined' && location.pathname) {
      var m = String(location.pathname).match(/\/r\/([a-z0-9][a-z0-9-]{2,140})/i);
      if (m) raw = m[1];
    }
    return String(raw || '').trim().toLowerCase().replace(/[^a-z0-9-]/g, '').slice(0, 145);
  }

  function isLiveHttpPage() {
    return typeof location !== 'undefined' && /^https?:$/i.test(location.protocol || '') && typeof fetch === 'function';
  }

  /* ------------------------------------------------------------------ allowlisted 3D tour parser */
  function parseSupportedTourUrl(raw, fallbackLabel) {
    if (Core && typeof Core.parseTourUrl === 'function') {
      var parsed = Core.parseTourUrl(raw, { allowHttp: false, allowVideo: false, fallbackLabel: fallbackLabel });
      if (!parsed) return null;
      return {
        vendor: parsed.vendor,
        embedUrl: parsed.embedUrl,
        openUrl: parsed.openUrl,
        label: String(fallbackLabel || (parsed.vendor + ' 3D Walkthrough')).slice(0, 80)
      };
    }
    var txt = String(raw || '').trim();
    if (!txt || txt.length > 600) return null;
    if (/^\/\//.test(txt)) txt = 'https:' + txt;
    var u;
    try { u = new URL(txt); } catch (e) { return null; }
    if (u.protocol !== 'https:' || u.username || u.password) return null;
    var host = u.hostname.toLowerCase().replace(/\.+$/, '');
    var hostNoWww = host.replace(/^www\./, '');
    var segs = u.pathname.split('/').filter(Boolean);
    var tokRe = /^[A-Za-z0-9_-]{3,120}$/;
    var vendor = null, embedUrl = null, openUrl = null;

    if (hostNoWww === 'my.matterport.com' || hostNoWww === 'matterport.com') {
      var mpId = u.searchParams.get('m') || '';
      if (!mpId && segs.length >= 2 && (segs[0] === 'show' || segs[0] === 'models' || segs[0] === 'space')) mpId = segs[1];
      if (!mpId && segs.length >= 3 && segs[0] === 'discover' && segs[1] === 'space') mpId = segs[2];
      if (!/^[A-Za-z0-9_-]{5,36}$/.test(mpId)) return null;
      vendor = 'Matterport';
      embedUrl = 'https://my.matterport.com/show/?m=' + mpId + '&play=1';
      openUrl = 'https://my.matterport.com/show/?m=' + mpId;
    } else if (hostNoWww === 'youriguide.com') {
      var igId = (segs[0] === 'embed' && segs.length === 2) ? segs[1] : (segs.length === 1 && segs[0] !== 'embed' ? segs[0] : '');
      if (!tokRe.test(igId)) return null;
      vendor = 'iGUIDE';
      embedUrl = 'https://youriguide.com/embed/' + igId + '?unbranded=1';
      openUrl = 'https://youriguide.com/' + igId;
    } else if (hostNoWww === 'kuula.co') {
      var isCol = segs[0] === 'share' && segs[1] === 'collection' && segs.length === 3;
      var kuId = isCol ? segs[2] : ((segs[0] === 'share' || segs[0] === 'post') && segs.length === 2 && segs[1] !== 'collection' ? segs[1] : '');
      if (!tokRe.test(kuId)) return null;
      var kuBase = 'https://kuula.co/share/' + (isCol ? 'collection/' : '') + kuId;
      vendor = 'Kuula';
      embedUrl = kuBase + '?logo=0&info=0&fs=1&vr=0&sd=1&thumbs=1';
      openUrl = kuBase;
    } else if (host === 'app.cloudpano.com') {
      var cpId = (segs[0] === 'tours' && segs.length === 2) ? segs[1] : '';
      if (!tokRe.test(cpId)) return null;
      vendor = 'CloudPano';
      embedUrl = openUrl = 'https://app.cloudpano.com/tours/' + cpId;
    } else if (hostNoWww === 'ricoh360.com' || host === 'tours.ricoh360.com' || host === 'view.ricoh360.com') {
      var rcHost = 'www.ricoh360.com', rcId = '';
      if ((host === 'tours.ricoh360.com' || host === 'view.ricoh360.com') && segs.length === 1) {
        rcHost = host; rcId = segs[0];
      } else if (segs[0] === 'tours' && segs.length === 2) {
        rcHost = (host === 'tours.ricoh360.com' || host === 'view.ricoh360.com') ? host : 'www.ricoh360.com';
        rcId = segs[1];
      }
      if (!tokRe.test(rcId)) return null;
      vendor = 'RICOH360';
      embedUrl = openUrl = rcHost === 'www.ricoh360.com' ? ('https://www.ricoh360.com/tours/' + rcId) : ('https://' + rcHost + '/' + rcId);
    } else if (hostNoWww === 'giraffe360.com' || host === 'tour.giraffe360.com') {
      var gfId = ((segs[0] === 'tours' || segs[0] === 'tour') && segs.length === 2) ? segs[1] : (segs.length === 1 ? segs[0] : '');
      if (!tokRe.test(gfId)) return null;
      vendor = 'Giraffe360';
      embedUrl = openUrl = 'https://tour.giraffe360.com/' + gfId + '/';
    } else if (host === 'players.cupix.com') {
      var cxId = (segs[0] === 'p' && segs.length === 2) ? segs[1] : (segs.length === 1 ? segs[0] : '');
      if (!tokRe.test(cxId)) return null;
      vendor = 'Cupix';
      embedUrl = openUrl = 'https://players.cupix.com/p/' + cxId;
    } else {
      return null;
    }

    return {
      vendor: vendor,
      embedUrl: embedUrl,
      openUrl: openUrl,
      label: String(fallbackLabel || (vendor + ' 3D Walkthrough')).slice(0, 80)
    };
  }

  function extractReportTours(rep) {
    if (!rep) return [];
    var out = [], seen = {};
    function collect(v, lbl) {
      if (!v) return;
      if (Array.isArray(v)) { v.forEach(function (x) { collect(x, lbl); }); return; }
      if (typeof v === 'object') {
        var cand = v.embedUrl || v.openUrl || v.url || v.href || v.link;
        collect(cand, v.label || v.name || lbl);
        return;
      }
      var parsed = parseSupportedTourUrl(String(v), lbl);
      if (parsed && !seen[parsed.embedUrl]) {
        seen[parsed.embedUrl] = 1;
        out.push(parsed);
      }
    }
    var h3d = (rep.modules && rep.modules.house3d && rep.modules.house3d.data) || rep.house3d || {};
    var facts = rep.facts || {};
    var ml = facts.mlsListing || {};
    var li = facts.listing || {};
    [
      h3d.tours,
      facts.virtualTours, facts.virtualTourUrl, facts.virtualTour, facts.tour3dUrl, facts.tourUrl,
      ml.virtualTours, ml.virtualTourUrl, ml.virtualTourUrlUnbranded, ml.virtualTourUrlBranded,
      li.virtualTours, li.virtualTourUrl
    ].forEach(function (src) { collect(src, null); });
    return out;
  }

  /* ------------------------------------------------------------------ state */
  var st = {
    slug: '',
    heroMode: 'maps',         // 'maps' | 'photos' | 'walkin'
    subMode: 'pano',          // 'pano' | 'tour'
    manifestLoaded: false,
    items: [],
    panos: [],
    tours: [],
    activePanoId: '',
    activeTourIdx: 0,
    showTourInputBar: false,
    tourMsg: '',
    tourErr: false,
    linkMode: false,
    pendingLinkCoords: null,  // { yaw, pitch, x, y }
    hotspotSaving: false,
    stageRoot: null,          // persistent #heroWalkinStage element
    uploadModalEl: null,
    uploadQueue: [],          // [{ file, dataUrl, width, height, autoPano, isPano360, roomTag, category, caption, status, err }]
    uploading: false,
    enhancing: false,
    viewer: null              // Three.js 360° spherical panorama controller
  };

  /* ------------------------------------------------------------------ CSS injection (light report chrome) */
  var WALKIN_CSS = [
    '.hm-tab .wk-tab-badge{display:inline-flex;align-items:center;justify-content:center;min-width:18px;height:18px;padding:0 5px;margin-left:5px;border-radius:999px;font-size:10.5px;font-weight:800;background:#ccfbf1;color:#0f766e;vertical-align:middle}',
    '.hm-tab.on .wk-tab-badge{background:rgba(255,255,255,.22);color:#fff}',
    '.wk-upload-pill{display:inline-flex;align-items:center;gap:5px;border:1px solid #99f6e4;background:#f0fdfa;color:#0f766e;border-radius:999px;padding:5px 12px;font:700 11.8px var(--font,Inter,system-ui,sans-serif);cursor:pointer;white-space:nowrap;transition:all .15s}',
    '.wk-upload-pill:hover{background:#ccfbf1;border-color:#0f766e;color:#115e59}',
    '.bento-pano-chip{position:absolute;top:38px;right:10px;z-index:4;border:1px solid rgba(153,246,228,.85);background:rgba(255,255,255,.93);color:#0f766e;border-radius:999px;padding:3px 9px;font:800 10.8px var(--font,Inter,system-ui,sans-serif);cursor:pointer;box-shadow:0 3px 10px rgba(15,23,42,.18);backdrop-filter:blur(6px)}',
    '.bento-pano-chip:hover{background:#0f766e;color:#fff}',
    '.wk-stage-shell{background:#fff;border:1px solid var(--line,#e2e8f0);border-radius:16px;overflow:hidden;box-shadow:0 10px 30px -16px rgba(15,23,42,.16);margin-bottom:6px}',
    '.wk-topbar{display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap;padding:9px 13px;background:#f8fafc;border-bottom:1px solid #e2e8f0}',
    '.wk-topbar-left,.wk-topbar-right{display:flex;align-items:center;gap:6px;flex-wrap:wrap}',
    '.wk-seg-btn{display:inline-flex;align-items:center;gap:5px;border:1px solid #cbd5e1;background:#fff;color:#334155;border-radius:999px;padding:5px 11px;font:700 11.8px var(--font,Inter,system-ui,sans-serif);cursor:pointer;transition:all .15s}',
    '.wk-seg-btn:hover{border-color:#0f766e;color:#0f766e}',
    '.wk-seg-btn.on{background:#0f766e;border-color:#0f766e;color:#fff}',
    '.wk-seg-btn.warn-on{background:#0284c7;border-color:#0284c7;color:#fff}',
    '.wk-room-title-pill{font:700 12px var(--font,Inter,system-ui,sans-serif);color:#0f172a;background:#eef2f6;padding:4px 10px;border-radius:999px}',
    '.wk-tour-bar{display:flex;align-items:center;gap:8px;flex-wrap:wrap;padding:9px 13px;background:#f0fdfa;border-bottom:1px solid #ccfbf1}',
    '.wk-tour-bar input{flex:1 1 280px;min-width:200px;border:1px solid #cbd5e1;background:#fff;color:#0f172a;border-radius:9px;padding:7px 11px;font:500 12.5px var(--font,Inter,system-ui,sans-serif)}',
    '.wk-tour-bar input:focus{outline:2px solid #14b8a6;border-color:#0f766e}',
    '.wk-msg{font-size:12px;font-weight:600;padding:6px 13px;border-bottom:1px solid #e2e8f0}',
    '.wk-msg.ok{background:#f0fdf4;color:#166534}',
    '.wk-msg.err{background:#fef2f2;color:#b91c1c}',
    '.wk-viewport{position:relative;width:100%;aspect-ratio:16 / 10;min-height:360px;max-height:500px;background:linear-gradient(180deg,#f1f5f9 0%,#e2e8f0 100%);overflow:hidden;user-select:none;touch-action:none}',
    '.wk-stage-shell:fullscreen .wk-viewport,.wk-viewport:fullscreen{max-height:none;height:100vh}',
    '.wk-viewport.link-cursor{cursor:crosshair}',
    '.wk-viewport.drag-cursor{cursor:grab}',
    '.wk-viewport.dragging{cursor:grabbing}',
    '.wk-canvas{width:100%;height:100%;display:block}',
    '.wk-iframe{width:100%;height:100%;border:0;display:block;background:#f8fafc}',
    '.wk-hotspots-layer{position:absolute;inset:0;pointer-events:none;z-index:5;overflow:hidden}',
    '.wk-hotspot-pill{position:absolute;pointer-events:auto;display:inline-flex;align-items:center;gap:6px;border:1.5px solid rgba(15,118,110,.85);background:rgba(255,255,255,.94);color:#0f172a;border-radius:999px;padding:6px 13px;font:800 12.2px var(--font,Inter,system-ui,sans-serif);cursor:pointer;box-shadow:0 8px 22px -4px rgba(15,23,42,.32);backdrop-filter:blur(8px);transform:translate(-50%,-50%);transition:transform .14s ease,background .14s ease;white-space:nowrap}',
    '.wk-hotspot-pill:hover{background:#0f766e;color:#fff;border-color:#fff;transform:translate(-50%,-50%) scale(1.06)}',
    '.wk-hotspot-pill .wk-hs-dot{width:8px;height:8px;border-radius:999px;background:#0f766e;box-shadow:0 0 0 3px rgba(20,184,166,.28)}',
    '.wk-hotspot-pill:hover .wk-hs-dot{background:#fff}',
    '.wk-hotspot-del{margin-left:3px;border:0;background:#fee2e2;color:#b91c1c;border-radius:999px;width:18px;height:18px;font-size:11px;font-weight:800;line-height:1;cursor:pointer;display:inline-grid;place-items:center}',
    '.wk-hud-controls{position:absolute;right:12px;bottom:12px;z-index:7;display:flex;align-items:center;gap:5px;background:rgba(255,255,255,.92);border:1px solid rgba(203,213,225,.9);border-radius:999px;padding:4px 7px;backdrop-filter:blur(8px);box-shadow:0 4px 14px rgba(15,23,42,.14)}',
    '.wk-hud-btn{border:0;background:transparent;color:#0f172a;border-radius:999px;min-width:28px;height:28px;padding:0 7px;font:800 13px var(--font,Inter,system-ui,sans-serif);cursor:pointer}',
    '.wk-hud-btn:hover{background:#e2e8f0}',
    '.wk-hud-fov{font:700 11px var(--font,Inter,system-ui,sans-serif);color:#475569;padding:0 4px;font-variant-numeric:tabular-nums}',
    '.wk-hud-hint{position:absolute;left:12px;bottom:12px;z-index:6;background:rgba(255,255,255,.9);border:1px solid rgba(203,213,225,.85);color:#334155;border-radius:999px;padding:5px 11px;font:600 11.4px var(--font,Inter,system-ui,sans-serif);backdrop-filter:blur(8px);pointer-events:none}',
    '.wk-link-banner{position:absolute;top:12px;left:50%;transform:translateX(-50%);z-index:8;background:rgba(255,255,255,.96);border:1.5px solid #0284c7;color:#0369a1;border-radius:999px;padding:6px 15px;font:700 12px var(--font,Inter,system-ui,sans-serif);box-shadow:0 8px 24px rgba(15,23,42,.18)}',
    '.wk-link-popover{position:absolute;z-index:10;width:min(300px,calc(100% - 24px));background:#fff;border:1px solid #cbd5e1;border-radius:14px;padding:12px 13px;box-shadow:0 18px 40px -10px rgba(15,23,42,.28)}',
    '.wk-link-popover label{display:block;font:700 11.2px var(--font,Inter,system-ui,sans-serif);color:#475569;margin-bottom:4px}',
    '.wk-link-popover select,.wk-link-popover input{width:100%;border:1px solid #cbd5e1;border-radius:8px;padding:6px 9px;font:600 12.5px var(--font,Inter,system-ui,sans-serif);color:#0f172a;background:#fff;margin-bottom:8px;box-sizing:border-box}',
    '.wk-fade-veil{position:absolute;inset:0;z-index:4;background:rgba(248,250,252,.68);backdrop-filter:blur(6px);opacity:0;pointer-events:none;transition:opacity .2s ease}',
    '.wk-fade-veil.on{opacity:1}',
    '.wk-loading{position:absolute;inset:0;z-index:6;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:8px;background:rgba(248,250,252,.85);color:#0f172a;font:700 13px var(--font,Inter,system-ui,sans-serif)}',
    '.wk-room-strip{display:flex;align-items:center;gap:8px;overflow-x:auto;padding:10px 13px;background:#fff;border-top:1px solid #e2e8f0;scrollbar-width:thin}',
    '.wk-room-card{display:inline-flex;align-items:center;gap:8px;border:1.5px solid #e2e8f0;background:#f8fafc;color:#0f172a;border-radius:11px;padding:5px 10px 5px 5px;cursor:pointer;flex:0 0 auto;transition:all .15s}',
    '.wk-room-card:hover{border-color:#14b8a6;background:#f0fdfa}',
    '.wk-room-card.on{border-color:#0f766e;background:#f0fdfa;box-shadow:0 2px 8px rgba(15,118,110,.16)}',
    '.wk-room-card img{width:54px;height:34px;border-radius:7px;object-fit:cover;background:#cbd5e1;display:block}',
    '.wk-room-card .wk-rc-meta{display:flex;flex-direction:column;align-items:flex-start;text-align:left}',
    '.wk-room-card .wk-rc-name{font:700 12px var(--font,Inter,system-ui,sans-serif);color:#0f172a}',
    '.wk-room-card .wk-rc-sub{font:600 10.5px var(--font,Inter,system-ui,sans-serif);color:#64748b}',
    '.wk-empty{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;padding:24px;background:linear-gradient(135deg,#f8fafc 0%,#f0fdfa 100%);overflow-y:auto}',
    '.wk-empty-card{max-width:620px;width:100%;background:#fff;border:1px solid #cbd5e1;border-radius:16px;padding:22px 24px;box-shadow:0 14px 34px -16px rgba(15,23,42,.15);text-align:left}',
    '.wk-empty-eyebrow{display:inline-block;font:800 10.8px var(--font,Inter,system-ui,sans-serif);letter-spacing:.6px;text-transform:uppercase;color:#0f766e;background:#ccfbf1;padding:3px 9px;border-radius:999px;margin-bottom:8px}',
    '.wk-empty-card h3{margin:0 0 6px;font:800 19px/1.25 var(--font,Inter,system-ui,sans-serif);color:#0f172a}',
    '.wk-empty-card p{margin:0 0 14px;font:400 13.2px/1.55 var(--font,Inter,system-ui,sans-serif);color:#475569}',
    '.wk-empty-actions{display:flex;flex-direction:column;gap:11px}',
    '.wk-empty-tour-row{display:flex;gap:8px;flex-wrap:wrap}',
    '.wk-empty-tour-row input{flex:1 1 240px;border:1px solid #cbd5e1;border-radius:10px;padding:8px 12px;font:500 13px var(--font,Inter,system-ui,sans-serif);color:#0f172a;background:#f8fafc}',
    '.wk-pri-btn{display:inline-flex;align-items:center;justify-content:center;gap:6px;border:1px solid #0f766e;background:#0f766e;color:#fff;border-radius:10px;padding:9px 15px;font:700 12.8px var(--font,Inter,system-ui,sans-serif);cursor:pointer;white-space:nowrap}',
    '.wk-pri-btn:hover{background:#115e59}',
    '.wk-pri-btn:disabled{opacity:.55;cursor:not-allowed}',
    '.wk-sec-btn{display:inline-flex;align-items:center;justify-content:center;gap:5px;border:1px solid #cbd5e1;background:#fff;color:#0f172a;border-radius:10px;padding:8px 13px;font:700 12.4px var(--font,Inter,system-ui,sans-serif);cursor:pointer;white-space:nowrap}',
    '.wk-sec-btn:hover{background:#f1f5f9}',
    /* Upload sheet modal — light frosted wash matching report page */
    '.wk-modal-bg{position:fixed;inset:0;z-index:3600;background:rgba(241,245,249,.76);-webkit-backdrop-filter:blur(10px);backdrop-filter:blur(10px);display:grid;place-items:center;padding:16px}',
    '.wk-modal-bg[hidden]{display:none!important}',
    '.wk-modal-dialog{background:#fff;border:1px solid #cbd5e1;border-radius:18px;width:min(680px,100%);max-height:calc(100dvh - 32px);display:flex;flex-direction:column;overflow:hidden;box-shadow:0 28px 70px -16px rgba(15,23,42,.22)}',
    '.wk-modal-hdr{display:flex;align-items:center;justify-content:space-between;gap:10px;padding:14px 18px;background:#f8fafc;border-bottom:1px solid #e2e8f0}',
    '.wk-modal-hdr h3{margin:0;font:800 16.5px var(--font,Inter,system-ui,sans-serif);color:#0f172a}',
    '.wk-modal-body{padding:16px 18px;overflow-y:auto;display:flex;flex-direction:column;gap:12px}',
    '.wk-dropzone{border:2px dashed #94a3b8;background:#f8fafc;border-radius:14px;padding:22px 16px;text-align:center;cursor:pointer;transition:all .15s}',
    '.wk-dropzone.dragover,.wk-dropzone:hover{border-color:#0f766e;background:#f0fdfa}',
    '.wk-q-list{display:flex;flex-direction:column;gap:9px}',
    '.wk-q-item{display:grid;grid-template-columns:76px 1fr auto;gap:10px;align-items:center;padding:9px 11px;border:1px solid #e2e8f0;border-radius:12px;background:#f8fafc}',
    '.wk-q-item img{width:76px;height:48px;object-fit:cover;border-radius:8px;background:#e2e8f0}',
    '.wk-q-fields{display:flex;flex-wrap:wrap;gap:6px;align-items:center}',
    '.wk-q-fields input[type="text"],.wk-q-fields select{border:1px solid #cbd5e1;border-radius:7px;padding:4px 8px;font:600 12px var(--font,Inter,system-ui,sans-serif);background:#fff;color:#0f172a}',
    '.wk-auto360-pill{display:inline-flex;align-items:center;gap:4px;background:#ccfbf1;color:#0f766e;border:1px solid #99f6e4;border-radius:999px;padding:2px 8px;font:800 10.6px var(--font,Inter,system-ui,sans-serif)}',
    '.wk-modal-foot{display:flex;align-items:center;justify-content:space-between;gap:10px;padding:12px 18px;background:#f8fafc;border-top:1px solid #e2e8f0}',
    '@media (max-width:720px){.wk-viewport{aspect-ratio:4 / 3;min-height:260px}.wk-hud-hint{display:none}.wk-q-item{grid-template-columns:60px 1fr}}'
  ].join('\n');

  function injectStyles() {
    if (byId('walkin-media-css')) return;
    var styleEl = document.createElement('style');
    styleEl.id = 'walkin-media-css';
    styleEl.textContent = WALKIN_CSS;
    var parent = document.head || document.body;
    if (parent && parent.appendChild) parent.appendChild(styleEl);
  }

  /* ------------------------------------------------------------------ Three.js loader */
  var _threePromise = null;
  function loadThreeEngine() {
    if (Core && typeof Core.loadThreeJs === 'function') {
      return Core.loadThreeJs(THREE_CDN_URLS);
    }
    if (window.THREE && window.THREE.WebGLRenderer && window.THREE.SphereGeometry) {
      return Promise.resolve(window.THREE);
    }
    if (_threePromise) return _threePromise;
    var urls = THREE_CDN_URLS.slice();
    if (window.OpenEstateHouse3D && Array.isArray(window.OpenEstateHouse3D.THREE_URLS)) {
      window.OpenEstateHouse3D.THREE_URLS.forEach(function (u) {
        if (u && urls.indexOf(u) === -1) urls.push(u);
      });
    }
    function tryIdx(idx) {
      if (idx >= urls.length) return Promise.reject(new Error('3D engine unavailable'));
      return import(urls[idx]).then(function (mod) {
        var resolved = (mod && mod.WebGLRenderer) ? mod : (mod && mod.default && mod.default.WebGLRenderer ? mod.default : mod);
        if (!window.THREE && resolved && resolved.WebGLRenderer) {
          try { window.THREE = resolved; } catch (e) {}
        }
        return resolved;
      }, function () {
        return tryIdx(idx + 1);
      });
    }
    _threePromise = tryIdx(0).catch(function (err) {
      _threePromise = null;
      throw err;
    });
    return _threePromise;
  }

  /* ------------------------------------------------------------------ Native Three.js 360° Panorama Controller */
  function createPanoViewer(THREE, dom) {
    var viewport = dom.viewport;
    var canvas = dom.canvas;
    var hotspotsLayer = dom.hotspotsLayer;
    var fovReadout = dom.fovReadout;
    var fadeVeil = dom.fadeVeil;
    var loadingBox = dom.loadingBox;

    var renderer = new THREE.WebGLRenderer({
      canvas: canvas,
      antialias: true,
      alpha: false,
      preserveDrawingBuffer: true
    });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    if ('outputColorSpace' in renderer && THREE.SRGBColorSpace) {
      renderer.outputColorSpace = THREE.SRGBColorSpace;
    }

    var scene = new THREE.Scene();
    var camera = new THREE.PerspectiveCamera(75, 1.6, 0.1, 200);
    camera.position.set(0, 0, 0.01);

    var sphereGeom = new THREE.SphereGeometry(50, 64, 40);
    sphereGeom.scale(-1, 1, 1);
    var sphereMat = new THREE.MeshBasicMaterial({ color: 0xffffff });
    var sphereMesh = new THREE.Mesh(sphereGeom, sphereMat);
    scene.add(sphereMesh);

    var state = {
      yaw: 0,
      pitch: 0,
      targetYaw: 0,
      targetPitch: 0,
      fov: 75,
      targetFov: 75,
      activeTexture: null,
      activeUrl: '',
      hotspots: [],
      dragging: false,
      downX: 0,
      downY: 0,
      lastX: 0,
      lastY: 0,
      movedPx: 0,
      pointers: {},
      pinchStartDist: 0,
      pinchStartFov: 75,
      disposed: false,
      rafId: 0
    };

    var lookTarget = new THREE.Vector3();
    var projVec = new THREE.Vector3();

    function syncSize() {
      if (state.disposed || !viewport) return;
      var w = Math.max(240, viewport.clientWidth || 640);
      var h = Math.max(180, viewport.clientHeight || 400);
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      renderer.setSize(w, h, false);
    }

    function dirFromYawPitch(yawDeg, pitchDeg, outVec) {
      var phi = THREE.MathUtils.degToRad(90 - pitchDeg);
      var theta = THREE.MathUtils.degToRad(yawDeg);
      outVec.set(
        Math.sin(phi) * Math.sin(theta),
        Math.cos(phi),
        -Math.sin(phi) * Math.cos(theta)
      );
      return outVec;
    }

    function unprojectScreenToYawPitch(clientX, clientY) {
      var rect = viewport.getBoundingClientRect();
      var w = Math.max(1, rect.width);
      var h = Math.max(1, rect.height);
      var x = clientX - rect.left;
      var y = clientY - rect.top;
      var ndcX = (x / w) * 2 - 1;
      var ndcY = -(y / h) * 2 + 1;
      var ray = new THREE.Vector3(ndcX, ndcY, 0.5).unproject(camera).sub(camera.position).normalize();
      var ny = clamp(ray.y, -1, 1);
      var pitch = 90 - THREE.MathUtils.radToDeg(Math.acos(ny));
      var yaw = THREE.MathUtils.radToDeg(Math.atan2(ray.x, -ray.z));
      return {
        yaw: Math.round(yaw * 100) / 100,
        pitch: Math.round(clamp(pitch, -85, 85) * 100) / 100,
        relX: x,
        relY: y
      };
    }

    function projectHotspots() {
      if (!hotspotsLayer) return;
      var pills = qAll('.wk-hotspot-pill', hotspotsLayer);
      if (!pills.length) return;
      var w = Math.max(1, viewport.clientWidth || 640);
      var h = Math.max(1, viewport.clientHeight || 400);
      var camDir = dirFromYawPitch(state.yaw, state.pitch, new THREE.Vector3());

      pills.forEach(function (pill) {
        var hy = parseFloat(pill.getAttribute('data-yaw') || '0');
        var hp = parseFloat(pill.getAttribute('data-pitch') || '0');
        var worldDir = dirFromYawPitch(hy, hp, projVec);
        var dot = worldDir.dot(camDir);
        if (dot <= 0.08) {
          pill.style.display = 'none';
          return;
        }
        var worldPt = worldDir.clone().multiplyScalar(42);
        worldPt.project(camera);
        var sx = (worldPt.x * 0.5 + 0.5) * w;
        var sy = (-worldPt.y * 0.5 + 0.5) * h;
        if (sx < -40 || sx > w + 40 || sy < -30 || sy > h + 30) {
          pill.style.display = 'none';
          return;
        }
        pill.style.display = 'inline-flex';
        pill.style.left = sx.toFixed(1) + 'px';
        pill.style.top = sy.toFixed(1) + 'px';
      });
    }

    function renderOnce() {
      if (state.disposed) return;
      camera.fov = state.fov;
      camera.updateProjectionMatrix();
      dirFromYawPitch(state.yaw, state.pitch, lookTarget);
      camera.lookAt(lookTarget);
      camera.updateMatrixWorld();
      renderer.render(scene, camera);
      projectHotspots();
      if (fovReadout) {
        fovReadout.textContent = Math.round(state.fov) + '°';
      }
    }

    function scheduleTick() {
      if (state.disposed || state.rafId) return;
      state.rafId = window.requestAnimationFrame(function step() {
        state.rafId = 0;
        if (state.disposed) return;
        var dy = state.targetYaw - state.yaw;
        var dp = state.targetPitch - state.pitch;
        var df = state.targetFov - state.fov;
        var moving = Math.abs(dy) > 0.03 || Math.abs(dp) > 0.03 || Math.abs(df) > 0.05;
        if (moving) {
          state.yaw += dy * 0.28;
          state.pitch = clamp(state.pitch + dp * 0.28, -85, 85);
          state.fov = clamp(state.fov + df * 0.28, 35, 100);
          renderOnce();
          scheduleTick();
        } else {
          state.yaw = state.targetYaw;
          state.pitch = clamp(state.targetPitch, -85, 85);
          state.fov = clamp(state.targetFov, 35, 100);
          renderOnce();
        }
      });
    }

    function setHotspotsDom(hotspots) {
      state.hotspots = Array.isArray(hotspots) ? hotspots : [];
      if (!hotspotsLayer) return;
      hotspotsLayer.innerHTML = state.hotspots.map(function (hs, idx) {
        var delBtn = st.linkMode
          ? '<span class="wk-hotspot-del" data-del-hs="' + idx + '" title="' + esc(L3('Remove link', '删除跳转点', 'Quitar enlace')) + '">×</span>'
          : '';
        return '<button type="button" class="wk-hotspot-pill" data-hs-idx="' + idx + '" data-target-id="' + esc(hs.targetId) + '" data-yaw="' + Number(hs.yaw || 0) + '" data-pitch="' + Number(hs.pitch || 0) + '">' +
          '<span class="wk-hs-dot"></span>' +
          '<span>' + esc(hs.label || L3('→ Next Room', '→ 下一房间', '→ Siguiente sala')) + '</span>' +
          delBtn +
        '</button>';
      }).join('');
      projectHotspots();
    }

    function loadPanorama(url, hotspots, smoothTransition) {
      if (!url) return Promise.resolve();
      setHotspotsDom(hotspots);
      if (state.activeUrl === url && state.activeTexture) {
        syncSize();
        renderOnce();
        return Promise.resolve();
      }
      if (smoothTransition && fadeVeil && fadeVeil.classList) {
        fadeVeil.classList.add('on');
        state.targetFov = clamp(state.fov - 12, 38, 100);
        scheduleTick();
      } else if (loadingBox) {
        loadingBox.style.display = 'flex';
      }

      return new Promise(function (resolve, reject) {
        var img = new Image();
        img.crossOrigin = 'anonymous';
        img.onload = function () {
          if (state.disposed) { resolve(); return; }
          var tex = new THREE.Texture(img);
          if (THREE.SRGBColorSpace) tex.colorSpace = THREE.SRGBColorSpace;
          tex.minFilter = THREE.LinearFilter;
          tex.magFilter = THREE.LinearFilter;
          tex.generateMipmaps = false;
          tex.needsUpdate = true;

          if (state.activeTexture) {
            try { state.activeTexture.dispose(); } catch (e) {}
          }
          state.activeTexture = tex;
          state.activeUrl = url;
          sphereMat.map = tex;
          sphereMat.needsUpdate = true;

          if (smoothTransition) {
            state.fov = 82;
            state.targetFov = 75;
          }
          if (loadingBox) loadingBox.style.display = 'none';
          if (fadeVeil && fadeVeil.classList) fadeVeil.classList.remove('on');
          syncSize();
          renderOnce();
          scheduleTick();
          resolve();
        };
        img.onerror = function () {
          if (loadingBox) loadingBox.style.display = 'none';
          if (fadeVeil && fadeVeil.classList) fadeVeil.classList.remove('on');
          reject(new Error('Panorama texture load failed'));
        };
        img.src = url;
      });
    }

    function activePointerCount() {
      return Object.keys(state.pointers).length;
    }

    function pointerPairDistance() {
      var keys = Object.keys(state.pointers);
      if (keys.length < 2) return 0;
      var a = state.pointers[keys[0]], b = state.pointers[keys[1]];
      return Math.hypot(a.x - b.x, a.y - b.y);
    }

    viewport.addEventListener('pointerdown', function (ev) {
      if (ev.target && ev.target.closest && ev.target.closest('.wk-hotspot-pill,.wk-hud-controls,.wk-link-popover')) return;
      state.pointers[ev.pointerId] = { x: ev.clientX, y: ev.clientY };
      if (viewport.setPointerCapture) {
        try { viewport.setPointerCapture(ev.pointerId); } catch (e) {}
      }
      if (activePointerCount() === 2) {
        state.pinchStartDist = pointerPairDistance();
        state.pinchStartFov = state.targetFov;
        state.dragging = false;
        return;
      }
      state.dragging = true;
      state.downX = state.lastX = ev.clientX;
      state.downY = state.lastY = ev.clientY;
      state.movedPx = 0;
      if (!st.linkMode && viewport.classList) viewport.classList.add('dragging');
    });

    viewport.addEventListener('pointermove', function (ev) {
      if (state.pointers[ev.pointerId]) {
        state.pointers[ev.pointerId].x = ev.clientX;
        state.pointers[ev.pointerId].y = ev.clientY;
      }
      if (activePointerCount() >= 2 && state.pinchStartDist > 8) {
        var curDist = pointerPairDistance();
        if (curDist > 8) {
          state.targetFov = clamp(state.pinchStartFov * (state.pinchStartDist / curDist), 35, 100);
          scheduleTick();
        }
        return;
      }
      if (!state.dragging) return;
      var dx = ev.clientX - state.lastX;
      var dy = ev.clientY - state.lastY;
      state.lastX = ev.clientX;
      state.lastY = ev.clientY;
      state.movedPx += Math.hypot(dx, dy);
      var speed = (state.fov / 75) * 0.16;
      state.targetYaw -= dx * speed;
      state.targetPitch = clamp(state.targetPitch + dy * speed, -85, 85);
      scheduleTick();
    });

    function endPointer(ev) {
      delete state.pointers[ev.pointerId];
      if (viewport.classList) viewport.classList.remove('dragging');
      var wasClick = state.dragging && state.movedPx < 6;
      state.dragging = false;
      if (wasClick && st.linkMode && st.panos.length >= 2) {
        var coords = unprojectScreenToYawPitch(ev.clientX, ev.clientY);
        openLinkPopoverAt(coords);
      }
    }

    viewport.addEventListener('pointerup', endPointer);
    viewport.addEventListener('pointercancel', endPointer);

    viewport.addEventListener('wheel', function (ev) {
      if (ev.target && ev.target.closest && ev.target.closest('.wk-link-popover')) return;
      ev.preventDefault();
      var delta = ev.deltaY > 0 ? 5.5 : -5.5;
      state.targetFov = clamp(state.targetFov + delta, 35, 100);
      scheduleTick();
    }, { passive: false });

    var ro = null;
    if (typeof ResizeObserver !== 'undefined') {
      ro = new ResizeObserver(function () {
        syncSize();
        renderOnce();
      });
      ro.observe(viewport);
    }

    syncSize();
    renderOnce();

    return {
      loadPanorama: loadPanorama,
      setHotspots: setHotspotsDom,
      zoomBy: function (deltaDeg) {
        state.targetFov = clamp(state.targetFov + deltaDeg, 35, 100);
        scheduleTick();
      },
      resetView: function () {
        state.targetYaw = 0;
        state.targetPitch = 0;
        state.targetFov = 75;
        scheduleTick();
      },
      resize: function () {
        syncSize();
        renderOnce();
      },
      getCameraState: function () {
        return { yaw: state.yaw, pitch: state.pitch, fov: state.fov };
      },
      dispose: function () {
        state.disposed = true;
        if (state.rafId) window.cancelAnimationFrame(state.rafId);
        if (ro) ro.disconnect();
        if (state.activeTexture) {
          try { state.activeTexture.dispose(); } catch (e) {}
        }
        try { sphereGeom.dispose(); sphereMat.dispose(); renderer.dispose(); } catch (e2) {}
      }
    };
  }

  /* ------------------------------------------------------------------ Manifest & Report Sync */
  function combinedTours() {
    var out = [], seen = {};
    (st.tours || []).forEach(function (t) {
      if (t && t.embedUrl && !seen[t.embedUrl]) {
        seen[t.embedUrl] = 1;
        out.push(t);
      }
    });
    extractReportTours(getRep()).forEach(function (t) {
      if (t && t.embedUrl && !seen[t.embedUrl]) {
        seen[t.embedUrl] = 1;
        out.push(t);
      }
    });
    return out;
  }

  function walkinAssetCount() {
    return (st.panos ? st.panos.length : 0) + combinedTours().length;
  }

  function applyManifestToReport(manifest) {
    if (!manifest || !manifest.ok) return;
    st.manifestLoaded = true;
    st.items = Array.isArray(manifest.items) ? manifest.items : [];
    st.panos = Array.isArray(manifest.panos)
      ? manifest.panos
      : st.items.filter(function (it) { return Boolean(it && it.isPano360); });
    st.tours = Array.isArray(manifest.tours) ? manifest.tours : [];

    if (st.panos.length && (!st.activePanoId || !st.panos.some(function (p) { return p.id === st.activePanoId; }))) {
      st.activePanoId = st.panos[0].id;
    }
    if (!st.panos.length && combinedTours().length) {
      st.subMode = 'tour';
    } else if (st.panos.length && st.subMode !== 'tour') {
      st.subMode = 'pano';
    }

    var rep = getRep();
    if (rep) {
      if (!rep.propertyMedia || typeof rep.propertyMedia !== 'object') {
        rep.propertyMedia = {};
      }
      if (st.items.length) {
        rep.propertyMedia.items = st.items.slice();
      }
      if (typeof manifest.hasSalePhotos === 'boolean') {
        rep.propertyMedia.hasSalePhotos = manifest.hasSalePhotos;
      }
      if (manifest.mediaStatus) {
        rep.propertyMedia.mediaStatus = manifest.mediaStatus;
      }
      if (manifest.provenanceNote) {
        rep.propertyMedia.provenanceNote = manifest.provenanceNote;
      }
      // Keep only neutral map/curbside external links when verified coordinates exist
      var latRaw = (rep.geo && rep.geo.refined && rep.geo.refined.lat) != null
        ? rep.geo.refined.lat
        : (rep.geo && rep.geo.lat);
      var lonRaw = (rep.geo && rep.geo.refined && rep.geo.refined.lon) != null
        ? rep.geo.refined.lon
        : (rep.geo && rep.geo.lon);
      var lat = typeof latRaw === 'number' && isFinite(latRaw) ? latRaw : parseFloat(latRaw);
      var lon = typeof lonRaw === 'number' && isFinite(lonRaw) ? lonRaw : parseFloat(lonRaw);
      if (isFinite(lat) && isFinite(lon)) {
        rep.propertyMedia.externalGalleryLinks = [
          { label: 'Street View 360°', url: 'https://www.google.com/maps?layer=c&cbll=' + lat.toFixed(6) + ',' + lon.toFixed(6), badge: 'Curbside' },
          { label: 'Oblique Aerial', url: 'https://www.bing.com/maps?cp=' + lat.toFixed(6) + '~' + lon.toFixed(6) + '&lvl=19&style=o', badge: '45° View' }
        ];
      } else {
        rep.propertyMedia.externalGalleryLinks = [];
      }
      if (rep.modules && rep.modules.propertyMedia && rep.modules.propertyMedia.data) {
        rep.modules.propertyMedia.data.items = rep.propertyMedia.items;
      }
      window.__REPORT_DATA__ = rep;
    }

    refreshWorkspacePreservingHeroMode();
    if (st.heroMode === 'walkin') {
      renderWalkinStageContent();
    }
  }

  function fetchManifest() {
    if (!st.slug || !isLiveHttpPage()) return Promise.resolve(null);
    return fetch('/api/media/' + encodeURIComponent(st.slug) + '/manifest', {
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
      cache: 'no-store'
    })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (j) {
        if (j && j.ok) applyManifestToReport(j);
        return j;
      })
      .catch(function () { return null; });
  }

  function refreshWorkspacePreservingHeroMode() {
    var savedMode = st.heroMode;
    if (window.OpenEstateWorkspace && typeof window.OpenEstateWorkspace.refreshAll === 'function') {
      window.OpenEstateWorkspace.refreshAll();
    }
    enhanceHeroMediaDom();
    applyHeroModeClasses(savedMode);
  }

  /* ------------------------------------------------------------------ Hero Bar & Mode Switching */
  function ensureStageRoot() {
    if (st.stageRoot) return st.stageRoot;
    var wrap = document.createElement('div');
    wrap.id = 'heroWalkinStage';
    wrap.className = 'wk-stage-shell hero-parked';
    st.stageRoot = wrap;
    return wrap;
  }

  function applyHeroModeClasses(mode) {
    st.heroMode = (mode === 'walkin' || mode === 'photos' || mode === 'maps') ? mode : 'maps';
    var gal = byId('gallery');
    var bento = byId('photoBentoGrid');
    var walkin = ensureStageRoot();

    if (gal && gal.classList) gal.classList.toggle('hero-parked', st.heroMode !== 'maps');
    if (bento && bento.classList) bento.classList.toggle('hero-parked', st.heroMode !== 'photos');
    if (walkin && walkin.classList) walkin.classList.toggle('hero-parked', st.heroMode !== 'walkin');

    qAll('#heroMediaSwitcher [data-media-mode]').forEach(function (btn) {
      var bm = btn.getAttribute('data-media-mode');
      if (bm === 'maps' || bm === 'photos' || bm === 'walkin') {
        btn.classList.toggle('on', bm === st.heroMode);
      }
    });

    if (st.heroMode === 'walkin') {
      renderWalkinStageContent();
      if (st.viewer) {
        setTimeout(function () { if (st.viewer) st.viewer.resize(); }, 30);
      }
    }
  }

  function decorateBentoTilesWith360Badges() {
    var bento = byId('photoBentoGrid');
    if (!bento || !st.items.length) return;
    qAll('.bento-tile[data-photo-idx]', bento).forEach(function (tile) {
      if (tile.querySelector('.bento-pano-chip')) return;
      var idx = +(tile.getAttribute('data-photo-idx') || 0);
      var item = st.items[idx];
      if (item && item.isPano360) {
        var chip = document.createElement('button');
        chip.type = 'button';
        chip.className = 'bento-pano-chip';
        chip.innerHTML = '🚪 ' + span3('360° Walk-In', '360° 实景漫游', '360° Recorrido');
        chip.addEventListener('click', function (ev) {
          ev.stopPropagation();
          st.activePanoId = item.id;
          st.subMode = 'pano';
          applyHeroModeClasses('walkin');
        });
        tile.appendChild(chip);
      }
    });
  }

  function enhanceHeroMediaDom() {
    if (st.enhancing) return;
    var mount = byId('heroMediaBento');
    var switcher = byId('heroMediaSwitcher');
    if (!mount || !switcher) return;

    st.enhancing = true;
    try {
      var tabsRow = switcher.querySelector('.hm-tabs');
      if (tabsRow && !byId('mediaTabWalkin')) {
        var count = walkinAssetCount();
        var walkBtn = document.createElement('button');
        walkBtn.type = 'button';
        walkBtn.id = 'mediaTabWalkin';
        walkBtn.className = 'hm-tab' + (st.heroMode === 'walkin' ? ' on' : '');
        walkBtn.setAttribute('data-media-mode', 'walkin');
        walkBtn.innerHTML =
          '🚪 ' + span3('3D Walk-In', '三维实景漫游', 'Recorrido 3D') +
          (count > 0 ? '<span class="wk-tab-badge" id="walkinTabCount">' + count + '</span>' : '');

        var refNode = byId('mediaTab3d') || byId('mediaTabSlides') || null;
        if (refNode && refNode.parentNode === tabsRow) {
          tabsRow.insertBefore(walkBtn, refNode);
        } else {
          tabsRow.appendChild(walkBtn);
        }

        walkBtn.addEventListener('click', function (ev) {
          ev.stopPropagation();
          applyHeroModeClasses('walkin');
        });

        // Hook sibling Map & Photos buttons so clicking them always parks #heroWalkinStage cleanly
        qAll('[data-media-mode="maps"], [data-media-mode="photos"]', tabsRow).forEach(function (sib) {
          if (sib._wkBound) return;
          sib._wkBound = true;
          sib.addEventListener('click', function () {
            var targetMode = sib.getAttribute('data-media-mode') === 'photos' ? 'photos' : 'maps';
            st.heroMode = targetMode;
            var walkStage = ensureStageRoot();
            if (walkStage && walkStage.classList) walkStage.classList.add('hero-parked');
            walkBtn.classList.remove('on');
          });
        });
      } else if (byId('mediaTabWalkin')) {
        var existingBadge = byId('walkinTabCount');
        var cnt = walkinAssetCount();
        if (cnt > 0) {
          if (existingBadge) existingBadge.textContent = String(cnt);
          else {
            var b = document.createElement('span');
            b.className = 'wk-tab-badge';
            b.id = 'walkinTabCount';
            b.textContent = String(cnt);
            byId('mediaTabWalkin').appendChild(b);
          }
        } else if (existingBadge && existingBadge.parentNode) {
          existingBadge.parentNode.removeChild(existingBadge);
        }
      }

      // Add "+ Upload Photos / 360°" button in Hero media bar & Bento provenance bar
      var portalsRow = switcher.querySelector('.hm-portals');
      if (portalsRow && !byId('heroUploadMediaBtn')) {
        var upBtn = document.createElement('button');
        upBtn.type = 'button';
        upBtn.id = 'heroUploadMediaBtn';
        upBtn.className = 'wk-upload-pill';
        upBtn.innerHTML = '＋ ' + span3('Upload Photos / 360°', '上传照片 / 360°', 'Subir Fotos / 360°');
        upBtn.addEventListener('click', function () { openUploadModal(); });
        portalsRow.insertBefore(upBtn, portalsRow.firstChild);
      }

      var provBar = mount.querySelector('.bento-prov-bar');
      if (provBar && !byId('bentoUploadMediaBtn')) {
        var bentoUp = document.createElement('button');
        bentoUp.type = 'button';
        bentoUp.id = 'bentoUploadMediaBtn';
        bentoUp.className = 'wk-upload-pill';
        bentoUp.innerHTML = '＋ ' + span3('Upload Photos / 360°', '上传照片 / 360°', 'Subir Fotos / 360°');
        bentoUp.addEventListener('click', function () { openUploadModal(); });
        provBar.appendChild(bentoUp);
      }

      var swMapBtn = byId('switchToInteractiveMapsBtn');
      if (swMapBtn && !swMapBtn._wkBound) {
        swMapBtn._wkBound = true;
        swMapBtn.addEventListener('click', function () {
          applyHeroModeClasses('maps');
        });
      }

      // Mount persistent #heroWalkinStage inside #heroMediaBento
      var stageEl = ensureStageRoot();
      if (stageEl.parentNode !== mount) {
        mount.appendChild(stageEl);
      }

      decorateBentoTilesWith360Badges();
      applyHeroModeClasses(st.heroMode);
    } finally {
      st.enhancing = false;
    }
  }

  /* ------------------------------------------------------------------ Walk-In Stage Renderer */
  function activePanoObj() {
    if (!st.panos.length) return null;
    for (var i = 0; i < st.panos.length; i++) {
      if (st.panos[i].id === st.activePanoId) return st.panos[i];
    }
    st.activePanoId = st.panos[0].id;
    return st.panos[0];
  }

  function destroyViewerIfPresent() {
    if (st.viewer) {
      try { st.viewer.dispose(); } catch (e) {}
      st.viewer = null;
    }
  }

  function renderWalkinStageContent() {
    var root = ensureStageRoot();
    var panos = st.panos || [];
    var tours = combinedTours();

    if (!panos.length && tours.length) st.subMode = 'tour';
    else if (panos.length && !tours.length) st.subMode = 'pano';

    // Keep existing WebGL canvas alive if already mounted in pano mode and switching rooms/lang
    var canReusePanoDom = st.subMode === 'pano' && panos.length > 0 && Boolean(byId('walkinWebglCanvas'));
    if (!canReusePanoDom) {
      destroyViewerIfPresent();
    }

    var curPano = activePanoObj();
    var topbarHtml =
      '<div class="wk-topbar">' +
        '<div class="wk-topbar-left">' +
          (panos.length > 0
            ? '<button type="button" class="wk-seg-btn' + (st.subMode === 'pano' ? ' on' : '') + '" data-wk-sub="pano">🌐 ' + span3('360° Walk-In (' + panos.length + ')', '360° 房间实景 (' + panos.length + ')', 'Salas 360° (' + panos.length + ')') + '</button>'
            : '') +
          (tours.length > 0
            ? '<button type="button" class="wk-seg-btn' + (st.subMode === 'tour' ? ' on' : '') + '" data-wk-sub="tour">🏠 ' + span3('3D Tour (' + tours.length + ')', '三维漫游 (' + tours.length + ')', 'Tour 3D (' + tours.length + ')') + '</button>'
            : '') +
          (st.subMode === 'pano' && curPano
            ? '<span class="wk-room-title-pill" id="walkinActiveRoomPill">📍 ' + esc(curPano.roomTag || '360° Room') + '</span>'
            : '') +
          (!panos.length && !tours.length
            ? '<span class="wk-room-title-pill">🚪 ' + span3('Hero 3D Walk-In Studio', '三维实景漫游工作台', 'Estudio de Recorrido 3D') + '</span>'
            : '') +
        '</div>' +
        '<div class="wk-topbar-right">' +
          (st.subMode === 'pano' && panos.length >= 2
            ? '<button type="button" class="wk-seg-btn' + (st.linkMode ? ' warn-on' : '') + '" id="walkinAddLinkBtn" aria-pressed="' + (st.linkMode ? 'true' : 'false') + '">🔗 ' + span3(st.linkMode ? 'Done Linking' : 'Add Room Link', st.linkMode ? '完成添加跳转' : '添加房间跳转点', st.linkMode ? 'Listo' : 'Añadir Enlace') + '</button>'
            : '') +
          '<button type="button" class="wk-seg-btn" id="walkinToggleTourBarBtn">🔗 ' + span3('Paste 3D Tour URL', '粘贴三维漫游链接', 'Pegar URL de Tour 3D') + '</button>' +
          '<button type="button" class="wk-upload-pill" id="walkinStageUploadBtn">＋ ' + span3('Upload 360° / Photos', '上传 360° / 照片', 'Subir 360° / Fotos') + '</button>' +
        '</div>' +
      '</div>';

    var tourBarHtml = st.showTourInputBar
      ? '<div class="wk-tour-bar">' +
          '<input type="url" id="walkinTourUrlInput" placeholder="' + esc(L3('https://my.matterport.com/show/?m=... or iGUIDE / Kuula / CloudPano / RICOH360 / Giraffe360 / Cupix', '粘贴 Matterport / iGUIDE / Kuula / CloudPano / RICOH360 / Giraffe360 / Cupix 漫游链接', 'Pegue enlace Matterport / iGUIDE / Kuula / CloudPano / RICOH360 / Giraffe360 / Cupix')) + '">' +
          '<button type="button" class="wk-pri-btn" id="walkinTourSaveBtn">' + span3('Save Tour', '保存漫游', 'Guardar Tour') + '</button>' +
        '</div>'
      : '';

    var msgHtml = st.tourMsg
      ? '<div class="wk-msg ' + (st.tourErr ? 'err' : 'ok') + '">' + esc(st.tourMsg) + '</div>'
      : '';

    if (canReusePanoDom) {
      // Update header, tour bar, banners, and room strip without tearing down the Three.js WebGL context
      var hdrHolder = root.querySelector('.wk-hdr-slot');
      if (hdrHolder) hdrHolder.innerHTML = topbarHtml + tourBarHtml + msgHtml;
      var stripHolder = byId('walkinRoomStripWrap');
      if (stripHolder) stripHolder.innerHTML = buildRoomStripHtml(panos, curPano);
      syncPanoOverlayAndRoom(curPano);
      bindStageToolbarEvents(root);
      return;
    }

    if (panos.length === 0 && tours.length === 0) {
      root.innerHTML =
        '<div class="wk-hdr-slot">' + topbarHtml + tourBarHtml + msgHtml + '</div>' +
        '<div class="wk-viewport">' +
          '<div class="wk-empty" id="walkinEmptyState">' +
            '<div class="wk-empty-card">' +
              '<span class="wk-empty-eyebrow">' + span3('Data-Honest Interior Walk-In', '真实房源实景漫游 · 绝不伪造室内图', 'Recorrido Interior Verificado') + '</span>' +
              '<h3>' + span3('No 360° Room Panoramas or 3D Walkthrough Recorded Yet', '该房源暂未收录 360° 全景照片或三维漫游链接', 'Aún no hay panoramas 360° ni recorrido 3D para esta casa') + '</h3>' +
              '<p>' + span3(
                'Upload 2:1 equirectangular 360° room photos (RICOH Theta, Insta360, or phone panorama) to walk room-to-room with hotspot arrows, or paste a supported 3D tour link.',
                '上传 2:1 等距柱状 360° 全景照片（支持 RICOH Theta、Insta360 或手机全景自动识别）即可开启房间级漫游并串联热点箭头；也可直接粘贴受支持的三维漫游链接。',
                'Suba fotos panorámicas equirectangulares 360° (RICOH Theta, Insta360 o panorama móvil) para caminar entre habitaciones, o pegue un enlace de recorrido 3D compatible.'
              ) + '</p>' +
              '<div class="wk-empty-actions">' +
                '<div><button type="button" class="wk-pri-btn" id="walkinEmptyUploadBtn">📷 ' + span3('Upload 360° Room Photos', '上传 360° 房间全景图', 'Subir Fotos 360° de Salas') + '</button></div>' +
                '<div class="wk-empty-tour-row">' +
                  '<input type="url" id="walkinEmptyTourInput" placeholder="' + esc(L3('Paste Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360, or Cupix URL…', '粘贴 Matterport、iGUIDE、Kuula、CloudPano、RICOH360、Giraffe360 或 Cupix 链接…', 'Pegue enlace Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360 o Cupix…')) + '">' +
                  '<button type="button" class="wk-sec-btn" id="walkinEmptyTourSaveBtn">' + span3('Embed 3D Tour', '嵌入三维漫游', 'Incrustar Tour 3D') + '</button>' +
                '</div>' +
              '</div>' +
            '</div>' +
          '</div>' +
        '</div>';
      bindStageToolbarEvents(root);
      return;
    }

    if (st.subMode === 'tour' && tours.length > 0) {
      var tIdx = clamp(st.activeTourIdx, 0, tours.length - 1);
      var activeTour = tours[tIdx];
      var tourChips = tours.length > 1
        ? '<div class="wk-room-strip">' + tours.map(function (t, i) {
            return '<button type="button" class="wk-seg-btn' + (i === tIdx ? ' on' : '') + '" data-wk-tour-idx="' + i + '">' + esc(t.vendor) + ' · ' + esc(t.label || ('#' + (i + 1))) + '</button>';
          }).join('') + '</div>'
        : '';

      root.innerHTML =
        '<div class="wk-hdr-slot">' + topbarHtml + tourBarHtml + msgHtml + '</div>' +
        '<div class="wk-viewport" id="walkinTourViewport">' +
          '<iframe class="wk-iframe" id="walkinTourIframe" src="' + esc(activeTour.embedUrl) + '" title="' + esc(activeTour.vendor + ' 3D Walkthrough') + '" loading="lazy" allowfullscreen allow="fullscreen; xr-spatial-tracking; gyroscope; accelerometer" referrerpolicy="strict-origin-when-cross-origin" sandbox="allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms allow-presentation"></iframe>' +
          '<div class="wk-hud-controls">' +
            '<a class="wk-hud-btn" style="text-decoration:none;display:inline-flex;align-items:center" href="' + esc(activeTour.openUrl) + '" target="_blank" rel="noopener noreferrer">' + esc(activeTour.vendor) + ' ↗</a>' +
            '<button type="button" class="wk-hud-btn" id="walkinFullscreenBtn" title="Fullscreen">⛶</button>' +
          '</div>' +
        '</div>' +
        tourChips;
      bindStageToolbarEvents(root);
      return;
    }

    // Mode A: Native Three.js 360° Walk-In Photo Mode
    root.innerHTML =
      '<div class="wk-hdr-slot">' + topbarHtml + tourBarHtml + msgHtml + '</div>' +
      '<div class="wk-viewport ' + (st.linkMode ? 'link-cursor' : 'drag-cursor') + '" id="walkinPanoViewport">' +
        '<canvas class="wk-canvas" id="walkinWebglCanvas"></canvas>' +
        '<div class="wk-fade-veil" id="walkinFadeVeil"></div>' +
        '<div class="wk-hotspots-layer" id="walkinHotspotsLayer"></div>' +
        '<div id="walkinLinkBannerSlot"></div>' +
        '<div id="walkinPopoverSlot"></div>' +
        '<div class="wk-loading" id="walkinLoadingBox">' + span3('Loading 360° room sphere…', '正在加载 360° 球面全景…', 'Cargando esfera panorámica 360°…') + '</div>' +
        '<div class="wk-hud-hint">' + span3('Drag to look around · Scroll or pinch to zoom', '拖动环视房间 · 滚轮或双指缩放视角', 'Arrastre para mirar · Rueda o pellizco para zoom') + '</div>' +
        '<div class="wk-hud-controls">' +
          '<button type="button" class="wk-hud-btn" id="walkinZoomInBtn" aria-label="Zoom in">＋</button>' +
          '<span class="wk-hud-fov" id="walkinFovReadout">75°</span>' +
          '<button type="button" class="wk-hud-btn" id="walkinZoomOutBtn" aria-label="Zoom out">−</button>' +
          '<button type="button" class="wk-hud-btn" id="walkinResetViewBtn" title="Reset view">⟲</button>' +
          '<button type="button" class="wk-hud-btn" id="walkinFullscreenBtn" title="Fullscreen">⛶</button>' +
        '</div>' +
      '</div>' +
      '<div id="walkinRoomStripWrap">' + buildRoomStripHtml(panos, curPano) + '</div>';

    bindStageToolbarEvents(root);
    bootOrUpdateThreePano(curPano);
  }

  function buildRoomStripHtml(panos, curPano) {
    if (!panos || !panos.length) return '';
    return '<div class="wk-room-strip" id="walkinRoomStrip">' +
      panos.map(function (p) {
        var isOn = curPano && p.id === curPano.id;
        var hsCnt = Array.isArray(p.hotspots) ? p.hotspots.length : 0;
        var subTxt = hsCnt > 0
          ? L3(hsCnt + ' link' + (hsCnt > 1 ? 's' : ''), hsCnt + ' 个房间跳转', hsCnt + ' enlace' + (hsCnt > 1 ? 's' : ''))
          : '360°';
        return '<button type="button" class="wk-room-card' + (isOn ? ' on' : '') + '" data-wk-room="' + esc(p.id) + '">' +
          '<img src="' + esc(p.thumbUrl || p.url) + '" alt="' + esc(p.roomTag) + '">' +
          '<span class="wk-rc-meta">' +
            '<span class="wk-rc-name">' + esc(p.roomTag || '360° Room') + '</span>' +
            '<span class="wk-rc-sub">' + esc(subTxt) + '</span>' +
          '</span>' +
        '</button>';
      }).join('') +
    '</div>';
  }

  function syncPanoOverlayAndRoom(curPano) {
    var vp = byId('walkinPanoViewport');
    if (vp && vp.classList) {
      vp.classList.toggle('link-cursor', Boolean(st.linkMode));
      vp.classList.toggle('drag-cursor', !st.linkMode);
    }
    var bannerSlot = byId('walkinLinkBannerSlot');
    if (bannerSlot) {
      bannerSlot.innerHTML = st.linkMode
        ? '<div class="wk-link-banner">🎯 ' + span3('Click anywhere in the 360° view to place a room-to-room arrow', '点击 360° 画面任意位置以放置通往其他房间的跳转箭头', 'Haga clic en cualquier punto de la vista 360° para añadir una flecha a otra sala') + '</div>'
        : '';
    }
    if (!st.linkMode) {
      var popSlot = byId('walkinPopoverSlot');
      if (popSlot) popSlot.innerHTML = '';
      st.pendingLinkCoords = null;
    }
    if (curPano) {
      bootOrUpdateThreePano(curPano);
    }
  }

  function bootOrUpdateThreePano(curPano) {
    if (!curPano) return;
    var viewport = byId('walkinPanoViewport');
    var canvas = byId('walkinWebglCanvas');
    if (!viewport || !canvas) return;

    if (st.viewer) {
      st.viewer.loadPanorama(curPano.url, curPano.hotspots || [], true).catch(function () {});
      return;
    }

    loadThreeEngine().then(function (THREE) {
      if (!byId('walkinWebglCanvas')) return;
      st.viewer = createPanoViewer(THREE, {
        viewport: viewport,
        canvas: canvas,
        hotspotsLayer: byId('walkinHotspotsLayer'),
        fovReadout: byId('walkinFovReadout'),
        fadeVeil: byId('walkinFadeVeil'),
        loadingBox: byId('walkinLoadingBox')
      });
      var latestPano = activePanoObj() || curPano;
      return st.viewer.loadPanorama(latestPano.url, latestPano.hotspots || [], false);
    }).catch(function () {
      var lb = byId('walkinLoadingBox');
      if (lb) {
        lb.innerHTML = '<span>' + span3('WebGL 360° renderer could not initialize in this browser.', '当前浏览器环境未能启动 WebGL 360° 渲染器。', 'No se pudo iniciar el visor WebGL 360° en este navegador.') + '</span>';
      }
    });
  }

  function openLinkPopoverAt(coords) {
    var slot = byId('walkinPopoverSlot');
    var vp = byId('walkinPanoViewport');
    var cur = activePanoObj();
    if (!slot || !vp || !cur) return;
    st.pendingLinkCoords = coords;
    var others = st.panos.filter(function (p) { return p.id !== cur.id; });
    if (!others.length) return;

    var w = vp.clientWidth || 640, h = vp.clientHeight || 400;
    var leftPx = clamp(coords.relX, 16, Math.max(20, w - 316));
    var topPx = clamp(coords.relY, 16, Math.max(20, h - 195));
    var firstTarget = others[0];

    slot.innerHTML =
      '<div class="wk-link-popover" id="walkinLinkPopover" style="left:' + Math.round(leftPx) + 'px;top:' + Math.round(topPx) + 'px">' +
        '<label>' + span3('Target 360° Room', '目标跳转房间', 'Sala 360° de destino') + '</label>' +
        '<select id="walkinLinkTargetSelect">' +
          others.map(function (o) {
            return '<option value="' + esc(o.id) + '" data-room-name="' + esc(o.roomTag) + '">' + esc(o.roomTag) + '</option>';
          }).join('') +
        '</select>' +
        '<label>' + span3('Arrow Label', '热点按钮文字', 'Etiqueta del botón') + '</label>' +
        '<input type="text" id="walkinLinkLabelInput" maxlength="60" value="' + esc('→ ' + firstTarget.roomTag) + '">' +
        '<div style="display:flex;justify-content:flex-end;gap:6px">' +
          '<button type="button" class="wk-sec-btn" id="walkinLinkCancelBtn" style="padding:5px 10px;font-size:11.8px">' + span3('Cancel', '取消', 'Cancelar') + '</button>' +
          '<button type="button" class="wk-pri-btn" id="walkinLinkSaveBtn" style="padding:5px 12px;font-size:11.8px">' + span3('Save Link', '保存跳转', 'Guardar') + '</button>' +
        '</div>' +
      '</div>';

    var sel = byId('walkinLinkTargetSelect');
    var lblInp = byId('walkinLinkLabelInput');
    if (sel && lblInp) {
      sel.addEventListener('change', function () {
        var opt = sel.options[sel.selectedIndex];
        var rName = opt ? opt.getAttribute('data-room-name') : '';
        if (rName) lblInp.value = '→ ' + rName;
      });
    }
    var cancelBtn = byId('walkinLinkCancelBtn');
    if (cancelBtn) {
      cancelBtn.addEventListener('click', function () {
        slot.innerHTML = '';
        st.pendingLinkCoords = null;
      });
    }
    var saveBtn = byId('walkinLinkSaveBtn');
    if (saveBtn) {
      saveBtn.addEventListener('click', function () {
        if (!sel || !st.pendingLinkCoords) return;
        var targetId = sel.value;
        var label = (lblInp && lblInp.value ? lblInp.value.trim() : '') || '→ Room';
        var nextHotspots = (Array.isArray(cur.hotspots) ? cur.hotspots.slice() : []).concat([{
          yaw: st.pendingLinkCoords.yaw,
          pitch: st.pendingLinkCoords.pitch,
          targetId: targetId,
          label: label
        }]);
        persistHotspots(cur.id, nextHotspots);
      });
    }
  }

  function persistHotspots(fromId, nextHotspots) {
    if (!st.slug || st.hotspotSaving) return;
    st.hotspotSaving = true;
    fetch('/api/media/' + encodeURIComponent(st.slug) + '/hotspots', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ fromId: fromId, hotspots: nextHotspots })
    })
      .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, body: j }; }); })
      .then(function (res) {
        st.hotspotSaving = false;
        if (res.ok && res.body && res.body.ok) {
          var saved = res.body.hotspots || nextHotspots;
          st.panos.forEach(function (p) { if (p.id === fromId) p.hotspots = saved; });
          st.items.forEach(function (it) { if (it.id === fromId) it.hotspots = saved; });
          var popSlot = byId('walkinPopoverSlot');
          if (popSlot) popSlot.innerHTML = '';
          st.pendingLinkCoords = null;
          if (st.viewer && st.activePanoId === fromId) {
            st.viewer.setHotspots(saved);
          }
          var stripHolder = byId('walkinRoomStripWrap');
          if (stripHolder) stripHolder.innerHTML = buildRoomStripHtml(st.panos, activePanoObj());
          bindRoomStripClicks();
        }
      })
      .catch(function () { st.hotspotSaving = false; });
  }

  function submitTourUrl(rawUrl) {
    var trimmed = String(rawUrl || '').trim();
    if (!trimmed) return;
    var localCheck = parseSupportedTourUrl(trimmed, null);
    if (!localCheck) {
      st.tourErr = true;
      st.tourMsg = L3(
        'Unsupported link. Supported 3D tour hosts: Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360, and Cupix.',
        '不支持该链接。仅支持 Matterport、iGUIDE、Kuula、CloudPano、RICOH360、Giraffe360 与 Cupix 三维漫游。',
        'Enlace no compatible. Servicios admitidos: Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360 y Cupix.'
      );
      renderWalkinStageContent();
      return;
    }

    if (!st.slug || !isLiveHttpPage()) {
      st.tours = [localCheck].concat(st.tours);
      st.subMode = 'tour';
      st.activeTourIdx = 0;
      st.showTourInputBar = false;
      st.tourErr = false;
      st.tourMsg = L3('3D walkthrough ready.', '三维漫游已就绪。', 'Recorrido 3D listo.');
      enhanceHeroMediaDom();
      renderWalkinStageContent();
      return;
    }

    fetch('/api/media/' + encodeURIComponent(st.slug) + '/tour', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ url: trimmed })
    })
      .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, body: j }; }); })
      .then(function (res) {
        if (res.ok && res.body && res.body.ok) {
          st.tours = Array.isArray(res.body.tours) ? res.body.tours : [res.body.tour];
          st.subMode = 'tour';
          st.activeTourIdx = 0;
          st.showTourInputBar = false;
          st.tourErr = false;
          st.tourMsg = L3('Saved ' + res.body.tour.vendor + ' 3D walkthrough.', '已保存 ' + res.body.tour.vendor + ' 三维漫游。', 'Recorrido 3D de ' + res.body.tour.vendor + ' guardado.');
          enhanceHeroMediaDom();
          renderWalkinStageContent();
        } else {
          st.tourErr = true;
          st.tourMsg = (res.body && res.body.error) || L3('Could not save tour URL.', '无法保存漫游链接。', 'No se pudo guardar el enlace.');
          renderWalkinStageContent();
        }
      })
      .catch(function () {
        st.tourErr = true;
        st.tourMsg = L3('Network error while saving tour URL.', '保存漫游链接时网络异常。', 'Error de red al guardar el enlace.');
        renderWalkinStageContent();
      });
  }

  function bindRoomStripClicks() {
    var root = ensureStageRoot();
    qAll('[data-wk-room]', root).forEach(function (btn) {
      if (btn._wkRoomBound) return;
      btn._wkRoomBound = true;
      btn.addEventListener('click', function () {
        var rid = btn.getAttribute('data-wk-room');
        if (rid && rid !== st.activePanoId) {
          st.activePanoId = rid;
          renderWalkinStageContent();
        }
      });
    });
  }

  function bindStageToolbarEvents(root) {
    qAll('[data-wk-sub]', root).forEach(function (b) {
      b.onclick = function () {
        st.subMode = b.getAttribute('data-wk-sub') === 'tour' ? 'tour' : 'pano';
        st.tourMsg = '';
        renderWalkinStageContent();
      };
    });

    qAll('[data-wk-tour-idx]', root).forEach(function (b) {
      b.onclick = function () {
        st.activeTourIdx = +(b.getAttribute('data-wk-tour-idx') || 0);
        renderWalkinStageContent();
      };
    });

    var linkBtn = byId('walkinAddLinkBtn');
    if (linkBtn) {
      linkBtn.onclick = function () {
        st.linkMode = !st.linkMode;
        renderWalkinStageContent();
        if (st.viewer) {
          var cur = activePanoObj();
          if (cur) st.viewer.setHotspots(cur.hotspots || []);
        }
      };
    }

    var toggleTourBtn = byId('walkinToggleTourBarBtn');
    if (toggleTourBtn) {
      toggleTourBtn.onclick = function () {
        st.showTourInputBar = !st.showTourInputBar;
        st.tourMsg = '';
        renderWalkinStageContent();
      };
    }

    var stageUpBtn = byId('walkinStageUploadBtn');
    if (stageUpBtn) stageUpBtn.onclick = function () { openUploadModal(); };
    var emptyUpBtn = byId('walkinEmptyUploadBtn');
    if (emptyUpBtn) emptyUpBtn.onclick = function () { openUploadModal(); };

    var saveTourBtn = byId('walkinTourSaveBtn');
    if (saveTourBtn) {
      saveTourBtn.onclick = function () {
        var inp = byId('walkinTourUrlInput');
        if (inp) submitTourUrl(inp.value);
      };
    }
    var emptySaveTourBtn = byId('walkinEmptyTourSaveBtn');
    if (emptySaveTourBtn) {
      emptySaveTourBtn.onclick = function () {
        var inp = byId('walkinEmptyTourInput');
        if (inp) submitTourUrl(inp.value);
      };
    }

    var zin = byId('walkinZoomInBtn');
    if (zin) zin.onclick = function () { if (st.viewer) st.viewer.zoomBy(-10); };
    var zout = byId('walkinZoomOutBtn');
    if (zout) zout.onclick = function () { if (st.viewer) st.viewer.zoomBy(10); };
    var rst = byId('walkinResetViewBtn');
    if (rst) rst.onclick = function () { if (st.viewer) st.viewer.resetView(); };
    var fsBtn = byId('walkinFullscreenBtn');
    if (fsBtn) {
      fsBtn.onclick = function () {
        var target = byId('walkinPanoViewport') || byId('walkinTourViewport') || root;
        if (document.fullscreenElement) {
          if (document.exitFullscreen) document.exitFullscreen();
        } else if (target && target.requestFullscreen) {
          target.requestFullscreen().catch(function () {});
        }
      };
    }

    var hsLayer = byId('walkinHotspotsLayer');
    if (hsLayer && !hsLayer._wkHsBound) {
      hsLayer._wkHsBound = true;
      hsLayer.addEventListener('click', function (ev) {
        var delEl = ev.target && ev.target.closest ? ev.target.closest('[data-del-hs]') : null;
        if (delEl) {
          ev.stopPropagation();
          var delIdx = +(delEl.getAttribute('data-del-hs') || 0);
          var cur = activePanoObj();
          if (cur && Array.isArray(cur.hotspots)) {
            var copy = cur.hotspots.slice();
            copy.splice(delIdx, 1);
            persistHotspots(cur.id, copy);
          }
          return;
        }
        var pill = ev.target && ev.target.closest ? ev.target.closest('.wk-hotspot-pill') : null;
        if (!pill) return;
        var tid = pill.getAttribute('data-target-id');
        if (tid && st.panos.some(function (p) { return p.id === tid; })) {
          st.activePanoId = tid;
          renderWalkinStageContent();
        }
      });
    }

    bindRoomStripClicks();
  }

  /* ------------------------------------------------------------------ Upload Sheet / Dialog */
  var ROOM_PRESETS = [
    ['Living Room', '客厅', 'Sala'],
    ['Kitchen', '厨房', 'Cocina'],
    ['Primary Bedroom', '主卧', 'Dormitorio Principal'],
    ['Bathroom', '卫浴', 'Baño'],
    ['Office / Den', '书房', 'Estudio'],
    ['Backyard', '后院', 'Patio Trasero'],
    ['Front Exterior', '正立面外景', 'Fachada Exterior']
  ];

  function ensureUploadModal() {
    if (st.uploadModalEl) return st.uploadModalEl;
    var modal = document.createElement('div');
    modal.id = 'walkinUploadModal';
    modal.className = 'wk-modal-bg';
    modal.hidden = true;
    if (document.body) document.body.appendChild(modal);
    st.uploadModalEl = modal;
    return modal;
  }

  function openUploadModal() {
    var modal = ensureUploadModal();
    st.uploadQueue = [];
    st.uploading = false;
    modal.hidden = false;
    renderUploadModal();
  }

  function closeUploadModal() {
    if (!st.uploadModalEl) return;
    st.uploadModalEl.hidden = true;
    st.uploadModalEl.innerHTML = '';
  }

  function inspectAndQueueFiles(fileList) {
    var files = Array.prototype.slice.call(fileList || []);
    files.slice(0, 24).forEach(function (file) {
      if (!file || !/^image\/(jpeg|png|webp)$/i.test(file.type || '')) return;
      var reader = new FileReader();
      reader.onload = function () {
        var dataUrl = String(reader.result || '');
        var probe = new Image();
        probe.onload = function () {
          var w = probe.naturalWidth || 0;
          var h = probe.naturalHeight || 0;
          var ratio = h > 0 ? w / h : 0;
          var auto360 = w >= 1600 && ratio >= 1.90 && ratio <= 2.10;
          var stem = String(file.name || 'Room').replace(/\.[^.]+$/, '').replace(/[_-]+/g, ' ').trim();
          st.uploadQueue.push({
            file: file,
            dataUrl: dataUrl,
            width: w,
            height: h,
            autoPano: auto360,
            isPano360: auto360 ? true : null,
            roomTag: stem ? stem.slice(0, 48) : (auto360 ? L3('Living Room', '客厅', 'Sala') : L3('Property Photo', '实拍照片', 'Foto de Propiedad')),
            category: auto360 ? 'staging' : 'exterior',
            status: 'ready',
            err: ''
          });
          renderUploadModal();
        };
        probe.src = dataUrl;
      };
      reader.readAsDataURL(file);
    });
  }

  function renderUploadModal() {
    var modal = ensureUploadModal();
    if (modal.hidden) return;

    var presetsHtml = ROOM_PRESETS.map(function (rp) {
      var lbl = L3(rp[0], rp[1], rp[2]);
      return '<button type="button" class="wk-seg-btn" style="padding:3px 9px;font-size:11px" data-room-preset="' + esc(lbl) + '">' + esc(lbl) + '</button>';
    }).join('');

    var queueHtml = st.uploadQueue.map(function (q, idx) {
      var is360 = q.isPano360 === true || (q.isPano360 === null && q.autoPano);
      return '<div class="wk-q-item">' +
        '<img src="' + esc(q.dataUrl) + '" alt="">' +
        '<div>' +
          '<div class="wk-q-fields">' +
            '<input type="text" data-q-room="' + idx + '" value="' + esc(q.roomTag) + '" placeholder="' + esc(L3('Room tag (e.g. Kitchen)', '房间名称（如：厨房）', 'Etiqueta (ej. Cocina)')) + '">' +
            '<select data-q-cat="' + idx + '">' +
              '<option value="staging"' + (q.category === 'staging' ? ' selected' : '') + '>' + esc(L3('Interior / Room', '室内 / 房间', 'Interior / Sala')) + '</option>' +
              '<option value="exterior"' + (q.category === 'exterior' ? ' selected' : '') + '>' + esc(L3('Exterior / Yard', '外立面 / 庭院', 'Exterior / Patio')) + '</option>' +
            '</select>' +
            '<label style="display:inline-flex;align-items:center;gap:4px;font-size:11.6px;font-weight:700;color:#334155;cursor:pointer">' +
              '<input type="checkbox" data-q-pano="' + idx + '"' + (is360 ? ' checked' : '') + '> 360°' +
            '</label>' +
            (q.autoPano ? '<span class="wk-auto360-pill">✦ ' + esc(L3('Auto 360° (' + q.width + '×' + q.height + ')', '自动识别 360° (' + q.width + '×' + q.height + ')', 'Auto 360° (' + q.width + '×' + q.height + ')')) + '</span>' : '<span style="font-size:11px;color:#64748b">' + q.width + '×' + q.height + '</span>') +
          '</div>' +
          (q.err ? '<div style="color:#b91c1c;font-size:11.5px;margin-top:4px">' + esc(q.err) + '</div>' : '') +
        '</div>' +
        '<button type="button" class="wk-sec-btn" style="padding:4px 9px;font-size:11.5px" data-q-rm="' + idx + '" aria-label="Remove">✕</button>' +
      '</div>';
    }).join('');

    modal.innerHTML =
      '<div class="wk-modal-dialog" role="dialog" aria-modal="true" aria-label="Upload Property Photos & 360 Panoramas">' +
        '<div class="wk-modal-hdr">' +
          '<h3>📷 ' + span3('Upload Property Photos & 360° Walk-In Panoramas', '上传房产实拍照片与 360° 全景图', 'Subir Fotos y Panoramas 360°') + '</h3>' +
          '<button type="button" class="wk-sec-btn" id="wkUploadCloseBtn" style="padding:5px 10px">✕</button>' +
        '</div>' +
        '<div class="wk-modal-body">' +
          '<div class="wk-dropzone" id="wkDropzone">' +
            '<input type="file" id="walkinFileInput" accept="image/jpeg,image/png,image/webp" multiple style="display:none">' +
            '<div style="font-weight:800;font-size:14px;color:#0f172a;margin-bottom:4px">' + span3('Drag & drop JPEG / PNG / WebP photos here, or click to browse', '拖拽 JPEG / PNG / WebP 照片至此，或点击选择文件', 'Arrastre fotos JPEG / PNG / WebP aquí o haga clic para elegir') + '</div>' +
            '<div style="font-size:12px;color:#64748b">' + span3('2:1 equirectangular panoramas (≥1600 px wide) are automatically detected as 360° Walk-In rooms (up to 12 MB each).', '宽高比约 2:1（宽度 ≥1600 像素）的全景照片将自动识别为 360° 实景漫游房间（单张上限 12 MB）。', 'Los panoramas 2:1 (≥1600 px de ancho) se detectan automáticamente como salas 360° (máx. 12 MB).') + '</div>' +
          '</div>' +
          (st.uploadQueue.length
            ? '<div style="display:flex;align-items:center;gap:5px;flex-wrap:wrap"><span style="font-size:11.3px;font-weight:700;color:#64748b">' + span3('Quick tag:', '快捷命名：', 'Etiqueta rápida:') + '</span>' + presetsHtml + '</div>' +
              '<div class="wk-q-list">' + queueHtml + '</div>'
            : '') +
        '</div>' +
        '<div class="wk-modal-foot">' +
          '<span style="font-size:12px;color:#64748b">' + esc(L3(st.uploadQueue.length + ' file(s) queued · stored locally on this server', '已选 ' + st.uploadQueue.length + ' 张影像 · 本地持久化存储', st.uploadQueue.length + ' archivo(s) en cola')) + '</span>' +
          '<div style="display:flex;gap:8px">' +
            '<button type="button" class="wk-sec-btn" id="wkUploadCancelBtn">' + span3('Cancel', '取消', 'Cancelar') + '</button>' +
            '<button type="button" class="wk-pri-btn" id="wkUploadSubmitBtn"' + (!st.uploadQueue.length || st.uploading ? ' disabled' : '') + '>' +
              (st.uploading ? span3('Uploading…', '正在上传…', 'Subiendo…') : span3('Upload Now', '开始上传', 'Subir Ahora')) +
            '</button>' +
          '</div>' +
        '</div>' +
      '</div>';

    var c1 = byId('wkUploadCloseBtn'), c2 = byId('wkUploadCancelBtn');
    if (c1) c1.onclick = closeUploadModal;
    if (c2) c2.onclick = closeUploadModal;
    modal.onclick = function (ev) { if (ev.target === modal) closeUploadModal(); };

    var dz = byId('wkDropzone');
    var fi = byId('walkinFileInput');
    if (dz && fi) {
      dz.onclick = function () { fi.click(); };
      fi.onchange = function () { if (fi.files && fi.files.length) inspectAndQueueFiles(fi.files); };
      dz.ondragover = function (ev) { ev.preventDefault(); dz.classList.add('dragover'); };
      dz.ondragleave = function () { dz.classList.remove('dragover'); };
      dz.ondrop = function (ev) {
        ev.preventDefault();
        dz.classList.remove('dragover');
        if (ev.dataTransfer && ev.dataTransfer.files) inspectAndQueueFiles(ev.dataTransfer.files);
      };
    }

    qAll('[data-room-preset]', modal).forEach(function (b) {
      b.onclick = function () {
        var tag = b.getAttribute('data-room-preset') || '';
        if (st.uploadQueue.length) {
          st.uploadQueue[st.uploadQueue.length - 1].roomTag = tag;
          renderUploadModal();
        }
      };
    });
    qAll('[data-q-room]', modal).forEach(function (inp) {
      inp.oninput = function () {
        var i = +(inp.getAttribute('data-q-room') || 0);
        if (st.uploadQueue[i]) st.uploadQueue[i].roomTag = inp.value;
      };
    });
    qAll('[data-q-cat]', modal).forEach(function (sel) {
      sel.onchange = function () {
        var i = +(sel.getAttribute('data-q-cat') || 0);
        if (st.uploadQueue[i]) st.uploadQueue[i].category = sel.value;
      };
    });
    qAll('[data-q-pano]', modal).forEach(function (chk) {
      chk.onchange = function () {
        var i = +(chk.getAttribute('data-q-pano') || 0);
        if (st.uploadQueue[i]) st.uploadQueue[i].isPano360 = Boolean(chk.checked);
      };
    });
    qAll('[data-q-rm]', modal).forEach(function (rm) {
      rm.onclick = function () {
        var i = +(rm.getAttribute('data-q-rm') || 0);
        st.uploadQueue.splice(i, 1);
        renderUploadModal();
      };
    });

    var subBtn = byId('wkUploadSubmitBtn');
    if (subBtn) subBtn.onclick = uploadQueuedFiles;
  }

  function uploadQueuedFiles() {
    if (!st.slug || !st.uploadQueue.length || st.uploading) return;
    st.uploading = true;
    renderUploadModal();

    var uploadedAnyPanoId = '';
    var idx = 0;

    function next() {
      if (idx >= st.uploadQueue.length) {
        st.uploading = false;
        var hasErr = st.uploadQueue.some(function (q) { return Boolean(q.err); });
        fetchManifest().then(function () {
          if (!hasErr) {
            closeUploadModal();
            if (uploadedAnyPanoId) {
              st.activePanoId = uploadedAnyPanoId;
              st.subMode = 'pano';
              applyHeroModeClasses('walkin');
            } else {
              applyHeroModeClasses('photos');
            }
          } else {
            renderUploadModal();
          }
        });
        return;
      }

      var cur = st.uploadQueue[idx];
      fetch('/api/media/' + encodeURIComponent(st.slug) + '/upload', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({
          filename: cur.file ? cur.file.name : 'upload.jpg',
          dataBase64: cur.dataUrl,
          roomTag: cur.roomTag,
          caption: cur.roomTag,
          category: cur.category,
          isPano360: cur.isPano360
        })
      })
        .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, body: j }; }); })
        .then(function (res) {
          if (res.ok && res.body && res.body.ok && res.body.item) {
            cur.err = '';
            if (res.body.item.isPano360) uploadedAnyPanoId = res.body.item.id;
          } else {
            cur.err = (res.body && res.body.error) || L3('Upload failed', '上传失败', 'Error al subir');
          }
          idx += 1;
          next();
        })
        .catch(function () {
          cur.err = L3('Network error during upload', '上传网络异常', 'Error de red');
          idx += 1;
          next();
        });
    }

    next();
  }

  /* ------------------------------------------------------------------ Boot & Observers */
  function bootWalkinMedia() {
    injectStyles();
    st.slug = resolveSlug();
    enhanceHeroMediaDom();

    // Observe #heroMediaBento re-renders from report_workspace.js (language switches, workspace refresh)
    var bentoMount = byId('heroMediaBento');
    if (typeof MutationObserver !== 'undefined' && bentoMount) {
      var mo = new MutationObserver(function () {
        if (st.enhancing) return;
        if (!byId('mediaTabWalkin') || (st.stageRoot && st.stageRoot.parentNode !== bentoMount)) {
          enhanceHeroMediaDom();
        }
      });
      mo.observe(bentoMount, { childList: true });
    }

    if (typeof MutationObserver !== 'undefined' && document.documentElement && document.documentElement.nodeType === 1) {
      new MutationObserver(function () {
        if (st.heroMode === 'walkin') renderWalkinStageContent();
      }).observe(document.documentElement, { attributes: true, attributeFilter: ['lang'] });
    }

    if (document.addEventListener) {
      document.addEventListener('fullscreenchange', function () {
        if (st.viewer) st.viewer.resize();
      });
      document.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape' && st.uploadModalEl && !st.uploadModalEl.hidden) {
          closeUploadModal();
        }
      });
    }

    fetchManifest();
  }

  window.OpenEstateWalkinMedia = {
    openWalkin: function (panoId) {
      if (panoId) st.activePanoId = panoId;
      applyHeroModeClasses('walkin');
    },
    openUpload: openUploadModal,
    refreshManifest: fetchManifest,
    parseSupportedTourUrl: parseSupportedTourUrl
  };

  if (document.readyState === 'loading' && document.addEventListener) {
    document.addEventListener('DOMContentLoaded', bootWalkinMedia);
  } else {
    bootWalkinMedia();
  }
})();
