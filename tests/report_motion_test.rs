// Regression tests for the report's motion video (`assets/report_motion.js`, opened from
// "Watch 50s Video Tour" / "Play as Motion Video"):
// 1. Page & theme rules (`AGENTS.md`): the script is inlined into every report page, carries zero
//    forbidden internal tokens or retired brands, preserves required element IDs, follows the
//    viewer's OS `prefers-color-scheme` with a live `matchMedia` listener, uses a frosted light
//    wash backdrop in light mode, and keeps `#walkthroughModal` matched to the light report page.
// 2. Under real Headless Chromium (via Playwright, resolved just like `e2e/playwright_e2e.mjs`),
//    on the procedural synthetic reports (`200 Synthparcelbeta Ave, Campbell, CA 95008` and
//    `100 Synthparcelalpha Ln, San Jose, CA 95118`): every frame of both aspect ratios renders cleanly
//    on a real HTML5 `<canvas>`, every on-screen number comes from the report (cost segments add
//    up to its monthly total, benchmarks are the report's own method values, outlines and aerial
//    imagery share one Web-Mercator projection), and imagery stays within USGS The National Map.
// 3. Light and dark looks follow `browser.newContext({ colorScheme })` and react live to
//    `page.emulateMedia({ colorScheme })`.
// 4. A sparse parcel loaded from `common::synth_holdout_by_id` leaves honest gaps
//    instead of inventing numbers.

mod common;

use open_estate::shared::slugify;
use open_estate::web::render::{render_report_html, REPORT_MOTION_JS};
use serde_json::{json, Value};
use uuid::Uuid;

const SJ: &str = common::SYNTH_PRIMARY_SLUG;
const CAMPBELL: &str = common::SYNTH_SECONDARY_SLUG;

fn fixture(slug: &str) -> Value {
    common::synth_report_for_slug(slug)
}

fn holdout_home_by_id(id: &str) -> Value {
    common::synth_holdout_by_id(id)
}

fn module<'a>(report: &'a Value, key: &str) -> &'a Value {
    let m = &report["modules"][key]["data"];
    if m.is_null() {
        &report[key]
    } else {
        m
    }
}

/// Real Headless Chromium Playwright runner on rendered report HTML and `<canvas>`.
const CHROMIUM_MOTION_RUNNER_MJS: &str = r#"
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
    console.log('SKIP playwright_not_resolvable');
    process.exit(0);
  }
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

const [, , htmlPath, lang, theme] = process.argv;
const html = fs.readFileSync(htmlPath, 'utf8');

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

try {
  const context = await browser.newContext({
    colorScheme: theme === 'dark' ? 'dark' : 'light',
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 1,
  });
  const page = await context.newPage();

  const imageUrls = [];
  await page.route('https://**', async (route) => {
    const url = route.request().url();
    if (
      url.includes('USGSImageryOnly/MapServer') ||
      url.includes('DPW_ImageryCached2025/MapServer') ||
      url.includes('KingCo_Aerial_2023/MapServer')
    ) {
      imageUrls.push(url);
      try {
        await route.continue();
      } catch {
        await route.abort();
      }
      return;
    }
    await route.abort();
  });

  await page.addInitScript((initLang) => {
    window.__dbMotionTexts = [];
    window.__dbMotionWarnings = [];
    window.__dbMotionDrawImages = 0;

    const origWarn = console.warn.bind(console);
    console.warn = (...args) => {
      window.__dbMotionWarnings.push(args.map(String).join(' '));
      origWarn(...args);
    };

    const proto = CanvasRenderingContext2D.prototype;
    const origFillText = proto.fillText;
    proto.fillText = function (s, x, y, ...rest) {
      if (typeof s !== 'string' || !Number.isFinite(x) || !Number.isFinite(y)) {
        window.__dbMotionWarnings.push('bad fillText ' + JSON.stringify([s, x, y]));
      }
      window.__dbMotionTexts.push(String(s));
      return origFillText.call(this, s, x, y, ...rest);
    };
    const origDrawImage = proto.drawImage;
    proto.drawImage = function (img, ...coords) {
      if (!coords.every(Number.isFinite)) {
        window.__dbMotionWarnings.push('bad drawImage ' + coords.join(','));
      }
      window.__dbMotionDrawImages += 1;
      return origDrawImage.call(this, img, ...coords);
    };

    document.addEventListener('DOMContentLoaded', () => {
      if (initLang && document.documentElement) {
        document.documentElement.lang = initLang;
      }
    });
  }, lang || 'en');

  await page.goto('about:blank');
  await page.setContent(html, { waitUntil: 'domcontentloaded' });
  if (lang && lang !== 'en') {
    await page.evaluate((l) => {
      document.documentElement.lang = l;
      if (window.OpenEstateCore && typeof window.OpenEstateCore.setLang === 'function') {
        window.OpenEstateCore.setLang(l);
      }
    }, lang);
  }

  const out = await page.evaluate(async (initialTheme) => {
    const M = window.OpenEstateMotion;
    const I = M._internals;
    const d = I.extractReelData();
    const res = {
      duration: M.duration,
      scenes: I.scenes.map((s) => s.key),
      hasPoint: d.hasPoint,
      geo: {
        lot: !!d.geo.lot,
        house: !!d.geo.house,
        acc: d.geo.acc.length,
        extentM: d.geo.extentM,
      },
      facts: d.facts,
      value:
        d.value && {
          est: d.value.est,
          low: d.value.low,
          high: d.value.high,
          refs: d.value.refs.map((r) => r.value),
        },
      cost:
        d.cost && {
          total: d.cost.total,
          parts: d.cost.parts.map((p) => p.value),
          quake: d.cost.quake,
          rent: d.cost.rent,
        },
      tiles: d.tiles.map((t) => t.key),
      pois: d.pois.map((p) => p.dist),
      verdict: d.verdict,
      checks: d.checks,
      captions: d.captions,
      theme: M.getState().theme,
      layers: d.layers.map((l) => l.url),
    };

    if (d.hasPoint) {
      const f = d.geo.focus;
      const cam = I.mkCam(d, f[0], f[1], 40, 0.5, 0.5);
      const P = I.projector(cam, 1280, 720);
      const c = P.ll(f[0], f[1]);
      const north = P.ll(f[0] + 40 / 110574, f[1]);
      const viaMerc = [P.mx(I.mercX(f[1] + 0.0003)), P.my(I.mercY(f[0] + 0.0002))];
      const viaLl = P.ll(f[0] + 0.0002, f[1] + 0.0003);
      res.projection = {
        centre: c,
        northPx: c[1] - north[1],
        sameTransform: Math.hypot(viaMerc[0] - viaLl[0], viaMerc[1] - viaLl[1]) < 1e-6,
      };
      const plan = I.planTiles(d, false, 1);
      res.plan = {
        n: plan.keys.length,
        opening: plan.opening,
        maxZ: Math.max(...plan.keys.map((k) => k[0])),
        minZ: Math.min(...plan.keys.map((k) => k[0])),
      };
    }

    M.open(0);
    await new Promise((r) => setTimeout(r, 80));

    let minTexts = Infinity;
    let frames = 0;
    for (const aspect of ['16:9', '9:16']) {
      M.setAspect(aspect);
      for (let t = 0; t < M.duration; t += 0.5) {
        const before = window.__dbMotionTexts.length;
        M.seekTime(t);
        frames++;
        minTexts = Math.min(minTexts, window.__dbMotionTexts.length - before);
      }
    }

    const modalEl = document.getElementById('motionReelModal');
    const modalHasLightClass = !!(modalEl && modalEl.classList.contains('db-motion-light'));

    res.render = {
      frames,
      minTexts,
      drawImages: window.__dbMotionDrawImages,
      warnings: window.__dbMotionWarnings.slice(0, 5),
      warningCount: window.__dbMotionWarnings.length,
      theme: M.getState().theme,
      modalHasLightClass,
    };
    res.pinned = [
      M.setTheme(initialTheme === 'dark' ? 'light' : 'dark'),
      M.setTheme(null),
    ];
    res.texts = Array.from(new Set(window.__dbMotionTexts));
    M.setAspect('16:9');
    return res;
  }, theme);

  // Verify live OS color-scheme change reactivity (`AGENTS.md`).
  const flippedScheme = theme === 'dark' ? 'light' : 'dark';
  await page.emulateMedia({ colorScheme: flippedScheme });
  const liveThemeAfterFlip = await page.evaluate(() => {
    const M = window.OpenEstateMotion;
    const modalEl = document.getElementById('motionReelModal');
    const wtEl = document.getElementById('walkthroughModal');
    const wtBg = wtEl ? getComputedStyle(wtEl).backgroundColor : '';
    const state = {
      theme: M.getState().theme,
      modalLightClass: !!(modalEl && modalEl.classList.contains('db-motion-light')),
      wtBg,
    };
    M.close();
    return state;
  });

  out.liveThemeAfterFlip = liveThemeAfterFlip;
  out.imageHosts = Array.from(
    new Set(
      imageUrls.map((u) =>
        u.replace(/^(https:\/\/[^/]+\/[^?]*?)(\/tile\/.*|\?.*)$/, '$1')
      )
    )
  );
  out.tileUrlsOk = imageUrls
    .filter((u) => /\/tile\//.test(u))
    .every(
      (u) =>
        /\/tile\/(\d+)\/(\d+)\/(\d+)$/.test(u) && +/\/tile\/(\d+)\//.exec(u)[1] <= 19
    );

  console.log('MOTION ' + JSON.stringify(out));
  await context.close();
} finally {
  await browser.close();
}
"#;

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_motion_in_chromium(report: &Value, lang: &str) -> Option<Value> {
    run_motion_in_chromium_with(report, lang, "light")
}

fn run_motion_in_chromium_with(report: &Value, lang: &str, theme: &str) -> Option<Value> {
    if !node_available() {
        eprintln!("node is not installed; skipping Chromium motion-video checks");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("openestate-motion-chromium-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let runner_path = dir.join("runner.mjs");
    let html_path = dir.join("report.html");
    let rendered_html = render_report_html(report);
    std::fs::write(&runner_path, CHROMIUM_MOTION_RUNNER_MJS).unwrap();
    std::fs::write(&html_path, rendered_html).unwrap();

    let out = std::process::Command::new("node")
        .arg(&runner_path)
        .arg(&html_path)
        .arg(lang)
        .arg(theme)
        .output()
        .expect("run node playwright runner");
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Chromium motion runner failed: {stdout}\n{stderr}"
    );
    if let Some(skip_line) = stdout.lines().find(|l| l.starts_with("SKIP ")) {
        eprintln!("skipping headless Chromium motion check ({skip_line})");
        return None;
    }
    let line = stdout
        .lines()
        .find(|l| l.starts_with("MOTION "))
        .unwrap_or_else(|| panic!("no MOTION line in: {stdout}\n{stderr}"));
    Some(serde_json::from_str(&line["MOTION ".len()..]).expect("runner JSON"))
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

#[test]
fn motion_script_obeys_page_and_theme_rules() {
    let js = REPORT_MOTION_JS;
    for tok in [
        "provider",
        "effort",
        "fallbackUsed",
        "deterministic-fallback",
        "aiModel",
        "gpt-",
        "OPENAI_API_KEY",
        "rust-sqlx-sqlite",
        "</script",
        "<script",
        "location.search",
    ] {
        assert!(
            !js.contains(tok),
            "assets/report_motion.js must not contain `{tok}`"
        );
    }
    assert!(!open_estate::shared::has_retired_brand(js));
    for id in [
        "motionReelModal",
        "motionReelCanvas",
        "motionReelStage",
        "openMotionReelBtn",
        "chapterMotionReelBtn",
        "bannerMotionReelBtn",
        "wtMotionReelBtn",
        "motionScenePill_",
        "motionSegFill_",
        "motionAspectBtn",
        "motionCloseBtn",
        "motionPlayBtn",
        "motionAudioBtn",
        "motionExportBtn",
        "dbMotionStyles",
    ] {
        assert!(
            js.contains(id),
            "the motion video keeps the `{id}` element id"
        );
    }
    assert!(
        !js.contains("speechSynthesis"),
        "voiceover has been removed"
    );
    assert!(
        !js.contains("motionVoiceBtn"),
        "voice toggle button has been removed"
    );
    assert!(js.contains("USGSImageryOnly/MapServer/tile/"));
    assert!(js.contains("DPW_ImageryCached2025/MapServer/tile/"));
    assert!(js.contains("KingCo_Aerial_2023/MapServer/tile/"));
    assert!(js.contains("USGS The National Map"));
    assert!(js.contains("resolveMotionSatellite"));
    assert!(
        js.contains("crossOrigin = 'anonymous'"),
        "images stay CORS-clean so the canvas can be exported"
    );

    // Standing theme rules (`AGENTS.md`): OS `prefers-color-scheme` + live change listener,
    // frosted light wash backdrop in light mode, and `#walkthroughModal` staying light.
    assert!(
        js.contains("prefers-color-scheme: dark"),
        "reads the system color scheme"
    );
    assert!(
        js.contains("addEventListener('change'"),
        "reacts live to OS color-scheme changes"
    );
    assert!(
        js.contains(".db-motion-light .db-motion-dialog"),
        "modal chrome provides a real light design"
    );
    assert!(
        js.contains(".db-motion-bg.db-motion-light{background:rgba(241,245,249"),
        "light video modal uses a frosted light wash backdrop"
    );
    assert!(
        !js.contains("@media (prefers-color-scheme: dark){.db-wt-motion-btn"),
        "walkthrough deck button stays matched to the light deck"
    );

    for slug in [CAMPBELL, SJ] {
        let report = fixture(slug);
        let html = render_report_html(&report);
        assert!(!html.contains("__REPORT_MOTION_JS__"));
        assert!(html.contains("window.OpenEstateMotion = {"));
        assert!(
            html.contains(
                ".wt-modal-bg{position:fixed;inset:0;z-index:3500;background:rgba(241,245,249"
            ),
            "[{slug}] walkthrough modal uses a frosted light wash backdrop"
        );
        assert!(
            !html.contains("@media (prefers-color-scheme: dark){\n.wt-modal-bg"),
            "[{slug}] walkthrough modal stays matched to the light report page"
        );
    }
}

#[test]
fn every_frame_and_number_comes_from_the_report() {
    for slug in [CAMPBELL, SJ] {
        let report = fixture(slug);
        let Some(out) = run_motion_in_chromium(&report, "en") else {
            return;
        };
        let ctx = format!("[{slug}]");

        assert_eq!(out["duration"], json!(50), "{ctx} 50-second cut");
        assert_eq!(
            out["scenes"],
            json!(["arrival", "lot", "value", "cost", "risk", "verdict"]),
            "{ctx}"
        );
        let render = &out["render"];
        assert_eq!(
            render["warningCount"],
            json!(0),
            "{ctx} frames render cleanly: {render}"
        );
        assert_eq!(
            render["frames"],
            json!(200),
            "{ctx} every half second, both aspect ratios"
        );
        assert!(
            render["minTexts"].as_u64().unwrap() >= 3,
            "{ctx} every frame carries text: {render}"
        );

        // Value: the report's estimate, and benchmarks that are the report's own method values.
        let val = module(&report, "valuation");
        assert_eq!(
            f(&out["value"]["est"]),
            f(&val["estimate"]["value"]),
            "{ctx} estimate"
        );
        let mut known: Vec<f64> = val["methods"]
            .as_array()
            .or(val["estimate"]["methods"].as_array())
            .map(|m| m.iter().map(|x| f(&x["value"])).collect())
            .unwrap_or_default();
        known.push(f(&report["facts"]["lastSale"]["price"]));
        for r in out["value"]["refs"].as_array().unwrap() {
            assert!(
                known.iter().any(|k| (k - f(r)).abs() < 1.0),
                "{ctx} benchmark {r} is one of the report's values {known:?}"
            );
        }

        // Cost: one ownership model; the stacked segments add up to its monthly total.
        let monthly = &val["ownership"]["monthly"];
        let total = f(&out["cost"]["total"]);
        assert_eq!(total, f(&monthly["total"]), "{ctx} monthly total");
        let sum: f64 = out["cost"]["parts"].as_array().unwrap().iter().map(f).sum();
        assert!(
            (sum - total).abs() <= (total * 0.015).max(25.0),
            "{ctx} segments {sum} vs total {total}"
        );
        assert_eq!(
            f(&out["cost"]["quake"]),
            f(&monthly["earthquakeInsurance"]),
            "{ctx} optional quake cover"
        );

        // Captions narrate the same numbers.
        let caps: Vec<&str> = out["captions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(caps.len(), 6, "{ctx}");
        let est_m = format!("${:.2}M", f(&out["value"]["est"]) / 1e6).replace(".00M", "M");
        assert!(
            caps[2].contains(est_m.trim_end_matches('0')),
            "{ctx} value caption {:?} names {est_m}",
            caps[2]
        );
        assert!(
            caps[3].contains(&format!("{}", total as i64 % 1000)),
            "{ctx} cost caption {:?}",
            caps[3]
        );
        assert!(
            caps.iter().all(|c| !c.is_empty()),
            "{ctx} every scene is narrated: {caps:?}"
        );

        // Imagery and outlines share one projection; the camera sits on the parcel.
        let pr = &out["projection"];
        assert_eq!(pr["sameTransform"], json!(true), "{ctx}");
        assert!(
            (f(&pr["centre"][0]) - 640.0).abs() < 1e-6
                && (f(&pr["centre"][1]) - 360.0).abs() < 1e-6,
            "{ctx} {pr}"
        );
        assert!((f(&pr["northPx"]) - 360.0).abs() < 3.6, "{ctx} {pr}");
        let plan = &out["plan"];
        assert!(
            plan["n"].as_u64().unwrap() > 20 && plan["n"].as_u64().unwrap() < 600,
            "{ctx} tile plan {plan}"
        );
        assert!(
            plan["opening"].as_u64().unwrap() <= plan["n"].as_u64().unwrap()
                && plan["opening"].as_u64().unwrap() < 80,
            "{ctx} quick opening {plan}"
        );
        assert!(
            plan["maxZ"].as_u64().unwrap() <= 19,
            "{ctx} never asks for patchy tile levels: {plan}"
        );
        assert_eq!(out["tileUrlsOk"], json!(true), "{ctx}");
        for host in out["imageHosts"].as_array().unwrap() {
            let h = host.as_str().unwrap();
            assert!(
                h.starts_with(
                    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer"
                ) || h.starts_with(
                    "https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer"
                ) || h.starts_with(
                    "https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer"
                ),
                "{ctx} imagery host {host}"
            );
        }

        // Nothing invented: no fallback figures from the old reel.
        let texts = out["texts"].to_string();
        for fake in ["1,250,000", "$1.25M", "$6,500/mo", "1,850"] {
            assert!(
                !texts.contains(fake),
                "{ctx} invented figure {fake} on screen"
            );
        }
    }
}

#[test]
fn chinese_and_spanish_cuts_render_cleanly() {
    let report = fixture(CAMPBELL);
    for lang in ["zh", "es"] {
        let Some(out) = run_motion_in_chromium(&report, lang) else {
            return;
        };
        assert_eq!(
            out["render"]["warningCount"],
            json!(0),
            "[{lang}] {}",
            out["render"]
        );
        let caps = out["captions"].as_array().unwrap();
        assert_eq!(caps.len(), 6);
        if lang == "zh" {
            assert!(
                caps[3].as_str().unwrap().starts_with("持有成本"),
                "{}",
                caps[3]
            );
            assert!(
                !caps.iter().any(|c| c.as_str().unwrap().contains("US$")),
                "one dollar style: {caps:?}"
            );
        } else {
            assert!(
                caps[3].as_str().unwrap().starts_with("Ser dueño"),
                "{}",
                caps[3]
            );
        }
    }
}

#[test]
fn sparse_holdout_report_leaves_gaps_instead_of_inventing_numbers() {
    // Synthetic holdout parcel (`synth-il-60201-vintage-sfr`).
    let holdout = holdout_home_by_id("synth-il-60201-vintage-sfr");
    let address = holdout["address"].as_str().expect("holdout address");
    let slug = slugify(address);
    let real_geo = holdout["raw"]["geo"].clone();

    // Case A: Synthetic holdout parcel before coordinates are resolved (`geo: {}`).
    let sparse_unlocated = json!({
        "meta": { "slug": slug, "displayAddress": address },
        "geo": {}
    });
    let html_unlocated = render_report_html(&sparse_unlocated);
    for fake in ["1,250,000", "$1.25M", "$6,500/mo", "1,850"] {
        assert!(
            !html_unlocated.contains(fake),
            "sparse unlocated HTML invented fallback figure {fake}"
        );
    }

    // Case B: Geocode-only parcel with real Evanston coordinates (`raw.geo`) and no valuation/cost modules.
    let sparse_geocoded = json!({
        "meta": { "slug": slug, "displayAddress": address },
        "geo": real_geo
    });
    let html_geocoded = render_report_html(&sparse_geocoded);
    for fake in ["1,250,000", "$1.25M", "$6,500/mo", "1,850"] {
        assert!(
            !html_geocoded.contains(fake),
            "sparse geocoded HTML invented fallback figure {fake}"
        );
    }

    let Some(out) = run_motion_in_chromium(&sparse_unlocated, "en") else {
        return;
    };
    assert_eq!(out["render"]["warningCount"], json!(0), "{}", out["render"]);
    assert_eq!(out["hasPoint"], json!(false));
    assert!(
        out["value"].is_null() && out["cost"].is_null(),
        "no estimate, no monthly cost: {out}"
    );
    assert_eq!(out["tiles"], json!([]));
    assert_eq!(out["pois"], json!([]));
    assert_eq!(out["layers"], json!([]), "no location, no imagery requests");
    let caps = out["captions"].as_array().unwrap();
    assert_eq!(caps[0], json!(format!("{address}.")));
    assert!(
        caps[1..].iter().all(|c| c.as_str().unwrap().is_empty()),
        "nothing to narrate beyond the address: {caps:?}"
    );
    let texts: Vec<String> = out["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    assert!(
        !texts.iter().any(|t| t.contains('$')),
        "no dollar figure without data: {texts:?}"
    );
    assert!(texts
        .iter()
        .any(|t| t.contains("No value estimate in this report")));

    if let Some(geo_out) = run_motion_in_chromium(&sparse_geocoded, "en") {
        assert_eq!(geo_out["hasPoint"], json!(true));
        assert!(geo_out["value"].is_null() && geo_out["cost"].is_null());
        let geo_texts = geo_out["texts"].to_string();
        assert!(
            !geo_texts.contains('$'),
            "geocoded-only parcel must not invent $: {geo_texts}"
        );
    }
}

#[test]
fn light_and_dark_looks_follow_the_system_setting_and_live_changes() {
    let report = fixture(CAMPBELL);
    for theme in ["light", "dark"] {
        let Some(out) = run_motion_in_chromium_with(&report, "en", theme) else {
            return;
        };
        assert_eq!(
            out["theme"],
            json!(theme),
            "the video starts in the system's {theme} look"
        );
        assert_eq!(out["render"]["theme"], json!(theme));
        assert_eq!(
            out["render"]["modalHasLightClass"],
            json!(theme == "light"),
            "light modal applies .db-motion-light class"
        );
        assert_eq!(
            out["render"]["warningCount"],
            json!(0),
            "[{theme}] {}",
            out["render"]
        );
        let other = if theme == "dark" { "light" } else { "dark" };
        assert_eq!(
            out["pinned"],
            json!([other, theme]),
            "setTheme pins a look; null follows the system again"
        );
        assert_eq!(
            out["liveThemeAfterFlip"]["theme"],
            json!(other),
            "live matchMedia change listener updates theme from {theme} to {other}"
        );
        assert_eq!(
            out["liveThemeAfterFlip"]["modalLightClass"],
            json!(other == "light")
        );
    }
}
