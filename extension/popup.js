// Popup controller: Manual US address lookup, Deal Room shortlist management, i18next EN/中文 UI, and Server / White-Label settings.

/* global BRAND_CONFIG, resolveBrandConfig, ExtI18n, chrome */
(function initPopup() {
  'use strict';

  let state = {
    lang: 'en',
    serverUrl: BRAND_CONFIG.defaultServerUrl,
    effectiveServerUrl: BRAND_CONFIG.defaultServerUrl,
    brand: BRAND_CONFIG,
    dealRoom: [],
  };

  const $ = (id) => document.getElementById(id);

  function t(key, vars) {
    return ExtI18n.t(key, vars, state.lang);
  }

  function nextLangCycle(cur) {
    return ExtI18n.nextLangCycle(cur);
  }

  function appendLangParam(rawUrl) {
    return ExtI18n.appendLangParam(rawUrl, state.lang);
  }

  function esc(s) {
    return String(s ?? '').replace(/[&<>"']/g, (c) => ({
      '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
    }[c]));
  }

  function fmtCompact(n) {
    return ExtI18n.formatCompactCurrency(n, state.lang);
  }

  function fmtCurrency(n) {
    return ExtI18n.formatCurrency(n, state.lang);
  }

  function sendMsg(msg) {
    return new Promise((resolve) => {
      chrome.runtime.sendMessage(msg, (resp) => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false, error: chrome.runtime.lastError.message });
        } else {
          resolve(resp || { ok: false });
        }
      });
    });
  }

  function applyThemeAndLabels() {
    ExtI18n.setLanguage(state.lang);
    document.documentElement.lang = ExtI18n.normalizeLang(state.lang);
    const brand = state.brand || BRAND_CONFIG;
    document.documentElement.style.setProperty('--brand', brand.accentColor || BRAND_CONFIG.accentColor);
    const logoEl = $('brandLogo');
    if (brand.logoSvg) {
      logoEl.innerHTML = brand.logoSvg;
      logoEl.classList.add('has-mark');
    } else {
      logoEl.textContent = brand.logoGlyph || BRAND_CONFIG.logoGlyph;
      logoEl.classList.remove('has-mark');
    }
    $('brandName').textContent = state.lang === 'zh'
      ? (brand.nameZh || brand.name)
      : state.lang === 'es'
        ? (brand.nameEs || brand.name)
        : brand.name;
    document.title = t('popup.title', { brandName: brand.name });

    $('srvUrlInput').placeholder = BRAND_CONFIG.defaultServerUrl;
    $('addrInput').placeholder = t('popup.addrPlaceholder');
    $('apiKeyInput').placeholder = t('popup.apiKeyPlaceholder');
    $('wlNameInput').placeholder = BRAND_CONFIG.name;
    $('wlColorInput').placeholder = BRAND_CONFIG.accentColor;
    $('langBtn').textContent = t('popup.langSwitchBtn');

    $('lblLookup').textContent = t('popup.lookupTitle');
    $('analyzeBtn').textContent = t('popup.analyzeBtn');
    $('lblDealRoom').textContent = t('popup.dealRoomTitle', {
      count: state.dealRoom.length,
      max: BRAND_CONFIG.maxDealRoomItems,
    });
    $('clearDealRoomBtn').textContent = t('popup.clearBtn');
    $('compareBtn').textContent = state.dealRoom.length >= 2
      ? t('popup.compareBtnReady', { count: state.dealRoom.length })
      : t('popup.compareBtnNeedTwo');
    $('compareBtn').disabled = state.dealRoom.length < 2;

    $('lblSettings').textContent = t('popup.settingsTitle');
    $('lblSrvUrl').textContent = t('popup.srvUrlLabel');
    $('useProdBtn').textContent = t('popup.useProdBtn', { domain: BRAND_CONFIG.domain });
    $('useDevBtn').textContent = t('popup.useLocalDevBtn');
    $('lblApiKey').textContent = t('popup.apiKeyLabel');
    $('lblWlName').textContent = t('popup.wlNameLabel');
    $('lblWlColor').textContent = t('popup.wlColorLabel');
    $('saveSettingsBtn').textContent = t('popup.saveSettingsBtn');

    renderDealRoomList();
  }

  function renderDealRoomList() {
    const box = $('dealRoomList');
    if (!state.dealRoom.length) {
      box.innerHTML = `<div class="empty">${esc(t('popup.emptyDealRoom'))}</div>`;
      return;
    }

    box.innerHTML = state.dealRoom.map((item) => {
      const valTxt = item.isCommercial && item.capRatePct
        ? t('popup.capSuffix', { val: Number(item.capRatePct).toFixed(2) })
        : (item.fairValueMid
            ? t('popup.estPrefix', { val: fmtCompact(item.fairValueMid) })
            : (item.askingPrice
                ? t('popup.askPrefix', { val: fmtCompact(item.askingPrice) })
                : (item.portal || t('popup.savedPortal'))));
      const monthlyPart = item.trueMonthlyCost
        ? ` · ${t('popup.perMonthSuffix', { val: fmtCurrency(item.trueMonthlyCost) })}`
        : '';
      return `<div class="dr-item" data-slug="${esc(item.slug)}">
        <div style="min-width:0;flex:1">
          <div class="dr-addr" title="${esc(item.address)}">${esc(item.address)}</div>
          <div class="dr-meta">${esc(item.portal || t('popup.savedPortal'))} · ${esc(valTxt)}${esc(monthlyPart)}</div>
        </div>
        ${item.overallScore != null ? `<span class="score-chip">${Math.round(item.overallScore)}</span>` : ''}
        <button type="button" class="rm-btn" data-rm="${esc(item.slug)}" title="${esc(t('popup.removeTitle'))}">×</button>
      </div>`;
    }).join('');

    box.querySelectorAll('[data-rm]').forEach((btn) => {
      btn.addEventListener('click', async () => {
        const slug = btn.getAttribute('data-rm');
        const res = await sendMsg({ type: 'DEAL_ROOM_REMOVE', slug });
        if (res?.ok) {
          state.dealRoom = res.dealRoom || [];
          if (res.serverUrl) state.effectiveServerUrl = res.serverUrl;
          applyThemeAndLabels();
        }
      });
    });
  }

  async function refreshHealth(overrideUrl) {
    const dot = $('srvStatus');
    const txt = $('srvStatusText');
    txt.textContent = t('popup.checking');
    const res = await sendMsg({ type: 'CHECK_HEALTH', serverUrl: overrideUrl });
    if (res?.ok) {
      state.effectiveServerUrl = res.serverUrl || state.serverUrl;
      dot.className = 'srv-dot ok';
      const host = String(res.serverUrl || '').replace(/^https?:\/\//, '');
      txt.textContent = res.usedDevFallback
        ? t('popup.connectedDevFallback', { host })
        : t('popup.connected', { host });
    } else {
      dot.className = 'srv-dot err';
      txt.textContent = t('popup.offline');
    }
    return res;
  }

  $('langBtn').addEventListener('click', async () => {
    const next = nextLangCycle(state.lang);
    state.lang = next;
    await sendMsg({ type: 'SET_LANG', lang: next });
    applyThemeAndLabels();
    refreshHealth();
  });

  $('clearDealRoomBtn').addEventListener('click', async () => {
    const res = await sendMsg({ type: 'DEAL_ROOM_CLEAR' });
    if (res?.ok) {
      state.dealRoom = [];
      applyThemeAndLabels();
    }
  });

  $('compareBtn').addEventListener('click', async () => {
    if (state.dealRoom.length < 2) return;
    const base = state.effectiveServerUrl || state.serverUrl;
    const slugs = state.dealRoom.map((x) => x.slug).join(',');
    const url = appendLangParam(`${base}/deal-room?slugs=${encodeURIComponent(slugs).replace(/%2C/gi, ',')}`);
    await sendMsg({ type: 'OPEN_TAB', url });
  });

  $('useProdBtn').addEventListener('click', () => {
    $('srvUrlInput').value = BRAND_CONFIG.defaultServerUrl;
  });

  $('useDevBtn').addEventListener('click', () => {
    $('srvUrlInput').value = BRAND_CONFIG.devFallbackServerUrl || 'http://127.0.0.1:8080';
  });

  let activePollTimer = null;

  function renderLookupResult(addr, d) {
    const out = $('lookupResult');
    const base = state.effectiveServerUrl || state.serverUrl;
    const rawReportUrl = d.urls?.reportUrl || `${base}/report?address=${encodeURIComponent(addr)}`;
    const reportUrl = appendLangParam(rawReportUrl);
    if (d.status === 'ready') {
      const rangeStr = `${fmtCompact(d.fairValue?.low)}–${fmtCompact(d.fairValue?.high)}`;
      const monthlyStr = d.trueMonthlyCost?.total
        ? t('popup.perMonthSuffix', { val: fmtCurrency(d.trueMonthlyCost.total) })
        : '—';
      out.innerHTML = `<div class="dr-item" style="flex-direction:column;align-items:stretch;gap:6px">
        <div style="display:flex;justify-content:space-between;align-items:center;gap:8px">
          <b style="font-size:12.5px">${esc(d.address || addr)}</b>
          ${d.overallScore != null ? `<span class="score-chip">${Math.round(d.overallScore)}/100</span>` : ''}
        </div>
        <div class="dr-meta">${esc(t('popup.fairValueLine', { range: rangeStr, monthly: monthlyStr }))}</div>
        <div style="display:flex;gap:6px">
          <button type="button" class="btn" id="popupSaveDrBtn" style="flex:1">${esc(t('popup.saveToDr'))}</button>
          <button type="button" class="btn pri" id="popupOpenRepBtn" style="flex:1">${esc(t('popup.openFull'))}</button>
        </div>
      </div>`;
      $('popupSaveDrBtn')?.addEventListener('click', async () => {
        const added = await sendMsg({
          type: 'DEAL_ROOM_ADD',
          item: {
            slug: d.slug,
            address: d.address || addr,
            portal: t('popup.manualPortal'),
            fairValueMid: d.fairValue?.mid ?? null,
            overallScore: d.overallScore ?? null,
            trueMonthlyCost: d.trueMonthlyCost?.total ?? null,
            verdict: d.verdict || '',
          },
        });
        if (added?.ok) {
          state.dealRoom = added.dealRoom || [];
          applyThemeAndLabels();
        }
      });
      $('popupOpenRepBtn')?.addEventListener('click', () => {
        sendMsg({ type: 'OPEN_TAB', url: reportUrl });
      });
    } else {
      out.innerHTML = `<div class="dr-item" style="flex-direction:column;align-items:stretch;gap:6px">
        <div><b>${esc(addr)}</b> — <span style="color:var(--brand);font-weight:700">${esc(d.step || t('popup.buildingBackground'))}</span></div>
        <div style="display:flex;gap:6px">
          ${d.slug ? `<button type="button" class="btn" id="popupSaveBuildingDrBtn" style="flex:1">${esc(t('popup.saveToDr'))}</button>` : ''}
          <button type="button" class="btn pri" id="popupOpenBuildBtn" style="flex:1">${esc(t('popup.openFull'))}</button>
        </div>
      </div>`;
      $('popupSaveBuildingDrBtn')?.addEventListener('click', async () => {
        const added = await sendMsg({
          type: 'DEAL_ROOM_ADD',
          item: { slug: d.slug, address: d.address || addr, portal: t('popup.manualPortal') },
        });
        if (added?.ok) {
          state.dealRoom = added.dealRoom || [];
          applyThemeAndLabels();
        }
      });
      $('popupOpenBuildBtn')?.addEventListener('click', () => {
        sendMsg({ type: 'OPEN_TAB', url: reportUrl });
      });
    }
  }

  function schedulePopupPoll(addr, jobId, slug, attempt = 1) {
    if (activePollTimer) clearTimeout(activePollTimer);
    if (attempt > 30) return;
    activePollTimer = setTimeout(async () => {
      const resp = await sendMsg({
        type: 'POLL_JOB',
        jobId,
        slug,
        address: addr,
      });
      if (resp?.ok && resp.data) {
        if (resp.serverUrl) state.effectiveServerUrl = resp.serverUrl;
        renderLookupResult(addr, resp.data);
        if (resp.data.status === 'building') {
          schedulePopupPoll(addr, resp.data.jobId || jobId, resp.data.slug || slug, attempt + 1);
        }
      }
    }, 2200);
  }

  const US_STATE_SET = new Set([
    'AL','AK','AZ','AR','CA','CO','CT','DE','FL','GA','HI','ID','IL','IN','IA','KS',
    'KY','LA','ME','MD','MA','MI','MN','MS','MO','MT','NE','NV','NH','NJ','NM','NY',
    'NC','ND','OH','OK','OR','PA','RI','SC','SD','TN','TX','UT','VT','VA','WA','WV',
    'WI','WY','DC','PR','VI','GU','AS','MP',
  ]);
  const US_STATE_NAMES = [
    'alabama','alaska','arizona','arkansas','california','colorado','connecticut','delaware',
    'florida','georgia','hawaii','idaho','illinois','indiana','iowa','kansas','kentucky',
    'louisiana','maine','maryland','massachusetts','michigan','minnesota','mississippi',
    'missouri','montana','nebraska','nevada','new hampshire','new jersey','new mexico',
    'new york','north carolina','north dakota','ohio','oklahoma','oregon','pennsylvania',
    'rhode island','south carolina','south dakota','tennessee','texas','utah','vermont',
    'virginia','washington','west virginia','wisconsin','wyoming','district of columbia',
    'puerto rico','virgin islands','guam',
  ];
  function isCompleteUsAddress(raw) {
    const addr = String(raw || '').trim();
    if (!addr || /^(?:p\.?\s*o\.?\s*box|post\s+office\s+box)\b/i.test(addr)) return false;
    const noCountry = addr.replace(/(?:,\s*|\s+)(?:USA|United\s+States(?:\s+of\s+America)?|U\.S\.A\.?)\s*$/i, '').trim();
    const hm = noCountry.match(/^(?:[NSEW]\d{1,6}[A-Za-z]?(?:[NSEW]\d{1,6}[A-Za-z]?)?|\d{1,6}[A-Za-z]?(?:-\d{1,5}[A-Za-z]?)?(?:\s+1\/2)?)\s+(.+?)$/i);
    if (!hm || !hm[1]) return false;
    const rest = hm[1].trim();
    const zm = rest.match(/^(.+?)(?:,\s*|\s+)(\d{5})(?:-\d{4})?\s*$/);
    if (zm && (zm[1].match(/[A-Za-z]/g) || []).length >= 2) return true;
    const parts = rest.split(',').map((s) => s.trim()).filter(Boolean);
    if (parts.length >= 2 && (parts[0].match(/[A-Za-z]/g) || []).length >= 2) {
      if (parts.length >= 3) {
        const last = parts[parts.length - 1].replace(/\.+$/, '');
        const prev = parts[parts.length - 2];
        const okSt = last.length === 2 ? US_STATE_SET.has(last.toUpperCase()) : US_STATE_NAMES.includes(last.toLowerCase());
        if ((prev.match(/[A-Za-z]/g) || []).length >= 2 && okSt) return true;
      }
      const tail = parts[parts.length - 1];
      const sm = tail.match(/^(.+?)\s+([A-Za-z]{2})\.?$/);
      if (sm && US_STATE_SET.has(sm[2].toUpperCase()) && (sm[1].match(/[A-Za-z]/g) || []).length >= 2) return true;
      const low = tail.toLowerCase();
      for (const st of US_STATE_NAMES) {
        if (low.endsWith(st)) {
          const pref = low.slice(0, low.length - st.length);
          if (/\s$/.test(pref) && (pref.match(/[a-z]/g) || []).length >= 2) return true;
        }
      }
    }
    return false;
  }

  $('lookupForm').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    if (activePollTimer) clearTimeout(activePollTimer);
    const addr = ($('addrInput').value || '').trim();
    const out = $('lookupResult');
    if (!isCompleteUsAddress(addr)) {
      out.style.display = 'block';
      out.innerHTML = `<div class="status-note" style="color:#dc2626">${esc(t('popup.incompleteAddr'))}</div>`;
      return;
    }
    out.style.display = 'block';
    out.innerHTML = `<div class="status-note">${esc(t('popup.analyzing', { addr }))}</div>`;

    const res = await sendMsg({
      type: 'FETCH_SUMMARY',
      payload: { address: addr, portal: 'Popup', enqueue: true },
    });
    if (!res?.ok || !res.data) {
      out.innerHTML = `<div class="status-note" style="color:#dc2626">${esc(res?.error || t('popup.requestFailed'))}</div>`;
      return;
    }
    if (res.serverUrl) state.effectiveServerUrl = res.serverUrl;
    renderLookupResult(addr, res.data);
    if (res.data.status === 'building' && res.data.jobId) {
      schedulePopupPoll(addr, res.data.jobId, res.data.slug, 1);
    }
  });

  $('saveSettingsBtn').addEventListener('click', async () => {
    const rawUrl = ($('srvUrlInput').value || '').trim() || BRAND_CONFIG.defaultServerUrl;
    const cleanUrl = rawUrl.replace(/\/+$/, '');
    const note = $('settingsNote');

    // Request optional host permission when user configures a custom origin
    try {
      const u = new URL(cleanUrl);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') {
        throw new Error('bad protocol');
      }
      if (u.hostname !== 'localhost' && u.hostname !== '127.0.0.1' && chrome.permissions?.request) {
        await new Promise((resolve) => {
          chrome.permissions.request({ origins: [`${u.origin}/*`] }, () => resolve());
        });
      }
    } catch {
      note.textContent = t('popup.invalidUrl');
      note.style.color = '#dc2626';
      return;
    }

    const saved = await sendMsg({
      type: 'SAVE_SETTINGS',
      settings: {
        serverUrl: cleanUrl,
        apiKey: ($('apiKeyInput').value || '').trim(),
        lang: state.lang,
        brandName: ($('wlNameInput').value || '').trim(),
        accentColor: ($('wlColorInput').value || '').trim(),
      },
    });
    if (saved?.ok) {
      state.serverUrl = saved.settings.serverUrl;
      state.effectiveServerUrl = saved.settings.serverUrl;
      state.brand = saved.brand || resolveBrandConfig(saved.settings);
      applyThemeAndLabels();
      const h = await refreshHealth(state.serverUrl);
      note.textContent = h?.ok
        ? t('popup.settingsSavedOk')
        : t('popup.settingsSavedOffline');
      note.style.color = h?.ok ? '#15803d' : '#b45309';
    }
  });

  (async () => {
    const init = await sendMsg({ type: 'GET_STATE' });
    if (init?.ok) {
      state.lang = ExtI18n.toShortLang(init.settings?.lang);
      state.serverUrl = init.settings?.serverUrl || BRAND_CONFIG.defaultServerUrl;
      state.effectiveServerUrl = init.serverUrl || state.serverUrl;
      state.brand = init.brand || resolveBrandConfig(init.settings);
      state.dealRoom = Array.isArray(init.dealRoom) ? init.dealRoom : [];

      $('srvUrlInput').value = state.serverUrl;
      $('apiKeyInput').value = init.settings?.apiKey || '';
      $('wlNameInput').value = init.settings?.brandName || '';
      $('wlColorInput').value = init.settings?.accentColor || '';
    }
    applyThemeAndLabels();
    refreshHealth();
  })();
})();
