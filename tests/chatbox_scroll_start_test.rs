//! Real Headless Chromium regression test verifying that all three AI chatboxes
//! (`#chatLog` in report drawer, `#bcChatLog` in Buyer Copilot, `#psChatLog` in PDF Studio)
//! scroll to the BEGINNING of the AI's response message rather than the bottom when answered.

#![recursion_limit = "512"]

mod common;

use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::ingest_report_into_db;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::net::TcpListener;
use uuid::Uuid;

struct ChatScrollHarness {
    base_url: String,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ChatScrollHarness {
    async fn start(primary_rep: Value) -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-chat-scroll-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        let http_cache_dir = temp_dir.join("http_cache");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
        std::fs::create_dir_all(&http_cache_dir).expect("create temp http cache dir");

        let db_path = temp_dir.join("openestate_chat_scroll.sqlite");
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

        let http = HttpClient::from_env(&env, Some(http_cache_dir));
        let db = DbStore::connect(&env).await.expect("connect sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let primary_html = render_report_html(&primary_rep);
        db.put_cached_report(common::SYNTH_PRIMARY_SLUG, &primary_rep, Some(&primary_html))
            .await
            .expect("seed primary cached report");
        ingest_report_into_db(&db, None, &env, &primary_rep, false)
            .await
            .expect("ingest primary report into db");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir,
            Some(temp_dir.join("datasets")),
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

impl Drop for ChatScrollHarness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
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
                    let rev_str = name.strip_prefix("chromium-")?;
                    let rev: u64 = rev_str.parse().unwrap_or(0);
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

    panic!("Headless Chromium binary not found");
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
    panic!("Node executable with WebSocket is required");
}

const CHATBOX_SCROLL_CDP_MJS: &str = r###"
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';

const CHROMIUM_EXEC = process.env.CHROMIUM_EXEC;
const BASE_URL = process.env.BASE_URL.replace(/\/+$/, '');
const PRIMARY_SLUG = process.env.PRIMARY_SLUG;
const USER_DATA_DIR = process.env.USER_DATA_DIR;

class CdpConnection {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 0;
    this.pending = new Map();
    this.listeners = new Map();
    ws.addEventListener('message', async (ev) => {
      const text = typeof ev.data === 'string'
        ? ev.data
        : Buffer.from(await ev.data.arrayBuffer()).toString('utf8');
      const msg = JSON.parse(text);
      if (msg.id != null) {
        const slot = this.pending.get(msg.id);
        if (slot) {
          this.pending.delete(msg.id);
          clearTimeout(slot.timer);
          if (msg.error) slot.reject(new Error(`${slot.method}: ${msg.error.message}`));
          else slot.resolve(msg.result || {});
        }
        return;
      }
      if (msg.method) {
        const key = `${msg.sessionId || 'browser'}::${msg.method}`;
        const list = this.listeners.get(key);
        if (list) {
          for (const fn of [...list]) {
            try { fn(msg.params || {}); } catch (_) {}
          }
        }
      }
    });
  }

  send(method, params = {}, sessionId = undefined, timeoutMs = 25000) {
    const id = ++this.nextId;
    const payload = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP ${method} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, method, timer });
      this.ws.send(JSON.stringify(payload));
    });
  }

  on(sessionId, method, handler) {
    const key = `${sessionId || 'browser'}::${method}`;
    let arr = this.listeners.get(key);
    if (!arr) { arr = []; this.listeners.set(key, arr); }
    arr.push(handler);
    return () => {
      const idx = arr.indexOf(handler);
      if (idx >= 0) arr.splice(idx, 1);
    };
  }
}

class CdpPage {
  constructor(cdp, sessionId, browserContextId) {
    this.cdp = cdp;
    this.sessionId = sessionId;
    this.browserContextId = browserContextId;
    this.pageErrors = [];
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
      this.pageErrors.push(String((det.exception && det.exception.description) || det.text || 'exception'));
    });
  }

  async init() {
    await this.cdp.send('Page.enable', {}, this.sessionId);
    await this.cdp.send('Runtime.enable', {}, this.sessionId);
    await this.cdp.send('DOM.enable', {}, this.sessionId);
    await this.cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1080,
      height: 760,
      deviceScaleFactor: 1,
      mobile: false,
    }, this.sessionId);
    await this.cdp.send('Fetch.enable', {
      patterns: [{ urlPattern: 'https://*', requestStage: 'Request' }],
    }, this.sessionId);
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
      throw new Error((det.exception && det.exception.description) || det.text || 'eval error');
    }
    return res.result ? res.result.value : undefined;
  }

  async waitFor(fn, opts = {}, ...args) {
    const timeout = opts.timeout ?? 20000;
    const deadline = Date.now() + timeout;
    let lastErr = null;
    while (Date.now() < deadline) {
      try {
        const val = await this.eval(fn, ...args);
        if (val) return val;
        lastErr = null;
      } catch (e) {
        lastErr = e;
      }
      await new Promise((r) => setTimeout(r, 40));
    }
    throw new Error(`Timed out waiting for ${opts.desc || 'condition'}${lastErr ? `: ${lastErr.message}` : ''}`);
  }

  async goto(url) {
    let loaded = false;
    const unsub = this.cdp.on(this.sessionId, 'Page.loadEventFired', () => { loaded = true; });
    try {
      await this.cdp.send('Page.navigate', { url }, this.sessionId);
      const deadline = Date.now() + 20000;
      while (!loaded && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 30));
      }
      await this.waitFor(() => document.readyState === 'complete', { desc: 'readyState complete' });
    } finally {
      unsub();
    }
  }

  async fill(selector, value) {
    await this.waitFor((sel) => !!document.querySelector(sel), { desc: `input ${selector}` }, selector);
    await this.eval((sel, val) => {
      const el = document.querySelector(sel);
      el.focus();
      const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const desc = Object.getOwnPropertyDescriptor(proto, 'value');
      if (desc && desc.set) desc.set.call(el, String(val));
      else el.value = String(val);
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    }, selector, String(value));
  }

  async submitForm(formSelector) {
    await this.eval((sel) => {
      const f = document.querySelector(sel);
      f.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
    }, formSelector);
  }
}

async function launchChromium() {
  const proc = spawn(CHROMIUM_EXEC, [
    '--headless=new',
    '--no-sandbox',
    '--disable-setuid-sandbox',
    '--disable-dev-shm-usage',
    '--disable-gpu',
    '--user-agent=Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36',
    '--remote-debugging-port=0',
    `--user-data-dir=${USER_DATA_DIR}`,
    'about:blank',
  ], { stdio: ['ignore', 'pipe', 'pipe'] });

  let wsUrl = null;
  const deadline = Date.now() + 15000;
  let logs = '';
  const onChunk = (chunk) => {
    logs += chunk.toString('utf8');
    const m = logs.match(/DevTools listening on (ws:\/\/[^\s]+)/);
    if (m) wsUrl = m[1];
  };
  proc.stdout.on('data', onChunk);
  proc.stderr.on('data', onChunk);
  while (!wsUrl && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 40));
  }
  if (!wsUrl) throw new Error(`Chromium WS URL not emitted:\n${logs}`);
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => {
    ws.addEventListener('open', res, { once: true });
    ws.addEventListener('error', rej, { once: true });
  });
  return { proc, ws, cdp: new CdpConnection(ws) };
}

function assertAlignedToMessageStart(metrics, label) {
  assert(metrics, `${label}: missing metrics`);
  assert(
    metrics.msgHeight > metrics.clientHeight,
    `${label}: expected AI reply taller than chat viewport so scroll position is unambiguous (msgHeight=${metrics.msgHeight}, clientHeight=${metrics.clientHeight}, text=${JSON.stringify(metrics.text)})`
  );
  const maxScrollTop = metrics.scrollHeight - metrics.clientHeight;
  assert(
    metrics.scrollTop > 0 && metrics.scrollTop < maxScrollTop - 15,
    `${label}: expected scrollTop between 0 and maxScrollTop-15, got scrollTop=${metrics.scrollTop}, maxScrollTop=${maxScrollTop}`
  );
  const expectedTop = metrics.logTop + metrics.clientTop + metrics.padTop;
  const delta = Math.abs(metrics.msgTop - expectedTop);
  assert(
    delta <= 4,
    `${label}: expected beginning of AI reply aligned near top of chatbox (msgTop=${metrics.msgTop}, expectedTop=${expectedTop}, delta=${delta})`
  );
  assert(
    metrics.msgBottom > metrics.logBottom,
    `${label}: expected bottom of overflowing AI reply below visible chat viewport (msgBottom=${metrics.msgBottom}, logBottom=${metrics.logBottom})`
  );
}

const MULTI_DOMAIN_QUESTION = 'Summarize valuation, monthly PITI payment, HOA dues, permits and inspection condition, flood and seismic hazards, transfer tax closing customs, and remodel ROI yield for 100 Synthparcelalpha Ln, San Jose, CA 95118.';

async function main() {
  const { proc, ws, cdp } = await launchChromium();
  try {
    const { browserContextId } = await cdp.send('Target.createBrowserContext', {});
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank', browserContextId });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const page = new CdpPage(cdp, sessionId, browserContextId);
    await page.init();

    // -------------------------------------------------------------------------
    // 1. Report Drawer Chatbox (#chatLog)
    // -------------------------------------------------------------------------
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}`);
    await page.waitFor(() => typeof window.openOpenEstateChat === 'function' && window.OpenEstateRole, {
      desc: 'Report page chat APIs ready',
    });
    await page.eval(() => {
      window.OpenEstateRole.setRole('buyer');
      window.openOpenEstateChat();
    });
    await page.waitFor(() => document.getElementById('openEstateDrawer')?.classList.contains('open'), {
      desc: '#openEstateDrawer open',
    });

    const initialDrawerScroll = await page.eval(() => document.getElementById('chatLog')?.scrollTop ?? -1);
    assert.equal(initialDrawerScroll, 0, 'Empty #chatLog must open at scrollTop=0');

    // First short question to seed prior conversation scroll offset
    await page.fill('#chatIn', 'What is the HOA monthly fee?');
    await page.submitForm('#chatForm');
    await page.waitFor(() => document.querySelectorAll('#chatLog [data-chat-entry="assistant"]').length >= 1, {
      desc: '1st #chatLog reply',
    });

    // Long multi-domain question -> reply overflows #chatLog
    await page.fill('#chatIn', MULTI_DOMAIN_QUESTION);
    await page.submitForm('#chatForm');
    const drawerMetrics = await page.waitFor(() => {
      const log = document.getElementById('chatLog');
      const msgs = log ? log.querySelectorAll('[data-chat-entry="assistant"]') : [];
      if (msgs.length < 2) return null;
      const last = msgs[msgs.length - 1];
      const lr = log.getBoundingClientRect();
      const mr = last.getBoundingClientRect();
      const padTop = parseFloat(getComputedStyle(log).paddingTop) || 0;
      return {
        scrollTop: log.scrollTop,
        scrollHeight: log.scrollHeight,
        clientHeight: log.clientHeight,
        clientTop: log.clientTop || 0,
        padTop,
        logTop: lr.top,
        logBottom: lr.bottom,
        msgTop: mr.top,
        msgBottom: mr.bottom,
        msgHeight: mr.height,
        text: last.innerText,
      };
    }, { desc: '2nd #chatLog long reply' });
    assertAlignedToMessageStart(drawerMetrics, '#chatLog (2nd long reply)');

    // Reload page and reopen drawer with restored sessionStorage history -> still aligned to beginning of last AI reply
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}`);
    await page.waitFor(() => typeof window.openOpenEstateChat === 'function', { desc: 'reload ready' });
    await page.eval(() => window.openOpenEstateChat());
    const reopenedMetrics = await page.waitFor(() => {
      const log = document.getElementById('chatLog');
      const msgs = log ? log.querySelectorAll('[data-chat-entry="assistant"]') : [];
      if (msgs.length < 2) return null;
      const last = msgs[msgs.length - 1];
      const lr = log.getBoundingClientRect();
      const mr = last.getBoundingClientRect();
      if (lr.height <= 0) return null;
      const padTop = parseFloat(getComputedStyle(log).paddingTop) || 0;
      return {
        scrollTop: log.scrollTop,
        scrollHeight: log.scrollHeight,
        clientHeight: log.clientHeight,
        clientTop: log.clientTop || 0,
        padTop,
        logTop: lr.top,
        logBottom: lr.bottom,
        msgTop: mr.top,
        msgBottom: mr.bottom,
        msgHeight: mr.height,
      };
    }, { desc: 'reopened #chatLog restored session' });
    assertAlignedToMessageStart(reopenedMetrics, '#chatLog (reopened session)');

    // Short follow-up question after long reply -> short reply must fit cleanly at bottom (start & end both visible)
    await page.fill('#chatIn', 'Write a python script');
    await page.submitForm('#chatForm');
    const shortReplyMetrics = await page.waitFor(() => {
      const log = document.getElementById('chatLog');
      const msgs = log ? log.querySelectorAll('[data-chat-entry="assistant"]') : [];
      if (msgs.length < 3) return null;
      const last = msgs[msgs.length - 1];
      const lr = log.getBoundingClientRect();
      const mr = last.getBoundingClientRect();
      return {
        logTop: lr.top,
        logBottom: lr.bottom,
        msgTop: mr.top,
        msgBottom: mr.bottom,
        msgHeight: mr.height,
        clientHeight: log.clientHeight,
      };
    }, { desc: '3rd #chatLog short reply' });
    assert(shortReplyMetrics.msgHeight < shortReplyMetrics.clientHeight, 'Expected short reply shorter than #chatLog');
    assert(
      shortReplyMetrics.msgTop >= shortReplyMetrics.logTop - 1 && shortReplyMetrics.msgBottom <= shortReplyMetrics.logBottom + 1,
      `Short reply must be completely visible inside #chatLog: ${JSON.stringify(shortReplyMetrics)}`
    );
    await page.eval(() => window.closeOpenEstateChat?.());

    // -------------------------------------------------------------------------
    // 2. Buyer Copilot Modal Chatbox (#bcChatLog)
    // -------------------------------------------------------------------------
    await page.eval((slug) => {
      window.OpenEstateBuyerCopilot.openModal(slug, '', { initialFn: 'chat', activeFn: 'chat' });
    }, PRIMARY_SLUG);
    await page.waitFor(() => !!document.getElementById('bcChatLog'), { desc: '#bcChatLog mounted' });

    await page.fill('#bcChatInput', 'What is the HOA monthly fee?');
    await page.submitForm('#bcChatForm');
    await page.waitFor(() => document.querySelectorAll('#bcChatLog [data-bc-chat-entry="assistant"]').length >= 1, {
      desc: '1st #bcChatLog reply',
    });

    await page.fill('#bcChatInput', MULTI_DOMAIN_QUESTION);
    await page.submitForm('#bcChatForm');
    const bcMetrics = await page.waitFor(() => {
      const log = document.getElementById('bcChatLog');
      const msgs = log ? log.querySelectorAll('[data-bc-chat-entry="assistant"]') : [];
      if (msgs.length < 2) return null;
      const last = msgs[msgs.length - 1];
      const lr = log.getBoundingClientRect();
      const mr = last.getBoundingClientRect();
      const padTop = parseFloat(getComputedStyle(log).paddingTop) || 0;
      return {
        scrollTop: log.scrollTop,
        scrollHeight: log.scrollHeight,
        clientHeight: log.clientHeight,
        clientTop: log.clientTop || 0,
        padTop,
        logTop: lr.top,
        logBottom: lr.bottom,
        msgTop: mr.top,
        msgBottom: mr.bottom,
        msgHeight: mr.height,
      };
    }, { desc: '2nd #bcChatLog long reply' });
    assertAlignedToMessageStart(bcMetrics, '#bcChatLog (2nd long reply)');
    await page.eval(() => {
      document.getElementById('bcReportModalBackdrop')?.classList.remove('on');
    });

    // -------------------------------------------------------------------------
    // 3. PDF Studio AI Side Chat (#psChatLog)
    // -------------------------------------------------------------------------
    await page.eval(async (slug) => {
      await window.OpenEstatePdfStudio.openStudio({
        slug,
        address: '100 Synthparcelalpha Ln, San Jose, CA 95118',
        role: 'buyer',
        initialTab: 'chat',
      });
    }, PRIMARY_SLUG);
    await page.waitFor(() => {
      const log = document.getElementById('psChatLog');
      return log && log.getBoundingClientRect().height > 40;
    }, { desc: '#psChatLog visible' });

    await page.fill('#psChatInput', 'What is the HOA monthly fee?');
    await page.submitForm('#psChatForm');
    await page.waitFor(() => document.querySelectorAll('#psChatLog [data-ps-chat-entry="assistant"]').length >= 1, {
      desc: '1st #psChatLog reply',
    });

    await page.fill('#psChatInput', MULTI_DOMAIN_QUESTION);
    await page.submitForm('#psChatForm');
    const psMetrics = await page.waitFor(() => {
      const log = document.getElementById('psChatLog');
      const msgs = log ? log.querySelectorAll('[data-ps-chat-entry="assistant"]') : [];
      if (msgs.length < 2) return null;
      const last = msgs[msgs.length - 1];
      const lr = log.getBoundingClientRect();
      const mr = last.getBoundingClientRect();
      const padTop = parseFloat(getComputedStyle(log).paddingTop) || 0;
      return {
        scrollTop: log.scrollTop,
        scrollHeight: log.scrollHeight,
        clientHeight: log.clientHeight,
        clientTop: log.clientTop || 0,
        padTop,
        logTop: lr.top,
        logBottom: lr.bottom,
        msgTop: mr.top,
        msgBottom: mr.bottom,
        msgHeight: mr.height,
      };
    }, { desc: '2nd #psChatLog long reply' });
    assertAlignedToMessageStart(psMetrics, '#psChatLog (2nd long reply)');

    assert.equal(page.pageErrors.length, 0, `Unexpected page errors: ${page.pageErrors.join('; ')}`);
    await page.close();
    console.log('ALL_CHATBOX_SCROLL_START_CHECKS_PASSED', JSON.stringify({
      drawer: { scrollTop: drawerMetrics.scrollTop, msgTop: drawerMetrics.msgTop, expectedTop: drawerMetrics.logTop + drawerMetrics.clientTop + drawerMetrics.padTop },
      copilot: { scrollTop: bcMetrics.scrollTop, msgTop: bcMetrics.msgTop, expectedTop: bcMetrics.logTop + bcMetrics.clientTop + bcMetrics.padTop },
      pdfStudio: { scrollTop: psMetrics.scrollTop, msgTop: psMetrics.msgTop, expectedTop: psMetrics.logTop + psMetrics.clientTop + psMetrics.padTop },
    }));
  } finally {
    try { await cdp.send('Browser.close'); } catch (_) {}
    try { ws.close(); } catch (_) {}
    try { proc.kill('SIGKILL'); } catch (_) {}
  }
}

main().catch((err) => {
  console.error('CHATBOX_SCROLL_START_TEST_FAILED:', err);
  process.exit(1);
});
"###;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_chatboxes_scroll_to_beginning_of_ai_response_in_chromium() {
    let chromium_exec = discover_chromium_executable();
    let node_exec = discover_node_executable();

    let mut primary_rep = common::synth_report_primary();
    normalize_stored_report(&mut primary_rep);
    primary_rep["facts"]["forSale"] = json!(true);
    primary_rep["facts"]["listingStatus"] = json!("Active");
    primary_rep["facts"]["listPrice"] = json!(1_620_000);
    primary_rep["facts"]["hoaMonthly"] = json!(285);
    primary_rep["hoaProfile"] = json!({
        "hasHoa": true,
        "hoaMonthly": 285,
        "hoaName": "Synthparcel Homeowners Association",
        "reserveFundedPct": 62.5
    });

    let srv = ChatScrollHarness::start(primary_rep).await;

    let script_path = srv.temp_dir.join("chatbox_scroll_cdp.mjs");
    std::fs::write(&script_path, CHATBOX_SCROLL_CDP_MJS).expect("write chatbox_scroll_cdp.mjs");

    let user_data_dir = srv.temp_dir.join("chrome_profile");
    std::fs::create_dir_all(&user_data_dir).expect("create chrome profile dir");

    let base_url_clone = srv.base_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(&node_exec)
            .arg(&script_path)
            .env("CHROMIUM_EXEC", &chromium_exec)
            .env("BASE_URL", &base_url_clone)
            .env("PRIMARY_SLUG", common::SYNTH_PRIMARY_SLUG)
            .env("USER_DATA_DIR", &user_data_dir)
            .output()
            .expect("spawn node chatbox scroll CDP runner")
    })
    .await
    .expect("join blocking CDP task");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }

    assert!(
        output.status.success() && stdout.contains("ALL_CHATBOX_SCROLL_START_CHECKS_PASSED"),
        "Chatbox scroll-start Chromium test failed (exit {:?})\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
        output.status.code()
    );
}
