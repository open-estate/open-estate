/* Open Estate Shared Frontend Core (`window.OpenEstateCore`).
 * Shared formatting, i18n/cookie resolution, report JSON access, singleton Three.js 0.170.0 loader,
 * allowlisted 3D tour URL parser, and OS color-scheme watcher across report and workspace scripts.
 */
(function () {
  'use strict';
  if (typeof window === 'undefined') return;
  if (window.OpenEstateCore && window.OpenEstateCore.__v === 1) return;

  var THREE_URLS = [
    'https://cdn.jsdelivr.net/npm/three@0.170.0/build/three.module.min.js',
    'https://unpkg.com/three@0.170.0/build/three.module.min.js'
  ];

  function isN(x) {
    return typeof x === 'number' && isFinite(x);
  }

  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  function normLang(raw) {
    var l = String(raw || '').trim().toLowerCase();
    if (!l) return '';
    if (l.indexOf('zh') === 0 || l === 'cn') return 'zh';
    if (l.indexOf('es') === 0) return 'es';
    if (l.indexOf('en') === 0) return 'en';
    return '';
  }

  function readCookie(name) {
    try {
      if (typeof document === 'undefined' || typeof document.cookie !== 'string' || !document.cookie) return '';
      var parts = document.cookie.split(';');
      for (var i = 0; i < parts.length; i++) {
        var kv = parts[i].trim();
        var eq = kv.indexOf('=');
        if (eq > 0 && kv.slice(0, eq).trim() === name) {
          return decodeURIComponent(kv.slice(eq + 1).trim());
        }
      }
    } catch (e) {}
    return '';
  }

  function hasCookieKey(name) {
    try {
      if (typeof document === 'undefined' || typeof document.cookie !== 'string' || !document.cookie) return false;
      var parts = document.cookie.split(';');
      for (var i = 0; i < parts.length; i++) {
        var kv = parts[i].trim();
        var eq = kv.indexOf('=');
        if (eq > 0 && kv.slice(0, eq).trim() === name) return true;
      }
    } catch (e) {}
    return false;
  }

  function initDisplayLinksState() {
    if (typeof document === 'undefined') return false;
    var enabled = false;
    try {
      var s = (typeof location !== 'undefined' && location.search) ? String(location.search) : '';
      var m = s.match(/[?&]display_link=([^&#]*)/i);
      var q = m ? decodeURIComponent(m[1]).trim().toLowerCase() : '';
      if (q === '1' || q === 'true') {
        document.cookie = 'display_link=1; path=/; max-age=2592000; SameSite=Lax';
        enabled = true;
      } else if (q === '0' || q === 'false') {
        document.cookie = 'display_link=0; path=/; max-age=0; SameSite=Lax';
        enabled = false;
      } else if (hasCookieKey('display_link')) {
        enabled = readCookie('display_link') === '1';
      } else if (document.documentElement && document.documentElement.getAttribute('data-display-links') === '1') {
        enabled = true;
      }
    } catch (e) {
      enabled = readCookie('display_link') === '1';
    }
    try {
      if (document.documentElement) {
        document.documentElement.dataset.displayLinks = enabled ? '1' : '0';
      }
    } catch (e2) {}
    return enabled;
  }

  function isDisplayLinksEnabled() {
    if (hasCookieKey('display_link')) {
      return readCookie('display_link') === '1';
    }
    if (typeof document !== 'undefined' && document.documentElement) {
      return document.documentElement.getAttribute('data-display-links') === '1';
    }
    return false;
  }

  function syncDebugLinksToggle() {
    if (typeof document === 'undefined' || typeof document.querySelectorAll !== 'function') return;
    var on = isDisplayLinksEnabled();
    var label = on ? 'Debug: Reference Links On (display_link=1)' : 'Debug: Reference Links Off';
    var btns = document.querySelectorAll('[data-oe-debug-links-toggle]');
    for (var i = 0; i < btns.length; i++) {
      var b = btns[i];
      b.textContent = label;
      b.setAttribute('aria-pressed', on ? 'true' : 'false');
    }
  }

  function setDisplayLinksCookie(on) {
    var enabled = Boolean(on);
    try {
      if (typeof document !== 'undefined') {
        document.cookie = 'display_link=' + (enabled ? '1' : '0') + '; path=/; max-age=' + (enabled ? 2592000 : 0) + '; SameSite=Lax';
        if (document.documentElement) {
          document.documentElement.dataset.displayLinks = enabled ? '1' : '0';
        }
      }
      syncDebugLinksToggle();
      if (typeof window !== 'undefined' && typeof CustomEvent === 'function') {
        window.dispatchEvent(new CustomEvent('oe:display-links-change', { detail: { enabled: enabled } }));
      }
    } catch (e) {}
    return enabled;
  }

  function toggleDisplayLinks(opts) {
    var options = opts || {};
    var next = typeof options.enabled === 'boolean' ? options.enabled : !isDisplayLinksEnabled();
    setDisplayLinksCookie(next);
    var shouldReload = options.reload !== false;
    if (shouldReload && typeof location !== 'undefined') {
      try {
        var u = new URL(location.href);
        if (location.protocol === 'file:') {
          if (next) u.searchParams.set('display_link', '1');
          else u.searchParams.delete('display_link');
          location.href = u.toString();
          return next;
        }
        if (u.searchParams.has('display_link')) {
          u.searchParams.delete('display_link');
          if (typeof history !== 'undefined' && typeof history.replaceState === 'function') {
            history.replaceState(null, '', u.pathname + u.search + u.hash);
          } else {
            location.href = u.toString();
            return next;
          }
        }
      } catch (e) {}
      location.reload();
    }
    return next;
  }

  initDisplayLinksState();

  function langCode() {
    var found = '';
    try {
      if (typeof LANG === 'string' && LANG) found = normLang(LANG);
    } catch (e) {}
    if (!found) {
      try {
        if (window.i18next && typeof window.i18next.language === 'string') {
          found = normLang(window.i18next.language);
        }
      } catch (e2) {}
    }
    if (!found && typeof document !== 'undefined') {
      try {
        if (typeof document.querySelector === 'function') {
          var onBtn = document.querySelector('#langSwitch button.on[data-lang]');
          if (onBtn && typeof onBtn.getAttribute === 'function') {
            found = normLang(onBtn.getAttribute('data-lang'));
          }
        }
      } catch (e3) {}
      if (!found && document.documentElement) {
        var de = document.documentElement;
        var attr = (typeof de.getAttribute === 'function' && de.getAttribute('lang')) || de.lang || '';
        found = normLang(attr);
      }
    }
    if (!found) {
      try {
        if (window.localStorage) found = normLang(window.localStorage.getItem('open_estate_lang'));
      } catch (e4) {}
    }
    if (!found) found = normLang(readCookie('open_estate_lang'));
    return found || 'en';
  }

  function numLocale(langOverride) {
    var l = langOverride ? (normLang(langOverride) || 'en') : langCode();
    return l === 'zh' ? 'zh-CN' : l === 'es' ? 'es-US' : 'en-US';
  }

  function fUsd(v, langOverride) {
    if (!isN(v)) return '—';
    try {
      return new Intl.NumberFormat(numLocale(langOverride), {
        style: 'currency',
        currency: 'USD',
        maximumFractionDigits: 0
      }).format(Math.round(v));
    } catch (e) {
      return (v < 0 ? '-$' : '$') + Math.round(Math.abs(v)).toLocaleString();
    }
  }

  function fUsdK(v, langOverride) {
    if (!isN(v)) return '—';
    var a = Math.abs(v);
    if (a >= 1e6) return (v < 0 ? '−$' : '$') + (a / 1e6).toFixed(2).replace(/\.?0+$/, '') + 'M';
    if (a >= 1e3) return (v < 0 ? '−$' : '$') + Math.round(a / 1e3) + 'K';
    return fUsd(v, langOverride);
  }

  function fNum(v, d, langOverride) {
    if (!isN(v)) return '—';
    try {
      return new Intl.NumberFormat(numLocale(langOverride), {
        maximumFractionDigits: d || 0,
        minimumFractionDigits: 0
      }).format(v);
    } catch (e) {
      return String(Math.round(v));
    }
  }

  var _cachedDomRep = null;
  function getRep() {
    try {
      if (typeof R !== 'undefined' && R && typeof R === 'object') return R;
    } catch (e) {}
    if (window.R && typeof window.R === 'object') return window.R;
    if (window.__REPORT_DATA__ && typeof window.__REPORT_DATA__ === 'object') return window.__REPORT_DATA__;
    if (_cachedDomRep && typeof _cachedDomRep === 'object') return _cachedDomRep;
    try {
      if (typeof document !== 'undefined' && typeof document.getElementById === 'function') {
        var el = document.getElementById('report-data');
        var txt = el && el.textContent ? String(el.textContent).trim() : '';
        if (txt && txt.charAt(0) === '{') {
          _cachedDomRep = JSON.parse(txt);
          window.__REPORT_DATA__ = _cachedDomRep;
          return _cachedDomRep;
        }
      }
    } catch (e2) {}
    return null;
  }

  function mod(rep, k) {
    var m = rep && rep.modules && rep.modules[k];
    return (m && m.data) || (rep && rep[k]) || null;
  }

  var _threeSingleton = null;
  function loadThreeJs(customUrls) {
    if (window.THREE && window.THREE.WebGLRenderer) {
      return Promise.resolve(window.THREE);
    }
    var urls = Array.isArray(customUrls) && customUrls.length ? customUrls.slice() : THREE_URLS.slice();
    var isDefault = urls.length === THREE_URLS.length && urls[0] === THREE_URLS[0];
    if (isDefault && _threeSingleton) return _threeSingleton;

    function tryAt(i) {
      if (i >= urls.length) return Promise.reject(new Error('3D engine unavailable'));
      return import(urls[i]).then(function (m) {
        var resolved = (m && m.WebGLRenderer) ? m : (m && m.default && m.default.WebGLRenderer ? m.default : m);
        if (!window.THREE && resolved && resolved.WebGLRenderer) {
          try { window.THREE = resolved; } catch (e) {}
        }
        return resolved;
      }, function () {
        return tryAt(i + 1);
      });
    }

    var p = tryAt(0).catch(function (err) {
      if (isDefault) _threeSingleton = null;
      throw err;
    });
    if (isDefault) _threeSingleton = p;
    return p;
  }

  function parseTourUrl(raw, opts) {
    var options = opts || {};
    var txt = String(raw || '').trim();
    if (!txt || txt.length > 600) return null;
    if (/^\/\//.test(txt)) txt = 'https:' + txt;
    var u;
    try { u = new URL(txt); } catch (e) { return null; }
    var allowHttp = Boolean(options.allowHttp);
    if (u.protocol !== 'https:' && !(allowHttp && u.protocol === 'http:')) return null;
    if (u.username || u.password) return null;

    var host = u.hostname.toLowerCase().replace(/\.+$/, '');
    var hostNoWww = host.replace(/^www\./, '');
    var segs = u.pathname.split('/').filter(Boolean);
    var tokRe = /^[A-Za-z0-9_-]{3,120}$/;
    var mk = function (vendor, embedUrl, openUrl, kind) {
      return {
        vendor: vendor,
        embedUrl: embedUrl,
        openUrl: openUrl,
        kind: kind || '3d',
        generic: vendor === 'Listing 3D tour',
        label: options.fallbackLabel || (vendor + ' 3D Tour')
      };
    };

    if (hostNoWww === 'my.matterport.com' || hostNoWww === 'matterport.com') {
      var mpId = u.searchParams.get('m') || '';
      if (!mpId && segs.length >= 2 && (segs[0] === 'show' || segs[0] === 'models' || segs[0] === 'space')) mpId = segs[1];
      if (!mpId && segs.length >= 3 && segs[0] === 'discover' && segs[1] === 'space') mpId = segs[2];
      if (!/^[A-Za-z0-9_-]{5,36}$/.test(mpId)) return null;
      return mk('Matterport', 'https://my.matterport.com/show/?m=' + mpId + '&play=1', 'https://my.matterport.com/show/?m=' + mpId);
    }
    if (hostNoWww === 'youriguide.com') {
      var igId = (segs[0] === 'embed' && segs.length === 2) ? segs[1] : (segs.length === 1 && segs[0] !== 'embed' ? segs[0] : '');
      if (!tokRe.test(igId)) return null;
      return mk('iGUIDE', 'https://youriguide.com/embed/' + igId + '?unbranded=1', 'https://youriguide.com/' + igId);
    }
    if (hostNoWww === 'kuula.co') {
      var isCol = segs[0] === 'share' && segs[1] === 'collection' && segs.length === 3;
      var kuId = isCol ? segs[2] : ((segs[0] === 'share' || segs[0] === 'post') && segs.length === 2 && segs[1] !== 'collection' ? segs[1] : '');
      if (!tokRe.test(kuId)) return null;
      var kuBase = 'https://kuula.co/share/' + (isCol ? 'collection/' : '') + kuId;
      return mk('Kuula', kuBase + '?logo=0&info=0&fs=1&vr=0&sd=1&thumbs=1', kuBase);
    }
    if (host === 'app.cloudpano.com') {
      var cpId = (segs[0] === 'tours' && segs.length === 2) ? segs[1] : '';
      if (!tokRe.test(cpId)) return null;
      return mk('CloudPano', 'https://app.cloudpano.com/tours/' + cpId, 'https://app.cloudpano.com/tours/' + cpId);
    }
    if (hostNoWww === 'ricoh360.com' || host === 'tours.ricoh360.com' || host === 'view.ricoh360.com') {
      var rcHost = 'www.ricoh360.com', rcId = '';
      if ((host === 'tours.ricoh360.com' || host === 'view.ricoh360.com') && segs.length === 1) {
        rcHost = host; rcId = segs[0];
      } else if (segs[0] === 'tours' && segs.length === 2) {
        rcHost = (host === 'tours.ricoh360.com' || host === 'view.ricoh360.com') ? host : 'www.ricoh360.com';
        rcId = segs[1];
      }
      if (!tokRe.test(rcId)) return null;
      var rcUrl = rcHost === 'www.ricoh360.com' ? ('https://www.ricoh360.com/tours/' + rcId) : ('https://' + rcHost + '/' + rcId);
      return mk(options.ricohLabel || 'RICOH360', rcUrl, rcUrl);
    }
    if (hostNoWww === 'giraffe360.com' || host === 'tour.giraffe360.com') {
      var gfId = ((segs[0] === 'tours' || segs[0] === 'tour') && segs.length === 2) ? segs[1] : (segs.length === 1 ? segs[0] : '');
      if (!tokRe.test(gfId)) return null;
      return mk('Giraffe360', 'https://tour.giraffe360.com/' + gfId + '/', 'https://tour.giraffe360.com/' + gfId + '/');
    }
    if (host === 'players.cupix.com') {
      var cxId = (segs[0] === 'p' && segs.length === 2) ? segs[1] : (segs.length === 1 ? segs[0] : '');
      if (!tokRe.test(cxId)) return null;
      return mk('Cupix', 'https://players.cupix.com/p/' + cxId, 'https://players.cupix.com/p/' + cxId);
    }
    if (options.allowVideo) {
      var m;
      if (hostNoWww === 'youtube.com' || hostNoWww === 'm.youtube.com' || hostNoWww === 'youtu.be' || hostNoWww === 'youtube-nocookie.com') {
        var vid = hostNoWww === 'youtu.be' ? (segs[0] || '') : (u.searchParams.get('v') || ((m = u.pathname.match(/^\/(?:embed|shorts|live)\/([A-Za-z0-9_-]{6,20})/)) && m[1]));
        if (vid && /^[A-Za-z0-9_-]{6,20}$/.test(vid)) {
          return mk('YouTube', 'https://www.youtube-nocookie.com/embed/' + vid, 'https://www.youtube.com/watch?v=' + vid, 'video');
        }
        return null;
      }
      if (hostNoWww === 'vimeo.com' || hostNoWww === 'player.vimeo.com') {
        m = u.pathname.match(/(?:^|\/)(\d{6,12})(?:\/|$)/);
        return m ? mk('Vimeo', 'https://player.vimeo.com/video/' + m[1], 'https://vimeo.com/' + m[1], 'video') : null;
      }
    }
    return null;
  }

  function scrubProse(s) {
    var raw = String(s == null ? '' : s);
    if (!raw) return '';
    if (typeof window.scrubPortalProse === 'function') {
      try { return window.scrubPortalProse(raw); } catch (e) {}
    }
    return raw;
  }

  function systemColorScheme() {
    try {
      return typeof window.matchMedia === 'function' && window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    } catch (e) {
      return 'light';
    }
  }

  function onSystemColorSchemeChange(cb) {
    if (typeof cb !== 'function') return function () {};
    try {
      if (typeof window.matchMedia !== 'function') return function () {};
      var mq = window.matchMedia('(prefers-color-scheme: dark)');
      if (!mq) return function () {};
      var handler = function () { cb(mq.matches ? 'dark' : 'light'); };
      if (typeof mq.addEventListener === 'function') {
        mq.addEventListener('change', handler);
        return function () { mq.removeEventListener('change', handler); };
      }
      if (typeof mq.addListener === 'function') {
        mq.addListener(handler);
        return function () { mq.removeListener(handler); };
      }
    } catch (e) {}
    return function () {};
  }

  function bindShareReportButton() {
    if (typeof document === 'undefined') return;
    var shareBtn = document.getElementById('shareReportBtn');
    if (!shareBtn || shareBtn.dataset.dbShareBound) return;
    shareBtn.dataset.dbShareBound = '1';
    var labelFor = function (copied) {
      var l = langCode();
      if (copied) return l === 'zh' ? '✓ 已复制' : l === 'es' ? '✓ Copiado' : '✓ Copied';
      return l === 'zh' ? '分享报告' : l === 'es' ? 'Compartir' : 'Share';
    };
    var syncLabel = function () {
      if (shareBtn.dataset.copying) return;
      var lbl = shareBtn.querySelector('.hblbl');
      if (lbl) lbl.textContent = labelFor(false);
    };
    syncLabel();
    shareBtn.addEventListener('click', function () {
      var cleanUrl = location.origin + location.pathname;
      try {
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(cleanUrl).catch(function () {});
        } else {
          var ta = document.createElement('textarea');
          ta.value = cleanUrl;
          ta.style.position = 'fixed';
          ta.style.opacity = '0';
          document.body.appendChild(ta);
          ta.select();
          document.execCommand('copy');
          ta.remove();
        }
      } catch (e) {}
      shareBtn.dataset.copying = '1';
      var lbl = shareBtn.querySelector('.hblbl');
      if (lbl) lbl.textContent = labelFor(true);
      setTimeout(function () {
        delete shareBtn.dataset.copying;
        syncLabel();
      }, 1800);
    });
    if (typeof MutationObserver !== 'undefined' && document.documentElement) {
      var mo = new MutationObserver(syncLabel);
      mo.observe(document.documentElement, { attributes: true, attributeFilter: ['lang'] });
    }
  }

  function bindDebugLinksDelegation() {
    if (typeof document === 'undefined' || window.__oeDebugLinksBound) return;
    window.__oeDebugLinksBound = true;
    syncDebugLinksToggle();
    document.addEventListener('click', function (ev) {
      var t = ev && ev.target;
      var el = t && t.nodeType === 1 ? t : (t && t.parentElement);
      var btn = el && typeof el.closest === 'function' ? el.closest('[data-oe-debug-links-toggle]') : null;
      if (!btn) return;
      ev.preventDefault();
      toggleDisplayLinks({ reload: true });
    });
  }

  if (typeof document !== 'undefined') {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', function () {
        bindShareReportButton();
        bindDebugLinksDelegation();
      });
    } else {
      bindShareReportButton();
      bindDebugLinksDelegation();
    }
  }

  function scrollChatToMessageStart(logEl, msgEl) {
    if (!logEl) return;
    if (!msgEl) {
      if (isN(logEl.scrollHeight)) logEl.scrollTop = logEl.scrollHeight;
      return;
    }
    try {
      if (typeof logEl.getBoundingClientRect === 'function' && typeof msgEl.getBoundingClientRect === 'function') {
        var logRect = logEl.getBoundingClientRect();
        var msgRect = msgEl.getBoundingClientRect();
        var ch = Number(logEl.clientHeight) || 0;
        if (logRect && msgRect && (logRect.height > 0 || ch > 0)) {
          var padTop = 0;
          try {
            if (typeof getComputedStyle === 'function') {
              padTop = parseFloat(getComputedStyle(logEl).paddingTop) || 0;
            }
          } catch (e) {}
          var clientTop = Number(logEl.clientTop) || 0;
          var curTop = Number(logEl.scrollTop) || 0;
          var targetTop = curTop + (msgRect.top - (logRect.top + clientTop)) - padTop;
          if (isN(targetTop)) {
            var nextTop = Math.max(0, Math.round(targetTop));
            if (isN(logEl.scrollHeight) && ch > 0) {
              nextTop = Math.min(Math.max(0, logEl.scrollHeight - ch), nextTop);
            }
            logEl.scrollTop = nextTop;
            return;
          }
        }
      }
    } catch (e2) {}
    if (typeof msgEl.offsetTop === 'number' && isN(msgEl.offsetTop)) {
      logEl.scrollTop = Math.max(0, msgEl.offsetTop);
    }
  }

  function syncChatLogScroll(logEl, opts) {
    if (!logEl) return;
    var o = opts || {};
    if (!o.hasEntries) {
      logEl.scrollTop = 0;
      return;
    }
    if (o.pending || o.lastRole === 'user') {
      if (isN(logEl.scrollHeight)) logEl.scrollTop = logEl.scrollHeight;
      return;
    }
    scrollChatToMessageStart(logEl, o.msgEl);
  }

  window.OpenEstateCore = {
    __v: 1,
    THREE_URLS: THREE_URLS,
    isN: isN,
    esc: esc,
    readCookie: readCookie,
    isDisplayLinksEnabled: isDisplayLinksEnabled,
    setDisplayLinksCookie: setDisplayLinksCookie,
    toggleDisplayLinks: toggleDisplayLinks,
    syncDebugLinksToggle: syncDebugLinksToggle,
    langCode: langCode,
    numLocale: numLocale,
    fUsd: fUsd,
    fUsdK: fUsdK,
    fNum: fNum,
    getRep: getRep,
    mod: mod,
    loadThreeJs: loadThreeJs,
    parseTourUrl: parseTourUrl,
    scrubProse: scrubProse,
    systemColorScheme: systemColorScheme,
    onSystemColorSchemeChange: onSystemColorSchemeChange,
    scrollChatToMessageStart: scrollChatToMessageStart,
    syncChatLogScroll: syncChatLogScroll
  };
})();
