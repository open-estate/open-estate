// Manifest V3 Service Worker for Quick Address Lookup, Context-Menu Research & Deal Room Shortlist.
// Centralizes all backend API calls (/api/extension/summary, /api/report/enqueue, /api/deal-room/compare)
// against your self-hosted backend instance with zero third-party portal DOM scraping.

/* global importScripts, BRAND_CONFIG, resolveBrandConfig, chrome */
importScripts('brand.js');

const PREFIX = BRAND_CONFIG.storagePrefix;
const KEYS = Object.freeze({
  SETTINGS: `${PREFIX}:settings`,
  CLIENT_ID: `${PREFIX}:clientId`,
  SUMMARY_CACHE: `${PREFIX}:summaryCache`,
  DEAL_ROOM: `${PREFIX}:dealRoom`,
});

const MAX_CACHE_ENTRIES = 50;
let lastReachableServerUrl = BRAND_CONFIG.defaultServerUrl;

function slugifyAddress(raw) {
  return String(raw || '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 120);
}

function normalizeServerUrl(url) {
  const raw = String(url || '').trim() || BRAND_CONFIG.defaultServerUrl;
  return raw.replace(/\/+$/, '');
}

function normalizeLangFlag(raw) {
  const lc = String(raw || '').trim().toLowerCase();
  if (lc.startsWith('zh')) return 'zh';
  if (lc.startsWith('es')) return 'es';
  return 'en';
}

function langQuerySuffix(lang, hasQuery) {
  const short = normalizeLangFlag(lang);
  if (short !== 'zh' && short !== 'es') return '';
  return `${hasQuery ? '&' : '?'}lang=${short}`;
}

async function getStorage(keys) {
  return new Promise((resolve) => {
    chrome.storage.local.get(keys, (res) => resolve(res || {}));
  });
}

async function setStorage(items) {
  return new Promise((resolve) => {
    chrome.storage.local.set(items, () => resolve());
  });
}

async function getClientId() {
  const data = await getStorage([KEYS.CLIENT_ID]);
  if (typeof data[KEYS.CLIENT_ID] === 'string' && data[KEYS.CLIENT_ID].length >= 16) {
    return data[KEYS.CLIENT_ID];
  }
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  const id = (BRAND_CONFIG.clientIdPrefix || 'oex-') + Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  await setStorage({ [KEYS.CLIENT_ID]: id });
  return id;
}

async function getSettings() {
  const data = await getStorage([KEYS.SETTINGS]);
  const stored = (data[KEYS.SETTINGS] && typeof data[KEYS.SETTINGS] === 'object')
    ? data[KEYS.SETTINGS]
    : {};

  const normalizedUrl = normalizeServerUrl(stored.serverUrl);
  const hasExplicitCustom = typeof stored.customServer === 'boolean'
    ? stored.customServer
    : Boolean(
        stored.serverUrl &&
        normalizeServerUrl(stored.serverUrl) !== BRAND_CONFIG.defaultServerUrl &&
        normalizeServerUrl(stored.serverUrl) !== 'http://localhost:8080'
      );

  const effectiveUrl = (!hasExplicitCustom && stored.serverUrl === 'http://localhost:8080')
    ? BRAND_CONFIG.defaultServerUrl
    : normalizedUrl;

  return {
    serverUrl: effectiveUrl,
    customServer: hasExplicitCustom,
    apiKey: typeof stored.apiKey === 'string' ? stored.apiKey.trim() : '',
    lang: normalizeLangFlag(stored.lang),
    brandName: typeof stored.brandName === 'string' ? stored.brandName.trim() : '',
    brandNameZh: typeof stored.brandNameZh === 'string' ? stored.brandNameZh.trim() : '',
    accentColor: typeof stored.accentColor === 'string' ? stored.accentColor.trim() : '',
  };
}

async function getDealRoom() {
  const data = await getStorage([KEYS.DEAL_ROOM]);
  return Array.isArray(data[KEYS.DEAL_ROOM]) ? data[KEYS.DEAL_ROOM] : [];
}

async function buildHeaders(settings) {
  const clientId = await getClientId();
  const headers = {
    'Content-Type': 'application/json',
    [BRAND_CONFIG.clientIdHeader]: clientId,
  };
  if (settings.apiKey) {
    headers.Authorization = `Bearer ${settings.apiKey}`;
  }
  return headers;
}

async function fetchApi(settings, path, init, overrideUrl) {
  const primaryBase = overrideUrl ? normalizeServerUrl(overrideUrl) : settings.serverUrl;
  const devBase = BRAND_CONFIG.devFallbackServerUrl
    ? normalizeServerUrl(BRAND_CONFIG.devFallbackServerUrl)
    : '';
  const canTryDevFallback = Boolean(
    !overrideUrl &&
    !settings.customServer &&
    !settings.apiKey &&
    devBase &&
    primaryBase === BRAND_CONFIG.defaultServerUrl &&
    devBase !== primaryBase
  );

  try {
    const res = await fetch(`${primaryBase}${path}`, init);
    if (canTryDevFallback && res.status >= 502) {
      try {
        const devRes = await fetch(`${devBase}${path}`, init);
        lastReachableServerUrl = devBase;
        return { res: devRes, serverUrl: devBase, usedDevFallback: true };
      } catch {
        // Keep primary gateway status if dev server is not running
      }
    }
    lastReachableServerUrl = primaryBase;
    return { res, serverUrl: primaryBase, usedDevFallback: false };
  } catch (err) {
    if (canTryDevFallback) {
      try {
        const devRes = await fetch(`${devBase}${path}`, init);
        lastReachableServerUrl = devBase;
        return { res: devRes, serverUrl: devBase, usedDevFallback: true };
      } catch {
        // Both production and dev fallback failed
      }
    }
    throw err;
  }
}

function makeCacheKey(payload) {
  const base = payload.slug ? String(payload.slug).toLowerCase() : slugifyAddress(payload.address);
  const ask = Number.isFinite(Number(payload.askingPrice)) && Number(payload.askingPrice) > 0
    ? Math.round(Number(payload.askingPrice))
    : 0;
  const cre = payload.isCommercial ? 'cre' : 'res';
  return `${base}::${ask}::${cre}`;
}

async function readCachedSummary(cacheKey) {
  const data = await getStorage([KEYS.SUMMARY_CACHE]);
  const map = data[KEYS.SUMMARY_CACHE];
  if (!map || typeof map !== 'object') return null;
  const entry = map[cacheKey];
  if (!entry || !entry.payload || typeof entry.savedAt !== 'number') return null;
  const ttl = entry.payload.aiPending
    ? (BRAND_CONFIG.aiPendingCacheTtlMs || 20 * 1000)
    : BRAND_CONFIG.cacheTtlMs;
  if (Date.now() - entry.savedAt > ttl) return null;
  return entry.payload;
}

async function writeCachedSummary(cacheKey, payload) {
  if (!cacheKey || !payload || payload.status !== 'ready') return;
  const data = await getStorage([KEYS.SUMMARY_CACHE]);
  const map = data[KEYS.SUMMARY_CACHE] && typeof data[KEYS.SUMMARY_CACHE] === 'object'
    ? Object.assign({}, data[KEYS.SUMMARY_CACHE])
    : {};
  map[cacheKey] = { savedAt: Date.now(), payload };

  const entries = Object.entries(map);
  if (entries.length > MAX_CACHE_ENTRIES) {
    entries.sort((a, b) => (b[1]?.savedAt || 0) - (a[1]?.savedAt || 0));
    const pruned = Object.fromEntries(entries.slice(0, MAX_CACHE_ENTRIES));
    await setStorage({ [KEYS.SUMMARY_CACHE]: pruned });
  } else {
    await setStorage({ [KEYS.SUMMARY_CACHE]: map });
  }
}

async function syncDealRoomSummaryIfPresent(summary) {
  if (!summary || summary.status !== 'ready' || !summary.slug) return;
  const list = await getDealRoom();
  const idx = list.findIndex(
    (item) => item.slug === summary.slug || slugifyAddress(item.address) === summary.slug
  );
  if (idx < 0) return;
  const next = list.slice();
  next[idx] = Object.assign({}, next[idx], {
    slug: summary.slug,
    address: summary.address || next[idx].address,
    overallScore: summary.overallScore ?? next[idx].overallScore ?? null,
    fairValueMid: summary.fairValue?.mid ?? next[idx].fairValueMid ?? null,
    trueMonthlyCost: summary.trueMonthlyCost?.total ?? next[idx].trueMonthlyCost ?? null,
    capRatePct: summary.creMetrics?.capRatePct ?? next[idx].capRatePct ?? null,
    verdict: summary.verdict ?? next[idx].verdict ?? '',
  });
  await setStorage({ [KEYS.DEAL_ROOM]: next });
}

function resolveEffectiveBase(settings) {
  if (settings.customServer || settings.serverUrl !== BRAND_CONFIG.defaultServerUrl) {
    return settings.serverUrl;
  }
  return lastReachableServerUrl || settings.serverUrl;
}

async function handleFetchSummary(reqPayload) {
  const settings = await getSettings();
  const payload = reqPayload || {};
  const cacheKey = makeCacheKey(payload);
  const bypassLocal = Boolean(payload.bypassLocalCache || payload.forceRefresh);

  if (!bypassLocal) {
    const cached = await readCachedSummary(cacheKey);
    if (cached) {
      return {
        ok: true,
        fromExtensionCache: true,
        data: cached,
        serverUrl: resolveEffectiveBase(settings),
        settings,
        brand: resolveBrandConfig(settings),
      };
    }
  }

  const headers = await buildHeaders(settings);
  const body = {
    address: payload.address || undefined,
    slug: payload.slug || undefined,
    portal: payload.portal || undefined,
    isCommercial: Boolean(payload.isCommercial),
    askingPrice: Number.isFinite(Number(payload.askingPrice)) && Number(payload.askingPrice) > 0
      ? Math.round(Number(payload.askingPrice))
      : undefined,
    capRatePct: Number.isFinite(Number(payload.capRatePct)) && Number(payload.capRatePct) > 0
      ? Number(payload.capRatePct)
      : undefined,
    noi: Number.isFinite(Number(payload.noi)) && Number(payload.noi) > 0
      ? Math.round(Number(payload.noi))
      : undefined,
    assetType: payload.assetType || undefined,
    sqft: Number.isFinite(Number(payload.sqft)) && Number(payload.sqft) > 0
      ? Math.round(Number(payload.sqft))
      : undefined,
    lotSqft: Number.isFinite(Number(payload.lotSqft)) && Number(payload.lotSqft) > 0
      ? Math.round(Number(payload.lotSqft))
      : undefined,
    yearBuilt: Number.isFinite(Number(payload.yearBuilt)) && Number(payload.yearBuilt) >= 1700
      ? Math.round(Number(payload.yearBuilt))
      : undefined,
    mlsNumber: typeof payload.mlsNumber === 'string' && payload.mlsNumber.trim()
      ? payload.mlsNumber.trim().slice(0, 32)
      : undefined,
    avmValue: Number.isFinite(Number(payload.avmValue)) && Number(payload.avmValue) > 0
      ? Math.round(Number(payload.avmValue))
      : undefined,
    saleHistory: Array.isArray(payload.saleHistory) && payload.saleHistory.length
      ? payload.saleHistory.slice(0, 12)
      : undefined,
    beds: Number.isFinite(Number(payload.beds)) ? Number(payload.beds) : undefined,
    baths: Number.isFinite(Number(payload.baths)) ? Number(payload.baths) : undefined,
    hoaMonthly: Number.isFinite(Number(payload.hoaMonthly)) && Number(payload.hoaMonthly) >= 0
      ? Math.round(Number(payload.hoaMonthly))
      : undefined,
    enqueue: payload.enqueue !== false,
    refresh: Boolean(payload.refresh ?? payload.forceRefresh),
    lang: settings.lang,
  };

  const { res, serverUrl, usedDevFallback } = await fetchApi(settings, '/api/extension/summary', {
    method: 'POST',
    headers,
    body: JSON.stringify(body),
  });
  const json = await res.json().catch(() => null);
  if (!res.ok && res.status !== 202) {
    return {
      ok: false,
      status: res.status,
      error: json?.error || `Server returned HTTP ${res.status}`,
      serverUrl,
      usedDevFallback,
      settings,
      brand: resolveBrandConfig(settings),
    };
  }
  if (json && json.status === 'ready') {
    await writeCachedSummary(cacheKey, json);
    await syncDealRoomSummaryIfPresent(json);
  }
  return {
    ok: true,
    fromExtensionCache: false,
    data: json,
    serverUrl,
    usedDevFallback,
    settings,
    brand: resolveBrandConfig(settings),
  };
}

async function handlePollJob(msg) {
  const settings = await getSettings();
  const headers = await buildHeaders(settings);
  if (msg.jobId) {
    const { res: jobRes } = await fetchApi(
      settings,
      `/api/job/${encodeURIComponent(msg.jobId)}`,
      { method: 'GET', headers }
    );
    if (jobRes.ok) {
      const snap = await jobRes.json().catch(() => null);
      if (snap && snap.error) {
        return { ok: false, error: snap.error, step: snap.step };
      }
      if (snap && snap.ready) {
        return handleFetchSummary(Object.assign({}, msg.portalData || {}, {
          slug: snap.slug || msg.slug,
          address: msg.address || snap.address,
          bypassLocalCache: true,
          refresh: false,
          enqueue: false,
        }));
      }
      if (snap && snap.done) {
        return { ok: false, error: snap.step || 'Report build failed', step: snap.step };
      }
      return {
        ok: true,
        data: {
          ok: true,
          status: 'building',
          jobId: msg.jobId,
          slug: snap?.slug || msg.slug,
          step: snap?.step || 'Compiling property intelligence…',
        },
      };
    }
  }
  return handleFetchSummary(Object.assign({}, msg.portalData || {}, {
    slug: msg.slug,
    address: msg.address,
    bypassLocalCache: true,
    refresh: false,
    enqueue: false,
  }));
}

async function handleEnqueueReport(msg) {
  const settings = await getSettings();
  const headers = await buildHeaders(settings);
  const { res, serverUrl } = await fetchApi(settings, '/api/report/enqueue', {
    method: 'POST',
    headers,
    body: JSON.stringify({
      address: msg.address,
      refresh: Boolean(msg.refresh),
      ai: msg.ai !== false,
    }),
  });
  const data = await res.json().catch(() => null);
  return { ok: res.ok || res.status === 202, status: res.status, serverUrl, data };
}

async function handleDealRoomAdd(item) {
  const settings = await getSettings();
  const list = await getDealRoom();
  const slug = item.slug || slugifyAddress(item.address);
  if (!slug) {
    return { ok: false, error: 'Missing property slug or street address.' };
  }

  const normalized = {
    slug,
    address: String(item.address || slug).trim(),
    portal: String(item.portal || 'Manual').trim(),
    isCommercial: Boolean(item.isCommercial),
    askingPrice: Number.isFinite(Number(item.askingPrice)) && Number(item.askingPrice) > 0
      ? Math.round(Number(item.askingPrice))
      : null,
    fairValueMid: Number.isFinite(Number(item.fairValueMid)) ? Math.round(Number(item.fairValueMid)) : null,
    overallScore: Number.isFinite(Number(item.overallScore)) ? Math.round(Number(item.overallScore)) : null,
    trueMonthlyCost: Number.isFinite(Number(item.trueMonthlyCost)) ? Math.round(Number(item.trueMonthlyCost)) : null,
    capRatePct: Number.isFinite(Number(item.capRatePct)) ? Number(item.capRatePct) : null,
    verdict: typeof item.verdict === 'string' ? item.verdict : '',
    savedAt: new Date().toISOString(),
  };

  const filtered = list.filter((x) => x.slug !== slug);
  const next = [normalized, ...filtered].slice(0, BRAND_CONFIG.maxDealRoomItems);
  await setStorage({ [KEYS.DEAL_ROOM]: next });

  // Best-effort background warm-up if report is not yet cached on server
  if (normalized.address && normalized.overallScore == null) {
    handleEnqueueReport({ address: normalized.address, ai: true }).catch(() => {});
  }

  const slugsParam = next.map((x) => encodeURIComponent(x.slug)).join(',');
  const activeBase = resolveEffectiveBase(settings);
  return {
    ok: true,
    dealRoom: next,
    serverUrl: activeBase,
    dealRoomUrl: `${activeBase}/deal-room?slugs=${slugsParam}${langQuerySuffix(settings.lang, true)}`,
  };
}

async function handleDealRoomRemove(slug) {
  const settings = await getSettings();
  const list = await getDealRoom();
  const next = list.filter((x) => x.slug !== slug);
  await setStorage({ [KEYS.DEAL_ROOM]: next });
  const slugsParam = next.map((x) => encodeURIComponent(x.slug)).join(',');
  const activeBase = resolveEffectiveBase(settings);
  return {
    ok: true,
    dealRoom: next,
    serverUrl: activeBase,
    dealRoomUrl: `${activeBase}/deal-room?slugs=${slugsParam}${langQuerySuffix(settings.lang, true)}`,
  };
}

async function handleDealRoomCompare(msg) {
  const settings = await getSettings();
  const headers = await buildHeaders(settings);
  const list = await getDealRoom();
  const slugs = Array.isArray(msg?.slugs) && msg.slugs.length > 0
    ? msg.slugs
    : list.map((x) => x.slug);
  const askingPrices = {};
  for (const item of list) {
    if (item.slug && Number.isFinite(item.askingPrice) && item.askingPrice > 0) {
      askingPrices[item.slug] = item.askingPrice;
    }
  }
  const slugsParam = slugs.map((s) => encodeURIComponent(s)).join(',');
  const activeBase = resolveEffectiveBase(settings);
  const dealRoomUrl = `${activeBase}/deal-room?slugs=${slugsParam}${langQuerySuffix(settings.lang, true)}`;

  if (slugs.length < 2) {
    return {
      ok: false,
      error: 'Save at least 2 properties to compare side-by-side in the Deal Room.',
      dealRoomUrl,
    };
  }

  const { res, serverUrl } = await fetchApi(settings, '/api/deal-room/compare', {
    method: 'POST',
    headers,
    body: JSON.stringify({
      slugs,
      askingPrices,
      lang: settings.lang,
    }),
  });
  const data = await res.json().catch(() => null);
  return {
    ok: res.ok,
    status: res.status,
    data,
    dealRoomUrl: `${serverUrl}/deal-room?slugs=${slugsParam}${langQuerySuffix(settings.lang, true)}`,
  };
}

async function handleCheckHealth(overrideUrl) {
  const settings = await getSettings();
  const headers = await buildHeaders(settings);
  try {
    const { res, serverUrl, usedDevFallback } = await fetchApi(
      settings,
      '/api/health',
      { method: 'GET', headers },
      overrideUrl
    );
    const data = await res.json().catch(() => null);
    return {
      ok: Boolean(res.ok && data?.ok),
      status: res.status,
      serverUrl,
      usedDevFallback,
      data,
    };
  } catch (err) {
    return {
      ok: false,
      serverUrl: overrideUrl ? normalizeServerUrl(overrideUrl) : settings.serverUrl,
      usedDevFallback: false,
      error: err?.message || 'Connection failed',
    };
  }
}

const CONTEXT_MENU_ANALYZE_ID = `${PREFIX}:analyze-selected-address`;

if (chrome.runtime?.onInstalled && chrome.contextMenus) {
  chrome.runtime.onInstalled.addListener(() => {
    chrome.contextMenus.removeAll(() => {
      chrome.contextMenus.create({
        id: CONTEXT_MENU_ANALYZE_ID,
        title: `Analyze "%s" (${BRAND_CONFIG.name})`,
        contexts: ['selection'],
      });
    });
  });

  chrome.contextMenus.onClicked.addListener(async (info) => {
    if (info.menuItemId !== CONTEXT_MENU_ANALYZE_ID) return;
    const selection = String(info.selectionText || '').trim().slice(0, 200);
    if (!selection) return;
    const settings = await getSettings();
    const activeBase = resolveEffectiveBase(settings);
    const targetUrl = `${activeBase}/report?address=${encodeURIComponent(selection)}${langQuerySuffix(settings.lang, true)}`;
    if (/^https?:\/\//i.test(targetUrl)) {
      chrome.tabs.create({ url: targetUrl });
    }
  });
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  (async () => {
    try {
      const action = message?.type || message?.action;
      switch (action) {
        case 'GET_STATE': {
          const [settings, dealRoom] = await Promise.all([getSettings(), getDealRoom()]);
          const slugsParam = dealRoom.map((x) => encodeURIComponent(x.slug)).join(',');
          const activeBase = resolveEffectiveBase(settings);
          sendResponse({
            ok: true,
            settings,
            serverUrl: activeBase,
            brand: resolveBrandConfig(settings),
            dealRoom,
            dealRoomUrl: `${activeBase}/deal-room${slugsParam ? `?slugs=${slugsParam}` : ''}${langQuerySuffix(settings.lang, Boolean(slugsParam))}`,
          });
          return;
        }
        case 'SAVE_SETTINGS': {
          const prev = await getSettings();
          const nextUrl = normalizeServerUrl(message.settings?.serverUrl ?? prev.serverUrl);
          const customServer = nextUrl !== BRAND_CONFIG.defaultServerUrl;
          const next = {
            serverUrl: nextUrl,
            customServer,
            apiKey: typeof message.settings?.apiKey === 'string' ? message.settings.apiKey.trim() : prev.apiKey,
            lang: normalizeLangFlag(message.settings?.lang || prev.lang),
            brandName: typeof message.settings?.brandName === 'string' ? message.settings.brandName.trim() : prev.brandName,
            brandNameZh: typeof message.settings?.brandNameZh === 'string' ? message.settings.brandNameZh.trim() : prev.brandNameZh,
            accentColor: typeof message.settings?.accentColor === 'string' ? message.settings.accentColor.trim() : prev.accentColor,
          };
          lastReachableServerUrl = nextUrl;
          await setStorage({ [KEYS.SETTINGS]: next });
          sendResponse({ ok: true, settings: next, brand: resolveBrandConfig(next) });
          return;
        }
        case 'SET_LANG': {
          const prev = await getSettings();
          const next = Object.assign({}, prev, { lang: normalizeLangFlag(message.lang) });
          await setStorage({ [KEYS.SETTINGS]: next });
          sendResponse({ ok: true, lang: next.lang, settings: next });
          return;
        }
        case 'CHECK_HEALTH': {
          sendResponse(await handleCheckHealth(message.serverUrl));
          return;
        }
        case 'FETCH_SUMMARY': {
          sendResponse(await handleFetchSummary(message.payload || message));
          return;
        }
        case 'POLL_JOB': {
          sendResponse(await handlePollJob(message));
          return;
        }
        case 'ENQUEUE_REPORT': {
          sendResponse(await handleEnqueueReport(message));
          return;
        }
        case 'DEAL_ROOM_ADD': {
          sendResponse(await handleDealRoomAdd(message.item || message));
          return;
        }
        case 'DEAL_ROOM_REMOVE': {
          sendResponse(await handleDealRoomRemove(message.slug));
          return;
        }
        case 'DEAL_ROOM_CLEAR': {
          await setStorage({ [KEYS.DEAL_ROOM]: [] });
          sendResponse({ ok: true, dealRoom: [] });
          return;
        }
        case 'DEAL_ROOM_COMPARE': {
          sendResponse(await handleDealRoomCompare(message));
          return;
        }
        case 'OPEN_TAB': {
          if (typeof message.url === 'string' && /^https?:\/\//i.test(message.url)) {
            chrome.tabs.create({ url: message.url });
            sendResponse({ ok: true });
          } else {
            sendResponse({ ok: false, error: 'Invalid target URL' });
          }
          return;
        }
        default:
          sendResponse({ ok: false, error: `Unknown extension action: ${String(action)}` });
      }
    } catch (err) {
      sendResponse({ ok: false, error: err?.message || String(err) });
    }
  })();
  return true;
});
