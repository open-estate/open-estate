// Role-Based Report Views (`buyer` | `seller` | `agent` | `all`) & Round 1 Visual Polish
// End-to-end integration + Headless Chromium verification against procedural synthetic reports.
// Boots real Axum `openestate` router + SQLite store and drives real Chromium across
// all 4 roles, `?role=` deep-links, keyboard navigation, EN/中文/ES switching, and viewports.

mod common;

use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::sync::seed_reports_from_dir;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use open_estate::web::summary::{backfill_facts_schools, summarize_report};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::TcpListener;
use uuid::Uuid;

const SECONDARY_SLUG: &str = common::SYNTH_SECONDARY_SLUG;
const PRIMARY_SLUG: &str = common::SYNTH_PRIMARY_SLUG;

fn synth_role_report(slug: &str) -> Value {
    let mut rep = common::synth_report_for_slug(slug);
    if let Some(comps) = rep["facts"]["comps"].as_array_mut() {
        for c in comps.iter_mut() {
            c["source"] = json!("County Recorder");
        }
    }
    if let Some(assigned) =
        rep["modules"]["civic"]["data"]["schools"]["assignedOrNearest"].as_array_mut()
    {
        let tiers = [
            json!({ "year": "2024-25", "elaTier": 4, "mathTier": 4, "sciTier": 4 }),
            json!({ "year": "2024-25", "elaTier": 5, "mathTier": 5, "sciTier": 5 }),
            json!({ "year": "2024-25", "elaTier": 4, "mathTier": 4, "sciTier": 4, "cciTier": 5, "gradTier": 4 }),
        ];
        for (idx, s) in assigned.iter_mut().enumerate() {
            s["caDashboard"] = tiers[idx % tiers.len()].clone();
        }
    }
    let civ_clone = rep["modules"]["civic"]["data"].clone();
    let geo_clone = rep["geo"].clone();
    backfill_facts_schools(&mut rep["facts"], Some(&civ_clone), &geo_clone);
    rep["summary"] = summarize_report(&rep);
    normalize_stored_report(&mut rep);
    rep
}

struct LiveTestServer {
    base_url: String,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl LiveTestServer {
    async fn start_seeded() -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-role-views-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        for slug in [SECONDARY_SLUG, PRIMARY_SLUG] {
            let sub = reports_dir.join(slug);
            std::fs::create_dir_all(&sub).expect("create seeded slug dir");
            let rep = synth_role_report(slug);
            std::fs::write(
                sub.join("report.json"),
                serde_json::to_string(&rep).expect("serialize synthetic report"),
            )
            .expect("write synthetic report.json");
        }

        let db_path = temp_dir.join("test.sqlite");
        let env = EnvConfig::from_map(HashMap::from([
            (
                "SQLITE_DB_PATH".to_string(),
                db_path.to_string_lossy().into_owned(),
            ),
            (
                "SQLITE_PATH".to_string(),
                db_path.to_string_lossy().into_owned(),
            ),
            (
                "REPORTS_DIR".to_string(),
                reports_dir.to_string_lossy().into_owned(),
            ),
            ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
        ]));

        let http = HttpClient::offline_loopback_only(Some(temp_dir.join("http_cache")));
        let db = DbStore::connect(&env).await.expect("connect sqlite");
        db.ensure_schema().await.expect("ensure schema");
        seed_reports_from_dir(&db, &reports_dir)
            .await
            .expect("seed synthetic reports");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir,
            None,
        );
        let router = build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        Self {
            base_url,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for LiveTestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

#[test]
fn rendered_html_includes_role_lens_hosts_and_zero_forbidden_strings() {
    assert!(
        !include_str!("../assets/template.html").contains("location.search")
            && !include_str!("../assets/report_workspace.js").contains("location.search"),
        "template.html and report_workspace.js must never use location.search"
    );

    let forbidden = [
        "gpt-4",
        "gpt-5",
        "claude-",
        "fable-",
        "fallbackUsed",
        "aiModel",
        "deterministic-fallback",
    ];

    for slug in [SECONDARY_SLUG, PRIMARY_SLUG] {
        let rep = synth_role_report(slug);
        let html = render_report_html(&rep);

        for required_id in [
            "id=\"roleSwitchHdr\"",
            "id=\"roleSwitchInline\"",
            "id=\"roleFocusBrief\"",
            "id=\"roleHiddenNote\"",
            "id=\"verdictHeroGrid\"",
            "id=\"verdictMediaCol\"",
            "id=\"questionCardsStack\"",
            "id=\"reportTocAside\"",
            "id=\"reportTocReopenBtn\"",
            "id=\"titleLegalRadarPanel\"",
            "id=\"buyerLivingIntelPanel\"",
            "id=\"sellerStrategyStudio\"",
            "id=\"agentDealDesk\"",
        ] {
            assert!(
                html.contains(required_id),
                "[{slug}] rendered HTML missing {required_id}"
            );
        }

        assert!(
            !open_estate::shared::has_retired_brand(&html),
            "[{slug}] rendered HTML leaked retired brand"
        );

        for tok in forbidden {
            assert!(
                !html.contains(tok),
                "[{slug}] rendered HTML leaked forbidden internal string `{tok}`"
            );
        }
    }
}

const ROLE_VIEWS_CHROMIUM_SCRIPT: &str = r#"
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

function resolvePlaywright() {
  const candidates = [
    process.env.PLAYWRIGHT_MODULE_PATH,
    'playwright',
  ].filter(Boolean);
  for (const c of candidates) {
    try {
      return require(c);
    } catch {}
  }
  return null;
}

const playwrightPkg = resolvePlaywright();
if (!playwrightPkg) {
  console.log('SKIP playwright_not_resolvable');
  process.exit(0);
}
const { chromium } = playwrightPkg;

function findChromiumExecutable() {
  if (
    process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH &&
    fs.existsSync(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH)
  ) {
    return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;
  }
  const cacheRoot = path.join(process.env.HOME || '/root', '.cache', 'ms-playwright');
  if (fs.existsSync(cacheRoot)) {
    const dirs = fs
      .readdirSync(cacheRoot)
      .filter((d) => d.startsWith('chromium-'))
      .sort()
      .reverse();
    for (const d of dirs) {
      for (const sub of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
        const candidate = path.join(cacheRoot, d, sub);
        if (fs.existsSync(candidate)) return candidate;
      }
    }
  }
  return undefined;
}

const [, , baseUrl, secondarySlug, primarySlug] = process.argv;

let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath: findChromiumExecutable(),
    args: ['--no-sandbox', '--disable-dev-shm-usage'],
  });
} catch (err) {
  console.log('SKIP chromium_launch_unavailable: ' + String(err?.message || err));
  process.exit(0);
}

const pageErrors = [];
try {
  const ctx = await browser.newContext({
    colorScheme: 'light',
    viewport: { width: 1480, height: 960 },
  });
  const page = await ctx.newPage();
  page.on('pageerror', (err) => pageErrors.push(String(err?.message || err)));
  await page.route('https://**', (r) => r.abort());

  // 1. Default load -> `all` view, zero pageerrors, hero column bottom alignment, A1-A13 fixes
  await page.goto(`${baseUrl}/r/${secondarySlug}`, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(120);

  const initialSnap = await page.evaluate(() => {
    const htmlRole = document.documentElement.getAttribute('data-role');
    const wsRole = window.OpenEstateWorkspace?.getRole?.();
    const hdrVis = (() => {
      const el = document.getElementById('roleSwitchHdr');
      if (!el) return false;
      const r = el.getBoundingClientRect();
      return getComputedStyle(el).display !== 'none' && r.width > 100 && r.height > 20 && r.height < 52;
    })();
    const inlVis = (() => {
      const el = document.getElementById('roleSwitchInline');
      if (!el) return false;
      return getComputedStyle(el).display !== 'none' && el.getBoundingClientRect().height > 0;
    })();
    const leftRect = document.getElementById('overview')?.getBoundingClientRect();
    const rightRect = document.getElementById('verdictMediaCol')?.getBoundingClientRect();
    const chooserCount = document.querySelectorAll('#roleFocusBrief .rb-chooser-row').length;

    // A1: #payBigTitle has no raw <span
    const payBigTitleTxt = document.getElementById('payBigTitle')?.textContent || '';
    // A2: #houseEqStrip has no snake_case enum
    const eqTxt = document.getElementById('houseEqStrip')?.innerText || document.getElementById('houseEqStrip')?.textContent || '';
    // A3: #gMainTag above .leaflet-control-attribution
    const gTagRect = document.getElementById('gMainTag')?.getBoundingClientRect();
    const attrRect = document.querySelector('#gMain .leaflet-control-attribution')?.getBoundingClientRect();
    const legendAboveAttr = (!gTagRect || !attrRect || attrRect.height === 0) ? true : (gTagRect.bottom <= attrRect.top + 2);
    // A7: flags sorted flag/bad -> watch -> clear
    const statuses = Array.from(document.querySelectorAll('#verdictFlagsChecks .vcheck-row')).map((li) =>
      li.classList.contains('flag') || li.classList.contains('bad') ? 0 : li.classList.contains('watch') ? 1 : 2
    );
    const sortedOk = statuses.every((v, i) => i === 0 || v >= statuses[i - 1]);
    // A13: hero school score matches Living card school score
    const heroText = document.getElementById('overview')?.innerText || '';
    const livingText = document.getElementById('qFaceLiving')?.innerText || '';
    const heroMatch = heroText.match(/(\d+(?:\.\d+)?)\/10\s*\n?\s*School score/i);
    const livMatch = livingText.match(/Assigned-school score[:\s]+(\d+(?:\.\d+)?)\/10/i);

    return {
      htmlRole,
      wsRole,
      hdrVis,
      inlVis,
      heroAlignDelta: (leftRect && rightRect) ? Math.abs(leftRect.bottom - rightRect.bottom) : 999,
      chooserCount,
      payBigTitleTxt,
      eqTxt,
      legendAboveAttr,
      sortedOk,
      heroSch: heroMatch ? heroMatch[1] : null,
      livSch: livMatch ? livMatch[1] : null,
    };
  });

  // Toggle drawers one by one (Living -> close -> Potential -> close -> Cost) to verify A4 (zero Core ReferenceError)
  for (const qId of ['living', 'potential', 'cost', 'risk', 'price']) {
    await page.evaluate((id) => {
      window.OpenEstateWorkspace.toggleQuestionCard(id, true);
    }, qId);
    await page.waitForTimeout(40);
    await page.evaluate((id) => {
      window.OpenEstateWorkspace.toggleQuestionCard(id, false);
    }, qId);
    await page.waitForTimeout(20);
  }

  // Re-check payBigTitle after interactive slider change in Cost calculator
  const afterCalcPayTitle = await page.evaluate(() => {
    window.OpenEstateWorkspace.toggleQuestionCard('cost', true);
    const cashBtn = document.querySelector('[data-loan-mode="cash"]');
    if (cashBtn) cashBtn.click();
    const txtCash = document.getElementById('payBigTitle')?.textContent || '';
    const convBtn = document.querySelector('[data-loan-mode="conventional"]');
    if (convBtn) convBtn.click();
    const txtConv = document.getElementById('payBigTitle')?.textContent || '';
    window.OpenEstateWorkspace.toggleQuestionCard('cost', false);
    return { txtCash, txtConv };
  });

  // 2. Test all 4 roles (`buyer`, `seller`, `agent`, `all`) + EN / 中文 / ES
  const roleSnapshots = {};
  const forbiddenRe = /\b(undefined|NaN|null|CA_PROP_19|CA_SB_9|CA_AB_2097|ALLSKY_SFC_SW_DWN|installed_verify_ownership|leased_or_ppa|assessor_and_disclosures_only|prep_before_listing|strong_tax_lock_in|standard_admitted_likely|high_fair_plan_likelihood|live_queried_zero_active_projects|deterministic_zoning_only|action_required|screened_verify_prelim|fcc_urban_tract_tier|non_warrantable_risk|no_litigation_blocker_detected|not_applicable_sfr|permit_and_tot_registration_required|strong_by_right_detached_and_jadu|price_in_or_offer_escrow_credit|gpt-4|gpt-5|claude-|fable-|fallbackUsed|aiModel|deterministic-fallback|zillow|zestimate|redfin|realtor\.com|trulia|movoto|opendoor|homes\.com|walkscore|walk\s+score)\b|<span\s+data-en=|>=|<=/i;

  for (const role of ['buyer', 'seller', 'agent', 'all']) {
    await page.click(`#roleTabHdr_${role}`);
    await page.waitForTimeout(40);

    const snap = await page.evaluate((r) => {
      const cardOrder = Array.from(document.querySelectorAll('#questionCardsStack > .q-card')).map((c) => c.getAttribute('data-q-id'));
      const cardNums = Array.from(document.querySelectorAll('#questionCardsStack > .q-card .q-num')).map((n) => n.textContent.trim());
      const chipOrder = Array.from(document.querySelectorAll('#chapterWorkspaceBar [data-qjump]')).map((b) => b.getAttribute('data-qjump'));
      const chipLabels = Array.from(document.querySelectorAll('#chapterWorkspaceBar [data-qjump] .chap-lbl')).map((s) => s.textContent.trim());
      const tocBranches = Array.from(document.querySelectorAll('#reportTocAside .toc-node'))
        .map((li) => li.id.replace(/^tocNode_/, ''))
        .filter((id) => id !== 'overview');
      const hiddenDomIds = ['rentResearch', 'insuranceQuoteStudio', 'county', 'cre', 'titleLegalRadarPanel', 'buyerLivingIntelPanel', 'sellerStrategyStudio', 'agentDealDesk'].filter((id) =>
        document.getElementById(id)?.classList.contains('role-hidden')
      );
      const briefTiles = Array.from(document.querySelectorAll('#roleFocusBrief .rb-tile')).map((t) => ({
        k: t.querySelector('.rb-tile-k')?.textContent.trim() || '',
        v: t.querySelector('.rb-tile-v')?.textContent.trim() || '',
      }));
      const briefChips = Array.from(document.querySelectorAll('#roleFocusBrief .rb-chip')).map((c) => c.textContent.trim());
      const briefActs = Array.from(document.querySelectorAll('#roleFocusBrief .rb-act')).map((a) => a.textContent.trim());
      const briefFoot = document.querySelector('#roleFocusBrief .rb-foot')?.innerText || '';
      const hiddenNoteTxt = document.getElementById('roleHiddenNote')?.hidden ? '' : (document.getElementById('roleHiddenNote')?.innerText || '');
      const tocNoteTxt = document.querySelector('#reportTocAside .toc-hidden-note')?.innerText || '';
      const urlHasRole = new URL(location.href).searchParams.get('role') === r;
      const storedRole = localStorage.getItem('openestate:role_lens:v1');

      return {
        htmlRole: document.documentElement.getAttribute('data-role'),
        urlHasRole,
        storedRole,
        cardOrder,
        cardNums,
        chipOrder,
        chipLabels,
        tocBranches,
        hiddenDomIds,
        briefTiles,
        briefChips,
        briefActs,
        briefFoot,
        hiddenNoteTxt,
        tocNoteTxt,
      };
    }, role);

    // Check EN / 中文 / ES visible text cleanliness & fengshui rule in this role
    const langChecks = {};
    for (const lang of ['en', 'zh', 'es']) {
      await page.click(`#langSwitch button[data-lang="${lang}"]`);
      await page.waitForTimeout(40);
      langChecks[lang] = await page.evaluate((reSrc) => {
        const re = new RegExp(reSrc, 'i');
        const visText = document.body.innerText || '';
        const badMatch = visText.match(re);
        const fsEl = document.getElementById('fengshui');
        const fsVisible = Boolean(fsEl && !fsEl.hidden && getComputedStyle(fsEl).display !== 'none');
        const tabText = document.querySelector('#roleSwitchHdr [aria-selected="true"]')?.textContent.trim() || '';
        const briefTitle = document.querySelector('#roleFocusBrief .rb-title')?.textContent.trim() || '';
        return {
          badToken: badMatch ? badMatch[0] : null,
          fsVisible,
          tabText,
          briefTitle,
        };
      }, forbiddenRe.source);
    }
    await page.click('#langSwitch button[data-lang="en"]');
    await page.waitForTimeout(30);

    roleSnapshots[role] = { ...snap, langChecks };
  }

  // 2B. Phase 2 Interactive Panels Check (Seller Net-Sheet & Agent CMA Workbench & Radars)
  await page.click('#roleTabHdr_seller');
  await page.evaluate(() => window.OpenEstateWorkspace.toggleQuestionCard('price', true));
  await page.waitForTimeout(40);
  const sellerBefore = await page.evaluate(() => ({
    studioHidden: document.getElementById('sellerStrategyStudio')?.classList.contains('role-hidden'),
    titleRadarLen: (document.getElementById('titleLegalRadarPanel')?.textContent || '').trim().length,
    priceVal: document.getElementById('sellerTargetPriceInput')?.value,
    payoffVal: document.getElementById('sellerLoanPayoffInput')?.value,
    feeVal: document.getElementById('sellerCommissionPctInput')?.value,
    bannerHidden: Boolean(document.getElementById('sellerNetResultBanner')?.hasAttribute('hidden')),
  }));
  await page.fill('#sellerTargetPriceInput', '1800000');
  await page.fill('#sellerLoanPayoffInput', '600000');
  await page.fill('#sellerCommissionPctInput', '4.5');
  await page.waitForTimeout(30);
  const sellerAfter = await page.evaluate(() => ({
    bannerHidden: Boolean(document.getElementById('sellerNetResultBanner')?.hasAttribute('hidden')),
    netValText: document.getElementById('sellerNetProceedsVal')?.textContent?.trim() || '',
  }));

  await page.click('#roleTabHdr_agent');
  await page.evaluate(() => window.OpenEstateWorkspace.toggleQuestionCard('price', true));
  await page.waitForTimeout(40);
  const cmaBefore = await page.evaluate(() => {
    const firstPill = document.querySelector('#agentDealDesk .escrow-card .pill');
    const pillBg = firstPill ? getComputedStyle(firstPill).backgroundColor : '';
    const tocHdrRect = document.querySelector('#reportTocAside .toc-title')?.getBoundingClientRect();
    return {
      deskHidden: document.getElementById('agentDealDesk')?.classList.contains('role-hidden'),
      gateCount: document.querySelectorAll('#agentDealDesk .escrow-card').length,
      chkCount: document.querySelectorAll('#agentDealDesk .agent-cma-chk').length,
      weighted: document.getElementById('agentCmaWeightedVal')?.textContent?.trim() || '',
      activeCount: document.getElementById('agentCmaActiveCount')?.textContent?.trim() || '',
      pillBg,
      tocTitleSingleLine: Boolean(!tocHdrRect || tocHdrRect.height <= 26),
    };
  });
  if (cmaBefore.chkCount >= 2) {
    await page.click('#agentDealDesk .agent-cma-chk >> nth=0');
  }
  await page.evaluate(() => {
    const sl = document.getElementById('agentCmaCondSlider');
    if (sl) {
      sl.value = '5';
      sl.dispatchEvent(new Event('input', { bubbles: true }));
    }
  });
  await page.waitForTimeout(30);
  const cmaAfter = await page.evaluate(() => ({
    weighted: document.getElementById('agentCmaWeightedVal')?.textContent?.trim() || '',
    activeCount: document.getElementById('agentCmaActiveCount')?.textContent?.trim() || '',
  }));
  await page.evaluate(() => window.OpenEstateWorkspace.toggleQuestionCard('price', false));

  // 3. Test jump-to-hidden (`county` while in `buyer` view -> switches to `all` first)
  await page.click('#roleTabHdr_buyer');
  await page.waitForTimeout(30);
  const jumpHiddenResult = await page.evaluate(() => {
    const beforeRole = window.OpenEstateWorkspace.getRole();
    const countyHiddenBefore = document.getElementById('county')?.classList.contains('role-hidden');
    window.OpenEstateWorkspace.jumpToSection('county', false);
    const afterRole = window.OpenEstateWorkspace.getRole();
    const countyHiddenAfter = document.getElementById('county')?.classList.contains('role-hidden');
    const riskOpen = document.getElementById('qCardRisk')?.classList.contains('open');
    return { beforeRole, countyHiddenBefore, afterRole, countyHiddenAfter, riskOpen };
  });

  // 4. Test "Show All Info" button from Seller view
  await page.click('#roleTabHdr_seller');
  await page.waitForTimeout(30);
  await page.click('#roleHiddenNote [data-role-set="all"]');
  await page.waitForTimeout(30);
  const afterShowAllRole = await page.evaluate(() => window.OpenEstateWorkspace.getRole());

  // 5. Test `?role=seller` URL deep-link + reload on second synthetic report (`primary`) + keyboard ArrowRight
  await page.goto(`${baseUrl}/r/${primarySlug}?role=seller`, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(80);
  const deepLinkSeller = await page.evaluate(() => ({
    role: window.OpenEstateWorkspace.getRole(),
    htmlRole: document.documentElement.getAttribute('data-role'),
    firstCard: document.querySelector('#questionCardsStack > .q-card')?.id,
    secondCard: document.querySelectorAll('#questionCardsStack > .q-card')[1]?.id,
  }));
  await page.focus('#roleTabHdr_seller');
  await page.keyboard.press('ArrowRight');
  await page.waitForTimeout(30);
  const afterKeyRightRole = await page.evaluate(() => window.OpenEstateWorkspace.getRole());
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(80);
  const afterReloadRole = await page.evaluate(() => window.OpenEstateWorkspace.getRole());

  // 6. Test 1280x800 and 1120x800 header fit + hero stretch, and 390x844 mobile ergonomics
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.waitForTimeout(50);
  const fit1280 = await page.evaluate(() => {
    const hdrSw = document.getElementById('roleSwitchHdr')?.getBoundingClientRect();
    const inlSw = document.getElementById('roleSwitchInline');
    const leftR = document.getElementById('overview')?.getBoundingClientRect();
    const rightR = document.getElementById('verdictMediaCol')?.getBoundingClientRect();
    return {
      hdrVisible: Boolean(hdrSw && hdrSw.width > 100 && hdrSw.height < 50),
      inlHidden: !inlSw || getComputedStyle(inlSw).display === 'none',
      heroDelta: (leftR && rightR) ? Math.abs(leftR.bottom - rightRect_or(leftR, rightR)) : 999,
    };
    function rightRect_or(_l, r) { return r.bottom; }
  });

  await page.setViewportSize({ width: 1120, height: 800 });
  await page.waitForTimeout(50);
  const fit1120 = await page.evaluate(() => {
    const hdrSw = document.getElementById('roleSwitchHdr')?.getBoundingClientRect();
    const inlSw = document.getElementById('roleSwitchInline');
    return {
      hdrVisible: Boolean(hdrSw && hdrSw.width > 100 && hdrSw.height < 50),
      inlHidden: !inlSw || getComputedStyle(inlSw).display === 'none',
    };
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(60);
  const mobileSnap = await page.evaluate(() => {
    const hdrH = document.querySelector('.hdr')?.getBoundingClientRect().height || 999;
    const hdrSw = document.getElementById('roleSwitchHdr');
    const inlSw = document.getElementById('roleSwitchInline')?.getBoundingClientRect();
    const reopenR = document.getElementById('reportTocReopenBtn')?.getBoundingClientRect();
    const horizOverflow = document.documentElement.scrollWidth > document.documentElement.clientWidth + 1;
    return {
      hdrH,
      hdrHidden: !hdrSw || getComputedStyle(hdrSw).display === 'none',
      inlFullWidth: Boolean(inlSw && inlSw.width >= 340 && inlSw.height >= 38),
      reopenCircleOk: Boolean(reopenR && reopenR.width <= 42 && reopenR.height <= 42),
      horizOverflow,
    };
  });

  console.log('ROLE_TEST_JSON ' + JSON.stringify({
    pageErrors,
    initialSnap,
    afterCalcPayTitle,
    roleSnapshots,
    sellerBefore,
    sellerAfter,
    cmaBefore,
    cmaAfter,
    jumpHiddenResult,
    afterShowAllRole,
    deepLinkSeller,
    afterKeyRightRole,
    afterReloadRole,
    fit1280,
    fit1120,
    mobileSnap,
  }));

  await ctx.close();
} finally {
  await browser.close();
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_views_switching_deep_linking_trilingual_and_polish_in_chromium() {
    let srv = LiveTestServer::start_seeded().await;

    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return;
    }

    let script_dir = std::env::temp_dir().join(format!("openestate-role-pw-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&script_dir).unwrap();
    let script_path = script_dir.join("check_roles.mjs");
    std::fs::write(&script_path, ROLE_VIEWS_CHROMIUM_SCRIPT).unwrap();

    let out = tokio::process::Command::new("node")
        .arg(&script_path)
        .arg(&srv.base_url)
        .arg(SECONDARY_SLUG)
        .arg(PRIMARY_SLUG)
        .output()
        .await
        .expect("execute node chromium role test");
    let _ = std::fs::remove_dir_all(&script_dir);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Chromium role views check failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    if stdout.lines().any(|l| l.starts_with("SKIP ")) {
        eprintln!("Skipping Chromium assertions: {stdout}");
        return;
    }

    let json_line = stdout
        .lines()
        .find(|l| l.starts_with("ROLE_TEST_JSON "))
        .unwrap_or_else(|| panic!("Missing ROLE_TEST_JSON output:\n{stdout}\n{stderr}"));
    let res: Value = serde_json::from_str(&json_line["ROLE_TEST_JSON ".len()..]).unwrap();

    // A4: Zero uncaught page errors across drawer toggles
    assert_eq!(
        res["pageErrors"],
        json!([]),
        "Expected zero pageerror events (including Core is not defined)"
    );

    // Initial default state (`all`) + A1, A2, A3, A5, A7, A13
    let init = &res["initialSnap"];
    assert_eq!(init["htmlRole"], json!("all"));
    assert_eq!(init["wsRole"], json!("all"));
    assert_eq!(init["hdrVis"], json!(true));
    assert_eq!(init["inlVis"], json!(false));
    assert!(
        init["heroAlignDelta"].as_f64().unwrap_or(999.0) <= 3.0,
        "Hero columns should align at bottom at 1480w, got delta={}",
        init["heroAlignDelta"]
    );
    assert_eq!(
        init["chooserCount"],
        json!(3),
        "All Info brief should display 3-row lens chooser"
    );
    assert!(
        !init["payBigTitleTxt"]
            .as_str()
            .unwrap_or("")
            .contains("<span"),
        "A1: #payBigTitle must not print raw <span markup: {}",
        init["payBigTitleTxt"]
    );
    assert!(
        !res["afterCalcPayTitle"]["txtCash"]
            .as_str()
            .unwrap_or("")
            .contains("<span")
            && !res["afterCalcPayTitle"]["txtConv"]
                .as_str()
                .unwrap_or("")
                .contains("<span"),
        "A1: #payBigTitle after loan-mode toggle must not contain raw <span: {:?}",
        res["afterCalcPayTitle"]
    );
    assert!(
        !init["eqTxt"]
            .as_str()
            .unwrap_or("")
            .contains("installed_verify"),
        "A2: #houseEqStrip must never print raw snake_case enum: {}",
        init["eqTxt"]
    );
    assert_eq!(
        init["legendAboveAttr"],
        json!(true),
        "A3: Hero footprint legend must sit above Leaflet attribution"
    );
    assert_eq!(
        init["sortedOk"],
        json!(true),
        "A7: Flags & checks must be sorted flag -> watch -> clear"
    );
    assert_eq!(
        init["heroSch"], init["livSch"],
        "A13: Hero school score and Living card composite must match"
    );

    // Verify all 4 roles (`buyer`, `seller`, `agent`, `all`)
    let snaps = &res["roleSnapshots"];
    let expected_orders = [
        (
            "buyer",
            vec!["price", "cost", "risk", "living", "potential"],
            5usize,
        ),
        (
            "seller",
            vec!["price", "potential", "risk", "living", "cost"],
            5usize,
        ),
        (
            "agent",
            vec!["price", "cost", "risk", "potential", "living"],
            0usize,
        ),
        (
            "all",
            vec!["cost", "risk", "price", "living", "potential"],
            0usize,
        ),
    ];

    for (role, exp_order, exp_hidden) in expected_orders {
        let s = &snaps[role];
        assert_eq!(s["htmlRole"], json!(role), "[{role}] html[data-role]");
        assert_eq!(s["urlHasRole"], json!(true), "[{role}] ?role= synced");
        assert_eq!(s["storedRole"], json!(role), "[{role}] localStorage synced");
        assert_eq!(
            s["cardOrder"],
            json!(exp_order),
            "[{role}] card order mismatch"
        );
        assert_eq!(
            s["chipOrder"],
            json!(exp_order),
            "[{role}] chapter chip order mismatch"
        );
        assert_eq!(
            s["tocBranches"],
            json!(exp_order),
            "[{role}] TOC branch order mismatch"
        );
        assert_eq!(
            s["cardNums"],
            json!(["1", "2", "3", "4", "5"]),
            "[{role}] card 1-5 numbering"
        );
        assert_eq!(
            s["hiddenDomIds"].as_array().unwrap().len(),
            exp_hidden,
            "[{role}] hidden section count mismatch: {:?}",
            s["hiddenDomIds"]
        );

        if role != "all" {
            assert_eq!(
                s["briefTiles"].as_array().unwrap().len(),
                4,
                "[{role}] expected 4 grounded tiles in Role Focus Brief"
            );
            assert!(
                !s["briefChips"].as_array().unwrap().is_empty(),
                "[{role}] expected grounded jump chips in Role Focus Brief"
            );
        }
        if exp_hidden > 0 {
            let needle = format!("+{exp_hidden}");
            assert!(
                s["hiddenNoteTxt"].as_str().unwrap_or("").contains(&needle)
                    && s["briefFoot"].as_str().unwrap_or("").contains(&needle)
                    && s["tocNoteTxt"].as_str().unwrap_or("").contains(&needle),
                "[{role}] all 3 hidden-section notes must report +{exp_hidden}: {:?}",
                s
            );
        }
        if role == "seller" {
            let acts = s["briefActs"].as_array().unwrap();
            assert!(
                acts.iter()
                    .any(|a| a.as_str().unwrap_or("").contains("Open net sheet")),
                "[seller] expected 'Open net sheet' button in brief actions: {:?}",
                acts
            );
        }
        if role == "agent" {
            assert_eq!(
                s["briefActs"].as_array().unwrap().len(),
                4,
                "[agent] expected 4 presentation action buttons"
            );
        }

        // Trilingual checks (EN, 中文, ES)
        for lang in ["en", "zh", "es"] {
            let lc = &s["langChecks"][lang];
            assert!(
                lc["badToken"].is_null(),
                "[{role}/{lang}] forbidden or broken token in visible text: {:?}",
                lc
            );
            assert_eq!(
                lc["fsVisible"],
                json!(lang == "zh"),
                "[{role}/{lang}] #fengshui must be visible iff lang == zh"
            );
            assert!(
                !lc["tabText"].as_str().unwrap_or("").is_empty()
                    && !lc["briefTitle"].as_str().unwrap_or("").is_empty(),
                "[{role}/{lang}] localized role tab & brief title must be non-empty"
            );
        }
    }

    // Phase 2 Interactive Panels (Seller Net-Sheet & Agent CMA Workbench & Radars)
    assert_eq!(res["sellerBefore"]["studioHidden"], json!(false));
    assert!(res["sellerBefore"]["titleRadarLen"].as_u64().unwrap_or(0) > 40);
    assert_eq!(res["sellerBefore"]["priceVal"], json!(""));
    assert_eq!(res["sellerBefore"]["payoffVal"], json!(""));
    assert_eq!(res["sellerBefore"]["feeVal"], json!(""));
    assert_eq!(res["sellerBefore"]["bannerHidden"], json!(true));
    assert_eq!(res["sellerAfter"]["bannerHidden"], json!(false));
    assert!(
        res["sellerAfter"]["netValText"]
            .as_str()
            .unwrap_or("")
            .starts_with('$'),
        "Seller Net Proceeds should reveal currency value after user input: {:?}",
        res["sellerAfter"]
    );

    assert_eq!(res["cmaBefore"]["deskHidden"], json!(false));
    assert_eq!(res["cmaBefore"]["gateCount"], json!(8));
    assert_ne!(
        res["cmaBefore"]["pillBg"],
        json!("rgba(0, 0, 0, 0)"),
        "Agent gate status pills must have colored background (not transparent/bare bold)"
    );
    assert_eq!(
        res["cmaBefore"]["tocTitleSingleLine"],
        json!(true),
        "TOC header title + role badge must stay single-line"
    );
    assert_ne!(
        res["cmaBefore"]["weighted"], res["cmaAfter"]["weighted"],
        "Agent CMA Weighted value must recalculate dynamically: before={:?} after={:?}",
        res["cmaBefore"], res["cmaAfter"]
    );
    if res["cmaBefore"]["chkCount"].as_u64().unwrap_or(0) >= 2 {
        assert_ne!(
            res["cmaBefore"]["activeCount"],
            res["cmaAfter"]["activeCount"]
        );
    }

    // Jump-to-hidden switches role to `all` first
    assert_eq!(res["jumpHiddenResult"]["beforeRole"], json!("buyer"));
    assert_eq!(res["jumpHiddenResult"]["countyHiddenBefore"], json!(true));
    assert_eq!(res["jumpHiddenResult"]["afterRole"], json!("all"));
    assert_eq!(res["jumpHiddenResult"]["countyHiddenAfter"], json!(false));
    assert_eq!(res["jumpHiddenResult"]["riskOpen"], json!(true));

    // Show All Info button restores `all`
    assert_eq!(res["afterShowAllRole"], json!("all"));

    // Deep-link `?role=seller`, ArrowRight -> `agent`, reload keeps `agent`
    assert_eq!(res["deepLinkSeller"]["role"], json!("seller"));
    assert_eq!(res["deepLinkSeller"]["firstCard"], json!("qCardPrice"));
    assert_eq!(
        res["deepLinkSeller"]["secondCard"],
        json!("qCardPotential")
    );
    assert_eq!(res["afterKeyRightRole"], json!("agent"));
    assert_eq!(res["afterReloadRole"], json!("agent"));

    // Responsive fit at 1280, 1120, and 390 mobile
    assert_eq!(res["fit1280"]["hdrVisible"], json!(true));
    assert_eq!(res["fit1280"]["inlHidden"], json!(true));
    assert!(
        res["fit1280"]["heroDelta"].as_f64().unwrap_or(999.0) <= 3.0,
        "Hero columns must align at 1280w, got delta={}",
        res["fit1280"]["heroDelta"]
    );
    assert_eq!(res["fit1120"]["hdrVisible"], json!(true));
    assert_eq!(res["fit1120"]["inlHidden"], json!(true));

    assert!(
        res["mobileSnap"]["hdrH"].as_f64().unwrap_or(999.0) <= 92.0,
        "A11: Mobile sticky header height must be <= 92px, got {}",
        res["mobileSnap"]["hdrH"]
    );
    assert_eq!(res["mobileSnap"]["hdrHidden"], json!(true));
    assert_eq!(res["mobileSnap"]["inlFullWidth"], json!(true));
    assert_eq!(res["mobileSnap"]["reopenCircleOk"], json!(true));
    assert_eq!(res["mobileSnap"]["horizOverflow"], json!(false));
}
