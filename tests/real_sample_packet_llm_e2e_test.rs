//! Real Internet Sample Disclosure Packet + Live LLM API Chromium E2E Regression Suite
//! (`cargo test --test real_sample_packet_llm_e2e_test`).
//!
//! Fixtures stored in `tests/fixtures/real_samples/` (4 real public residential real-estate PDFs,
//! 118 total pages):
//!   - `lakes_area_inspection.pdf` (40 pages: general home inspection + summary/safety hazards + PII)
//!   - `cpm_inspection.pdf` (21 pages: photo-illustrated electrical/plumbing defects + phone PII)
//!   - `capital_nhd_sample.pdf` (33 pages: California Statutory NHD + C.A.R. DSDT / FHDS wildfire notices)
//!   - `pct_prelim_guide.pdf` (24 pages: Preliminary Title Report reader & Schedule B breakdown)
//!
//! Two test modes:
//!   1. `real_sample_packet_browser_e2e`: Always-on hermetic E2E verifying 4-PDF `.zip` unpacking,
//!      PDF.js vector & photo page rendering, PII scrubbing, Circle/Arrow/Pin annotations, and `/api/ask`
//!      wiring without external network calls.
//!   2. `live_keyed_real_sample_packet_llm_chromium_e2e`: Live OpenAI Responses API E2E test (`OPENAI_API_KEY`
//!      required; fails loudly if unconfigured or if deterministic fallback triggers unexpectedly). Exercises:
//!      - Single real 40-page inspection PDF upload -> live LLM executive brief + PDF Studio summary jump +
//!        mouse annotations + live `/api/ask` chat grounded in uploaded packet defects
//!      - Multi-PDF 118-page `.zip` disclosure packet -> live LLM synthesis (`aiEnhanced == true`) +
//!        Mode-2 redacted share + photo inspection defect view + NHD wildfire disclosure view + live `/api/ask`
//!        Q&A across multiple uploaded PDFs.

#![recursion_limit = "512"]

mod common;

use open_estate::ai::crc32_ieee;
use open_estate::db::{DbStore, TABLE_NAMES};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::{ingest_listings, ingest_report_into_db, IngestListingsRequest};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::net::TcpListener;
use uuid::Uuid;

struct SampleE2eServer {
    base_url: String,
    _db: DbStore,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SampleE2eServer {
    async fn start(base_env: EnvConfig, primary_rep: Value) -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-real-sample-e2e-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        let http_cache_dir = temp_dir.join("http_cache");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
        std::fs::create_dir_all(&http_cache_dir).expect("create temp http cache dir");

        let db_path = temp_dir.join("real_sample_e2e.sqlite");
        let env = base_env.with_overrides([
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
            ("ENABLE_SMTP_FEATURES".to_string(), "false".to_string()),
        ]);

        let http = HttpClient::from_env(&env, Some(http_cache_dir));
        let db = DbStore::connect(&env).await.expect("connect e2e sqlite");
        db.ensure_schema().await.expect("ensure schema");
        assert_eq!(
            TABLE_NAMES.len(),
            44,
            "strict 44-table schema invariant must hold"
        );

        let primary_html = render_report_html(&primary_rep);
        db.put_cached_report(
            common::SYNTH_PRIMARY_SLUG,
            &primary_rep,
            Some(&primary_html),
        )
        .await
        .expect("seed primary cached report");
        db.record_report_refresh(common::SYNTH_PRIMARY_SLUG)
            .await
            .expect("record primary refresh");

        ingest_report_into_db(&db, None, &env, &primary_rep, false)
            .await
            .expect("ingest primary report into db");

        ingest_listings(
            &db,
            &http,
            &env,
            IngestListingsRequest {
                zip: Some("95118".to_string()),
                listings: vec![json!({
                    "id": "mls-synth-sample-active-1",
                    "address": "515 Synthbrook Cir, San Jose, CA 95118",
                    "city": "San Jose",
                    "state": "CA",
                    "zip": "95118",
                    "status": "Active",
                    "price": 1_620_000,
                    "listPrice": 1_620_000,
                    "beds": 4.0,
                    "baths": 2.5,
                    "sqft": 2000,
                    "lotSqft": 6200,
                    "yearBuilt": 1988,
                    "propertyType": "Single Family",
                    "source": "real_sample_e2e"
                })],
                trigger_alerts: false,
                ..Default::default()
            },
        )
        .await
        .expect("ingest synthetic active listing");

        let state = AppState::with_components(
            env,
            http,
            db.clone(),
            HttpDefenseShield::default(),
            reports_dir,
            Some(temp_dir.join("datasets")),
        );
        let router = build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind sample e2e port");
        let addr: SocketAddr = listener.local_addr().expect("read bound addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        });

        Self {
            base_url: format!("http://127.0.0.1:{}", addr.port()),
            _db: db,
            temp_dir,
            shutdown_tx: Some(tx),
        }
    }
}

impl Drop for SampleE2eServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn build_stored_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut central_dir = Vec::new();

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32_ieee(data);
        let size = u32::try_from(data.len()).expect("entry fits u32");
        let local_offset = u32::try_from(archive.len()).expect("offset fits u32");

        archive.write_all(&0x0403_4b50u32.to_le_bytes()).unwrap();
        archive.write_all(&20u16.to_le_bytes()).unwrap();
        archive.write_all(&0u16.to_le_bytes()).unwrap();
        archive.write_all(&0u16.to_le_bytes()).unwrap();
        archive.write_all(&0u16.to_le_bytes()).unwrap();
        archive.write_all(&0u16.to_le_bytes()).unwrap();
        archive.write_all(&crc.to_le_bytes()).unwrap();
        archive.write_all(&size.to_le_bytes()).unwrap();
        archive.write_all(&size.to_le_bytes()).unwrap();
        archive
            .write_all(&(name_bytes.len() as u16).to_le_bytes())
            .unwrap();
        archive.write_all(&0u16.to_le_bytes()).unwrap();
        archive.write_all(name_bytes).unwrap();
        archive.write_all(data).unwrap();

        central_dir
            .write_all(&0x0201_4b50u32.to_le_bytes())
            .unwrap();
        central_dir.write_all(&20u16.to_le_bytes()).unwrap();
        central_dir.write_all(&20u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&crc.to_le_bytes()).unwrap();
        central_dir.write_all(&size.to_le_bytes()).unwrap();
        central_dir.write_all(&size.to_le_bytes()).unwrap();
        central_dir
            .write_all(&(name_bytes.len() as u16).to_le_bytes())
            .unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u16.to_le_bytes()).unwrap();
        central_dir.write_all(&0u32.to_le_bytes()).unwrap();
        central_dir.write_all(&local_offset.to_le_bytes()).unwrap();
        central_dir.write_all(name_bytes).unwrap();
    }

    let cd_offset = u32::try_from(archive.len()).expect("cd offset fits u32");
    let cd_size = u32::try_from(central_dir.len()).expect("cd size fits u32");
    let count = u16::try_from(entries.len()).expect("entry count fits u16");
    archive.write_all(&central_dir).unwrap();
    archive.write_all(&0x0605_4b50u32.to_le_bytes()).unwrap();
    archive.write_all(&0u16.to_le_bytes()).unwrap();
    archive.write_all(&0u16.to_le_bytes()).unwrap();
    archive.write_all(&count.to_le_bytes()).unwrap();
    archive.write_all(&count.to_le_bytes()).unwrap();
    archive.write_all(&cd_size.to_le_bytes()).unwrap();
    archive.write_all(&cd_offset.to_le_bytes()).unwrap();
    archive.write_all(&0u16.to_le_bytes()).unwrap();
    archive
}

fn discover_chromium_executable() -> PathBuf {
    if let Ok(raw) = std::env::var("PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_file() {
                return p;
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let cache_root = Path::new(&home).join(".cache/ms-playwright");
        if let Ok(rd) = std::fs::read_dir(&cache_root) {
            let mut dirs: Vec<(u64, PathBuf)> = rd
                .flatten()
                .filter_map(|ent| {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    let rev: u64 = name.strip_prefix("chromium-")?.parse().unwrap_or(0);
                    Some((rev, ent.path()))
                })
                .collect();
            dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            for (_, dir) in dirs {
                for sub in ["chrome-linux64/chrome", "chrome-linux/chrome"] {
                    let candidate = dir.join(sub);
                    if candidate.is_file() {
                        return candidate;
                    }
                }
            }
        }
    }
    for bin_name in ["chromium", "chromium-browser", "google-chrome"] {
        if let Ok(out) = Command::new("which").arg(bin_name).output() {
            if out.status.success() {
                let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    panic!("Headless Chromium executable not found");
}

fn discover_node_executable() -> PathBuf {
    if let Ok(out) = Command::new("node").arg("--version").output() {
        if out.status.success() {
            return PathBuf::from("node");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let fallback = Path::new(&home).join(".local/bin/node");
        if fallback.is_file() {
            return fallback;
        }
    }
    panic!("Node >= 22 executable with WebSocket is required");
}

fn load_live_llm_env() -> EnvConfig {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let parent_env = manifest_dir
        .parent()
        .map(|p| EnvConfig::from_process_and_dotenv(Some(p)))
        .unwrap_or_else(EnvConfig::from_env);
    let base = if parent_env.llm_configured() {
        parent_env
    } else {
        EnvConfig::from_process_and_dotenv(Some(manifest_dir))
    };
    assert!(
        base.llm_configured(),
        "live_keyed_* tests require OPENAI_API_KEY (+ OPENAI_BASE_URL) in env or .env"
    );
    base
}

fn build_primary_report_fixture() -> Value {
    let mut rep = common::synth_report_primary();
    normalize_stored_report(&mut rep);
    rep["facts"]["asOf"] = json!("2026-11-15");
    rep["facts"]["forSale"] = json!(true);
    rep["facts"]["listingStatus"] = json!("Active");
    rep["facts"]["listPrice"] = json!(1_620_000);
    rep["facts"]["sqft"] = json!(2000);
    rep["facts"]["yearBuilt"] = json!(1988);
    rep["facts"]["propertyType"] = json!("Single Family");
    rep["facts"]["apn"] = json!("421-18-022");
    rep["facts"]["mlsListing"] = json!({
        "status": "Active",
        "listPrice": 1_620_000,
        "mlsNumber": "ML81999999",
        "agentEmail": "listing.agent@example.com",
        "agentName": "Jane Listing",
        "officeName": "Silicon Valley Realty"
    });
    rep
}

const SAMPLE_CDP_RUNNER_MJS: &str = r###"
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const CHROMIUM_EXEC = process.env.CHROMIUM_EXEC;
const BASE_URL = process.env.BASE_URL.replace(/\/+$/, '');
const PRIMARY_SLUG = process.env.PRIMARY_SLUG;
const SINGLE_PDF_PATH = process.env.SINGLE_PDF_PATH;
const ZIP_PACKET_PATH = process.env.ZIP_PACKET_PATH;
const EXPECT_LIVE_LLM = process.env.EXPECT_LIVE_LLM === '1';
const USER_DATA_DIR = process.env.USER_DATA_DIR;
const SCREENSHOT_DIR = (process.env.OPEN_ESTATE_SAMPLE_SHOTS_DIR || '').trim();
const BOB_EMAIL = process.env.BOB_EMAIL || 'bob.agent@example.com';

if (SCREENSHOT_DIR) {
  fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
}

class CdpConnection {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 0;
    this.pending = new Map();
    this.listeners = new Map();
    ws.addEventListener('message', async (ev) => {
      let text;
      if (typeof ev.data === 'string') text = ev.data;
      else if (ev.data instanceof ArrayBuffer) text = Buffer.from(ev.data).toString('utf8');
      else if (ev.data && typeof ev.data.text === 'function') text = await ev.data.text();
      else text = String(ev.data);

      const msg = JSON.parse(text);
      if (msg.id !== undefined) {
        const entry = this.pending.get(msg.id);
        if (entry) {
          this.pending.delete(msg.id);
          if (msg.error) entry.reject(new Error(`CDP ${entry.method}: ${msg.error.message || JSON.stringify(msg.error)}`));
          else entry.resolve(msg.result || {});
        }
        return;
      }
      const key = `${msg.sessionId || ''}:${msg.method}`;
      const list = this.listeners.get(key) || [];
      for (const fn of [...list]) {
        try { fn(msg.params || {}); } catch (_) {}
      }
    });
  }
  send(method, params = {}, sessionId = undefined) {
    const id = ++this.nextId;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { method, resolve, reject });
      this.ws.send(JSON.stringify(payload));
    });
  }
  on(sessionId, method, handler) {
    const key = `${sessionId || ''}:${method}`;
    const list = this.listeners.get(key) || [];
    list.push(handler);
    this.listeners.set(key, list);
    return () => {
      const cur = this.listeners.get(key) || [];
      this.listeners.set(key, cur.filter((x) => x !== handler));
    };
  }
}

class CdpPage {
  constructor(cdp, sessionId, browserContextId, targetId) {
    this.cdp = cdp;
    this.sessionId = sessionId;
    this.browserContextId = browserContextId;
    this.targetId = targetId;
    this.pageErrors = [];
    this.consoleErrors = [];
    this.disclosuresResponses = [];
    this.askResponses = [];

    this.cdp.on(this.sessionId, 'Fetch.requestPaused', (params) => {
      const url = (params.request && params.request.url) || '';
      if (url.startsWith('https://')) {
        this.cdp.send('Fetch.failRequest', { requestId: params.requestId, errorReason: 'Failed' }, this.sessionId).catch(() => {});
      } else {
        this.cdp.send('Fetch.continueRequest', { requestId: params.requestId }, this.sessionId).catch(() => {});
      }
    });

    this.cdp.on(this.sessionId, 'Runtime.exceptionThrown', (params) => {
      const det = params.exceptionDetails || {};
      const msg = (det.exception && (det.exception.description || det.exception.value)) || det.text || JSON.stringify(det);
      this.pageErrors.push(String(msg));
    });

    this.cdp.on(this.sessionId, 'Runtime.consoleAPICalled', (params) => {
      if (params.type === 'error') {
        const text = (params.args || []).map((a) => a.value !== undefined ? String(a.value) : (a.description || a.type || '')).join(' ');
        this.consoleErrors.push(text);
      }
    });

    this.cdp.on(this.sessionId, 'Page.javascriptDialogOpening', () => {
      this.cdp.send('Page.handleJavaScriptDialog', { accept: true }, this.sessionId).catch(() => {});
    });
  }

  async init() {
    await this.cdp.send('Page.enable', {}, this.sessionId);
    await this.cdp.send('Runtime.enable', {}, this.sessionId);
    await this.cdp.send('DOM.enable', {}, this.sessionId);
    await this.cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1440,
      height: 920,
      deviceScaleFactor: 1,
      mobile: false,
    }, this.sessionId);
    await this.cdp.send('Fetch.enable', {
      patterns: [{ urlPattern: 'https://*', requestStage: 'Request' }],
    }, this.sessionId);
    await this.cdp.send('DOM.getDocument', { depth: 0 }, this.sessionId);
  }

  async installApiSpy() {
    await this.eval(() => {
      if (window.__oeApiSpyInstalled) return;
      window.__oeApiSpyInstalled = true;
      window.__oeSpyRecords = { disclosures: [], ask: [] };
      const origFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const url = typeof input === 'string' ? input : (input && input.url ? input.url : '');
        const method = ((init && init.method) || 'GET').toUpperCase();
        const res = await origFetch(input, init);
        if (method === 'POST' && (url.includes('/api/copilot/disclosures') || url.includes('/api/ask'))) {
          try {
            const cloned = res.clone();
            const json = await cloned.json();
            if (url.includes('/api/ask')) {
              window.__oeSpyRecords.ask.push({ status: res.status, json });
            } else if (!url.includes('/share') && !url.includes('/authorized')) {
              window.__oeSpyRecords.disclosures.push({ status: res.status, json });
            }
          } catch (_) {}
        }
        return res;
      };
    });
  }

  async close() {
    await this.cdp.send('Target.disposeBrowserContext', { browserContextId: this.browserContextId }).catch(() => {});
  }

  async eval(fnOrExpr, ...args) {
    const expression = typeof fnOrExpr === 'function'
      ? `(${fnOrExpr.toString()})(${args.map((a) => JSON.stringify(a)).join(',')})`
      : String(fnOrExpr);
    const res = await this.cdp.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    }, this.sessionId);
    if (res.exceptionDetails) {
      const det = res.exceptionDetails;
      const errText = (det.exception && (det.exception.description || det.exception.value)) || det.text || 'Runtime.evaluate exception';
      throw new Error(`JS evaluation error: ${errText}`);
    }
    return res.result ? res.result.value : undefined;
  }

  async waitFor(fnOrExpr, opts = {}, ...args) {
    const timeout = opts.timeout ?? 25000;
    const interval = opts.interval ?? 80;
    const desc = opts.desc || 'condition';
    const deadline = Date.now() + timeout;
    let lastErr = null;
    while (Date.now() < deadline) {
      try {
        const val = await this.eval(fnOrExpr, ...args);
        if (val) return val;
        lastErr = null;
      } catch (e) {
        lastErr = e;
      }
      await new Promise((r) => setTimeout(r, interval));
    }
    const diag = await this.eval(() => ({
      url: location.href,
      errBanner: document.getElementById('bcErrBanner')?.textContent || '',
      psErrBanner: document.getElementById('psErrorBannerText')?.textContent || '',
      chatLog: document.getElementById('psChatLog')?.innerText?.slice(0, 300) || '',
    })).catch(() => ({}));
    throw new Error(`Timed out after ${timeout}ms waiting for ${desc}${lastErr ? ` (${lastErr.message})` : ''} | diag=${JSON.stringify(diag)}`);
  }

  async goto(url) {
    let loaded = false;
    const unsub = this.cdp.on(this.sessionId, 'Page.loadEventFired', () => { loaded = true; });
    try {
      const nav = await this.cdp.send('Page.navigate', { url }, this.sessionId);
      if (nav.errorText) throw new Error(`Navigation failed to ${url}: ${nav.errorText}`);
      const deadline = Date.now() + 20000;
      while (!loaded && Date.now() < deadline) await new Promise((r) => setTimeout(r, 30));
      await this.waitFor(() => document.readyState === 'complete', { desc: `readyState complete (${url})` });
    } finally {
      unsub();
    }
    await this.cdp.send('DOM.getDocument', { depth: 0 }, this.sessionId);
  }

  async click(selector, opts = {}) {
    const box = await this.waitFor((sel) => {
      const el = document.querySelector(sel);
      if (!el || el.disabled) return null;
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      const r = el.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) return null;
      return {
        x: Math.max(1, Math.min(window.innerWidth - 2, r.left + r.width / 2)),
        y: Math.max(1, Math.min(window.innerHeight - 2, r.top + r.height / 2)),
      };
    }, { desc: `clickable ${selector}`, timeout: opts.timeout ?? 15000 }, selector);

    await this.cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: box.x, y: box.y, button: 'none' }, this.sessionId);
    await this.cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: box.x, y: box.y, button: 'left', buttons: 1, clickCount: 1 }, this.sessionId);
    await this.cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: box.x, y: box.y, button: 'left', buttons: 0, clickCount: 1 }, this.sessionId);
  }

  async fill(selector, value) {
    await this.waitFor((sel, val) => {
      const el = document.querySelector(sel);
      if (!el || el.disabled) return false;
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      el.focus();
      const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
      if (setter) setter.call(el, String(val)); else el.value = String(val);
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    }, { desc: `fill ${selector}` }, selector, value);
  }

  async setFiles(selector, filePaths) {
    const doc = await this.cdp.send('DOM.getDocument', { depth: -1 }, this.sessionId);
    const { nodeId } = await this.cdp.send('DOM.querySelector', {
      nodeId: doc.root.nodeId,
      selector,
    }, this.sessionId);
    assert(nodeId > 0, `File input not found: ${selector}`);
    await this.cdp.send('DOM.setFileInputFiles', { nodeId, files: filePaths }, this.sessionId);
  }

  async dragOnElement(selector, [x0Pct, y0Pct], [x1Pct, y1Pct], steps = 8) {
    const rect = await this.waitFor((sel) => {
      const el = document.querySelector(sel);
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return r.width > 10 && r.height > 10 ? { left: r.left, top: r.top, width: r.width, height: r.height } : null;
    }, { desc: `rect for ${selector}` }, selector);
    const sx = rect.left + rect.width * x0Pct;
    const sy = rect.top + rect.height * y0Pct;
    const ex = rect.left + rect.width * x1Pct;
    const ey = rect.top + rect.height * y1Pct;

    await this.cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: sx, y: sy, button: 'none' }, this.sessionId);
    await this.cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: sx, y: sy, button: 'left', buttons: 1, clickCount: 1 }, this.sessionId);
    for (let i = 1; i <= steps; i++) {
      const t = i / steps;
      await this.cdp.send('Input.dispatchMouseEvent', {
        type: 'mouseMoved',
        x: sx + (ex - sx) * t,
        y: sy + (ey - sy) * t,
        button: 'left',
        buttons: 1,
      }, this.sessionId);
    }
    await this.cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: ex, y: ey, button: 'left', buttons: 0, clickCount: 1 }, this.sessionId);
  }

  async snap(filename) {
    if (!SCREENSHOT_DIR) return;
    await this.eval(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    const { data } = await this.cdp.send('Page.captureScreenshot', { format: 'png' }, this.sessionId);
    const outPath = path.join(SCREENSHOT_DIR, filename);
    fs.writeFileSync(outPath, Buffer.from(data, 'base64'));
    console.log(`   📸 saved ${outPath}`);
  }

  assertZeroErrors(label) {
    assert.deepStrictEqual(this.pageErrors, [], `[${label}] Unexpected pageErrors: ${JSON.stringify(this.pageErrors)}`);
    assert.deepStrictEqual(this.consoleErrors, [], `[${label}] Unexpected consoleErrors: ${JSON.stringify(this.consoleErrors)}`);
  }
}

async function launchChromiumCdp() {
  const args = [
    '--headless=new',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-dev-shm-usage',
    '--disable-background-networking',
    '--disable-component-update',
    '--disable-default-apps',
    '--disable-extensions',
    '--disable-sync',
    '--no-first-run',
    '--no-default-browser-check',
    '--metrics-recording-only',
    '--mute-audio',
    '--window-size=1440,920',
    '--user-agent=Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36',
    '--remote-debugging-port=0',
    `--user-data-dir=${USER_DATA_DIR}`,
    'about:blank',
  ];
  const proc = spawn(CHROMIUM_EXEC, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let stderrBuf = '';
  const wsUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`Chromium DevTools timeout. stderr:\n${stderrBuf}`)), 15000);
    const onChunk = (chunk) => {
      stderrBuf += chunk.toString('utf8');
      const m = stderrBuf.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (m) { clearTimeout(timer); resolve(m[1]); }
    };
    proc.stderr.on('data', onChunk);
    proc.stdout.on('data', onChunk);
    proc.on('exit', (code, sig) => {
      clearTimeout(timer);
      reject(new Error(`Chromium exited prematurely (code=${code}, sig=${sig}). stderr:\n${stderrBuf}`));
    });
  });
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('WebSocket timeout')), 10000);
    ws.addEventListener('open', () => { clearTimeout(t); resolve(); });
    ws.addEventListener('error', (e) => { clearTimeout(t); reject(new Error(`WS error: ${e.message || e.type}`)); });
  });
  return { proc, ws, cdp: new CdpConnection(ws) };
}

async function createIsolatedPage(cdp) {
  const { browserContextId } = await cdp.send('Target.createBrowserContext', { disposeOnDetach: true });
  const { targetId } = await cdp.send('Target.createTarget', {
    url: 'about:blank',
    browserContextId,
    width: 1440,
    height: 920,
  });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = new CdpPage(cdp, sessionId, browserContextId, targetId);
  await page.init();
  return page;
}

async function waitPdfCanvasHasInk(page, expectedDocSubstring = '', expectedPage = null) {
  return await page.waitFor((docSub, expPage) => {
    const bd = document.getElementById('oePdfStudioBackdrop');
    if (!bd || !bd.classList.contains('on')) return null;
    const st = window.OpenEstatePdfStudio?.getState?.();
    if (!st || !st.isOpen || !st.pdfJsActive) return null;
    if (docSub && !String(st.renderedDocName || '').includes(docSub)) return null;
    if (expPage != null && st.renderedPage !== expPage) return null;
    const pdfCanvas = document.getElementById('psPdfCanvas');
    if (!pdfCanvas || pdfCanvas.width < 100 || pdfCanvas.height < 100) return null;
    const data = pdfCanvas.getContext('2d').getImageData(0, 0, pdfCanvas.width, pdfCanvas.height).data;
    let nonWhite = 0;
    for (let i = 0; i < data.length; i += 16) {
      if (data[i + 3] > 0 && (data[i] < 225 || data[i + 1] < 225 || data[i + 2] < 225)) {
        nonWhite++;
        if (nonWhite > 60) break;
      }
    }
    if (nonWhite <= 60) return null;
    const tl = document.getElementById('psTextLayer');
    return {
      docName: st.currentDocName,
      currentPage: st.currentPage,
      numPages: st.numPages,
      pdfJsActive: !!st.pdfJsActive,
      tlTextLen: (tl?.textContent || '').trim().length,
      nonWhite,
    };
  }, { desc: `PDF Studio render (${expectedDocSubstring || 'any'} p.${expectedPage ?? 'any'})`, timeout: 25000 }, expectedDocSubstring, expectedPage);
}

async function runSingleInspectionFlow(cdp) {
  console.log('▶ Flow A: Single Real 40-page Home Inspection PDF + Live LLM + PDF Studio + /api/ask');
  const page = await createIsolatedPage(cdp);
  try {
    await page.goto(`${BASE_URL}/assistant`);
    await page.installApiSpy();

    await page.fill('#bcAddrInput', '100 Synthparcelalpha Ln, San Jose, CA 95118');
    await page.click('#bcLookupBtn');
    await page.waitFor(() => document.getElementById('bcDynamicBody')?.innerText.includes('Actively For Sale'), {
      desc: 'Active listing lookup card',
    });
    await page.click('[data-fn-pick="disclosures"]');
    await page.waitFor(() => !!document.getElementById('bcDiscFileInput'), { desc: 'Disclosures upload dropzone without login gate' });
    const ossShareAbsence = await page.eval(() => ({
      regBar: !!document.getElementById('bcRegUserBar'),
      shareBtn: (() => {
        const b = document.getElementById('shareReportBtn');
        return !!(b && !b.hidden && getComputedStyle(b).display !== 'none');
      })(),
      authSection: !!document.getElementById('bcAuthorizedSection'),
    }));
    assert.equal(ossShareAbsence.regBar, false, 'Open Estate OSS must not render account registration bar');
    assert.equal(ossShareAbsence.shareBtn, false, 'Open Estate OSS must hide #shareReportBtn');
    assert.equal(ossShareAbsence.authSection, false, 'Open Estate OSS must not render seller-authorized sharing section');

    await page.setFiles('#bcDiscFileInput', [SINGLE_PDF_PATH]);
    const analysisUi = await page.waitFor(() => {
      const badge = document.getElementById('bcOneTimeCompletedBadge');
      const panel = document.getElementById('bcPanelFn2');
      if (!badge || !panel) return null;
      const brief = document.getElementById('bcAiPacketBriefCard');
      const modelBadge = document.getElementById('bcSynthBadge');
      return {
        panelText: panel.innerText,
        briefText: brief ? brief.innerText : '',
        modelBadgeText: modelBadge ? modelBadge.innerText : '',
        spy: window.__oeSpyRecords?.disclosures || [],
      };
    }, { desc: 'Single real PDF disclosure analysis completion', timeout: 180000 });

    assert(analysisUi.spy.length >= 1, 'Expected captured POST /api/copilot/disclosures response');
    const apiData = analysisUi.spy[0].json?.data || {};
    console.log(`   ℹ Single PDF API response: model=${apiData.model} aiEnhanced=${apiData.aiEnhanced} ms=${apiData.ms} docs=${apiData.packetDocumentCount} defects=${(apiData.deduplicatedDefects || []).length}`);
    assert.equal(apiData.packetDocumentCount, 1);
    assert(analysisUi.panelText.length > 200);
    if (EXPECT_LIVE_LLM) {
      assert.equal(apiData.aiEnhanced, true, `Expected live LLM aiEnhanced=true on single sample PDF, got warnings=${JSON.stringify(analysisUi.spy[0].json?.warnings)}`);
      assert(apiData.model && !String(apiData.model).includes('deterministic'), `Expected real LLM model, got ${apiData.model}`);
      assert(analysisUi.briefText.length > 40, 'Expected #bcAiPacketBriefCard to display live LLM synthesis');
    }
    await page.snap('01_real_inspection_ai_analysis.png');

    // Open PDF Studio on the cited finding and inspect the real 40-page Home Inspection PDF
    await page.click('[data-pdf-cite-idx="0"]');
    const initRender = await waitPdfCanvasHasInk(page, 'lakes_area_inspection.pdf');
    assert.equal(initRender.numPages, 40, `Expected 40 pages in lakes_area_inspection.pdf, got ${initRender.numPages}`);
    assert.equal(initRender.pdfJsActive, true, 'PDF.js must render the 40-page sample inspection PDF');

    // Jump directly to page 38 (SUMMARY* Potential Safety Hazards & Deferred Cost Items) via toolbar #psPageInput
    await page.fill('#psPageInput', '38');
    await page.eval(() => {
      const inp = document.getElementById('psPageInput');
      inp.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    });
    const p38 = await waitPdfCanvasHasInk(page, 'lakes_area_inspection.pdf', 38);
    assert.equal(p38.currentPage, 38);

    // Draw Circle, Arrow, and Pin annotations over p.38
    await page.click('[data-ps-tool="circle"]');
    await page.dragOnElement('#psAnnotCanvas', [0.18, 0.34], [0.76, 0.56], 8);
    await page.click('[data-ps-tool="arrow"]');
    await page.dragOnElement('#psAnnotCanvas', [0.12, 0.62], [0.32, 0.48], 8);
    await page.click('[data-ps-tool="pin"]');
    await page.dragOnElement('#psAnnotCanvas', [0.46, 0.44], [0.46, 0.44], 1);

    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 3, {
      desc: '3 annotations persisted on page 38 of real inspection PDF',
    });
    await page.snap('02_real_inspection_pdf_studio.png');

    // Switch to AI side chat and send live /api/ask grounded in the uploaded inspection PDF
    await page.click('#psTabBtnChat');
    const curVal = await page.eval(() => document.getElementById('psChatInput')?.value || '');
    await page.fill('#psChatInput', `${curVal}Based on the uploaded home inspection packet, summarize the water heater, chimney, and deferred cost risks for a buyer.`);
    await page.click('#psChatSendBtn');
    const askState = await page.waitFor(() => {
      const recs = window.__oeSpyRecords?.ask || [];
      const msgs = Array.from(document.querySelectorAll('#psChatLog .ps-msg.a'));
      if (!recs.length || !msgs.length) return null;
      const lastTxt = msgs[msgs.length - 1].innerText.trim();
      if (!lastTxt || lastTxt.includes('Checking records')) return null;
      return { reply: lastTxt, api: recs[recs.length - 1].json };
    }, { desc: 'PDF Studio live /api/ask reply on single inspection PDF', timeout: 150000 });

    console.log(`   ℹ PDF Studio single-PDF /api/ask: model=${askState.api?.model} ms=${askState.api?.ms} replyLen=${askState.reply.length}`);
    assert(askState.reply.length > 40, `Expected meaningful reply, got: ${askState.reply}`);
    if (EXPECT_LIVE_LLM) {
      assert(
        askState.api?.model && !String(askState.api.model).includes('deterministic') && !String(askState.api.model).includes('guardrail'),
        `Expected real live OpenAI model for /api/ask, got ${JSON.stringify(askState.api)}`
      );
      const low = askState.reply.toLowerCase();
      assert(
        low.includes('water heater') || low.includes('chimney') || low.includes('furnace') || low.includes('inspection') || low.includes('relief valve'),
        `Expected live LLM /api/ask reply to reference uploaded inspection packet findings, got: ${askState.reply}`
      );
    }

    page.assertZeroErrors('Flow A');
    console.log('✓ Flow A passed');
  } finally {
    await page.close();
  }
}

async function runMultiDocZipFlow(cdp) {
  console.log('▶ Flow B: Multi-PDF 118-page .zip Real Disclosure Packet + Live LLM + Redacted Share + Photo PDF + NHD View');
  const page = await createIsolatedPage(cdp);
  try {
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}`);
    await page.installApiSpy();

    await page.click('#buyerCopilotHdrBtn');
    await page.waitFor(() => document.getElementById('bcReportModalBackdrop')?.classList.contains('on'), {
      desc: 'Copilot modal opened',
    });
    await page.click('#bcReportModalBackdrop [data-fn-pick="disclosures"]');
    await page.waitFor(() => !!document.querySelector('#bcReportModalBackdrop #bcDiscFileInput'), {
      desc: 'Disclosures upload dropzone in modal without login gate',
    });

    await page.setFiles('#bcReportModalBackdrop #bcDiscFileInput', [ZIP_PACKET_PATH]);
    const zipUi = await page.waitFor(() => {
      const modal = document.getElementById('bcReportModalBackdrop');
      const badge = modal?.querySelector('#bcOneTimeCompletedBadge');
      const panel = modal?.querySelector('#bcPanelFn2');
      if (!badge || !panel) return null;
      const brief = modal.querySelector('#bcAiPacketBriefCard');
      const cards = Array.from(panel.querySelectorAll('.bc-prio-card')).map((el) => ({
        text: el.innerText,
        missing: el.classList.contains('missing'),
      }));
      return {
        panelText: panel.innerText,
        briefText: brief ? brief.innerText : '',
        cards,
        hasRegBar: !!modal.querySelector('#bcRegUserBar'),
        hasShareForm: !!modal.querySelector('#bcShareDealTeamForm'),
        hasAuthSection: !!modal.querySelector('#bcAuthorizedSection'),
        spy: window.__oeSpyRecords?.disclosures || [],
      };
    }, { desc: 'Multi-PDF 118-page .zip analysis completion', timeout: 210000 });

    assert.equal(zipUi.hasRegBar, false, 'Open Estate OSS modal must not show #bcRegUserBar');
    assert.equal(zipUi.hasShareForm, false, 'Open Estate OSS modal must not show #bcShareDealTeamForm');
    assert.equal(zipUi.hasAuthSection, false, 'Open Estate OSS modal must not show #bcAuthorizedSection');
    assert(zipUi.spy.length >= 1, 'Expected captured POST /api/copilot/disclosures for .zip');
    const zipApi = zipUi.spy[0].json?.data || {};
    console.log(`   ℹ ZIP Packet API response: model=${zipApi.model} aiEnhanced=${zipApi.aiEnhanced} ms=${zipApi.ms} docs=${zipApi.packetDocumentCount} defects=${(zipApi.deduplicatedDefects || []).length}`);
    assert.equal(zipApi.packetDocumentCount, 4, `Expected 4 PDFs inside real_disclosure_packet.zip, got ${zipApi.packetDocumentCount}`);
    // Ensure real PII phone number from cpm_inspection.pdf was masked across serialized response
    const zipJsonStr = JSON.stringify(zipApi);
    assert(!zipJsonStr.includes('(561) 866 9956') && !zipJsonStr.includes('561-866-9956'), 'Phone PII must be redacted in ZIP analysis');
    if (EXPECT_LIVE_LLM) {
      assert.equal(zipApi.aiEnhanced, true, `Expected live LLM aiEnhanced=true on .zip packet, got warnings=${JSON.stringify(zipUi.spy[0].json?.warnings)}`);
      assert(zipApi.model && !String(zipApi.model).includes('deterministic'), `Expected real LLM model, got ${zipApi.model}`);
      assert(zipUi.briefText.length > 40, 'Expected #bcAiPacketBriefCard to display live LLM synthesis for ZIP');
    }

    await page.snap('03_real_zip_packet_ai_analysis.png');

    // Open PDF Studio with all 4 real PDFs unpacked from .zip
    await page.click('#bcReportModalBackdrop [data-pdf-cite-idx="0"]');
    const studioZip = await waitPdfCanvasHasInk(page);
    const options = await page.eval(() => Array.from(document.getElementById('psFileSelect')?.options || []).map((o) => o.textContent.trim()));
    assert.equal(options.length, 4, `Expected 4 unpacked real PDFs in #psFileSelect, got ${JSON.stringify(options)}`);

    // Switch to cpm_inspection.pdf page 17 (photo + reversed hot/neutral master bath outlet) via real select & page input
    const cpmIdx = options.findIndex((o) => o.includes('cpm_inspection.pdf'));
    assert(cpmIdx >= 0, `cpm_inspection.pdf missing from #psFileSelect: ${JSON.stringify(options)}`);
    await page.eval((idx) => {
      const sel = document.getElementById('psFileSelect');
      sel.value = String(idx);
      sel.dispatchEvent(new Event('change', { bubbles: true }));
    }, cpmIdx);
    await waitPdfCanvasHasInk(page, 'cpm_inspection.pdf');
    await page.fill('#psPageInput', '17');
    await page.eval(() => {
      document.getElementById('psPageInput').dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    });
    await waitPdfCanvasHasInk(page, 'cpm_inspection.pdf', 17);

    await page.click('[data-ps-tool="circle"]');
    await page.dragOnElement('#psAnnotCanvas', [0.16, 0.50], [0.52, 0.72], 8);
    await page.click('[data-ps-tool="arrow"]');
    await page.dragOnElement('#psAnnotCanvas', [0.64, 0.56], [0.48, 0.62], 8);
    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 2, {
      desc: 'Annotations on photo inspection p.17',
    });
    await page.snap('04_real_photo_inspection_pdf_studio.png');

    // Switch to capital_nhd_sample.pdf p.2 (California NHD Report Snapshot + AB38 Wildfire) & ask live multi-doc /api/ask
    const nhdIdx = options.findIndex((o) => o.includes('capital_nhd_sample.pdf'));
    assert(nhdIdx >= 0, `capital_nhd_sample.pdf missing from #psFileSelect: ${JSON.stringify(options)}`);
    await page.eval((idx) => {
      const sel = document.getElementById('psFileSelect');
      sel.value = String(idx);
      sel.dispatchEvent(new Event('change', { bubbles: true }));
    }, nhdIdx);
    await waitPdfCanvasHasInk(page, 'capital_nhd_sample.pdf');
    await page.click('#psNextPageBtn');
    await waitPdfCanvasHasInk(page, 'capital_nhd_sample.pdf', 2);

    await page.click('[data-ps-tool="circle"]');
    await page.dragOnElement('#psAnnotCanvas', [0.10, 0.25], [0.90, 0.44], 8);
    await page.click('#psTabBtnChat');
    await page.fill('#psChatInput', 'Summarize the major hazards and electrical or plumbing defects across this 4-document disclosure packet, and what repair credits or verifications a buyer should request.');
    await page.click('#psChatSendBtn');
    const zipAsk = await page.waitFor(() => {
      const recs = window.__oeSpyRecords?.ask || [];
      const msgs = Array.from(document.querySelectorAll('#psChatLog .ps-msg.a'));
      if (!recs.length || !msgs.length) return null;
      const lastTxt = msgs[msgs.length - 1].innerText.trim();
      if (!lastTxt || lastTxt.includes('Checking records')) return null;
      return { reply: lastTxt, api: recs[recs.length - 1].json };
    }, { desc: 'PDF Studio live /api/ask on 4-PDF ZIP disclosure packet', timeout: 150000 });

    console.log(`   ℹ PDF Studio ZIP /api/ask: model=${zipAsk.api?.model} ms=${zipAsk.api?.ms} replyLen=${zipAsk.reply.length}`);
    assert(zipAsk.reply.length > 50, `Expected detailed multi-doc /api/ask reply, got: ${zipAsk.reply}`);
    if (EXPECT_LIVE_LLM) {
      assert(
        zipAsk.api?.model && !String(zipAsk.api.model).includes('deterministic') && !String(zipAsk.api.model).includes('guardrail'),
        `Expected real OpenAI model on ZIP /api/ask, got ${JSON.stringify(zipAsk.api)}`
      );
    }
    await page.snap('05_real_zip_packet_pdf_studio_llm_chat.png');

    page.assertZeroErrors('Flow B');
    console.log('✓ Flow B passed');
  } finally {
    await page.close();
  }
}

async function main() {
  const { proc, ws, cdp } = await launchChromiumCdp();
  try {
    await runSingleInspectionFlow(cdp);
    await runMultiDocZipFlow(cdp);
    console.log('\nALL REAL SAMPLE PACKET BROWSER E2E CHECKS PASSED');
  } finally {
    try { await cdp.send('Browser.close'); } catch (_) {}
    try { ws.close(); } catch (_) {}
    try { proc.kill('SIGKILL'); } catch (_) {}
  }
}

main().catch((err) => {
  console.error('\nREAL SAMPLE PACKET E2E FAILED:', err);
  process.exit(1);
});
"###;

async fn run_real_sample_browser_suite(base_env: EnvConfig, expect_live_llm: bool) {
    let chromium_exec = discover_chromium_executable();
    let node_exec = discover_node_executable();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures_dir = manifest_dir.join("tests/fixtures/real_samples");

    let lakes_pdf = fixtures_dir.join("lakes_area_inspection.pdf");
    let cpm_pdf = fixtures_dir.join("cpm_inspection.pdf");
    let nhd_pdf = fixtures_dir.join("capital_nhd_sample.pdf");
    let prelim_pdf = fixtures_dir.join("pct_prelim_guide.pdf");
    for p in [&lakes_pdf, &cpm_pdf, &nhd_pdf, &prelim_pdf] {
        assert!(p.is_file(), "Missing real sample fixture: {}", p.display());
    }

    let primary_rep = build_primary_report_fixture();
    let srv = SampleE2eServer::start(base_env, primary_rep).await;

    let work_dir = std::env::temp_dir().join(format!("oe-real-sample-cdp-{}", Uuid::new_v4()));
    let user_data_dir = work_dir.join("chrome-profile");
    std::fs::create_dir_all(&user_data_dir).unwrap();

    // Verify sharing/account routes remain disabled (404) in Open Estate OSS
    let http_client = Client::new();
    for (route, payload) in [
        (
            "/api/copilot/auth/register",
            json!({ "email": "oss.check@example.com", "displayName": "OSS Check", "role": "buyer" }),
        ),
        (
            "/api/copilot/disclosures/share",
            json!({ "slug": common::SYNTH_PRIMARY_SLUG, "recipientEmail": "bob.realpartner@example.com", "collaboratorAttested": true }),
        ),
        (
            "/api/copilot/disclosures/authorized",
            json!({ "slug": common::SYNTH_PRIMARY_SLUG, "uploaderRole": "seller", "signerName": "Owner", "authorizedToDistribute": true, "documents": [] }),
        ),
    ] {
        let resp = http_client
            .post(format!("{}{}", srv.base_url, route))
            .header("Origin", &srv.base_url)
            .json(&payload)
            .send()
            .await
            .expect("POST oss disabled sharing route");
        assert_eq!(resp.status(), 404, "Open Estate OSS must not expose sharing route {route}");
    }
    let bob_email = "bob.realpartner@example.com";

    let zip_bytes = build_stored_zip_bytes(&[
        ("01_lakes_area_inspection.pdf", &std::fs::read(&lakes_pdf).unwrap()),
        ("02_cpm_inspection.pdf", &std::fs::read(&cpm_pdf).unwrap()),
        ("03_capital_nhd_sample.pdf", &std::fs::read(&nhd_pdf).unwrap()),
        ("04_pct_prelim_guide.pdf", &std::fs::read(&prelim_pdf).unwrap()),
    ]);
    let zip_path = work_dir.join("Real_Sample_Disclosure_Packet.zip");
    std::fs::write(&zip_path, &zip_bytes).expect("write zip packet");

    let script_path = work_dir.join("real_sample_cdp_e2e.mjs");
    std::fs::write(&script_path, SAMPLE_CDP_RUNNER_MJS).expect("write cdp runner");

    let screenshot_dir = if expect_live_llm {
        std::env::var("OPEN_ESTATE_SAMPLE_SHOTS_DIR").unwrap_or_default()
    } else {
        std::env::var("OPEN_ESTATE_SAMPLE_DET_SHOTS_DIR").unwrap_or_default()
    };
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&node_exec);
        cmd.arg(&script_path)
            .env("CHROMIUM_EXEC", &chromium_exec)
            .env("BASE_URL", &srv.base_url)
            .env("PRIMARY_SLUG", common::SYNTH_PRIMARY_SLUG)
            .env("SINGLE_PDF_PATH", &lakes_pdf)
            .env("ZIP_PACKET_PATH", &zip_path)
            .env("BOB_EMAIL", bob_email)
            .env("EXPECT_LIVE_LLM", if expect_live_llm { "1" } else { "0" })
            .env("USER_DATA_DIR", &user_data_dir);
        if !screenshot_dir.trim().is_empty() {
            cmd.env("OPEN_ESTATE_SAMPLE_SHOTS_DIR", screenshot_dir.trim());
        } else {
            cmd.env_remove("OPEN_ESTATE_SAMPLE_SHOTS_DIR");
        }
        cmd.output().expect("spawn node CDP E2E runner")
    })
    .await
    .expect("join CDP E2E task");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }

    let _ = std::fs::remove_dir_all(&work_dir);

    assert!(
        output.status.success(),
        "Real sample Chromium E2E failed (exit {:?})\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
        output.status.code()
    );
    assert!(stdout.contains("ALL REAL SAMPLE PACKET BROWSER E2E CHECKS PASSED"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_sample_packet_browser_e2e() {
    run_real_sample_browser_suite(EnvConfig::from_map(HashMap::new()), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_keyed_real_sample_packet_llm_chromium_e2e() {
    let base_env = load_live_llm_env();
    run_real_sample_browser_suite(base_env, true).await;
}
