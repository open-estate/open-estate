//! Modular Admin Console Settings & AI Prompts API + UI (`admin_settings` tab).
//!
//! Exposes `/api/admin/settings*` routes guarded by operator authentication, CSRF validation,
//! key/value allowlist validation, and immutable audit logging, plus the HTML/JS tab panel
//! mounted cleanly into `/admin`.

use crate::db::{
    is_allowed_admin_setting_key, validate_admin_setting_value, MAX_CHATBOX_MAX_WORDS,
    MAX_PROMPT_SETTING_CHARS, MIN_CHATBOX_MAX_WORDS, SETTING_CHATBOX_MAX_WORDS,
};
use crate::guardrails::{enforce_allowed_body_keys, GuardrailError};
use crate::web::admin_console::{
    admin_no_store_json, log_admin_action, require_admin, require_admin_mutation,
};
use crate::web::http_util::{map_anyhow, map_guardrail, parse_json_body};
use crate::web::server::AppState;
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, Query, State},
    http::{Extensions, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn admin_settings_routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/settings", get(handle_admin_settings_list))
        .route(
            "/api/admin/settings/:key",
            get(handle_admin_setting_get)
                .put(handle_admin_setting_update)
                .patch(handle_admin_setting_update)
                .post(handle_admin_setting_update)
                .delete(handle_admin_setting_reset),
        )
        .route(
            "/api/admin/settings/:key/reset",
            post(handle_admin_setting_reset),
        )
}

fn extract_setting_value_from_json(body: &Value) -> Option<String> {
    let raw = body
        .get("value")
        .or_else(|| body.get("customValue"))
        .or_else(|| body.get("settingValue"))?;
    match raw {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn setting_validation_error(code: &'static str, msg: impl Into<String>) -> Response {
    admin_no_store_json(
        StatusCode::BAD_REQUEST,
        json!({ "ok": false, "code": code, "error": msg.into() }),
    )
}

pub async fn handle_admin_settings_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let category = params
        .get("category")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match state.db.list_admin_settings(category).await {
        Ok(items) => admin_no_store_json(
            StatusCode::OK,
            json!({
                "ok": true,
                "total": items.len(),
                "items": items,
                "limits": {
                    "minChatboxMaxWords": MIN_CHATBOX_MAX_WORDS,
                    "maxChatboxMaxWords": MAX_CHATBOX_MAX_WORDS,
                    "maxPromptChars": MAX_PROMPT_SETTING_CHARS,
                },
            }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_setting_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(key): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    match state.db.get_admin_setting(&key).await {
        Ok(Some(setting)) => {
            admin_no_store_json(StatusCode::OK, json!({ "ok": true, "setting": setting }))
        }
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "code": "SETTING_NOT_FOUND",
                "error": "Admin setting key not found.",
            }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_setting_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(key): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !is_allowed_admin_setting_key(&key) {
        return setting_validation_error(
            "UNSUPPORTED_SETTING_KEY",
            format!("Unsupported or disallowed admin setting key: {key}"),
        );
    }
    let body = match parse_json_body(raw_body, 4, 16) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body.is_object() {
        return map_guardrail(GuardrailError::new(
            "INVALID_JSON_BODY",
            "Request body must be a JSON object.",
            400,
        ));
    }
    if let Err(ge) = enforce_allowed_body_keys(&body, &["value", "customValue", "settingValue"]) {
        return map_guardrail(ge);
    }
    let Some(raw_val) = extract_setting_value_from_json(&body) else {
        return setting_validation_error(
            "INVALID_SETTING_VALUE",
            "JSON body must provide a string or integer \"value\".",
        );
    };
    let clean_val = match validate_admin_setting_value(&key, &raw_val) {
        Ok(v) => v,
        Err(err) => return setting_validation_error("INVALID_SETTING_VALUE", err.to_string()),
    };
    match state
        .db
        .upsert_admin_setting(&key, &clean_val, Some(&auth.actor))
        .await
    {
        Ok(setting) => {
            log_admin_action(
                &state,
                &auth,
                "setting.update",
                "admin_setting",
                &setting.key,
                &json!({
                    "category": setting.category,
                    "isCustomized": setting.is_customized,
                    "valueChars": setting.effective_value.chars().count(),
                }),
            )
            .await;
            admin_no_store_json(StatusCode::OK, json!({ "ok": true, "setting": setting }))
        }
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_setting_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(key): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !is_allowed_admin_setting_key(&key) {
        return setting_validation_error(
            "UNSUPPORTED_SETTING_KEY",
            format!("Unsupported or disallowed admin setting key: {key}"),
        );
    }
    match state.db.reset_admin_setting(&key).await {
        Ok(setting) => {
            log_admin_action(
                &state,
                &auth,
                "setting.reset",
                "admin_setting",
                &setting.key,
                &json!({ "category": setting.category }),
            )
            .await;
            admin_no_store_json(StatusCode::OK, json!({ "ok": true, "setting": setting }))
        }
        Err(e) => map_anyhow(e),
    }
}

pub(crate) fn admin_settings_tab_button_html() -> &'static str {
    r#"<button type="button" class="nav-tab" data-tab="settings" data-sub="Configure buyer chatbox word cap, AI system prompts, and plain-language style rules" role="tab" aria-selected="false">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M19.14 12.94c.04-.31.06-.63.06-.94s-.02-.63-.06-.94l2.03-1.58a.49.49 0 0 0 .12-.61l-1.92-3.32a.49.49 0 0 0-.59-.22l-2.39.96a7.03 7.03 0 0 0-1.62-.94l-.36-2.54a.484.484 0 0 0-.48-.41h-3.84c-.24 0-.44.17-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96a.49.49 0 0 0-.59.22L2.74 8.87a.48.48 0 0 0 .12.61l2.03 1.58c-.04.31-.06.63-.06.94s.02.63.06.94l-2.03 1.58a.49.49 0 0 0-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32a.49.49 0 0 0-.12-.61l-2.01-1.58zM12 15.6c-1.98 0-3.6-1.62-3.6-3.6s1.62-3.6 3.6-3.6 3.6 1.62 3.6 3.6-1.62 3.6-3.6 3.6z"/></svg><span class="nav-lbl">Settings &amp; Prompts</span></span>
        <span class="tab-cnt" id="cntSettings">6</span>
      </button>"#
}

pub(crate) fn admin_settings_panel_html() -> &'static str {
    r#"  <!-- TAB 6: SETTINGS & AI PROMPTS -->
  <section class="panel" id="panel-settings" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <span class="pill pill-info" id="settingsCustomSummary">0 customized · 6 default</span>
        <span style="font-size:12px;color:var(--muted)">Changes persist in SQLite/MySQL and apply immediately to live Q&amp;A and AI generators. Core safety constitution remains locked.</span>
      </div>
      <div class="toolbar-right">
        <button type="button" class="btn sm" id="reloadSettingsBtn">Reload Settings</button>
      </div>
    </div>
    <div id="settingsStatusBanner" class="card" style="display:none;padding:10px 14px;font-size:12.5px"></div>
    <div id="settingsList" style="display:flex;flex-direction:column;gap:14px">
      <div class="card"><div class="empty-state">Select <strong>Settings &amp; Prompts</strong> to load active prompt templates and word-limit controls.</div></div>
    </div>
  </section>"#
}

pub(crate) fn admin_settings_script_js() -> String {
    let tpl = r#"
  let settingsData = [];
  const SETTINGS_MAX_WORDS_KEY = '__MAX_WORDS_KEY__';
  const SETTINGS_MIN_WORDS = __MIN_WORDS__;
  const SETTINGS_MAX_WORDS = __MAX_WORDS__;
  const SETTINGS_MAX_CHARS = __MAX_CHARS__;

  function setSettingsBanner(msg, isErr = false) {
    const el = document.getElementById('settingsStatusBanner');
    if (!el) return;
    if (!msg) { el.style.display = 'none'; el.textContent = ''; return; }
    el.style.display = 'block';
    el.style.borderColor = isErr ? 'var(--bad-line)' : 'var(--good-line)';
    el.style.color = isErr ? 'var(--bad-ink)' : 'var(--good-ink)';
    el.textContent = msg;
  }

  function renderAdminSettings() {
    const list = document.getElementById('settingsList');
    const summary = document.getElementById('settingsCustomSummary');
    const cnt = document.getElementById('cntSettings');
    if (!list) return;
    const total = settingsData.length;
    const customCount = settingsData.filter(s => !!s.isCustomized).length;
    if (cnt) cnt.textContent = String(total);
    if (summary) summary.textContent = `${customCount} customized · ${Math.max(0, total - customCount)} default`;
    if (!total) {
      list.innerHTML = '<div class="card"><div class="empty-state">No configurable prompt settings registered.</div></div>';
      return;
    }
    list.innerHTML = settingsData.map((s, idx) => {
      const isNum = s.key === SETTINGS_MAX_WORDS_KEY;
      const badge = s.isCustomized
        ? '<span class="pill pill-brand setting-badge" data-status="customized">Customized</span>'
        : '<span class="pill pill-muted setting-badge" data-status="default">Default</span>';
      const meta = s.updatedAt
        ? `Updated ${esc(fmtShortTs(s.updatedAt))}${s.updatedBy ? ` by ${esc(s.updatedBy)}` : ''}`
        : 'Using built-in factory default';
      const inp = isNum
        ? `<div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap">
             <input type="number" class="inp mono setting-input" id="settingInp_${idx}" data-setting-input="${esc(s.key)}" min="${SETTINGS_MIN_WORDS}" max="${SETTINGS_MAX_WORDS}" step="1" value="${esc(s.effectiveValue)}" aria-label="${esc(s.label)}" style="width:140px">
             <span style="font-size:12px;color:var(--muted)">Allowed range: ${SETTINGS_MIN_WORDS}–${SETTINGS_MAX_WORDS} words (default ${esc(s.defaultValue)})</span>
           </div>`
        : `<textarea class="inp code setting-input" id="settingInp_${idx}" data-setting-input="${esc(s.key)}" rows="6" spellcheck="false" maxlength="${SETTINGS_MAX_CHARS}" aria-label="${esc(s.label)}" style="min-height:140px">${esc(s.effectiveValue)}</textarea>`;
      return `<article class="card setting-card" data-setting-key="${esc(s.key)}" style="margin-bottom:0">
        <div class="card-hdr" style="margin-bottom:8px">
          <div>
            <h3 class="card-title">${esc(s.label)}</h3>
            <div class="mono" style="font-size:11.5px;color:var(--muted);margin-top:2px">${esc(s.key)} · <span data-meta>${meta}</span></div>
          </div>
          ${badge}
        </div>
        <p style="margin:0 0 10px;font-size:12.5px;color:var(--ink-2)">${esc(s.description)}</p>
        ${inp}
        <details style="margin-top:8px">
          <summary style="cursor:pointer;font-size:12px;color:var(--muted)">View Built-in Default</summary>
          <pre class="json-view" style="margin-top:6px;white-space:pre-wrap;max-height:180px">${esc(s.defaultValue)}</pre>
        </details>
        <div class="ext-foot" style="margin-top:12px">
          <span class="mono setting-chars" style="font-size:11.5px;color:var(--muted)">${String((s.effectiveValue || '').length)} chars</span>
          <div style="display:flex;gap:8px;align-items:center">
            <button type="button" class="btn sm setting-reset-btn" data-key="${esc(s.key)}" ${s.isCustomized ? '' : 'disabled'}>Reset to Default</button>
            <button type="button" class="btn sm primary setting-save-btn" data-key="${esc(s.key)}">Save</button>
          </div>
        </div>
      </article>`;
    }).join('');
  }

  async function loadAdminSettings() {
    setSettingsBanner('');
    try {
      const res = await apiFetch('/api/admin/settings');
      settingsData = Array.isArray(res.items) ? res.items : [];
      renderAdminSettings();
    } catch (err) {
      setSettingsBanner(err.message, true);
      showToast(err.message, true);
    }
  }

  const settingsListEl = document.getElementById('settingsList');
  if (settingsListEl) {
    settingsListEl.addEventListener('input', (e) => {
      const inp = e.target.closest('.setting-input');
      const card = inp && inp.closest('.setting-card');
      const charsEl = card && card.querySelector('.setting-chars');
      if (charsEl && inp) charsEl.textContent = `${String(inp.value || '').length} chars`;
    });
    settingsListEl.addEventListener('click', async (e) => {
      const saveBtn = e.target.closest('.setting-save-btn');
      if (saveBtn) {
        const key = saveBtn.dataset.key;
        const card = saveBtn.closest('.setting-card');
        const inp = card && card.querySelector('.setting-input');
        if (!key || !inp) return;
        saveBtn.disabled = true;
        setSettingsBanner('');
        try {
          const res = await apiFetch(`/api/admin/settings/${encodeURIComponent(key)}`, {
            method: 'PUT',
            body: { value: String(inp.value || '') }
          });
          const updated = res.setting;
          if (updated) {
            settingsData = settingsData.map(item => item.key === key ? updated : item);
            renderAdminSettings();
          }
          showToast(`Saved setting '${key}'.`);
          setSettingsBanner(`Saved '${key}'.`);
        } catch (err) {
          setSettingsBanner(err.message, true);
          showToast(err.message, true);
          saveBtn.disabled = false;
        }
        return;
      }
      const resetBtn = e.target.closest('.setting-reset-btn');
      if (resetBtn) {
        const key = resetBtn.dataset.key;
        if (!key) return;
        resetBtn.disabled = true;
        setSettingsBanner('');
        try {
          const res = await apiFetch(`/api/admin/settings/${encodeURIComponent(key)}/reset`, {
            method: 'POST',
            body: {}
          });
          const updated = res.setting;
          if (updated) {
            settingsData = settingsData.map(item => item.key === key ? updated : item);
            renderAdminSettings();
          }
          showToast(`Reset '${key}' to default.`);
          setSettingsBanner(`Reset '${key}' to built-in default.`);
        } catch (err) {
          setSettingsBanner(err.message, true);
          showToast(err.message, true);
          resetBtn.disabled = false;
        }
      }
    });
  }

  document.querySelectorAll('.nav-tab[data-tab="settings"]').forEach((btn) => {
    btn.addEventListener('click', () => { loadAdminSettings(); });
  });
  const reloadSettingsBtn = document.getElementById('reloadSettingsBtn');
  if (reloadSettingsBtn) reloadSettingsBtn.addEventListener('click', () => { loadAdminSettings(); });
  const refreshAllForSettings = document.getElementById('refreshAllBtn');
  if (refreshAllForSettings) {
    refreshAllForSettings.addEventListener('click', () => {
      if (activeTab === 'settings') loadAdminSettings();
    });
  }
"#;
    tpl.replace("__MAX_WORDS_KEY__", SETTING_CHATBOX_MAX_WORDS)
        .replace("__MIN_WORDS__", &MIN_CHATBOX_MAX_WORDS.to_string())
        .replace("__MAX_WORDS__", &MAX_CHATBOX_MAX_WORDS.to_string())
        .replace("__MAX_CHARS__", &MAX_PROMPT_SETTING_CHARS.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_setting_value_accepts_string_or_integer_and_fragments_embed_cleanly() {
        assert_eq!(
            extract_setting_value_from_json(&json!({ "value": 90 })).as_deref(),
            Some("90")
        );
        assert_eq!(
            extract_setting_value_from_json(&json!({ "customValue": "concise reply" })).as_deref(),
            Some("concise reply")
        );
        assert!(extract_setting_value_from_json(&json!({ "value": true })).is_none());
        let script = admin_settings_script_js();
        assert!(!script.contains("</script>"));
        assert!(script.contains("/api/admin/settings"));
        assert!(admin_settings_tab_button_html().contains("data-tab=\"settings\""));
        assert!(admin_settings_panel_html().contains("id=\"panel-settings\""));
    }
}
