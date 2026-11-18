#!/usr/bin/env node
/**
 * Open Estate Full Real-Browser E2E Regression Suite (Playwright + Headless Chromium)
 *
 * Usage:
 *   BASE_URL=http://127.0.0.1:41234 node e2e/playwright_e2e.mjs
 *   BASE_URL=https://openestate.org SCREENSHOT_DIR=./audit_after node e2e/playwright_e2e.mjs
 *
 * Verifies end-to-end in a real Chromium browser:
 *  1. Homepage address autocomplete dropdown (#homeInput / #homeAc) — >=3 char trigger, <mark>
 *     highlighting, mouse click, keyboard ArrowDown/ArrowUp/Enter/Escape, re-open on focus, clear (×).
 *  2. Client-side incomplete-address blocking on Homepage (#homeForm / #homeAddrErr) — blocks bare
 *     city, bare ZIP, street-without-location, 5-digit-house-number-without-location, street-suffix
 *     "Ct"/"Wy" lookalikes, city+ZIP-without-street, and PO Boxes; 0 enqueue calls, 0 pending cards.
 *  3. Server-side incomplete-address blocking — GET /report?address=<incomplete> returns HTTP 400
 *     styled HTML (#invalidAddressCard) with #completeAddrBtn -> /?q=... prefill; POST /api/report/enqueue
 *     & GET /api/report return HTTP 400 INVALID_ADDRESS JSON.
 *  4. Homepage i18n (EN / 中文 / ES) including direct /?lang=zh load and localized validation errors.
 *  5. Complete property report (/r/:slug/) — all 19 sections rendered, zero tech/brand leaks
 *     (gpt-4.1, gpt-4o-mini, v1.0.0-rust, retired brand).
 *  6. Report navbar search (#searchWrap / #searchInput / #searchAc / #searchAddrErr) — uncrushed
 *     input width on 1440px desktop, wide >=400px dropdown, inline incomplete-address blocking
 *     without overlapping suggestions, localized Analyze / 分析 / Analizar button.
 *  7. Report interactive features — Likes toggle, 24h Refresh button/cooldown, Buyer Diligence &
 *     HOA tabs, strictly Simplified Chinese #fengshui across EN/中文/ES + Kua 命卦 calculator,
 *     Docs & AI tabs, CRE Underwriting calculation, and AI Q&A (/api/ask).
 *  8. Your Match / AI Home Finder (/?tab=match), Area Alerts (/?tab=alerts), and Deal Room (/deal-room).
 */

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
let playwrightPkg;
try {
  playwrightPkg = require('playwright');
} catch {
  if (process.env.PLAYWRIGHT_MODULE_PATH) {
    playwrightPkg = require(process.env.PLAYWRIGHT_MODULE_PATH);
  } else {
    throw new Error('Playwright not found. Install it (`npm i -D playwright`) or set PLAYWRIGHT_MODULE_PATH.');
  }
}
const { chromium } = playwrightPkg;

const BASE_URL = (process.env.BASE_URL || 'http://127.0.0.1:8787').replace(/\/+$/, '');
const PRIMARY_SLUG = process.env.PRIMARY_SLUG || '100-synthparcelalpha-ln-san-jose-ca-95118';
const SECONDARY_SLUG = process.env.SECONDARY_SLUG || '200-synthparcelbeta-ave-campbell-ca-95008';
const SCREENSHOT_DIR = process.env.SCREENSHOT_DIR ? path.resolve(process.env.SCREENSHOT_DIR) : null;

if (SCREENSHOT_DIR) {
  fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
}

function findChromiumExecutable() {
  if (process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH && fs.existsSync(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH)) {
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

const DESKTOP_CHROME_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36';

const RETIRED_BRAND = ['Home', 'Lens'].join('');
const FORBIDDEN_TOKENS = [
  'gpt-4.1',
  'gpt-4o-mini',
  'v1.0.0-rust',
  'fallbackUsed',
  'deterministic-fallback',
  'rust-sqlx-sqlite',
  'max reasoning',
  'aiModel',
  'OPENAI_BASE_URL',
  'OPENAI_API_KEY',
  RETIRED_BRAND,
];

const STRICT_WAVE2_FORBIDDEN_TOKENS = [
  'provider',
  'effort',
  'gpt-4.1',
  'gpt-4o-mini',
  'fallbackUsed',
  'deterministic-fallback',
  'rust-sqlx-sqlite',
  'max reasoning',
  'aiModel',
  'OPENAI_BASE_URL',
  'OPENAI_API_KEY',
  RETIRED_BRAND,
];

function assertNoStrictRawForbidden(rawText, label) {
  const hay = String(rawText || '').toLowerCase();
  for (const tok of STRICT_WAVE2_FORBIDDEN_TOKENS) {
    assert(
      !hay.includes(tok.toLowerCase()),
      `[${label}] Raw payload leaked forbidden substring "${tok}"`
    );
  }
}

async function assertNoForbiddenLeaks(page, label) {
  const { visibleText, html } = await page.evaluate(() => ({
    visibleText: document.body?.innerText || '',
    html: document.documentElement?.innerHTML || '',
  }));
  for (const tok of FORBIDDEN_TOKENS) {
    assert(
      !visibleText.includes(tok),
      `[${label}] Visible page text leaked forbidden token "${tok}"`
    );
    assert(
      !html.toLowerCase().includes(tok.toLowerCase()),
      `[${label}] Rendered HTML leaked forbidden token "${tok}"`
    );
  }
}

let shotSeq = 0;
async function snap(page, name, fullPage = false) {
  if (!SCREENSHOT_DIR) return;
  shotSeq += 1;
  const file = path.join(SCREENSHOT_DIR, `${String(shotSeq).padStart(2, '0')}_${name}.png`);
  await page.screenshot({ path: file, fullPage });
  console.log(`   📸 saved ${file}`);
}

async function snapNamed(page, filename, fullPage = false) {
  if (!SCREENSHOT_DIR) return;
  const file = path.join(SCREENSHOT_DIR, filename);
  await page.screenshot({ path: file, fullPage });
  console.log(`   📸 saved ${file}`);
}

async function run() {
  const executablePath = findChromiumExecutable();
  console.log(`▶ Starting Open Estate E2E Browser Suite against ${BASE_URL}`);
  const browser = await chromium.launch({
    headless: true,
    ...(executablePath ? { executablePath } : {}),
  });

  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 1,
    userAgent: DESKTOP_CHROME_UA,
  });

  await context.route('**/api/suggest**', async (route) => {
    const reqUrl = new URL(route.request().url());
    const q = (reqUrl.searchParams.get('q') || '').trim();
    if (q.length < 3) {
      await route.continue();
      return;
    }
    if (q.toLowerCase().includes('synthparcel')) {
      const suggestions = q.startsWith('400')
        ? [
            {
              full: '400 Synthparceldelta Dr, San Jose, CA 95118',
              primary: '400 Synthparceldelta Dr',
              secondary: 'San Jose, CA 95118',
              source: 'osm',
            },
          ]
        : [
            {
              full: '300 Synthparcelgamma Ct, San Jose, CA 95118',
              primary: '300 Synthparcelgamma Ct',
              secondary: 'San Jose, CA 95118',
              source: 'osm',
            },
            {
              full: '400 Synthparceldelta Dr, San Jose, CA 95118',
              primary: '400 Synthparceldelta Dr',
              secondary: 'San Jose, CA 95118',
              source: 'osm',
            },
          ];
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          ok: true,
          provider: 'OpenStreetMap',
          suggestions,
        }),
      });
      return;
    }
    await route.continue();
  });

  const pageErrors = [];
  const page = await context.newPage();
  page.on('pageerror', (err) => {
    pageErrors.push(String(err?.stack || err?.message || err));
  });

  let enqueueCalls = [];
  page.on('request', (req) => {
    if (req.url().includes('/api/report/enqueue') && req.method() === 'POST') {
      enqueueCalls.push({ url: req.url(), postData: req.postData() || '' });
    }
  });

  try {
    // =========================================================================
    // 1. Homepage Autocomplete Dropdown (#homeInput / #homeAc)
    // =========================================================================
    const homeResp = await page.goto(`${BASE_URL}/`, { waitUntil: 'domcontentloaded' });
    assert.equal(homeResp.status(), 200, 'GET / should return 200');
    await assertNoForbiddenLeaks(page, 'Homepage /');

    // < 3 chars should NOT open #homeAc
    await page.fill('#homeInput', '30');
    await page.waitForTimeout(220);
    assert.equal(await page.locator('#homeAc.open').count(), 0, '#homeAc must stay closed for <3 chars');

    // >= 3 chars opens #homeAc with highlighted <mark> suggestions
    await page.fill('#homeInput', '300 Synthparcel');
    await page.locator('#homeAc.open .aci').first().waitFor({ state: 'visible', timeout: 8000 });
    const homeAcBox = await page.locator('#homeAc').boundingBox();
    assert(homeAcBox && homeAcBox.width >= 400, `Expected #homeAc width >= 400px, got ${homeAcBox?.width}`);
    const markCount = await page.locator('#homeAc.open .aci .pri mark').count();
    assert(markCount >= 1, 'Expected <mark> highlight inside #homeAc suggestions');
    await snap(page, 'home_autocomplete_open');

    // Outside mousedown closes #homeAc; re-focusing #homeInput re-opens it
    await page.mouse.click(10, 10);
    await page.waitForFunction(() => !document.getElementById('homeAc')?.classList.contains('open'));
    await page.focus('#homeInput');
    await page.waitForFunction(() => document.getElementById('homeAc')?.classList.contains('open'));

    // Keyboard navigation: ArrowDown highlights item 0, Enter picks it without submitting
    await page.keyboard.press('ArrowDown');
    await page.locator('#homeAc.open .aci.act').first().waitFor({ state: 'visible', timeout: 3000 });
    await page.keyboard.press('Enter');
    const pickedVal = await page.inputValue('#homeInput');
    assert(pickedVal.length > '300 Synthparcel'.length && pickedVal.includes(','), `Expected full picked address, got "${pickedVal}"`);
    assert.equal(await page.locator('#homeAc.open').count(), 0, '#homeAc should close after picking');
    assert(new URL(page.url()).pathname === '/', 'Picking an unviewed suggestion should fill input, not navigate away');

    // Clear button (×) resets input and hides dropdown
    await page.click('#homeClr');
    assert.equal(await page.inputValue('#homeInput'), '', '#homeClr should clear #homeInput');

    // Mouse click on suggestion also populates #homeInput cleanly
    await page.fill('#homeInput', '400 Synthparceldelta');
    await page.locator('#homeAc.open .aci').first().waitFor({ state: 'visible', timeout: 8000 });
    await page.locator('#homeAc.open .aci').first().click();
    const mousePicked = await page.inputValue('#homeInput');
    assert(mousePicked.toLowerCase().includes('400'), `Expected clicked suggestion in #homeInput, got "${mousePicked}"`);
    await page.click('#homeClr');
    console.log('✓ [1/9] Homepage address autocomplete dropdown (open, <mark>, keyboard, focus, clear, click)');

    // =========================================================================
    // 2. Homepage Client-Side Incomplete Address Blocking
    // =========================================================================
    enqueueCalls = [];
    const incompleteCases = [
      'San Jose',
      '95118',
      '300 Synthparcel',
      '123 Synthalpha St',
      '123 Synthalpha Ct',
      '20500 Synthblvd Blvd',
      'San Jose, CA 95121',
      'Synthparcelalpha Ln, San Jose, CA 95118',
      'PO Box 123, San Jose, CA 95118',
    ];

    for (const badAddr of incompleteCases) {
      await page.fill('#homeInput', badAddr);
      await page.click('#homeGoBtn');
      assert.equal(new URL(page.url()).pathname, '/', `Incomplete address "${badAddr}" must NOT navigate away from /`);
      const errVisible = await page.locator('#homeAddrErr.on').isVisible();
      assert(errVisible, `Expected #homeAddrErr.on visible for "${badAddr}"`);
      const errTxt = (await page.locator('#homeAddrErr').textContent() || '').trim();
      assert(errTxt.length >= 20, `Expected descriptive error text for "${badAddr}", got "${errTxt}"`);
      const hasInvalidCls = await page.evaluate(() => document.getElementById('homeForm')?.classList.contains('invalid'));
      assert(hasInvalidCls, `Expected #homeForm.invalid for "${badAddr}"`);
    }
    await page.fill('#homeInput', '300 Synthparcel');
    await page.locator('#homeAc.open .aci').first().waitFor({ state: 'visible', timeout: 8000 });
    await page.click('#homeGoBtn');
    const homeErrRect = await page.locator('#homeAddrErr').boundingBox();
    const homeAcRect = await page.locator('#homeAc').boundingBox();
    assert(homeErrRect && homeAcRect, 'Expected both #homeAddrErr and #homeAc visible simultaneously');
    assert(
      homeAcRect.y >= homeErrRect.y + homeErrRect.height - 2,
      `#homeAc (y=${homeAcRect.y}) must sit below #homeAddrErr (bottom=${homeErrRect.y + homeErrRect.height})`
    );
    await snap(page, 'home_incomplete_blocked_with_suggestions');

    // Also test pressing Enter directly when no suggestion is highlighted (idx === -1)
    await page.fill('#homeInput', 'San Jose');
    await page.keyboard.press('Enter');
    assert.equal(new URL(page.url()).pathname, '/', 'Enter on incomplete address must NOT navigate');
    assert(await page.locator('#homeAddrErr.on').isVisible(), 'Enter on incomplete address must show #homeAddrErr');

    assert.equal(enqueueCalls.length, 0, `Expected 0 /api/report/enqueue calls for incomplete inputs, got ${enqueueCalls.length}`);
    const pendingInStorage = await page.evaluate(() => localStorage.getItem('openestate:pending_reports:v1'));
    assert(!pendingInStorage || pendingInStorage === '[]', `Expected 0 polluted pending cards in localStorage, got ${pendingInStorage}`);
    console.log('✓ [2/9] Homepage client-side incomplete address blocking (all 9 patterns blocked, 0 enqueue calls, 0 bogus pending cards)');

    // =========================================================================
    // 3. Server-Side Incomplete Address Blocking (HTML 400 + JSON 400)
    // =========================================================================
    const badNavResp = await page.goto(`${BASE_URL}/report?address=300+Synthparcel`, { waitUntil: 'domcontentloaded' });
    assert.equal(badNavResp.status(), 400, 'Direct GET /report?address=300+Synthparcel must return HTTP 400');
    await page.locator('#invalidAddressCard').waitFor({ state: 'visible', timeout: 3000 });
    await snap(page, 'server_400_invalid_address_card');

    // Clicking #completeAddrBtn returns to /?q=300%20Synthparcel, pre-fills #homeInput, and opens #homeAc
    await page.click('#completeAddrBtn');
    await page.waitForURL((u) => u.pathname === '/' && u.searchParams.get('q') === '300 Synthparcel', { timeout: 5000 });
    assert.equal(await page.inputValue('#homeInput'), '300 Synthparcel');
    await page.locator('#homeAc.open .aci').first().waitFor({ state: 'visible', timeout: 8000 });

    // Direct API rejections (POST /api/report/enqueue & GET /api/report)
    for (const badApiAddr of ['San Jose', '300 Synthparcel', 'San Jose, CA 95121', '20500 Synthblvd Blvd', 'PO Box 123, San Jose, CA 95118']) {
      const apiRes = await page.evaluate(async (addr) => {
        const r1 = await fetch('/api/report/enqueue', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ address: addr }),
        });
        const j1 = await r1.json().catch(() => ({}));
        const r2 = await fetch('/api/report?address=' + encodeURIComponent(addr));
        const j2 = await r2.json().catch(() => ({}));
        return { s1: r1.status, c1: j1.code, s2: r2.status, c2: j2.code };
      }, badApiAddr);
      assert.equal(apiRes.s1, 400, `POST /api/report/enqueue must reject "${badApiAddr}" with 400`);
      assert.equal(apiRes.c1, 'INVALID_ADDRESS', `POST /api/report/enqueue code for "${badApiAddr}"`);
      assert.equal(apiRes.s2, 400, `GET /api/report must reject "${badApiAddr}" with 400`);
      assert.equal(apiRes.c2, 'INVALID_ADDRESS', `GET /api/report code for "${badApiAddr}"`);
    }
    console.log('✓ [3/9] Server-side incomplete address blocking (styled HTML 400 + /?q= recovery + API 400 INVALID_ADDRESS)');

    // =========================================================================
    // 4. Homepage Language Switching (/?lang=zh, ES, EN)
    // =========================================================================
    await page.goto(`${BASE_URL}/?lang=zh`, { waitUntil: 'domcontentloaded' });
    const zhLangAttr = await page.getAttribute('html', 'lang');
    assert(zhLangAttr && zhLangAttr.startsWith('zh'), `Expected html[lang^=zh] on /?lang=zh, got "${zhLangAttr}"`);
    const zhBtnText = (await page.locator('#homeGoBtn').textContent() || '').trim();
    assert(
      zhBtnText.includes('生成') && zhBtnText.includes('报告'),
      `Expected Chinese #homeGoBtn on /?lang=zh, got "${zhBtnText}"`
    );
    await page.fill('#homeInput', '300 Synthparcel');
    await page.click('#homeGoBtn');
    const zhErrText = (await page.locator('#homeAddrErr').textContent() || '').trim();
    assert(zhErrText.includes('地址不完整'), `Expected Chinese incomplete address error, got "${zhErrText}"`);
    await snap(page, 'home_zh_localized');

    await page.click('#langSwitch button[data-lang="es"]');
    const esBtnText = (await page.locator('#homeGoBtn').textContent() || '').trim();
    assert.equal(esBtnText, 'Generar informe', `Expected Spanish #homeGoBtn, got "${esBtnText}"`);
    const esErrText = (await page.locator('#homeAddrErr').textContent() || '').trim();
    assert(esErrText.includes('Dirección incompleta'), `Expected Spanish incomplete address error, got "${esErrText}"`);

    await page.click('#langSwitch button[data-lang="en"]');
    assert.equal(await page.getAttribute('html', 'lang'), 'en');
    console.log('✓ [4/9] Homepage i18n (/?lang=zh initial load, ES toggle, EN restore, localized address errors)');

    // =========================================================================
    // 5. Complete Property Report & Uncrushed Navbar Search (#searchInput / #searchAc)
    // =========================================================================
    const repResp = await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
    assert.equal(repResp.status(), 200, `GET /r/${PRIMARY_SLUG}/ should return 200`);
    await page.locator('#overview h1').waitFor({ state: 'visible', timeout: 8000 });
    // Wait for /api/health to enable #searchForm
    await page.waitForFunction(() => !document.getElementById('searchForm')?.classList.contains('off'), null, { timeout: 6000 });
    await assertNoForbiddenLeaks(page, `Report /r/${PRIMARY_SLUG}/`);

    // Verify all 21 sections exist and rendered without "Section render error"
    const sectionErrCount = await page.locator('main .empty:has-text("Section render error")').count();
    assert.equal(sectionErrCount, 0, 'Report must have 0 Section render errors');
    for (const secId of [
      'overview', 'house', 'house3d', 'value', 'history', 'county', 'comps', 'market', 'outlook',
      'payment', 'diligence', 'risk', 'environment', 'schools', 'safety',
      'neighborhood', 'fengshui', 'inspection', 'ai', 'more', 'cre',
    ]) {
      assert.equal(await page.locator(`#${secId}`).count(), 1, `Missing section #${secId}`);
    }

    // Verify navbar search input & dropdown dimensions (regression test for crushed 35px input!)
    const swrapBox = await page.locator('#searchWrap').boundingBox();
    const sinputBox = await page.locator('#searchInput').boundingBox();
    assert(swrapBox && swrapBox.width >= 220, `Expected #searchWrap width >= 220px, got ${swrapBox?.width}`);
    assert(sinputBox && sinputBox.width >= 115, `Expected #searchInput width >= 115px (uncrushed), got ${sinputBox?.width}`);

    await page.fill('#searchInput', '300 Synthparcel');
    await page.locator('#searchAc.open .aci').first().waitFor({ state: 'visible', timeout: 8000 });
    const navAcBox = await page.locator('#searchAc').boundingBox();
    assert(navAcBox && navAcBox.width >= 400, `Expected #searchAc width >= 400px, got ${navAcBox?.width}`);
    await snap(page, 'report_navbar_autocomplete_wide');

    // Submit incomplete "300 Synthparcel" in report navbar -> blocked inline, no overlap between #searchAddrErr and #searchAc
    enqueueCalls = [];
    const beforeRepUrl = page.url();
    await page.click('#navSearchBtn');
    assert.equal(page.url(), beforeRepUrl, 'Incomplete report navbar search must NOT navigate');
    assert(await page.locator('#searchAddrErr.on').isVisible(), 'Expected #searchAddrErr.on visible');
    assert.equal(enqueueCalls.length, 0, 'Incomplete report navbar search must NOT call /api/report/enqueue');
    const errRect = await page.locator('#searchAddrErr').boundingBox();
    const acRectAfterErr = await page.locator('#searchAc').boundingBox();
    if (errRect && acRectAfterErr) {
      assert(
        acRectAfterErr.y >= errRect.y + errRect.height - 2,
        `#searchAc (y=${acRectAfterErr.y}) must sit below #searchAddrErr (bottom=${errRect.y + errRect.height})`
      );
    }
    await snap(page, 'report_navbar_incomplete_blocked');

    // Pick suggestion in #searchAc -> clears error and populates full address
    await page.locator('#searchAc.open .aci').first().click();
    assert.equal(await page.locator('#searchAddrErr.on').count(), 0, 'Picking suggestion should clear #searchAddrErr');
    const navPicked = await page.inputValue('#searchInput');
    assert(navPicked.includes(','), `Expected full address in #searchInput, got "${navPicked}"`);
    await page.click('#searchClr');
    console.log(`✓ [5/9] Report /r/${PRIMARY_SLUG}/ 19 sections, wide navbar search (${Math.round(sinputBox.width)}px input / ${Math.round(navAcBox.width)}px dropdown), & inline incomplete block`);

    // =========================================================================
    // 6. Report Interactive Features (Likes, Refresh, Diligence/HOA, Chinese Feng Shui + Kua, Docs, CRE, AI Q&A)
    // =========================================================================
    // Likes toggle
    await page.locator('#likeBtn').waitFor({ state: 'visible', timeout: 5000 });
    const likedBefore = await page.evaluate(() => document.getElementById('likeBtn')?.classList.contains('liked'));
    await page.click('#likeBtn');
    await page.waitForFunction(
      (prev) => Boolean(document.getElementById('likeBtn')?.classList.contains('liked')) !== prev,
      likedBefore,
      { timeout: 6000 }
    );
    // Toggle back to keep live count clean
    await page.click('#likeBtn');
    await page.waitForFunction(
      (prev) => Boolean(document.getElementById('likeBtn')?.classList.contains('liked')) === prev,
      likedBefore,
      { timeout: 6000 }
    );

    // Refresh button presence & state
    assert.equal(await page.locator('#refreshBtn').count(), 1, 'Expected #refreshBtn in report header');

    // Dedicated 3D Model tab (#house3d)
    assert.equal(await page.locator('#tabsnav a[href="#house3d"]').count(), 1, 'Expected 3D Model tab link in #tabsnav');
    assert.equal(await page.locator('#house3d #h3dStage').count(), 1, 'Expected #h3dStage inside #house3d');

    // Lot-Number Hover Detail Popover (#lotDetailPopover) on #overview hero KPI & .facts
    const lotTriggersCount = await page.locator('#overview [data-lot-hover="1"]').count();
    if (lotTriggersCount > 0) {
      const heroLotTrig = page.locator('#overview .keys [data-lot-hover="1"]').first();
      await heroLotTrig.scrollIntoViewIfNeeded();
      await heroLotTrig.hover();
      await page.locator('#lotDetailPopover').waitFor({ state: 'visible', timeout: 3000 });
      const popBox = await page.locator('#lotDetailPopover').boundingBox();
      const vp = page.viewportSize() || { width: 1440, height: 900 };
      assert(popBox && popBox.x >= 0 && popBox.y >= 0 && popBox.x + popBox.width <= vp.width, `Expected #lotDetailPopover inside viewport, got ${JSON.stringify(popBox)}`);
      const popTextEn = await page.locator('#lotDetailPopover').innerText();
      assert(popTextEn.includes('Lot Size & Multi-Source Detail') && popTextEn.includes('sq ft') && popTextEn.includes('Full County Records'), `Unexpected #lotDetailPopover text: "${popTextEn}"`);
      await snap(page, 'report_lot_hover_en');

      // Also verify .facts lot trigger opens #lotDetailPopover cleanly without overflow:hidden clipping
      const factsLotTrig = page.locator('#overview .facts [data-lot-hover="1"]').first();
      if ((await factsLotTrig.count()) > 0) {
        await factsLotTrig.scrollIntoViewIfNeeded();
        await factsLotTrig.hover();
        await page.locator('#lotDetailPopover').waitFor({ state: 'visible', timeout: 3000 });
        const factsPopBox = await page.locator('#lotDetailPopover').boundingBox();
        assert(factsPopBox && factsPopBox.width >= 300 && factsPopBox.height >= 120, `Expected unclipped #lotDetailPopover from .facts trigger, got ${JSON.stringify(factsPopBox)}`);
      }
      // Dismiss popover
      await page.mouse.move(5, 5);
      await page.waitForFunction(() => Boolean(document.getElementById('lotDetailPopover')?.hidden), null, { timeout: 3000 });
    }

    // Dedicated County Records section (#county inside Q2 Risk drawer)
    assert.equal(await page.locator('#tabsnav a[href="#county"]').count(), 1, 'Expected County Records link in #tabsnav');
    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('county'));
    await page.locator('#county').waitFor({ state: 'visible', timeout: 5000 });
    const countyTextEn = await page.locator('#county').innerText();
    assert(countyTextEn.includes('County Records') && countyTextEn.includes('Parcel'), `Expected County Records content in #county, got "${countyTextEn.slice(0, 120)}"`);
    if ((await page.locator('#countyRawDetails').count()) > 0) {
      const countyEvDrawer = page.locator('#county details.sec-evidence-drawer:not([open]) > summary.sec-evidence-summary');
      if ((await countyEvDrawer.count()) > 0) {
        await countyEvDrawer.first().click();
      }
      await page.fill('#countyRawSearch', 'APN');
      assert((await page.locator('#countyRawTable tbody tr:visible').count()) >= 1, 'Expected raw county field search to match APN');
      await page.fill('#countyRawSearch', '');
    }

    // Buyer Diligence & HOA sub-tabs (inside Q1 Cost drawer)
    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('diligence'));
    await page.locator('#diligence .seg button').first().waitFor({ state: 'visible', timeout: 6000 });
    const dilBtns = page.locator('#diligence .seg button');
    const dilCount = await dilBtns.count();
    assert(dilCount >= 3, `Expected >=3 sub-tabs in #diligence, got ${dilCount}`);
    for (let i = 0; i < dilCount; i++) {
      await dilBtns.nth(i).click();
    }

    // Feng Shui strictly hidden in EN mode (zero visible 堪舆风水 in EN!)
    assert.equal(await page.getAttribute('html', 'lang'), 'en');
    assert.equal(await page.locator('#fengshui').isVisible(), false, '#fengshui must be hidden when language is EN');
    const visibleBodyEn = await page.evaluate(() => document.body.innerText || '');
    assert(!visibleBodyEn.includes('堪舆风水'), 'Visible EN page must never show 堪舆风水');
    assert.equal(await page.locator('a[href="/ops"]').count(), 0, 'Report page must have zero public /ops links');

    // Docs & AI tabs (#inspection inside Q2 Risk drawer)
    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('inspection'));
    const docTabs = page.locator('#inspection .seg button:visible');
    const docTabCount = await docTabs.count();
    for (let i = 0; i < docTabCount; i++) {
      await docTabs.nth(i).click();
    }

    // CRE Underwriting (#cre inside Q5 Potential drawer — cloud/CRE-gated in Personal OSS residential scope)
    const creAllowedOnPage = await page.evaluate(
      () => Boolean(window.__CLOUD_CRE_ENABLED__ === true || window.__REPORT__?.cre || window.__REPORT__?.modules?.cre)
    );
    if (creAllowedOnPage) {
      await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('cre'));
      await page.locator('#creRunBtn').waitFor({ state: 'visible', timeout: 5000 });
      await page.click('#creRunBtn');
      await page.waitForFunction(
        () => (document.getElementById('cre')?.innerText || '').includes('NOI') && !(document.getElementById('creRunBtn')?.disabled),
        null,
        { timeout: 8000 }
      );
    } else {
      assert.equal(await page.locator('#cre').isVisible(), false, 'Personal OSS residential report hides #cre unless cloud/CRE data is present');
    }

    // AI Analyst Q&A (#chatForm via floating Open Estate drawer)
    await page.evaluate(() => window.openOpenEstateChat?.());
    await page.locator('#chatIn').waitFor({ state: 'visible', timeout: 4000 });
    await page.fill('#chatIn', 'What is the estimated monthly cost and HOA status?');
    await page.click('#chatSend');
    await page.waitForFunction(
      () => {
        const msgs = [...document.querySelectorAll('#chatLog .msg.a')];
        return msgs.length >= 2 && !msgs[msgs.length - 1].textContent.includes('Thinking with Open Estate AI');
      },
      null,
      { timeout: 25000 }
    );
    const lastAiMsg = await page.locator('#chatLog .msg.a').last().innerText();
    assert(lastAiMsg.length >= 15 && !lastAiMsg.startsWith('Could not reach /api/ask'), `Unexpected /api/ask reply: "${lastAiMsg}"`);
    await page.evaluate(() => window.closeOpenEstateChat?.());
    console.log('✓ [6/9] Report interactive features (Likes, Refresh, Diligence/HOA, EN hides 风水, Docs tabs, CRE Underwriting, AI Q&A)');

    // =========================================================================
    // 7. Report Multi-Language Switch (中文 & ES) + Chinese-Only 风水 + Localized Analyze Button
    // =========================================================================
    await page.click('#langSwitch button[data-lang="zh"]');
    await page.waitForFunction(
      () => document.documentElement.lang === 'zh-CN' && !document.querySelector('#langSwitch button[data-lang="zh"]')?.disabled,
      null,
      { timeout: 25000 }
    );
    assert.equal((await page.locator('#navSearchBtn').textContent() || '').trim(), '分析', '#navSearchBtn must localize to 分析 in 中文');
    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('county'));
    assert((await page.locator('#county').innerText()).includes('县'), '#county must localize to Chinese in 中文 mode');

    // Feng Shui + Kua 命卦 calculator visible and interactive in 中文 mode
    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('fengshui'));
    await page.locator('#fengshui').waitFor({ state: 'visible', timeout: 5000 });
    const fsTextZh = await page.locator('#fengshui').innerText();
    assert(fsTextZh.includes('堪舆风水') || fsTextZh.includes('九宫飞星') || fsTextZh.includes('八宅'), 'Expected Chinese Feng Shui content in 中文 mode');
    await page.fill('#kuaYear', '1988');
    await page.click('#kuaBtn');
    const kuaOutText = await page.locator('#kuaOut').innerText();
    assert(kuaOutText.includes('命') && kuaOutText.includes('吉方'), `Expected Kua 命卦 output, got "${kuaOutText}"`);

    await page.evaluate(() => window.scrollTo(0, 0));
    if ((await page.locator('#overview .keys [data-lot-hover="1"]').count()) > 0) {
      const zhTrig = page.locator('#overview .keys [data-lot-hover="1"]').first();
      await zhTrig.scrollIntoViewIfNeeded();
      await zhTrig.hover();
      await page.locator('#lotDetailPopover').waitFor({ state: 'visible', timeout: 3000 });
      const zhPopTxt = await page.locator('#lotDetailPopover').innerText();
      assert(zhPopTxt.includes('宗地面积多源核对明细') && zhPopTxt.includes('查看完整县府档案'), `Expected Chinese #lotDetailPopover, got "${zhPopTxt}"`);
      await snap(page, 'report_lot_hover_zh');
      await page.mouse.move(5, 5);
    }
    await snap(page, 'report_zh_localized');

    await page.click('#langSwitch button[data-lang="es"]');
    await page.waitForFunction(() => document.documentElement.lang === 'es', null, { timeout: 5000 });
    assert.equal((await page.locator('#navSearchBtn').textContent() || '').trim(), 'Analizar', '#navSearchBtn must localize to Analizar in ES');
    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('county'));
    assert((await page.locator('#county').innerText()).includes('Catastral'), '#county must localize to Spanish in ES mode');
    await page.evaluate(() => window.scrollTo(0, 0));
    if ((await page.locator('#overview .keys [data-lot-hover="1"]').count()) > 0) {
      const esTrig = page.locator('#overview .keys [data-lot-hover="1"]').first();
      await esTrig.scrollIntoViewIfNeeded();
      await esTrig.hover();
      await page.locator('#lotDetailPopover').waitFor({ state: 'visible', timeout: 3000 });
      const esPopTxt = await page.locator('#lotDetailPopover').innerText();
      assert(esPopTxt.includes('Detalle y Conciliación del Lote'), `Expected Spanish #lotDetailPopover, got "${esPopTxt}"`);
      await page.mouse.move(5, 5);
    }
    assert.equal(await page.locator('#fengshui').isVisible(), false, '#fengshui must be hidden when language is ES');
    const visibleBodyEs = await page.evaluate(() => document.body.innerText || '');
    assert(!visibleBodyEs.includes('堪舆风水'), 'Visible ES page must never show 堪舆风水');

    await page.click('#langSwitch button[data-lang="en"]');
    await page.waitForFunction(() => document.documentElement.lang === 'en', null, { timeout: 5000 });
    assert.equal((await page.locator('#navSearchBtn').textContent() || '').trim(), 'Analyze');
    console.log('✓ [7/9] Report i18n (中文 -> 分析 + 堪舆风水, ES -> Analizar without 风水, EN -> Analyze without 风水)');

    // =========================================================================
    // 8. Your Match / AI Home Finder (/?tab=match) & Area Alerts (/?tab=alerts)
    // =========================================================================
    await page.goto(`${BASE_URL}/?tab=match`, { waitUntil: 'domcontentloaded' });
    await page.locator('#matchQuizStudio').waitFor({ state: 'visible', timeout: 5000 });
    await page.click('#toggleAllStepsBtn');
    assert.equal(await page.locator('.qstep.act').count(), 6, 'Show All Steps should display all 6 quiz steps');
    await page.click('#toggleAllStepsBtn');

    await page.fill('#nlQuery', '3+ bed single family home in 95118 under $2.6M');
    await page.click('#nlSearchBtn');
    await page.waitForFunction(
      () => !document.getElementById('nlSearchBtn')?.disabled && (document.getElementById('nlResults')?.innerText || '').length > 10,
      null,
      { timeout: 20000 }
    );
    await assertNoForbiddenLeaks(page, 'Your Match / AI Home Finder');
    await snap(page, 'your_match_finder');

    // Area & ZIP Alerts tab (hidden in top tab strip when SMTP is disabled, reachable via /?tab=alerts)
    if (await page.locator('#tabBtnAlerts').isVisible()) {
      await page.click('#tabBtnAlerts');
    } else {
      await page.goto(`${BASE_URL}/?tab=alerts`, { waitUntil: 'domcontentloaded' });
    }
    await page.locator('#subForm').waitFor({ state: 'visible', timeout: 3000 });
    await page.fill('#subEmail', 'e2e-regression-check@example.com');
    await page.fill('#subZips', '95118');
    await page.fill('#subReq', 'Single-family quiet street');
    await page.click('#subSubmitBtn');
    await page.waitForFunction(
      () => !document.getElementById('subSubmitBtn')?.disabled && (document.getElementById('subStatusMsg')?.textContent || '').length > 5,
      null,
      { timeout: 10000 }
    );
    const savedAlertRm = page.locator('[data-rm-alert]').first();
    if ((await savedAlertRm.count()) > 0) {
      await savedAlertRm.click();
    }
    console.log('✓ [8/9] Your Match 6-step studio, AI Home Finder (zero retired-brand leaks), & Area Alerts subscription');

    // =========================================================================
    // 9. Deal Room Side-by-Side Comparison (/deal-room)
    // =========================================================================
    // Visit secondary slug first so both slugs exist in visitor history if present
    const secCheck = await page.goto(`${BASE_URL}/r/${SECONDARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
    const dealUrl = secCheck.status() === 200
      ? `${BASE_URL}/deal-room?slugs=${encodeURIComponent(PRIMARY_SLUG)},${encodeURIComponent(SECONDARY_SLUG)}`
      : `${BASE_URL}/deal-room`;
    const dealResp = await page.goto(dealUrl, { waitUntil: 'domcontentloaded' });
    assert.equal(dealResp.status(), 200, 'GET /deal-room should return 200');
    await assertNoForbiddenLeaks(page, 'Deal Room');
    if (secCheck.status() === 200) {
      await page.waitForFunction(
        () => document.body.innerText.includes('Overall Score') || document.body.innerText.includes('综合评分'),
        null,
        { timeout: 8000 }
      );
      const zhDealBtn = page.locator('button[data-lang="zh"], button[data-lang="zh-CN"]').first();
      if ((await zhDealBtn.count()) > 0) {
        await zhDealBtn.click();
        await page.waitForTimeout(250);
      }
    }
    await snap(page, 'deal_room_comparison');
    console.log('✓ [9/10] Deal Room side-by-side comparison & zero forbidden leaks');

    // =========================================================================
    // 10. Open-Source Zero-Login UX & Direct Report Link Sharing
    // =========================================================================
    await page.goto(`${BASE_URL}/`, { waitUntil: 'domcontentloaded' });
    await assertNoForbiddenLeaks(page, 'Homepage Open-Source UI');
    assert.equal(await page.locator('#accountBtn').count(), 0, 'No SaaS Sign In button in open-source edition');

    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
    assert.equal(await page.locator('#accountBtn').count(), 0, 'No SaaS Sign In button on report navbar');
    if (await page.locator('#shareReportBtn').isVisible()) {
      await page.click('#shareReportBtn');
      assert.equal(await page.locator('#accountModal.open').count(), 0, 'Clicking Share copies direct link without opening any auth modal');
    }
    console.log('✓ [10/12] Open-source zero-login UX & direct report link sharing');

    // =========================================================================
    // 11. Verdict-First Desktop Hero, 5 Buyer Question Cards, Media Bento + Lightbox, Insurance Quote Studio, Walkthrough Deck
    // =========================================================================
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
    await page.locator('#reportTocAside').waitFor({ state: 'visible', timeout: 8000 });
    await page.locator('#densitySwitch').waitFor({ state: 'visible', timeout: 5000 });
    assert.equal(await page.locator('#chapterWorkspaceWrap').isVisible(), true, 'Desktop keeps sticky #chapterWorkspaceWrap visible in header');
    assert.equal(await page.locator('#chapterWorkspaceWrap .chap-pills').isVisible(), false, 'Desktop with Outline open hides duplicate .chap-pills only');
    await assertNoForbiddenLeaks(page, `Desktop Workspace /r/${PRIMARY_SLUG}/`);

    const parityDesktopCheck = await page.evaluate(() => {
      const hdr = document.querySelector('header.hdr');
      const cmdBtn = document.getElementById('toggleCommandCenterBtn');
      const wtBtn = document.getElementById('stickyWalkthroughBtn');
      const vidBtn = document.getElementById('chapterMotionReelBtn');
      const densSw = document.getElementById('densitySwitch');
      const cmdStyle = cmdBtn ? getComputedStyle(cmdBtn) : null;
      const roleBrief = document.getElementById('roleFocusBrief');
      const chooserBtns = roleBrief ? [...roleBrief.querySelectorAll('.rb-chooser-row[data-role-set]')] : [];

      for (let i = 0; i < 4; i++) {
        window.OpenEstateWorkspace.applyProgressiveDisclosure();
      }
      const expandInDrawers = document.querySelectorAll('details.sec-evidence-drawer > .sec-evidence-body > .brief-table-expand, details.sec-evidence-drawer > .sec-evidence-body > .brief-chk-expand').length;
      const adjDuplicateExpands = [...document.querySelectorAll('.brief-table-expand, .brief-chk-expand')].filter(
        (b) => b.nextElementSibling && (b.nextElementSibling.classList.contains('brief-table-expand') || b.nextElementSibling.classList.contains('brief-chk-expand'))
      ).length;
      const clampedHlRows = document.querySelectorAll('tr.hl.brief-clamped-row').length;
      const l2aTrapped = [
        '#ficoTierTable',
        '#mpVertTab',
        '#mpPaneMortgage',
        '#rateShockMatrixBox',
        '#house .hpfeats',
        '#houseFeatTabs',
        '#houseFeatDetectedOnly',
        '#comps .ccard',
        '#publicResearchBox',
      ].filter((sel) => {
        const node = document.querySelector(sel);
        return Boolean(node && node.closest('details.sec-evidence-drawer'));
      });

      return {
        allInStickyHdr: Boolean(hdr && densSw && cmdBtn && wtBtn && vidBtn && hdr.contains(densSw) && hdr.contains(cmdBtn) && hdr.contains(wtBtn) && hdr.contains(vidBtn)),
        cmdColor: cmdStyle ? cmdStyle.color : '',
        cmdBg: cmdStyle ? cmdStyle.backgroundColor : '',
        roleBriefVisible: Boolean(roleBrief && !roleBrief.hidden && roleBrief.offsetHeight > 40),
        chooserRoles: chooserBtns.map((b) => b.getAttribute('data-role-set')),
        expandInDrawers,
        adjDuplicateExpands,
        clampedHlRows,
        l2aTrapped,
      };
    });
    assert(parityDesktopCheck.allInStickyHdr, 'Sticky header must contain #densitySwitch, #toggleCommandCenterBtn, #stickyWalkthroughBtn, and #chapterMotionReelBtn');
    assert.notEqual(parityDesktopCheck.cmdColor, 'rgb(255, 255, 255)', `#toggleCommandCenterBtn must not render white text on light surface, got ${parityDesktopCheck.cmdColor} on ${parityDesktopCheck.cmdBg}`);
    assert(parityDesktopCheck.roleBriefVisible, '"Complete dossier" #roleFocusBrief must be visible in default role="all"');
    assert.deepEqual(parityDesktopCheck.chooserRoles, ['buyer', 'seller', 'agent'], 'Expected 3 role quick-pick buttons in #roleFocusBrief');
    assert.equal(parityDesktopCheck.expandInDrawers, 0, 'Repeated applyProgressiveDisclosure() must never swallow expand buttons into .sec-evidence-drawer');
    assert.equal(parityDesktopCheck.adjDuplicateExpands, 0, 'Repeated applyProgressiveDisclosure() must never create duplicate adjacent expand buttons');
    assert.equal(parityDesktopCheck.clampedHlRows, 0, '<tr class="hl"> total/summary rows must never be clamped in Brief mode');
    assert.deepEqual(parityDesktopCheck.l2aTrapped, [], `Interactive L2a controls must never be wrapped inside .sec-evidence-drawer: ${parityDesktopCheck.l2aTrapped.join(', ')}`);

    // Verify collapsed desktop TOC reveals .chap-pills alongside sticky right-side buttons, then reopen TOC
    await page.evaluate(() => window.OpenEstateWorkspace.setTocSidebarOpen(false));
    assert.equal(await page.locator('#chapterWorkspaceWrap .chap-pills').isVisible(), true, 'Collapsed desktop TOC shows .chap-pills in sticky bar');
    assert.equal(await page.locator('#toggleCommandCenterBtn').isVisible(), true, 'Collapsed desktop TOC keeps #toggleCommandCenterBtn visible');
    assert.equal(await page.locator('#stickyWalkthroughBtn').isVisible(), true, 'Collapsed desktop TOC keeps #stickyWalkthroughBtn visible');
    await page.evaluate(() => window.OpenEstateWorkspace.setTocSidebarOpen(true));

    // Verify sticky controls stay pinned near top of viewport when scrolled down
    await page.evaluate(() => window.scrollTo(0, 500));
    const stickyWhileScrolled = await page.evaluate(() => {
      const ids = ['densitySwitch', 'toggleCommandCenterBtn', 'stickyWalkthroughBtn', 'chapterMotionReelBtn'];
      return ids.every((id) => {
        const r = document.getElementById(id)?.getBoundingClientRect();
        return Boolean(r && r.width > 20 && r.top >= 0 && r.bottom <= 150);
      });
    });
    assert(stickyWhileScrolled, 'Sticky mode & tour controls must remain pinned near top of viewport on scroll');
    await page.evaluate(() => window.scrollTo(0, 0));

    const desktopOverflowOverview = await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth + 2
    );
    assert(desktopOverflowOverview, 'Desktop report overview must have zero horizontal overflow');

    // Verdict-First Split Hero above the fold on 1440 x 900
    const heroBoxCheck = await page.evaluate(() => {
      const ov = document.getElementById('overview')?.getBoundingClientRect();
      const media = document.getElementById('verdictMediaCol')?.getBoundingClientRect();
      const fl = document.getElementById('verdictFindingLine');
      const tiles = [...document.querySelectorAll('#verdictKeyTiles .verdict-tile')];
      const checks = [...document.querySelectorAll('#verdictFlagsChecks .vcheck-row')];
      return {
        ov,
        media,
        findingText: (fl?.innerText || '').trim(),
        tileCount: tiles.length,
        tilesText: tiles.map((t) => t.innerText.trim()),
        checkCount: checks.length,
        qCardCount: document.querySelectorAll('#questionCardsStack .q-card').length,
        nestedDetailsInDrawers: document.querySelectorAll('#questionCardsStack .q-drawer details:not(.sec-evidence-drawer)').length,
        evidenceDrawerCount: document.querySelectorAll('#questionCardsStack .q-drawer details.sec-evidence-drawer').length,
        defaultDensity: document.documentElement.getAttribute('data-density'),
      };
    });
    assert(heroBoxCheck.ov && heroBoxCheck.media && heroBoxCheck.media.left > heroBoxCheck.ov.left + 200, 'Desktop 1440x900 must display #overview beside #verdictMediaCol');
    assert(heroBoxCheck.findingText.length >= 25, `Expected plain-language #verdictFindingLine, got "${heroBoxCheck.findingText}"`);
    assert.equal(heroBoxCheck.tileCount, 3, 'Expected 3 real-quantity tiles in #verdictKeyTiles');
    assert(heroBoxCheck.tilesText.every((t) => !/\b\d+\s*\/\s*100\b/.test(t)), 'Verdict key tiles must never show bare 0-100 composite scores');
    assert(heroBoxCheck.checkCount >= 4, `Expected >=4 Flags & Clear Checks rows, got ${heroBoxCheck.checkCount}`);
    assert.equal(heroBoxCheck.qCardCount, 5, 'Expected 5 Buyer Question Cards on one page');
    assert.equal(heroBoxCheck.nestedDetailsInDrawers, 0, 'Zero un-flattened legacy <details> drawers allowed inside Question Card drawers');
    assert(heroBoxCheck.evidenceDrawerCount >= 4, `Expected >=4 L2b .sec-evidence-drawer blocks, got ${heroBoxCheck.evidenceDrawerCount}`);
    assert.equal(heroBoxCheck.defaultDensity, 'brief', 'Default report density should be brief');

    // Brief | Full details toggle + keyboard shortcut D
    await page.click('#densityBtnFull');
    assert.equal(await page.getAttribute('html', 'data-density'), 'full', 'Clicking Full details sets html[data-density="full"]');
    await page.keyboard.press('d');
    assert.equal(await page.getAttribute('html', 'data-density'), 'brief', 'Pressing D toggles back to html[data-density="brief"]');

    // 5 Buyer Question Cards expand/collapse cleanly & TOC jump opens parent drawer
    assert.equal(await page.locator('#qToggle_price').getAttribute('aria-expanded'), 'false', 'Q3 Price drawer starts collapsed');
    await page.click('#reportTocAside [data-toc-target="qCardPrice"]');
    await page.locator('#benchmarkValueMount').waitFor({ state: 'visible', timeout: 4000 });
    assert.equal(await page.locator('#qToggle_price').getAttribute('aria-expanded'), 'true', 'Clicking TOC Price opens Q3 Price drawer');
    await page.click('#qToggle_price');
    assert.equal(await page.locator('#qToggle_price').getAttribute('aria-expanded'), 'false', 'Clicking #qToggle_price collapses Q3 Price drawer');

    await page.evaluate(() => window.OpenEstateWorkspace.jumpToSection('risk'));
    assert.equal(await page.locator('#qToggle_risk').getAttribute('aria-expanded'), 'true', 'jumpToSection("risk") opens Q2 Risk drawer');
    await page.click('#qToggle_risk');

    await page.evaluate(() => window.scrollTo(0, 0));
    await page.waitForFunction(
      () => [...document.querySelectorAll('#photoBentoGrid img')].every((i) => i.complete && i.naturalWidth > 0),
      null,
      { timeout: 6000 }
    ).catch(() => {});
    await page.waitForTimeout(200);
    await snapNamed(page, 'desktop_overview.png');

    // Hero: the live satellite map with the lot / house / garage outlines is the default view;
    // listing photos & aerial archive crops are one tab away (Hero Media Bento & Lightbox).
    assert(
      !(await page.locator('#gallery').evaluate((el) => el.classList.contains('hero-parked'))),
      'The outlined satellite map (#gallery) must be the default hero view'
    );
    assert(
      await page.locator('#mediaTabMaps').evaluate((el) => el.classList.contains('on')),
      '#mediaTabMaps must be the active hero tab by default'
    );
    await page.waitForFunction(
      () => document.querySelectorAll('#gMain .leaflet-propOutline-pane path').length >= 1,
      null,
      { timeout: 8000 }
    );
    await page.click('#mediaTabPhotos');
    await page.locator('#heroMediaBento #photoBentoGrid').waitFor({ state: 'visible', timeout: 5000 });
    assert((await page.locator('#photoBentoGrid .bento-tile[data-photo-idx]').count()) >= 1, 'Expected at least 1 photo tile in #photoBentoGrid');
    assert(
      await page.locator('#gallery').evaluate((el) => el.classList.contains('hero-parked')),
      'Switching to Photos parks the map'
    );
    await page.locator('#openPhotoLightboxBtn').click();
    await page.locator('#photoLightboxModal').waitFor({ state: 'visible', timeout: 4000 });
    const plbInitial = (await page.locator('#plbCounter').innerText()).trim();
    assert(plbInitial.startsWith('1 / '), `Expected lightbox counter starting with "1 / ", got "${plbInitial}"`);
    const plbTotal = Number(plbInitial.split('/')[1]?.trim() || '1');
    if (plbTotal >= 2) {
      await page.click('#plbNextBtn');
      const plbSecond = (await page.locator('#plbCounter').innerText()).trim();
      assert(plbSecond.startsWith('2 / '), `Expected lightbox Next to advance to "2 / ", got "${plbSecond}"`);
      await page.click('#plbPrevBtn');
      assert((await page.locator('#plbCounter').innerText()).trim().startsWith('1 / '));
    }
    await page.click('#plbCloseBtn');
    await page.waitForFunction(() => Boolean(document.getElementById('photoLightboxModal')?.hidden), null, { timeout: 3000 });
    await page.click('#mediaTabMaps');
    assert(
      !(await page.locator('#gallery').evaluate((el) => el.classList.contains('hero-parked'))),
      'Map · Lot & House Outlines tab brings the outlined map back'
    );

    // Interactive Insurance Quote Studio (inside Q1 Cost card)
    await page.click('#reportTocAside [data-toc-target="qCardCost"]');
    await page.locator('#insuranceQuoteStudio #insCarrierTable').waitFor({ state: 'visible', timeout: 5000 });
    assert((await page.locator('#insCarrierTable tbody tr').count()) >= 3, 'Expected >=3 carriers in #insCarrierTable');
    const insMonthlyBefore = (await page.locator('#insStudioMonthly').innerText()).trim();
    const totalPayBefore = (await page.locator('#insStudioTotalPayment').innerText()).trim();
    await page.click('#insDeductibleSeg [data-deductible="5000"]');
    await page.waitForFunction(
      (prev) => (document.getElementById('insStudioMonthly')?.innerText || '').trim() !== prev,
      insMonthlyBefore,
      { timeout: 4000 }
    );
    const insAfterDed = (await page.locator('#insStudioMonthly').innerText()).trim();
    const totalAfterDed = (await page.locator('#insStudioTotalPayment').innerText()).trim();
    assert.notEqual(insAfterDed, insMonthlyBefore, 'Switching deductible to $5,000 must update #insStudioMonthly');
    assert.notEqual(totalAfterDed, totalPayBefore, 'Switching deductible must update #insStudioTotalPayment');
    await page.locator('#insToggleEarthquake').click();
    await page.waitForFunction(
      (prev) => (document.getElementById('insStudioMonthly')?.innerText || '').trim() !== prev,
      insAfterDed,
      { timeout: 4000 }
    );
    const insAfterQuake = (await page.locator('#insStudioMonthly').innerText()).trim();
    await page.locator('#insToggleFlood').click();
    await page.waitForFunction(
      (prev) => (document.getElementById('insStudioMonthly')?.innerText || '').trim() !== prev,
      insAfterQuake,
      { timeout: 4000 }
    );
    await page.locator('#insuranceQuoteStudio').scrollIntoViewIfNeeded();
    await snapNamed(page, 'desktop_insurance.png');

    // Interactive Home Walkthrough Slide Deck (#walkthroughModal via single sticky bar #stickyWalkthroughBtn) + POST /api/report/:slug/slides
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.locator('#stickyWalkthroughBtn').waitFor({ state: 'visible', timeout: 4000 });
    await page.click('#stickyWalkthroughBtn');
    await page.locator('#walkthroughModal').waitFor({ state: 'visible', timeout: 4000 });
    assert.equal((await page.locator('#wtStepCounter').innerText()).trim(), '1 / 8', 'Slide deck must open on slide 1 / 8');
    const slideTitle1 = (await page.locator('#wtSlideTitle').innerText()).trim();
    await page.click('#wtNextBtn');
    assert.equal((await page.locator('#wtStepCounter').innerText()).trim(), '2 / 8', 'Next button must advance slide 1 -> 2');
    const slideTitle2 = (await page.locator('#wtSlideTitle').innerText()).trim();
    assert.notEqual(slideTitle2, slideTitle1, 'Slide 2 title must differ from Slide 1');
    await page.click('#wtLangBar [data-wt-lang="zh"]');
    await page.waitForFunction(() => document.documentElement.lang === 'zh-CN', null, { timeout: 15000 });
    const zhSlideTitle = (await page.locator('#wtSlideTitle').innerText()).trim();
    assert(/[\u4e00-\u9fff]/.test(zhSlideTitle), `Expected Chinese slide title in 中文 mode, got "${zhSlideTitle}"`);
    await page.click('#wtLangBar [data-wt-lang="es"]');
    await page.waitForFunction(() => document.documentElement.lang === 'es', null, { timeout: 5000 });
    const esSlideTitle = (await page.locator('#wtSlideTitle').innerText()).trim();
    assert.notEqual(esSlideTitle, zhSlideTitle, 'ES slide title must localize to Spanish');
    await page.click('#wtLangBar [data-wt-lang="en"]');
    await page.waitForFunction(() => document.documentElement.lang === 'en', null, { timeout: 5000 });
    const slidesApiCheck = await page.evaluate(async (slug) => {
      const r = await fetch(`/api/report/${encodeURIComponent(slug)}/slides`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({}),
      });
      const j = await r.json().catch(() => ({}));
      return { status: r.status, ok: j.ok, count: j.walkthroughSlides?.slides?.length || 0 };
    }, PRIMARY_SLUG);
    assert.equal(slidesApiCheck.status, 200, 'POST /api/report/:slug/slides must return 200');
    assert.equal(slidesApiCheck.ok, true);
    assert.equal(slidesApiCheck.count, 8, 'POST /api/report/:slug/slides must return 8 slides');
    await snapNamed(page, 'desktop_slides_modal.png');
    await page.click('#wtCloseBtn');
    await page.waitForFunction(() => Boolean(document.getElementById('walkthroughModal')?.hidden), null, { timeout: 3000 });

    // Also verify SECONDARY_SLUG gets Verdict-First Hero, 5 Question Cards, Insurance Quote Studio, & 8 walkthroughSlides
    if (SECONDARY_SLUG && SECONDARY_SLUG !== PRIMARY_SLUG) {
      const secRep = await page.goto(`${BASE_URL}/r/${SECONDARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
      if (secRep && secRep.status() === 200) {
        await page.locator('#verdictFindingLine').waitFor({ state: 'visible', timeout: 5000 });
        await page.click('#mediaTabPhotos');
        await page.locator('#heroMediaBento #photoBentoGrid').waitFor({ state: 'visible', timeout: 5000 });
        await page.click('#reportTocAside [data-toc-target="qCardCost"]');
        await page.locator('#insuranceQuoteStudio #insCarrierTable').waitFor({ state: 'visible', timeout: 5000 });
        await page.locator('#stickyWalkthroughBtn').waitFor({ state: 'visible', timeout: 4000 });
        await assertNoForbiddenLeaks(page, `Desktop Workspace /r/${SECONDARY_SLUG}/`);
      }
    }
    console.log('✓ [11/15] Verdict-First Desktop Hero, 5 Buyer Question Cards, Media Bento + Lightbox, Insurance Quote Studio, & 8-Slide Walkthrough Deck');

    // =========================================================================
    // 13. 60fps Property Motion Reel (#motionReelModal, Scene Scrub 1->3, 16:9 <-> 9:16)
    // =========================================================================
    await page.setViewportSize({ width: 1440, height: 920 });
    const rawRepResp = await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
    assert.equal(rawRepResp.status(), 200, 'GET /r/:slug/ should return 200');
    const rawReportHtml = await rawRepResp.text();
    assertNoStrictRawForbidden(rawReportHtml, `Raw SSR HTML /r/${PRIMARY_SLUG}/`);

    await page.locator('#chapterMotionReelBtn').waitFor({ state: 'visible', timeout: 6000 });
    await page.click('#chapterMotionReelBtn');
    await page.locator('#motionReelModal').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('#motionReelCanvas').waitFor({ state: 'visible', timeout: 5000 });
    await page.waitForTimeout(350);

    // Verify default 16:9 on Desktop & non-empty rendered pixels on #motionReelCanvas
    const initialReelInfo = await page.evaluate(() => {
      const c = document.getElementById('motionReelCanvas');
      const st = window.OpenEstateMotion?.getState?.() || {};
      let nonZeroSamples = 0;
      let distinctColors = new Set();
      if (c) {
        try {
          const ctx = c.getContext('2d');
          const w = c.width;
          const h = c.height;
          for (let gx = 1; gx <= 8; gx++) {
            for (let gy = 1; gy <= 6; gy++) {
              const px = ctx.getImageData(Math.floor((w * gx) / 9), Math.floor((h * gy) / 7), 1, 1).data;
              if (px[3] > 0 && (px[0] + px[1] + px[2]) > 0) {
                nonZeroSamples += 1;
                distinctColors.add(`${px[0] >> 4}-${px[1] >> 4}-${px[2] >> 4}`);
              }
            }
          }
        } catch (_e) {
          nonZeroSamples = -1;
        }
      }
      return {
        width: c?.width || 0,
        height: c?.height || 0,
        aspect: st.aspect,
        sceneIndex: st.sceneIndex,
        nonZeroSamples,
        distinctColorCount: distinctColors.size,
      };
    });
    assert.equal(initialReelInfo.aspect, '16:9', 'Desktop Motion Reel must default to 16:9');
    assert.equal(initialReelInfo.width, 1280, '16:9 Motion Reel canvas width must be 1280');
    assert.equal(initialReelInfo.height, 720, '16:9 Motion Reel canvas height must be 720');
    // The video follows the viewer's system theme, live (light by default in Playwright).
    const themeInfo = await page.evaluate(() => ({
      theme: window.OpenEstateMotion?.getState?.()?.theme,
      light: document.getElementById('motionReelModal')?.classList?.contains('db-motion-light'),
    }));
    assert.equal(themeInfo.theme, 'light', 'Motion video must follow a light system theme');
    assert.equal(themeInfo.light, true, 'Motion video chrome must use its light variant on a light system');
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.waitForFunction(() => window.OpenEstateMotion?.getState?.()?.theme === 'dark', null, { timeout: 3000 });
    assert.equal(
      await page.evaluate(() => document.getElementById('motionReelModal')?.classList?.contains('db-motion-dark')),
      true,
      'Motion video must switch to dark live when the system does'
    );
    await page.emulateMedia({ colorScheme: 'light' });
    await page.waitForFunction(() => window.OpenEstateMotion?.getState?.()?.theme === 'light', null, { timeout: 3000 });
    const canvasPngBuf = await page.locator('#motionReelCanvas').screenshot();
    assert(canvasPngBuf.length > 4000, `Expected non-empty #motionReelCanvas PNG (>4KB), got ${canvasPngBuf.length}B`);
    if (initialReelInfo.nonZeroSamples >= 0) {
      assert(
        initialReelInfo.nonZeroSamples >= 12 && initialReelInfo.distinctColorCount >= 2,
        `Expected non-empty multi-color #motionReelCanvas pixels, got ${JSON.stringify(initialReelInfo)}`
      );
    }

    // Scene scrub 1 -> 3 (0-indexed 0 -> 2)
    await page.click('#motionScenePill_2');
    await page.waitForTimeout(750);
    const scrubbedScene = await page.evaluate(() => ({
      sceneIndex: window.OpenEstateMotion?.getState?.()?.sceneIndex,
      pillActive: document.getElementById('motionScenePill_2')?.classList?.contains('on') || false,
    }));
    assert.equal(scrubbedScene.sceneIndex, 2, 'Scrubbing to Scene 3 must set sceneIndex=2');
    assert.equal(scrubbedScene.pillActive, true, '#motionScenePill_2 must become active');
    await snapNamed(page, 'desktop_motion_reel.png');

    // Toggle aspect 16:9 -> 9:16 -> 16:9
    await page.click('#motionAspectBtn');
    await page.waitForTimeout(200);
    const toggledPortrait = await page.evaluate(() => {
      const c = document.getElementById('motionReelCanvas');
      return {
        aspect: window.OpenEstateMotion?.getState?.()?.aspect,
        width: c?.width || 0,
        height: c?.height || 0,
      };
    });
    assert.equal(toggledPortrait.aspect, '9:16', 'Aspect toggle must switch 16:9 -> 9:16');
    assert.equal(toggledPortrait.width, 720);
    assert.equal(toggledPortrait.height, 1280);

    await page.click('#motionAspectBtn');
    await page.waitForTimeout(150);
    const toggledBack = await page.evaluate(() => window.OpenEstateMotion?.getState?.()?.aspect);
    assert.equal(toggledBack, '16:9', 'Second aspect toggle must switch 9:16 -> 16:9');

    // Close cleanly
    await page.click('#motionCloseBtn');
    await page.waitForFunction(
      () => Boolean(document.getElementById('motionReelModal')?.hidden),
      null,
      { timeout: 3000 }
    );
    console.log('✓ [12/15] 60fps Property Motion Reel (#motionReelModal, non-empty canvas, Scene 1->3 scrub, 16:9<->9:16 toggle)');

    // =========================================================================
    // 14. Claude-Style Buyer Command Center (#toggleCommandCenterBtn / ?view=dashboard + /artifact)
    // =========================================================================
    await page.locator('#toggleCommandCenterBtn').waitFor({ state: 'visible', timeout: 5000 });
    await page.click('#toggleCommandCenterBtn');
    await page.locator('#buyerCommandCenter').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('#cmdSimPanel').waitFor({ state: 'visible', timeout: 4000 });
    await page.locator('#cmdBentoCards').waitFor({ state: 'visible', timeout: 4000 });
    await page.locator('#artifactStudioPane').waitFor({ state: 'visible', timeout: 4000 });

    const cmdNoOverflow = await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth + 2
    );
    assert(cmdNoOverflow, 'Desktop Buyer Command Center must have zero horizontal overflow');
    assert(
      await page.evaluate(() => new URL(location.href).searchParams.get('view') === 'dashboard'),
      'Activating Command Center must set ?view=dashboard'
    );
    assert(
      await page.evaluate(() => {
        const hdr = document.querySelector('header.hdr');
        const cmdBtn = document.getElementById('toggleCommandCenterBtn');
        const shell = document.getElementById('buyerCommandCenter');
        return (
          document.body.classList.contains('db-cmd-active') &&
          !document.getElementById('verdictHeroGrid')?.offsetParent &&
          Boolean(hdr && cmdBtn && hdr.contains(cmdBtn)) &&
          Boolean(cmdBtn && shell && cmdBtn.getBoundingClientRect().bottom <= shell.getBoundingClientRect().top + 8)
        );
      }),
      'Verdict hero & question cards must be parked and #toggleCommandCenterBtn must stay sticky above #buyerCommandCenter'
    );

    // Real-time Deal & Cashflow slider dynamically updates #simNetMonthly
    const simBefore = (await page.locator('#simNetMonthly').innerText()).trim();
    await page.evaluate(() => {
      const sl = document.getElementById('simDownSlider');
      if (sl) {
        sl.value = sl.value === '45' ? '10' : '45';
        sl.dispatchEvent(new Event('input', { bubbles: true }));
        sl.dispatchEvent(new Event('change', { bubbles: true }));
      }
    });
    await page.waitForFunction(
      (prev) => (document.getElementById('simNetMonthly')?.innerText || '').trim() !== prev,
      simBefore,
      { timeout: 3000 }
    );
    const simAfter = (await page.locator('#simNetMonthly').innerText()).trim();
    assert.notEqual(simAfter, simBefore, 'Adjusting #simDownSlider must dynamically update #simNetMonthly');

    // Verify Fix #9: #simRentOffsetSlider reveals #simCapRateBox even without baseline rentEst, and estVal=null hides #simMetricsRow
    const simKpiGating = await page.evaluate(() => {
      const rep = window.OpenEstateCore?.getRep?.() || window.__REPORT__ || window.R;
      const own = rep?.modules?.valuation?.data?.ownership;
      const est = rep?.modules?.valuation?.data?.estimate;
      const facts = rep?.facts;
      const savedRentEst = own ? own.rentEstimate : undefined;
      const savedEstVal = est ? est.value : undefined;
      const savedZillowAvm = facts?.avms?.zillow ? facts.avms.zillow.value : undefined;
      const savedLastSale = facts?.lastSale ? facts.lastSale.price : undefined;
      const rentSl = document.getElementById('simRentOffsetSlider');

      if (own) own.rentEstimate = null;
      if (rentSl) {
        rentSl.value = '0';
        rentSl.dispatchEvent(new Event('input', { bubbles: true }));
      }
      const capHiddenAtZero = Boolean(document.getElementById('simCapRateBox')?.hidden);
      if (rentSl) {
        rentSl.value = '1400';
        rentSl.dispatchEvent(new Event('input', { bubbles: true }));
      }
      const capBox = document.getElementById('simCapRateBox');
      const capRevealedByOffset = Boolean(capBox && !capBox.hidden && getComputedStyle(capBox).display !== 'none');

      if (est) est.value = null;
      if (facts?.avms?.zillow) facts.avms.zillow.value = null;
      if (facts?.lastSale) facts.lastSale.price = null;
      if (rentSl) rentSl.dispatchEvent(new Event('input', { bubbles: true }));
      const mRow = document.getElementById('simMetricsRow');
      const metricsHiddenWhenNoVal = Boolean(mRow && mRow.hidden && getComputedStyle(mRow).display === 'none');

      if (est && savedEstVal !== undefined) est.value = savedEstVal;
      if (facts?.avms?.zillow && savedZillowAvm !== undefined) facts.avms.zillow.value = savedZillowAvm;
      if (facts?.lastSale && savedLastSale !== undefined) facts.lastSale.price = savedLastSale;
      if (own && savedRentEst !== undefined) own.rentEstimate = savedRentEst;
      if (rentSl) {
        rentSl.value = '0';
        rentSl.dispatchEvent(new Event('input', { bubbles: true }));
      }
      return { capHiddenAtZero, capRevealedByOffset, metricsHiddenWhenNoVal };
    });
    assert.equal(simKpiGating.capHiddenAtZero, true, '#simCapRateBox hides when baseline rentEst is null and rentOffset is 0');
    assert.equal(simKpiGating.capRevealedByOffset, true, 'Dragging #simRentOffsetSlider > 0 reveals #simCapRateBox even without baseline rentEst');
    assert.equal(simKpiGating.metricsHiddenWhenNoVal, true, '#simMetricsRow uses display:none when estVal is null');

    // Trigger POST /api/report/:slug/artifact via preset chip or #artifactGenerateBtn
    await page.fill('#artifactPromptInput', '10-Year Equity P10/P50/P90 vs Renting');
    const [artResp] = await Promise.all([
      page.waitForResponse(
        (r) => r.url().includes(`/api/report/${encodeURIComponent(PRIMARY_SLUG)}/artifact`) && r.request().method() === 'POST',
        { timeout: 20000 }
      ),
      page.click('#artifactGenerateBtn'),
    ]);
    assert.equal(artResp.status(), 200, 'POST /api/report/:slug/artifact must return HTTP 200');
    const artRawText = await artResp.text();
    assertNoStrictRawForbidden(artRawText, 'POST /api/report/:slug/artifact JSON');
    const artBody = JSON.parse(artRawText);
    assert.equal(artBody.ok, true, 'Artifact API response must have ok=true');
    assert(
      Array.isArray(artBody.artifact?.categories) && artBody.artifact.categories.length >= 2,
      'Artifact must include >=2 chart categories'
    );
    await page.waitForFunction(
      () => !document.getElementById('artifactGenerateBtn')?.disabled,
      null,
      { timeout: 5000 }
    );
    const chartBoxBounds = await page.locator('#artifactEchartBox').boundingBox();
    assert(
      chartBoxBounds && chartBoxBounds.width > 180 && chartBoxBounds.height > 150,
      `Expected rendered #artifactEchartBox, got ${JSON.stringify(chartBoxBounds)}`
    );
    const chartCanvasCount = await page.locator('#artifactEchartBox canvas, #artifactEchartBox svg').count();
    assert(chartCanvasCount >= 1, 'Expected ECharts canvas/svg rendered inside #artifactEchartBox');
    await assertNoForbiddenLeaks(page, 'Desktop Buyer Command Center');
    await snapNamed(page, 'desktop_command_center.png');

    // Switch back to Verdict-First Report view cleanly via sticky question jump
    await page.click('#qJump_cost');
    await page.waitForFunction(
      () => Boolean(document.getElementById('buyerCommandCenter')?.hidden),
      null,
      { timeout: 4000 }
    );
    assert(
      await page.locator('#verdictHeroGrid').isVisible(),
      'Exiting Command Center must restore #verdictHeroGrid'
    );

    // Verify direct ?view=dashboard deep-link load
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/?view=dashboard`, { waitUntil: 'domcontentloaded' });
    await page.locator('#buyerCommandCenter').waitFor({ state: 'visible', timeout: 6000 });
    await page.click('#cmdBackToReportBtn');
    await page.waitForFunction(
      () => Boolean(document.getElementById('buyerCommandCenter')?.hidden),
      null,
      { timeout: 4000 }
    );
    console.log('✓ [13/15] Claude-Style Buyer Command Center (?view=dashboard, live cashflow sliders, POST /artifact + ECharts)');

    // =========================================================================
    // 15. Ops Telemetry Control Panel (/ops & GET /api/ops/telemetry)
    // =========================================================================
    const opsApiCheck = await page.evaluate(async () => {
      const r = await fetch('/api/ops/telemetry', { cache: 'no-store' });
      const raw = await r.text();
      return { status: r.status, raw, json: JSON.parse(raw) };
    });
    assert.equal(opsApiCheck.status, 200, 'GET /api/ops/telemetry must return HTTP 200');
    assertNoStrictRawForbidden(opsApiCheck.raw, 'GET /api/ops/telemetry');
    assert.equal(opsApiCheck.json.ok, true, '/api/ops/telemetry must return ok=true');
    assert(opsApiCheck.json.server?.totalRequests >= 1, 'Expected server.totalRequests >= 1 in /api/ops/telemetry');
    assert(Array.isArray(opsApiCheck.json.routes) && opsApiCheck.json.routes.length >= 4, 'Expected route latency buckets');
    assert(Array.isArray(opsApiCheck.json.connectors) && opsApiCheck.json.connectors.length >= 6, 'Expected upstream connector catalog');

    const opsPageResp = await page.goto(`${BASE_URL}/ops`, { waitUntil: 'domcontentloaded' });
    assert.equal(opsPageResp.status(), 200, 'GET /ops must return HTTP 200');
    const opsRawHtml = await opsPageResp.text();
    assertNoStrictRawForbidden(opsRawHtml, 'GET /ops raw HTML');
    await page.locator('#kpiTotalReq').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('#connectorGrid').waitFor({ state: 'visible', timeout: 5000 });
    await page.locator('#routeTableBody').waitFor({ state: 'visible', timeout: 5000 });
    await assertNoForbiddenLeaks(page, 'Ops Dashboard /ops');

    // Verify EN / 中文 / ES language switcher & Refresh Now on /ops
    await page.click('#langSwitch button[data-lang="zh"]');
    await page.waitForTimeout(150);
    await page.click('#langSwitch button[data-lang="es"]');
    await page.waitForTimeout(150);
    await page.click('#langSwitch button[data-lang="en"]');
    await page.click('#refreshNowBtn');
    await page.waitForTimeout(300);

    const opsDesktopNoOverflow = await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth + 2
    );
    assert(opsDesktopNoOverflow, 'Desktop /ops must have zero horizontal overflow');
    await snapNamed(page, 'desktop_ops_dashboard.png');
    console.log('✓ [14/15] Ops Telemetry Control Panel (/ops & /api/ops/telemetry — live metrics, connectors, 0 leaks, 0 overflow)');

    // =========================================================================
    // 16. Multi-Device Responsive Verification (iPad 820x1180 & iPhone 390x844)
    // =========================================================================
    const responsiveDevices = [
      {
        name: 'ipad',
        viewport: { width: 820, height: 1180 },
        deviceScaleFactor: 2,
        isMobile: true,
        hasTouch: true,
        userAgent:
          'Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1',
      },
      {
        name: 'iphone',
        viewport: { width: 390, height: 844 },
        deviceScaleFactor: 3,
        isMobile: true,
        hasTouch: true,
        userAgent:
          'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1',
      },
    ];
    for (const dev of responsiveDevices) {
      const devCtx = await browser.newContext({
        viewport: dev.viewport,
        deviceScaleFactor: dev.deviceScaleFactor,
        isMobile: dev.isMobile,
        hasTouch: dev.hasTouch,
        userAgent: dev.userAgent,
      });
      try {
        const dp = await devCtx.newPage();
        dp.on('pageerror', (err) => pageErrors.push(`[${dev.name}] ${err.message}`));

        // Homepage overflow check
        await dp.goto(`${BASE_URL}/`, { waitUntil: 'domcontentloaded' });
        await dp.locator('#homeForm').waitFor({ state: 'visible', timeout: 8000 });
        const homeNoOverflow = await dp.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth + 2
        );
        assert(homeNoOverflow, `[${dev.name}] Homepage / exceeded viewport width (${dev.viewport.width}px)`);

        // Property Report Verdict-First overview check
        await dp.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
        await dp.locator('#chapterWorkspaceBar').waitFor({ state: 'visible', timeout: 8000 });
        await dp.locator('#verdictFindingLine').waitFor({ state: 'visible', timeout: 5000 });
        const repNoOverflow = await dp.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth + 2
        );
        assert(repNoOverflow, `[${dev.name}] Report overview exceeded viewport width (${dev.viewport.width}px)`);

        if (dev.name === 'iphone') {
          const mobileStack = await dp.evaluate(() => {
            const ov = document.getElementById('overview')?.getBoundingClientRect();
            const media = document.getElementById('verdictMediaCol')?.getBoundingClientRect();
            return { ovTop: ov?.top ?? 9999, mediaTop: media?.top ?? 0 };
          });
          assert(mobileStack.ovTop < mobileStack.mediaTop, '[iphone] Verdict #overview must appear above #verdictMediaCol on mobile');
        }

        // Lot Detail Popover tap-to-open inside viewport bounds
        const mobileLotTrig = dp.locator('#overview .keys [data-lot-hover="1"]').first();
        if ((await mobileLotTrig.count()) > 0) {
          await mobileLotTrig.scrollIntoViewIfNeeded();
          await mobileLotTrig.tap();
          await dp.locator('#lotDetailPopover').waitFor({ state: 'visible', timeout: 3000 });
          const popBox = await dp.locator('#lotDetailPopover').boundingBox();
          assert(
            popBox && popBox.x >= 0 && popBox.x + popBox.width <= dev.viewport.width + 1,
            `[${dev.name}] #lotDetailPopover clipped horizontally: ${JSON.stringify(popBox)}`
          );
          await dp.locator('#overview h1').tap();
          await dp.waitForFunction(() => Boolean(document.getElementById('lotDetailPopover')?.hidden), null, { timeout: 3000 });
        }
        await dp.evaluate(() => window.scrollTo(0, 0));
        await dp.waitForFunction(
          () => [...document.querySelectorAll('#photoBentoGrid img')].every((i) => i.complete && i.naturalWidth > 0),
          null,
          { timeout: 6000 }
        ).catch(() => {});
        await dp.waitForTimeout(150);
        await snapNamed(dp, `${dev.name}_overview.png`);

        // iPad Q3 Price (Value & True Estimate Hub) screenshot + overflow assertion
        if (dev.name === 'ipad') {
          await dp.locator('#qJump_price').tap();
          await dp.locator('#benchmarkValueMount').waitFor({ state: 'visible', timeout: 4000 });
          await dp.locator('#benchmarkValueMount').scrollIntoViewIfNeeded();
          const valNoOverflow = await dp.evaluate(
            () => document.documentElement.scrollWidth <= window.innerWidth + 2
          );
          assert(valNoOverflow, '[ipad] Value & True Estimate Hub exceeded viewport width');
          await snapNamed(dp, 'ipad_value_hub.png');
        }

        // Insurance Quote Studio responsive check + screenshot (inside Q1 Cost card)
        await dp.locator('#qJump_cost').tap();
        await dp.locator('#insuranceQuoteStudio').waitFor({ state: 'visible', timeout: 5000 });
        await dp.locator('#insDeductibleSeg [data-deductible="5000"]').tap();
        await dp.evaluate(() => {
          const studio = document.getElementById('insuranceQuoteStudio');
          if (studio) studio.scrollIntoView({ block: 'start', behavior: 'instant' });
        });
        await dp.waitForTimeout(100);
        const insNoOverflow = await dp.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth + 2
        );
        assert(insNoOverflow, `[${dev.name}] Insurance Studio exceeded viewport width (${dev.viewport.width}px)`);
        if (dev.name === 'iphone') {
          await snapNamed(dp, 'iphone_insurance.png');
        }

        // Walkthrough Slide Deck modal bounds & touch navigation via single sticky bar #stickyWalkthroughBtn
        await dp.evaluate(() => window.scrollTo(0, 0));
        await dp.locator('#stickyWalkthroughBtn').waitFor({ state: 'visible', timeout: 4000 });
        await dp.locator('#stickyWalkthroughBtn').scrollIntoViewIfNeeded();
        await dp.locator('#stickyWalkthroughBtn').tap();
        await dp.locator('#walkthroughModal').waitFor({ state: 'visible', timeout: 4000 });
        const modalCardBox = await dp.locator('#walkthroughModal .wt-dialog, #walkthroughModal > div').first().boundingBox();
        assert(
          modalCardBox &&
            modalCardBox.x >= 0 &&
            modalCardBox.x + modalCardBox.width <= dev.viewport.width + 2 &&
            modalCardBox.height <= dev.viewport.height + 2,
          `[${dev.name}] #walkthroughModal clipped viewport (${JSON.stringify(dev.viewport)}): ${JSON.stringify(modalCardBox)}`
        );
        await dp.locator('#wtNextBtn').tap();
        assert.equal((await dp.locator('#wtStepCounter').innerText()).trim(), '2 / 8', `[${dev.name}] Walkthrough next slide`);
        if (dev.name === 'iphone') {
          await snapNamed(dp, 'iphone_slides_modal.png');
        }
        await dp.locator('#wtCloseBtn').tap();
        await dp.waitForFunction(() => Boolean(document.getElementById('walkthroughModal')?.hidden), null, { timeout: 3000 });

        // Wave 2A: Motion Reel responsive bounds & aspect default (9:16 on iPhone, 16:9 on iPad)
        await dp.locator('#chapterMotionReelBtn').scrollIntoViewIfNeeded();
        await dp.locator('#chapterMotionReelBtn').tap();
        await dp.locator('#motionReelModal').waitFor({ state: 'visible', timeout: 5000 });
        await dp.waitForTimeout(300);
        const devReelState = await dp.evaluate(() => {
          const c = document.getElementById('motionReelCanvas');
          const st = window.OpenEstateMotion?.getState?.() || {};
          return {
            aspect: st.aspect,
            width: c?.width || 0,
            height: c?.height || 0,
            noOverflow: document.documentElement.scrollWidth <= window.innerWidth + 2,
          };
        });
        const expectedAspect = dev.viewport.width <= 640 ? '9:16' : '16:9';
        assert.equal(devReelState.aspect, expectedAspect, `[${dev.name}] Motion Reel default aspect must be ${expectedAspect}`);
        assert(devReelState.noOverflow, `[${dev.name}] Motion Reel exceeded viewport width`);
        const devReelModalBox = await dp.locator('#motionReelModal').boundingBox();
        assert(
          devReelModalBox && devReelModalBox.width <= dev.viewport.width + 2,
          `[${dev.name}] #motionReelModal clipped horizontally: ${JSON.stringify(devReelModalBox)}`
        );
        await dp.locator('#motionScenePill_1').tap();
        await dp.waitForTimeout(750);
        await snapNamed(dp, `${dev.name}_motion_reel.png`);
        await dp.locator('#motionCloseBtn').tap();
        await dp.waitForFunction(() => Boolean(document.getElementById('motionReelModal')?.hidden), null, { timeout: 3000 });

        // Wave 2B: Buyer Command Center responsive check (?view=dashboard)
        await dp.locator('#toggleCommandCenterBtn').scrollIntoViewIfNeeded();
        await dp.locator('#toggleCommandCenterBtn').tap();
        await dp.locator('#buyerCommandCenter').waitFor({ state: 'visible', timeout: 5000 });
        await dp.waitForTimeout(250);
        const devCmdNoOverflow = await dp.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth + 2
        );
        assert(devCmdNoOverflow, `[${dev.name}] Buyer Command Center exceeded viewport width (${dev.viewport.width}px)`);
        await snapNamed(dp, `${dev.name}_command_center.png`);
        await dp.locator('#cmdBackToReportBtn').tap();
        await dp.waitForFunction(() => Boolean(document.getElementById('buyerCommandCenter')?.hidden), null, { timeout: 4000 });

        // Wave 2C: Ops Telemetry Control Panel (/ops) responsive check
        const devOpsResp = await dp.goto(`${BASE_URL}/ops`, { waitUntil: 'domcontentloaded' });
        assert.equal(devOpsResp.status(), 200, `[${dev.name}] GET /ops must return 200`);
        await dp.locator('#kpiTotalReq').waitFor({ state: 'visible', timeout: 5000 });
        await dp.waitForTimeout(250);
        const devOpsNoOverflow = await dp.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth + 2
        );
        assert(devOpsNoOverflow, `[${dev.name}] /ops exceeded viewport width (${dev.viewport.width}px)`);
        await assertNoForbiddenLeaks(dp, `[${dev.name}] /ops`);
        await snapNamed(dp, `${dev.name}_ops_dashboard.png`);
      } finally {
        await devCtx.close();
      }
    }
    console.log('✓ [15/15] Responsive Multi-Device Suite passed (iPad 820x1180 & iPhone 390x844 — Motion Reel, Command Center, & /ops — 0 overflow, 0 clipping)');

    // =========================================================================
    // 17. OS Dark Mode (`colorScheme: 'dark'`) Contrast & Feature Parity Check
    // =========================================================================
    const darkCtx = await browser.newContext({
      colorScheme: 'dark',
      viewport: { width: 1440, height: 900 },
      userAgent: DESKTOP_CHROME_UA,
    });
    try {
      const darkPage = await darkCtx.newPage();
      darkPage.on('pageerror', (err) => pageErrors.push(`[dark] ${err.message}`));
      await darkPage.goto(`${BASE_URL}/r/${PRIMARY_SLUG}/`, { waitUntil: 'domcontentloaded' });
      await darkPage.locator('#densitySwitch').waitFor({ state: 'visible', timeout: 6000 });
      await darkPage.locator('#toggleCommandCenterBtn').waitFor({ state: 'visible', timeout: 6000 });
      const darkCheck = await darkPage.evaluate(() => {
        const cmdBtn = document.getElementById('toggleCommandCenterBtn');
        const cs = cmdBtn ? getComputedStyle(cmdBtn) : null;
        const bodyBg = getComputedStyle(document.body).backgroundColor;
        const roleBrief = document.getElementById('roleFocusBrief');
        return {
          bodyBg,
          cmdColor: cs ? cs.color : '',
          cmdBg: cs ? cs.backgroundColor : '',
          roleBriefVis: Boolean(roleBrief && !roleBrief.hidden && roleBrief.offsetHeight > 40),
        };
      });
      assert.notEqual(darkCheck.bodyBg, 'rgb(245, 246, 248)', `Expected dark report background in prefers-color-scheme: dark, got ${darkCheck.bodyBg}`);
      assert.notEqual(darkCheck.cmdColor, darkCheck.cmdBg, `[dark] #toggleCommandCenterBtn text and background must contrast (${darkCheck.cmdColor} vs ${darkCheck.cmdBg})`);
      assert(darkCheck.roleBriefVis, '[dark] "Complete dossier" #roleFocusBrief must be visible in dark mode');
      await snapNamed(darkPage, 'desktop_dark_overview.png');

      await darkPage.click('#toggleCommandCenterBtn');
      await darkPage.locator('#buyerCommandCenter').waitFor({ state: 'visible', timeout: 5000 });
      await snapNamed(darkPage, 'desktop_dark_command_center.png');
      await darkPage.click('#toggleCommandCenterBtn');
      await darkPage.waitForFunction(() => !document.body.classList.contains('db-cmd-active'), null, { timeout: 3000 });
    } finally {
      await darkCtx.close();
    }
    console.log('✓ [16/16] OS Dark Mode (prefers-color-scheme: dark) contrast, sticky toolbar, & Command Center parity passed');

    assert.equal(pageErrors.length, 0, `Uncaught pageerror(s) during E2E run:\n${pageErrors.join('\n')}`);
    console.log(`\n✅ ALL OPEN_ESTATE PLAYWRIGHT E2E CHECKS PASSED (${BASE_URL}) — 0 page errors!`);
  } finally {
    await browser.close();
  }
}

run().catch((err) => {
  console.error('\n❌ OPEN_ESTATE PLAYWRIGHT E2E FAILED:', err);
  process.exit(1);
});
