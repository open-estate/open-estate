#!/usr/bin/env node
/**
 * Open Estate OSS Docker Container E2E Verification Suite (Playwright + Headless Chromium)
 *
 * Usage:
 *   BASE_URL=http://127.0.0.1:18080 node e2e/oss_docker_e2e.mjs
 *
 * Designed specifically for pure keyless self-hosted Docker containers (fresh /data volume,
 * ZERO API keys, ZERO hardcoded real residential addresses).
 */

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
function discoverHomePlaywrightCandidates() {
  const home = process.env.HOME;
  if (!home || !fs.existsSync(home)) return [];
  const found = [];
  const direct = path.join(home, 'node_modules', 'playwright');
  if (fs.existsSync(direct)) found.push(direct);
  try {
    for (const d1 of fs.readdirSync(home, { withFileTypes: true })) {
      if (!d1.isDirectory() || d1.name.startsWith('.')) continue;
      const p1 = path.join(home, d1.name);
      const m1 = path.join(p1, 'node_modules', 'playwright');
      if (fs.existsSync(m1)) found.push(m1);
      try {
        for (const d2 of fs.readdirSync(p1, { withFileTypes: true })) {
          if (!d2.isDirectory() || d2.name.startsWith('.')) continue;
          const m2 = path.join(p1, d2.name, 'node_modules', 'playwright');
          if (fs.existsSync(m2)) found.push(m2);
        }
      } catch {
        // ignore unreadable subdirectories
      }
    }
  } catch {
    // ignore unreadable home directory
  }
  return found;
}

function loadPlaywright() {
  const candidates = [
    process.env.PLAYWRIGHT_MODULE_PATH,
    'playwright',
    ...discoverHomePlaywrightCandidates(),
  ].filter(Boolean);
  for (const mod of candidates) {
    try {
      return require(mod);
    } catch {
      // try next candidate
    }
  }
  throw new Error('Playwright not found. Set PLAYWRIGHT_MODULE_PATH or install playwright.');
}

const { chromium } = loadPlaywright();
const BASE_URL = (process.env.BASE_URL || 'http://127.0.0.1:8080').replace(/\/+$/, '');
const ORIGIN = new URL(BASE_URL).origin;

const FORBIDDEN_PORTALS = ['redfin', 'zillow', 'realtor.com', 'homes.com', 'trulia', 'movoto'];

function findChromiumExecutable() {
  if (
    process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH &&
    fs.existsSync(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH)
  ) {
    return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;
  }
  const cacheRoot = path.join(process.env.HOME || '/root', '.cache', 'ms-playwright');
  if (fs.existsSync(cacheRoot)) {
    const dirs = fs.readdirSync(cacheRoot).filter((d) => d.startsWith('chromium-')).sort().reverse();
    for (const d of dirs) {
      for (const sub of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
        const candidate = path.join(cacheRoot, d, sub);
        if (fs.existsSync(candidate)) return candidate;
      }
    }
  }
  return undefined;
}

async function apiJson(pathname, opts = {}) {
  const headers = {
    Origin: ORIGIN,
    Referer: `${ORIGIN}/`,
    'User-Agent':
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36',
    ...(opts.headers || {}),
  };
  if (opts.body && !headers['Content-Type']) {
    headers['Content-Type'] = 'application/json';
  }
  const res = await fetch(`${BASE_URL}${pathname}`, {
    method: opts.method || 'GET',
    headers,
    body: opts.body ? (typeof opts.body === 'string' ? opts.body : JSON.stringify(opts.body)) : undefined,
  });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // non-JSON response
  }
  return { status: res.status, ok: res.ok, text, json, headers: res.headers };
}

function assertCleanText(text, label) {
  const low = String(text || '').toLowerCase();
  assert(!low.includes('sample-pill'), `[${label}] Leaked forbidden sample report bar`);
  assert(!low.includes('v1.0.0-rust'), `[${label}] Leaked internal build tag`);
  assert(!low.includes('openai_api_key'), `[${label}] Leaked env var name`);
}

async function generateSyntheticReport(address, lat, lon) {
  const enq = await apiJson('/api/report/enqueue', {
    method: 'POST',
    body: { address, ai: false, refresh: false },
  });
  assert(
    enq.status === 200 || enq.status === 202,
    `POST /api/report/enqueue failed (${enq.status}): ${enq.text}`
  );
  assert(enq.json && enq.json.slug, `Missing slug from enqueue: ${enq.text}`);
  const slug = enq.json.slug;

  if (enq.json.status === 'cached') {
    return slug;
  }

  const jobId = enq.json.jobId;
  assert(jobId, `Expected jobId for in-flight report build: ${enq.text}`);

  const deadline = Date.now() + 120_000;
  let lastStep = null;
  while (Date.now() < deadline) {
    const st = await apiJson(`/api/job/${encodeURIComponent(jobId)}`);
    assert(st.ok && st.json, `Job status failed (${st.status}): ${st.text}`);
    lastStep = st.json.step || st.json.status;
    if (st.json.error) {
      throw new Error(`Report generation failed for ${address}: ${st.json.error}`);
    }
    if (st.json.ready || st.json.done || st.json.status === 'done' || st.json.status === 'cached') {
      return st.json.slug || slug;
    }
    await new Promise((r) => setTimeout(r, 800));
  }
  throw new Error(`Timed out waiting for report job ${jobId} (lastStep=${lastStep})`);
}

async function verifyBrowserScheme(browser, colorScheme, reportSlug, ipZip) {
  console.log(`\n▶ Browser verification (${colorScheme} colorScheme)...`);
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    colorScheme,
    extraHTTPHeaders: {
      'X-Forwarded-For': '8.8.8.8',
    },
  });

  const page = await context.newPage();
  const pageErrors = [];
  const consoleErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message || String(err)));
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      const txt = msg.text();
      // Ignore benign aborted fetches during tab switches or offline tile blocks
      if (!txt.includes('net::ERR_FAILED') && !txt.includes('AbortError')) {
        consoleErrors.push(txt);
      }
    }
  });

  // 1. Homepage /
  const homeRes = await page.goto(`${BASE_URL}/`, { waitUntil: 'domcontentloaded', timeout: 30_000 });
  assert.equal(homeRes.status(), 200, `GET / must return 200 (${colorScheme})`);

  await page.locator('#areaOnsaleSection').waitFor({ state: 'visible', timeout: 10_000 });
  await page.locator('#areaZipInput').waitFor({ state: 'visible', timeout: 10_000 });
  await page.locator('#areaGeoBtn').waitFor({ state: 'visible', timeout: 10_000 });

  const homeText = await page.evaluate(() => document.body?.innerText || '');
  const homeHtml = await page.evaluate(() => document.documentElement?.innerHTML || '');
  assert(homeText.includes('Open Estate'), `Homepage must show Open Estate branding (${colorScheme})`);
  assert(
    homeText.includes('See onsale property in your area:'),
    `Homepage must show "See onsale property in your area:" (${colorScheme})`
  );
  assertCleanText(homeText, `home-text-${colorScheme}`);
  assertCleanText(homeHtml, `home-html-${colorScheme}`);

  // Wait for automatic IP location detection to populate area results without GPS error
  await page.locator('#areaOnsaleResults .area-home-card').first().waitFor({ state: 'visible', timeout: 15_000 });
  const autoStatus = await page.locator('#areaOnsaleStatus').innerText().catch(() => '');
  assert(
    !autoStatus.toLowerCase().includes('permission denied') && !autoStatus.toLowerCase().includes('geolocation'),
    `Automatic IP detection must not show unsolicited GPS permission error: "${autoStatus}"`
  );

  // If IP ZIP differed from 95118 and we ingested a synthetic listing for it, ensure cards rendered
  if (ipZip) {
    const cardsCount = await page.locator('#areaOnsaleResults .area-home-card').count();
    assert(cardsCount >= 1, `Expected >=1 auto-populated card for IP ZIP ${ipZip}, got ${cardsCount}`);
    console.log(`   IP auto-detected ZIP=${ipZip}, initial area cards=${cardsCount}`);
  }

  // Explicitly test #areaZipInput -> 95118 -> #areaZipBtn
  await page.fill('#areaZipInput', '95118');
  await page.click('#areaZipBtn');
  await page.waitForFunction(
    () => {
      const txt = document.getElementById('areaOnsaleResults')?.innerText || '';
      return txt.includes('Synthparcelbeta') && document.querySelectorAll('#areaOnsaleResults .area-home-card').length >= 2;
    },
    null,
    { timeout: 15_000 }
  );
  const zipCards = await page.locator('#areaOnsaleResults .area-home-card').count();
  assert(zipCards >= 2, `Expected >=2 ingested synthetic listings for 95118, got ${zipCards}`);
  const areaGridText = await page.locator('#areaOnsaleResults').innerText();
  assert(areaGridText.includes('Synthparcelbeta'), `Expected synthetic listing in #areaOnsaleResults: ${areaGridText}`);
  for (const portal of FORBIDDEN_PORTALS) {
    assert(!areaGridText.toLowerCase().includes(portal), `Portal brand "${portal}" leaked in #areaOnsaleResults`);
  }

  // 2. Interactive Tabs (#tab-shop, #tab-match, #tab-alerts, #tab-report)
  await page.click('#tabBtnShop');
  await page.locator('#tab-shop.act').waitFor({ state: 'visible', timeout: 5000 });
  await page.click('#tabBtnMatch');
  await page.locator('#tab-match.act').waitFor({ state: 'visible', timeout: 5000 });
  await page.click('#tabBtnAlerts');
  await page.locator('#tab-alerts.act').waitFor({ state: 'visible', timeout: 5000 });
  await page.click('#tabBtnReport');
  await page.locator('#tab-report.act').waitFor({ state: 'visible', timeout: 5000 });

  // 3. Dedicated pages: /shop, /match, /alerts, /deal-room, /admin
  for (const route of ['/shop', '/match', '/alerts', `/deal-room?slugs=${encodeURIComponent(reportSlug)}`]) {
    const r = await page.goto(`${BASE_URL}${route}`, { waitUntil: 'domcontentloaded', timeout: 25_000 });
    assert(r.status() === 200, `GET ${route} must return 200, got ${r.status()} (${colorScheme})`);
    const txt = await page.evaluate(() => document.body?.innerText || '');
    assertCleanText(txt, `${route}-${colorScheme}`);
  }
  const adminRes = await page.goto(`${BASE_URL}/admin`, { waitUntil: 'domcontentloaded', timeout: 25_000 });
  assert(
    [200, 401, 403, 503].includes(adminRes.status()),
    `GET /admin returned unexpected status ${adminRes.status()} (${colorScheme})`
  );
  const adminText = await page.evaluate(() => document.body?.innerText || '');
  assert(adminText.includes('Open Estate'), `/admin must show Open Estate branding (${colorScheme})`);
  assertCleanText(adminText, `/admin-${colorScheme}`);

  // 4. Rendered Property Report (/r/:slug/)
  const repRes = await page.goto(`${BASE_URL}/r/${encodeURIComponent(reportSlug)}/`, {
    waitUntil: 'domcontentloaded',
    timeout: 30_000,
  });
  assert.equal(repRes.status(), 200, `GET /r/${reportSlug}/ must return 200 (${colorScheme})`);
  await page.locator('#overview').waitFor({ state: 'visible', timeout: 15_000 });
  const repText = await page.evaluate(() => document.body?.innerText || '');
  assert(repText.includes('Open Estate'), `Report must show Open Estate branding (${colorScheme})`);
  assert(repText.includes('Synthparcelalpha'), `Report must show synthetic property address (${colorScheme})`);
  assertCleanText(repText, `report-${colorScheme}`);

  // Verify Leaflet map container and ECharts instances
  const mapCount = await page.locator('.leaflet-container').count();
  assert(mapCount >= 1, `Expected Leaflet map (.leaflet-container) in report (${colorScheme})`);

  await page.waitForFunction(
    () => document.querySelectorAll('canvas, [_echarts_instance_]').length >= 1,
    null,
    { timeout: 10_000 }
  );

  // Verify report tab navigation & diligence subtabs work cleanly
  const navLinks = page.locator('#tabsnav a');
  assert((await navLinks.count()) >= 3, `Expected >=3 navigation tabs in #tabsnav (${colorScheme})`);
  if ((await page.locator('#diligence .seg button').count()) > 0) {
    await page.evaluate(() => window.OpenEstateWorkspace?.jumpToSection?.('diligence'));
    await page.locator('#diligence .seg button').first().click();
  }

  assert.equal(
    pageErrors.length,
    0,
    `Expected 0 uncaught pageerrors in ${colorScheme} mode, got: ${JSON.stringify(pageErrors)}`
  );
  console.log(`✓ [${colorScheme}] Browser suite passed (0 pageerrors, ${consoleErrors.length} console errors)`);
  await context.close();
}

async function run() {
  console.log(`▶ Running Open Estate OSS Docker E2E against ${BASE_URL}`);

  // 1. Health & readiness endpoints
  for (const ep of ['/healthz', '/readyz', '/api/health']) {
    const r = await apiJson(ep);
    assert.equal(r.status, 200, `GET ${ep} must return 200, got ${r.status}`);
    assertCleanText(r.text, ep);
  }
  console.log('✓ Health endpoints (/healthz, /readyz, /api/health) OK');

  // 2. /api/geo/ip location detection
  const geoDirect = await apiJson('/api/geo/ip');
  assert.equal(geoDirect.status, 200, `GET /api/geo/ip direct must return 200: ${geoDirect.text}`);
  const geoXff = await apiJson('/api/geo/ip', { headers: { 'X-Forwarded-For': '8.8.8.8' } });
  assert.equal(geoXff.status, 200, `GET /api/geo/ip XFF must return 200: ${geoXff.text}`);
  assert(geoXff.json && geoXff.json.ok, `Expected /api/geo/ip XFF ok=true: ${geoXff.text}`);
  const ipZip = geoXff.json.postalCode || geoXff.json.zip || null;
  console.log(`✓ /api/geo/ip OK (XFF postalCode=${ipZip}, city=${geoXff.json.city})`);

  // 3. Ingest synthetic listings (ZERO real residential addresses)
  const syntheticListings = [
    {
      address: '100 Synthparcelalpha Ln, San Jose, CA 95118',
      street: '100 Synthparcelalpha Ln',
      city: 'San Jose',
      state: 'CA',
      zip: '95118',
      listPrice: 1850000,
      beds: 4,
      baths: 2.5,
      sqft: 2150,
      lotSqft: 6400,
      yearBuilt: 1978,
      propertyType: 'Single Family Residential',
      status: 'for_sale',
      daysOnMarket: 7,
      lat: 37.2580,
      lon: -121.8980,
      source: 'Open Test MLS Feed',
    },
    {
      address: '200 Synthparcelbeta Ave, San Jose, CA 95118',
      street: '200 Synthparcelbeta Ave',
      city: 'San Jose',
      state: 'CA',
      zip: '95118',
      listPrice: 1695000,
      beds: 3,
      baths: 2.0,
      sqft: 1780,
      lotSqft: 5900,
      yearBuilt: 1974,
      propertyType: 'Single Family Residential',
      status: 'for_sale',
      daysOnMarket: 12,
      lat: 37.2595,
      lon: -121.8950,
      source: 'Open Test MLS Feed',
    },
    {
      address: '400 Synthparceldelta Dr, San Jose, CA 95118',
      street: '400 Synthparceldelta Dr',
      city: 'San Jose',
      state: 'CA',
      zip: '95118',
      listPrice: 1925000,
      beds: 4,
      baths: 3.0,
      sqft: 2340,
      lotSqft: 7100,
      yearBuilt: 1982,
      propertyType: 'Single Family Residential',
      status: 'for_sale',
      daysOnMarket: 4,
      lat: 37.2562,
      lon: -121.8920,
      source: 'Open Test MLS Feed',
    },
  ];
  const ingest95118 = await apiJson('/api/listings/ingest', {
    method: 'POST',
    body: { zip: '95118', triggerAlerts: false, listings: syntheticListings },
  });
  assert.equal(ingest95118.status, 200, `POST /api/listings/ingest failed: ${ingest95118.text}`);

  if (ipZip && /^\d{5}$/.test(ipZip) && ipZip !== '95118') {
    const la = typeof geoXff.json.lat === 'number' ? geoXff.json.lat : 37.3861;
    const lo = typeof geoXff.json.lon === 'number' ? geoXff.json.lon : -122.0839;
    await apiJson('/api/listings/ingest', {
      method: 'POST',
      body: {
        zip: ipZip,
        triggerAlerts: false,
        listings: [
          {
            address: `300 Synthparcelgamma Ct, ${geoXff.json.city || 'Testville'}, ${geoXff.json.regionCode || 'CA'} ${ipZip}`,
            street: '300 Synthparcelgamma Ct',
            city: geoXff.json.city || 'Testville',
            state: geoXff.json.regionCode || 'CA',
            zip: ipZip,
            listPrice: 1450000,
            beds: 3,
            baths: 2.0,
            sqft: 1650,
            yearBuilt: 1985,
            propertyType: 'Single Family Residential',
            status: 'for_sale',
            daysOnMarket: 5,
            lat: la,
            lon: lo,
            source: 'Open Test MLS Feed',
          },
        ],
      },
    });
  }
  console.log('✓ Ingested synthetic test listings via POST /api/listings/ingest');

  // 4. Verify /api/area/forsale & /api/suggest
  const forsale = await apiJson('/api/area/forsale?zip=95118&limit=6');
  assert.equal(forsale.status, 200, `GET /api/area/forsale failed: ${forsale.text}`);
  assert(
    forsale.json && Array.isArray(forsale.json.listings) && forsale.json.listings.length >= 2,
    `Expected >=2 listings from /api/area/forsale: ${forsale.text}`
  );
  assertCleanText(forsale.text, '/api/area/forsale');
  for (const portal of FORBIDDEN_PORTALS) {
    assert(!forsale.text.toLowerCase().includes(portal), `Portal leak "${portal}" in /api/area/forsale`);
  }

  const suggest = await apiJson('/api/suggest?q=100%20Synthparcel');
  assert.equal(suggest.status, 200, `GET /api/suggest failed: ${suggest.text}`);
  console.log('✓ /api/area/forsale & /api/suggest OK');

  // 5. Generate keyless property report for synthetic listing
  const reportSlug = await generateSyntheticReport(
    '100 Synthparcelalpha Ln, San Jose, CA 95118',
    37.2580,
    -121.8980
  );
  console.log(`✓ Generated keyless synthetic property report: /r/${reportSlug}/`);

  // 6. Launch headless Chromium and run light + dark colorScheme suites
  const executablePath = findChromiumExecutable();
  const browser = await chromium.launch({
    headless: true,
    ...(executablePath ? { executablePath } : {}),
  });
  try {
    await verifyBrowserScheme(browser, 'light', reportSlug, ipZip);
    await verifyBrowserScheme(browser, 'dark', reportSlug, ipZip);
  } finally {
    await browser.close();
  }

  console.log('\n🎉 ALL Open Estate OSS Docker E2E checks passed (light + dark)!');
}

run().catch((err) => {
  console.error('❌ E2E failed:', err);
  process.exit(1);
});
