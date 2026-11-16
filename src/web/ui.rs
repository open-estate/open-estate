// Server-rendered HTML views powered by `minijinja` + `html_escape` + `fluent-templates`
// and client-side MIT `i18next` + `Intl` formatting:
// - 4-Tab Home, Faceted Shop Homes, AI Home Finder & Area ZIP Alerts (`/`, `/shop`, `/match`)
// - Live Async Report Progress Page (`/report?address=...`)
// - One-Click Unsubscribe Confirmation (`/api/subscriptions/unsubscribe`)

use crate::shared::{format_brand_wordmark_html, humanize_progress_step};
use crate::web::favicon::{logo_mark_img, FAVICON_HEAD_TAGS};
use crate::web::i18n::{
    bcp47_code, is_es, is_zh, resolve_lang_str, short_code, t, t_args, t_brand, t_brand_args,
    LANG_COOKIE_NAME,
};
use crate::web::render::{
    localize_i18n_json_for_brand, safe_inline_js, safe_inline_json, safe_open_estate_core_js,
    I18NEXT_JS, I18N_EN_JSON, I18N_ES_JSON, I18N_ZH_JSON,
};
use minijinja::{context, Environment};
use once_cell::sync::Lazy;

const HOME_TEMPLATE: &str = r###"<!doctype html><html lang="{{ html_lang }}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{ t_home_title }}</title>
{{ favicon_tags | safe }}
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;600;700;800&family=Noto+Serif+SC:wght@700;900&display=swap" rel="stylesheet">
<link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" crossorigin="">
<script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js" crossorigin=""></script>
<style>
*{box-sizing:border-box}body{margin:0;background:#f5f6f8;color:#111827;font:15px/1.55 Inter,system-ui,sans-serif}
.wrap{max-width:1060px;margin:0 auto;padding:28px 20px 64px;transition:max-width .18s ease}
.wrap.shop-wide{max-width:1260px}
.topbar{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-bottom:12px;flex-wrap:wrap}
.logo{display:inline-flex;align-items:center;gap:10px;font-weight:800;font-size:22px}
.logo .logo-mark{width:36px;height:36px;flex:0 0 36px;display:block}
.logo b{color:#0f766e}
.langsw{display:inline-flex;align-items:center;background:#fff;border:1px solid #d1d5db;border-radius:999px;padding:2px;gap:2px}
.langsw button{border:0;background:transparent;border-radius:999px;padding:4px 11px;font:700 12px Inter,sans-serif;color:#4b5563;cursor:pointer}
.langsw button.on{background:#0f766e;color:#fff;box-shadow:0 1px 3px rgba(15,118,110,.3)}
h1{font-size:32px;line-height:1.18;letter-spacing:-.75px;margin:0 0 7px}.sub{color:#4b5563;font-size:15px;line-height:1.5;max-width:860px;margin:0 0 15px}
.sbox{position:relative}#homeForm{display:flex;align-items:center;gap:10px;background:#fff;padding:8px 8px 8px 16px;border-radius:16px;border:1px solid #d1d5db;box-shadow:0 10px 30px -18px rgba(17,24,39,.2)}
#homeForm:focus-within{border-color:#0f766e;box-shadow:0 0 0 3px rgba(20,184,166,.18)}
#homeForm.invalid{border-color:#dc2626;box-shadow:0 0 0 3px rgba(220,38,38,.16)}
.home-pop{position:absolute;top:calc(100% + 6px);left:0;right:0;z-index:2000;display:flex;flex-direction:column;gap:6px;pointer-events:none}
.addr-err{display:none;pointer-events:auto;padding:9px 13px;border-radius:11px;background:#fef2f2;border:1px solid #fecaca;color:#991b1b;font-size:13.5px;font-weight:600;line-height:1.45;box-shadow:0 12px 28px -12px rgba(17,24,39,.22)}.addr-err.on{display:block}
.sico{color:#6b7280;display:grid;place-items:center}#homeInput{flex:1;border:0;padding:11px 6px;font:inherit;font-size:16px;color:#111827;background:transparent;outline:none;min-width:0}
.clr{border:0;background:#f3f4f6;color:#6b7280;width:26px;height:26px;border-radius:50%;font:700 15px/1 Inter,sans-serif;cursor:pointer;display:none;place-items:center;padding:0}.clr.on{display:grid}
button.go{border:0;background:#0f766e;color:#fff;font:700 15px Inter,sans-serif;padding:12px 22px;border-radius:11px;cursor:pointer;white-space:nowrap}
button.go:hover{background:#0d655e}
.ac{pointer-events:auto;background:#fff;border:1px solid #e5e7eb;border-radius:15px;box-shadow:0 18px 40px -14px rgba(17,24,39,.28);overflow:hidden;display:none}.ac.open{display:block}
.acempty{padding:11px 15px;font-size:13px;color:#6b7280}
.aci{display:flex;align-items:center;gap:12px;padding:11px 15px;cursor:pointer;border-bottom:1px solid #f3f4f6}.aci:hover,.aci.act{background:#effcf8}
.aci .pin{width:30px;height:30px;border-radius:9px;background:#f3f4f6;color:#4b5563;display:grid;place-items:center;flex:0 0 auto}.aci.act .pin,.aci:hover .pin{background:#d1fae5;color:#0f766e}
.aci .txt{flex:1;min-width:0}.aci .pri{font-size:14.5px;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.aci .pri mark{background:none;color:#0f766e;font-weight:800}.aci .sec{font-size:12.5px;color:#6b7280}
.aci .tag{font-size:11px;font-weight:700;color:#0f766e;background:#effcf8;border:1px solid #bbf7d0;border-radius:999px;padding:2px 8px}
.acfoot{padding:6px 14px;background:#f9fafb;border-top:1px solid #f1f2f4;font-size:11px;color:#9ca3af;display:flex;justify-content:space-between}
.rhead{display:flex;justify-content:space-between;align-items:center;margin:22px 0 0;gap:12px}
.clrhist{border:1px solid #e5e7eb;background:#fff;color:#6b7280;font:600 12px Inter,sans-serif;padding:5px 11px;border-radius:8px;cursor:pointer}.clrhist:hover{border-color:#d1d5db;color:#111827}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));gap:14px;margin-top:12px}.cardwrap{position:relative}.card{display:block;height:100%;background:#fff;border:1px solid #e5e7eb;border-radius:14px;padding:16px 32px 16px 16px;text-decoration:none;color:inherit;transition:border-color .15s,box-shadow .15s}.card:hover{border-color:#14b8a6;box-shadow:0 8px 22px -14px rgba(15,118,110,.45)}.card.live{border-color:#fcd34d;background:linear-gradient(180deg,#fffef7 0%,#fff 100%)}.card .a{font-weight:700}.card .v{font-size:22px;font-weight:800;color:#0f766e;margin:6px 0 2px}.card.live .v{font-size:15.5px;color:#92400e}.card .s{font-size:12.5px;color:#6b7280}
.bgbadge{display:inline-flex;align-items:center;gap:5px;font-size:11px;font-weight:700;padding:2px 8px;border-radius:999px;margin-bottom:6px}.bg-done{background:#ecfdf5;color:#065f46;border:1px solid #a7f3d0}.bg-live{background:#fffbeb;color:#92400e;border:1px solid #fde68a}
.cardrm{position:absolute;top:10px;right:10px;width:22px;height:22px;border-radius:50%;border:0;background:transparent;color:#9ca3af;font:700 15px/1 Inter,sans-serif;cursor:pointer;display:grid;place-items:center;padding:0}.cardrm:hover{background:#f3f4f6;color:#374151}
.tabs{display:inline-flex;gap:6px;background:#e5e7eb;padding:5px;border-radius:14px;margin:0 0 14px;flex-wrap:wrap}
.tabbtn{border:0;background:transparent;color:#374151;font:700 13.5px Inter,system-ui,sans-serif;padding:9px 16px;border-radius:10px;cursor:pointer;transition:background .15s,color .15s}
.tabbtn.act{background:#fff;color:#0f766e;box-shadow:0 2px 8px rgba(17,24,39,.08)}
.tabpanel{display:none}.tabpanel.act{display:block}
/* Area on-sale properties & above-the-fold buyer value showcase */
.area-onsale{margin-top:12px}
.area-onsale-bar{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.area-onsale-lbl{font-size:12.5px;font-weight:700;color:#374151}
.area-onsale-ctrls{display:inline-flex;align-items:center;gap:6px;flex-wrap:wrap}
.area-zip-form{display:inline-flex;align-items:center;background:#fff;border:1px solid #d1d5db;border-radius:10px;padding:2px 3px 2px 9px;gap:4px;box-shadow:0 2px 6px -4px rgba(17,24,39,.14)}
.area-zip-form:focus-within{border-color:#0f766e;box-shadow:0 0 0 2.5px rgba(15,118,110,.14)}
.area-zip-input{border:0;outline:none;width:136px;font:600 12.5px Inter,sans-serif;color:#111827;background:transparent}
.area-zip-btn,.area-geo-btn{border:0;border-radius:8px;padding:6px 11px;font:700 12px Inter,sans-serif;cursor:pointer;transition:background .15s,border-color .15s,color .15s}
.area-zip-btn{background:#0f766e;color:#fff}
.area-zip-btn:hover:not(:disabled){background:#0d655e}
.area-geo-btn{display:inline-flex;align-items:center;gap:5px;background:#fff;border:1px solid #a7f3d0;color:#065f46;border-radius:10px;padding:6px 11px}
.area-geo-btn:hover:not(:disabled){background:#effcf8;border-color:#0f766e}
.area-zip-btn:disabled,.area-geo-btn:disabled{opacity:.65;cursor:wait}
.area-onsale-status{margin-top:8px;font-size:12.5px;color:#4b5563;font-weight:600;display:none}
.area-onsale-status.on{display:block}
.area-onsale-status.err{color:#b91c1c}
.area-onsale-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:10px;margin-top:10px}
@media(max-width:860px){.area-onsale-grid{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media(max-width:560px){.area-onsale-grid{grid-template-columns:1fr}}
.area-home-card{display:flex;flex-direction:column;gap:5px;background:#fff;border:1px solid #e5e7eb;border-radius:13px;padding:12px 14px;text-decoration:none;color:inherit;transition:border-color .15s,box-shadow .15s,transform .12s;box-shadow:0 4px 12px -10px rgba(17,24,39,.25)}
.area-home-card:hover{border-color:#0f766e;box-shadow:0 8px 20px -12px rgba(15,118,110,.32);transform:translateY(-1px)}
.area-hc-top{display:flex;justify-content:space-between;align-items:center;gap:8px}
.area-hc-status{display:inline-flex;align-items:center;gap:5px;font-size:11px;font-weight:800;letter-spacing:.04em;text-transform:uppercase;color:#065f46;background:#ecfdf5;border:1px solid #a7f3d0;border-radius:999px;padding:2px 8px}
.area-hc-status.pend{color:#92400e;background:#fffbeb;border-color:#fde68a}
.area-hc-dom{font-size:11px;color:#6b7280;font-weight:600}
.area-hc-price{font-size:18px;font-weight:800;color:#111827;letter-spacing:-.3px;font-variant-numeric:tabular-nums;margin-top:1px}
.area-hc-street{font-size:13.5px;font-weight:700;color:#111827;line-height:1.3}
.area-hc-loc{font-size:12px;color:#6b7280}
.area-hc-specs{font-size:12px;color:#374151;font-weight:600}
.area-hc-cta{margin-top:auto;padding-top:4px;font-size:12px;font-weight:700;color:#0f766e}
.home-showcase{display:grid;grid-template-columns:minmax(0,1.03fr) minmax(0,.97fr);gap:16px;margin-top:18px;align-items:start}
@media(max-width:900px){.home-showcase{grid-template-columns:1fr}}
.bq-card,.anat-card{background:#fff;border:1px solid #e5e7eb;border-radius:16px;padding:18px 20px;box-shadow:0 10px 28px -20px rgba(17,24,39,.2)}
.bq-kicker{font-size:11px;font-weight:800;letter-spacing:.07em;text-transform:uppercase;color:#0f766e;margin-bottom:4px}
.bq-head{font-size:18.5px;font-weight:800;letter-spacing:-.3px;line-height:1.26;color:#111827;margin:0 0 13px}
.bq-list{display:flex;flex-direction:column;gap:10px}
.bq-item{display:flex;align-items:flex-start;gap:11px;padding:10px 12px;border-radius:12px;background:#f9fafb;border:1px solid #eef2f5}
.bq-item.lead{background:#f0fdfa;border-color:#ccfbf1}
.bq-item.lead-warn{background:#fffbeb;border-color:#fef3c7}
.bq-num{width:26px;height:26px;border-radius:8px;background:#0f766e;color:#fff;font:800 11.5px/1 Inter,sans-serif;display:grid;place-items:center;flex:0 0 26px;margin-top:1px}
.bq-item.lead-warn .bq-num{background:#b45309}
.bq-body{flex:1;min-width:0}
.bq-q{font-size:13.8px;font-weight:800;color:#111827;line-height:1.3;margin:0 0 2px}
.bq-a{font-size:12.5px;color:#4b5563;line-height:1.45;margin:0}
.sp-ctarow{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;margin-top:12px;padding-top:11px;border-top:1px solid #e5e7eb}
.sp-src{font-size:11.5px;color:#6b7280;font-weight:600}
.anat-list{display:flex;flex-direction:column;gap:9px;margin-top:10px}
.anat-row{padding:10px 12px;border-radius:11px;background:#f9fafb;border:1px solid #eef2f5}
.anat-row b{display:block;font-size:13px;color:#111827;margin-bottom:2px}
.anat-row span{display:block;font-size:12px;color:#4b5563;line-height:1.42}
@media(max-width:480px){
  .wrap{padding:18px 14px 48px}
  h1{font-size:24px;letter-spacing:-.5px}
  .sub{font-size:13.8px;margin-bottom:12px}
  .tabs{gap:4px;padding:4px;width:100%}
  .tabbtn{flex:1 1 auto;text-align:center;font-size:12px;padding:8px 8px}
  #homeForm{padding:6px 6px 6px 12px;gap:6px}
  #homeInput{font-size:15px;padding:9px 4px}
  button.go{padding:10px 13px;font-size:13.5px}
  .bq-card,.anat-card{padding:15px 14px}
}
.panelcard{background:#fff;border:1px solid #d1d5db;border-radius:16px;padding:20px;box-shadow:0 10px 30px -18px rgba(17,24,39,.18)}
.nlarea{width:100%;border:1px solid #d1d5db;border-radius:12px;padding:12px 14px;font:15px/1.5 Inter,system-ui,sans-serif;color:#111827;background:#fff;resize:vertical;min-height:78px;outline:none}
.nlarea:focus,.tinput:focus,.stsel:focus{border-color:#0f766e;box-shadow:0 0 0 3px rgba(20,184,166,.16)}
.tinput{width:100%;border:1px solid #d1d5db;border-radius:10px;padding:9px 12px;font:14px Inter,system-ui,sans-serif;color:#111827;outline:none;background:#fff}
.row2{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin-bottom:12px}
.row3{display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-bottom:12px}
.row4{display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin-bottom:12px}
@media(max-width:760px){.row3,.row4{grid-template-columns:repeat(2,1fr)}}
@media(max-width:640px){.row2,.row3,.row4{grid-template-columns:1fr}}
.lbl{display:block;font-size:12.5px;font-weight:700;color:#374151;margin-bottom:5px}
.chips{display:flex;flex-wrap:wrap;gap:7px;margin-top:10px}
.chip{border:1px solid #e5e7eb;background:#f9fafb;color:#374151;font:600 12px Inter,sans-serif;padding:5px 10px;border-radius:999px;cursor:pointer}
.chip:hover{border-color:#14b8a6;background:#effcf8;color:#0f766e}
.barfoot{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-top:12px;flex-wrap:wrap}
.stsel{border:1px solid #d1d5db;border-radius:9px;padding:8px 10px;font:600 13px Inter,sans-serif;background:#fff;color:#374151;outline:none}
.btnpri{border:0;background:#0f766e;color:#fff;font:700 14.5px Inter,sans-serif;padding:11px 20px;border-radius:11px;cursor:pointer}
.btnpri:hover{background:#0d655e}.btnpri:disabled{opacity:.6;cursor:wait}
.btnsec{border:1px solid #0f766e;background:#effcf8;color:#0f766e;font:700 12.5px Inter,sans-serif;padding:7px 12px;border-radius:9px;cursor:pointer;text-decoration:none;display:inline-flex;align-items:center;gap:5px}
.btnsec:hover{background:#d1fae5}
.btnmut{border:1px solid #d1d5db;background:#fff;color:#374151;font:600 12.5px Inter,sans-serif;padding:7px 12px;border-radius:9px;cursor:pointer}
.btnmut:hover{background:#f3f4f6}
.mbanner{margin-top:18px;background:#effcf8;border:1px solid #a7f3d0;border-radius:13px;padding:14px 16px}
.mcard{background:#fff;border:1px solid #e5e7eb;border-radius:14px;padding:16px;transition:border-color .15s,box-shadow .15s}
.mcard:hover{border-color:#14b8a6;box-shadow:0 8px 22px -14px rgba(15,118,110,.38)}
.mbadge{display:inline-block;font-size:11px;font-weight:700;padding:2px 8px;border-radius:999px}
.mb-sale{background:#d1fae5;color:#065f46}.mb-pend{background:#fef3c7;color:#92400e}.mb-sold{background:#ede9fe;color:#5b21b6}
.mb-score{background:#f0fdfa;color:#0f766e;border:1px solid #99f6e4}
.mb-sch{background:#eff6ff;color:#1d4ed8}
.whybox{margin:10px 0;padding:9px 11px;background:#f9fafb;border-left:3px solid #14b8a6;border-radius:6px;font-size:13px;color:#1f2937}
.wzgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(230px,1fr));gap:10px;margin-top:12px}
.wzcard{background:#f9fafb;border:1px solid #e5e7eb;border-radius:12px;padding:12px;font-size:13px}
.chkrow{display:flex;gap:14px;flex-wrap:wrap;margin:8px 0;font-size:13.5px;color:#374151}
.chkrow label{display:inline-flex;align-items:center;gap:6px;cursor:pointer}
/* Match Quiz & Profile Studio */
.studio{margin-bottom:18px;border:1px solid #99f6e4;background:linear-gradient(180deg,#f7fefa 0%,#ffffff 120px)}
.sthead{display:flex;justify-content:space-between;align-items:flex-start;gap:12px;flex-wrap:wrap;margin-bottom:14px}
.stepnav{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:16px}
.stepbtn{border:1px solid #d1d5db;background:#fff;color:#374151;font:700 12px Inter,sans-serif;padding:7px 11px;border-radius:10px;cursor:pointer;display:inline-flex;align-items:center;gap:6px}
.stepbtn.act{border-color:#0f766e;background:#0f766e;color:#fff}
.stepbtn .num{width:18px;height:18px;border-radius:50%;background:#e5e7eb;color:#374151;font-size:11px;display:grid;place-items:center}
.stepbtn.act .num{background:rgba(255,255,255,.22);color:#fff}
.qstep{display:none;padding:14px 0 6px;border-top:1px solid #eef2f5}.qstep.act{display:block}
.qtitle{font-size:16px;font-weight:800;color:#111827;margin:0 0 4px}
.qsub{font-size:13px;color:#4b5563;margin:0 0 12px}
.presetgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(180px,1fr));gap:8px;margin-bottom:14px}
.presetbtn{text-align:left;border:1px solid #e5e7eb;background:#fff;border-radius:12px;padding:10px 11px;cursor:pointer;transition:border-color .15s,background .15s}
.presetbtn:hover{border-color:#14b8a6;background:#f0fdfa}
.presetbtn.act{border-color:#0f766e;background:#effcf8;box-shadow:0 0 0 2px rgba(15,118,110,.15)}
.presetbtn .pt{font:700 13px Inter,sans-serif;color:#111827}
.presetbtn .pd{font-size:11.5px;color:#6b7280;margin-top:2px;line-height:1.35}
.areabox{position:relative;margin-bottom:8px}
.areadrop{position:absolute;top:calc(100% + 4px);left:0;right:0;z-index:1500;background:#fff;border:1px solid #d1d5db;border-radius:12px;box-shadow:0 14px 32px -12px rgba(17,24,39,.25);max-height:260px;overflow-y:auto;display:none}
.areadrop.open{display:block}
.areaitem{display:flex;justify-content:space-between;align-items:center;padding:8px 12px;cursor:pointer;border-bottom:1px solid #f3f4f6;font-size:13px}
.areaitem:hover{background:#effcf8}
.selchip{display:inline-flex;align-items:center;gap:6px;background:#ecfdf5;border:1px solid #a7f3d0;color:#065f46;border-radius:999px;padding:4px 10px;font:700 12px Inter,sans-serif}
.selchip.city{background:#eff6ff;border-color:#bfdbfe;color:#1e40af}
.selchip button{border:0;background:none;color:inherit;cursor:pointer;font-weight:800;padding:0;line-height:1}
.stylegrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(175px,1fr));gap:10px;margin:10px 0 14px}
.stylecard{border:1px solid #e5e7eb;background:#fff;border-radius:12px;padding:10px;display:flex;flex-direction:column;gap:6px;transition:border-color .15s,box-shadow .15s}
.stylecard.love{border-color:#0f766e;background:#f0fdfa}
.stylecard.fine{border-color:#3b82f6;background:#eff6ff}
.stylecard.avoid{border-color:#dc2626;background:#fef2f2}
.stylesvg{height:58px;display:grid;place-items:center;background:#f9fafb;border-radius:8px;padding:4px;color:#0f766e}
.stylecard.avoid .stylesvg{color:#b91c1c}
.stylename{font:700 12.5px Inter,sans-serif;color:#111827;line-height:1.25}
.stylereso{font-size:10.5px;color:#6b7280;line-height:1.25;min-height:24px}
.stylebtns{display:grid;grid-template-columns:repeat(4,1fr);gap:3px;margin-top:auto}
.stylebtns button{border:1px solid #e5e7eb;background:#fff;color:#4b5563;font:700 10.5px Inter,sans-serif;padding:4px 2px;border-radius:6px;cursor:pointer}
.stylebtns button.on-love{background:#0f766e;color:#fff;border-color:#0f766e}
.stylebtns button.on-fine{background:#2563eb;color:#fff;border-color:#2563eb}
.stylebtns button.on-neutral{background:#374151;color:#fff;border-color:#374151}
.stylebtns button.on-avoid{background:#dc2626;color:#fff;border-color:#dc2626}
.featgrid{display:flex;flex-wrap:wrap;gap:7px;margin-top:8px}
.featchip{display:inline-flex;align-items:center;gap:5px;border:1px solid #d1d5db;background:#fff;border-radius:999px;padding:5px 11px;font:600 12px Inter,sans-serif;cursor:pointer;color:#374151}
.featchip.on{border-color:#0f766e;background:#effcf8;color:#065f46}
.dimgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(270px,1fr));gap:10px;margin-top:10px}
.dimrow{border:1px solid #e5e7eb;background:#f9fafb;border-radius:12px;padding:10px 12px}
.dimtop{display:flex;justify-content:space-between;align-items:center;gap:8px;font-size:13px;font-weight:700}
.dimrow input[type=range]{width:100%;accent-color:#0f766e;margin:6px 0 4px}
.dimshare{background:#ecfdf5;color:#065f46;border:1px solid #a7f3d0;border-radius:999px;padding:1px 8px;font-size:11.5px;font-weight:800;font-variant-numeric:tabular-nums}
.maxdiffgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(230px,1fr));gap:10px;margin:8px 0 14px}
.mdcard{border:1px solid #e5e7eb;background:#fff;border-radius:12px;padding:10px 12px}
.mdopt{display:flex;justify-content:space-between;align-items:center;gap:6px;padding:5px 0;border-top:1px dashed #eef2f5;font-size:12.5px}
.mdopt button{border:1px solid #d1d5db;background:#fff;border-radius:6px;padding:2px 7px;font:700 11px Inter,sans-serif;cursor:pointer}
.mdopt button.most{background:#0f766e;color:#fff;border-color:#0f766e}
.mdopt button.least{background:#fef2f2;color:#b91c1c;border-color:#fecaca}
.placeitem{display:flex;justify-content:space-between;align-items:center;gap:10px;border:1px solid #e5e7eb;background:#f9fafb;border-radius:11px;padding:9px 12px;margin-top:8px;font-size:13px}
.dbgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));gap:9px;margin-top:10px}
.dbcard{border:1px solid #e5e7eb;background:#f9fafb;border-radius:11px;padding:10px 12px;font-size:13px}
.dbcard.on{border-color:#f59e0b;background:#fffbeb}
.digitbtn{width:28px;height:28px;border-radius:8px;border:1px solid #d1d5db;background:#fff;font:700 12.5px Inter,sans-serif;cursor:pointer}
.digitbtn.on{background:#dc2626;color:#fff;border-color:#dc2626}
.noticepill{display:inline-flex;align-items:center;gap:6px;background:#eff6ff;border:1px solid #bfdbfe;color:#1e40af;border-radius:9px;padding:6px 10px;font-size:12px;font-weight:700;margin-bottom:10px}
/* Ranked Area Explorer, Map & Compare */
.expwrap{margin-bottom:18px}
.exphead{display:flex;justify-content:space-between;align-items:flex-start;gap:12px;flex-wrap:wrap;margin-bottom:12px}
.expctrl{display:flex;flex-wrap:wrap;gap:10px;align-items:center;margin-top:8px}
.viewtabs{display:inline-flex;gap:4px;background:#f3f4f6;padding:3px;border-radius:10px;border:1px solid #e5e7eb}
.viewbtn{border:0;background:transparent;padding:6px 12px;border-radius:8px;font:700 12.5px Inter,sans-serif;color:#4b5563;cursor:pointer}
.viewbtn.act{background:#0f766e;color:#fff}
.rankbadge{display:inline-flex;align-items:center;justify-content:center;min-width:30px;height:24px;padding:0 8px;border-radius:999px;background:#111827;color:#fff;font:800 12px Inter,sans-serif}
.mb-tie{background:#fef3c7;color:#92400e;border:1px solid #fcd34d}
.mb-strong{background:#ecfdf5;color:#065f46;border:1px solid #a7f3d0}
.mb-good{background:#eff6ff;color:#1d4ed8;border:1px solid #bfdbfe}
.mb-fair{background:#fffbeb;color:#92400e;border:1px solid #fde68a}
.mb-weak{background:#fef2f2;color:#991b1b;border:1px solid #fecaca}
.expgrid{display:grid;grid-template-columns:repeat(auto-fill,minmax(310px,1fr));gap:12px;margin-top:12px}
.expmap{height:430px;border-radius:13px;border:1px solid #d1d5db;margin-top:12px;overflow:hidden;background:#f8fafc}
.cmptray{margin-top:14px;border:1px solid #bfdbfe;background:#f8fbff;border-radius:14px;padding:14px}
.cmptblwrap{overflow-x:auto;margin-top:10px}
.cmptbl{width:100%;border-collapse:collapse;font-size:12.5px;background:#fff;border-radius:10px;overflow:hidden}
.cmptbl th,.cmptbl td{padding:8px 10px;border-bottom:1px solid #eef2f5;text-align:left;vertical-align:top}
.cmptbl th{background:#f9fafb;font-weight:700;color:#374151;min-width:130px}
.starbtn{border:1px solid #e5e7eb;background:#fff;color:#4b5563;border-radius:8px;padding:5px 9px;font:700 12px Inter,sans-serif;cursor:pointer}
.starbtn.on{background:#fef3c7;border-color:#f59e0b;color:#92400e}
.cmpbtn{border:1px solid #d1d5db;background:#fff;color:#374151;border-radius:8px;padding:5px 9px;font:700 12px Inter,sans-serif;cursor:pointer}
.cmpbtn.on{background:#eff6ff;border-color:#2563eb;color:#1d4ed8}
.pinrank{display:inline-flex;align-items:center;gap:4px;color:#fff;font:800 11.5px Inter,sans-serif;padding:3px 8px;border-radius:999px;border:2px solid #fff;box-shadow:0 3px 10px rgba(0,0,0,.28);white-space:nowrap}
/* Faceted Home Shopping (/shop & #tab-shop) */
.shop-shell{display:grid;grid-template-columns:270px minmax(0,1fr);gap:18px;align-items:start}
.shop-sidebar{position:sticky;top:14px;max-height:calc(100vh - 28px);overflow-y:auto;background:#fff;border:1px solid #d1d5db;border-radius:16px;padding:15px 16px;box-shadow:0 10px 28px -20px rgba(17,24,39,.18)}
.shop-side-head{display:flex;justify-content:space-between;align-items:center;gap:8px;padding-bottom:10px;border-bottom:1px solid #eef2f5}
.shop-side-title{font-size:13.5px;font-weight:800;color:#111827;display:inline-flex;align-items:center;gap:6px}
.shop-reset-link{border:0;background:none;color:#0f766e;font:700 12px Inter,sans-serif;cursor:pointer;padding:2px 4px;border-radius:6px}
.shop-reset-link:hover{background:#effcf8;text-decoration:underline}
.fgroup{padding:11px 0;border-bottom:1px solid #eef2f5}
.fgroup:last-child{border-bottom:0;padding-bottom:2px}
.fg-title{font-size:11.5px;font-weight:800;letter-spacing:.04em;text-transform:uppercase;color:#111827;margin:0 0 6px;display:flex;justify-content:space-between;align-items:center;gap:6px}
.fg-sub{font-size:11px;font-weight:700;color:#6b7280;margin:7px 0 3px}
.fchk{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:3.5px 5px;border-radius:7px;font-size:12.6px;color:#374151;cursor:pointer;line-height:1.35;user-select:none}
.fchk:hover{background:#f0fdfa;color:#0f766e}
.fchk-lbl{display:inline-flex;align-items:center;gap:7px;min-width:0;flex:1}
.fchk-lbl span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.fchk input[type=checkbox],.fchk input[type=radio]{accent-color:#0f766e;margin:0;width:14px;height:14px;flex:0 0 14px;cursor:pointer}
.fcnt{font-size:11px;font-weight:700;color:#4b5563;background:#f3f4f6;border-radius:999px;padding:1px 7px;font-variant-numeric:tabular-nums;flex:0 0 auto}
.fcnt.zero{opacity:.45}
.fdyn-list{display:flex;flex-direction:column;gap:1px;max-height:184px;overflow-y:auto}
.fdyn-empty{font-size:11.5px;color:#9ca3af;padding:3px 4px}
.fgrid2{display:grid;grid-template-columns:1fr 1fr;gap:7px;margin-bottom:6px}
.fmini-sel{width:100%;border:1px solid #d1d5db;border-radius:8px;padding:5px 7px;font:600 12px Inter,sans-serif;background:#fff;color:#374151;outline:none}
.fmini-sel:focus{border-color:#0f766e}
.shop-sync-mini{display:flex;gap:6px;margin-top:8px}
.shop-sync-mini .tinput{padding:6px 9px;font-size:12.5px}
.shop-sync-mini .btnsec{padding:6px 10px;font-size:12px;white-space:nowrap}
.shop-sync-msg{font-size:11.5px;color:#0f766e;font-weight:600;margin-top:4px;min-height:0}
.shop-main{min-width:0;display:flex;flex-direction:column;gap:12px}
.shop-topbar{background:#fff;border:1px solid #d1d5db;border-radius:16px;padding:13px 15px;box-shadow:0 10px 26px -20px rgba(17,24,39,.18);display:flex;flex-direction:column;gap:10px}
.shop-toprow{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.shop-q-inp{flex:1 1 250px;min-width:180px}
.shop-loc-inp{flex:0 1 175px;min-width:135px}
.shop-mob-toggle{display:none;align-items:center;gap:6px;white-space:nowrap}
.shop-quick-bar{display:flex;align-items:center;gap:6px;flex-wrap:wrap}
.shop-qpill{border:1px solid #e5e7eb;background:#f9fafb;color:#374151;font:700 11.8px Inter,sans-serif;padding:4.5px 10px;border-radius:999px;cursor:pointer;display:inline-flex;align-items:center;gap:5px;transition:border-color .14s,background .14s,color .14s}
.shop-qpill:hover{border-color:#14b8a6;background:#f0fdfa;color:#0f766e}
.shop-qpill.on{border-color:#0f766e;background:#ecfdf5;color:#065f46;box-shadow:0 0 0 1px rgba(15,118,110,.15)}
.shop-active-chips{display:flex;align-items:center;gap:6px;flex-wrap:wrap}
.shop-active-chips:empty{display:none}
.achip{display:inline-flex;align-items:center;gap:5px;background:#effcf8;border:1px solid #99f6e4;color:#065f46;border-radius:999px;padding:3px 9px;font:700 11.8px Inter,sans-serif}
.achip button{border:0;background:none;color:#0f766e;cursor:pointer;font:800 13px/1 Inter,sans-serif;padding:0 1px}
.achip button:hover{color:#991b1b}
.shop-meta-row{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;font-size:13px;color:#4b5563;padding:0 2px}
.shop-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(295px,1fr));gap:14px}
.shop-card{background:#fff;border:1px solid #e5e7eb;border-radius:15px;overflow:hidden;display:flex;flex-direction:column;transition:border-color .15s,box-shadow .15s}
.shop-card:hover{border-color:#14b8a6;box-shadow:0 12px 26px -16px rgba(15,118,110,.42)}
.shop-photo{height:152px;background:#f3f4f6;position:relative;overflow:hidden;border-bottom:1px solid #f1f5f9}
.shop-photo img{width:100%;height:100%;object-fit:cover;display:block}
.shop-body{padding:14px 15px 15px;display:flex;flex-direction:column;gap:7px;flex:1}
.shop-badges{display:flex;justify-content:space-between;align-items:center;gap:6px;flex-wrap:wrap}
.shop-addr{font-size:15px;font-weight:800;color:#111827;line-height:1.3;text-decoration:none}
.shop-addr:hover{color:#0f766e}
.shop-subloc{font-size:12px;color:#6b7280;margin-top:-3px}
.shop-price-row{display:flex;align-items:baseline;justify-content:space-between;gap:8px;flex-wrap:wrap;margin-top:1px}
.shop-price{font-size:22px;font-weight:800;color:#0f766e;letter-spacing:-.4px;font-variant-numeric:tabular-nums}
.shop-specs{font-size:12.4px;color:#374151;line-height:1.42}
.util-pills{display:flex;flex-wrap:wrap;gap:5px;margin-top:3px}
.util-pills:empty{display:none}
.upill{display:inline-flex;align-items:center;gap:4px;font:700 11.3px/1.25 Inter,sans-serif;padding:3.5px 8px;border-radius:7px;border:1px solid #e5e7eb;background:#f9fafb;color:#374151;font-variant-numeric:tabular-nums}
.upill-mo{background:#f0fdfa;border-color:#99f6e4;color:#0f766e}
.upill-util{background:#f8fafc;border-color:#cbd5e1;color:#334155}
.upill-elec{background:#fffbeb;border-color:#fde68a;color:#92400e}
.upill-muni{background:#ecfdf5;border-color:#a7f3d0;color:#065f46}
.upill-fiber{background:#eff6ff;border-color:#bfdbfe;color:#1d4ed8}
.upill-inet{background:#f5f3ff;border-color:#ddd6fe;color:#5b21b6}
.upill-wst{background:#f0f9ff;border-color:#bae6fd;color:#0369a1}
.upill-hw{background:#fefce8;border-color:#fef08a;color:#854d0e}
.upill-ok{background:#f0fdf4;border-color:#bbf7d0;color:#166534}
.upill-warn{background:#fef2f2;border-color:#fecaca;color:#991b1b}
.shop-foot{display:flex;justify-content:space-between;align-items:center;gap:8px;flex-wrap:wrap;margin-top:auto;padding-top:9px;border-top:1px solid #f1f5f9}
.shop-ptype{font-size:11.5px;font-weight:600;color:#6b7280}
.shop-empty{background:#fff;border:1px solid #e5e7eb;border-radius:16px;padding:28px 22px;text-align:center;box-shadow:0 10px 28px -22px rgba(17,24,39,.16)}
.shop-empty h3{font-size:17px;margin:0 0 6px;color:#111827}
.shop-empty p{font-size:13.5px;color:#4b5563;max-width:560px;margin:0 auto 14px;line-height:1.5}
.shop-empty-actions{display:flex;justify-content:center;align-items:center;gap:8px;flex-wrap:wrap}
.geo-loc-host{margin-bottom:10px}
.geo-loc-host:empty{display:none;margin:0}
.shop-topbar .geo-loc-host{margin-bottom:0}
.geo-pill-bar{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;padding:6px 11px;border-radius:10px;background:#f8fafc;border:1px solid #e2e8f0;font-size:12px;color:#334155}
.geo-pill-left{display:inline-flex;align-items:center;gap:7px;flex-wrap:wrap;min-width:0;font-weight:600}
.geo-pill-actions{display:inline-flex;align-items:center;gap:5px;flex-wrap:wrap}
.geo-act{border:1px solid #cbd5e1;background:#fff;color:#0f766e;font:700 11.3px/1.2 Inter,sans-serif;padding:3px 8px;border-radius:999px;cursor:pointer;display:inline-flex;align-items:center;gap:4px;white-space:nowrap}
.geo-act:hover{border-color:#0f766e;background:#effcf8}
.geo-act.on{border-color:#0f766e;background:#ecfdf5;color:#065f46}
.geo-act.mut{color:#475569}
.geo-act.mut:hover{color:#991b1b;border-color:#fca5a5;background:#fef2f2}
.geo-zip-inline{width:76px;padding:2.5px 6px;border:1px solid #0f766e;border-radius:6px;font:700 11.5px Inter,sans-serif;color:#111827;outline:none}
.shop-geo-callout{background:#effcf8;border:1px solid #99f6e4;border-radius:13px;padding:11px 14px;display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;font-size:12.8px;color:#065f46}
.shop-geo-callout:empty{display:none}
@media(max-width:920px){
  .shop-shell{grid-template-columns:1fr}
  .shop-mob-toggle{display:inline-flex}
  .shop-sidebar{position:static;max-height:none;display:none}
  .shop-sidebar.open{display:block}
}
</style></head><body><div class="wrap">
<div class="topbar">
  <div class="logo">{{ logo_mark | safe }}<span>{{ brand_wordmark | safe }}</span></div>
  <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap">
    <a class="btnsec" href="/deal-room" data-i18n="common.dealRoom">{{ t_deal_room }}</a>
    <div class="langsw" id="langSwitch" title="Switch language (English / 简体中文 / Español)">
      <button type="button" data-lang="en" class="{% if not is_zh and not is_es %}on{% endif %}">EN</button>
      <button type="button" data-lang="zh-CN" class="{% if is_zh %}on{% endif %}">中文</button>
      <button type="button" data-lang="es" class="{% if is_es %}on{% endif %}">ES</button>
    </div>
  </div>
</div>
<h1 id="heroTitle" data-i18n="home.heroHeading">{{ t_hero_heading }}</h1>
<p class="sub" id="heroSub">{{ hero_subtitle }}</p>
<div class="tabs" role="tablist">
  <button type="button" class="tabbtn act" data-tab-target="report" id="tabBtnReport" data-i18n="home.tabReport">{{ t_tab_report }}</button>
  <button type="button" class="tabbtn" data-tab-target="shop" id="tabBtnShop">{{ t_tab_shop }}</button>
  <button type="button" class="tabbtn" data-tab-target="match" id="tabBtnMatch" data-i18n="home.tabMatch">{{ t_tab_match }}</button>
  <button type="button" class="tabbtn" data-tab-target="alerts" id="tabBtnAlerts" data-i18n="home.tabAlerts">{{ t_tab_alerts }}</button>
</div>
<div id="tab-report" class="tabpanel act">
  <div class="sbox"><form id="homeForm" action="/report" method="get" novalidate><span class="sico">🔍</span><input id="homeInput" name="address" placeholder="{{ t_search_placeholder }}" data-i18n-placeholder="home.searchPlaceholder" autocomplete="off" required><button type="button" class="clr" id="homeClr">×</button><button type="submit" class="go" id="homeGoBtn" data-i18n="home.generateBtn">{{ t_generate_btn }}</button></form><div class="home-pop"><div class="addr-err" id="homeAddrErr" role="alert"></div><div class="ac" id="homeAc" role="listbox"></div></div></div>
  <section class="area-onsale" id="areaOnsaleSection" aria-labelledby="areaOnsaleLabel">
    <div class="area-onsale-bar" id="areaOnsaleBar">
      <span class="area-onsale-lbl" id="areaOnsaleLabel">{{ area_onsale_label }}</span>
      <div class="area-onsale-ctrls">
        <form class="area-zip-form" id="areaZipForm" novalidate>
          <input type="text" class="area-zip-input" id="areaZipInput" inputmode="numeric" pattern="[0-9]*" maxlength="5" autocomplete="postal-code" placeholder="{{ area_zip_placeholder }}" aria-label="{{ area_zip_placeholder }}">
          <button type="submit" class="area-zip-btn" id="areaZipBtn">{{ area_zip_btn }}</button>
        </form>
        <button type="button" class="area-geo-btn" id="areaGeoBtn">{{ area_geo_btn }}</button>
      </div>
    </div>
    <div class="area-onsale-status" id="areaOnsaleStatus" role="status" aria-live="polite"></div>
    <div class="area-onsale-grid" id="areaOnsaleResults"></div>
  </section>
  <div id="userRecentReports"></div>
  <section class="home-showcase" id="homeShowcase" aria-labelledby="bqHeading">
    <div class="bq-card" id="buyerQuestionsCard">
      <div class="bq-kicker" id="bqKicker">{{ bq_kicker }}</div>
      <h2 class="bq-head" id="bqHeading">{{ bq_heading }}</h2>
      <div class="bq-list">
        <div class="bq-item lead">
          <span class="bq-num">01</span>
          <div class="bq-body">
            <h3 class="bq-q" id="bq1Q">{{ bq1_q }}</h3>
            <p class="bq-a" id="bq1A">{{ bq1_a }}</p>
          </div>
        </div>
        <div class="bq-item lead-warn">
          <span class="bq-num">02</span>
          <div class="bq-body">
            <h3 class="bq-q" id="bq2Q">{{ bq2_q }}</h3>
            <p class="bq-a" id="bq2A">{{ bq2_a }}</p>
          </div>
        </div>
        <div class="bq-item">
          <span class="bq-num">03</span>
          <div class="bq-body">
            <h3 class="bq-q" id="bq3Q">{{ bq3_q }}</h3>
            <p class="bq-a" id="bq3A">{{ bq3_a }}</p>
          </div>
        </div>
        <div class="bq-item">
          <span class="bq-num">04</span>
          <div class="bq-body">
            <h3 class="bq-q" id="bq4Q">{{ bq4_q }}</h3>
            <p class="bq-a" id="bq4A">{{ bq4_a }}</p>
          </div>
        </div>
        <div class="bq-item">
          <span class="bq-num">05</span>
          <div class="bq-body">
            <h3 class="bq-q" id="bq5Q">{{ bq5_q }}</h3>
            <p class="bq-a" id="bq5A">{{ bq5_a }}</p>
          </div>
        </div>
      </div>
    </div>
    <div class="anat-card" id="reportAnatomyCard">
      <div class="bq-kicker" id="anatKicker">{{ anat_kicker }}</div>
      <h2 class="bq-head" id="anatHeading" style="margin-bottom:4px">{{ anat_heading }}</h2>
      <p style="margin:0;font-size:12.5px;color:#4b5563" id="anatSub">{{ anat_sub }}</p>
      <div class="anat-list">
        <div class="anat-row"><b id="anat1T">{{ anat1_t }}</b><span id="anat1D">{{ anat1_d }}</span></div>
        <div class="anat-row"><b id="anat2T">{{ anat2_t }}</b><span id="anat2D">{{ anat2_d }}</span></div>
        <div class="anat-row"><b id="anat3T">{{ anat3_t }}</b><span id="anat3D">{{ anat3_d }}</span></div>
        <div class="anat-row"><b id="anat4T">{{ anat4_t }}</b><span id="anat4D">{{ anat4_d }}</span></div>
      </div>
      <div class="sp-ctarow">
        <span class="sp-src" id="anatFoot">{{ anat_foot }}</span>
        <button type="button" class="btnsec" id="anatFocusSearchBtn">{{ anat_cta }}</button>
      </div>
    </div>
  </section>
</div>

<div id="tab-shop" class="tabpanel">
  <div class="shop-shell">
    <aside class="shop-sidebar" id="shopSidebar" aria-label="Faceted home filters">
      <div class="shop-side-head">
        <span class="shop-side-title" id="shopSideTitle">⚙ Filter by Specs</span>
        <button type="button" class="shop-reset-link" id="shopSidebarResetBtn">Clear All</button>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleInternet">1. Home Internet &amp; Fiber</div>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfFiberOnly"> <span id="lblSfFiber">Symmetrical Fiber (FTTH)</span></span><span class="fcnt" id="fcntFiber">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfHighSpeedOnly"> <span id="lblSfHighSpeed">High-Speed Broadband</span></span><span class="fcnt" id="fcntHighSpeed">0</span></label>
        <div class="fg-sub" id="fgSubMinDl">Min Download Speed</div>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMinDl" value="" checked> <span data-shop-any="1">Any Speed</span></span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMinDl" value="1000"> <span>1+ Gbps</span></span><span class="fcnt" id="fcntSpeed1G">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMinDl" value="2000"> <span>2+ Gbps</span></span><span class="fcnt" id="fcntSpeed2G">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMinDl" value="5000"> <span>5+ Gbps</span></span><span class="fcnt" id="fcntSpeed5G">0</span></label>
        <div class="fg-sub" id="fgSubIspCarriers">Broadband Carriers</div>
        <div class="fdyn-list" id="shopFacetIspList"></div>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleElectric">2. Electricity Carrier &amp; Rate</div>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfMuniElec"> <span id="lblSfMuniElec">Municipal Public Power Only</span></span><span class="fcnt" id="fcntMuniElec">0</span></label>
        <div class="fg-sub" id="fgSubMaxElecRate">Max Tariff Rate</div>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxElecRate" value="" checked> <span data-shop-any="1">Any Rate</span></span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxElecRate" value="0.20"> <span>&le; $0.20/kWh</span></span><span class="fcnt" id="fcntRate20">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxElecRate" value="0.25"> <span>&le; $0.25/kWh</span></span><span class="fcnt" id="fcntRate25">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxElecRate" value="0.35"> <span>&le; $0.35/kWh</span></span><span class="fcnt" id="fcntRate35">0</span></label>
        <div class="fg-sub" id="fgSubElecCarriers">Electric Utility Carriers</div>
        <div class="fdyn-list" id="shopFacetElecList"></div>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleBudget">3. All-In Monthly &amp; Price</div>
        <div class="fg-sub" id="fgSubMaxMonthly">Max All-In Monthly</div>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxMonthly" value="" checked> <span data-shop-any="1">Any</span></span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxMonthly" value="4500"> <span>&le; $4,500/mo</span></span><span class="fcnt" id="fcntMo4500">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxMonthly" value="6500"> <span>&le; $6,500/mo</span></span><span class="fcnt" id="fcntMo6500">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxMonthly" value="9000"> <span>&le; $9,000/mo</span></span><span class="fcnt" id="fcntMo9000">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxMonthly" value="12500"> <span>&le; $12,500/mo</span></span><span class="fcnt" id="fcntMo12500">0</span></label>
        <div class="fg-sub" id="fgSubMaxPrice">Max Purchase Price</div>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxPrice" value="" checked> <span data-shop-any="1">Any</span></span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxPrice" value="800000"> <span>&le; $800K</span></span><span class="fcnt" id="fcntPr800">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxPrice" value="1250000"> <span>&le; $1.25M</span></span><span class="fcnt" id="fcntPr1250">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxPrice" value="1750000"> <span>&le; $1.75M</span></span><span class="fcnt" id="fcntPr1750">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="radio" name="sfMaxPrice" value="2500000"> <span>&le; $2.5M</span></span><span class="fcnt" id="fcntPr2500">0</span></label>
        <div class="fg-sub" id="fgSubHoa">HOA Dues</div>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfNoHoa"> <span id="lblSfNoHoa">No HOA ($0/mo)</span></span><span class="fcnt" id="fcntHoa0">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfHoa300"> <span id="lblSfHoa300">HOA &le; $300/mo</span></span><span class="fcnt" id="fcntHoa300">0</span></label>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleBedsType">4. Beds, Baths &amp; Property Type</div>
        <div class="fgrid2">
          <div>
            <div class="fg-sub" style="margin-top:0" id="fgSubMinBeds">Min Beds</div>
            <select id="sfMinBeds" class="fmini-sel" aria-label="Minimum bedrooms">
              <option value="">Any</option><option value="1">1+ bd</option><option value="2">2+ bd</option><option value="3">3+ bd</option><option value="4">4+ bd</option><option value="5">5+ bd</option>
            </select>
          </div>
          <div>
            <div class="fg-sub" style="margin-top:0" id="fgSubMinBaths">Min Baths</div>
            <select id="sfMinBaths" class="fmini-sel" aria-label="Minimum bathrooms">
              <option value="">Any</option><option value="1">1+ ba</option><option value="1.5">1.5+ ba</option><option value="2">2+ ba</option><option value="2.5">2.5+ ba</option><option value="3">3+ ba</option>
            </select>
          </div>
        </div>
        <div class="fg-sub" id="fgSubPropType">Property Type</div>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-sf-ptype="single_family"> <span id="lblPtSf">Single-Family</span></span><span class="fcnt" id="fcntPt_single_family">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-sf-ptype="townhouse"> <span id="lblPtTh">Townhouse</span></span><span class="fcnt" id="fcntPt_townhouse">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-sf-ptype="condo"> <span id="lblPtCd">Condo</span></span><span class="fcnt" id="fcntPt_condo">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-sf-ptype="multi_family"> <span id="lblPtMf">Multi-Family (2–4)</span></span><span class="fcnt" id="fcntPt_multi_family">0</span></label>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleHardware">5. Energy, Lot &amp; Hardware</div>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfSolar"> <span id="lblSfSolar">Solar Panels</span></span><span class="fcnt" id="fcntSolar">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfEv"> <span id="lblSfEv">EV Charger</span></span><span class="fcnt" id="fcntEv">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfHeatPump"> <span id="lblSfHeatPump">Heat Pump HVAC</span></span><span class="fcnt" id="fcntHeatPump">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfHasAdu"> <span id="lblSfHasAdu">Existing ADU</span></span><span class="fcnt" id="fcntHasAdu">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfAduFeasible"> <span id="lblSfAduFeasible">ADU-Eligible Lot</span></span><span class="fcnt" id="fcntAduFeasible">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfPool"> <span id="lblSfPool">Pool</span></span><span class="fcnt" id="fcntPool">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfGarage2"> <span id="lblSfGarage2">2+ Garage Spaces</span></span><span class="fcnt" id="fcntGarage2">0</span></label>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleWaterTax">6. Water, Sewer, Tax &amp; Hazards</div>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfMuniWater"> <span id="lblSfMuniWater">Municipal Water</span></span><span class="fcnt" id="fcntMuniWater">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfMuniSewer"> <span id="lblSfMuniSewer">Municipal Sewer (No Septic)</span></span><span class="fcnt" id="fcntMuniSewer">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfNoSpecialTax"> <span id="lblSfNoSpecTax">No Special Tax (Mello-Roos/MUD/CDD)</span></span><span class="fcnt" id="fcntNoSpecTax">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfLowFlood"> <span id="lblSfLowFlood">Outside FEMA SFHA Flood</span></span><span class="fcnt" id="fcntLowFlood">0</span></label>
        <label class="fchk"><span class="fchk-lbl"><input type="checkbox" id="sfLowFire"> <span id="lblSfLowFire">Outside High Wildfire Zone</span></span><span class="fcnt" id="fcntLowFire">0</span></label>
        <div class="fg-sub" id="fgSubSchoolGrade">Min Official School Grade</div>
        <select id="sfSchoolGrade" class="fmini-sel" aria-label="Minimum official school grade">
          <option value="">Any School Grade</option>
          <option value="A">A / A- (Official 8.0+)</option>
          <option value="B">B or Better (Official 6.0+)</option>
          <option value="C">C or Better (Official 4.0+)</option>
        </select>
      </div>

      <div class="fgroup">
        <div class="fg-title" id="fgTitleLocation">7. Location (City / ZIP)</div>
        <div class="fdyn-list" id="shopFacetLocList"></div>
        <div class="fg-sub" id="fgSubSyncZip">Sync Real ZIP into Catalog</div>
        <div class="shop-sync-mini">
          <input id="shopSideZipInput" class="tinput" type="text" maxlength="5" inputmode="numeric" placeholder="5-digit US ZIP">
          <button type="button" class="btnsec" id="shopSideSyncBtn">Sync ZIP</button>
        </div>
        <div class="shop-sync-msg" id="shopSideSyncMsg"></div>
      </div>
    </aside>

    <div class="shop-main">
      <div class="shop-topbar">
        <div class="shop-toprow">
          <button type="button" class="btnmut shop-mob-toggle" id="shopMobileFilterBtn" aria-expanded="false">☰ <span id="lblShopMobFilters">Filters</span> <span id="shopMobActiveCnt"></span></button>
          <input id="shopSearchInput" class="tinput shop-q-inp" type="search" placeholder="Filter by specs or keyword (fiber, municipal power, solar, ADU, single-story)…" autocomplete="off">
          <input id="shopLocQuickInput" class="tinput shop-loc-inp" type="text" placeholder="5-digit US ZIP or City" autocomplete="off">
          <select id="shopStatusSelect" class="stsel" aria-label="Shop listing status">
            <option value="all">All Indexed Homes</option>
            <option value="for_sale,pending">Active &amp; Pending</option>
            <option value="for_sale,pending,sold">Active + Sold</option>
            <option value="sold">Sold Only</option>
          </select>
          <select id="shopSortSelect" class="stsel" aria-label="Sort shop homes">
            <option value="monthly_asc">All-In Monthly: Low &rarr; High</option>
            <option value="electric_rate_asc">Electric Rate ($/kWh): Low &rarr; High</option>
            <option value="internet_desc">Fiber &amp; Speed: High &rarr; Low</option>
            <option value="price_asc">Price: Low &rarr; High</option>
            <option value="price_desc">Price: High &rarr; Low</option>
            <option value="school_desc">School Score: High &rarr; Low</option>
            <option value="sqft_desc">Living Sq Ft: High &rarr; Low</option>
            <option value="newest">Newest Listed</option>
            <option value="best_match">Featured / Best Fit</option>
          </select>
          <button type="button" class="btnpri" id="shopApplyBtn" style="padding:9px 16px;font-size:13.5px">Search</button>
        </div>
        <div class="geo-loc-host" id="shopGeoLocBar"></div>
        <div class="shop-quick-bar" id="shopQuickBar">
          <button type="button" class="shop-qpill" data-qpreset="fiber">🌐 <span id="qpFiber">Symmetrical Fiber</span></button>
          <button type="button" class="shop-qpill" data-qpreset="muni_elec">⚡ <span id="qpMuniElec">Municipal Power</span></button>
          <button type="button" class="shop-qpill" data-qpreset="rate_25">💲 <span id="qpRate25">&le; $0.25/kWh</span></button>
          <button type="button" class="shop-qpill" data-qpreset="no_hoa">🏡 <span id="qpNoHoa">$0 HOA</span></button>
          <button type="button" class="shop-qpill" data-qpreset="no_spec_tax">🛡 <span id="qpNoSpecTax">No Mello-Roos/MUD</span></button>
          <button type="button" class="shop-qpill" data-qpreset="adu_ready">📐 <span id="qpAduReady">ADU Ready</span></button>
          <button type="button" class="shop-qpill" data-qpreset="solar">☀ <span id="qpSolar">Solar</span></button>
        </div>
        <div class="shop-active-chips" id="shopActiveChips"></div>
      </div>
      <div id="shopGeoUnsyncedBanner" class="shop-geo-callout"></div>
      <div class="shop-meta-row" id="shopMetaRow"></div>
      <div id="shopResultsHost"></div>
    </div>
  </div>
</div>

<div id="tab-match" class="tabpanel">
  <div class="geo-loc-host" id="matchGeoLocBar"></div>
  <div class="panelcard studio" id="matchQuizStudio">
    <div class="sthead">
      <div>
        <h2 style="font-size:19px;margin:0 0 3px" id="studioHeading">✦ Your Match — 60-Second Quiz &amp; Preference Studio</h2>
        <div style="font-size:13px;color:#4b5563" id="studioSub">Deterministic personalized scoring across 11 buyer dimensions — stored privately in your browser.</div>
      </div>
      <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
        <span class="mbadge mb-score" id="studioSummaryPill">Balanced · 0/10 ZIPs · 0/6 Cities · 0/3 Places</span>
        <button type="button" class="btnmut" id="toggleAllStepsBtn">Show All Steps</button>
      </div>
    </div>

    <div class="stepnav" id="quizStepNav">
      <button type="button" class="stepbtn act" data-step="1"><span class="num">1</span><span data-i18n="step1">Areas &amp; Presets</span></button>
      <button type="button" class="stepbtn" data-step="2"><span class="num">2</span><span data-i18n="step2">Budget &amp; Needs</span></button>
      <button type="button" class="stepbtn" data-step="3"><span class="num">3</span><span data-i18n="step3">House Style</span></button>
      <button type="button" class="stepbtn" data-step="4"><span class="num">4</span><span data-i18n="step4">Important Places</span></button>
      <button type="button" class="stepbtn" data-step="5"><span class="num">5</span><span data-i18n="step5">Priorities (11)</span></button>
      <button type="button" class="stepbtn" data-step="6"><span class="num">6</span><span data-i18n="step6">Schools &amp; Deal-Breakers</span></button>
    </div>

    <div class="qstep act" id="quizStep1" data-step="1">
      <h3 class="qtitle" id="s1Title">1. Area Scope &amp; Lifestyle Presets</h3>
      <p class="qsub" id="s1Sub">Start with a lifestyle preset (weights are identical across languages) and pick up to 10 ZIP codes and 6 cities.</p>
      <div class="lbl" id="lblPresets">Lifestyle Presets (10)</div>
      <div class="presetgrid" id="presetGrid"></div>
      <div class="row2">
        <div>
          <label class="lbl" for="areaSearchInput" id="lblAreaPicker">Search active/recent ZIPs or Cities</label>
          <div class="areabox">
            <input id="areaSearchInput" class="tinput" placeholder="Type 5-digit US ZIP or city…" autocomplete="off">
            <div class="areadrop" id="areaDropdown"></div>
          </div>
          <div style="font-size:11.5px;color:#6b7280" id="areaHint">Click a suggestion or type a 5-digit ZIP and press Enter.</div>
        </div>
        <div>
          <div class="lbl"><span id="lblSelectedZips">Selected ZIPs</span> (<span id="zipCount">0</span>/10) &amp; <span id="lblSelectedCities">Cities</span> (<span id="cityCount">0</span>/6)</div>
          <div class="chips" id="areaChipsBox" style="margin-top:4px;min-height:38px"></div>
        </div>
      </div>
    </div>

    <div class="qstep" id="quizStep2" data-step="2">
      <h3 class="qtitle" id="s2Title">2. Budget, Financing &amp; Home Needs</h3>
      <p class="qsub" id="s2Sub">All-in monthly cost scales with down payment and loan type; beds, baths, size and multigenerational layout shape Home Fit.</p>
      <div class="row4">
        <div>
          <label class="lbl" for="qMonthlyBudget" id="lblMonthlyBudget">Max Monthly Budget ($/mo)</label>
          <input id="qMonthlyBudget" class="tinput" type="number" min="500" max="500000" step="100" placeholder="e.g. 16000">
        </div>
        <div>
          <label class="lbl" for="qMaxPrice" id="lblMaxPrice">Max Purchase Price ($)</label>
          <input id="qMaxPrice" class="tinput" type="number" min="50000" max="100000000" step="10000" placeholder="e.g. 2200000">
        </div>
        <div>
          <label class="lbl" for="qDownPct" id="lblDownPct">Down Payment (%)</label>
          <input id="qDownPct" class="tinput" type="number" min="0" max="100" step="1" value="20">
        </div>
        <div>
          <label class="lbl" for="qLoanProgram" id="lblLoanProgram">Loan Type</label>
          <select id="qLoanProgram" class="stsel" style="width:100%">
            <option value="30yr_fixed">30-Year Fixed (Conventional)</option>
            <option value="15yr_fixed">15-Year Fixed (Conventional)</option>
            <option value="arm_5_1">5/1 ARM (Conventional)</option>
            <option value="fha">FHA Loan</option>
            <option value="va">VA Loan</option>
            <option value="cash">All Cash</option>
          </select>
        </div>
      </div>
      <div class="row4">
        <div>
          <label class="lbl" for="qMinBeds" id="lblMinBeds">Min Bedrooms</label>
          <select id="qMinBeds" class="stsel" style="width:100%">
            <option value="">Any</option><option value="1">1+</option><option value="2">2+</option><option value="3">3+</option><option value="4">4+</option><option value="5">5+</option><option value="6">6+</option>
          </select>
        </div>
        <div>
          <label class="lbl" for="qMinBaths" id="lblMinBaths">Min Bathrooms</label>
          <select id="qMinBaths" class="stsel" style="width:100%">
            <option value="">Any</option><option value="1">1+</option><option value="1.5">1.5+</option><option value="2">2+</option><option value="2.5">2.5+</option><option value="3">3+</option><option value="4">4+</option>
          </select>
        </div>
        <div>
          <label class="lbl" for="qMinSqft" id="lblMinSqft">Target Living Sq Ft</label>
          <input id="qMinSqft" class="tinput" type="number" min="300" max="30000" step="50" placeholder="e.g. 1900">
        </div>
        <div>
          <label class="lbl" for="qMinLotSqft" id="lblMinLotSqft">Target Lot Sq Ft</label>
          <input id="qMinLotSqft" class="tinput" type="number" min="500" max="5000000" step="250" placeholder="e.g. 6000">
        </div>
      </div>
      <div class="lbl" id="lblPropTypes">Property Types (leave empty for any)</div>
      <div class="chkrow" id="propTypesBox"></div>
      <div class="chkrow" style="margin-top:10px;padding-top:8px;border-top:1px dashed #e5e7eb">
        <label><input type="checkbox" id="qMultigen"> <span id="lblMultigen">Multigenerational household (ADU size/baths count toward fit; layout weighs 40%)</span></label>
        <label><input type="checkbox" id="qAduNeeded"> <span id="lblAduNeeded">Prioritize existing ADU / in-law unit</span></label>
      </div>
    </div>

    <div class="qstep" id="quizStep3" data-step="3">
      <h3 class="qtitle" id="s3Title">3. House Style &amp; Character (16 RESO Style Families)</h3>
      <p class="qsub" id="s3Sub">Choose Love, Like, Neutral, or Avoid for any architectural styles, check desired character features, and tune style weight.</p>
      <div class="row2" style="background:#f9fafb;padding:12px;border-radius:12px;border:1px solid #e5e7eb">
        <div>
          <label class="lbl" for="qStyleSlider"><span id="lblStyleWeight">Style &amp; Character Weight</span>: <b id="qStyleWeightVal">6</b> (<span id="qStyleShareVal">6.0%</span>)</label>
          <input id="qStyleSlider" type="range" min="0" max="100" step="1" value="6" style="width:100%;accent-color:#0f766e">
        </div>
        <div>
          <label class="lbl" for="qEraPref" id="lblEraPref">Era Fallback Preference (when listing omits style)</label>
          <select id="qEraPref" class="stsel" style="width:100%">
            <option value="any">Any build era</option>
            <option value="pre_1945">Historic / Pre-1945 character</option>
            <option value="mid_century_1945_1979">Mid-century (1945–1979)</option>
            <option value="contemporary_1980_2009">Late-20th / 2000s (1980–2009)</option>
            <option value="modern_2010_plus">Newer / Modern build (2010+)</option>
          </select>
        </div>
      </div>
      <div class="stylegrid" id="styleCardsGrid"></div>
      <div class="lbl" id="lblFeatures" style="margin-top:8px">Character Features Checklist</div>
      <div class="featgrid" id="featureWishesGrid"></div>
    </div>

    <div class="qstep" id="quizStep4" data-step="4">
      <h3 class="qtitle" id="s4Title">4. Important Places (up to 3)</h3>
      <p class="qsub" id="s4Sub">Add work, school, or family addresses for drive-time scoring (routed via configured router or road-distance estimates; places are yours alone and never auto-suggested by demographics).</p>
      <div class="row4">
        <div>
          <label class="lbl" for="placeLabelInput" id="lblPlaceLabel">Label</label>
          <input id="placeLabelInput" class="tinput" placeholder="Work / School / Family">
        </div>
        <div style="grid-column:span 2;position:relative">
          <label class="lbl" for="placeAddrInput" id="lblPlaceAddr">Street Address (autocomplete)</label>
          <input id="placeAddrInput" class="tinput" placeholder="Street address, City, State ZIP" autocomplete="off">
          <div class="ac" id="placeAcBox"></div>
        </div>
        <div>
          <label class="lbl" for="placeModeInput" id="lblPlaceMode">Travel Mode</label>
          <select id="placeModeInput" class="stsel" style="width:100%">
            <option value="drive">Drive (routed / est.)</option>
          </select>
        </div>
      </div>
      <div class="row4">
        <div>
          <label class="lbl" for="placeMaxMinInput" id="lblPlaceMaxMin">Max Commute (min)</label>
          <input id="placeMaxMinInput" class="tinput" type="number" min="5" max="240" value="30">
        </div>
        <div>
          <label class="lbl" for="placeTripsInput" id="lblPlaceTrips">Trips / Week</label>
          <input id="placeTripsInput" class="tinput" type="number" min="1" max="50" value="5">
        </div>
        <div>
          <label class="lbl" for="placeLatInput">Lat (auto-filled)</label>
          <input id="placeLatInput" class="tinput" type="number" step="0.0001" placeholder="Latitude">
        </div>
        <div>
          <label class="lbl" for="placeLonInput">Lon (auto-filled)</label>
          <input id="placeLonInput" class="tinput" type="number" step="0.0001" placeholder="Longitude">
        </div>
      </div>
      <div style="display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap">
        <div style="display:flex;align-items:center;gap:10px">
          <label class="lbl" for="qTrafficBuffer" style="margin:0"><span id="lblTrafficBuf">Peak Traffic Buffer</span>: <b id="qTrafficBufVal">1.30×</b></label>
          <input id="qTrafficBuffer" type="range" min="1.0" max="2.0" step="0.05" value="1.3" style="width:130px;accent-color:#0f766e">
        </div>
        <div style="display:flex;gap:8px;align-items:center">
          <span id="placeGeoStatus" style="font-size:12px;color:#6b7280"></span>
          <button type="button" class="btnsec" id="addPlaceBtn">＋ Add Place (<span id="placeCount">0</span>/3)</button>
        </div>
      </div>
      <div id="placesList" style="margin-top:8px"></div>
    </div>

    <div class="qstep" id="quizStep5" data-step="5">
      <h3 class="qtitle" id="s5Title">5. Priorities — Quick Trade-offs &amp; 11 Live Sliders</h3>
      <p class="qsub" id="s5Sub">Pick quick Most/Least trade-offs to shape priorities fast, then fine-tune any of the 11 dimensions. Weights normalize to 100% automatically.</p>
      <div class="lbl" id="lblMaxDiff">Quick Priority Trade-offs (MaxDiff Boosters)</div>
      <div class="maxdiffgrid" id="maxDiffBox"></div>
      <div style="display:flex;justify-content:space-between;align-items:center;gap:10px;margin-top:10px;flex-wrap:wrap">
        <div class="lbl" style="margin:0"><span id="lblAllSliders">All 11 Dimension Weights</span> · <span id="sliderSumBadge" class="mbadge mb-score">Raw sum: 100 · Share: 100.0%</span></div>
        <button type="button" class="btnmut" id="normalizeWeightsBtn">Normalize Sliders to 100</button>
      </div>
      <div class="dimgrid" id="dimSlidersGrid"></div>
    </div>

    <div class="qstep" id="quizStep6" data-step="6">
      <h3 class="qtitle" id="s6Title">6. Public Schools &amp; Deal-Breakers</h3>
      <p class="qsub" id="s6Sub">Choose which school levels count when Schools has weight, and enable hard deal-breaker screens.</p>
      <div class="noticepill" id="schoolDataNotice">ⓘ Official CA public state/federal academic data only (CAASPP &amp; CA School Dashboard; commercial school ratings are never scored)</div>
      <div class="lbl" id="lblSchoolLevels">Public School Levels to Include (leave all unchecked to average all 3)</div>
      <div class="chkrow" id="schoolLevelsBox"></div>
      <div class="lbl" id="lblDealBreakers" style="margin-top:12px">Hard Deal-Breakers (7 Screens)</div>
      <div class="dbgrid" id="dealBreakersGrid"></div>
    </div>

    <div class="barfoot" style="border-top:1px solid #e5e7eb;padding-top:12px;margin-top:14px">
      <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
        <button type="button" class="btnmut" id="prevStepBtn">← Back</button>
        <button type="button" class="btnmut" id="nextStepBtn">Next →</button>
        <button type="button" class="btnmut" id="resetProfileBtn">Reset</button>
        <button type="button" class="btnmut" id="toggleExportBtn">Export / Import JSON</button>
      </div>
      <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
        <span id="profileSaveStatus" style="font-size:12.5px;font-weight:700;color:#0f766e"></span>
        <button type="button" class="btnsec" id="saveProfileBtn">✓ Save &amp; Validate Profile</button>
        <button type="button" class="btnpri" id="studioRankBtn">✦ Rank Homes in My Areas →</button>
      </div>
    </div>
    <div id="profileExportBox" style="display:none;margin-top:12px;padding-top:12px;border-top:1px dashed #d1d5db">
      <textarea id="profileJsonArea" class="nlarea" style="font-family:ui-monospace,monospace;font-size:12px;min-height:90px"></textarea>
      <div style="display:flex;gap:8px;margin-top:8px">
        <button type="button" class="btnsec" id="importProfileJsonBtn">Import JSON</button>
        <button type="button" class="btnmut" id="copyProfileJsonBtn">Copy JSON</button>
      </div>
    </div>
  </div>

  <div class="panelcard expwrap" id="areaExplorerSection">
    <div class="exphead">
      <div>
        <h2 style="font-size:18px;margin:0 0 3px" id="explorerHeading">✦ Ranked Home Explorer — List, Map &amp; Compare</h2>
        <div style="font-size:13px;color:#4b5563" id="explorerSub">Deterministically rank active or sold homes across your selected ZIPs &amp; cities, then compare 2–4 homes side-by-side or adjust weights for instant preview.</div>
      </div>
      <div class="viewtabs" id="explorerViewTabs">
        <button type="button" class="viewbtn act" data-exp-view="list" id="expViewListBtn">☰ List</button>
        <button type="button" class="viewbtn" data-exp-view="map" id="expViewMapBtn">🗺 Map</button>
        <button type="button" class="viewbtn" data-exp-view="compare" id="expViewCompareBtn">⚖ Compare (<span id="compareCountBadge">0</span>)</button>
      </div>
    </div>
    <div class="expctrl">
      <select id="expStatusFilter" class="stsel" aria-label="Area status filter">
        <option value="for_sale,pending">Active &amp; Pending</option>
        <option value="for_sale,pending,sold">Active + Sold</option>
        <option value="sold">Recently Sold Only</option>
        <option value="">All Statuses</option>
      </select>
      <label style="font-size:12.5px;display:inline-flex;align-items:center;gap:5px;cursor:pointer"><input type="checkbox" id="expAutoSync"> <span id="lblExpAutoSync">Auto-sync ZIPs</span></label>
      <label style="font-size:12.5px;display:inline-flex;align-items:center;gap:5px;cursor:pointer"><input type="checkbox" id="expIncludeFlagged"> <span id="lblExpIncludeFlagged">Show deal-breaker flagged &amp; near-misses</span></label>
      <label style="font-size:12.5px;display:inline-flex;align-items:center;gap:5px;cursor:pointer"><input type="checkbox" id="expTriggerDeep"> <span id="lblExpDeepScan">Deep-scan top uncached homes</span></label>
      <button type="button" class="btnpri" id="rankAreaBtn">✦ Rank Homes in My Areas</button>
    </div>
    <div id="explorerRefineBanner"></div>
    <div id="compareTrayBar"></div>
    <div id="explorerResults"></div>
    <div id="expMapWrap" style="display:none"><div id="expMapCanvas" class="expmap"></div></div>
    <div id="expCompareWrap" style="display:none"></div>
    <div id="savedHomesBox" style="margin-top:14px"></div>
  </div>

  <div class="panelcard">
    <label class="lbl" for="nlQuery" id="nlLabel" data-i18n="home.matchLabel">{{ t_match_label }}</label>
    <textarea id="nlQuery" class="nlarea" placeholder="{{ t_match_placeholder }}" data-i18n-placeholder="home.matchPlaceholder"></textarea>
    <div class="chips">
      <button type="button" class="chip" data-ex="4+ bed single-family home under $2.2M with A schools and 6,000+ sqft lot, no HOA" data-i18n="home.chip1">{{ t_match_chip_1 }}</button>
      <button type="button" class="chip" data-ex="3+ bed house under $1.95M, remodeled or single-story" data-i18n="home.chip2">{{ t_match_chip_2 }}</button>
      <button type="button" class="chip" data-ex="5 bedroom home with 2,000+ sqft and good schools, include recently sold" data-i18n="home.chip3">{{ t_match_chip_3 }}</button>
    </div>
    <div class="barfoot">
      <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
        <select id="nlStatus" class="stsel" aria-label="Listing status filter">
          <option value="auto" data-i18n="home.statusAuto">{{ t_status_auto }}</option>
          <option value="for_sale,pending" data-i18n="home.statusActive">{{ t_status_active }}</option>
          <option value="for_sale,pending,sold" data-i18n="home.statusBoth">{{ t_status_both }}</option>
          <option value="sold" data-i18n="home.statusSold">{{ t_status_sold }}</option>
        </select>
      </div>
      <div style="display:flex;gap:8px">
        <button type="button" id="nlToAlertBtn" class="btnsec" data-i18n="home.matchAlertBtn">{{ t_match_alert_btn }}</button>
        <button type="button" id="nlSearchBtn" class="btnpri" data-i18n="home.matchSearchBtn">{{ t_match_search_btn }}</button>
      </div>
    </div>
  </div>
  <div id="nlResults"></div>
</div>

<div id="tab-alerts" class="tabpanel">
  <div class="panelcard">
    <div style="display:flex;justify-content:space-between;align-items:flex-start;gap:12px;flex-wrap:wrap;margin-bottom:6px">
      <h2 style="font-size:18px;margin:0" data-i18n="home.alertsTitle">{{ t_alerts_title }}</h2>
      <a class="btnsec" id="manageAlertsBtn" href="/alerts" data-i18n="home.alertsManageBtn">{{ t_alerts_manage_btn }}</a>
    </div>
    <p style="margin:0 0 14px;font-size:13.5px;color:#4b5563" data-i18n="home.alertsSubtitle">{{ t_alerts_subtitle }}</p>
    <div class="geo-loc-host" id="alertsGeoLocBar"></div>
    <form id="subForm">
      <div class="row2">
        <div>
          <label class="lbl" for="subEmail" data-i18n="home.alertsEmail">{{ t_alerts_email }}</label>
          <input id="subEmail" class="tinput" type="email" placeholder="buyer@example.com" required>
        </div>
        <div>
          <label class="lbl" for="subZips" data-i18n="home.alertsZips">{{ t_alerts_zips }}</label>
          <input id="subZips" class="tinput" type="text" placeholder="5-digit US ZIP(s), comma-separated" required>
        </div>
      </div>
      <div style="margin-bottom:10px">
        <label class="lbl" for="subReq" data-i18n="home.alertsReq">{{ t_alerts_req }}</label>
        <textarea id="subReq" class="nlarea" placeholder="{{ t_alerts_req_ph }}" data-i18n-placeholder="home.alertsReqPlaceholder" required></textarea>
      </div>
      <div style="margin-bottom:10px;max-width:360px">
        <label class="lbl" for="subFreq" data-i18n="home.alertsFreqLabel">{{ t_alerts_freq_label }}</label>
        <select id="subFreq" class="stsel" style="width:100%">
          <option value="instant" data-i18n="home.alertsFreqInstant">{{ t_alerts_freq_instant }}</option>
          <option value="daily" data-i18n="home.alertsFreqDaily">{{ t_alerts_freq_daily }}</option>
          <option value="weekly" data-i18n="home.alertsFreqWeekly">{{ t_alerts_freq_weekly }}</option>
        </select>
      </div>
      <div class="chkrow">
        <label><input type="checkbox" id="subNew" checked> <span data-i18n="home.alertsNotifyNew">{{ t_alerts_notify_new }}</span></label>
        <label><input type="checkbox" id="subSold"> <span data-i18n="home.alertsNotifySold">{{ t_alerts_notify_sold }}</span></label>
      </div>
      <div class="barfoot">
        <span id="subStatusMsg" style="font-size:13px;color:#0f766e;font-weight:600"></span>
        <button type="submit" id="subSubmitBtn" class="btnpri" data-i18n="home.alertsSubmitBtn">{{ t_alerts_submit_btn }}</button>
      </div>
    </form>
  </div>

  <div class="panelcard" style="margin-top:16px">
    <div style="display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap">
      <div>
        <h3 style="font-size:15.5px;margin:0" data-i18n="home.alertsSavedTitle">{{ t_alerts_saved_title }}</h3>
        <div id="wzSummary" style="font-size:12.5px;color:#6b7280;margin-top:2px" data-i18n="home.alertsSavedSummary">{{ t_alerts_saved_summary }}</div>
      </div>
    </div>
    <div id="wzList" class="wzgrid"></div>
  </div>
</div>
<footer style="margin-top:28px;padding-top:14px;border-top:1px solid #e5e7eb;display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;font-size:12px;color:#6b7280">
  <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">
    <span><b>{{ brand_name }}</b> · {{ brand_domain }}</span>
    <span>·</span>
    <span id="footerNote">{{ footer_note }}</span>
  </div>
  <div style="display:flex;align-items:center;gap:14px">
    <a href="/deal-room" data-i18n="common.dealRoom" style="color:#6b7280;text-decoration:none;font-weight:600">{{ t_deal_room }}</a>
    <a href="/admin" style="color:#6b7280;text-decoration:none;font-weight:600">Admin Console</a>
  </div>
</footer>
</div>
<script>{{ i18next_js | safe }}</script>
<script>
window.__OPEN_ESTATE_BRAND__ = {{ brand_json | safe }};
window.__OPEN_ESTATE_INIT_LANG__ = {{ init_lang_json | safe }};
window.__OPEN_ESTATE_LANG_COOKIE__ = {{ lang_cookie_json | safe }};
window.__OPEN_ESTATE_I18N_RESOURCES__ = {
  en: { translation: {{ i18n_en_json | safe }} },
  "zh-CN": { translation: {{ i18n_zh_json | safe }} },
  es: { translation: {{ i18n_es_json | safe }} }
};
</script>
<script>{{ home_client_js | safe }}</script><script>{{ core_ui_js | safe }}</script></body></html>"###;

const HOME_CLIENT_JS: &str = r###"
(function(){
  const LANG_COOKIE = window.__OPEN_ESTATE_LANG_COOKIE__ || 'open_estate_lang';
  const i18nRes = window.__OPEN_ESTATE_I18N_RESOURCES__ || { en: { translation: {} }, 'zh-CN': { translation: {} }, es: { translation: {} } };
  const BRAND_INFO = window.__OPEN_ESTATE_BRAND__ || {};
  const BRAND_NAME = (BRAND_INFO.name && String(BRAND_INFO.name).trim()) || __HOME_BRAND_NAME_JSON__;
  const _DEF_BRAND_RE = new RegExp('Open\\s+' + 'Estate', 'g');
  const displayListingSource = (src) => {
    const s = String(src || '');
    return s ? s.replace(_DEF_BRAND_RE, BRAND_NAME) : s;
  };
  const rawInitLang = String(window.__OPEN_ESTATE_INIT_LANG__ || '').toLowerCase();
  const urlInitLang = (new URLSearchParams(location.search).get('lang') || '').toLowerCase();
  const effInitLang = urlInitLang || rawInitLang;
  let CUR_LANG = effInitLang.startsWith('zh') ? 'zh-CN' : effInitLang.startsWith('es') ? 'es' : 'en';
  if (window.i18next) {
    window.i18next.init({
      lng: CUR_LANG,
      fallbackLng: 'en',
      resources: i18nRes,
      interpolation: { escapeValue: false }
    });
  }
  const T = (key, vars, fallback) => {
    if (window.i18next && typeof window.i18next.t === 'function') {
      const out = window.i18next.t(key, vars || {});
      if (out && out !== key) return out;
    }
    return fallback || key;
  };
  const fmtCurrency = (v) => {
    try {
      return new Intl.NumberFormat(CUR_LANG === 'zh-CN' ? 'zh-CN' : CUR_LANG === 'es' ? 'es-US' : 'en-US', { style: 'currency', currency: 'USD', maximumFractionDigits: 0 }).format(Math.round(v));
    } catch { return '$' + Math.round(v).toLocaleString('en-US'); }
  };
  const fmtNum = (v) => {
    try { return new Intl.NumberFormat(CUR_LANG === 'zh-CN' ? 'zh-CN' : CUR_LANG === 'es' ? 'es-US' : 'en-US').format(v); }
    catch { return String(v); }
  };
  const US_STATE_SET = new Set([
    'AL','AK','AZ','AR','CA','CO','CT','DE','FL','GA','HI','ID','IL','IN','IA','KS',
    'KY','LA','ME','MD','MA','MI','MN','MS','MO','MT','NE','NV','NH','NJ','NM','NY',
    'NC','ND','OH','OK','OR','PA','RI','SC','SD','TN','TX','UT','VT','VA','WA','WV',
    'WI','WY','DC','PR','VI','GU','AS','MP'
  ]);
  const US_STATE_NAMES = [
    'alabama','alaska','arizona','arkansas','california','colorado','connecticut','delaware',
    'florida','georgia','hawaii','idaho','illinois','indiana','iowa','kansas','kentucky',
    'louisiana','maine','maryland','massachusetts','michigan','minnesota','mississippi',
    'missouri','montana','nebraska','nevada','new hampshire','new jersey','new mexico',
    'new york','north carolina','north dakota','ohio','oklahoma','oregon','pennsylvania',
    'rhode island','south carolina','south dakota','tennessee','texas','utah','vermont',
    'virginia','washington','west virginia','wisconsin','wyoming','district of columbia',
    'puerto rico','virgin islands','guam'
  ];
  const isUsStateToken = (raw) => {
    const t = String(raw || '').trim().replace(/\.+$/, '');
    if (t.length === 2) return US_STATE_SET.has(t.toUpperCase());
    return US_STATE_NAMES.includes(t.toLowerCase());
  };
  const hasCityAndStateInSeg = (seg) => {
    const trimmed = String(seg || '').trim();
    if (!trimmed) return false;
    const m = trimmed.match(/^(.+?)\s+([A-Za-z]{2})\.?$/);
    if (m && US_STATE_SET.has(m[2].toUpperCase()) && (m[1].match(/[A-Za-z]/g) || []).length >= 2) {
      return true;
    }
    const low = trimmed.toLowerCase();
    for (const st of US_STATE_NAMES) {
      if (low.endsWith(st)) {
        const pref = low.slice(0, low.length - st.length);
        if (/\s$/.test(pref) && (pref.match(/[a-z]/g) || []).length >= 2) return true;
      }
    }
    return false;
  };
  const addrErrMessage = (kind) => {
    if (kind === 'po_box') {
      return CUR_LANG === 'zh-CN'
        ? '不支持邮政信箱（PO Box）— 请输入具体的美国街道门牌地址。'
        : CUR_LANG === 'es'
          ? 'No se admiten apartados postales (PO Box) — ingrese una dirección física en EE. UU.'
          : 'PO Boxes are not supported — please enter a physical US street address.';
    }
    if (kind === 'missing_street') {
      return CUR_LANG === 'zh-CN'
        ? '地址不完整 — 请输入门牌号、街道名称及城市/州或 5 位邮编，或从下拉建议中选择。'
        : CUR_LANG === 'es'
          ? 'Dirección incompleta — incluya número exterior, nombre de calle y ciudad/estado o ZIP de 5 dígitos, o elija una sugerencia.'
          : 'Incomplete address — please include street number, street name, and city/state or 5-digit ZIP, or pick from the dropdown.';
    }
    return CUR_LANG === 'zh-CN'
      ? '地址不完整 — 请补充城市与州名或 5 位邮编（或从下拉列表中选择完整地址）。'
      : CUR_LANG === 'es'
        ? 'Dirección incompleta — agregue ciudad y estado o código ZIP de 5 dígitos (o elija una dirección completa de la lista).'
        : 'Incomplete address — please include city & state or 5-digit ZIP code (or select a full address from the dropdown).';
  };
  const validateCompleteUsAddressClient = (raw) => {
    const addr = String(raw || '').trim();
    if (!addr) return { ok: false, kind: 'missing_street', message: addrErrMessage('missing_street') };
    if (/^(?:p\.?\s*o\.?\s*box|post\s+office\s+box)\b/i.test(addr)) {
      return { ok: false, kind: 'po_box', message: addrErrMessage('po_box') };
    }
    const noCountry = addr.replace(/(?:,\s*|\s+)(?:USA|United\s+States(?:\s+of\s+America)?|U\.S\.A\.?)\s*$/i, '').trim();
    const hm = noCountry.match(/^(?:[NSEW]\d{1,6}[A-Za-z]?(?:[NSEW]\d{1,6}[A-Za-z]?)?|\d{1,6}[A-Za-z]?(?:-\d{1,5}[A-Za-z]?)?(?:\s+1\/2)?)\s+(.+?)$/i);
    if (!hm || !hm[1]) {
      return { ok: false, kind: 'missing_street', message: addrErrMessage('missing_street') };
    }
    const rest = hm[1].trim();
    const zm = rest.match(/^(.+?)(?:,\s*|\s+)(\d{5})(?:-\d{4})?\s*$/);
    if (zm && (zm[1].match(/[A-Za-z]/g) || []).length >= 2) {
      return { ok: true };
    }
    const parts = rest.split(',').map((s) => s.trim()).filter(Boolean);
    if (parts.length >= 2 && (parts[0].match(/[A-Za-z]/g) || []).length >= 2) {
      if (parts.length >= 3) {
        const last = parts[parts.length - 1];
        const prev = parts[parts.length - 2];
        if ((prev.match(/[A-Za-z]/g) || []).length >= 2 && isUsStateToken(last)) {
          return { ok: true };
        }
      }
      if (hasCityAndStateInSeg(parts[parts.length - 1])) {
        return { ok: true };
      }
    }
    const streetAlpha = (rest.match(/[A-Za-z]/g) || []).length;
    const kind = streetAlpha < 2 ? 'missing_street' : 'missing_location';
    return { ok: false, kind, message: addrErrMessage(kind) };
  };

  const inp = document.getElementById('homeInput'), form = document.getElementById('homeForm'), box = document.getElementById('homeAc'), clr = document.getElementById('homeClr'), addrErrEl = document.getElementById('homeAddrErr');
  let activeErrKind = null;
  const showHomeAddrErr = (kind, customMsg) => {
    activeErrKind = kind || 'missing_location';
    if (addrErrEl) {
      addrErrEl.textContent = customMsg || addrErrMessage(activeErrKind);
      addrErrEl.classList.add('on');
    }
    if (form) form.classList.add('invalid');
  };
  const clearHomeAddrErr = () => {
    activeErrKind = null;
    if (addrErrEl) {
      addrErrEl.textContent = '';
      addrErrEl.classList.remove('on');
    }
    if (form) form.classList.remove('invalid');
  };

  const applyDomTranslations = () => {
    document.documentElement.lang = CUR_LANG;
    document.title = T('home.title', null, document.title);
    document.querySelectorAll('[data-i18n]').forEach((el) => {
      const k = el.getAttribute('data-i18n');
      if (!k) return;
      el.textContent = T(k, null, el.textContent);
    });
    document.querySelectorAll('[data-i18n-placeholder]').forEach((el) => {
      const k = el.getAttribute('data-i18n-placeholder');
      if (!k) return;
      el.setAttribute('placeholder', T(k, null, el.getAttribute('placeholder') || ''));
    });
    document.querySelectorAll('#langSwitch button[data-lang]').forEach((b) => {
      b.classList.toggle('on', b.getAttribute('data-lang') === CUR_LANG);
    });
    if (activeErrKind && addrErrEl && addrErrEl.classList.contains('on')) {
      addrErrEl.textContent = addrErrMessage(activeErrKind);
    }
  };
  const setLang = (next) => {
    CUR_LANG = (next === 'zh-CN' || next === 'zh') ? 'zh-CN' : next === 'es' ? 'es' : 'en';
    document.cookie = LANG_COOKIE + '=' + encodeURIComponent(CUR_LANG) + ';path=/;max-age=31536000;SameSite=Lax';
    if (window.i18next) window.i18next.changeLanguage(CUR_LANG);
    applyDomTranslations();
    if (typeof switchUiLang === 'function') switchUiLang(CUR_LANG);
    renderRecent(false);
    loadWatchedZips();
    try {
      const u = new URL(location.href);
      u.searchParams.set('lang', CUR_LANG === 'zh-CN' ? 'zh' : CUR_LANG);
      history.replaceState(null, '', u.toString());
    } catch {}
  };
  document.querySelectorAll('#langSwitch button[data-lang]').forEach((b) => {
    b.addEventListener('click', () => setLang(b.getAttribute('data-lang')));
  });

  const RECENT_KEY = 'openestate:recent_reports:v1';
  const PENDING_KEY = 'openestate:pending_reports:v1';
  const PENDING_TTL_MS = 86400000;
  const escH = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const slugifyLocal = (s) => String(s || '').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '').slice(0, 80);
  const readRecent = () => {
    try {
      const arr = JSON.parse(localStorage.getItem(RECENT_KEY) || '[]');
      return Array.isArray(arr) ? arr.filter((x) => x && x.slug && x.address) : [];
    } catch { return []; }
  };
  const writeRecent = (arr) => {
    try {
      if (arr && arr.length) localStorage.setItem(RECENT_KEY, JSON.stringify(arr.slice(0, 12)));
      else localStorage.removeItem(RECENT_KEY);
    } catch {}
  };
  const readPending = () => {
    try {
      const now = Date.now();
      const arr = JSON.parse(localStorage.getItem(PENDING_KEY) || '[]');
      if (!Array.isArray(arr)) return [];
      const valid = arr.filter((x) => x && x.address && validateCompleteUsAddressClient(x.address).ok && (x.rawSlug || x.slug || x.jobId) && (now - (Number(x.startedAt) || now) <= PENDING_TTL_MS));
      if (valid.length !== arr.length) {
        if (valid.length) localStorage.setItem(PENDING_KEY, JSON.stringify(valid.slice(0, 12)));
        else localStorage.removeItem(PENDING_KEY);
      }
      return valid;
    } catch { return []; }
  };
  const writePending = (arr) => {
    try {
      if (arr && arr.length) localStorage.setItem(PENDING_KEY, JSON.stringify(arr.slice(0, 12)));
      else localStorage.removeItem(PENDING_KEY);
    } catch {}
  };
  const matchPendingItem = (x, ref) => {
    if (!x || !ref) return false;
    if (ref.jobId && x.jobId && x.jobId === ref.jobId) return true;
    if (ref.slug && (x.slug === ref.slug || x.rawSlug === ref.slug)) return true;
    if (ref.rawSlug && (x.rawSlug === ref.rawSlug || x.slug === ref.rawSlug)) return true;
    return false;
  };
  const upsertPending = (patch) => {
    try {
      const cleanAddr = String(patch.address || '').trim();
      const rawSl = patch.rawSlug || slugifyLocal(cleanAddr);
      if (cleanAddr.length < 5 || !rawSl || !validateCompleteUsAddressClient(cleanAddr).ok) return;
      const prev = readPending();
      const ex = prev.find((x) => matchPendingItem(x, { jobId: patch.jobId, slug: patch.slug, rawSlug: rawSl }));
      const merged = Object.assign(
        { jobId: null, address: cleanAddr, slug: null, rawSlug: rawSl, step: null, startedAt: Date.now(), reEnqueued: false },
        ex || {},
        Object.fromEntries(Object.entries(patch).filter(([, v]) => v !== undefined && v !== null && v !== '')),
        { rawSlug: (ex && ex.rawSlug) || rawSl, startedAt: (ex && ex.startedAt) || patch.startedAt || Date.now() }
      );
      const rest = prev.filter((x) => !matchPendingItem(x, merged));
      writePending([merged, ...rest]);
    } catch {}
  };
  const savePendingRecent = (addr, exactSlug) => {
    try {
      const clean = String(addr || '').trim();
      if (clean.length < 5 || !validateCompleteUsAddressClient(clean).ok) return;
      const sl = exactSlug || slugifyLocal(clean);
      if (!sl) return;
      upsertPending({ address: clean, rawSlug: slugifyLocal(clean), slug: exactSlug || null, startedAt: Date.now() });
      const prev = readRecent();
      const ex = prev.find((x) => x.slug === sl);
      if (ex && ex.value != null) {
        writeRecent([Object.assign({}, ex, { viewedAt: new Date().toISOString() }), ...prev.filter((x) => x.slug !== sl)]);
      }
    } catch {}
  };
  const promoteFetchedReport = (rep, fallbackAddr, extraSlugs, completedInBg) => {
    if (!rep || !rep.meta || !rep.meta.slug) return false;
    const canonSlug = rep.meta.slug;
    const dispAddr = rep.meta.displayAddress || fallbackAddr || canonSlug;
    const val = rep.modules && rep.modules.valuation && rep.modules.valuation.data && rep.modules.valuation.data.estimate ? rep.modules.valuation.data.estimate.value : null;
    const overall = rep.summary && rep.summary.scores ? rep.summary.scores.overall : null;
    const aiPend = Boolean(rep.meta.aiPending);
    const verdict = aiPend ? T('home.recentGenerating') : (rep.summary && rep.summary.verdict ? rep.summary.verdict : T('home.recentViewReport'));
    const clearSet = new Set([canonSlug, slugifyLocal(dispAddr), ...(extraSlugs || [])].filter(Boolean));
    const prev = readRecent();
    const prevEx = prev.find((x) => clearSet.has(x.slug));
    const entry = {
      slug: canonSlug,
      address: dispAddr,
      value: val != null ? val : (prevEx ? prevEx.value : null),
      overall: overall != null ? overall : (prevEx ? prevEx.overall : null),
      verdict,
      pending: aiPend,
      completedInBackground: Boolean(completedInBg || (prevEx && prevEx.completedInBackground)),
      viewedAt: new Date().toISOString(),
    };
    writeRecent([entry, ...prev.filter((x) => !clearSet.has(x.slug))]);
    return true;
  };

  let hydrateTimer = null, reconcileTimer = null, reconciling = false;
  const fetchJsonTimeout = async (url, timeoutMs = 6000, opts = {}) => {
    const c = new AbortController();
    const t = setTimeout(() => c.abort(), timeoutMs);
    try {
      const r = await fetch(url, Object.assign({ cache: 'no-store', signal: c.signal }, opts));
      return r;
    } finally {
      clearTimeout(t);
    }
  };

  const reconcilePendingReports = async () => {
    if (reconciling) return;
    const list = readPending();
    if (!list.length) return;
    reconciling = true;
    let changed = false, stillBuilding = false, hadNetworkDrop = false;
    const nextPending = [];
    try {
      for (const p of list) {
        const keys = [...new Set([p.jobId, p.slug, p.rawSlug].filter(Boolean))];
        let jobData = null, got404 = false;
        for (const k of keys) {
          try {
            const jr = await fetchJsonTimeout('/api/job/' + encodeURIComponent(k) + '?t=' + Date.now(), 6000);
            if (jr.ok) {
              jobData = await jr.json();
              break;
            }
            if (jr.status === 404) got404 = true;
          } catch {
            hadNetworkDrop = true;
            break;
          }
        }
        if (hadNetworkDrop && !jobData) {
          nextPending.push(p);
          stillBuilding = true;
          continue;
        }
        if (jobData) {
          if (jobData.error) {
            const dropSlugs = new Set([jobData.slug, p.slug, p.rawSlug].filter(Boolean));
            const cleanedRecent = readRecent().filter((r) => !(dropSlugs.has(r.slug) && r.value == null));
            writeRecent(cleanedRecent);
            changed = true;
            continue;
          }
          const targetSlug = jobData.slug || p.slug || p.rawSlug;
          if ((jobData.ready || jobData.done) && targetSlug) {
            try {
              const rr = await fetchJsonTimeout('/r/' + encodeURIComponent(targetSlug) + '/report.json?t=' + Date.now(), 7000);
              if (rr.ok) {
                const rep = await rr.json();
                if (promoteFetchedReport(rep, jobData.address || p.address, [targetSlug, p.slug, p.rawSlug], true)) {
                  changed = true;
                  continue;
                }
              }
            } catch {
              hadNetworkDrop = true;
            }
          }
          const updated = Object.assign({}, p, {
            jobId: jobData.id || p.jobId,
            slug: jobData.slug || p.slug,
            address: jobData.address || p.address,
            step: jobData.step || p.step,
          });
          if (updated.step !== p.step || updated.slug !== p.slug || updated.jobId !== p.jobId) changed = true;
          nextPending.push(updated);
          stillBuilding = true;
          continue;
        }
        if (got404) {
          const trySlug = p.slug || p.rawSlug;
          let promoted = false;
          if (trySlug) {
            try {
              const rr = await fetchJsonTimeout('/r/' + encodeURIComponent(trySlug) + '/report.json?t=' + Date.now(), 6000);
              if (rr.ok) {
                const rep = await rr.json();
                promoted = promoteFetchedReport(rep, p.address, [trySlug, p.rawSlug], true);
                if (promoted) changed = true;
              }
            } catch {
              hadNetworkDrop = true;
            }
          }
          if (promoted) continue;
          if (!p.reEnqueued && navigator.onLine !== false) {
            try {
              const er = await fetchJsonTimeout('/api/report/enqueue', 6000, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ address: p.address }),
              });
              if (er.ok) {
                const ej = await er.json();
                nextPending.push(Object.assign({}, p, {
                  reEnqueued: true,
                  jobId: ej.jobId || p.jobId,
                  slug: ej.slug || p.slug,
                  step: ej.step || T('home.recentBgServer'),
                }));
                changed = true;
                stillBuilding = true;
                continue;
              }
              if (er.status === 400 || er.status === 422) {
                changed = true;
                continue;
              }
            } catch {
              hadNetworkDrop = true;
            }
          }
          if (hadNetworkDrop || Date.now() - (Number(p.startedAt) || 0) < 45000) {
            nextPending.push(p);
            stillBuilding = true;
          } else {
            changed = true;
          }
        }
      }
      writePending(nextPending);
    } finally {
      reconciling = false;
    }
    if (changed) renderRecent(false);
    if (stillBuilding) {
      clearTimeout(reconcileTimer);
      reconcileTimer = setTimeout(reconcilePendingReports, hadNetworkDrop ? 3500 : 2000);
    }
  };

  const hydratePendingCards = async () => {
    const list = readRecent();
    const pending = list.filter((r) => r.pending || r.value == null);
    if (!pending.length) return;
    let changed = false, stillPending = false;
    for (const r of pending.slice(0, 4)) {
      try {
        const res = await fetchJsonTimeout('/r/' + encodeURIComponent(r.slug) + '/report.json?t=' + Date.now(), 6000);
        if (!res.ok) { stillPending = true; continue; }
        const rep = await res.json();
        if (rep && rep.meta && rep.meta.slug) {
          r.slug = rep.meta.slug;
          r.address = rep.meta.displayAddress || r.address;
          r.value = rep.modules && rep.modules.valuation && rep.modules.valuation.data && rep.modules.valuation.data.estimate ? rep.modules.valuation.data.estimate.value : r.value;
          r.overall = rep.summary && rep.summary.scores ? rep.summary.scores.overall : r.overall;
          r.verdict = rep.meta.aiPending ? T('home.recentGenerating') : (rep.summary && rep.summary.verdict ? rep.summary.verdict : T('home.recentViewReport'));
          r.pending = Boolean(rep.meta.aiPending);
          if (r.pending) stillPending = true;
          changed = true;
        }
      } catch { stillPending = true; }
    }
    if (changed) {
      writeRecent(list);
      renderRecent(false);
    }
    if (stillPending) {
      clearTimeout(hydrateTimer);
      hydrateTimer = setTimeout(hydratePendingCards, 3500);
    }
  };

  const renderRecent = (checkPending = true) => {
    const wrap = document.getElementById('userRecentReports');
    if (!wrap) return;
    const recentList = readRecent();
    const readySlugs = new Set(recentList.filter((r) => r.value != null || !r.pending).flatMap((r) => [r.slug, slugifyLocal(r.address)]));
    const activePending = readPending().filter((p) => !readySlugs.has(p.slug) && !readySlugs.has(p.rawSlug));
    const pendingRawSet = new Set(activePending.flatMap((p) => [p.slug, p.rawSlug]).filter(Boolean));
    const visibleRecent = recentList.filter((r) => !(r.value == null && pendingRawSet.has(r.slug)));
    if (!activePending.length && !visibleRecent.length) { wrap.innerHTML = ''; return; }

    const liveCardsHtml = activePending.map((p) => {
      const rmKey = p.rawSlug || p.slug || slugifyLocal(p.address);
      return '<div class="cardwrap"><a class="card live" href="/report?address=' + encodeURIComponent(p.address) + '">' +
        '<div class="bgbadge bg-live">' + escH(T('home.recentBuilding')) + '</div>' +
        '<div class="a">' + escH(p.address) + '</div>' +
        '<div class="v">' + escH(p.step || T('home.recentBgServer')) + '</div>' +
        '<div class="s">' + escH(T('home.recentAutoUpdates')) + '</div>' +
      '</a><button type="button" class="cardrm" data-rm-pending="' + escH(rmKey) + '" title="Dismiss" aria-label="Remove">×</button></div>';
    }).join('');

    const recentCardsHtml = visibleRecent.map((r) =>
      '<div class="cardwrap"><a class="card" href="/r/' + encodeURIComponent(r.slug) + '/" data-open-slug="' + escH(r.slug) + '">' +
        (r.completedInBackground ? '<div class="bgbadge bg-done">' + escH(T('home.recentCompletedBg')) + '</div>' : '') +
        '<div class="a">' + escH(r.address) + '</div>' +
        '<div class="v">' + (r.value ? escH(fmtCurrency(r.value)) : (r.pending ? escH(T('home.recentGenerating')) : escH(T('home.recentViewReport')))) + '</div>' +
        '<div class="s">' + (r.overall != null ? escH(T('home.recentScorePrefix', { score: r.overall })) : '') + escH(r.verdict || '') + '</div>' +
      '</a><button type="button" class="cardrm" data-rm-slug="' + escH(r.slug) + '" title="Remove" aria-label="Remove">×</button></div>'
    ).join('');

    wrap.innerHTML =
      '<div class="rhead"><h2 style="font-size:16px;margin:0">' + escH(T('home.recentTitle')) + '</h2><button type="button" class="clrhist" id="clearRecentBtn">' + escH(T('home.recentClear')) + '</button></div>' +
      '<div class="grid">' + liveCardsHtml + recentCardsHtml + '</div>';

    const clrBtn = document.getElementById('clearRecentBtn');
    if (clrBtn) {
      clrBtn.addEventListener('click', () => {
        try {
          localStorage.removeItem(RECENT_KEY);
          localStorage.removeItem(PENDING_KEY);
        } catch {}
        try {
          fetch('/api/deal-room/history/clear', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: '{}',
            keepalive: true,
          }).catch(() => {});
        } catch {}
        renderRecent(false);
      });
    }
    wrap.querySelectorAll('[data-rm-pending]').forEach((btn) => {
      btn.addEventListener('click', (ev) => {
        ev.preventDefault();
        ev.stopPropagation();
        const rk = btn.getAttribute('data-rm-pending');
        writePending(readPending().filter((x) => !matchPendingItem(x, { slug: rk, rawSlug: rk })));
        renderRecent(false);
      });
    });
    wrap.querySelectorAll('[data-rm-slug]').forEach((btn) => {
      btn.addEventListener('click', (ev) => {
        ev.preventDefault();
        ev.stopPropagation();
        const slug = btn.getAttribute('data-rm-slug');
        writeRecent(readRecent().filter((x) => x.slug !== slug));
        if (slug) {
          try {
            fetch('/api/deal-room/history/remove', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ slug }),
              keepalive: true,
            }).catch(() => {});
          } catch {}
        }
        renderRecent(false);
      });
    });
    wrap.querySelectorAll('[data-open-slug]').forEach((a) => {
      a.addEventListener('click', () => {
        const sl = a.getAttribute('data-open-slug');
        const next = readRecent().map((x) => x.slug === sl ? Object.assign({}, x, { completedInBackground: false }) : x);
        writeRecent(next);
      });
    });
    if (checkPending) {
      hydratePendingCards();
      reconcilePendingReports();
    }
  };
  applyDomTranslations();
  renderRecent(true);

  const enqueueAndTrackAddress = (addr) => {
    const clean = String(addr || '').trim();
    if (clean.length < 5 || !validateCompleteUsAddressClient(clean).ok) return;
    const rawSl = slugifyLocal(clean);
    savePendingRecent(clean, null);
    try {
      fetch('/api/report/enqueue', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ address: clean }),
        keepalive: true,
      }).then((r) => {
        if (r.status === 400 || r.status === 422) {
          writePending(readPending().filter((x) => !matchPendingItem(x, { slug: rawSl, rawSlug: rawSl })));
          renderRecent(false);
          return null;
        }
        return r.ok ? r.json() : null;
      }).then((j) => {
        if (j && (j.jobId || j.slug)) {
          upsertPending({ address: clean, rawSlug: rawSl, jobId: j.jobId || null, slug: j.slug || null, step: j.step || null });
        }
      }).catch(() => {});
    } catch {}
  };
  let items = [], idx = -1, tmr = null, ctrl = null;
  const close = () => { box.classList.remove('open'); box.innerHTML = ''; idx = -1; };
  const hi = (text, q) => {
    const terms = q.trim().toLowerCase().split(/\s+/).filter(Boolean);
    if (!terms.length) return escH(text);
    const re = new RegExp('(' + terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|') + ')', 'ig');
    return escH(text).replace(re, '<mark>$1</mark>');
  };
  const pick = (it) => {
    if (!it) return;
    clearTimeout(tmr); if (ctrl) ctrl.abort();
    inp.value = it.full; clr.classList.add('on'); clearHomeAddrErr(); items = []; close();
    if (it.viewed && it.slug) {
      location.href = '/r/' + encodeURIComponent(it.slug) + '/';
      return;
    }
    inp.focus();
    try { const len = inp.value.length; inp.setSelectionRange(len, len); } catch {}
  };
  const render = (q) => {
    if (!items.length) {
      if (q && q.trim().length >= 5) {
        const emptyMsg = CUR_LANG === 'zh-CN'
          ? '未找到匹配的美国街道地址 — 请核对门牌号、街道名称与邮编。'
          : CUR_LANG === 'es'
            ? 'No se encontraron direcciones en EE. UU. — verifique número, calle y código ZIP.'
            : 'No matching US street addresses found — check the street number, street name, and ZIP code.';
        box.innerHTML = '<div class="acempty">' + escH(emptyMsg) + '</div>';
        box.classList.add('open');
      } else {
        close();
      }
      return;
    }
    const recentMap = new Map(readRecent().flatMap((r) => [[r.slug, r.slug], [slugifyLocal(r.address), r.slug]]));
    items = items.map((it) => {
      const sl = recentMap.get(slugifyLocal(it.full));
      return sl ? Object.assign({}, it, { viewed: true, slug: sl }) : it;
    });
    const pin = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M20 10c0 6-8 12-8 12S4 16 4 10a8 8 0 1 1 16 0Z"/><circle cx="12" cy="10" r="3"/></svg>';
    box.innerHTML = items.map((it, i) => '<div class="aci' + (i === idx ? ' act' : '') + '" data-i="' + i + '" role="option"><span class="pin">' + pin + '</span><div class="txt"><div class="pri">' + hi(it.primary, q) + '</div>' + (it.secondary ? '<div class="sec">' + escH(it.secondary) + '</div>' : '') + '</div>' + (it.viewed ? '<span class="tag">' + escH(T('home.acViewed')) + '</span>' : '') + '</div>').join('') +
      '<div class="acfoot"><span>' + escH(T('home.acNavHint')) + '</span><span>' + escH(T('home.acUsBadge')) + '</span></div>';
    box.classList.add('open');
    box.querySelectorAll('.aci').forEach((el) => {
      const it = items[+el.dataset.i];
      el.addEventListener('mousedown', (ev) => { ev.preventDefault(); pick(it); });
      el.addEventListener('click', (ev) => { ev.preventDefault(); if (box.classList.contains('open')) pick(it); });
    });
  };
  let activeGeo = null; // { postalCode, city, regionCode, lat, lon, metro, source, mode: 'ip'|'gps'|'manual' }
  let geoClearedByUser = false;
  let geoResolvedOnce = false;
  let geoEditingHostId = null;
  let shopScopeMode = 'auto'; // 'auto' | 'zip' | 'all'
  let shopAutoGeoPendingCheck = false;
  let shopDetectedZipZeroLocal = false;
  let autoFilledSyncZip = '';
  let autoFilledSubZip = '';
  let autoSeededMatchZip = '';
  const shopSpecsById = new Map();
  const hydratedSpecZips = new Set();

  const buildSuggestGeoBiasQs = () => {
    if (geoClearedByUser || !activeGeo) return '';
    const parts = [];
    if (typeof activeGeo.lat === 'number' && Number.isFinite(activeGeo.lat) && typeof activeGeo.lon === 'number' && Number.isFinite(activeGeo.lon)) {
      parts.push('lat=' + encodeURIComponent(activeGeo.lat.toFixed(5)));
      parts.push('lon=' + encodeURIComponent(activeGeo.lon.toFixed(5)));
    }
    if (activeGeo.regionCode && /^[A-Za-z]{2}$/.test(activeGeo.regionCode)) {
      parts.push('state=' + encodeURIComponent(activeGeo.regionCode.toUpperCase()));
    }
    return parts.length ? ('&' + parts.join('&')) : '';
  };

  const cacheShopListingSpecs = (rows) => {
    if (!Array.isArray(rows)) return;
    for (const r of rows) {
      if (!r || typeof r !== 'object') continue;
      if (r.id) shopSpecsById.set(String(r.id), r);
      if (r.address) shopSpecsById.set('addr:' + String(r.address).trim().toLowerCase(), r);
    }
  };

  const hydrateShopSpecsForZips = async (rawZips) => {
    const need = [...new Set((rawZips || []).map((z) => String(z || '').trim().slice(0, 5)).filter((z) => /^\d{5}$/.test(z) && !hydratedSpecZips.has(z)))].slice(0, 6);
    if (!need.length) return false;
    need.forEach((z) => hydratedSpecZips.add(z));
    try {
      const r = await fetchJsonTimeout('/api/listings/shop?zips=' + encodeURIComponent(need.join(',')) + '&statuses=all&limit=120', 2800);
      if (!r.ok) return false;
      const j = await r.json();
      if (j && Array.isArray(j.listings) && j.listings.length) {
        cacheShopListingSpecs(j.listings);
        return true;
      }
    } catch {}
    return false;
  };

  const fetchSuggestNow = (q) => {
    clearTimeout(tmr); if (ctrl) ctrl.abort();
    if (q.trim().length < 3) { items = []; close(); return; }
    tmr = setTimeout(async () => {
      ctrl = new AbortController();
      try {
        const r = await fetch('/api/suggest?q=' + encodeURIComponent(q.trim()) + buildSuggestGeoBiasQs(), { signal: ctrl.signal }).then((x) => x.json());
        if (inp.value.trim() !== q.trim()) return;
        items = (r && (r.suggestions || r.items)) || []; idx = -1; render(q);
      } catch {}
    }, 140);
  };
  form.addEventListener('submit', (e) => {
    const chk = validateCompleteUsAddressClient(inp.value);
    if (!chk.ok) {
      e.preventDefault();
      showHomeAddrErr(chk.kind, chk.message);
      inp.focus();
      if (items.length && inp.value.trim().length >= 3) box.classList.add('open');
      else if (inp.value.trim().length >= 3) fetchSuggestNow(inp.value);
      return;
    }
    clearHomeAddrErr();
    enqueueAndTrackAddress(inp.value);
  });
  window.addEventListener('pageshow', () => renderRecent(true));
  window.addEventListener('online', () => { reconcilePendingReports(); hydratePendingCards(); });
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      renderRecent(false);
      reconcilePendingReports();
      hydratePendingCards();
    }
  });
  inp.addEventListener('input', () => {
    const q = inp.value; clr.classList.toggle('on', q.length > 0);
    if (activeErrKind && validateCompleteUsAddressClient(q).ok) clearHomeAddrErr();
    fetchSuggestNow(q);
  });
  inp.addEventListener('focus', () => {
    if (items.length && inp.value.trim().length >= 3) box.classList.add('open');
  });
  inp.addEventListener('keydown', (e) => {
    if (!box.classList.contains('open') || !items.length) return;
    if (e.key === 'ArrowDown') { e.preventDefault(); idx = (idx + 1) % items.length; render(inp.value); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); idx = (idx - 1 + items.length) % items.length; render(inp.value); }
    else if (e.key === 'Enter' && idx >= 0 && items[idx]) { e.preventDefault(); pick(items[idx]); }
    else if (e.key === 'Escape') close();
  });
  clr.addEventListener('click', () => { clearTimeout(tmr); if (ctrl) ctrl.abort(); inp.value = ''; clr.classList.remove('on'); clearHomeAddrErr(); items = []; close(); inp.focus(); });
  document.addEventListener('mousedown', (e) => { if (!e.target.closest('.sbox')) close(); });

  const tabBtns = document.querySelectorAll('[data-tab-target]');
  const panels = document.querySelectorAll('.tabpanel');
  const MY_ALERTS_KEY = 'openestate:my_alerts:v1';
  const wzSummary = document.getElementById('wzSummary');
  const wzList = document.getElementById('wzList');
  function readMyAlerts() {
    try {
      const a = JSON.parse(localStorage.getItem(MY_ALERTS_KEY) || '[]');
      return Array.isArray(a) ? a.filter((x) => x && x.email && Array.isArray(x.zips)) : [];
    } catch { return []; }
  }
  function alertFreqBadgeLabel(freq) {
    const f = String(freq || 'instant').toLowerCase();
    if (f === 'daily') return T('home.alertsFreqDaily', null, 'Daily digest');
    if (f === 'weekly') return T('home.alertsFreqWeekly', null, 'Weekly digest');
    return T('home.alertsFreqInstant', null, 'Instant');
  }
  function loadWatchedZips() {
    const list = readMyAlerts();
    wzSummary.textContent = list.length ? T('home.alertsSavedCount', { count: list.length }) : T('home.alertsSavedSummary');
    wzList.innerHTML = list.length
      ? list.map((a, i) => {
          const tok = String(a.unsubscribeToken || '').trim();
          const manageHref = tok ? ('/alerts?token=' + encodeURIComponent(tok)) : '/alerts';
          const freqPill = '<span style="display:inline-block;font-size:10.5px;font-weight:700;padding:1px 7px;border-radius:999px;background:#effcf8;border:1px solid #99f6e4;color:#0f766e">' + escH(alertFreqBadgeLabel(a.frequency)) + '</span>';
          return '<div class="wzcard"><div style="display:flex;justify-content:space-between;gap:6px;font-weight:700;align-items:center"><span>ZIP ' + escH(a.zips.join(', ')) + '</span><button type="button" data-rm-alert="' + i + '" style="border:0;background:none;color:#9ca3af;cursor:pointer;font-weight:700">×</button></div><div style="color:#4b5563;font-size:12px;margin-top:3px;display:flex;align-items:center;gap:6px;flex-wrap:wrap"><span>' + escH(a.email) + '</span>' + freqPill + '</div><div style="color:#6b7280;font-size:11.5px;margin-top:4px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">' + escH(a.requirements || '') + '</div><div style="margin-top:8px"><a class="btnsec" style="padding:4px 9px;font-size:11.5px" href="' + escH(manageHref) + '">' + escH(T('home.alertsManageBtn', null, 'Manage Saved Alerts →')) + '</a></div></div>';
        }).join('')
      : '<div style="font-size:13px;color:#6b7280;grid-column:1/-1">' + escH(T('home.alertsNoSaved')) + '</div>';
    wzList.querySelectorAll('[data-rm-alert]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const i = Number(btn.getAttribute('data-rm-alert'));
        try {
          const next = readMyAlerts().filter((_, j) => j !== i);
          if (next.length) localStorage.setItem(MY_ALERTS_KEY, JSON.stringify(next));
          else localStorage.removeItem(MY_ALERTS_KEY);
        } catch {}
        loadWatchedZips();
      });
    });
  }
  const firstCarrierItem = (raw) => {
    if (Array.isArray(raw)) return String(raw[0] || '').trim();
    if (typeof raw === 'string') return String(raw.split(',')[0] || '').trim();
    return '';
  };
  const renderUtilityFiberPills = (m, compact) => {
    if (!m || typeof m !== 'object') return '';
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const pills = [];
    const rawAllIn = m.allInMonthly != null ? Number(m.allInMonthly) : (m.estAllInMonthly != null ? Number(m.estAllInMonthly) : NaN);
    if (Number.isFinite(rawAllIn) && rawAllIn > 0) {
      pills.push('<span class="upill upill-mo">~' + escH(fmtCurrency(rawAllIn)) + escH(L('/mo all-in', '/月全包', '/mes total')) + '</span>');
    }
    const rawUtil = m.estMonthlyUtilities != null ? Number(m.estMonthlyUtilities) : NaN;
    if (!compact && Number.isFinite(rawUtil) && rawUtil > 0) {
      pills.push('<span class="upill upill-util">~' + escH(fmtCurrency(rawUtil)) + escH(L('/mo utils', '/月水电燃气', '/mes servicios')) + '</span>');
    }
    const elecCarrier = String(m.electricCarrier || '').trim();
    const elecRate = (typeof m.electricRateKwh === 'number' && Number.isFinite(m.electricRateKwh) && m.electricRateKwh > 0) ? m.electricRateKwh : null;
    const isMuniElec = m.electricUtilityType === 'municipal';
    if (elecCarrier || elecRate != null) {
      const rateStr = elecRate != null ? ('$' + elecRate.toFixed(3).replace(/(\.\d\d)0$/, '$1') + '/kWh') : '';
      const eParts = [elecCarrier || L('Electric', '电力', 'Electricidad'), rateStr].filter(Boolean);
      pills.push('<span class="upill ' + (isMuniElec ? 'upill-muni' : 'upill-elec') + '">⚡ ' + escH(eParts.join(' · ')) + '</span>');
    }
    const hasFiber = m.fiberAvailable === true || m.fiberAvailable === 1;
    const topIsp = String(m.internetTopCarrier || '').trim() || (hasFiber ? firstCarrierItem(m.fiberCarriers) : '') || firstCarrierItem(m.internetCarriers);
    const dlMbps = (typeof m.maxDownloadMbps === 'number' && Number.isFinite(m.maxDownloadMbps) && m.maxDownloadMbps > 0) ? m.maxDownloadMbps : null;
    if (hasFiber || topIsp || dlMbps != null) {
      let spd = '';
      if (dlMbps != null) {
        if (dlMbps >= 1000) {
          const g = dlMbps / 1000;
          spd = (Number.isInteger(g) ? String(g) : g.toFixed(1).replace(/\.0$/, '')) + ' Gbps';
        } else {
          spd = Math.round(dlMbps) + ' Mbps';
        }
      }
      if (hasFiber) spd = spd ? (spd + ' FTTH') : 'FTTH';
      const iParts = [topIsp || (hasFiber ? L('Fiber', '光纤', 'Fibra') : L('Broadband', '宽带', 'Banda ancha')), spd].filter(Boolean);
      pills.push('<span class="upill ' + (hasFiber ? 'upill-fiber' : 'upill-inet') + '">🌐 ' + escH(iParts.join(' · ')) + '</span>');
    }
    if (!compact) {
      const wSrc = String(m.waterSourceType || m.waterSource || '').trim().toLowerCase();
      const sSrc = String(m.sewerType || '').trim().toLowerCase();
      if (wSrc === 'municipal' && sSrc === 'municipal') {
        pills.push('<span class="upill upill-wst">💧 ' + escH(L('Muni Water & Sewer', '市政供水与排污', 'Agua y Alcantarillado Muni')) + '</span>');
      } else if (wSrc || sSrc) {
        const cap = (w) => w.charAt(0).toUpperCase() + w.slice(1);
        const wsTxt = [wSrc ? (cap(wSrc) + ' ' + L('Water', '供水', 'Agua')) : null, sSrc ? (cap(sSrc) + ' ' + L('Sewer', '排污', 'Alcantarillado')) : null].filter(Boolean).join(' · ');
        pills.push('<span class="upill upill-wst">💧 ' + escH(wsTxt) + '</span>');
      }
      if (m.hasSpecialTax === true || m.hasSpecialTax === 1) {
        pills.push('<span class="upill upill-warn">⚠ ' + escH(L('Special Tax District', '含特别附加税', 'Impuesto Especial')) + '</span>');
      } else if (m.hasSpecialTax === false || m.hasSpecialTax === 0) {
        pills.push('<span class="upill upill-ok">✓ ' + escH(L('No Special Tax', '无附加特别税', 'Sin Impuesto Especial')) + '</span>');
      }
    }
    if (m.hasSolar === true || m.hasSolar === 1) pills.push('<span class="upill upill-hw">☀ ' + escH(L('Solar', '太阳能', 'Solar')) + '</span>');
    if (m.hasEvCharger === true || m.hasEvCharger === 1) pills.push('<span class="upill upill-hw">🔌 ' + escH(L('EV Charger', '充电桩', 'Cargador EV')) + '</span>');
    if (!compact && (m.hasHeatPump === true || m.hasHeatPump === 1)) pills.push('<span class="upill upill-hw">♨ ' + escH(L('Heat Pump', '热泵空调', 'Bomba de Calor')) + '</span>');
    if (m.hasAdu === true || m.hasAdu === 1) pills.push('<span class="upill upill-hw">🏠 ' + escH(L('Existing ADU', '现成 ADU', 'ADU Existente')) + '</span>');
    else if (!compact && (m.aduFeasible === true || m.aduFeasible === 1)) pills.push('<span class="upill upill-hw">📐 ' + escH(L('ADU-Eligible', '可建 ADU', 'Apto ADU')) + '</span>');
    if (!compact && (m.hasPool === true || m.hasPool === 1)) pills.push('<span class="upill upill-hw">🏊 ' + escH(L('Pool', '泳池', 'Piscina')) + '</span>');
    if (!compact && typeof m.garageSpaces === 'number' && m.garageSpaces >= 2) pills.push('<span class="upill upill-hw">🚗 ' + escH(m.garageSpaces + '+ ' + L('Garage', '车库', 'Garaje')) + '</span>');
    return pills.length ? '<div class="util-pills">' + pills.join('') + '</div>' : '';
  };

  // ============================================================================================
  // Amazon-Style Faceted Home Shopping (/shop & #tab-shop) — 100% Real DB Listings Only
  // ============================================================================================
  const shopSidebar = document.getElementById('shopSidebar');
  const shopMobileFilterBtn = document.getElementById('shopMobileFilterBtn');
  const shopSearchInput = document.getElementById('shopSearchInput');
  const shopLocQuickInput = document.getElementById('shopLocQuickInput');
  const shopStatusSelect = document.getElementById('shopStatusSelect');
  const shopSortSelect = document.getElementById('shopSortSelect');
  const shopApplyBtn = document.getElementById('shopApplyBtn');
  const shopActiveChips = document.getElementById('shopActiveChips');
  const shopMetaRow = document.getElementById('shopMetaRow');
  const shopResultsHost = document.getElementById('shopResultsHost');
  const sfFiberOnly = document.getElementById('sfFiberOnly');
  const sfHighSpeedOnly = document.getElementById('sfHighSpeedOnly');
  const sfMuniElec = document.getElementById('sfMuniElec');
  const sfNoHoa = document.getElementById('sfNoHoa');
  const sfHoa300 = document.getElementById('sfHoa300');
  const sfMinBeds = document.getElementById('sfMinBeds');
  const sfMinBaths = document.getElementById('sfMinBaths');
  const sfSolar = document.getElementById('sfSolar');
  const sfEv = document.getElementById('sfEv');
  const sfHeatPump = document.getElementById('sfHeatPump');
  const sfHasAdu = document.getElementById('sfHasAdu');
  const sfAduFeasible = document.getElementById('sfAduFeasible');
  const sfPool = document.getElementById('sfPool');
  const sfGarage2 = document.getElementById('sfGarage2');
  const sfMuniWater = document.getElementById('sfMuniWater');
  const sfMuniSewer = document.getElementById('sfMuniSewer');
  const sfNoSpecialTax = document.getElementById('sfNoSpecialTax');
  const sfLowFlood = document.getElementById('sfLowFlood');
  const sfLowFire = document.getElementById('sfLowFire');
  const sfSchoolGrade = document.getElementById('sfSchoolGrade');

  const shopSelectedIsps = new Set();
  const shopSelectedElecs = new Set();
  const shopSelectedCities = new Set();
  const shopSelectedZips = new Set();
  let shopCtrl = null;
  let lastShopResponse = null;

  const getRadioVal = (name) => {
    const el = document.querySelector('input[name="' + name + '"]:checked');
    return el ? String(el.value || '') : '';
  };
  const setRadioVal = (name, val) => {
    document.querySelectorAll('input[name="' + name + '"]').forEach((r) => {
      r.checked = String(r.value || '') === String(val || '');
    });
  };
  const prettyPropType = (pt) => {
    const map = {
      single_family: CUR_LANG === 'zh-CN' ? '独栋住宅' : CUR_LANG === 'es' ? 'Unifamiliar' : 'Single-Family',
      townhouse: CUR_LANG === 'zh-CN' ? '联排别墅' : CUR_LANG === 'es' ? 'Adosada' : 'Townhouse',
      condo: CUR_LANG === 'zh-CN' ? '公寓 Condo' : CUR_LANG === 'es' ? 'Condominio' : 'Condo',
      multi_family: CUR_LANG === 'zh-CN' ? '多户住宅' : CUR_LANG === 'es' ? 'Multifamiliar' : 'Multi-Family',
      manufactured: CUR_LANG === 'zh-CN' ? '装配式住宅' : CUR_LANG === 'es' ? 'Prefabricada' : 'Manufactured',
    };
    return map[pt] || pt || '';
  };

  const resetAllShopFilters = () => {
    if (shopSearchInput) shopSearchInput.value = '';
    if (shopLocQuickInput) shopLocQuickInput.value = '';
    [sfFiberOnly, sfHighSpeedOnly, sfMuniElec, sfNoHoa, sfHoa300, sfSolar, sfEv, sfHeatPump, sfHasAdu, sfAduFeasible, sfPool, sfGarage2, sfMuniWater, sfMuniSewer, sfNoSpecialTax, sfLowFlood, sfLowFire].forEach((cb) => { if (cb) cb.checked = false; });
    ['sfMinDl', 'sfMaxElecRate', 'sfMaxMonthly', 'sfMaxPrice'].forEach((nm) => setRadioVal(nm, ''));
    if (sfMinBeds) sfMinBeds.value = '';
    if (sfMinBaths) sfMinBaths.value = '';
    if (sfSchoolGrade) sfSchoolGrade.value = '';
    document.querySelectorAll('[data-sf-ptype]').forEach((cb) => { cb.checked = false; });
    shopSelectedIsps.clear();
    shopSelectedElecs.clear();
    shopSelectedCities.clear();
    shopSelectedZips.clear();
  };

  const syncQuickPresetButtons = () => {
    const mapOn = {
      fiber: Boolean(sfFiberOnly?.checked),
      muni_elec: Boolean(sfMuniElec?.checked),
      rate_25: getRadioVal('sfMaxElecRate') === '0.25',
      no_hoa: Boolean(sfNoHoa?.checked),
      no_spec_tax: Boolean(sfNoSpecialTax?.checked),
      adu_ready: Boolean(sfHasAdu?.checked || sfAduFeasible?.checked),
      solar: Boolean(sfSolar?.checked),
    };
    document.querySelectorAll('#shopQuickBar [data-qpreset]').forEach((btn) => {
      const k = btn.getAttribute('data-qpreset');
      btn.classList.toggle('on', Boolean(mapOn[k]));
    });
  };

  const buildActiveFilterEntries = () => {
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const list = [];
    const q = (shopSearchInput?.value || '').trim();
    if (q) list.push({ label: '“' + q + '”', clear: () => { shopSearchInput.value = ''; } });
    const qloc = (shopLocQuickInput?.value || '').trim();
    if (qloc) list.push({ label: L('Area: ', '区域：', 'Zona: ') + qloc, clear: () => { shopLocQuickInput.value = ''; } });
    for (const z of shopSelectedZips) {
      list.push({ label: 'ZIP ' + z, clear: () => { shopSelectedZips.delete(z); } });
    }
    for (const c of shopSelectedCities) {
      list.push({ label: c, clear: () => { shopSelectedCities.delete(c); } });
    }
    if (sfFiberOnly?.checked) list.push({ label: L('Symmetrical Fiber', '对称光纤 FTTH', 'Fibra FTTH'), clear: () => { sfFiberOnly.checked = false; } });
    if (sfHighSpeedOnly?.checked) list.push({ label: L('High-Speed Broadband', '高速宽带', 'Banda Ancha Rápida'), clear: () => { sfHighSpeedOnly.checked = false; } });
    const minDl = getRadioVal('sfMinDl');
    if (minDl) list.push({ label: (Number(minDl) / 1000) + '+ Gbps', clear: () => setRadioVal('sfMinDl', '') });
    for (const isp of shopSelectedIsps) {
      list.push({ label: '🌐 ' + isp, clear: () => { shopSelectedIsps.delete(isp); } });
    }
    if (sfMuniElec?.checked) list.push({ label: L('Municipal Power', '市政公共电力', 'Energía Municipal'), clear: () => { sfMuniElec.checked = false; } });
    const maxEr = getRadioVal('sfMaxElecRate');
    if (maxEr) list.push({ label: '≤ $' + maxEr + '/kWh', clear: () => setRadioVal('sfMaxElecRate', '') });
    for (const ec of shopSelectedElecs) {
      list.push({ label: '⚡ ' + ec, clear: () => { shopSelectedElecs.delete(ec); } });
    }
    const maxMo = getRadioVal('sfMaxMonthly');
    if (maxMo) list.push({ label: '≤ ' + fmtCurrency(Number(maxMo)) + L('/mo', '/月', '/mes'), clear: () => setRadioVal('sfMaxMonthly', '') });
    const maxPr = getRadioVal('sfMaxPrice');
    if (maxPr) list.push({ label: '≤ ' + fmtCurrency(Number(maxPr)), clear: () => setRadioVal('sfMaxPrice', '') });
    if (sfNoHoa?.checked) list.push({ label: '$0 HOA', clear: () => { sfNoHoa.checked = false; } });
    else if (sfHoa300?.checked) list.push({ label: 'HOA ≤ $300/mo', clear: () => { sfHoa300.checked = false; } });
    if (sfMinBeds?.value) list.push({ label: sfMinBeds.value + '+ bd', clear: () => { sfMinBeds.value = ''; } });
    if (sfMinBaths?.value) list.push({ label: sfMinBaths.value + '+ ba', clear: () => { sfMinBaths.value = ''; } });
    document.querySelectorAll('[data-sf-ptype]:checked').forEach((cb) => {
      const pt = cb.getAttribute('data-sf-ptype');
      list.push({ label: prettyPropType(pt), clear: () => { cb.checked = false; } });
    });
    if (sfSolar?.checked) list.push({ label: L('Solar', '太阳能', 'Solar'), clear: () => { sfSolar.checked = false; } });
    if (sfEv?.checked) list.push({ label: L('EV Charger', '充电桩', 'Cargador EV'), clear: () => { sfEv.checked = false; } });
    if (sfHeatPump?.checked) list.push({ label: L('Heat Pump', '热泵空调', 'Bomba de Calor'), clear: () => { sfHeatPump.checked = false; } });
    if (sfHasAdu?.checked) list.push({ label: L('Existing ADU', '现成 ADU', 'ADU Existente'), clear: () => { sfHasAdu.checked = false; } });
    if (sfAduFeasible?.checked) list.push({ label: L('ADU-Eligible', '可建 ADU', 'Apto ADU'), clear: () => { sfAduFeasible.checked = false; } });
    if (sfPool?.checked) list.push({ label: L('Pool', '泳池', 'Piscina'), clear: () => { sfPool.checked = false; } });
    if (sfGarage2?.checked) list.push({ label: L('2+ Garage', '2+ 车位车库', '2+ Garaje'), clear: () => { sfGarage2.checked = false; } });
    if (sfMuniWater?.checked) list.push({ label: L('Municipal Water', '市政供水', 'Agua Municipal'), clear: () => { sfMuniWater.checked = false; } });
    if (sfMuniSewer?.checked) list.push({ label: L('Municipal Sewer', '市政排污', 'Alcantarillado Muni'), clear: () => { sfMuniSewer.checked = false; } });
    if (sfNoSpecialTax?.checked) list.push({ label: L('No Special Tax', '无附加特别税', 'Sin Impuesto Especial'), clear: () => { sfNoSpecialTax.checked = false; } });
    if (sfLowFlood?.checked) list.push({ label: L('Outside SFHA Flood', '非高危洪泛区', 'Fuera Zona Inundable'), clear: () => { sfLowFlood.checked = false; } });
    if (sfLowFire?.checked) list.push({ label: L('Outside High Wildfire', '非高危山火区', 'Fuera Riesgo Incendio'), clear: () => { sfLowFire.checked = false; } });
    if (sfSchoolGrade?.value) list.push({ label: L('School ', '公立校 ', 'Escuela ') + sfSchoolGrade.value + '+', clear: () => { sfSchoolGrade.value = ''; } });
    return list;
  };

  const renderShopActiveChips = () => {
    syncQuickPresetButtons();
    const entries = buildActiveFilterEntries();
    const mobCnt = document.getElementById('shopMobActiveCnt');
    if (mobCnt) mobCnt.textContent = entries.length ? '(' + entries.length + ')' : '';
    if (!shopActiveChips) return entries;
    if (!entries.length) {
      shopActiveChips.innerHTML = '';
      return entries;
    }
    const clearLbl = CUR_LANG === 'zh-CN' ? '清除全部筛选' : CUR_LANG === 'es' ? 'Limpiar filtros' : 'Clear All';
    shopActiveChips.innerHTML = entries.map((e, idx) =>
      '<span class="achip"><span>' + escH(e.label) + '</span><button type="button" data-rm-achip="' + idx + '" aria-label="Remove">×</button></span>'
    ).join('') + '<button type="button" class="shop-reset-link" id="shopClearChipsBtn">' + escH(clearLbl) + '</button>';
    shopActiveChips.querySelectorAll('[data-rm-achip]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const i = Number(btn.getAttribute('data-rm-achip'));
        if (entries[i]) { entries[i].clear(); loadShopCatalog(); }
      });
    });
    document.getElementById('shopClearChipsBtn')?.addEventListener('click', () => {
      resetAllShopFilters();
      loadShopCatalog();
    });
    return entries;
  };

  const buildShopSearchParams = (extraOpts) => {
    const p = new URLSearchParams();
    const q = (shopSearchInput?.value || '').trim();
    if (q) p.set('q', q);

    const zips = new Set(shopSelectedZips);
    const cities = new Set(shopSelectedCities);
    const qloc = (shopLocQuickInput?.value || '').trim();
    if (qloc) {
      qloc.split(/[,|]+/).map((s) => s.trim()).filter(Boolean).forEach((tok) => {
        const zm = tok.match(/^(\d{5})(?:-\d{4})?$/);
        if (zm) zips.add(zm[1]);
        else {
          const zIn = tok.match(/\b(\d{5})\b/);
          if (zIn) zips.add(zIn[1]);
          else cities.add(tok.replace(/\s+[A-Za-z]{2}$/, '').trim() || tok);
        }
      });
    }
    if (!(extraOpts && extraOpts.ignoreLocation)) {
      if (zips.size === 1) p.set('zip', [...zips][0]);
      else if (zips.size > 1) p.set('zips', [...zips].join(','));
      if (cities.size) p.set('cities', [...cities].join('|'));
    }
    if (shopStatusSelect?.value) p.set('statuses', shopStatusSelect.value);
    if (shopSortSelect?.value) p.set('sortBy', shopSortSelect.value);

    if (sfFiberOnly?.checked) p.set('fiberOnly', 'true');
    if (sfHighSpeedOnly?.checked) p.set('highSpeedOnly', 'true');
    const minDl = getRadioVal('sfMinDl');
    if (minDl) p.set('minDownloadMbps', minDl);
    if (shopSelectedIsps.size) p.set('internetCarriers', [...shopSelectedIsps].join('|'));

    if (sfMuniElec?.checked) p.set('municipalElectricOnly', 'true');
    const maxEr = getRadioVal('sfMaxElecRate');
    if (maxEr) p.set('maxElectricRateKwh', maxEr);
    if (shopSelectedElecs.size) p.set('electricCarriers', [...shopSelectedElecs].join('|'));

    const maxMo = getRadioVal('sfMaxMonthly');
    if (maxMo) p.set('maxAllInMonthly', maxMo);
    const maxPr = getRadioVal('sfMaxPrice');
    if (maxPr) p.set('maxPrice', maxPr);
    if (sfNoHoa?.checked) p.set('maxHoaMonthly', '0');
    else if (sfHoa300?.checked) p.set('maxHoaMonthly', '300');

    if (sfMinBeds?.value) p.set('minBeds', sfMinBeds.value);
    if (sfMinBaths?.value) p.set('minBaths', sfMinBaths.value);
    const ptypes = [];
    document.querySelectorAll('[data-sf-ptype]:checked').forEach((cb) => ptypes.push(cb.getAttribute('data-sf-ptype')));
    if (ptypes.length) p.set('propertyTypes', ptypes.join(','));

    if (sfSolar?.checked) p.set('hasSolar', 'true');
    if (sfEv?.checked) p.set('hasEvCharger', 'true');
    if (sfHeatPump?.checked) p.set('hasHeatPump', 'true');
    if (sfHasAdu?.checked) p.set('hasAdu', 'true');
    if (sfAduFeasible?.checked) p.set('aduFeasible', 'true');
    if (sfPool?.checked) p.set('hasPool', 'true');
    if (sfGarage2?.checked) p.set('minGarageSpaces', '2');

    if (sfMuniWater?.checked) p.set('municipalWaterOnly', 'true');
    if (sfMuniSewer?.checked) p.set('municipalSewerOnly', 'true');
    if (sfNoSpecialTax?.checked) p.set('excludeSpecialTax', 'true');
    if (sfLowFlood?.checked) p.set('excludeHighFlood', 'true');
    if (sfLowFire?.checked) p.set('excludeHighWildfire', 'true');
    if (sfSchoolGrade?.value) p.set('minSchoolGrade', sfSchoolGrade.value);

    p.set('limit', '60');
    if (extraOpts && extraOpts.syncIfEmpty) p.set('syncIfEmpty', 'true');
    return p;
  };

  const setFacetCountPill = (id, count) => {
    const el = document.getElementById(id);
    if (!el) return;
    const n = Number(count) || 0;
    el.textContent = fmtNum(n);
    el.classList.toggle('zero', n === 0);
  };

  const renderShopFacets = (facets) => {
    if (!facets || typeof facets !== 'object') return;
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const inet = facets.internet || {};
    setFacetCountPill('fcntFiber', inet.fiberAvailable);
    setFacetCountPill('fcntHighSpeed', inet.highSpeedAvailable);
    setFacetCountPill('fcntSpeed1G', inet.speed1GbpsPlus);
    setFacetCountPill('fcntSpeed2G', inet.speed2GbpsPlus);
    setFacetCountPill('fcntSpeed5G', inet.speed5GbpsPlus);

    const ispBox = document.getElementById('shopFacetIspList');
    if (ispBox) {
      const ispItems = Array.isArray(inet.carriers) ? inet.carriers.slice() : [];
      const seenIsp = new Set(ispItems.map((x) => x.name));
      for (const sel of shopSelectedIsps) {
        if (!seenIsp.has(sel)) ispItems.unshift({ name: sel, count: 0 });
      }
      ispBox.innerHTML = ispItems.length
        ? ispItems.map((it) => {
            const chk = shopSelectedIsps.has(it.name) ? ' checked' : '';
            const n = Number(it.count) || 0;
            return '<label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-isp-carrier="' + escH(it.name) + '"' + chk + '> <span title="' + escH(it.name) + '">' + escH(it.name) + '</span></span><span class="fcnt' + (n === 0 ? ' zero' : '') + '">' + escH(fmtNum(n)) + '</span></label>';
          }).join('')
        : '<div class="fdyn-empty">' + escH(L('No carrier data in current view', '当前范围暂无运营商记录', 'Sin datos de operadores en esta vista')) + '</div>';
      ispBox.querySelectorAll('[data-isp-carrier]').forEach((cb) => {
        cb.addEventListener('change', () => {
          const nm = cb.getAttribute('data-isp-carrier');
          if (cb.checked) shopSelectedIsps.add(nm); else shopSelectedIsps.delete(nm);
          loadShopCatalog();
        });
      });
    }

    const elec = facets.electricity || {};
    setFacetCountPill('fcntMuniElec', elec.municipalPublicPower);
    setFacetCountPill('fcntRate20', elec.rateUnder020);
    setFacetCountPill('fcntRate25', elec.rateUnder025);
    setFacetCountPill('fcntRate35', elec.rateUnder035);

    const elecBox = document.getElementById('shopFacetElecList');
    if (elecBox) {
      const eItems = Array.isArray(elec.carriers) ? elec.carriers.slice() : [];
      const seenElec = new Set(eItems.map((x) => x.name));
      for (const sel of shopSelectedElecs) {
        if (!seenElec.has(sel)) eItems.unshift({ name: sel, count: 0 });
      }
      elecBox.innerHTML = eItems.length
        ? eItems.map((it) => {
            const chk = shopSelectedElecs.has(it.name) ? ' checked' : '';
            const n = Number(it.count) || 0;
            const rt = (typeof it.ratePerKwhUsd === 'number' && Number.isFinite(it.ratePerKwhUsd)) ? (' · $' + it.ratePerKwhUsd.toFixed(3).replace(/(\.\d\d)0$/, '$1')) : '';
            const disp = it.name + rt;
            return '<label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-elec-carrier="' + escH(it.name) + '"' + chk + '> <span title="' + escH(disp) + '">' + escH(disp) + '</span></span><span class="fcnt' + (n === 0 ? ' zero' : '') + '">' + escH(fmtNum(n)) + '</span></label>';
          }).join('')
        : '<div class="fdyn-empty">' + escH(L('No utility data in current view', '当前范围暂无电力公司记录', 'Sin datos eléctricos en esta vista')) + '</div>';
      elecBox.querySelectorAll('[data-elec-carrier]').forEach((cb) => {
        cb.addEventListener('change', () => {
          const nm = cb.getAttribute('data-elec-carrier');
          if (cb.checked) shopSelectedElecs.add(nm); else shopSelectedElecs.delete(nm);
          loadShopCatalog();
        });
      });
    }

    const mo = facets.monthlyAllIn || {};
    setFacetCountPill('fcntMo4500', mo.under4500);
    setFacetCountPill('fcntMo6500', mo.under6500);
    setFacetCountPill('fcntMo9000', mo.under9000);
    setFacetCountPill('fcntMo12500', mo.under12500);

    const pr = facets.price || {};
    setFacetCountPill('fcntPr800', pr.under800k);
    setFacetCountPill('fcntPr1250', pr.under1250k);
    setFacetCountPill('fcntPr1750', pr.under1750k);
    setFacetCountPill('fcntPr2500', pr.under2500k);

    const hoa = facets.hoa || {};
    setFacetCountPill('fcntHoa0', hoa.noHoa);
    setFacetCountPill('fcntHoa300', hoa.under300);

    const ptMap = new Map((facets.propertyTypes || []).map((x) => [x.name, x.count]));
    ['single_family', 'townhouse', 'condo', 'multi_family'].forEach((k) => setFacetCountPill('fcntPt_' + k, ptMap.get(k) || 0));

    const hw = facets.hardware || {};
    setFacetCountPill('fcntSolar', hw.solar);
    setFacetCountPill('fcntEv', hw.evCharger);
    setFacetCountPill('fcntHeatPump', hw.heatPump);
    setFacetCountPill('fcntHasAdu', hw.hasAdu);
    setFacetCountPill('fcntAduFeasible', hw.aduFeasible);
    setFacetCountPill('fcntPool', hw.pool);
    setFacetCountPill('fcntGarage2', hw.garage2Plus);

    const wst = facets.waterSewerTax || {};
    setFacetCountPill('fcntMuniWater', wst.municipalWater);
    setFacetCountPill('fcntMuniSewer', wst.municipalSewer);
    setFacetCountPill('fcntNoSpecTax', wst.noSpecialTax);
    setFacetCountPill('fcntLowFlood', wst.lowFloodRisk);
    setFacetCountPill('fcntLowFire', wst.lowWildfireRisk);

    const loc = facets.locations || {};
    const locBox = document.getElementById('shopFacetLocList');
    if (locBox) {
      const cityRows = Array.isArray(loc.cities) ? loc.cities.slice() : [];
      const seenCities = new Set(cityRows.map((x) => x.name));
      for (const sc of shopSelectedCities) {
        if (!seenCities.has(sc)) cityRows.unshift({ name: sc, count: 0 });
      }
      const zipRows = Array.isArray(loc.zips) ? loc.zips.slice() : [];
      const seenZips = new Set(zipRows.map((x) => x.zip));
      for (const sz of shopSelectedZips) {
        if (!seenZips.has(sz)) zipRows.unshift({ zip: sz, count: 0 });
      }
      const cHtml = cityRows.map((c) => {
        const chk = shopSelectedCities.has(c.name) ? ' checked' : '';
        const n = Number(c.count) || 0;
        return '<label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-loc-city="' + escH(c.name) + '"' + chk + '> <span>' + escH(c.name) + '</span></span><span class="fcnt' + (n === 0 ? ' zero' : '') + '">' + escH(fmtNum(n)) + '</span></label>';
      }).join('');
      const zHtml = zipRows.map((z) => {
        const chk = shopSelectedZips.has(z.zip) ? ' checked' : '';
        const n = Number(z.count) || 0;
        return '<label class="fchk"><span class="fchk-lbl"><input type="checkbox" data-loc-zip="' + escH(z.zip) + '"' + chk + '> <span>ZIP ' + escH(z.zip) + '</span></span><span class="fcnt' + (n === 0 ? ' zero' : '') + '">' + escH(fmtNum(n)) + '</span></label>';
      }).join('');
      locBox.innerHTML = (cHtml + zHtml) || '<div class="fdyn-empty">' + escH(L('No indexed cities or ZIPs yet', '暂未索引任何城市或邮编', 'Sin ciudades ni códigos ZIP indexados')) + '</div>';
      locBox.querySelectorAll('[data-loc-city]').forEach((cb) => {
        cb.addEventListener('change', () => {
          const nm = cb.getAttribute('data-loc-city');
          if (cb.checked) shopSelectedCities.add(nm); else shopSelectedCities.delete(nm);
          loadShopCatalog();
        });
      });
      locBox.querySelectorAll('[data-loc-zip]').forEach((cb) => {
        cb.addEventListener('change', () => {
          const zp = cb.getAttribute('data-loc-zip');
          if (cb.checked) shopSelectedZips.add(zp); else shopSelectedZips.delete(zp);
          loadShopCatalog();
        });
      });
    }
  };

  const renderShopGeoUnsyncedBanner = () => {
    const banner = document.getElementById('shopGeoUnsyncedBanner');
    if (!banner) return;
    if (!shopDetectedZipZeroLocal || geoClearedByUser || !activeGeo || !activeGeo.postalCode) {
      banner.innerHTML = '';
      return;
    }
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const cityZip = [activeGeo.city, activeGeo.postalCode].filter(Boolean).join(' ');
    const cityRegionZip = [activeGeo.city, activeGeo.regionCode].filter(Boolean).join(', ') + (activeGeo.city || activeGeo.regionCode ? ' ' : '') + activeGeo.postalCode;
    const totalAll = lastShopResponse ? (Number(lastShopResponse.total) || (lastShopResponse.listings || []).length) : 0;
    const syncBtnLbl = L(
      '⟳ Sync listings for ' + cityZip,
      '⟳ 一键同步 ' + cityZip + ' 房源',
      '⟳ Sincronizar viviendas en ' + cityZip
    );
    banner.innerHTML = '<div style="min-width:0;flex:1 1 260px"><b>📍 ' + escH(L(
      '0 local listings synced yet for ' + cityRegionZip,
      '本地目录暂未同步 ' + cityRegionZip + ' 的房源',
      'Aún hay 0 viviendas locales sincronizadas para ' + cityRegionZip
    )) + '</b><div style="font-size:12px;color:#0f766e;margin-top:2px">' + escH(totalAll > 0
      ? L(
          'Showing all ' + fmtNum(totalAll) + ' indexed homes across your catalog below — or sync live listings for ' + cityZip + ' in one click.',
          '下方已自动展示目录内全部 ' + fmtNum(totalAll) + ' 套房源，您也可以一键同步 ' + cityZip + ' 的实时房源。',
          'Mostrando las ' + fmtNum(totalAll) + ' viviendas indexadas abajo — o sincronice ' + cityZip + ' con un clic.'
        )
      : L(
          'Click to sync real listings for ' + cityZip + ' into your local catalog.',
          '点击右侧按钮即可拉取并同步 ' + cityZip + ' 的真实房源。',
          'Haga clic para sincronizar viviendas reales de ' + cityZip + '.'
        )) + '</div><div id="shopGeoBannerMsg" class="shop-sync-msg"></div></div>' +
      '<div style="display:flex;gap:6px;align-items:center;flex-wrap:wrap">' +
        '<button type="button" class="btnpri" id="shopGeoBannerSyncBtn" style="padding:7px 13px;font-size:12.5px">' + escH(syncBtnLbl) + '</button>' +
        '<button type="button" class="btnmut" id="shopGeoBannerKeepAllBtn" style="padding:6px 10px;font-size:12px">' + escH(L('All Indexed Areas ✓', '浏览全部已收录区域 ✓', 'Todas las zonas ✓')) + '</button>' +
      '</div>';
    document.getElementById('shopGeoBannerSyncBtn')?.addEventListener('click', () => {
      syncRealZipAndReload(
        activeGeo.postalCode,
        document.getElementById('shopGeoBannerMsg'),
        document.getElementById('shopGeoBannerSyncBtn')
      );
    });
    document.getElementById('shopGeoBannerKeepAllBtn')?.addEventListener('click', () => {
      shopDetectedZipZeroLocal = false;
      shopScopeMode = 'all';
      banner.innerHTML = '';
      if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
    });
  };

  const syncRealZipAndReload = async (rawZip, msgEl, btnEl) => {
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const cleanZip = String(rawZip || '').replace(/\D/g, '').slice(0, 5);
    if (cleanZip.length !== 5) {
      if (msgEl) { msgEl.style.color = '#991b1b'; msgEl.textContent = L('Enter a valid 5-digit US ZIP code.', '请输入有效的 5 位美国邮编。', 'Ingrese un código ZIP válido de 5 dígitos.'); }
      return;
    }
    if (btnEl) btnEl.disabled = true;
    if (msgEl) { msgEl.style.color = '#0f766e'; msgEl.textContent = L('Loading listings for ZIP ' + cleanZip + '…', '正在加载邮编 ' + cleanZip + ' 的房源…', 'Cargando viviendas en ZIP ' + cleanZip + '…'); }
    try {
      shopDetectedZipZeroLocal = false;
      shopAutoGeoPendingCheck = false;
      shopScopeMode = 'zip';
      hydratedSpecZips.delete(cleanZip);
      if (shopLocQuickInput && !(shopLocQuickInput.value || '').trim()) shopLocQuickInput.value = cleanZip;
      else shopSelectedZips.add(cleanZip);
      await loadShopCatalog({ keepZipWhenEmpty: true });
      if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
      if (msgEl) { msgEl.style.color = '#065f46'; msgEl.textContent = L('Filtered ZIP ' + cleanZip + '.', '已筛选邮编 ' + cleanZip + '。', 'ZIP ' + cleanZip + ' filtrado.'); }
    } catch (err) {
      if (msgEl) { msgEl.style.color = '#991b1b'; msgEl.textContent = String(err.message || err); }
    } finally {
      if (btnEl) btnEl.disabled = false;
    }
  };

  const renderShopResultsView = (j) => {
    if (!shopResultsHost || !shopMetaRow) return;
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const activeEntries = renderShopActiveChips();
    renderShopFacets(j.facets || {});
    renderShopGeoUnsyncedBanner();

    const list = Array.isArray(j.listings) ? j.listings : [];
    cacheShopListingSpecs(list);
    const total = Number(j.total) || list.length;
    shopMetaRow.innerHTML = '<div><b>' + escH(fmtNum(total)) + '</b> ' +
      escH(total === 1 ? L('real home matched', '套真实房源符合条件', 'vivienda real encontrada') : L('real homes matched', '套真实房源符合条件', 'viviendas reales encontradas')) +
      (list.length < total ? ' · ' + escH(L('showing top ' + list.length, '展示前 ' + list.length + ' 套', 'mostrando ' + list.length)) : '') +
      '</div><div style="font-size:12px;color:#6b7280">' +
      escH(L('Verified utility tariffs, FCC/carrier broadband & public tax rolls', '核验水电费率、宽带光纤覆盖与公开税务档案', 'Tarifas eléctricas, fibra y registros fiscales públicos')) +
      '</div>';

    if (!list.length) {
      const hintZip = ((shopLocQuickInput?.value || '').match(/\b(\d{5})\b/) || [])[1] || [...shopSelectedZips][0] || (!geoClearedByUser && activeGeo && activeGeo.postalCode ? activeGeo.postalCode : '');
      const syncCtaLbl = (hintZip && activeGeo && activeGeo.postalCode === hintZip && activeGeo.city)
        ? L('Sync listings for ' + activeGeo.city + ' ' + hintZip, '同步 ' + activeGeo.city + ' ' + hintZip + ' 房源', 'Sincronizar ' + activeGeo.city + ' ' + hintZip)
        : L('Sync Real ZIP', '同步真实邮编房源', 'Sincronizar ZIP Real');
      const hasZipScope = Boolean(((shopLocQuickInput?.value || '').trim()) || shopSelectedZips.size || shopSelectedCities.size);
      shopResultsHost.innerHTML = '<div class="shop-empty">' +
        '<h3>' + escH(activeEntries.length
          ? L('0 indexed homes match all selected filters', '暂无已收录房源同时满足当前筛选条件', '0 viviendas indexadas cumplen todos los filtros seleccionados')
          : L('No homes indexed in the local catalog yet', '本地房源目录暂未同步房源', 'Aún no hay viviendas sincronizadas en el catálogo local')) + '</h3>' +
        '<p>' + escH(L(
          BRAND_NAME + ' never displays mock or invented listings. Relax a filter above, sync a real 5-digit US ZIP code from live feeds, or generate a full report on any US street address.',
          BRAND_NAME + ' 绝不展示任何虚构或模拟房源。您可以放宽上方筛选条件、输入 5 位美国邮编实时同步真实房源，或在首页生成任意街道地址的深度报告。',
          BRAND_NAME + ' nunca muestra anuncios falsos ni inventados. Relaje algún filtro, sincronice un código ZIP real de 5 dígitos o genere un informe completo por dirección.'
        )) + '</p>' +
        '<div class="shop-empty-actions">' +
          (hasZipScope ? '<button type="button" class="btnsec" id="shopEmptyAllAreasBtn">' + escH(L('All Indexed Areas', '浏览全部已收录区域', 'Todas las zonas indexadas')) + '</button>' : '') +
          (activeEntries.length ? '<button type="button" class="btnmut" id="shopEmptyClearBtn">' + escH(L('Clear All Filters', '清除全部筛选', 'Limpiar todos los filtros')) + '</button>' : '') +
          '<input id="shopEmptyZipInput" class="tinput" style="width:140px;padding:8px 10px" maxlength="5" inputmode="numeric" placeholder="' + escH(L('5-digit US ZIP', '输入 5 位邮编', 'ZIP de 5 dígitos')) + '" value="' + escH(hintZip) + '">' +
          '<button type="button" class="btnpri" id="shopEmptySyncBtn">' + escH(syncCtaLbl) + '</button>' +
          '<button type="button" class="btnmut" id="shopEmptyGoReportBtn">' + escH(L('Analyze a Street Address →', '查询单套街道地址 →', 'Analizar una Dirección →')) + '</button>' +
        '</div>' +
        '<div id="shopEmptySyncMsg" class="shop-sync-msg" style="margin-top:8px"></div>' +
      '</div>';
      document.getElementById('shopEmptyAllAreasBtn')?.addEventListener('click', () => {
        if (shopLocQuickInput) shopLocQuickInput.value = '';
        shopSelectedZips.clear();
        shopSelectedCities.clear();
        shopScopeMode = 'all';
        if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
        loadShopCatalog({ keepZipWhenEmpty: true });
      });
      document.getElementById('shopEmptyClearBtn')?.addEventListener('click', () => {
        resetAllShopFilters();
        shopScopeMode = 'all';
        if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
        loadShopCatalog({ keepZipWhenEmpty: true });
      });
      document.getElementById('shopEmptySyncBtn')?.addEventListener('click', () => {
        const zIn = document.getElementById('shopEmptyZipInput');
        const msg = document.getElementById('shopEmptySyncMsg');
        const btn = document.getElementById('shopEmptySyncBtn');
        syncRealZipAndReload(zIn ? zIn.value : '', msg, btn);
      });
      document.getElementById('shopEmptyGoReportBtn')?.addEventListener('click', () => {
        switchTab('report');
        setTimeout(() => inp?.focus(), 50);
      });
      return;
    }

    const cardsHtml = list.map((m) => {
      const stClass = m.status === 'sold' ? 'mb-sold' : m.status === 'pending' ? 'mb-pend' : 'mb-sale';
      const stLabel = m.status === 'sold'
        ? (T('home.badgeSold', null, L('Sold', '已成交', 'Vendida')) + (m.soldDate ? ' · ' + m.soldDate : ''))
        : m.status === 'pending'
          ? T('home.badgePending', null, L('Pending', '待过户', 'Pendiente'))
          : m.status === 'off_market'
            ? L('Cached Report', '已归档报告', 'Informe Guardado')
            : T('home.badgeSale', null, L('For Sale', '在售', 'En Venta'));
      const dispPrice = m.price || m.estimatedValue || m.soldPrice || null;
      const specs = [
        m.beds != null ? m.beds + ' bd' : null,
        m.baths != null ? m.baths + ' ba' : null,
        m.sqft ? fmtNum(m.sqft) + ' sq ft' : null,
        m.lotSqft ? fmtNum(m.lotSqft) + ' sq ft lot' : null,
        m.yearBuilt ? L('Built ' + m.yearBuilt, m.yearBuilt + ' 年建', 'Año ' + m.yearBuilt) : null,
        m.hoaMonthly === 0 ? '$0 HOA' : (m.hoaMonthly != null ? 'HOA $' + fmtNum(m.hoaMonthly) + '/mo' : null),
      ].filter(Boolean).join(' · ');
      const subLoc = [m.city, m.state, m.zip].filter(Boolean).join(', ');
      const href = m.reportSlug ? ('/r/' + encodeURIComponent(m.reportSlug) + '/') : ('/report?address=' + encodeURIComponent(m.address));
      const ctaLbl = m.reportSlug
        ? L('View Full Report →', '查看完整报告 →', 'Ver informe completo →')
        : L('Generate Full Report →', '生成深度报告 →', 'Generar informe completo →');
      const hasValidPhoto = typeof m.photoUrl === 'string' && /^https?:\/\//i.test(m.photoUrl.trim());
      const photoHtml = hasValidPhoto
        ? '<div class="shop-photo"><img src="' + escH(m.photoUrl.trim()) + '" alt="' + escH(m.address || '') + '" loading="lazy" referrerpolicy="no-referrer" onerror="this.parentElement.style.display=\'none\'"></div>'
        : '';
      const ptypeLabel = [prettyPropType(m.propertyType), m.architecturalStyle].filter(Boolean).join(' · ');
      return '<article class="shop-card">' +
        photoHtml +
        '<div class="shop-body">' +
          '<div class="shop-badges"><div style="display:flex;gap:5px;flex-wrap:wrap;align-items:center">' +
            '<span class="mbadge ' + stClass + '">' + escH(stLabel) + '</span>' +
            (m.schoolScore != null ? '<span class="mbadge mb-sch">' + escH(L('School ', '学区 ', 'Escuela ') + m.schoolScore + (m.schoolGrade ? ' (' + m.schoolGrade + ')' : '')) + '</span>' : '') +
            (m.overallScore != null ? '<span class="mbadge mb-score">' + escH(L('Score ', '评分 ', 'Puntaje ') + Math.round(m.overallScore)) + '</span>' : '') +
          '</div><span style="font-size:11px;color:#9ca3af">' + escH(displayListingSource(m.source || '')) + '</span></div>' +
          '<a class="shop-addr" href="' + escH(href) + '">' + escH(m.address) + '</a>' +
          (subLoc ? '<div class="shop-subloc">' + escH(subLoc) + '</div>' : '') +
          '<div class="shop-price-row"><span class="shop-price">' + (dispPrice ? escH(fmtCurrency(dispPrice)) : escH(L('See Valuation', '查看估值', 'Ver Estimación'))) + '</span>' +
            (m.pricePerSqft ? '<span style="font-size:12px;color:#6b7280;font-weight:600">' + escH(fmtCurrency(m.pricePerSqft)) + '/sq ft</span>' : '') +
          '</div>' +
          (specs ? '<div class="shop-specs">' + escH(specs) + '</div>' : '') +
          renderUtilityFiberPills(m, false) +
          '<div class="shop-foot"><span class="shop-ptype">' + escH(ptypeLabel) + '</span><a class="btnsec" href="' + escH(href) + '">' + escH(ctaLbl) + '</a></div>' +
        '</div>' +
      '</article>';
    }).join('');

    shopResultsHost.innerHTML = '<div class="shop-grid">' + cardsHtml + '</div>';
    shopResultsHost.querySelectorAll('a[href^="/report?address="]').forEach((a) => {
      a.addEventListener('click', () => {
        try {
          const u = new URL(a.getAttribute('href'), location.origin);
          const ad = u.searchParams.get('address');
          if (ad) enqueueAndTrackAddress(ad);
        } catch {}
      });
    });
  };

  async function loadShopCatalog(extraOpts) {
    if (!shopResultsHost) return;
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    renderShopActiveChips();
    if (shopCtrl) shopCtrl.abort();
    const myCtrl = new AbortController();
    shopCtrl = myCtrl;
    if (shopApplyBtn) shopApplyBtn.disabled = true;
    if (!lastShopResponse) {
      shopResultsHost.innerHTML = '<div class="mbanner">' + escH(L('Loading faceted home catalog…', '正在加载条件选房目录…', 'Cargando catálogo de viviendas…')) + '</div>';
    }
    try {
      const checkingAutoZip = Boolean(
        (shopAutoGeoPendingCheck || (extraOpts && extraOpts.fromAutoGeo)) &&
        !(extraOpts && extraOpts.keepZipWhenEmpty) &&
        !geoClearedByUser &&
        activeGeo &&
        activeGeo.postalCode &&
        (shopLocQuickInput?.value || '').trim() === activeGeo.postalCode &&
        shopSelectedZips.size === 0 &&
        shopSelectedCities.size === 0
      );
      const qs = buildShopSearchParams(extraOpts);
      const res = await fetch('/api/listings/shop?' + qs.toString(), { cache: 'no-store', signal: myCtrl.signal });
      const j = await res.json();
      if (myCtrl.signal.aborted) return;
      if (!res.ok || !j.ok) {
        shopResultsHost.innerHTML = '<div class="mbanner" style="background:#fef2f2;border-color:#fecaca;color:#991b1b">' + escH(j.error || j.message || L('Could not load catalog.', '加载房源目录失败。', 'No se pudo cargar el catálogo.')) + '</div>';
        return;
      }
      if (checkingAutoZip) {
        shopAutoGeoPendingCheck = false;
        const cnt = Number(j.total) || (j.listings || []).length;
        if (cnt === 0) {
          shopDetectedZipZeroLocal = true;
          shopScopeMode = 'all';
          if (shopLocQuickInput) shopLocQuickInput.value = '';
          const allQs = buildShopSearchParams({ ignoreLocation: true });
          const allRes = await fetch('/api/listings/shop?' + allQs.toString(), { cache: 'no-store', signal: myCtrl.signal });
          const allJ = await allRes.json();
          if (myCtrl.signal.aborted) return;
          if (allRes.ok && allJ && allJ.ok) {
            lastShopResponse = allJ;
            if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
            renderShopResultsView(allJ);
            return;
          }
        } else {
          shopDetectedZipZeroLocal = false;
          shopScopeMode = 'zip';
          if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
        }
      }
      lastShopResponse = j;
      renderShopResultsView(j);
    } catch (err) {
      if (err && err.name === 'AbortError') return;
      shopResultsHost.innerHTML = '<div class="mbanner" style="background:#fef2f2;border-color:#fecaca;color:#991b1b">' + escH(String((err && err.message) || err)) + '</div>';
    } finally {
      if (shopCtrl === myCtrl && shopApplyBtn) shopApplyBtn.disabled = false;
    }
  }

  shopMobileFilterBtn?.addEventListener('click', () => {
    if (!shopSidebar) return;
    const open = shopSidebar.classList.toggle('open');
    shopMobileFilterBtn.setAttribute('aria-expanded', open ? 'true' : 'false');
  });
  document.getElementById('shopSidebarResetBtn')?.addEventListener('click', () => {
    resetAllShopFilters();
    loadShopCatalog();
  });
  shopApplyBtn?.addEventListener('click', () => loadShopCatalog());
  shopSearchInput?.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); loadShopCatalog(); } });
  shopLocQuickInput?.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); loadShopCatalog(); } });
  [shopStatusSelect, shopSortSelect, sfMinBeds, sfMinBaths, sfSchoolGrade].forEach((sel) => {
    sel?.addEventListener('change', () => loadShopCatalog());
  });
  [sfFiberOnly, sfHighSpeedOnly, sfMuniElec, sfSolar, sfEv, sfHeatPump, sfHasAdu, sfAduFeasible, sfPool, sfGarage2, sfMuniWater, sfMuniSewer, sfNoSpecialTax, sfLowFlood, sfLowFire].forEach((cb) => {
    cb?.addEventListener('change', () => loadShopCatalog());
  });
  sfNoHoa?.addEventListener('change', () => {
    if (sfNoHoa.checked && sfHoa300) sfHoa300.checked = false;
    loadShopCatalog();
  });
  sfHoa300?.addEventListener('change', () => {
    if (sfHoa300.checked && sfNoHoa) sfNoHoa.checked = false;
    loadShopCatalog();
  });
  document.querySelectorAll('input[name="sfMinDl"], input[name="sfMaxElecRate"], input[name="sfMaxMonthly"], input[name="sfMaxPrice"], [data-sf-ptype]').forEach((el) => {
    el.addEventListener('change', () => loadShopCatalog());
  });
  document.querySelectorAll('#shopQuickBar [data-qpreset]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const k = btn.getAttribute('data-qpreset');
      if (k === 'fiber' && sfFiberOnly) sfFiberOnly.checked = !sfFiberOnly.checked;
      else if (k === 'muni_elec' && sfMuniElec) sfMuniElec.checked = !sfMuniElec.checked;
      else if (k === 'rate_25') setRadioVal('sfMaxElecRate', getRadioVal('sfMaxElecRate') === '0.25' ? '' : '0.25');
      else if (k === 'no_hoa' && sfNoHoa) { sfNoHoa.checked = !sfNoHoa.checked; if (sfNoHoa.checked && sfHoa300) sfHoa300.checked = false; }
      else if (k === 'no_spec_tax' && sfNoSpecialTax) sfNoSpecialTax.checked = !sfNoSpecialTax.checked;
      else if (k === 'adu_ready' && sfAduFeasible) {
        const nextOn = !(sfHasAdu?.checked || sfAduFeasible.checked);
        if (!nextOn && sfHasAdu) sfHasAdu.checked = false;
        sfAduFeasible.checked = nextOn;
      }
      else if (k === 'solar' && sfSolar) sfSolar.checked = !sfSolar.checked;
      loadShopCatalog();
    });
  });
  document.getElementById('shopSideSyncBtn')?.addEventListener('click', () => {
    syncRealZipAndReload(
      document.getElementById('shopSideZipInput')?.value || '',
      document.getElementById('shopSideSyncMsg'),
      document.getElementById('shopSideSyncBtn')
    );
  });
  document.getElementById('shopSideZipInput')?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      syncRealZipAndReload(
        document.getElementById('shopSideZipInput')?.value || '',
        document.getElementById('shopSideSyncMsg'),
        document.getElementById('shopSideSyncBtn')
      );
    }
  });

  const mainWrapEl = document.querySelector('.wrap');
  const switchTab = (name) => {
    tabBtns.forEach((b) => b.classList.toggle('act', b.dataset.tabTarget === name));
    panels.forEach((p) => p.classList.toggle('act', p.id === 'tab-' + name));
    if (mainWrapEl) mainWrapEl.classList.toggle('shop-wide', name === 'shop');
    if (name === 'shop') loadShopCatalog();
    if (name === 'alerts') loadWatchedZips();
    if (name === 'match' && typeof window.__initMatchStudio === 'function') window.__initMatchStudio();
    try {
      const u = new URL(location.href);
      if (name === 'report') {
        u.searchParams.delete('tab');
        if (u.pathname === '/match' || u.pathname === '/shop') u.pathname = '/';
      } else if (name === 'shop' && u.pathname === '/shop') {
        u.searchParams.delete('tab');
      } else {
        if ((u.pathname === '/match' && name !== 'match') || (u.pathname === '/shop' && name !== 'shop')) {
          u.pathname = '/';
        }
        u.searchParams.set('tab', name);
      }
      history.replaceState(null, '', u.toString());
    } catch {}
  };
  tabBtns.forEach((b) => b.addEventListener('click', () => switchTab(b.dataset.tabTarget)));
  const urlParams = new URLSearchParams(location.search);
  const initTab = urlParams.get('tab');
  const openShopByRoute = location.pathname === '/shop';
  const openMatchByRoute = location.pathname === '/match' || location.hash === '#quiz';
  const initQueryAddr = (urlParams.get('q') || '').trim();
  const initZip = (urlParams.get('zip') || urlParams.get('zips') || '').replace(/\D/g, '').slice(0, 5);
  if (openShopByRoute || initTab === 'shop') {
    if (initQueryAddr && shopSearchInput) shopSearchInput.value = initQueryAddr;
    if (initZip.length === 5 && shopLocQuickInput) shopLocQuickInput.value = initZip;
    if (urlParams.get('fiberOnly') === 'true' && sfFiberOnly) sfFiberOnly.checked = true;
    if (urlParams.get('municipalElectricOnly') === 'true' && sfMuniElec) sfMuniElec.checked = true;
    const urlSort = urlParams.get('sortBy');
    if (urlSort && shopSortSelect && [...shopSortSelect.options].some((o) => o.value === urlSort)) {
      shopSortSelect.value = urlSort;
    }
  } else if (initQueryAddr && inp) {
    inp.value = initQueryAddr;
    clr.classList.add('on');
    setTimeout(() => { inp.focus(); fetchSuggestNow(initQueryAddr); }, 40);
  }
  if (initTab && ['report', 'shop', 'match', 'alerts'].includes(initTab)) switchTab(initTab);
  else if (openShopByRoute) switchTab('shop');
  else if (openMatchByRoute) switchTab('match');
  if (location.hash === '#quiz') {
    setTimeout(() => document.getElementById('matchQuizStudio')?.scrollIntoView({ behavior: 'smooth', block: 'start' }), 60);
  }

  const nlQuery = document.getElementById('nlQuery'), nlStatus = document.getElementById('nlStatus'), nlSearchBtn = document.getElementById('nlSearchBtn'), nlToAlertBtn = document.getElementById('nlToAlertBtn'), nlResults = document.getElementById('nlResults');
  let lastParsedZips = [];
  async function runMatchSearch() {
    const q = (nlQuery.value || '').trim();
    if (!q) { nlQuery.focus(); return; }
    const curLang = (window.OpenEstateProfileStore && window.OpenEstateProfileStore.loadUi().lang) || CUR_LANG || 'en';
    nlSearchBtn.disabled = true;
    nlSearchBtn.textContent = curLang === 'zh-CN' ? T('home.searchingHomes', null, '正在搜索…') : curLang === 'es' ? 'Buscando…' : T('home.searchingHomes', null, 'Searching homes…');
    nlResults.innerHTML = '<div class="mbanner">' + escH(curLang === 'zh-CN' ? T('home.searchingBanner', null, '正在匹配符合您条件的房源…') : curLang === 'es' ? 'Buscando viviendas según sus criterios…' : T('home.searchingBanner', null, 'Finding homes that match your criteria…')) + '</div>';
    try {
      const stVal = nlStatus.value;
      const statuses = stVal === 'auto' ? undefined : stVal.split(',');
      const res = await fetch('/api/match', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ query: q, lang: curLang, ...(statuses ? { statuses } : {}) }) });
      const j = await res.json();
      if (!res.ok || !j.ok) {
        nlResults.innerHTML = '<div class="mbanner" style="background:#fef2f2;border-color:#fecaca;color:#991b1b">' + escH(j.error || j.message || T('home.searchFailed')) + '</div>';
        return;
      }
      const f = (j.query && j.query.filters) || {};
      lastParsedZips = f.zips || [];
      const matchZips = [...new Set([...(f.zips || []), ...(j.matches || []).map((m) => m.zip).filter(Boolean)])];
      const needsSpecHydration = (j.matches || []).some((m) => m && m.electricCarrier == null && m.allInMonthly == null && m.estAllInMonthly == null && m.fiberAvailable == null && !shopSpecsById.has(String(m.id)));
      if (needsSpecHydration && matchZips.length) {
        await hydrateShopSpecsForZips(matchZips);
      }
      const cards = (j.matches || []).map((m) => {
        const extraSpec = shopSpecsById.get(String(m.id)) || shopSpecsById.get('addr:' + String(m.address || '').trim().toLowerCase()) || {};
        const fullM = Object.assign({}, extraSpec, (j.listings && j.listings[m.id]) || {}, m);
        const stClass = m.status === 'sold' ? 'mb-sold' : m.status === 'pending' ? 'mb-pend' : 'mb-sale';
        const stLabel = m.status === 'sold' ? (T('home.badgeSold') + (m.soldDate ? ' · ' + m.soldDate : '')) : m.status === 'pending' ? T('home.badgePending') : T('home.badgeSale');
        const specs = [m.beds != null ? m.beds + ' bd' : null, m.baths != null ? m.baths + ' ba' : null, m.sqft ? fmtNum(m.sqft) + ' sq ft' : null, m.lotSqft ? fmtNum(m.lotSqft) + ' sq ft lot' : null, m.yearBuilt ? T('home.built', { year: m.yearBuilt }) : null, m.hoaMonthly === 0 ? T('home.noHoa') : m.hoaMonthly != null ? T('home.hoaPerMo', { amt: fmtNum(m.hoaMonthly) }) : null].filter(Boolean).join(' · ');
        const utilPillsHtml = renderUtilityFiberPills(fullM, true);
        const href = m.reportSlug ? ('/r/' + encodeURIComponent(m.reportSlug) + '/') : ('/report?address=' + encodeURIComponent(m.address));
        const btnLabel = curLang === 'es' ? (m.reportSlug ? 'Ver informe completo →' : 'Analizar informe →') : escH(m.reportSlug ? T('home.viewFullReport') : T('home.analyzeFullReport'));
        return '<div class="mcard"><div style="display:flex;justify-content:space-between;gap:8px;flex-wrap:wrap;margin-bottom:6px"><div style="display:flex;gap:6px;flex-wrap:wrap"><span class="mbadge ' + stClass + '">' + escH(stLabel) + '</span><span class="mbadge mb-score">' + escH(m.matchScore) + '/100 · ' + escH(m.matchTier || 'Match') + '</span>' + (m.schoolScore != null ? '<span class="mbadge mb-sch">' + escH(T('home.schoolTag', { score: m.schoolScore })) + (m.schoolGrade ? ' (' + escH(m.schoolGrade) + ')' : '') + '</span>' : '') + '</div><span style="font-size:11.5px;color:#6b7280">' + escH(displayListingSource(m.source || '')) + '</span></div><div style="font-weight:700;font-size:15.5px">' + escH(m.address) + '</div><div style="font-size:22px;font-weight:800;color:#0f766e;margin:4px 0">' + (m.price ? escH(fmtCurrency(m.price)) : escH(T('home.viewEstimate'))) + '</div><div style="font-size:12.5px;color:#4b5563">' + escH(specs) + '</div>' + utilPillsHtml + '<div class="whybox">' + escH(m.whyItMatches || '') + '</div><div style="display:flex;justify-content:space-between;align-items:center;margin-top:8px"><span style="font-size:12px;color:#6b7280">' + escH(m.propertyType || '') + '</span><a class="btnsec" href="' + escH(href) + '">' + btnLabel + '</a></div></div>';
      }).join('');
      nlResults.innerHTML = '<div class="mbanner"><div style="font-weight:700;color:#065f46">' + escH(j.overview || '') + '</div>' + (f.fairHousingNotice ? '<div style="font-size:12px;color:#6b7280;margin-top:4px">' + escH(T('home.fairHousingNote')) + ' ' + escH(displayListingSource(f.fairHousingNotice)) + '</div>' : '') + '</div>' + (cards ? '<div class="grid" style="margin-top:14px">' + cards + '</div>' : '');
      nlResults.querySelectorAll('a[href^="/report?address="]').forEach((a) => {
        a.addEventListener('click', () => {
          try {
            const u = new URL(a.getAttribute('href'), location.origin);
            const ad = u.searchParams.get('address');
            if (ad) enqueueAndTrackAddress(ad);
          } catch {}
        });
      });
    } catch (err) {
      nlResults.innerHTML = '<div class="mbanner" style="background:#fef2f2;border-color:#fecaca;color:#991b1b">Error: ' + escH(err.message || err) + '</div>';
    } finally {
      nlSearchBtn.disabled = false;
      const finLang = (window.OpenEstateProfileStore && window.OpenEstateProfileStore.loadUi().lang) || CUR_LANG || 'en';
      nlSearchBtn.textContent = finLang === 'es' ? 'Buscar viviendas compatibles' : T('home.matchSearchBtn');
    }
  }
  document.querySelectorAll('.chip[data-ex]').forEach((ch) => ch.addEventListener('click', () => {
    const baseEx = ch.dataset.ex || '';
    const activeZipSuffix = (!geoClearedByUser && activeGeo && activeGeo.postalCode && !/\b\d{5}\b/.test(baseEx))
      ? (' in ' + activeGeo.postalCode)
      : '';
    nlQuery.value = baseEx + activeZipSuffix;
    runMatchSearch();
  }));
  nlSearchBtn.addEventListener('click', runMatchSearch);
  nlQuery.addEventListener('keydown', (e) => { if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') { e.preventDefault(); runMatchSearch(); } });
  nlToAlertBtn.addEventListener('click', () => {
    const q = (nlQuery.value || '').trim();
    if (q) document.getElementById('subReq').value = q;
    const zipsInText = q.match(/\b\d{5}\b/g) || [];
    const fallbackZip = (!geoClearedByUser && activeGeo && activeGeo.postalCode) ? [activeGeo.postalCode] : [];
    const merged = [...new Set([...lastParsedZips, ...zipsInText, ...fallbackZip])];
    if (merged.length && !document.getElementById('subZips').value.trim()) document.getElementById('subZips').value = merged.join(', ');
    switchTab('alerts'); document.getElementById('subEmail').focus();
  });

  const subForm = document.getElementById('subForm'), subStatusMsg = document.getElementById('subStatusMsg'), subSubmitBtn = document.getElementById('subSubmitBtn');
  subForm.addEventListener('submit', async (e) => {
    e.preventDefault(); subSubmitBtn.disabled = true; subStatusMsg.style.color = '#0f766e'; subStatusMsg.textContent = T('home.submittingAlert');
    try {
      const emailVal = document.getElementById('subEmail').value.trim();
      const reqVal = document.getElementById('subReq').value.trim();
      const notifyNewVal = document.getElementById('subNew').checked;
      const notifySoldVal = document.getElementById('subSold').checked;
      const freqEl = document.getElementById('subFreq');
      const freqVal = (freqEl && freqEl.value) || 'instant';
      const res = await fetch('/api/subscriptions', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          email: emailVal,
          zips: document.getElementById('subZips').value.trim(),
          requirements: reqVal,
          notifyOnNew: notifyNewVal,
          notifyOnSold: notifySoldVal,
          frequency: freqVal
        })
      });
      const j = await res.json();
      if (!res.ok || !j.ok) {
        subStatusMsg.style.color = '#991b1b';
        if (j && j.code === 'PAYWALL_ALERT_LIMIT_REACHED' && j.upgradeUrl) {
          const upgLbl = CUR_LANG === 'zh-CN' ? '升级套餐 →' : CUR_LANG === 'es' ? 'Mejorar plan →' : 'Upgrade plan →';
          subStatusMsg.innerHTML = escH(j.error || T('home.subFailed')) + ' <a href="' + escH(j.upgradeUrl) + '" style="color:#0f766e;font-weight:700;text-decoration:underline">' + escH(upgLbl) + '</a>';
        } else {
          subStatusMsg.textContent = (j && (j.error || j.message)) || T('home.subFailed');
        }
        return;
      }
      const subObj = j.subscription || {};
      const unsubTok = String(subObj.unsubscribeToken || '').trim();
      const savedFreq = String(subObj.frequency || freqVal || 'instant');
      try {
        const prev = readMyAlerts().filter((x) => !subObj.id || x.id !== subObj.id);
        localStorage.setItem(MY_ALERTS_KEY, JSON.stringify([{
          id: subObj.id || String(Date.now()),
          email: emailVal,
          name: subObj.name || '',
          zips: subObj.zips || [],
          requirements: reqVal,
          notifyOnNew: notifyNewVal,
          notifyOnSold: notifySoldVal,
          frequency: savedFreq,
          active: subObj.active !== false,
          unsubscribeToken: unsubTok,
          createdAt: subObj.createdAt || new Date().toISOString()
        }, ...prev].slice(0, 12)));
      } catch {}
      subStatusMsg.style.color = '#065f46';
      const manageHref = unsubTok ? ('/alerts?token=' + encodeURIComponent(unsubTok)) : '/alerts';
      subStatusMsg.innerHTML = escH(T('home.subSuccess', { zips: (subObj.zips || []).join(', ') })) +
        ' <a href="' + escH(manageHref) + '" style="color:#0f766e;text-decoration:underline;margin-left:8px">' + escH(T('home.alertsManageBtn', null, 'Manage Saved Alerts →')) + '</a>';
      loadWatchedZips();
    } catch (err) {
      subStatusMsg.style.color = '#991b1b'; subStatusMsg.textContent = String(err.message || err);
    } finally { subSubmitBtn.disabled = false; }
  });

  // ============================================================================================
  // Your Match: Browser Profile Store (openestate:your_match_profile:v1) + 60-Second Quiz Studio
  // ============================================================================================
  const PROFILE_KEY = 'openestate:your_match_profile:v1';
  const UI_STATE_KEY = 'openestate:your_match_ui:v1';
  const BALANCED_WEIGHTS = {
    cost: 16, schools: 9, commute: 13, convenience: 10, homefit: 12, style: 6,
    hazards: 9, crime: 8, environment: 7, investment: 8, fengshui: 2,
  };
  const VALID_DIMS = Object.keys(BALANCED_WEIGHTS);
  const VALID_LOAN_TYPES = new Set(['conventional', 'fha', 'va', 'cash']);
  const VALID_PROP_TYPES = new Set(['single_family', 'townhouse', 'condo', 'multi_family', 'manufactured', 'other']);
  const VALID_SCHOOL_LEVELS = new Set(['elementary', 'middle', 'high']);
  const VALID_SCREENS = new Set(['sfha_flood', 'very_high_fire', 'freeway_within_500ft', 't_junction', 'hoa_over', 'style_avoid', 'address_digits']);
  const VALID_STYLE_FAMILIES = new Set([
    'modern', 'mid_century', 'ranch', 'split_level', 'craftsman', 'farmhouse', 'barn', 'colonial',
    'cape_cod', 'victorian', 'tudor', 'mediterranean', 'southwestern', 'cabin', 'traditional', 'other_distinctive',
  ]);
  const VALID_STYLE_PREFS = new Set(['love', 'fine', 'avoid']);
  const VALID_FEATURES = new Set([
    'open_plan', 'high_ceilings', 'natural_light', 'hardwood', 'fireplace', 'pool',
    'view', 'main_level_primary', 'office', 'workshop_garage', 'ev_ready',
  ]);
  const loanProgramToScoringType = (prog) => {
    if (prog === 'fha' || prog === 'va' || prog === 'cash') return prog;
    return 'conventional';
  };

  function defaultProfileDoc() {
    return {
      version: 1,
      weights: Object.assign({}, BALANCED_WEIGHTS),
      preset: 'balanced',
      budget: {},
      downPaymentPct: 20,
      loanType: 'conventional',
      propertyTypes: [],
      multigenerational: false,
      schoolLevels: [],
      places: [],
      trafficBuffer: 1.3,
      stylePreferences: {},
      mustHaves: [],
      dealBreakers: [],
      avoidDigits: [],
      featureWishes: [],
      critical: [],
    };
  }

  function normalizeProfileDoc(raw) {
    const base = defaultProfileDoc();
    if (!raw || typeof raw !== 'object') return base;
    const wOut = {};
    let posCount = 0;
    const rawW = (raw.weights && typeof raw.weights === 'object') ? raw.weights : BALANCED_WEIGHTS;
    for (const d of VALID_DIMS) {
      const n = Number(rawW[d]);
      const val = Number.isFinite(n) && n >= 0 ? Math.min(100, Math.round(n * 100) / 100) : 0;
      wOut[d] = val;
      if (val > 0) posCount++;
    }
    if (posCount === 0) Object.assign(wOut, BALANCED_WEIGHTS);

    const budget = {};
    if (raw.budget && typeof raw.budget === 'object') {
      const mp = Number(raw.budget.maxPrice);
      const mo = Number(raw.budget.monthly);
      if (Number.isFinite(mp) && mp > 0 && mp <= 1e9) budget.maxPrice = mp;
      if (Number.isFinite(mo) && mo > 0 && mo <= 1e7) budget.monthly = mo;
    }

    const downRaw = Number(raw.downPaymentPct);
    const downPaymentPct = Number.isFinite(downRaw) ? Math.max(0, Math.min(100, downRaw)) : 20;
    const loanType = VALID_LOAN_TYPES.has(raw.loanType) ? raw.loanType : 'conventional';
    const tbRaw = Number(raw.trafficBuffer);
    const trafficBuffer = Number.isFinite(tbRaw) ? Math.max(1.0, Math.min(2.0, tbRaw)) : 1.3;

    const numOpt = (v, lo, hi) => {
      if (v === null || v === undefined || v === '') return undefined;
      const n = Number(v);
      return Number.isFinite(n) && n > lo && n <= hi ? n : undefined;
    };
    const numOptZero = (v, hi) => {
      if (v === null || v === undefined || v === '') return undefined;
      const n = Number(v);
      return Number.isFinite(n) && n >= 0 && n <= hi ? n : undefined;
    };

    const places = Array.isArray(raw.places) ? raw.places.slice(0, 5).filter((p) => (
      p && typeof p.label === 'string' && p.label.trim().length > 0 &&
      Number.isFinite(Number(p.lat)) && Math.abs(Number(p.lat)) <= 90 &&
      Number.isFinite(Number(p.lon)) && Math.abs(Number(p.lon)) <= 180 &&
      Number.isFinite(Number(p.maxMinutes)) && Number(p.maxMinutes) > 0 && Number(p.maxMinutes) <= 240
    )).map((p) => ({
      label: String(p.label).trim().slice(0, 80),
      lat: Number(p.lat),
      lon: Number(p.lon),
      maxMinutes: Number(p.maxMinutes),
      tripsPerWeek: Number.isFinite(Number(p.tripsPerWeek)) && Number(p.tripsPerWeek) > 0 ? Math.min(50, Number(p.tripsPerWeek)) : 5,
      mode: 'drive',
    })) : [];

    const stylePreferences = {};
    if (raw.stylePreferences && typeof raw.stylePreferences === 'object') {
      for (const [k, v] of Object.entries(raw.stylePreferences)) {
        if (VALID_STYLE_FAMILIES.has(k) && VALID_STYLE_PREFS.has(v)) stylePreferences[k] = v;
      }
    }

    const uniqValid = (arr, set) => Array.isArray(arr) ? [...new Set(arr.filter((x) => set.has(x)))] : [];
    const dealBreakers = uniqValid(raw.dealBreakers, VALID_SCREENS);
    const mustHaves = uniqValid(raw.mustHaves, VALID_SCREENS).filter((x) => !dealBreakers.includes(x));
    const avoidDigits = Array.isArray(raw.avoidDigits) ? [...new Set(raw.avoidDigits.map(Number).filter((n) => Number.isInteger(n) && n >= 0 && n <= 9))] : [];
    const hoaMaxMonthly = numOptZero(raw.hoaMaxMonthly, 1e6);

    const out = {
      version: 1,
      weights: wOut,
      ...(typeof raw.preset === 'string' && raw.preset.trim() ? { preset: raw.preset.trim() } : {}),
      budget,
      downPaymentPct,
      loanType,
      ...(numOptZero(raw.minBeds, 20) !== undefined ? { minBeds: numOptZero(raw.minBeds, 20) } : {}),
      ...(numOptZero(raw.minBaths, 20) !== undefined ? { minBaths: numOptZero(raw.minBaths, 20) } : {}),
      ...(numOpt(raw.minSqft, 0, 100000) !== undefined ? { minSqft: numOpt(raw.minSqft, 0, 100000) } : {}),
      ...(numOpt(raw.minLotSqft, 0, 50000000) !== undefined ? { minLotSqft: numOpt(raw.minLotSqft, 0, 50000000) } : {}),
      propertyTypes: uniqValid(raw.propertyTypes, VALID_PROP_TYPES),
      multigenerational: Boolean(raw.multigenerational),
      schoolLevels: uniqValid(raw.schoolLevels, VALID_SCHOOL_LEVELS),
      places,
      trafficBuffer,
      stylePreferences,
      mustHaves,
      dealBreakers,
      ...(hoaMaxMonthly !== undefined ? { hoaMaxMonthly } : {}),
      avoidDigits,
      featureWishes: uniqValid(raw.featureWishes, VALID_FEATURES),
      critical: uniqValid(raw.critical, new Set(VALID_DIMS)).filter((d) => (wOut[d] || 0) > 0),
    };
    if ((out.dealBreakers.includes('hoa_over') || out.mustHaves.includes('hoa_over')) && out.hoaMaxMonthly === undefined) {
      out.hoaMaxMonthly = 500;
    }
    if ((out.dealBreakers.includes('address_digits') || out.mustHaves.includes('address_digits')) && out.avoidDigits.length === 0) {
      out.avoidDigits = [4];
    }
    if ((out.dealBreakers.includes('style_avoid') || out.mustHaves.includes('style_avoid')) && !Object.values(out.stylePreferences).includes('avoid')) {
      out.dealBreakers = out.dealBreakers.filter((x) => x !== 'style_avoid');
      out.mustHaves = out.mustHaves.filter((x) => x !== 'style_avoid');
    }
    return out;
  }

  function defaultUiState() {
    return { lang: 'en', areas: { zips: [], cities: [] }, loanProgram: '30yr_fixed', eraPreference: 'any', aduNeeded: false, step: 1, showAllSteps: false };
  }

  const SAVED_HOMES_KEY = 'openestate:saved_homes:v1';

  const profileStore = {
    mode: 'local',
    storageKey: PROFILE_KEY,
    uiStorageKey: UI_STATE_KEY,
    savedHomesKey: SAVED_HOMES_KEY,
    hasSaved() {
      try { return Boolean(localStorage.getItem(PROFILE_KEY)); } catch { return false; }
    },
    loadSaved() {
      try {
        const s = localStorage.getItem(PROFILE_KEY);
        if (!s) return null;
        return normalizeProfileDoc(JSON.parse(s));
      } catch { return null; }
    },
    load() {
      return this.loadSaved() || defaultProfileDoc();
    },
    save(profile) {
      const norm = normalizeProfileDoc(profile);
      try { localStorage.setItem(PROFILE_KEY, JSON.stringify(norm)); } catch {}
      return norm;
    },
    reset() {
      try { localStorage.removeItem(PROFILE_KEY); } catch {}
      return defaultProfileDoc();
    },
    loadSavedHomes() {
      try {
        const arr = JSON.parse(localStorage.getItem(SAVED_HOMES_KEY) || '[]');
        return Array.isArray(arr) ? arr.filter((x) => x && x.id) : [];
      } catch { return []; }
    },
    saveSavedHomes(list) {
      const clean = Array.isArray(list) ? list.slice(0, 60) : [];
      try { localStorage.setItem(SAVED_HOMES_KEY, JSON.stringify(clean)); } catch {}
      return clean;
    },
    isSavedHome(id) {
      return this.loadSavedHomes().some((h) => h.id === id);
    },
    toggleSavedHome(home) {
      if (!home || !home.id) return false;
      const list = this.loadSavedHomes();
      const idx = list.findIndex((h) => h.id === home.id);
      if (idx >= 0) {
        list.splice(idx, 1);
        this.saveSavedHomes(list);
        return false;
      }
      this.saveSavedHomes([Object.assign({}, home, { savedAt: new Date().toISOString() }), ...list]);
      return true;
    },
    loadUi() {
      try {
        const raw = JSON.parse(localStorage.getItem(UI_STATE_KEY) || '{}');
        const def = defaultUiState();
        const normLang = raw.lang === 'zh-CN' ? 'zh-CN' : (raw.lang === 'es' ? 'es' : 'en');
        return {
          lang: normLang,
          areas: {
            zips: Array.isArray(raw.areas?.zips) ? raw.areas.zips.slice(0, 10) : [],
            cities: Array.isArray(raw.areas?.cities) ? raw.areas.cities.slice(0, 6) : [],
          },
          loanProgram: typeof raw.loanProgram === 'string' ? raw.loanProgram : def.loanProgram,
          eraPreference: typeof raw.eraPreference === 'string' ? raw.eraPreference : def.eraPreference,
          aduNeeded: Boolean(raw.aduNeeded),
          step: Number.isInteger(raw.step) && raw.step >= 1 && raw.step <= 6 ? raw.step : 1,
          showAllSteps: Boolean(raw.showAllSteps),
        };
      } catch { return defaultUiState(); }
    },
    saveUi(patch) {
      const next = Object.assign(this.loadUi(), patch || {});
      try { localStorage.setItem(UI_STATE_KEY, JSON.stringify(next)); } catch {}
      return next;
    },
    async syncFromAccount() {
      try {
        if (typeof window.__openEstateHasSession !== 'function' || !window.__openEstateHasSession()) return null;
        const res = await fetch('/api/me/profile', { method: 'GET', headers: { 'Accept': 'application/json' } });
        if (!res.ok) return null;
        const j = await res.json();
        if (j && j.profile) return this.save(j.profile);
      } catch { /* 404 or offline: gracefully remain in local mode */ }
      return null;
    },
    async syncToAccount(profile) {
      try {
        if (typeof window.__openEstateHasSession !== 'function' || !window.__openEstateHasSession()) return false;
        const res = await fetch('/api/me/profile', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ profile: normalizeProfileDoc(profile) }),
        });
        return res.ok;
      } catch { return false; }
    },
    adapter: {
      mode: 'local',
      async load() { return profileStore.load(); },
      async save(p) { const n = profileStore.save(p); profileStore.syncToAccount(n); return n; },
      async reset() { return profileStore.reset(); },
    },
  };
  window.OpenEstateProfileStore = profileStore;

  const STYLE_SVGS = {
    modern: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M6 16h44v28H6zM44 22h30v22H44z"/><path d="M4 16h48M42 22h34" stroke-width="2.5"/><rect x="12" y="21" width="22" height="14" fill="currentColor" fill-opacity=".1"/><rect x="52" y="27" width="16" height="17" fill="currentColor" fill-opacity=".1"/></svg>',
    mid_century: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M4 24l36-12 36 8"/><path d="M10 22v22h60V19"/><path d="M10 22l30-10v10H10z" fill="currentColor" fill-opacity=".1"/><rect x="16" y="27" width="24" height="17"/><path d="M50 44V26h14v18"/></svg>',
    ranch: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M4 26L40 16l36 10H4z"/><path d="M8 26v18h64V26"/><rect x="14" y="30" width="14" height="8"/><rect x="34" y="30" width="8" height="14"/><rect x="48" y="30" width="18" height="14" fill="currentColor" fill-opacity=".08"/></svg>',
    split_level: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M6 24L24 14l18 10H6z"/><path d="M36 18L56 8l18 10H36z"/><path d="M8 24v20h30V24M38 18v26h34V18"/><rect x="14" y="32" width="18" height="12"/><rect x="46" y="22" width="10" height="7"/><rect x="46" y="34" width="10" height="10"/></svg>',
    craftsman: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M6 25L40 9l34 16"/><path d="M16 25l24-9 24 9"/><rect x="34" y="13" width="12" height="6"/><path d="M12 25v19h56V25"/><path d="M18 44V31l6-2 2 15M62 44V31l-6-2-2 15" fill="currentColor" fill-opacity=".1"/><rect x="35" y="31" width="10" height="13"/></svg>',
    farmhouse: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M18 22L40 6l22 16v22H18V22z"/><path d="M8 31h64v13H8z"/><path d="M8 31l10-6M72 31l-10-6"/><path d="M26 16v28M33 11v33M47 11v33M54 16v28" stroke-opacity=".35"/><rect x="36" y="34" width="8" height="10"/></svg>',
    barn: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M10 27L20 14 40 7l20 7 10 13v17H10V27z"/><rect x="35" y="14" width="10" height="8"/><rect x="29" y="29" width="22" height="15"/><path d="M29 29l22 15M51 29L29 44"/></svg>',
    colonial: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="14" y="17" width="52" height="27"/><path d="M11 17L40 8l29 9H11z"/><path d="M62 8v6h5V8"/><rect x="20" y="21" width="7" height="7"/><rect x="36.5" y="21" width="7" height="7"/><rect x="53" y="21" width="7" height="7"/><rect x="20" y="32" width="7" height="7"/><rect x="53" y="32" width="7" height="7"/><path d="M35 44V32l5-3 5 3v12"/></svg>',
    cape_cod: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M8 28L40 10l32 18H8z"/><rect x="12" y="28" width="56" height="16"/><path d="M21 23l5-5 5 5v5H21zM49 23l5-5 5 5v5H49z" fill="currentColor" fill-opacity=".08"/><rect x="36" y="32" width="8" height="12"/><rect x="37" y="7" width="6" height="6"/></svg>',
    victorian: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M12 22L22 6l10 16v22H12V22z"/><path d="M32 24L48 12l18 12v20H32V24z"/><circle cx="22" cy="18" r="3"/><rect x="18" y="30" width="8" height="14"/><rect x="42" y="29" width="8" height="15"/><path d="M40 18l8-6 8 6"/></svg>',
    tudor: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M10 24L26 9l16 15M36 24L54 11l16 13"/><path d="M12 24v20h56V24"/><path d="M26 9v35M18 17v27M54 11v33M12 31h56" stroke-opacity=".45"/><path d="M35 44v-9a5 5 0 0 1 10 0v9"/><rect x="60" y="7" width="6" height="10"/></svg>',
    mediterranean: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M8 22L40 12l32 10H8z" fill="currentColor" fill-opacity=".1"/><rect x="12" y="22" width="56" height="22"/><path d="M18 44v-9a5 5 0 0 1 10 0v9M35 44v-10a5 5 0 0 1 10 0v10M52 44v-9a5 5 0 0 1 10 0v9"/></svg>',
    southwestern: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M8 20h18v-6h28v8h18v22H8V20z"/><path d="M13 24h5M21 24h5M31 19h5M39 19h5M47 19h5M57 26h5M65 26h5" stroke-width="2.2"/><path d="M34 44v-11a6 6 0 0 1 12 0v11"/></svg>',
    cabin: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M14 44L40 8l26 36H14z"/><path d="M22 33h36M26 27h28M31 20h18" stroke-opacity=".45"/><rect x="35" y="34" width="10" height="10"/><path d="M68 44V30l-5 7h10l-5-7"/></svg>',
    traditional: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M8 24L40 11l32 13H8z"/><rect x="13" y="24" width="54" height="20"/><path d="M32 24l8-8 8 8"/><rect x="19" y="29" width="8" height="8"/><rect x="36" y="31" width="8" height="13"/><rect x="53" y="29" width="8" height="8"/></svg>',
    other_distinctive: '<svg width="78" height="48" viewBox="0 0 80 50" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M12 44V26h10V16h10V8h16v8h10v10h10v18H12z"/><path d="M40 8v36M32 16v28M48 16v28" stroke-opacity=".4"/><path d="M34 44V32a6 6 0 0 1 12 0v12"/></svg>',
  };

  const FALLBACK_CATALOG = {
    version: 1, profileVersion: 1, lang: 'en',
    constraints: { maxZips: 10, maxCities: 6, maxPlaces: 3 },
    dimensions: [
      { key: 'cost', label: 'Cost', description: 'All-in monthly cost compared with your budget.' },
      { key: 'schools', label: 'Schools', description: 'Assigned public schools, scored from official state data.' },
      { key: 'commute', label: 'Commute and my places', description: 'Drive time to the places you go, with a traffic buffer.' },
      { key: 'convenience', label: 'Daily convenience', description: 'Nearby shops, parks and services, and how walkable the area is.' },
      { key: 'homefit', label: 'Home fit', description: 'Beds, baths, size, lot, condition and layout compared with your needs.' },
      { key: 'style', label: 'Style and character', description: 'How closely the house matches the styles and features you like.' },
      { key: 'hazards', label: 'Natural hazards', description: 'Flood, wildfire, earthquake and other natural-hazard exposure.' },
      { key: 'crime', label: 'Crime', description: 'Reported city-level violent and property crime rates compared with the national rate.' },
      { key: 'environment', label: 'Environment and quiet', description: 'Air, noise, drinking water and nearby contamination.' },
      { key: 'investment', label: 'Investment and resale', description: 'Local appreciation, rental yield and affordability trends.' },
      { key: 'fengshui', label: 'Feng shui and orientation', description: 'Form-school and flying-star reading of the site (cultural guidance).' },
    ],
    presets: [
      { key: 'balanced', label: 'Balanced', description: 'A bit of everything, with cost and commute first.', weights: { cost: 16, schools: 9, commute: 13, convenience: 10, homefit: 12, style: 6, hazards: 9, crime: 8, environment: 7, investment: 8, fengshui: 2 } },
      { key: 'education_first', label: 'Education-first', description: 'Assigned public schools carry the most weight.', weights: { cost: 12, schools: 28, commute: 9, convenience: 6, homefit: 12, style: 4, hazards: 7, crime: 9, environment: 5, investment: 6, fengshui: 2 } },
      { key: 'commute_first', label: 'Commute-first', description: 'Short drives to the places you go matter most.', weights: { cost: 14, schools: 4, commute: 28, convenience: 14, homefit: 9, style: 5, hazards: 6, crime: 6, environment: 5, investment: 7, fengshui: 2 } },
      { key: 'multigenerational', label: 'Multigenerational household', description: 'Room for extended family: space, layout and in-law units.', weights: { cost: 16, schools: 9, commute: 9, convenience: 10, homefit: 24, style: 4, hazards: 7, crime: 8, environment: 5, investment: 6, fengshui: 2 } },
      { key: 'first_home', label: 'First home, budget-focused', description: 'Keeping the monthly cost comfortable comes first.', weights: { cost: 32, schools: 5, commute: 13, convenience: 9, homefit: 13, style: 3, hazards: 6, crime: 7, environment: 3, investment: 7, fengshui: 2 } },
      { key: 'investor', label: 'Investor', description: 'Appreciation, rental yield and resale come first.', weights: { cost: 15, schools: 7, commute: 5, convenience: 7, homefit: 7, style: 3, hazards: 10, crime: 7, environment: 5, investment: 32, fengshui: 2 } },
      { key: 'feng_shui', label: 'Feng-shui-conscious', description: 'Feng shui and orientation count alongside the basics.', weights: { cost: 12, schools: 12, commute: 7, convenience: 7, homefit: 12, style: 6, hazards: 8, crime: 7, environment: 7, investment: 7, fengshui: 15 } },
      { key: 'quiet_healthy', label: 'Quiet and healthy', description: 'Clean air, low noise and low natural-hazard exposure come first.', weights: { cost: 10, schools: 5, commute: 7, convenience: 8, homefit: 10, style: 4, hazards: 18, crime: 10, environment: 22, investment: 4, fengshui: 2 } },
      { key: 'walkable', label: 'Walkable lifestyle', description: 'Shops, parks and services within walking distance.', weights: { cost: 12, schools: 4, commute: 14, convenience: 28, homefit: 7, style: 6, hazards: 6, crime: 8, environment: 6, investment: 7, fengshui: 2 } },
      { key: 'design_lover', label: 'Design lover', description: 'The house style and character matter a lot.', weights: { cost: 14, schools: 5, commute: 10, convenience: 10, homefit: 14, style: 24, hazards: 6, crime: 6, environment: 5, investment: 4, fengshui: 2 } },
    ],
    styleFamilies: [
      { key: 'modern', label: 'Modern / Contemporary', resoValues: ['Modern', 'Contemporary'] },
      { key: 'mid_century', label: 'Mid-century modern', resoValues: ['Mid-Century Modern'] },
      { key: 'ranch', label: 'Ranch', resoValues: ['Ranch'] },
      { key: 'split_level', label: 'Split-level', resoValues: ['Split Level', 'Raised Ranch'] },
      { key: 'craftsman', label: 'Craftsman / Bungalow', resoValues: ['Craftsman', 'Bungalow', 'Prairie'] },
      { key: 'farmhouse', label: 'Farmhouse', resoValues: ['Farmhouse'] },
      { key: 'barn', label: 'Barn-style', resoValues: ['Barndominium'] },
      { key: 'colonial', label: 'Colonial', resoValues: ['Colonial', 'Dutch Colonial', 'Georgian'] },
      { key: 'cape_cod', label: 'Cape Cod', resoValues: ['Cape Cod'] },
      { key: 'victorian', label: 'Victorian', resoValues: ['Victorian', 'Queen Anne'] },
      { key: 'tudor', label: 'Tudor / European', resoValues: ['Tudor', 'French Provincial'] },
      { key: 'mediterranean', label: 'Mediterranean / Spanish', resoValues: ['Mediterranean', 'Spanish'] },
      { key: 'southwestern', label: 'Southwestern', resoValues: ['Pueblo'] },
      { key: 'cabin', label: 'Cabin / Rustic', resoValues: ['Cabin', 'Rustic', 'A-Frame'] },
      { key: 'traditional', label: 'Traditional', resoValues: ['Traditional', 'Creole'] },
      { key: 'other_distinctive', label: 'Other distinctive', resoValues: ['Art Deco', 'Dome', 'Berm'] },
    ],
    features: [
      { key: 'open_plan', label: 'Open floor plan' }, { key: 'high_ceilings', label: 'High or vaulted ceilings' },
      { key: 'natural_light', label: 'Natural light' }, { key: 'hardwood', label: 'Hardwood floors' },
      { key: 'fireplace', label: 'Fireplace' }, { key: 'pool', label: 'Pool' }, { key: 'view', label: 'View' },
      { key: 'main_level_primary', label: 'Main-level primary suite' }, { key: 'office', label: 'Home office' },
      { key: 'workshop_garage', label: 'Workshop garage' }, { key: 'ev_ready', label: 'EV charging' },
    ],
    screens: [
      { key: 'sfha_flood', label: 'High-risk flood zone (FEMA SFHA)' },
      { key: 'very_high_fire', label: 'Very high fire hazard zone' },
      { key: 'freeway_within_500ft', label: 'Freeway within 500 ft' },
      { key: 't_junction', label: 'T-junction (a street points at the house)' },
      { key: 'hoa_over', label: 'HOA dues above my limit' },
      { key: 'style_avoid', label: 'A style I marked “avoid”' },
      { key: 'address_digits', label: 'House number with digits I avoid' },
    ],
    schoolLevels: [
      { key: 'elementary', label: 'Elementary' }, { key: 'middle', label: 'Middle' }, { key: 'high', label: 'High' },
    ],
    propertyTypes: [
      { key: 'single_family', label: 'Single-family home' }, { key: 'townhouse', label: 'Townhouse' },
      { key: 'condo', label: 'Condo' }, { key: 'multi_family', label: 'Multi-family (2–4 units)' },
      { key: 'manufactured', label: 'Manufactured home' }, { key: 'other', label: 'Other' },
    ],
  };

  const UI_STRINGS = {
    en: {
      heroTitle: 'Comprehensive US Property Due-Diligence',
      heroSub: 'Instant valuation, natural hazard & environmental screening, real monthly cost, public schools & neighborhood life, 3D LiDAR roof & sun model, and AI inspection & title review for any US home.',
      tabReport: '🔍 Street Address Report', tabShop: 'Shop Homes', tabMatch: '✦ AI Home Finder', tabAlerts: '🔔 Area & ZIP Alerts',
      areaOnsaleLabel: 'See onsale property in your area:',
      areaZipPlaceholder: '5-digit US ZIP',
      areaZipBtn: 'Find homes',
      areaGeoBtn: '📍 Use my location',
      areaGeoLocating: 'Requesting your browser location & loading active listings nearby…',
      areaZipLoading: 'Looking up active for-sale properties…',
      areaGeoDenied: 'Location permission was declined — allow location access in your browser site settings or enter a 5-digit US ZIP code.',
      areaGeoUnavailable: 'Your device or browser could not determine your location — please enter a 5-digit US ZIP code instead.',
      areaGeoUnsupported: 'Browser location is unavailable here — enter a 5-digit US ZIP code instead.',
      areaZipInvalid: 'Enter a valid 5-digit US ZIP code.',
      areaEmpty: 'No active for-sale properties found in this area yet. Try a nearby ZIP code or configure a live MLS feed.',
      areaFoundPrefix: 'Active homes for sale in',
      areaGenerateCta: 'Generate report →',
      areaStatusForSale: 'For Sale',
      areaStatusPending: 'Pending',
      bqKicker: 'What your report answers',
      bqHeading: 'Know the real monthly cost and hidden risks before you make an offer',
      bq1Q: 'What will it really cost each month?',
      bq1A: 'Mortgage principal & interest, property taxes reassessed to your purchase price (never the seller’s old bill), homeowners & hazard insurance, maintenance reserve, and HOA — compared against market rent.',
      bq2Q: 'What could go wrong — or wasn’t disclosed?',
      bq2A: 'FEMA flood zone, USGS earthquake shaking & active faults, wildfire zones, EPA radon, Superfund & cleanup sites, water safety & traffic noise — plus upload inspection, disclosure, HOA, or title PDFs to cross-check against public records.',
      bq3Q: 'Is the asking price right?',
      bq3A: `${BRAND_NAME}’s True Estimate and value range from repeat-sales history, adjusted comparable sales, and county tax rolls — plus historical upside and drawdown stress scenarios.`,
      bq4Q: 'What is it like to live there?',
      bq4A: 'Assigned public schools from official state academic records, EPA walkability, groceries, parks & transit within a mile, drive times to your own places, and city crime context.',
      bq5Q: 'What can you do with the property?',
      bq5A: 'Interactive 3D LiDAR model of roof planes, tree overhang & year-round sun/shadow, zoning & ADU/lot context, a 50-second video tour, and side-by-side Deal Room comparison.',
      anatKicker: 'Inside every report',
      anatHeading: 'What you get for any US street address',
      anatSub: 'Every figure is computed from official public records and cited sources — nothing invented.',
      anat1T: '1 · All-in monthly cost breakdown vs. market rent',
      anat1D: 'Mortgage P&I, purchase-price reassessed property tax, homeowners & seismic/flood insurance, maintenance reserve, and HOA.',
      anat2T: '2 · Parcel-level hazard, environmental & document review',
      anat2D: 'FEMA, USGS, CAL FIRE / USFS, EPA Superfund, SDWIS water & US DOT noise — plus AI cross-check of uploaded inspection, disclosure & title PDFs.',
      anat3T: '3 · True Estimate, adjusted comps & downside stress test',
      anat3D: 'Repeat-sales indexation, nearby sold comps, county tax roll history, and 5-year historical return bands.',
      anat4T: '4 · 3D LiDAR lot model, 50s video tour & neighborhood life',
      anat4D: 'USGS 3DEP roof/tree/sun massing, official state school data, EPA walkability, and custom commute routing.',
      anatFoot: 'Federal, state & county public records',
      anatCta: 'Try an address above ↑',
      alertsCsvDesc: 'Paste a public MLS or county ZIP CSV export to seed or refresh local homes immediately:',
      footerNote: 'Public-record due-diligence research for US homes · Not a licensed appraisal or physical inspection',
      studioHeading: '✦ Your Match — 60-Second Quiz & Preference Studio',
      studioSub: 'Deterministic personalized scoring across 11 buyer dimensions — stored privately in your browser.',
      showAll: 'Show All Steps', showSteps: 'Step-by-Step View',
      step1: 'Areas & Presets', step2: 'Budget & Needs', step3: 'House Style',
      step4: 'Important Places', step5: 'Priorities (11)', step6: 'Schools & Deal-Breakers',
      s1Title: '1. Area Scope & Lifestyle Presets',
      s1Sub: 'Start with a lifestyle preset (weights are identical across languages) and pick up to 10 ZIP codes and 6 cities.',
      lblPresets: 'Lifestyle Presets (10)', lblAreaPicker: 'Search active/recent ZIPs or Cities',
      areaPh: 'Type 5-digit US ZIP or city…',
      areaHint: 'Click a suggestion or type a 5-digit ZIP and press Enter.',
      lblSelectedZips: 'Selected ZIPs', lblSelectedCities: 'Cities',
      s2Title: '2. Budget, Financing & Home Needs',
      s2Sub: 'All-in monthly cost scales with down payment and loan type; beds, baths, size and multigenerational layout shape Home Fit.',
      lblMonthlyBudget: 'Max Monthly Budget ($/mo)', lblMaxPrice: 'Max Purchase Price ($)',
      lblDownPct: 'Down Payment (%)', lblLoanProgram: 'Loan Type',
      lblMinBeds: 'Min Bedrooms', lblMinBaths: 'Min Bathrooms', lblMinSqft: 'Target Living Sq Ft', lblMinLotSqft: 'Target Lot Sq Ft',
      lblPropTypes: 'Property Types (leave empty for any)',
      lblMultigen: 'Multigenerational household (ADU size/baths count toward fit; layout weighs 40%)',
      lblAduNeeded: 'Prioritize existing ADU / in-law unit',
      s3Title: '3. House Style & Character (16 RESO Style Families)',
      s3Sub: 'Choose Love, Like, Neutral, or Avoid for any architectural styles, check desired character features, and tune style weight.',
      lblStyleWeight: 'Style & Character Weight', lblEraPref: 'Era Fallback Preference (when listing omits style)',
      lblFeatures: 'Character Features Checklist',
      love: 'Love', like: 'Like', neutral: 'Neutral', avoid: 'Avoid',
      s4Title: '4. Important Places (up to 3)',
      s4Sub: 'Add work, school, or family addresses for drive-time scoring (routed via configured router or road-distance estimates; places are yours alone and never auto-suggested by demographics).',
      lblPlaceLabel: 'Label', lblPlaceAddr: 'Street Address (autocomplete)', lblPlaceMode: 'Travel Mode',
      lblPlaceMaxMin: 'Max Commute (min)', lblPlaceTrips: 'Trips / Week', lblTrafficBuf: 'Peak Traffic Buffer',
      addPlace: '＋ Add Place',
      s5Title: '5. Priorities — Quick Trade-offs & 11 Live Sliders',
      s5Sub: 'Pick quick Most/Least trade-offs to shape priorities fast, then fine-tune any of the 11 dimensions. Weights normalize to 100% automatically.',
      lblMaxDiff: 'Quick Priority Trade-offs (MaxDiff Boosters)', lblAllSliders: 'All 11 Dimension Weights',
      normBtn: 'Normalize Sliders to 100', mostBtn: 'Most', leastBtn: 'Least', criticalLbl: 'Critical (<40 caps)',
      s6Title: '6. Public Schools & Deal-Breakers',
      s6Sub: 'Choose which school levels count when Schools has weight, and enable hard deal-breaker screens.',
      schoolNotice: 'ⓘ Official CA public state/federal academic data only (CAASPP & CA School Dashboard; commercial school ratings are never scored)',
      lblSchoolLevels: 'Public School Levels to Include (leave all unchecked to average all 3)',
      lblDealBreakers: 'Hard Deal-Breakers (7 Screens)',
      back: '← Back', next: 'Next →', reset: 'Reset', exportJson: 'Export / Import JSON',
      saveValidate: '✓ Save & Validate Profile', nlLabel: 'Describe your ideal home (price, ZIP or city, bedrooms, school grade, lot size, HOA…)',
      findBtn: 'Find Matching Homes', alertBtn: '🔔 Alert me on matches',
      hoaLimitLbl: 'Max HOA ($/mo):', avoidDigitsLbl: 'Avoid house digits:',
      explorerHeading: '✦ Ranked Home Explorer — List, Map & Compare',
      explorerSub: 'Deterministically rank active or sold homes across your selected ZIPs & cities, then compare 2–4 homes side-by-side or adjust weights for instant preview.',
      expViewList: '☰ List', expViewMap: '🗺 Map', expViewCompare: '⚖ Compare',
      rankAreaBtn: '✦ Rank Homes in My Areas', studioRankBtn: '✦ Rank Homes in My Areas →',
      lblExpAutoSync: 'Auto-sync ZIPs', lblExpIncludeFlagged: 'Show deal-breaker flagged & near-misses',
      lblExpDeepScan: 'Deep-scan top uncached homes',
    },
    'zh-CN': {
      heroTitle: '美国房产全维度智能尽职调查',
      heroSub: '为美国任一住宅提供真实估值、每月真实持有成本、自然灾害与环境筛查、公立学区与生活圈、3D 屋顶日照模型、传统堪舆风水及验房披露文件 AI 审阅。',
      tabReport: '🔍 单套房屋深度报告', tabShop: '条件选房', tabMatch: '✦ 智能找房 & 专属匹配', tabAlerts: '🔔 邮编新盘邮件提醒',
      areaOnsaleLabel: '查看您所在区域的在售房源：',
      areaZipPlaceholder: '输入 5 位美国邮编',
      areaZipBtn: '查找房源',
      areaGeoBtn: '📍 使用当前位置',
      areaGeoLocating: '正在获取浏览器定位并加载附近在售房源…',
      areaZipLoading: '正在查询该区域在售房源…',
      areaGeoDenied: '位置权限已被拒绝 — 请在浏览器地址栏站点设置中允许定位，或直接输入 5 位美国邮编查询。',
      areaGeoUnavailable: '当前设备或浏览器暂无法获取位置信息 — 请直接输入 5 位美国邮编查询。',
      areaGeoUnsupported: '当前浏览器不支持定位 — 请输入 5 位美国邮编查询。',
      areaZipInvalid: '请输入有效的 5 位美国邮编。',
      areaEmpty: '该区域暂无在售房源数据，请尝试附近邮编或配置实时房源连接器。',
      areaFoundPrefix: '当前区域在售房源 ·',
      areaGenerateCta: '点击生成报告 →',
      areaStatusForSale: '在售',
      areaStatusPending: '签约中',
      bqKicker: '一份报告解答买房 5 大核心问题',
      bqHeading: '下 Offer 前，先算清真实月供与潜在隐患',
      bq1Q: '每个月真正要花多少钱？',
      bq1A: '涵盖房贷本息、按您成交价重估的房产税（绝非卖家旧税单）、房屋与灾害保险、维护预留金及 HOA 月费，并与周边市场月租直观对比。',
      bq2Q: '有哪些潜在隐患或未披露问题？',
      bq2A: '逐项核查 FEMA 洪泛区、USGS 地震动与活断层、山火等级区、EPA 氡气、超级基金与污染场地、水质记录与交通噪音；支持上传验房、卖家披露、HOA 与产权 PDF 与公开记录交叉核对。',
      bq3Q: '挂牌价格合理吗？',
      bq3A: '综合同套历史成交指数回推、特征修正可比成交与县税务评估生成真实估值与区间，并附历史涨跌压力测试。',
      bq4Q: '住在这里日常体验如何？',
      bq4A: '采用州政府官方公立学校学业数据、EPA 步行便利指数、1 英里内超市/公园/公交统计、自选常去地点车程及城市治安背景。',
      bq5Q: '房屋与地块有哪些改造潜力？',
      bq5A: '基于激光雷达（LiDAR）生成交互式 3D 屋顶坡面、树木遮挡与全年日照阴影模型，结合分区与 ADU 规则、50 秒视频导览及多房源对比室。',
      anatKicker: '报告内容概览',
      anatHeading: '输入任一美国街道地址即可获得',
      anatSub: '所有数据均实时取自联邦、州与县级公开记录并附原始出处，绝不虚构任何数字。',
      anat1T: '1 · 全口径每月持有成本 vs. 市场月租',
      anat1D: '房贷本息、按成交价重估地税、房屋与地震/洪水险、维护预留金与 HOA 明细拆解。',
      anat2T: '2 · 地块级灾害、环境筛查与披露文件 AI 核验',
      anat2D: '覆盖 FEMA、USGS、山火、EPA 超级基金、饮用水与道路噪音，并可上传验房/披露/产权 PDF 交叉比对。',
      anat3T: '3 · 真实估值、可比成交修正与下行压力测试',
      anat3D: '融合同套复售指数、周边近期成交、县评估记录与 5 年历史收益分布区间。',
      anat4T: '4 · 3D 激光雷达模型、50 秒视频导览与生活圈',
      anat4D: 'USGS 3DEP 屋顶/树木/日照建模、州政府官方学区数据、EPA 步行便利指数与自定义通勤测算。',
      anatFoot: '联邦、州与县级权威公开记录',
      anatCta: '在上方输入地址体验 ↑',
      alertsCsvDesc: '粘贴公开房源列表 CSV 导出内容，即可立即导入或刷新本地房源库：',
      footerNote: '基于美国公开记录的购房尽职调查研究 · 不构成执业估价或现场验房报告',
      studioHeading: '✦ 专属匹配 — 60 秒偏好问卷与权重工作台',
      studioSub: '基于 11 项买房维度的确定性个性化评分（绝不用大模型瞎编分数）— 仅保存在您的浏览器本地。',
      showAll: '展开全部 6 步', showSteps: '分步向导视图',
      step1: '区域与预设', step2: '预算与户型', step3: '建筑风格',
      step4: '常去地点', step5: '11 项权重滑块', step6: '公立学区与红线',
      s1Title: '1. 目标区域与生活方式预设',
      s1Sub: '可先选一种生活方式预设（中英文数值权重完全一致），并多选最多 10 个邮编与 6 个城市。',
      lblPresets: '生活方式预设（10 种）', lblAreaPicker: '搜索活跃/近期房源邮编或城市',
      areaPh: '输入 5 位美国邮编或城市名称…',
      areaHint: '点击联想项或输入 5 位邮编后按回车添加。',
      lblSelectedZips: '已选邮编', lblSelectedCities: '已选城市',
      s2Title: '2. 预算、贷款方案与房屋需求',
      s2Sub: '每月总持有成本按首付比例与贷款类型联动；卧室、卫浴、面积与多代同堂影响房屋契合度。',
      lblMonthlyBudget: '每月预算上限（$/月）', lblMaxPrice: '最高购房总价（$）',
      lblDownPct: '首付比例（%）', lblLoanProgram: '贷款类型',
      lblMinBeds: '最少卧室', lblMinBaths: '最少卫浴', lblMinSqft: '目标室内面积 (sqft)', lblMinLotSqft: '目标占地面积 (sqft)',
      lblPropTypes: '房屋类型（全不选表示不限）',
      lblMultigen: '多代同堂需求（现成 ADU 面积和卫浴计入契合度，格局权重提升至 40%）',
      lblAduNeeded: '优先带现成 ADU / 姻亲房',
      s3Title: '3. 房屋建筑风格与特色（16 种 RESO 标准风格）',
      s3Sub: '为喜欢的建筑风格标记“很喜欢 / 喜欢 / 中立 / 避免”，勾选期望特色并调节风格权重。',
      lblStyleWeight: '风格与特色权重', lblEraPref: '建筑年代偏好（当挂牌未标明风格时作参考）',
      lblFeatures: '房屋特色清单',
      love: '很喜欢', like: '喜欢', neutral: '中立', avoid: '避免',
      s4Title: '4. 常去地点（最多 3 个）',
      s4Sub: '添加工作、学校或家人地址，基于已配置路线引擎或道路距离估算通勤时间（仅按您提供的地址计算，绝不按人群特征推荐）。',
      lblPlaceLabel: '标签', lblPlaceAddr: '美国街道地址（自动联想）', lblPlaceMode: '出行方式',
      lblPlaceMaxMin: '可接受最长车程（分钟）', lblPlaceTrips: '每周往返次数', lblTrafficBuf: '高峰拥堵缓冲系数',
      addPlace: '＋ 添加地点',
      s5Title: '5. 优先级权衡 — 快速对比与 11 项实时滑块',
      s5Sub: '先通过快速“最看重 / 最不看重”卡片定调，再微调全部 11 项维度，系统自动换算 100% 权重占比。',
      lblMaxDiff: '快速优先级权衡（MaxDiff）', lblAllSliders: '全部 11 项维度权重滑块',
      normBtn: '一键归一化为 100', mostBtn: '最看重', leastBtn: '最不看重', criticalLbl: '硬性底线 (<40 封顶)',
      s6Title: '6. 公立学校与一票否决红线',
      s6Sub: '选择纳入计算的公立学段，并启用 7 项一票否决（Deal-Breaker）筛选开关。',
      schoolNotice: 'ⓘ Official CA public state/federal academic data only（仅采用加州及联邦官方公立学校学业数据，绝不采用商业学区评分）',
      lblSchoolLevels: '纳入评分的公立学段（全不选则平均全部 3 个学段）',
      lblDealBreakers: '一票否决硬性红线（7 项筛选）',
      back: '← 上一步', next: '下一步 →', reset: '恢复默认', exportJson: '导出 / 导入 JSON',
      saveValidate: '✓ 验证并保存偏好档案', nlLabel: '用自然语言描述理想房屋（价格、邮编或城市、卧室、学区、占地、HOA…）',
      findBtn: '查找匹配房源', alertBtn: '🔔 订阅匹配新盘提醒',
      hoaLimitLbl: 'HOA 月费上限 ($/mo):', avoidDigitsLbl: '避开门牌数字:',
      explorerHeading: '✦ 区域专属匹配排行榜 — 列表、地图与对比',
      explorerSub: '对所选邮编与城市内的在售或近期成交房源进行确定性综合排序，支持 2–4 套房源并排对比及滑块实时预览重排。',
      expViewList: '☰ 列表视图', expViewMap: '🗺 地图分布', expViewCompare: '⚖ 对比分析',
      rankAreaBtn: '✦ 按偏好排定目标区域房源', studioRankBtn: '✦ 排定目标区域房源 →',
      lblExpAutoSync: '自动同步邮编房源', lblExpIncludeFlagged: '显示触及红线及接近达标房源',
      lblExpDeepScan: '后台深度扫描前几套未缓存房源',
    },
    es: {
      heroTitle: 'Análisis Integral de Viviendas en EE. UU.',
      heroSub: 'Estimación Real, costo mensual completo, riesgos naturales y ambientales, escuelas públicas y vecindario, modelo 3D LiDAR de techo y sol, y revisión con IA de inspecciones y título para cualquier vivienda en EE. UU.',
      tabReport: '🔍 Informe por Dirección', tabShop: 'Comprar Casas', tabMatch: '✦ Buscador Inteligente y Your Match', tabAlerts: '🔔 Alertas por Código Postal',
      areaOnsaleLabel: 'Ver propiedades en venta en su zona:',
      areaZipPlaceholder: 'Código ZIP de 5 dígitos',
      areaZipBtn: 'Buscar casas',
      areaGeoBtn: '📍 Usar mi ubicación',
      areaGeoLocating: 'Solicitando ubicación del navegador y buscando casas en venta…',
      areaZipLoading: 'Buscando propiedades activas en venta…',
      areaGeoDenied: 'Permiso de ubicación denegado — permita el acceso en su navegador o ingrese un código ZIP de 5 dígitos.',
      areaGeoUnavailable: 'No se pudo determinar su ubicación actual — ingrese un código ZIP de EE. UU. de 5 dígitos.',
      areaGeoUnsupported: 'Ubicación no disponible aquí — ingrese un código ZIP de 5 dígitos.',
      areaZipInvalid: 'Ingrese un código ZIP válido de 5 dígitos.',
      areaEmpty: 'Aún no hay propiedades en venta indexadas en esta zona. Pruebe otro ZIP cercano.',
      areaFoundPrefix: 'Propiedades en venta en',
      areaGenerateCta: 'Generar informe →',
      areaStatusForSale: 'En venta',
      areaStatusPending: 'Pendiente',
      bqKicker: 'Lo que responde su informe',
      bqHeading: 'Conozca el costo mensual real y los riesgos ocultos antes de ofertar',
      bq1Q: '¿Cuánto costará realmente cada mes?',
      bq1A: 'Capital e intereses hipotecarios, impuesto predial reevaluado al precio de compra (no el recibo antiguo del vendedor), seguro de hogar y riesgos, reserva de mantenimiento y HOA — comparado con la renta de mercado.',
      bq2Q: '¿Qué podría salir mal o no fue revelado?',
      bq2A: 'Zona de inundación FEMA, sacudida sísmica y fallas activas USGS, incendios forestales, radón EPA, sitios Superfund, agua potable y ruido vial — más carga de PDFs de inspección, divulgaciones, HOA o título para cruzarlos con registros públicos.',
      bq3Q: '¿El precio de venta es justo?',
      bq3A: 'Estimación Real y rango de valor calculados con historial de ventas repetidas, ventas comparables ajustadas y registros fiscales del condado, con escenarios históricos al alza y de caída.',
      bq4Q: '¿Cómo es vivir allí día a día?',
      bq4A: 'Escuelas públicas asignadas con datos académicos estatales oficiales, caminabilidad EPA, supermercados, parques y transporte a 1 milla, tiempos de traslado a sus lugares importantes y contexto de seguridad.',
      bq5Q: '¿Qué se puede hacer con la propiedad?',
      bq5A: 'Modelo 3D LiDAR interactivo de planos de techo, árboles y sol/sombra todo el año, zonificación y potencial de ADU, video tour de 50 segundos y comparación lado a lado en Sala de Comparación.',
      anatKicker: 'Dentro de cada informe',
      anatHeading: 'Lo que obtiene para cualquier dirección en EE. UU.',
      anatSub: 'Cada cifra proviene de registros públicos oficiales con fuentes citadas — sin datos inventados.',
      anat1T: '1 · Desglose del costo mensual real vs. renta de mercado',
      anat1D: 'Hipoteca, impuesto predial reevaluado al comprar, seguros, reserva de mantenimiento y cuota HOA.',
      anat2T: '2 · Riesgos de la parcela, ambiente y revisión de documentos con IA',
      anat2D: 'FEMA, USGS, incendios, EPA Superfund, agua SDWIS y ruido US DOT — más cruce de PDFs de inspección, divulgación y título.',
      anat3T: '3 · Estimación Real, comparables ajustados y prueba de estrés',
      anat3D: 'Índice de ventas repetidas, cierres cercanos, historial fiscal del condado y bandas históricas a 5 años.',
      anat4T: '4 · Modelo 3D LiDAR, video tour de 50s y vida diaria',
      anat4D: 'Volumetría USGS 3DEP de techo/árboles/sol, datos escolares oficiales, caminabilidad EPA y rutas propias.',
      anatFoot: 'Registros públicos federales, estatales y del condado',
      anatCta: 'Probar una dirección arriba ↑',
      alertsCsvDesc: 'Pegue un archivo CSV público de viviendas por código ZIP para cargar o actualizar casas al instante:',
      footerNote: 'Investigación con registros públicos de EE. UU. · No sustituye una tasación oficial ni una inspección física',
      studioHeading: '✦ Your Match — Cuestionario de 60 Segundos y Preferencias',
      studioSub: 'Puntuación determinista personalizada en 11 dimensiones — guardada de forma privada en su navegador.',
      showAll: 'Ver los 6 pasos', showSteps: 'Vista paso a paso',
      step1: 'Zonas y Estilos', step2: 'Presupuesto', step3: 'Arquitectura',
      step4: 'Mis Lugares', step5: 'Prioridades (11)', step6: 'Escuelas y Límites',
      s1Title: '1. Zonas de Búsqueda y Perfiles de Estilo de Vida',
      s1Sub: 'Elija un perfil inicial (los pesos son idénticos en todos los idiomas) y seleccione hasta 10 códigos ZIP y 6 ciudades.',
      lblPresets: 'Perfiles de Estilo de Vida (10)', lblAreaPicker: 'Buscar códigos ZIP o ciudades con viviendas',
      areaPh: 'Escriba un ZIP de 5 dígitos o ciudad…',
      areaHint: 'Haga clic en una sugerencia o escriba un ZIP de 5 dígitos y presione Enter.',
      lblSelectedZips: 'ZIPs seleccionados', lblSelectedCities: 'Ciudades',
      s2Title: '2. Presupuesto, Financiamiento y Necesidades',
      s2Sub: 'El costo mensual total se calcula según el enganche y tipo de préstamo; recámaras, baños, tamaño y diseño multigeneracional ajustan su compatibilidad.',
      lblMonthlyBudget: 'Presupuesto Mensual Máx. ($/mes)', lblMaxPrice: 'Precio Máximo de Compra ($)',
      lblDownPct: 'Enganche (%)', lblLoanProgram: 'Tipo de Préstamo',
      lblMinBeds: 'Recámaras Mín.', lblMinBaths: 'Baños Mín.', lblMinSqft: 'Área Interior Meta (pies²)', lblMinLotSqft: 'Terreno Meta (pies²)',
      lblPropTypes: 'Tipos de Propiedad (vacío = cualquiera)',
      lblMultigen: 'Hogar multigeneracional (el área/baños de ADU suman; distribución pesa 40%)',
      lblAduNeeded: 'Priorizar ADU o unidad independiente existente',
      s3Title: '3. Estilo Arquitectónico y Carácter (16 Familias RESO)',
      s3Sub: 'Indique Me encanta, Me gusta, Neutral o Evitar para cada estilo, elija características deseadas y ajuste el peso.',
      lblStyleWeight: 'Peso de Estilo y Carácter', lblEraPref: 'Preferencia de Época (si el anuncio omite el estilo)',
      lblFeatures: 'Características Deseadas',
      love: 'Me encanta', like: 'Me gusta', neutral: 'Neutral', avoid: 'Evitar',
      s4Title: '4. Lugares Importantes (hasta 3)',
      s4Sub: 'Agregue direcciones de trabajo, escuela o familia para calcular tiempos de traslado (enrutados si está configurado o estimados por distancia vial; nunca sugeridos por demografía).',
      lblPlaceLabel: 'Etiqueta', lblPlaceAddr: 'Dirección (autocompletar)', lblPlaceMode: 'Modo de Viaje',
      lblPlaceMaxMin: 'Tiempo Máximo (min)', lblPlaceTrips: 'Viajes / Semana', lblTrafficBuf: 'Margen de Tráfico Pico',
      addPlace: '＋ Agregar Lugar',
      s5Title: '5. Prioridades — Comparaciones Rápidas y 11 Controles en Vivo',
      s5Sub: 'Elija comparaciones rápidas Más/Menos importante o ajuste cualquiera de las 11 dimensiones (se normalizan a 100%).',
      lblMaxDiff: 'Prioridades Rápidas (MaxDiff)', lblAllSliders: 'Las 11 Dimensiones de Puntuación',
      normBtn: 'Normalizar a 100', mostBtn: 'Más', leastBtn: 'Menos', criticalLbl: 'Crítico (<40 limita)',
      s6Title: '6. Escuelas Públicas y Factores Excluyentes',
      s6Sub: 'Elija niveles escolares oficiales cuando Escuelas tenga peso y active filtros excluyentes estrictos.',
      schoolNotice: 'ⓘ Official CA public state/federal academic data only · Datos académicos oficiales públicos únicamente (CAASPP y CA School Dashboard; nunca calificaciones comerciales)',
      lblSchoolLevels: 'Niveles Escolares a Incluir (sin selección = promedio de los 3)',
      lblDealBreakers: 'Factores Excluyentes Estrictos (7 Filtros)',
      back: '← Atrás', next: 'Siguiente →', reset: 'Restablecer', exportJson: 'Exportar / Importar JSON',
      saveValidate: '✓ Guardar y Validar Perfil', nlLabel: 'Describa su casa ideal (precio, código ZIP o ciudad, recámaras, escuelas, terreno, HOA…)',
      findBtn: 'Buscar Viviendas Compatibles', alertBtn: '🔔 Recibir alertas de estas viviendas',
      hoaLimitLbl: 'HOA Máx. ($/mes):', avoidDigitsLbl: 'Evitar dígitos en dirección:',
      explorerHeading: '✦ Explorador Clasificado — Lista, Mapa y Comparación',
      explorerSub: 'Ordene viviendas activas o vendidas en sus ZIPs y ciudades de forma determinista, compare de 2 a 4 casas lado a lado o mueva los controles al instante.',
      expViewList: '☰ Lista', expViewMap: '🗺 Mapa', expViewCompare: '⚖ Comparar',
      rankAreaBtn: '✦ Clasificar Viviendas en Mis Zonas', studioRankBtn: '✦ Clasificar en Mis Zonas →',
      lblExpAutoSync: 'Sincronizar ZIPs automáticamente', lblExpIncludeFlagged: 'Mostrar excluyentes y casi compatibles',
      lblExpDeepScan: 'Escaneo profundo en segundo plano para mejores candidatas',
    },
  };

  const MAXDIFF_ROUNDS = [
    { id: 'r1', dims: ['cost', 'schools', 'commute'] },
    { id: 'r2', dims: ['homefit', 'style', 'convenience'] },
    { id: 'r3', dims: ['hazards', 'environment', 'crime'] },
    { id: 'r4', dims: ['investment', 'fengshui', 'homefit'] },
  ];

  let activeCatalog = FALLBACK_CATALOG;
  let currentProfile = profileStore.load();
  let uiState = profileStore.loadUi();
  const maxDiffPicks = {};
  let studioInitialized = false;
  let explorerView = 'list';
  let lastAreaRankResult = null;
  let explorerNeedsServerResync = false;
  let rankPollTimer = null;
  let expLeafletMap = null;
  const compareIds = [];

  const tUi = (k) => (UI_STRINGS[uiState.lang] || UI_STRINGS.en)[k] || UI_STRINGS.en[k] || k;
  const tr = (en, zh, es) => (uiState.lang === 'zh-CN' ? zh : uiState.lang === 'es' ? es : en);

  async function fetchCatalogForLang(lang) {
    try {
      const r = await fetch('/api/match/catalog?lang=' + encodeURIComponent(lang));
      if (r.ok) {
        const j = await r.json();
        if (j && Array.isArray(j.dimensions) && Array.isArray(j.presets)) {
          activeCatalog = j;
        }
      }
    } catch {}
  }

  function computeShares(weights) {
    let sum = 0;
    for (const d of VALID_DIMS) sum += Math.max(0, Number(weights[d]) || 0);
    const shares = {};
    for (const d of VALID_DIMS) shares[d] = sum > 0 ? (Math.max(0, Number(weights[d]) || 0) / sum) : 0;
    return { sum, shares };
  }

  function persistAll(msg) {
    currentProfile = profileStore.save(currentProfile);
    uiState = profileStore.saveUi(uiState);
    updateStudioSummary();
    if (msg) {
      const el = document.getElementById('profileSaveStatus');
      if (el) { el.style.color = '#0f766e'; el.textContent = msg; }
    }
  }

  function updateStudioSummary() {
    const pill = document.getElementById('studioSummaryPill');
    if (!pill) return;
    const pObj = (activeCatalog.presets || []).find((p) => p.key === currentProfile.preset);
    const presetName = pObj ? pObj.label : tr('Custom', '自定义', 'Personalizado');
    pill.textContent = `${presetName} · ${uiState.areas.zips.length}/10 ${tr('ZIPs', '邮编', 'ZIPs')} · ${uiState.areas.cities.length}/6 ${tr('Cities', '城市', 'Ciudades')} · ${(currentProfile.places || []).length}/3 ${tr('Places', '地点', 'Lugares')}`;
    const zc = document.getElementById('zipCount'); if (zc) zc.textContent = String(uiState.areas.zips.length);
    const cc = document.getElementById('cityCount'); if (cc) cc.textContent = String(uiState.areas.cities.length);
    const pc = document.getElementById('placeCount'); if (pc) pc.textContent = String((currentProfile.places || []).length);
  }

  function applyStaticUiLabels() {
    document.documentElement.lang = uiState.lang === 'zh-CN' ? 'zh-CN' : uiState.lang === 'es' ? 'es' : 'en';
    document.querySelectorAll('#langSwitch button').forEach((b) => b.classList.toggle('on', b.dataset.lang === uiState.lang));
    const mapText = [
      ['heroTitle', 'heroTitle'], ['heroSub', 'heroSub'],
      ['tabBtnReport', 'tabReport'], ['tabBtnShop', 'tabShop'], ['tabBtnMatch', 'tabMatch'], ['tabBtnAlerts', 'tabAlerts'],
      ['areaOnsaleLabel', 'areaOnsaleLabel'], ['areaZipBtn', 'areaZipBtn'], ['areaGeoBtn', 'areaGeoBtn'],
      ['bqKicker', 'bqKicker'], ['bqHeading', 'bqHeading'],
      ['bq1Q', 'bq1Q'], ['bq1A', 'bq1A'], ['bq2Q', 'bq2Q'], ['bq2A', 'bq2A'],
      ['bq3Q', 'bq3Q'], ['bq3A', 'bq3A'], ['bq4Q', 'bq4Q'], ['bq4A', 'bq4A'],
      ['bq5Q', 'bq5Q'], ['bq5A', 'bq5A'],
      ['anatKicker', 'anatKicker'], ['anatHeading', 'anatHeading'], ['anatSub', 'anatSub'],
      ['anat1T', 'anat1T'], ['anat1D', 'anat1D'], ['anat2T', 'anat2T'], ['anat2D', 'anat2D'],
      ['anat3T', 'anat3T'], ['anat3D', 'anat3D'], ['anat4T', 'anat4T'], ['anat4D', 'anat4D'],
      ['anatFoot', 'anatFoot'], ['anatFocusSearchBtn', 'anatCta'],
      ['alertsCsvDesc', 'alertsCsvDesc'], ['footerNote', 'footerNote'],
      ['studioHeading', 'studioHeading'], ['studioSub', 'studioSub'],
      ['s1Title', 's1Title'], ['s1Sub', 's1Sub'], ['lblPresets', 'lblPresets'], ['lblAreaPicker', 'lblAreaPicker'],
      ['areaHint', 'areaHint'], ['lblSelectedZips', 'lblSelectedZips'], ['lblSelectedCities', 'lblSelectedCities'],
      ['s2Title', 's2Title'], ['s2Sub', 's2Sub'], ['lblMonthlyBudget', 'lblMonthlyBudget'], ['lblMaxPrice', 'lblMaxPrice'],
      ['lblDownPct', 'lblDownPct'], ['lblLoanProgram', 'lblLoanProgram'], ['lblMinBeds', 'lblMinBeds'], ['lblMinBaths', 'lblMinBaths'],
      ['lblMinSqft', 'lblMinSqft'], ['lblMinLotSqft', 'lblMinLotSqft'], ['lblPropTypes', 'lblPropTypes'],
      ['lblMultigen', 'lblMultigen'], ['lblAduNeeded', 'lblAduNeeded'],
      ['s3Title', 's3Title'], ['s3Sub', 's3Sub'], ['lblStyleWeight', 'lblStyleWeight'], ['lblEraPref', 'lblEraPref'], ['lblFeatures', 'lblFeatures'],
      ['s4Title', 's4Title'], ['s4Sub', 's4Sub'], ['lblPlaceLabel', 'lblPlaceLabel'], ['lblPlaceAddr', 'lblPlaceAddr'],
      ['lblPlaceMode', 'lblPlaceMode'], ['lblPlaceMaxMin', 'lblPlaceMaxMin'], ['lblPlaceTrips', 'lblPlaceTrips'], ['lblTrafficBuf', 'lblTrafficBuf'],
      ['s5Title', 's5Title'], ['s5Sub', 's5Sub'], ['lblMaxDiff', 'lblMaxDiff'], ['lblAllSliders', 'lblAllSliders'],
      ['normalizeWeightsBtn', 'normBtn'], ['s6Title', 's6Title'], ['s6Sub', 's6Sub'],
      ['schoolDataNotice', 'schoolNotice'], ['lblSchoolLevels', 'lblSchoolLevels'], ['lblDealBreakers', 'lblDealBreakers'],
      ['prevStepBtn', 'back'], ['nextStepBtn', 'next'], ['resetProfileBtn', 'reset'], ['toggleExportBtn', 'exportJson'],
      ['saveProfileBtn', 'saveValidate'], ['studioRankBtn', 'studioRankBtn'],
      ['explorerHeading', 'explorerHeading'], ['explorerSub', 'explorerSub'],
      ['expViewListBtn', 'expViewList'], ['expViewMapBtn', 'expViewMap'],
      ['rankAreaBtn', 'rankAreaBtn'], ['lblExpAutoSync', 'lblExpAutoSync'], ['lblExpIncludeFlagged', 'lblExpIncludeFlagged'], ['lblExpDeepScan', 'lblExpDeepScan'],
      ['nlLabel', 'nlLabel'], ['nlSearchBtn', 'findBtn'], ['nlToAlertBtn', 'alertBtn'],
    ];
    for (const [id, key] of mapText) {
      const el = document.getElementById(id);
      if (el) el.textContent = tUi(key);
    }
    const az = document.getElementById('areaZipInput');
    if (az) {
      az.placeholder = tUi('areaZipPlaceholder');
      az.setAttribute('aria-label', tUi('areaZipPlaceholder'));
    }
    const cmpBtn = document.getElementById('expViewCompareBtn');
    if (cmpBtn) cmpBtn.innerHTML = `${escH(tUi('expViewCompare'))} (<span id="compareCountBadge">${compareIds.length}</span>)`;
    const ai = document.getElementById('areaSearchInput');
    if (ai) ai.placeholder = tUi('areaPh');
    const toggleAll = document.getElementById('toggleAllStepsBtn');
    if (toggleAll) toggleAll.textContent = uiState.showAllSteps ? tUi('showSteps') : tUi('showAll');
    for (let i = 1; i <= 6; i++) {
      const span = document.querySelector(`[data-i18n="step${i}"]`);
      if (span) span.textContent = tUi(`step${i}`);
    }
    if (typeof window.__renderAreaOnsalePayload === 'function') window.__renderAreaOnsalePayload();
    if (typeof applyShopStaticLabels === 'function') applyShopStaticLabels();
    if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
  }

  function renderQuizStepsVisibility() {
    const showAll = uiState.showAllSteps;
    document.querySelectorAll('.stepbtn').forEach((b) => {
      b.classList.toggle('act', !showAll && Number(b.dataset.step) === uiState.step);
    });
    document.querySelectorAll('.qstep').forEach((sec) => {
      sec.classList.toggle('act', showAll || Number(sec.dataset.step) === uiState.step);
    });
    const prev = document.getElementById('prevStepBtn');
    const next = document.getElementById('nextStepBtn');
    if (prev) prev.style.display = showAll ? 'none' : '';
    if (next) next.style.display = showAll ? 'none' : '';
  }

  function renderPresets() {
    const grid = document.getElementById('presetGrid');
    if (!grid) return;
    grid.innerHTML = (activeCatalog.presets || []).map((p) => {
      const act = currentProfile.preset === p.key ? ' act' : '';
      return `<button type="button" class="presetbtn${act}" data-preset="${escH(p.key)}"><div class="pt">${escH(p.label)}</div><div class="pd">${escH(p.description || '')}</div></button>`;
    }).join('');
    grid.querySelectorAll('[data-preset]').forEach((btn) => {
      btn.addEventListener('click', () => {
        const key = btn.dataset.preset;
        const p = (activeCatalog.presets || []).find((x) => x.key === key);
        if (!p) return;
        currentProfile.preset = key;
        currentProfile.weights = Object.assign({}, p.weights);
        persistAll();
        renderPresets();
        renderStyleSliderValue();
        renderSliders();
        if (typeof previewReRankClientSide === 'function') previewReRankClientSide();
      });
    });
  }

  function renderSelectedAreas() {
    const box = document.getElementById('areaChipsBox');
    if (!box) return;
    const zChips = uiState.areas.zips.map((z) => `<span class="selchip">ZIP ${escH(z)} <button type="button" data-rm-zip="${escH(z)}" aria-label="Remove">×</button></span>`);
    const cChips = uiState.areas.cities.map((c, idx) => `<span class="selchip city">${escH(c.label || `${c.city}, ${c.state}`)} <button type="button" data-rm-city="${idx}" aria-label="Remove">×</button></span>`);
    box.innerHTML = (zChips.length || cChips.length)
      ? zChips.concat(cChips).join('')
      : `<span style="font-size:12px;color:#6b7280">${escH(tr('No ZIPs or cities selected yet.', '暂未选择区域（点击搜索框选择邮编或城市）', 'Aún no ha seleccionado códigos ZIP ni ciudades.'))}</span>`;
    box.querySelectorAll('[data-rm-zip]').forEach((b) => b.addEventListener('click', () => {
      uiState.areas.zips = uiState.areas.zips.filter((z) => z !== b.dataset.rmZip);
      persistAll();
      renderSelectedAreas();
    }));
    box.querySelectorAll('[data-rm-city]').forEach((b) => b.addEventListener('click', () => {
      const idx = Number(b.dataset.rmCity);
      uiState.areas.cities = uiState.areas.cities.filter((_, j) => j !== idx);
      persistAll();
      renderSelectedAreas();
    }));
    updateStudioSummary();
  }

  function addZipChip(zip) {
    const clean = String(zip || '').trim().slice(0, 5);
    if (!/^\d{5}$/.test(clean)) return;
    if (uiState.areas.zips.includes(clean) || uiState.areas.zips.length >= 10) return;
    uiState.areas.zips.push(clean);
    persistAll();
    renderSelectedAreas();
  }

  function addCityChip(cityObj) {
    if (!cityObj || !cityObj.city) return;
    if (uiState.areas.cities.length >= 6) return;
    const label = cityObj.label || (cityObj.state ? `${cityObj.city}, ${cityObj.state}` : cityObj.city);
    if (uiState.areas.cities.some((c) => (c.label || '').toLowerCase() === label.toLowerCase())) return;
    uiState.areas.cities.push({ city: cityObj.city, state: cityObj.state || '', label });
    persistAll();
    renderSelectedAreas();
  }

  function renderBudgetAndNeeds() {
    const setVal = (id, v) => { const el = document.getElementById(id); if (el) el.value = v ?? ''; };
    setVal('qMonthlyBudget', currentProfile.budget?.monthly);
    setVal('qMaxPrice', currentProfile.budget?.maxPrice);
    setVal('qDownPct', currentProfile.downPaymentPct ?? 20);
    setVal('qLoanProgram', uiState.loanProgram || (currentProfile.loanType === 'conventional' ? '30yr_fixed' : currentProfile.loanType));
    setVal('qMinBeds', currentProfile.minBeds != null ? String(currentProfile.minBeds) : '');
    setVal('qMinBaths', currentProfile.minBaths != null ? String(currentProfile.minBaths) : '');
    setVal('qMinSqft', currentProfile.minSqft);
    setVal('qMinLotSqft', currentProfile.minLotSqft);
    const mg = document.getElementById('qMultigen'); if (mg) mg.checked = Boolean(currentProfile.multigenerational);
    const ad = document.getElementById('qAduNeeded'); if (ad) ad.checked = Boolean(uiState.aduNeeded);

    const ptBox = document.getElementById('propTypesBox');
    if (ptBox) {
      const selected = new Set(currentProfile.propertyTypes || []);
      ptBox.innerHTML = (activeCatalog.propertyTypes || []).map((k) =>
        `<label><input type="checkbox" data-proptype="${escH(k.key)}" ${selected.has(k.key) ? 'checked' : ''}> ${escH(k.label)}</label>`
      ).join('');
      ptBox.querySelectorAll('[data-proptype]').forEach((cb) => cb.addEventListener('change', () => {
        const arr = [];
        ptBox.querySelectorAll('[data-proptype]:checked').forEach((x) => arr.push(x.dataset.proptype));
        currentProfile.propertyTypes = arr;
        persistAll();
      }));
    }
  }

  function renderStyleSliderValue() {
    const sl = document.getElementById('qStyleSlider');
    const val = document.getElementById('qStyleWeightVal');
    const sh = document.getElementById('qStyleShareVal');
    const w = Math.round(currentProfile.weights.style || 0);
    const { shares } = computeShares(currentProfile.weights);
    if (sl) sl.value = String(w);
    if (val) val.textContent = String(w);
    if (sh) sh.textContent = `${(shares.style * 100).toFixed(1)}%`;
  }

  function renderStylesAndFeatures() {
    renderStyleSliderValue();
    const era = document.getElementById('qEraPref');
    if (era) era.value = uiState.eraPreference || 'any';
    const grid = document.getElementById('styleCardsGrid');
    if (grid) {
      const prefs = currentProfile.stylePreferences || {};
      grid.innerHTML = (activeCatalog.styleFamilies || []).map((f) => {
        const pref = prefs[f.key] || 'neutral';
        const svg = STYLE_SVGS[f.key] || STYLE_SVGS.traditional;
        const resoTxt = (f.resoValues || []).slice(0, 3).join(' · ');
        return `<div class="stylecard ${pref}" data-style-card="${escH(f.key)}">
          <div class="stylesvg">${svg}</div>
          <div class="stylename">${escH(f.label)}</div>
          <div class="stylereso">${escH(resoTxt)}</div>
          <div class="stylebtns">
            <button type="button" data-sp="love" class="${pref === 'love' ? 'on-love' : ''}">${escH(tUi('love'))}</button>
            <button type="button" data-sp="fine" class="${pref === 'fine' ? 'on-fine' : ''}">${escH(tUi('like'))}</button>
            <button type="button" data-sp="neutral" class="${pref === 'neutral' ? 'on-neutral' : ''}">${escH(tUi('neutral'))}</button>
            <button type="button" data-sp="avoid" class="${pref === 'avoid' ? 'on-avoid' : ''}">${escH(tUi('avoid'))}</button>
          </div>
        </div>`;
      }).join('');
      grid.querySelectorAll('[data-style-card]').forEach((card) => {
        const fam = card.dataset.styleCard;
        card.querySelectorAll('button[data-sp]').forEach((btn) => btn.addEventListener('click', () => {
          const sp = btn.dataset.sp;
          if (!currentProfile.stylePreferences) currentProfile.stylePreferences = {};
          if (sp === 'neutral') delete currentProfile.stylePreferences[fam];
          else currentProfile.stylePreferences[fam] = sp;
          persistAll();
          renderStylesAndFeatures();
          renderDealBreakers();
        }));
      });
    }

    const fGrid = document.getElementById('featureWishesGrid');
    if (fGrid) {
      const wished = new Set(currentProfile.featureWishes || []);
      fGrid.innerHTML = (activeCatalog.features || []).map((f) =>
        `<button type="button" class="featchip ${wished.has(f.key) ? 'on' : ''}" data-feat="${escH(f.key)}">${wished.has(f.key) ? '✓ ' : ''}${escH(f.label)}</button>`
      ).join('');
      fGrid.querySelectorAll('[data-feat]').forEach((btn) => btn.addEventListener('click', () => {
        const k = btn.dataset.feat;
        const set = new Set(currentProfile.featureWishes || []);
        if (set.has(k)) set.delete(k); else set.add(k);
        currentProfile.featureWishes = [...set];
        persistAll();
        renderStylesAndFeatures();
      }));
    }
  }

  function renderPlacesList() {
    const box = document.getElementById('placesList');
    if (!box) return;
    const tb = document.getElementById('qTrafficBuffer');
    const tbVal = document.getElementById('qTrafficBufVal');
    if (tb) tb.value = String(currentProfile.trafficBuffer ?? 1.3);
    if (tbVal) tbVal.textContent = `${Number(currentProfile.trafficBuffer ?? 1.3).toFixed(2)}×`;
    const list = currentProfile.places || [];
    box.innerHTML = list.length
      ? list.map((p, i) => `<div class="placeitem">
          <div><b>${escH(p.label)}</b> · <span style="color:#0f766e">${escH(p.mode || 'drive')} ≤ ${escH(p.maxMinutes)} min</span> · ${escH(p.tripsPerWeek)} trips/wk <span style="font-size:11.5px;color:#6b7280">(${Number(p.lat).toFixed(4)}, ${Number(p.lon).toFixed(4)})</span></div>
          <button type="button" class="btnmut" data-rm-place="${i}" style="padding:3px 9px">×</button>
        </div>`).join('')
      : `<div style="font-size:12.5px;color:#6b7280">${escH(tr('No places added yet (add up to 3 commute/family destinations above).', '暂未添加常去地点（可添加最多 3 个通勤/常去地址）', 'Aún no hay lugares agregados (añada hasta 3 destinos de trabajo o familia).'))}</div>`;
    box.querySelectorAll('[data-rm-place]').forEach((btn) => btn.addEventListener('click', () => {
      const i = Number(btn.dataset.rmPlace);
      currentProfile.places = (currentProfile.places || []).filter((_, j) => j !== i);
      persistAll();
      renderPlacesList();
    }));
    updateStudioSummary();
  }

  function renderMaxDiffAndSliders() {
    const mdBox = document.getElementById('maxDiffBox');
    const dimMap = Object.fromEntries((activeCatalog.dimensions || []).map((d) => [d.key, d]));
    if (mdBox) {
      mdBox.innerHTML = MAXDIFF_ROUNDS.map((rd, idx) => {
        const pick = maxDiffPicks[rd.id] || {};
        return `<div class="mdcard"><div style="font-size:11.5px;font-weight:800;color:#0f766e;margin-bottom:4px">${escH(tr(`Trade-off #${idx + 1}`, `权衡 #${idx + 1}`, `Prioridad #${idx + 1}`))}</div>` +
          rd.dims.map((dk) => {
            const lbl = dimMap[dk] ? dimMap[dk].label : dk;
            return `<div class="mdopt"><span>${escH(lbl)}</span><span style="display:flex;gap:4px">
              <button type="button" data-md="${rd.id}" data-kind="most" data-dim="${dk}" class="${pick.most === dk ? 'most' : ''}">${escH(tUi('mostBtn'))}</button>
              <button type="button" data-md="${rd.id}" data-kind="least" data-dim="${dk}" class="${pick.least === dk ? 'least' : ''}">${escH(tUi('leastBtn'))}</button>
            </span></div>`;
          }).join('') + '</div>';
      }).join('');
      mdBox.querySelectorAll('button[data-md]').forEach((btn) => btn.addEventListener('click', () => {
        const { md, kind, dim } = btn.dataset;
        const prev = maxDiffPicks[md] || {};
        if (kind === 'most') {
          if (prev.most !== dim) {
            currentProfile.weights[dim] = Math.min(100, (currentProfile.weights[dim] || 0) + 8);
            if (prev.least === dim) delete prev.least;
            prev.most = dim;
          }
        } else {
          if (prev.least !== dim) {
            currentProfile.weights[dim] = Math.max(0, (currentProfile.weights[dim] || 0) - 4);
            if (prev.most === dim) delete prev.most;
            prev.least = dim;
          }
        }
        maxDiffPicks[md] = prev;
        currentProfile.preset = undefined;
        persistAll();
        renderPresets();
        renderMaxDiffAndSliders();
        renderStyleSliderValue();
        if (typeof previewReRankClientSide === 'function') previewReRankClientSide();
      }));
    }
    renderSliders();
  }

  function renderSliders() {
    const grid = document.getElementById('dimSlidersGrid');
    if (!grid) return;
    const { sum, shares } = computeShares(currentProfile.weights);
    const sumBadge = document.getElementById('sliderSumBadge');
    const shareTotal = Object.values(shares).reduce((a, b) => a + b, 0) * 100;
    if (sumBadge) {
      sumBadge.textContent = tr(
        `Raw sum: ${Math.round(sum)} · Share: ${shareTotal.toFixed(1)}%`,
        `原始合计: ${Math.round(sum)} · 占比总和: ${shareTotal.toFixed(1)}%`,
        `Suma: ${Math.round(sum)} · Total: ${shareTotal.toFixed(1)}%`
      );
    }
    const critSet = new Set(currentProfile.critical || []);
    grid.innerHTML = (activeCatalog.dimensions || []).map((d) => {
      const val = Math.round(currentProfile.weights[d.key] || 0);
      const pct = (shares[d.key] * 100).toFixed(1);
      return `<div class="dimrow" data-dim-row="${escH(d.key)}">
        <div class="dimtop"><span>${escH(d.label)}</span><span class="dimshare" id="dsh_${escH(d.key)}">${val} · ${pct}%</span></div>
        <input type="range" min="0" max="100" step="1" value="${val}" data-dim-slider="${escH(d.key)}">
        <div style="display:flex;justify-content:space-between;align-items:center;gap:6px">
          <span style="font-size:11px;color:#6b7280">${escH(d.description || '')}</span>
          <label style="font-size:11px;white-space:nowrap;cursor:pointer;color:#374151"><input type="checkbox" data-dim-crit="${escH(d.key)}" ${critSet.has(d.key) ? 'checked' : ''}> ${escH(tUi('criticalLbl'))}</label>
        </div>
      </div>`;
    }).join('');
    grid.querySelectorAll('[data-dim-slider]').forEach((inp) => inp.addEventListener('input', () => {
      const k = inp.dataset.dimSlider;
      currentProfile.weights[k] = Number(inp.value);
      currentProfile.preset = undefined;
      const { sum: s2, shares: sh2 } = computeShares(currentProfile.weights);
      for (const dim of VALID_DIMS) {
        const badge = document.getElementById(`dsh_${dim}`);
        if (badge) badge.textContent = `${Math.round(currentProfile.weights[dim] || 0)} · ${(sh2[dim] * 100).toFixed(1)}%`;
      }
      if (sumBadge) {
        const tot = Object.values(sh2).reduce((a, b) => a + b, 0) * 100;
        sumBadge.textContent = tr(
          `Raw sum: ${Math.round(s2)} · Share: ${tot.toFixed(1)}%`,
          `原始合计: ${Math.round(s2)} · 占比总和: ${tot.toFixed(1)}%`,
          `Suma: ${Math.round(s2)} · Total: ${tot.toFixed(1)}%`
        );
      }
      renderStyleSliderValue();
      persistAll();
      if (typeof previewReRankClientSide === 'function') previewReRankClientSide();
    }));
    grid.querySelectorAll('[data-dim-crit]').forEach((cb) => cb.addEventListener('change', () => {
      const k = cb.dataset.dimCrit;
      const s = new Set(currentProfile.critical || []);
      if (cb.checked && (currentProfile.weights[k] || 0) > 0) s.add(k); else s.delete(k);
      currentProfile.critical = [...s];
      persistAll();
    }));
  }

  function renderSchoolsAndDealBreakers() {
    const schBox = document.getElementById('schoolLevelsBox');
    if (schBox) {
      const sel = new Set(currentProfile.schoolLevels || []);
      schBox.innerHTML = (activeCatalog.schoolLevels || []).map((lv) =>
        `<label><input type="checkbox" data-sch-lvl="${escH(lv.key)}" ${sel.has(lv.key) ? 'checked' : ''}> ${escH(lv.label)}</label>`
      ).join('');
      schBox.querySelectorAll('[data-sch-lvl]').forEach((cb) => cb.addEventListener('change', () => {
        const arr = [];
        schBox.querySelectorAll('[data-sch-lvl]:checked').forEach((x) => arr.push(x.dataset.schLvl));
        currentProfile.schoolLevels = arr;
        persistAll();
      }));
    }

    const dbGrid = document.getElementById('dealBreakersGrid');
    if (dbGrid) {
      const dbs = new Set(currentProfile.dealBreakers || []);
      const avoidDigits = new Set(currentProfile.avoidDigits || []);
      dbGrid.innerHTML = (activeCatalog.screens || []).map((sc) => {
        const on = dbs.has(sc.key);
        let extra = '';
        if (sc.key === 'hoa_over' && on) {
          extra = `<div style="margin-top:8px;display:flex;align-items:center;gap:6px"><span style="font-size:12px">${escH(tUi('hoaLimitLbl'))}</span><input id="qHoaMax" class="tinput" style="width:95px;padding:4px 8px;font-size:12.5px" type="number" min="0" max="50000" value="${escH(currentProfile.hoaMaxMonthly ?? 500)}"></div>`;
        } else if (sc.key === 'address_digits' && on) {
          extra = `<div style="margin-top:8px"><div style="font-size:12px;margin-bottom:4px">${escH(tUi('avoidDigitsLbl'))}</div><div style="display:flex;flex-wrap:wrap;gap:4px">${[0,1,2,3,4,5,6,7,8,9].map((d) => `<button type="button" class="digitbtn ${avoidDigits.has(d) ? 'on' : ''}" data-digit="${d}">${d}</button>`).join('')}</div></div>`;
        }
        return `<div class="dbcard ${on ? 'on' : ''}">
          <label style="display:flex;align-items:flex-start;gap:8px;cursor:pointer;font-weight:700"><input type="checkbox" data-db-screen="${escH(sc.key)}" ${on ? 'checked' : ''}> <span>${escH(sc.label)}</span></label>${extra}
        </div>`;
      }).join('');
      dbGrid.querySelectorAll('[data-db-screen]').forEach((cb) => cb.addEventListener('change', () => {
        const k = cb.dataset.dbScreen;
        const set = new Set(currentProfile.dealBreakers || []);
        if (cb.checked) {
          if (k === 'style_avoid' && !Object.values(currentProfile.stylePreferences || {}).includes('avoid')) {
            const stMsg = document.getElementById('profileSaveStatus');
            if (stMsg) { stMsg.style.color = '#b45309'; stMsg.textContent = tr('Mark at least one style as "Avoid" in Step 3 first.', '请先在步骤 3 中将至少一种风格标为“避免”', 'Marque al menos un estilo como "Evitar" en el Paso 3.'); }
            cb.checked = false;
            return;
          }
          set.add(k);
          if (k === 'hoa_over' && currentProfile.hoaMaxMonthly == null) currentProfile.hoaMaxMonthly = 500;
          if (k === 'address_digits' && (!currentProfile.avoidDigits || !currentProfile.avoidDigits.length)) currentProfile.avoidDigits = [4];
        } else {
          set.delete(k);
        }
        currentProfile.dealBreakers = [...set];
        persistAll();
        renderSchoolsAndDealBreakers();
      }));
      const hoaIn = document.getElementById('qHoaMax');
      if (hoaIn) hoaIn.addEventListener('input', () => {
        const n = Number(hoaIn.value);
        if (Number.isFinite(n) && n >= 0) { currentProfile.hoaMaxMonthly = n; persistAll(); }
      });
      dbGrid.querySelectorAll('[data-digit]').forEach((btn) => btn.addEventListener('click', () => {
        const d = Number(btn.dataset.digit);
        const s = new Set(currentProfile.avoidDigits || []);
        if (s.has(d)) { if (s.size > 1) s.delete(d); } else s.add(d);
        currentProfile.avoidDigits = [...s].sort((a, b) => a - b);
        persistAll();
        renderSchoolsAndDealBreakers();
      }));
    }
  }

  function renderAllStudio() {
    applyStaticUiLabels();
    renderQuizStepsVisibility();
    renderPresets();
    renderSelectedAreas();
    renderBudgetAndNeeds();
    renderStylesAndFeatures();
    renderPlacesList();
    renderMaxDiffAndSliders();
    renderSchoolsAndDealBreakers();
    updateStudioSummary();
  }

  async function switchUiLang(nextLang) {
    const lang = nextLang === 'zh-CN' || nextLang === 'zh' ? 'zh-CN' : (nextLang === 'es' ? 'es' : 'en');
    uiState = profileStore.saveUi({ lang });
    await fetchCatalogForLang(lang);
    renderAllStudio();
    if (typeof applyShopStaticLabels === 'function') applyShopStaticLabels();
    if (typeof renderGeoLocationBars === 'function') renderGeoLocationBars();
    if (lastShopResponse && typeof renderShopResultsView === 'function') renderShopResultsView(lastShopResponse);
    if (lastAreaRankResult && typeof renderExplorer === 'function') renderExplorer();
    if (typeof renderSavedHomes === 'function') renderSavedHomes();
  }

  function initMatchStudio() {
    if (studioInitialized) return;
    studioInitialized = true;

    const urlLang = new URLSearchParams(location.search).get('lang');
    if (urlLang && (urlLang.startsWith('zh') || urlLang.startsWith('es') || urlLang === 'en')) {
      uiState = profileStore.saveUi({ lang: urlLang.startsWith('zh') ? 'zh-CN' : urlLang.startsWith('es') ? 'es' : 'en' });
    } else if (CUR_LANG === 'zh-CN' && uiState.lang !== 'zh-CN') {
      uiState = profileStore.saveUi({ lang: 'zh-CN' });
    }
    fetchCatalogForLang(uiState.lang).then(renderAllStudio);
    renderAllStudio();
    profileStore.syncFromAccount().then((synced) => {
      if (synced) { currentProfile = synced; renderAllStudio(); }
    });

    document.querySelectorAll('#quizStepNav .stepbtn').forEach((btn) => btn.addEventListener('click', () => {
      uiState = profileStore.saveUi({ step: Number(btn.dataset.step), showAllSteps: false });
      renderAllStudio();
    }));
    document.getElementById('toggleAllStepsBtn')?.addEventListener('click', () => {
      uiState = profileStore.saveUi({ showAllSteps: !uiState.showAllSteps });
      applyStaticUiLabels();
      renderQuizStepsVisibility();
    });
    document.getElementById('prevStepBtn')?.addEventListener('click', () => {
      uiState = profileStore.saveUi({ step: Math.max(1, uiState.step - 1) });
      renderQuizStepsVisibility();
    });
    document.getElementById('nextStepBtn')?.addEventListener('click', () => {
      uiState = profileStore.saveUi({ step: Math.min(6, uiState.step + 1) });
      renderQuizStepsVisibility();
    });

    // Step 1: Area autocomplete via GET /api/match/areas?q=...
    const areaInput = document.getElementById('areaSearchInput');
    const areaDrop = document.getElementById('areaDropdown');
    let areaTimer = null;
    async function loadAreaSuggestions(q) {
      if (!areaDrop) return;
      try {
        const res = await fetch('/api/match/areas?q=' + encodeURIComponent(q || ''));
        if (!res.ok) return;
        const j = await res.json();
        const zips = j.zips || [];
        const cities = j.cities || [];
        if (!zips.length && !cities.length) {
          areaDrop.innerHTML = `<div class="areaitem" style="color:#6b7280">${escH(tr('No indexed areas matched — type any 5-digit US ZIP and press Enter.', '暂无匹配区域 — 可直接输入 5 位邮编后回车添加', 'Sin zonas indexadas — escriba un ZIP de 5 dígitos y presione Enter.'))}</div>`;
          areaDrop.classList.add('open');
          return;
        }
        const zHtml = zips.map((z) => `<div class="areaitem" data-pick-zip="${escH(z.zip)}"><span><b>ZIP ${escH(z.zip)}</b>${z.city ? ` · ${escH(z.city)}${z.state ? ', ' + escH(z.state) : ''}` : ''}</span><span class="mbadge mb-score">${z.count}</span></div>`).join('');
        const cHtml = cities.map((c) => `<div class="areaitem" data-pick-city="${escH(c.city)}" data-pick-state="${escH(c.state || '')}" data-pick-label="${escH(c.label || c.city)}"><span><b>${escH(c.label || c.city)}</b></span><span class="mbadge mb-sch">${c.count}</span></div>`).join('');
        areaDrop.innerHTML = zHtml + cHtml;
        areaDrop.classList.add('open');
        areaDrop.querySelectorAll('[data-pick-zip]').forEach((el) => el.addEventListener('mousedown', (ev) => {
          ev.preventDefault();
          addZipChip(el.dataset.pickZip);
          if (areaInput) areaInput.value = '';
          areaDrop.classList.remove('open');
        }));
        areaDrop.querySelectorAll('[data-pick-city]').forEach((el) => el.addEventListener('mousedown', (ev) => {
          ev.preventDefault();
          addCityChip({ city: el.dataset.pickCity, state: el.dataset.pickState, label: el.dataset.pickLabel });
          if (areaInput) areaInput.value = '';
          areaDrop.classList.remove('open');
        }));
      } catch {}
    }
    areaInput?.addEventListener('focus', () => loadAreaSuggestions(areaInput.value));
    areaInput?.addEventListener('input', () => {
      clearTimeout(areaTimer);
      areaTimer = setTimeout(() => loadAreaSuggestions(areaInput.value), 140);
    });
    areaInput?.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        const m = (areaInput.value || '').match(/\b(\d{5})\b/);
        if (m) { addZipChip(m[1]); areaInput.value = ''; areaDrop?.classList.remove('open'); }
      } else if (e.key === 'Escape') areaDrop?.classList.remove('open');
    });
    document.addEventListener('mousedown', (e) => { if (!e.target.closest('.areabox')) areaDrop?.classList.remove('open'); });

    // Step 2: Budget & Needs bindings
    const bindNumBudget = (id, key) => document.getElementById(id)?.addEventListener('input', (e) => {
      const n = Number(e.target.value);
      if (!currentProfile.budget) currentProfile.budget = {};
      if (Number.isFinite(n) && n > 0) currentProfile.budget[key] = n; else delete currentProfile.budget[key];
      persistAll();
    });
    bindNumBudget('qMonthlyBudget', 'monthly');
    bindNumBudget('qMaxPrice', 'maxPrice');
    document.getElementById('qDownPct')?.addEventListener('input', (e) => {
      const n = Number(e.target.value);
      if (Number.isFinite(n)) { currentProfile.downPaymentPct = Math.max(0, Math.min(100, n)); persistAll(); }
    });
    document.getElementById('qLoanProgram')?.addEventListener('change', (e) => {
      const prog = e.target.value;
      uiState = profileStore.saveUi({ loanProgram: prog });
      currentProfile.loanType = loanProgramToScoringType(prog);
      persistAll();
    });
    const bindOptField = (id, key) => document.getElementById(id)?.addEventListener('input', (e) => {
      const raw = e.target.value;
      const n = Number(raw);
      if (raw !== '' && Number.isFinite(n) && n >= 0) currentProfile[key] = n; else delete currentProfile[key];
      persistAll();
    });
    bindOptField('qMinBeds', 'minBeds');
    bindOptField('qMinBaths', 'minBaths');
    bindOptField('qMinSqft', 'minSqft');
    bindOptField('qMinLotSqft', 'minLotSqft');
    document.getElementById('qMultigen')?.addEventListener('change', (e) => { currentProfile.multigenerational = e.target.checked; persistAll(); });
    document.getElementById('qAduNeeded')?.addEventListener('change', (e) => { uiState = profileStore.saveUi({ aduNeeded: e.target.checked }); });

    // Step 3: Style slider + Era
    document.getElementById('qStyleSlider')?.addEventListener('input', (e) => {
      currentProfile.weights.style = Number(e.target.value);
      currentProfile.preset = undefined;
      persistAll();
      renderPresets();
      renderStyleSliderValue();
      renderSliders();
      if (typeof previewReRankClientSide === 'function') previewReRankClientSide();
    });
    document.getElementById('qEraPref')?.addEventListener('change', (e) => {
      uiState = profileStore.saveUi({ eraPreference: e.target.value });
    });

    // Step 4: Important Places (/api/suggest + /api/match/geocode)
    const pAddr = document.getElementById('placeAddrInput');
    const pAc = document.getElementById('placeAcBox');
    const pLat = document.getElementById('placeLatInput');
    const pLon = document.getElementById('placeLonInput');
    const pGeoSt = document.getElementById('placeGeoStatus');
    let pTimer = null, pCtrl = null;
    pAddr?.addEventListener('input', () => {
      const q = pAddr.value.trim();
      clearTimeout(pTimer);
      if (pCtrl) pCtrl.abort();
      if (q.length < 3) { pAc?.classList.remove('open'); return; }
      pTimer = setTimeout(async () => {
        pCtrl = new AbortController();
        try {
          const r = await fetch('/api/suggest?q=' + encodeURIComponent(q) + buildSuggestGeoBiasQs(), { signal: pCtrl.signal }).then((x) => x.json());
          const suggs = r.suggestions || [];
          if (!suggs.length || !pAc) { pAc?.classList.remove('open'); return; }
          pAc.innerHTML = suggs.map((s) => `<div class="aci" data-full="${escH(s.full)}"><div class="txt"><div class="pri">${escH(s.primary)}</div><div class="sec">${escH(s.secondary || '')}</div></div></div>`).join('');
          pAc.classList.add('open');
          pAc.querySelectorAll('.aci').forEach((el) => el.addEventListener('mousedown', async (ev) => {
            ev.preventDefault();
            const full = el.dataset.full;
            pAddr.value = full;
            pAc.classList.remove('open');
            if (pGeoSt) pGeoSt.textContent = tr('Locating coordinates…', '正在定位坐标…', 'Localizando coordenadas…');
            try {
              const g = await fetch('/api/match/geocode?q=' + encodeURIComponent(full)).then((x) => x.json());
              if (g && g.ok && g.matched) {
                if (pLat) pLat.value = String(g.lat);
                if (pLon) pLon.value = String(g.lon);
                if (pGeoSt) pGeoSt.textContent = `✓ (${Number(g.lat).toFixed(4)}, ${Number(g.lon).toFixed(4)})`;
              } else if (pGeoSt) {
                pGeoSt.textContent = tr('Could not geocode — enter Lat/Lon manually.', '未定位，请手动填写经纬度', 'No se pudo geocodificar — ingrese Lat/Lon.');
              }
            } catch {
              if (pGeoSt) pGeoSt.textContent = tr('Offline — enter Lat/Lon manually.', '离线模式 — 请手动输入经纬度', 'Sin conexión — ingrese Lat/Lon.');
            }
          }));
        } catch {}
      }, 150);
    });
    document.addEventListener('mousedown', (e) => { if (!e.target.closest('#quizStep4')) pAc?.classList.remove('open'); });
    document.getElementById('qTrafficBuffer')?.addEventListener('input', (e) => {
      currentProfile.trafficBuffer = Number(e.target.value);
      persistAll();
      const tbVal = document.getElementById('qTrafficBufVal');
      if (tbVal) tbVal.textContent = `${currentProfile.trafficBuffer.toFixed(2)}×`;
    });
    document.getElementById('addPlaceBtn')?.addEventListener('click', async () => {
      if ((currentProfile.places || []).length >= 3) {
        if (pGeoSt) pGeoSt.textContent = tr('Maximum 3 places allowed.', '最多添加 3 个常去地点', 'Máximo 3 lugares permitidos.');
        return;
      }
      const label = (document.getElementById('placeLabelInput')?.value || '').trim() || (pAddr?.value || '').split(',')[0].trim() || 'Place';
      let lat = Number(pLat?.value);
      let lon = Number(pLon?.value);
      if ((!Number.isFinite(lat) || !Number.isFinite(lon)) && pAddr?.value?.trim().length >= 5) {
        try {
          const g = await fetch('/api/match/geocode?q=' + encodeURIComponent(pAddr.value.trim())).then((x) => x.json());
          if (g && g.ok && g.matched) { lat = Number(g.lat); lon = Number(g.lon); }
        } catch {}
      }
      if (!Number.isFinite(lat) || !Number.isFinite(lon) || (lat === 0 && lon === 0)) {
        if (pGeoSt) pGeoSt.textContent = tr('Select an address suggestion or enter valid Lat/Lon.', '请先从地址联想选择或输入有效的经纬度', 'Seleccione una sugerencia o ingrese Lat/Lon válidos.');
        return;
      }
      const maxMinutes = Math.max(5, Math.min(240, Number(document.getElementById('placeMaxMinInput')?.value) || 30));
      const tripsPerWeek = Math.max(1, Math.min(50, Number(document.getElementById('placeTripsInput')?.value) || 5));
      currentProfile.places = [...(currentProfile.places || []), { label: label.slice(0, 80), lat, lon, maxMinutes, tripsPerWeek, mode: 'drive' }];
      persistAll();
      renderPlacesList();
      if (document.getElementById('placeLabelInput')) document.getElementById('placeLabelInput').value = '';
      if (pAddr) pAddr.value = '';
      if (pLat) pLat.value = '';
      if (pLon) pLon.value = '';
      if (pGeoSt) pGeoSt.textContent = '';
    });

    // Step 5: Normalize weights button
    document.getElementById('normalizeWeightsBtn')?.addEventListener('click', () => {
      const { sum } = computeShares(currentProfile.weights);
      if (sum > 0) {
        for (const d of VALID_DIMS) {
          currentProfile.weights[d] = Math.round(((currentProfile.weights[d] || 0) / sum) * 100);
        }
        const newSum = VALID_DIMS.reduce((a, d) => a + currentProfile.weights[d], 0);
        if (newSum !== 100 && newSum > 0) {
          const top = VALID_DIMS.reduce((a, b) => (currentProfile.weights[b] > currentProfile.weights[a] ? b : a), VALID_DIMS[0]);
          currentProfile.weights[top] = Math.max(0, currentProfile.weights[top] + (100 - newSum));
        }
      }
      persistAll();
      renderSliders();
      renderStyleSliderValue();
      if (typeof previewReRankClientSide === 'function') previewReRankClientSide();
    });

    // Footer actions: Save & Validate via POST /api/match/profile, Reset, Export/Import
    document.getElementById('saveProfileBtn')?.addEventListener('click', async () => {
      const st = document.getElementById('profileSaveStatus');
      currentProfile = profileStore.save(currentProfile);
      uiState = profileStore.saveUi(uiState);
      if (st) { st.style.color = '#0f766e'; st.textContent = tr('Validating…', '正在验证…', 'Validando…'); }
      try {
        const res = await fetch('/api/match/profile', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(currentProfile),
        });
        const j = await res.json();
        if (!res.ok || !j.ok) {
          if (st) { st.style.color = '#991b1b'; st.textContent = j.error || 'Validation failed'; }
          return;
        }
        currentProfile = profileStore.save(j.profile);
        profileStore.syncToAccount(currentProfile);
        if (st) {
          st.style.color = '#065f46';
          st.textContent = tr('✓ Validated & saved to browser', '✓ 已验证并保存到浏览器', '✓ Validado y guardado en navegador');
        }
      } catch {
        if (st) { st.style.color = '#065f46'; st.textContent = tr('✓ Saved locally', '✓ 已保存在本地', '✓ Guardado localmente'); }
      }
    });

    document.getElementById('resetProfileBtn')?.addEventListener('click', () => {
      currentProfile = profileStore.reset();
      uiState = profileStore.saveUi({ areas: { zips: [], cities: [] }, loanProgram: '30yr_fixed', eraPreference: 'any', aduNeeded: false });
      renderAllStudio();
      const st = document.getElementById('profileSaveStatus');
      if (st) { st.style.color = '#0f766e'; st.textContent = tr('Reset to Balanced defaults', '已恢复均衡默认', 'Restablecido a Equilibrado'); }
    });

    document.getElementById('toggleExportBtn')?.addEventListener('click', () => {
      const box = document.getElementById('profileExportBox');
      const area = document.getElementById('profileJsonArea');
      if (!box || !area) return;
      const open = box.style.display === 'none';
      box.style.display = open ? 'block' : 'none';
      if (open) area.value = JSON.stringify(currentProfile, null, 2);
    });
    document.getElementById('copyProfileJsonBtn')?.addEventListener('click', () => {
      const area = document.getElementById('profileJsonArea');
      if (area) navigator.clipboard?.writeText(area.value);
    });
    document.getElementById('importProfileJsonBtn')?.addEventListener('click', () => {
      const area = document.getElementById('profileJsonArea');
      const st = document.getElementById('profileSaveStatus');
      try {
        const parsed = JSON.parse(area.value);
        currentProfile = profileStore.save(parsed);
        renderAllStudio();
        if (st) { st.style.color = '#065f46'; st.textContent = tr('✓ Imported profile JSON', '✓ 已导入 JSON 配置', '✓ JSON importado'); }
      } catch (e) {
        if (st) { st.style.color = '#991b1b'; st.textContent = 'Invalid JSON: ' + e.message; }
      }
    });

    // ---- Area Explorer: POST /api/match/rank + GET /api/match/rank/:id + List/Map/Compare/Stars ----
    function stopRankPoll() {
      if (rankPollTimer) { clearTimeout(rankPollTimer); rankPollTimer = null; }
    }
    function bandMeta(bandKey, score) {
      const k = bandKey || (score >= 75 ? 'strong' : score >= 62 ? 'good' : score >= 50 ? 'fair' : 'weak');
      if (k === 'strong') return { cls: 'mb-strong', color: '#0f766e', label: tr('Strong match', '高度匹配', 'Gran coincidencia') };
      if (k === 'good') return { cls: 'mb-good', color: '#2563eb', label: tr('Good match', '较好匹配', 'Buena coincidencia') };
      if (k === 'fair') return { cls: 'mb-fair', color: '#d97706', label: tr('Fair match', '一般匹配', 'Coincidencia regular') };
      return { cls: 'mb-weak', color: '#dc2626', label: tr('Weak match', '匹配度低', 'Baja coincidencia') };
    }
    function fmtPrice(v) { return (typeof v === 'number' && isFinite(v)) ? ('$' + Math.round(v).toLocaleString('en-US')) : tr('Est. pending', '待估价', 'Por estimar'); }
    function setExplorerView(nextView) {
      explorerView = nextView;
      document.querySelectorAll('#explorerViewTabs button').forEach((b) => b.classList.toggle('act', b.dataset.expView === explorerView));
      const lr = document.getElementById('explorerResults');
      const mw = document.getElementById('expMapWrap');
      const cw = document.getElementById('expCompareWrap');
      if (lr) lr.style.display = explorerView === 'list' ? 'block' : 'none';
      if (mw) mw.style.display = explorerView === 'map' ? 'block' : 'none';
      if (cw) cw.style.display = explorerView === 'compare' ? 'block' : 'none';
      if (explorerView === 'map') renderExplorerMap();
      if (explorerView === 'compare') renderCompareView();
    }
    document.querySelectorAll('#explorerViewTabs button').forEach((b) => {
      b.addEventListener('click', () => setExplorerView(b.dataset.expView));
    });

    function previewReRankClientSide() {
      if (!lastAreaRankResult || !lastAreaRankResult.ranked) return;
      const t0 = performance.now();
      const rescoreList = (arr, isMain) => {
        if (!Array.isArray(arr)) return [];
        const updated = arr.map((h) => {
          const dims = h.dimensions || [];
          let wSum = 0;
          for (const d of dims) wSum += Math.max(0, Number(currentProfile.weights?.[d.dimension]) || 0);
          if (wSum <= 0) return h;
          let sumPts = 0;
          const nextDims = dims.map((d) => {
            const w = Math.max(0, Number(currentProfile.weights?.[d.dimension]) || 0);
            const share = w / wSum;
            const points = share * (d.utility || 0);
            sumPts += points;
            return Object.assign({}, d, { share, points });
          });
          let score = sumPts;
          if (Array.isArray(h.cappedBy) && h.cappedBy.length && score > 59) score = 59;
          const bm = bandMeta(null, score);
          return Object.assign({}, h, {
            score, matchScore: score,
            band: score >= 75 ? 'strong' : score >= 62 ? 'good' : score >= 50 ? 'fair' : 'weak',
            bandLabel: bm.label, dimensions: nextDims,
          });
        });
        updated.sort((a, b) => ((b.score || 0) - (a.score || 0)) || String(a.id).localeCompare(String(b.id)));
        if (isMain) {
          for (let i = 0; i < updated.length; i++) {
            updated[i].rank = i + 1;
            const prevDiff = i > 0 ? Math.abs((updated[i - 1].score || 0) - (updated[i].score || 0)) : 99;
            const nextDiff = i + 1 < updated.length ? Math.abs((updated[i].score || 0) - (updated[i + 1].score || 0)) : 99;
            updated[i].tied = prevDiff <= 1.0 || nextDiff <= 1.0;
          }
        }
        return updated;
      };
      lastAreaRankResult.ranked.ranked = rescoreList(lastAreaRankResult.ranked.ranked, true);
      lastAreaRankResult.ranked.nearMisses = rescoreList(lastAreaRankResult.ranked.nearMisses, false);
      explorerNeedsServerResync = true;
      renderExplorer();
      void (performance.now() - t0);
    }
    window.previewReRankClientSide = previewReRankClientSide;

    function toggleCompareId(id) {
      const idx = compareIds.indexOf(id);
      if (idx >= 0) compareIds.splice(idx, 1);
      else {
        if (compareIds.length >= 4) compareIds.shift();
        compareIds.push(id);
      }
      const badge = document.getElementById('compareCountBadge');
      if (badge) badge.textContent = String(compareIds.length);
      renderCompareTray();
      renderExplorerList();
      if (explorerView === 'compare') renderCompareView();
    }

    function allScoredFromResult(res) {
      const r = res?.ranked || {};
      return [...(r.ranked || []), ...(r.nearMisses || []), ...(r.flagged || [])];
    }

    function whyOneBeatsTwo(h1, h2) {
      if (!h1 || !h2) return '';
      const diffs = [];
      const map2 = Object.fromEntries((h2.dimensions || []).map((d) => [d.dimension, d]));
      const dimLabels = Object.fromEntries((activeCatalog.dimensions || []).map((d) => [d.key, d.label]));
      for (const d1 of (h1.dimensions || [])) {
        const d2 = map2[d1.dimension];
        if (!d2) continue;
        const deltaPts = (d1.points || 0) - (d2.points || 0);
        const deltaUtil = (d1.utility || 0) - (d2.utility || 0);
        diffs.push({ dim: d1.dimension, label: dimLabels[d1.dimension] || d1.dimension, deltaPts, deltaUtil, u1: d1.utility, u2: d2.utility });
      }
      diffs.sort((a, b) => Math.abs(b.deltaPts) - Math.abs(a.deltaPts));
      const wins1 = diffs.filter((x) => x.deltaPts > 0.3).slice(0, 3);
      const wins2 = diffs.filter((x) => x.deltaPts < -0.3).slice(0, 2);
      const gap = ((h1.matchScore ?? h1.score ?? 0) - (h2.matchScore ?? h2.score ?? 0)).toFixed(1);
      const wTxt = wins1.map((x) => `${escH(x.label)} (+${x.deltaPts.toFixed(1)} pts · ${Math.round(x.u1)} vs ${Math.round(x.u2)})`).join(' · ');
      const cTxt = wins2.map((x) => `${escH(x.label)} (${x.deltaPts.toFixed(1)} pts)`).join(' · ');
      return `<div style="font-size:13px;line-height:1.5"><b>${escH(tr('Why #1 beats #2 for you', '为什么 #1 比 #2 更适合您', 'Por qué el n.º 1 supera al n.º 2 para usted'))}</b> (+${gap} pts): ${wTxt || escH(tr('Closely tied across weighted criteria.', '各项加权维度得分非常接近。', 'Muy parejos en sus prioridades ponderadas.'))}${cTxt ? `<div style="margin-top:3px;color:#4b5563">${escH(tr('#2 counters on:', '#2 的相对优势维度：', 'El n.º 2 destaca en:'))} ${cTxt}</div>` : ''}</div>`;
    }

    function renderCompareTray() {
      const bar = document.getElementById('compareTrayBar');
      if (!bar) return;
      const badge = document.getElementById('compareCountBadge');
      if (badge) badge.textContent = String(compareIds.length);
      if (!compareIds.length) { bar.innerHTML = ''; return; }
      const listings = lastAreaRankResult?.listings || {};
      const chips = compareIds.map((id) => {
        const ad = (listings[id]?.address || id).split(',')[0];
        return `<span class="selchip city">${escH(ad)} <button type="button" data-rm-cmp="${escH(id)}">×</button></span>`;
      }).join('');
      bar.innerHTML = `<div class="cmptray" style="margin-top:10px;padding:10px 12px;display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap">
        <div style="display:flex;gap:6px;flex-wrap:wrap;align-items:center"><b style="font-size:12.5px">${escH(tr('Compare tray (2–4):', '对比栏 (2–4 套)：', 'Comparación (2–4):'))}</b>${chips}</div>
        <div style="display:flex;gap:6px">
          <button type="button" class="btnmut" id="clearCompareBtn">${escH(tr('Clear', '清空', 'Limpiar'))}</button>
          <button type="button" class="btnsec" id="openCompareViewBtn">${escH(tr('Open Side-by-Side', '打开并排对比', 'Ver lado a lado'))} (${compareIds.length})</button>
        </div>
      </div>`;
      bar.querySelectorAll('[data-rm-cmp]').forEach((b) => b.addEventListener('click', () => toggleCompareId(b.dataset.rmCmp)));
      bar.querySelector('#clearCompareBtn')?.addEventListener('click', () => { compareIds.length = 0; renderCompareTray(); renderExplorerList(); if (explorerView === 'compare') renderCompareView(); });
      bar.querySelector('#openCompareViewBtn')?.addEventListener('click', () => setExplorerView('compare'));
    }

    function renderCompareView() {
      const wrap = document.getElementById('expCompareWrap');
      if (!wrap) return;
      const all = allScoredFromResult(lastAreaRankResult);
      const listings = lastAreaRankResult?.listings || {};
      const picked = compareIds.map((id) => all.find((h) => h.id === id)).filter(Boolean);
      if (picked.length < 2) {
        wrap.innerHTML = `<div class="mbanner">${escH(tr('Select 2 to 4 homes using "Compare" buttons in the list or map to compare them side-by-side.', '请在列表或地图中点击“对比”选择 2–4 套房源进行并排分析。', 'Seleccione de 2 a 4 viviendas usando "Comparar" en la lista o mapa para verlas lado a lado.'))}</div>`;
        return;
      }
      const dimLabels = Object.fromEntries((activeCatalog.dimensions || []).map((d) => [d.key, d.label]));
      const whyHtml = whyOneBeatsTwo(picked[0], picked[1]);
      const cols = picked.map((h, i) => {
        const m = listings[h.id] || {};
        const sc = Math.round(h.matchScore ?? h.score ?? 0);
        return `<th>#${h.rank || i + 1} · ${escH((m.address || h.id).split(',')[0])}<div style="font-weight:800;color:#0f766e">${sc}/100</div></th>`;
      }).join('');
      const row = (lbl, fn) => `<tr><th>${escH(lbl)}</th>${picked.map((h, i) => `<td>${fn(h, listings[h.id] || {}, i)}</td>`).join('')}</tr>`;
      const dimRows = VALID_DIMS.map((dk) => row(dimLabels[dk] || dk, (h) => {
        const d = (h.dimensions || []).find((x) => x.dimension === dk);
        if (!d) return '—';
        return `<b>${Math.round(d.utility)}</b>/100 <span style="font-size:11px;color:#6b7280">(${Math.round((d.share || 0) * 100)}%)</span>${d.estimated ? ' <span class="mbadge mb-fair">est.</span>' : ''}`;
      })).join('');
      wrap.innerHTML = `<div class="cmptray">
        <div class="whybox" style="margin-top:0;background:#fff">${whyHtml}</div>
        <div class="cmptblwrap"><table class="cmptbl"><thead><tr><th>${escH(tr('Criterion', '对比项目', 'Criterio'))}</th>${cols}</tr></thead><tbody>
          ${row(tr('Price', '价格', 'Precio'), (_, m) => `<b>${escH(fmtPrice(m.price))}</b>`)}
          ${row(tr('Measured Coverage', '数据覆盖率', 'Cobertura medida'), (h) => `${Math.round((h.coverage || 0) * 100)}%`)}
          ${row(tr('Specs', '户型参数', 'Especificaciones'), (_, m) => escH([m.beds != null ? `${m.beds} bd` : null, m.baths != null ? `${m.baths} ba` : null, m.sqft ? `${m.sqft.toLocaleString()} sf` : null].filter(Boolean).join(' · ') || '—'))}
          ${row(tr('Public Schools (Official)', '公立学校（官方数据）', 'Escuelas públicas (Oficial)'), (_, m) => m.schoolScore != null ? `${m.schoolScore}/10 ${m.schoolGrade ? `(${escH(m.schoolGrade)})` : ''} · <span style="font-size:11px;color:#6b7280">CDE/CAASPP</span>` : '—')}
          ${row(tr('Hazards', '自然灾害', 'Riesgos'), (_, m) => escH([m.floodZone ? `Flood ${m.floodZone}` : null, m.wildfireZone ? `Fire ${m.wildfireZone}` : null].filter(Boolean).join(' · ') || '—'))}
          ${row(tr('Commutes', '通勤时间', 'Trayectos'), (_, m) => (m.commutes || []).map((c) => `${escH(c.label)}: ${Math.round(c.bufferedMin ?? c.freeFlowMin)}m${c.estimated ? ' (est.)' : ''}`).join('<br>') || '—')}
          ${dimRows}
        </tbody></table></div>
      </div>`;
    }

    function renderExplorerMap() {
      const canvas = document.getElementById('expMapCanvas');
      if (!canvas || !lastAreaRankResult) return;
      const listings = lastAreaRankResult.listings || {};
      const homes = allScoredFromResult(lastAreaRankResult).filter((h) => {
        const m = listings[h.id];
        return m && typeof m.lat === 'number' && typeof m.lon === 'number' && isFinite(m.lat) && isFinite(m.lon);
      });
      if (typeof window.L === 'undefined') {
        canvas.innerHTML = `<div style="padding:14px;overflow-y:auto;height:100%"><div style="font-weight:700;margin-bottom:8px">${escH(tr('Interactive map preview (Leaflet CDN unavailable offline) — Pin Coordinates by Match Band:', '地图预览（离线环境显示坐标与匹配色标）：', 'Vista previa del mapa (sin CDN offline) — Coordenadas por nivel:'))}</div>${homes.map((h) => {
          const m = listings[h.id];
          const sc = Math.round(h.matchScore ?? h.score ?? 0);
          const bm = bandMeta(h.band, sc);
          return `<div style="padding:6px 8px;border-bottom:1px solid #e5e7eb;font-size:12.5px"><span class="mbadge ${bm.cls}">#${h.rank || '—'} · ${sc}/100</span> <b>${escH(m.address)}</b> (${m.lat.toFixed(4)}, ${m.lon.toFixed(4)})</div>`;
        }).join('') || `<div style="color:#6b7280">${escH(tr('No geocoded pins available yet.', '暂无经纬度坐标。', 'Sin coordenadas aún.'))}</div>`}</div>`;
        return;
      }
      if (canvas._ro) { try { canvas._ro.disconnect(); } catch {} canvas._ro = null; }
      if (expLeafletMap) { try { expLeafletMap.remove(); } catch {} expLeafletMap = null; }
      canvas.innerHTML = '';
      if (!homes.length) {
        canvas.innerHTML = `<div style="padding:18px;color:#6b7280">${escH(tr('No geocoded home coordinates in this result set.', '当前结果暂无经纬度坐标。', 'No hay coordenadas geocodificadas en estos resultados.'))}</div>`;
        return;
      }
      const firstM = listings[homes[0].id];
      expLeafletMap = window.L.map(canvas, { scrollWheelZoom: false, zoomControl: true, minZoom: 3, maxZoom: 19 }).setView([firstM.lat, firstM.lon], 12);
      window.L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
        maxZoom: 19,
        maxNativeZoom: 19,
        referrerPolicy: 'strict-origin-when-cross-origin',
        attribution: '© <a href="https://www.openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a> contributors',
      }).addTo(expLeafletMap);
      expLeafletMap.on('click focus zoomstart dragstart', () => { try { expLeafletMap.scrollWheelZoom.enable(); } catch {} });
      canvas.addEventListener('mouseleave', () => { try { expLeafletMap.scrollWheelZoom.disable(); } catch {} });
      canvas.addEventListener('wheel', (ev) => { if (ev.ctrlKey || ev.metaKey) { try { expLeafletMap.scrollWheelZoom.enable(); } catch {} } }, { passive: true, capture: true });
      if (typeof ResizeObserver !== 'undefined') {
        const ro = new ResizeObserver(() => { try { expLeafletMap?.invalidateSize({ pan: false }); } catch {} });
        ro.observe(canvas);
        canvas._ro = ro;
      }
      const bounds = [];
      homes.forEach((h) => {
        const m = listings[h.id];
        const sc = Math.round(h.matchScore ?? h.score ?? 0);
        const bm = bandMeta(h.band, sc);
        const bg = h.flagged ? '#6b7280' : bm.color;
        const rk = h.rank > 0 ? `#${h.rank}` : '!';
        const icon = window.L.divIcon({
          className: '',
          html: `<div class="pinrank" style="background:${bg}">${rk} · ${sc}</div>`,
          iconSize: [64, 24],
          iconAnchor: [32, 12],
        });
        const href = m.reportSlug ? `/r/${encodeURIComponent(m.reportSlug)}/` : `/report?address=${encodeURIComponent(m.address)}`;
        window.L.marker([m.lat, m.lon], { icon })
          .addTo(expLeafletMap)
          .bindPopup(`<b>#${h.rank || '—'} · ${escH(m.address)}</b><br>${escH(fmtPrice(m.price))} · <b>${sc}/100</b> (${escH(bm.label)})<br><a href="${escH(href)}">${escH(tr('Open report →', '打开报告 →', 'Ver informe →'))}</a>`);
        bounds.push([m.lat, m.lon]);
      });
      if (bounds.length > 1) expLeafletMap.fitBounds(bounds, { padding: [30, 30], maxZoom: 14 });
      setTimeout(() => { try { expLeafletMap.invalidateSize(); } catch {} }, 60);
    }

    function renderHomeCard(h, m, isFlaggedSection) {
      const extraSpec = shopSpecsById.get(String(h.id)) || shopSpecsById.get('addr:' + String(m.address || '').trim().toLowerCase()) || {};
      const fullM = Object.assign({}, extraSpec, m);
      const utilPillsHtml = renderUtilityFiberPills(fullM, true);
      const sc = Math.round(h.matchScore ?? h.score ?? 0);
      const bm = bandMeta(h.band, sc);
      const cov = Math.round((h.coverage ?? 0) * 100);
      const href = m.reportSlug ? `/r/${encodeURIComponent(m.reportSlug)}/` : `/report?address=${encodeURIComponent(m.address || h.id)}`;
      const isCmp = compareIds.includes(h.id);
      const isStar = profileStore.isSavedHome(h.id);
      const specs = [
        m.beds != null ? `${m.beds} bd` : null,
        m.baths != null ? `${m.baths} ba` : null,
        m.sqft ? `${m.sqft.toLocaleString('en-US')} sf` : null,
        m.lotSqft ? `${m.lotSqft.toLocaleString('en-US')} lot sf` : null,
        m.yearBuilt ? `${tr('Built', '建于', 'Año')} ${m.yearBuilt}` : null,
        m.hoaMonthly != null ? (m.hoaMonthly === 0 ? 'No HOA' : `HOA $${m.hoaMonthly}/mo`) : null,
      ].filter(Boolean).join(' · ');
      const reasons = (h.reasons || []).slice(0, 2).map((r) => r.text).filter(Boolean);
      const trades = (h.tradeOffs || []).slice(0, 2).map((t) => t.text).filter(Boolean);
      const gates = (h.gates || []).map((g) => g.text || g.reason).filter(Boolean);
      const commutes = (m.commutes || []).slice(0, 2).map((c) => `${escH(c.label)}: ~${Math.round(c.bufferedMin ?? c.freeFlowMin)}m${c.estimated ? ' (est.)' : ''}`).join(' · ');
      const estList = (h.estimated || []).slice(0, 3).map((e) => `<span class="mbadge mb-fair">${escH(e.dimension)} (${escH(tr('est.', '估', 'est.'))})</span>`).join('');
      return `<div class="mcard" style="${isFlaggedSection ? 'border-color:#fca5a5;background:#fffafa' : ''}">
        <div style="display:flex;justify-content:space-between;align-items:center;gap:6px;flex-wrap:wrap;margin-bottom:6px">
          <div style="display:flex;gap:5px;flex-wrap:wrap;align-items:center">
            <span class="rankbadge">${h.rank > 0 ? `#${h.rank}` : '⚠'}</span>
            ${h.tied ? `<span class="mbadge mb-tie">${escH(tr('TIE (≤1 pt)', '并列 (≤1分)', 'EMPATE (≤1 pt)'))}</span>` : ''}
            <span class="mbadge ${bm.cls}">Match ${sc}/100 · ${escH(h.bandLabel || bm.label)}</span>
            <span class="mbadge mb-sch">${cov}% ${escH(tr('measured', '实测', 'medido'))}</span>
            ${estList}
          </div>
          <span style="font-size:11px;color:#6b7280">${escH(m.sourceTier || 'light_scan')}</span>
        </div>
        <div style="font-weight:800;font-size:15px">${escH(m.address || h.id)}</div>
        <div style="font-size:21px;font-weight:800;color:#0f766e;margin:3px 0">${escH(fmtPrice(m.price))} <span style="font-size:12px;font-weight:600;color:#6b7280">${m.status ? escH(m.status.replace('_', ' ').toUpperCase()) : ''}</span></div>
        ${specs ? `<div style="font-size:12.5px;color:#4b5563">${escH(specs)}</div>` : ''}
        ${utilPillsHtml}
        ${m.schoolScore != null ? `<div style="font-size:12px;color:#1d4ed8;margin-top:3px">🎓 ${escH(tr('Public schools:', '公立学区：', 'Escuelas públicas:'))} <b>${m.schoolScore}/10</b> ${m.schoolGrade ? `(${escH(m.schoolGrade)})` : ''} <span style="color:#6b7280">(CDE / CAASPP)</span></div>` : ''}
        ${commutes ? `<div style="font-size:12px;color:#374151;margin-top:2px">🚗 ${commutes}</div>` : ''}
        ${gates.length ? `<div class="whybox" style="border-left-color:#dc2626;background:#fef2f2;color:#991b1b">⚠ ${escH(gates.join(' · '))}</div>` : ''}
        ${reasons.length ? `<div class="whybox"><b>${escH(tr('Why it fits:', '契合点：', 'Por qué encaja:'))}</b> ${escH(reasons.join(' · '))}</div>` : ''}
        ${trades.length ? `<div style="font-size:12px;color:#92400e;margin-bottom:8px"><b>${escH(tr('Trade-offs:', '权衡：', 'Desventajas:'))}</b> ${escH(trades.join(' · '))}</div>` : ''}
        <div style="display:flex;justify-content:space-between;align-items:center;gap:6px;flex-wrap:wrap;margin-top:8px">
          <div style="display:flex;gap:6px">
            <button type="button" class="starbtn ${isStar ? 'on' : ''}" data-star-id="${escH(h.id)}">${isStar ? '★' : '☆'} ${escH(isStar ? tr('Saved', '已收藏', 'Guardada') : tr('Save', '收藏', 'Guardar'))}</button>
            <button type="button" class="cmpbtn ${isCmp ? 'on' : ''}" data-cmp-id="${escH(h.id)}">${isCmp ? '✓ ' : '⚖ '}${escH(isCmp ? tr('Comparing', '对比中', 'Comparando') : tr('Compare', '对比', 'Comparar'))}</button>
          </div>
          <a class="btnsec" href="${escH(href)}">${escH(m.reportSlug ? tr('Full Report →', '完整报告 →', 'Ver informe →') : tr('Analyze →', '深度分析 →', 'Analizar →'))}</a>
        </div>
      </div>`;
    }

    function renderExplorerList() {
      const host = document.getElementById('explorerResults');
      if (!host) return;
      if (!lastAreaRankResult) {
        host.innerHTML = `<div style="font-size:13px;color:#6b7280;margin-top:10px">${escH(tr('Pick 1–10 ZIPs or 1–6 cities in Step 1, then click "Rank Homes in My Areas".', '先在第 1 步选择 1–10 个邮编或 1–6 个城市，然后点击“按偏好排定目标区域房源”。', 'Seleccione 1–10 ZIPs o 1–6 ciudades en el Paso 1 y haga clic en "Clasificar Viviendas".'))}</div>`;
        return;
      }
      const r = lastAreaRankResult.ranked || {};
      const listings = lastAreaRankResult.listings || {};
      const mainList = r.ranked || [];
      const nearList = r.nearMisses || [];
      const flagList = r.flagged || [];
      const resyncBanner = explorerNeedsServerResync
        ? `<div class="noticepill" style="margin-top:10px">⚡ ${escH(tr('Instant slider re-sort preview active (<50ms). Click "Rank Homes in My Areas" to re-evaluate gates on the server.', '已实时预览重排（<50ms）。点击上方按钮可同步服务端红线重算。', 'Vista previa instantánea activa (<50ms). Haga clic arriba para recalcular filtros en el servidor.'))}</div>`
        : '';
      const mainHtml = mainList.length
        ? `<div class="expgrid">${mainList.map((h) => renderHomeCard(h, listings[h.id] || {}, false)).join('')}</div>`
        : `<div class="mbanner">${escH(tr('No homes matched without deal-breakers or near-misses in the selected areas.', '所选区域暂无完全通过筛选的房源。', 'No hubo viviendas sin exclusiones en las zonas seleccionadas.'))}</div>`;
      const showExtra = document.getElementById('expIncludeFlagged')?.checked;
      const nearHtml = (showExtra && nearList.length)
        ? `<div style="margin-top:14px"><div class="lbl">${escH(tr('Near-misses (missed must-have)', '接近达标房源（未满足必备项）', 'Casi compatibles (faltó requisito)'))} (${nearList.length})</div><div class="expgrid">${nearList.map((h) => renderHomeCard(h, listings[h.id] || {}, true)).join('')}</div></div>`
        : '';
      const flagHtml = (showExtra && flagList.length)
        ? `<div style="margin-top:14px"><div class="lbl">${escH(tr('Flagged by deal-breaker', '触及一票否决房源', 'Marcadas por factor excluyente'))} (${flagList.length})</div><div class="expgrid">${flagList.map((h) => renderHomeCard(h, listings[h.id] || {}, true)).join('')}</div></div>`
        : '';
      host.innerHTML = resyncBanner + mainHtml + nearHtml + flagHtml;
      host.querySelectorAll('[data-cmp-id]').forEach((b) => b.addEventListener('click', () => toggleCompareId(b.dataset.cmpId)));
      host.querySelectorAll('[data-star-id]').forEach((b) => b.addEventListener('click', () => {
        const id = b.dataset.starId;
        const m = listings[id] || {};
        const h = allScoredFromResult(lastAreaRankResult).find((x) => x.id === id);
        profileStore.toggleSavedHome({
          id,
          address: m.address || id,
          price: m.price,
          zip: m.zip,
          reportSlug: m.reportSlug,
          score: h ? Math.round(h.matchScore ?? h.score ?? 0) : null,
        });
        renderExplorerList();
        renderSavedHomes();
      }));
    }

    function renderRefineBanner() {
      const b = document.getElementById('explorerRefineBanner');
      if (!b || !lastAreaRankResult) { if (b) b.innerHTML = ''; return; }
      const d = lastAreaRankResult;
      const refining = d.stage === 'refining';
      const msg = refining
        ? tr(
            `Background Tier-2 deep scan running: ${d.refinedDone || 0}/${d.refiningCount || 0} top homes refined (${d.totalScored || 0} scored)…`,
            `后台 Tier-2 深度扫描进行中：已完成 ${d.refinedDone || 0}/${d.refiningCount || 0} 套热门房源（共评分 ${d.totalScored || 0} 套）…`,
            `Escaneo profundo en segundo plano: ${d.refinedDone || 0}/${d.refiningCount || 0} refinadas (${d.totalScored || 0} clasificadas)…`
          )
        : tr(
            `Ranked ${d.totalScored || 0} homes across your areas${(d.syncedZips || []).length ? ` (synced ZIPs: ${d.syncedZips.join(', ')})` : ''}.`,
            `已确定性排序 ${d.totalScored || 0} 套房源${(d.syncedZips || []).length ? `（已同步邮编：${d.syncedZips.join(', ')}）` : ''}。`,
            `Clasificadas ${d.totalScored || 0} viviendas de forma determinista${(d.syncedZips || []).length ? ` (ZIPs sincronizados: ${d.syncedZips.join(', ')})` : ''}.`
          );
      b.innerHTML = `<div class="mbanner" style="margin-top:10px;${refining ? 'background:#fffbeb;border-color:#fde68a;color:#92400e' : ''}"><b>${escH(msg)}</b></div>`;
    }

    function renderExplorer() {
      renderRefineBanner();
      renderCompareTray();
      renderExplorerList();
      if (explorerView === 'map') renderExplorerMap();
      if (explorerView === 'compare') renderCompareView();
    }

    function renderSavedHomes() {
      const box = document.getElementById('savedHomesBox');
      if (!box) return;
      const saved = profileStore.loadSavedHomes();
      if (!saved.length) { box.innerHTML = ''; return; }
      box.innerHTML = `<div style="border-top:1px dashed #d1d5db;padding-top:10px">
        <div class="lbl">★ ${escH(tr('Saved / Starred Homes', '我收藏的房源', 'Viviendas Guardadas'))} (${saved.length})</div>
        <div class="chips" style="margin-top:4px">${saved.slice(0, 12).map((s) => {
          const href = s.reportSlug ? `/r/${encodeURIComponent(s.reportSlug)}/` : `/report?address=${encodeURIComponent(s.address)}`;
          return `<span class="selchip"><a href="${escH(href)}" style="color:inherit;text-decoration:none">${escH((s.address || s.id).split(',')[0])}${s.score != null ? ` (${s.score})` : ''}</a> <button type="button" data-unstar="${escH(s.id)}">×</button></span>`;
        }).join('')}</div>
      </div>`;
      box.querySelectorAll('[data-unstar]').forEach((b) => b.addEventListener('click', () => {
        profileStore.toggleSavedHome({ id: b.dataset.unstar });
        renderSavedHomes();
        renderExplorerList();
      }));
    }

    async function pollRankJob(jobId) {
      stopRankPoll();
      rankPollTimer = setTimeout(async () => {
        try {
          const r = await fetch(`/api/match/rank/${encodeURIComponent(jobId)}?lang=${encodeURIComponent(uiState.lang)}`);
          if (!r.ok) return;
          const j = await r.json();
          if (j && j.ok) {
            lastAreaRankResult = j;
            renderExplorer();
            if (j.stage === 'refining') pollRankJob(jobId);
          }
        } catch {}
      }, 2200);
    }

    async function runAreaRank() {
      stopRankPoll();
      explorerNeedsServerResync = false;
      const btn = document.getElementById('rankAreaBtn');
      const banner = document.getElementById('explorerRefineBanner');
      if (!uiState.areas.zips.length && !uiState.areas.cities.length) {
        if (banner) banner.innerHTML = `<div class="mbanner" style="background:#fffbeb;border-color:#fde68a;color:#92400e">${escH(tr('Add at least 1 ZIP or city in Step 1 above before ranking.', '请先在上方第 1 步添加至少 1 个邮编或城市。', 'Agregue al menos 1 código ZIP o ciudad en el Paso 1 antes de clasificar.'))}</div>`;
        return;
      }
      if (btn) { btn.disabled = true; btn.textContent = tr('Ranking…', '正在排序…', 'Clasificando…'); }
      if (banner) banner.innerHTML = `<div class="mbanner">${escH(tr('Scoring homes & computing driving commutes deterministically…', '正在进行确定性匹配评分与通勤路网计算…', 'Calculando puntuaciones deterministas y tiempos de trayecto…'))}</div>`;
      try {
        const stRaw = document.getElementById('expStatusFilter')?.value || '';
        const statuses = stRaw ? stRaw.split(',').filter(Boolean) : [];
        const autoSync = Boolean(document.getElementById('expAutoSync')?.checked);
        const includeFlagged = Boolean(document.getElementById('expIncludeFlagged')?.checked);
        const triggerDeepScan = Boolean(document.getElementById('expTriggerDeep')?.checked);
        const res = await fetch('/api/match/rank', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            profile: currentProfile,
            zips: uiState.areas.zips,
            cities: uiState.areas.cities,
            ...(statuses.length ? { statuses } : { statuses: [] }),
            autoSync,
            includeFlagged,
            triggerDeepScan,
            lang: uiState.lang,
          }),
        });
        const j = await res.json();
        if (!res.ok || !j.ok) {
          if (banner) banner.innerHTML = `<div class="mbanner" style="background:#fef2f2;border-color:#fecaca;color:#991b1b">${escH(j.error || 'Area ranking failed')}</div>`;
          return;
        }
        lastAreaRankResult = j;
        renderExplorer();
        const rankZips = [...new Set([...uiState.areas.zips, ...Object.values(j.listings || {}).map((x) => x && x.zip).filter(Boolean)])];
        if (rankZips.length) {
          hydrateShopSpecsForZips(rankZips).then((loaded) => { if (loaded) renderExplorerList(); });
        }
        if (j.stage === 'refining' && j.jobId) pollRankJob(j.jobId);
      } catch (err) {
        if (banner) banner.innerHTML = `<div class="mbanner" style="background:#fef2f2;border-color:#fecaca;color:#991b1b">${escH(err.message || err)}</div>`;
      } finally {
        if (btn) { btn.disabled = false; btn.textContent = tUi('rankAreaBtn'); }
      }
    }

    document.getElementById('rankAreaBtn')?.addEventListener('click', runAreaRank);
    document.getElementById('studioRankBtn')?.addEventListener('click', () => {
      runAreaRank();
      document.getElementById('areaExplorerSection')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    });
    document.getElementById('expIncludeFlagged')?.addEventListener('change', () => {
      if (lastAreaRankResult) runAreaRank();
    });
    renderExplorerList();
    renderSavedHomes();
  }
  (function initAreaOnsale() {
    const AREA_PREF_KEY = 'oe_area_onsale_pref_v1';
    const zipForm = document.getElementById('areaZipForm');
    const zipInput = document.getElementById('areaZipInput');
    const zipBtn = document.getElementById('areaZipBtn');
    const geoBtn = document.getElementById('areaGeoBtn');
    const statusEl = document.getElementById('areaOnsaleStatus');
    const gridEl = document.getElementById('areaOnsaleResults');
    if (!zipForm || !zipInput || !zipBtn || !geoBtn || !statusEl || !gridEl) return;

    let lastPayload = null;
    let manualAreaOnsaleSelected = false;
    let reqSeq = 0;
    let activeAbort = null;

    const setBusy = (busy) => {
      zipBtn.disabled = busy;
      geoBtn.disabled = busy;
    };
    const setStatus = (msg, isErr) => {
      statusEl.textContent = msg || '';
      statusEl.classList.toggle('on', Boolean(msg));
      statusEl.classList.toggle('err', Boolean(msg && isErr));
    };
    const fmtAreaMoney = (v) => {
      if (!Number.isFinite(v) || v <= 0) return '';
      try {
        const loc = uiState.lang === 'zh-CN' ? 'zh-CN' : uiState.lang === 'es' ? 'es-US' : 'en-US';
        return new Intl.NumberFormat(loc, { style: 'currency', currency: 'USD', maximumFractionDigits: 0 }).format(Math.round(v));
      } catch {
        return '$' + Math.round(v).toLocaleString('en-US');
      }
    };
    const fmtAreaNum = (v) => {
      if (!Number.isFinite(v)) return '';
      try {
        const loc = uiState.lang === 'zh-CN' ? 'zh-CN' : uiState.lang === 'es' ? 'es-US' : 'en-US';
        return new Intl.NumberFormat(loc).format(Math.round(v));
      } catch {
        return String(Math.round(v));
      }
    };

    function renderAreaPayload() {
      if (!lastPayload) return;
      const lg = uiState.lang;
      const L = (en, zh, es) => (lg === 'zh-CN' ? zh : lg === 'es' ? es : en);
      const items = Array.isArray(lastPayload.listings) ? lastPayload.listings : [];
      const areaObj = lastPayload.area || {};
      const areaLabel = areaObj.label || [areaObj.city, areaObj.state].filter(Boolean).join(', ') || areaObj.zip || '';
      if (!items.length) {
        gridEl.innerHTML = '';
        setStatus(tUi('areaEmpty'), false);
        return;
      }
      setStatus(`${tUi('areaFoundPrefix')} ${areaLabel} (${items.length})`.replace(/\s+/g, ' ').trim(), false);
      gridEl.innerHTML = items.map((item) => {
        const isPend = String(item.status || '').toLowerCase().includes('pend');
        const stTxt = isPend ? tUi('areaStatusPending') : tUi('areaStatusForSale');
        const domTxt = Number.isFinite(item.daysOnMarket) && item.daysOnMarket > 0
          ? L(`${item.daysOnMarket}d`, `${item.daysOnMarket}天`, `${item.daysOnMarket}d`)
          : '';
        const priceTxt = fmtAreaMoney(item.price);
        const cityStateZip = [item.city, item.state].filter(Boolean).join(', ') + (item.zip ? ` ${item.zip}` : '');
        const specs = [];
        if (Number.isFinite(item.beds) && item.beds > 0) specs.push(L(`${item.beds} bd`, `${item.beds} 卧`, `${item.beds} hab`));
        if (Number.isFinite(item.baths) && item.baths > 0) specs.push(L(`${item.baths} ba`, `${item.baths} 卫`, item.baths === 1 ? '1 baño' : `${item.baths} baños`));
        if (Number.isFinite(item.sqft) && item.sqft > 0) specs.push(L(`${fmtAreaNum(item.sqft)} sq ft`, `${fmtAreaNum(item.sqft)} 平方英尺`, `${fmtAreaNum(item.sqft)} pies²`));
        const href = item.reportUrl || (`/report?address=${encodeURIComponent(item.address || '')}`);
        return `<a class="area-home-card" href="${escH(href)}">
          <div class="area-hc-top">
            <span class="area-hc-status${isPend ? ' pend' : ''}">${escH(stTxt)}</span>
            ${domTxt ? `<span class="area-hc-dom">${escH(domTxt)}</span>` : ''}
          </div>
          ${priceTxt ? `<div class="area-hc-price">${escH(priceTxt)}</div>` : ''}
          <div class="area-hc-street">${escH(item.street || item.address || '')}</div>
          ${cityStateZip.trim() ? `<div class="area-hc-loc">${escH(cityStateZip.trim())}</div>` : ''}
          ${specs.length ? `<div class="area-hc-specs">${escH(specs.join(' · '))}</div>` : ''}
          <div class="area-hc-cta">${escH(tUi('areaGenerateCta'))}</div>
        </a>`;
      }).join('');
    }
    window.__renderAreaOnsalePayload = renderAreaPayload;

    async function loadAreaOnsale(params, loadingMsg, opts) {
      const background = Boolean(opts && opts.background);
      const persistPref = Boolean(opts && opts.persistPref);
      const syncDetectedMode = opts && opts.syncDetectedMode ? String(opts.syncDetectedMode) : '';
      const mySeq = ++reqSeq;
      if (activeAbort) {
        try { activeAbort.abort(); } catch {}
      }
      const ctrl = typeof AbortController !== 'undefined' ? new AbortController() : null;
      activeAbort = ctrl;
      if (!background) setBusy(true);
      setStatus(loadingMsg || tUi('areaZipLoading'), false);
      try {
        const qs = new URLSearchParams();
        if (params.zip) qs.set('zip', params.zip);
        if (Number.isFinite(params.lat) && Number.isFinite(params.lon)) {
          qs.set('lat', String(params.lat));
          qs.set('lon', String(params.lon));
        }
        qs.set('limit', '6');
        const res = await fetch('/api/area/forsale?' + qs.toString(), {
          credentials: 'same-origin',
          signal: ctrl ? ctrl.signal : undefined,
        });
        const data = await res.json().catch(() => ({}));
        if (mySeq !== reqSeq) return;
        if (!res.ok || !data || data.ok === false) {
          gridEl.innerHTML = '';
          lastPayload = null;
          if (background) {
            setStatus('', false);
          } else {
            setStatus((data && data.error) || tUi('areaEmpty'), true);
          }
          return;
        }
        lastPayload = data;
        const resolvedZip = (data.area && data.area.zip && /^\d{5}$/.test(String(data.area.zip)))
          ? String(data.area.zip)
          : (params.zip && /^\d{5}$/.test(String(params.zip)) ? String(params.zip) : '');
        if (resolvedZip && (!zipInput.value.trim() || !background || syncDetectedMode)) {
          zipInput.value = resolvedZip;
        }
        if (syncDetectedMode === 'gps') {
          applyDetectedGeoDefaults({
            postalCode: resolvedZip,
            city: (data.area && data.area.city) || '',
            regionCode: (data.area && data.area.state) || '',
            lat: Number.isFinite(params.lat) ? params.lat : null,
            lon: Number.isFinite(params.lon) ? params.lon : null,
            source: 'gps',
            mode: 'gps',
          }, { userInitiated: true, skipAreaOnsaleFetch: true });
        } else if (resolvedZip && activeGeo && !activeGeo.postalCode) {
          applyDetectedGeoDefaults({
            postalCode: resolvedZip,
            city: (data.area && data.area.city) || activeGeo.city || '',
            regionCode: (data.area && data.area.state) || activeGeo.regionCode || '',
            lat: activeGeo.lat,
            lon: activeGeo.lon,
            metro: activeGeo.metro || '',
            source: activeGeo.source || 'ip',
            mode: activeGeo.mode || 'ip',
          }, { userInitiated: !background, skipAreaOnsaleFetch: true });
        }
        if (persistPref) {
          try {
            const pref = resolvedZip
              ? { mode: 'zip', zip: resolvedZip }
              : params;
            localStorage.setItem(AREA_PREF_KEY, JSON.stringify(pref));
          } catch {}
        }
        renderAreaPayload();
      } catch (err) {
        if (mySeq !== reqSeq || (err && err.name === 'AbortError')) return;
        gridEl.innerHTML = '';
        lastPayload = null;
        if (background) {
          setStatus('', false);
        } else {
          setStatus(tUi('areaEmpty'), true);
        }
      } finally {
        if (mySeq === reqSeq) setBusy(false);
      }
    }

    window.__syncAreaOnsaleFromGeo = (geo, syncOpts) => {
      if (!geo) return;
      const userInitiated = Boolean(syncOpts && syncOpts.userInitiated);
      if (!userInitiated && (manualAreaOnsaleSelected || (zipInput.value || '').trim().length > 0)) {
        return;
      }
      const cleanZip = String(geo.postalCode || '').replace(/\D/g, '').slice(0, 5);
      if (cleanZip.length === 5) {
        if (userInitiated) manualAreaOnsaleSelected = true;
        zipInput.value = cleanZip;
        loadAreaOnsale(
          { mode: 'zip', zip: cleanZip },
          tUi('areaZipLoading'),
          { background: !userInitiated, persistPref: userInitiated }
        );
      } else if (typeof geo.lat === 'number' && Number.isFinite(geo.lat) && typeof geo.lon === 'number' && Number.isFinite(geo.lon)) {
        if (userInitiated) manualAreaOnsaleSelected = true;
        loadAreaOnsale(
          { mode: 'geo', lat: geo.lat, lon: geo.lon },
          tUi('areaZipLoading'),
          { background: !userInitiated, persistPref: userInitiated }
        );
      }
    };

    window.__clearAreaOnsaleFromGeo = () => {
      manualAreaOnsaleSelected = false;
      lastPayload = null;
      if (activeAbort) {
        try { activeAbort.abort(); } catch {}
        activeAbort = null;
      }
      reqSeq++;
      setBusy(false);
      zipInput.value = '';
      gridEl.innerHTML = '';
      setStatus('', false);
      try { localStorage.removeItem(AREA_PREF_KEY); } catch {}
    };

    zipForm.addEventListener('submit', (e) => {
      e.preventDefault();
      const raw = (zipInput.value || '').trim().replace(/\D+/g, '').slice(0, 5);
      if (raw.length !== 5) {
        setStatus(tUi('areaZipInvalid'), true);
        zipInput.focus();
        return;
      }
      manualAreaOnsaleSelected = true;
      zipInput.value = raw;
      applyDetectedGeoDefaults(
        { postalCode: raw, city: '', regionCode: '', lat: null, lon: null, mode: 'manual' },
        { userInitiated: true, skipAreaOnsaleFetch: true }
      );
      loadAreaOnsale(
        { mode: 'zip', zip: raw },
        tUi('areaZipLoading'),
        { background: false, persistPref: true }
      );
    });

    geoBtn.addEventListener('click', () => {
      if (!navigator.geolocation || typeof navigator.geolocation.getCurrentPosition !== 'function') {
        setStatus(tUi('areaGeoUnsupported'), true);
        zipInput.focus();
        return;
      }
      setBusy(true);
      setStatus(tUi('areaGeoLocating'), false);
      navigator.geolocation.getCurrentPosition(
        (pos) => {
          const lat = pos && pos.coords && pos.coords.latitude;
          const lon = pos && pos.coords && pos.coords.longitude;
          if (!Number.isFinite(lat) || !Number.isFinite(lon)) {
            setBusy(false);
            setStatus(tUi('areaGeoDenied'), true);
            return;
          }
          manualAreaOnsaleSelected = true;
          loadAreaOnsale(
            { mode: 'geo', lat, lon },
            tUi('areaGeoLocating'),
            { background: false, persistPref: true, syncDetectedMode: 'gps' }
          );
        },
        (err) => {
          setBusy(false);
          const msg = err && err.code === 1 ? tUi('areaGeoDenied') : tUi('areaGeoUnavailable');
          setStatus(msg, true);
          zipInput.focus();
        },
        { enableHighAccuracy: false, timeout: 12000, maximumAge: 60000 }
      );
    });

    if (initZip.length === 5) {
      manualAreaOnsaleSelected = true;
      zipInput.value = initZip;
      loadAreaOnsale({ mode: 'zip', zip: initZip }, tUi('areaZipLoading'), { background: true, persistPref: false });
      return;
    }

    try {
      const cleared = sessionStorage.getItem('openestate:geo_cleared:v1') === '1';
      const saved = !cleared ? JSON.parse(localStorage.getItem(AREA_PREF_KEY) || 'null') : null;
      if (saved && saved.zip && /^\d{5}$/.test(String(saved.zip))) {
        manualAreaOnsaleSelected = true;
        zipInput.value = String(saved.zip);
        loadAreaOnsale({ mode: 'zip', zip: String(saved.zip) }, tUi('areaZipLoading'), { background: true, persistPref: false });
      } else if (saved && Number.isFinite(saved.lat) && Number.isFinite(saved.lon)) {
        manualAreaOnsaleSelected = true;
        loadAreaOnsale({ mode: 'geo', lat: saved.lat, lon: saved.lon }, tUi('areaZipLoading'), { background: true, persistPref: false });
      }
    } catch {}
  })();
  document.getElementById('anatFocusSearchBtn')?.addEventListener('click', () => {
    if (inp) {
      inp.scrollIntoView({ behavior: 'smooth', block: 'center' });
      setTimeout(() => inp.focus(), 120);
    }
  });

  function applyShopStaticLabels() {
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const setTxt = (id, en, zh, es) => { const el = document.getElementById(id); if (el) el.textContent = L(en, zh, es); };
    setTxt('tabBtnShop', 'Shop Homes', '条件选房', 'Comprar Casas');
    setTxt('shopSideTitle', '⚙ Filter by Specs', '⚙ 按参数筛选房源', '⚙ Filtrar por especificaciones');
    setTxt('shopSidebarResetBtn', 'Clear All', '重置全部', 'Limpiar');
    setTxt('fgTitleInternet', '1. Home Internet & Fiber', '1. 家庭宽带与光纤', '1. Internet y Fibra Óptica');
    setTxt('lblSfFiber', 'Symmetrical Fiber (FTTH)', '对称光纤入户 (FTTH)', 'Fibra Simétrica (FTTH)');
    setTxt('lblSfHighSpeed', 'High-Speed Broadband', '高速宽带覆盖', 'Banda Ancha de Alta Velocidad');
    setTxt('fgSubMinDl', 'Min Download Speed', '最低下行带宽', 'Velocidad Mín. de Descarga');
    setTxt('fgSubIspCarriers', 'Broadband Carriers', '宽带与光纤运营商', 'Operadores de Internet');
    setTxt('fgTitleElectric', '2. Electricity Carrier & Rate', '2. 电力公司与电价费率', '2. Compañía Eléctrica y Tarifa');
    setTxt('lblSfMuniElec', 'Municipal Public Power Only', '仅市政公共电力公司', 'Solo Energía Pública Municipal');
    setTxt('fgSubMaxElecRate', 'Max Tariff Rate', '最高度电费率', 'Tarifa Eléctrica Máxima');
    setTxt('fgSubElecCarriers', 'Electric Utility Carriers', '供电局 / 电力公司', 'Compañías Eléctricas');
    setTxt('fgTitleBudget', '3. All-In Monthly & Price', '3. 全包月供与总价', '3. Costo Mensual Total y Precio');
    setTxt('fgSubMaxMonthly', 'Max All-In Monthly', '全包月供上限', 'Costo Mensual Total Máx.');
    setTxt('fgSubMaxPrice', 'Max Purchase Price', '购房总价上限', 'Precio Máximo de Compra');
    setTxt('fgSubHoa', 'HOA Dues', '物业费 (HOA)', 'Cuota HOA');
    setTxt('lblSfNoHoa', 'No HOA ($0/mo)', '无 HOA ($0/月)', 'Sin HOA ($0/mes)');
    setTxt('lblSfHoa300', 'HOA ≤ $300/mo', 'HOA ≤ $300/月', 'HOA ≤ $300/mes');
    setTxt('fgTitleBedsType', '4. Beds, Baths & Property Type', '4. 卧室、卫浴与房型', '4. Recámaras, Baños y Tipo');
    setTxt('fgSubMinBeds', 'Min Beds', '最少卧室', 'Recámaras Mín.');
    setTxt('fgSubMinBaths', 'Min Baths', '最少卫浴', 'Baños Mín.');
    setTxt('fgSubPropType', 'Property Type', '房屋类型', 'Tipo de Propiedad');
    setTxt('lblPtSf', 'Single-Family', '独栋住宅', 'Unifamiliar');
    setTxt('lblPtTh', 'Townhouse', '联排别墅', 'Adosada');
    setTxt('lblPtCd', 'Condo', '公寓 Condo', 'Condominio');
    setTxt('lblPtMf', 'Multi-Family (2–4)', '多户住宅 (2–4户)', 'Multifamiliar (2–4)');
    setTxt('fgTitleHardware', '5. Energy, Lot & Hardware', '5. 能源、地块与配置', '5. Energía, Lote y Equipamiento');
    setTxt('lblSfSolar', 'Solar Panels', '太阳能光伏', 'Paneles Solares');
    setTxt('lblSfEv', 'EV Charger', '电动汽车充电桩', 'Cargador EV');
    setTxt('lblSfHeatPump', 'Heat Pump HVAC', '热泵冷暖空调', 'Bomba de Calor');
    setTxt('lblSfHasAdu', 'Existing ADU', '现成合规 ADU', 'ADU Existente');
    setTxt('lblSfAduFeasible', 'ADU-Eligible Lot', '可加建 ADU 地块', 'Lote Apto para ADU');
    setTxt('lblSfPool', 'Pool', '私人泳池', 'Piscina');
    setTxt('lblSfGarage2', '2+ Garage Spaces', '2+ 车位车库', 'Garaje 2+ Autos');
    setTxt('fgTitleWaterTax', '6. Water, Sewer, Tax & Hazards', '6. 供水、排污、特别税与灾害', '6. Agua, Alcantarillado, Impuestos y Riesgos');
    setTxt('lblSfMuniWater', 'Municipal Water', '市政自来水', 'Agua Municipal');
    setTxt('lblSfMuniSewer', 'Municipal Sewer (No Septic)', '市政排污（非化粪池）', 'Alcantarillado Municipal');
    setTxt('lblSfNoSpecTax', 'No Special Tax (Mello-Roos/MUD/CDD)', '无附加特别税 (Mello-Roos/MUD/CDD)', 'Sin Impuesto Especial (Mello-Roos/MUD/CDD)');
    setTxt('lblSfLowFlood', 'Outside FEMA SFHA Flood', '非 FEMA 高危洪泛区', 'Fuera de Zona Inundable FEMA');
    setTxt('lblSfLowFire', 'Outside High Wildfire Zone', '非高危山火等级区', 'Fuera de Riesgo Alto de Incendio');
    setTxt('fgSubSchoolGrade', 'Min Official School Grade', '公立学区官方评级下限', 'Grado Escolar Oficial Mín.');
    setTxt('fgTitleLocation', '7. Location (City / ZIP)', '7. 城市与邮编 (City / ZIP)', '7. Ubicación (Ciudad / ZIP)');
    setTxt('fgSubSyncZip', 'Sync Real ZIP into Catalog', '实时同步真实邮编房源', 'Sincronizar ZIP Real');
    setTxt('shopSideSyncBtn', 'Sync ZIP', '同步邮编', 'Sincronizar');
    setTxt('lblShopMobFilters', 'Filters', '筛选条件', 'Filtros');
    setTxt('shopApplyBtn', 'Search', '搜索房源', 'Buscar');
    setTxt('qpFiber', 'Symmetrical Fiber', '对称光纤', 'Fibra Simétrica');
    setTxt('qpMuniElec', 'Municipal Power', '市政电力', 'Energía Municipal');
    setTxt('qpRate25', '≤ $0.25/kWh', '≤ $0.25/度', '≤ $0.25/kWh');
    setTxt('qpNoHoa', '$0 HOA', '$0 物业费', '$0 HOA');
    setTxt('qpNoSpecTax', 'No Mello-Roos/MUD', '无特别附加税', 'Sin Mello-Roos/MUD');
    setTxt('qpAduReady', 'ADU Ready', '支持 ADU', 'Apto ADU');
    setTxt('qpSolar', 'Solar', '太阳能', 'Solar');
    document.querySelectorAll('[data-shop-any="1"]').forEach((el) => { el.textContent = L('Any', '不限', 'Cualquiera'); });
    if (shopSearchInput) shopSearchInput.placeholder = L(
      'Filter by specs or keyword (e.g. fiber, municipal power, solar, ADU, single-story)…',
      '按配置或关键词筛选（如 光纤、市政电力、太阳能、ADU、平层）…',
      'Filtrar por especificaciones o palabra clave (fibra, energía municipal, solar, ADU)…'
    );
    if (shopLocQuickInput) shopLocQuickInput.placeholder = L('ZIP or City', '邮编或城市', 'ZIP o Ciudad');
    const shopSideZip = document.getElementById('shopSideZipInput');
    if (shopSideZip) shopSideZip.placeholder = L('5-digit US ZIP', '5 位美国邮编', 'ZIP de 5 dígitos');
  }

  // ============================================================================================
  // Default IP / Browser Geolocation Area Detection (/api/geo/ip + already-granted GPS)
  // Zero unsolicited permission prompts · Zero hardcoded fallback ZIPs when offline/unresolvable
  // ============================================================================================
  const GEO_CLEARED_SS_KEY = 'openestate:geo_cleared:v1';
  const GEO_AUTO_MATCH_SS_KEY = 'openestate:auto_match_zip:v1';
  try {
    geoClearedByUser = sessionStorage.getItem(GEO_CLEARED_SS_KEY) === '1';
    autoSeededMatchZip = sessionStorage.getItem(GEO_AUTO_MATCH_SS_KEY) || '';
  } catch {}

  const formatGeoHeadline = (g) => {
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const cityState = [g.city, g.regionCode].filter(Boolean).join(', ');
    const areaLabel = [cityState, g.postalCode].filter(Boolean).join(' ');
    if (g.mode === 'gps') {
      return L('📍 Near ' + areaLabel + ' (GPS)', '📍 精确定位：' + areaLabel + ' (GPS)', '📍 Ubicación precisa: ' + areaLabel + ' (GPS)');
    }
    if (g.mode === 'manual') {
      return L('📍 Active area: ' + areaLabel, '📍 当前选定区域：' + areaLabel, '📍 Zona activa: ' + areaLabel);
    }
    return L('📍 Detected near ' + areaLabel + ' (IP)', '📍 已检测附近区域：' + areaLabel + ' (IP)', '📍 Detectado cerca de ' + areaLabel + ' (IP)');
  };

  function clearDetectedGeo() {
    const prevZip = activeGeo && activeGeo.postalCode ? activeGeo.postalCode : '';
    geoClearedByUser = true;
    geoEditingHostId = null;
    shopDetectedZipZeroLocal = false;
    shopAutoGeoPendingCheck = false;
    shopScopeMode = 'all';
    try { sessionStorage.setItem(GEO_CLEARED_SS_KEY, '1'); } catch {}
    if (typeof window.__clearAreaOnsaleFromGeo === 'function') {
      window.__clearAreaOnsaleFromGeo();
    }
    if (shopLocQuickInput && prevZip && (shopLocQuickInput.value || '').trim() === prevZip) {
      shopLocQuickInput.value = '';
    }
    if (prevZip) shopSelectedZips.delete(prevZip);
    const subEl = document.getElementById('subZips');
    if (subEl && prevZip && subEl.value.trim() === prevZip && autoFilledSubZip === prevZip) subEl.value = '';
    if (autoSeededMatchZip && uiState && uiState.areas && uiState.areas.zips.length === 1 && uiState.areas.zips[0] === autoSeededMatchZip && uiState.areas.cities.length === 0) {
      uiState.areas.zips = [];
      uiState = profileStore.saveUi({ areas: { zips: [], cities: [] } });
      try { sessionStorage.removeItem(GEO_AUTO_MATCH_SS_KEY); } catch {}
      autoSeededMatchZip = '';
      if (studioInitialized) renderSelectedAreas();
    }
    renderShopGeoUnsyncedBanner();
    renderGeoLocationBars();
    if (document.getElementById('tab-shop')?.classList.contains('act')) {
      loadShopCatalog({ keepZipWhenEmpty: true });
    }
  }

  function applyDetectedGeoDefaults(geoObj, opts) {
    if (!geoObj) return;
    const cleanZip = String(geoObj.postalCode || '').replace(/\D/g, '').slice(0, 5);
    const userInitiated = Boolean(opts && opts.userInitiated);
    const skipAreaOnsaleFetch = Boolean(opts && opts.skipAreaOnsaleFetch);
    activeGeo = {
      postalCode: cleanZip.length === 5 ? cleanZip : '',
      city: String(geoObj.city || '').trim(),
      regionCode: String(geoObj.regionCode || '').trim(),
      lat: (typeof geoObj.lat === 'number' && Number.isFinite(geoObj.lat)) ? geoObj.lat : null,
      lon: (typeof geoObj.lon === 'number' && Number.isFinite(geoObj.lon)) ? geoObj.lon : null,
      metro: String(geoObj.metro || '').trim(),
      source: String(geoObj.source || '').trim(),
      mode: geoObj.mode || 'ip',
    };
    if (!activeGeo.postalCode) {
      if (!skipAreaOnsaleFetch && activeGeo.lat != null && activeGeo.lon != null && typeof window.__syncAreaOnsaleFromGeo === 'function') {
        window.__syncAreaOnsaleFromGeo(activeGeo, { userInitiated });
      }
      renderGeoLocationBars();
      return;
    }
    geoClearedByUser = false;
    try { sessionStorage.removeItem(GEO_CLEARED_SS_KEY); } catch {}

    // 1. Sync #areaOnsaleSection ("See onsale property in your area:") automatically when not skipped
    if (!skipAreaOnsaleFetch && typeof window.__syncAreaOnsaleFromGeo === 'function') {
      window.__syncAreaOnsaleFromGeo(activeGeo, { userInitiated });
    }

    // 2. Pre-fill empty ZIP inputs in #tab-alerts & #tab-shop sidebar
    const subEl = document.getElementById('subZips');
    if (subEl && (!subEl.value.trim() || (userInitiated && subEl.value.trim() === autoFilledSubZip))) {
      subEl.value = activeGeo.postalCode;
      autoFilledSubZip = activeGeo.postalCode;
    }
    const sideZipEl = document.getElementById('shopSideZipInput');
    if (sideZipEl && (!sideZipEl.value.trim() || userInitiated)) {
      sideZipEl.value = activeGeo.postalCode;
    }

    // 3. Pre-fill /match area default when user profile has 0 saved ZIPs/cities
    const curUi = profileStore.loadUi();
    const canSeedMatch = (curUi.areas.zips.length === 0 && curUi.areas.cities.length === 0) ||
      (userInitiated && autoSeededMatchZip && curUi.areas.zips.length === 1 && curUi.areas.zips[0] === autoSeededMatchZip && curUi.areas.cities.length === 0);
    if (canSeedMatch) {
      autoSeededMatchZip = activeGeo.postalCode;
      try { sessionStorage.setItem(GEO_AUTO_MATCH_SS_KEY, autoSeededMatchZip); } catch {}
      uiState = profileStore.saveUi({ areas: { zips: [activeGeo.postalCode], cities: [] } });
      if (studioInitialized) {
        renderSelectedAreas();
      } else {
        updateStudioSummary();
      }
    }

    // 4. Pre-fill #tab-shop default location context (with automatic fallback if 0 local listings)
    const shopHasManualLoc = Boolean(((shopLocQuickInput?.value || '').trim()) || shopSelectedZips.size || shopSelectedCities.size);
    if (!shopHasManualLoc || userInitiated) {
      if (userInitiated) {
        shopSelectedZips.clear();
        shopSelectedCities.clear();
      }
      if (shopLocQuickInput) shopLocQuickInput.value = activeGeo.postalCode;
      shopScopeMode = 'zip';
      shopDetectedZipZeroLocal = false;
      shopAutoGeoPendingCheck = true;
      if (document.getElementById('tab-shop')?.classList.contains('act')) {
        loadShopCatalog({ fromAutoGeo: true });
      }
    }
    renderGeoLocationBars();
  }

  async function resolveBrowserGpsLocation(timeoutMs) {
    if (!navigator.geolocation) return null;
    const pos = await new Promise((resolve) => {
      try {
        navigator.geolocation.getCurrentPosition(
          (p) => resolve(p),
          () => resolve(null),
          { enableHighAccuracy: false, timeout: timeoutMs || 6000, maximumAge: 300000 }
        );
      } catch { resolve(null); }
    });
    if (!pos || !pos.coords) return null;
    const lat = Number(pos.coords.latitude);
    const lon = Number(pos.coords.longitude);
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) return null;

    // First try server-side reverse lookup if /api/geo/ip accepts lat/lon
    try {
      const r = await fetchJsonTimeout('/api/geo/ip?lat=' + encodeURIComponent(lat) + '&lon=' + encodeURIComponent(lon), 3500);
      if (r.ok) {
        const j = await r.json();
        const zp = String((j && (j.postalCode || j.zip)) || '').match(/\b(\d{5})\b/)?.[1] || '';
        const nearCoord = typeof j?.lat === 'number' && typeof j?.lon === 'number' && Math.abs(j.lat - lat) < 0.2 && Math.abs(j.lon - lon) < 0.2;
        if (j && j.ok && zp && nearCoord) {
          return {
            ok: true,
            postalCode: zp,
            city: j.city || '',
            regionCode: j.regionCode || j.state || '',
            lat,
            lon,
            metro: j.metro || '',
            source: 'gps',
            mode: 'gps',
          };
        }
      }
    } catch {}

    // Reverse geocode via OpenStreetMap Nominatim for exact 5-digit postcode
    try {
      const nr = await fetchJsonTimeout('https://nominatim.openstreetmap.org/reverse?format=jsonv2&lat=' + encodeURIComponent(lat) + '&lon=' + encodeURIComponent(lon) + '&zoom=18&addressdetails=1', 4000);
      if (nr.ok) {
        const nj = await nr.json();
        const addr = (nj && nj.address) || {};
        const zp = String(addr.postcode || '').match(/\b(\d{5})\b/)?.[1] || '';
        const city = addr.city || addr.town || addr.village || addr.municipality || addr.suburb || '';
        const isoState = String(addr['ISO3166-2-lvl4'] || '').replace(/^US-/i, '');
        const regionCode = /^[A-Za-z]{2}$/.test(isoState) ? isoState.toUpperCase() : '';
        if (zp) {
          return { ok: true, postalCode: zp, city, regionCode, lat, lon, metro: '', source: 'gps', mode: 'gps' };
        }
      }
    } catch {}

    return { ok: true, postalCode: '', city: '', regionCode: '', lat, lon, metro: '', source: 'gps', mode: 'gps' };
  }

  async function fetchIpLocation() {
    try {
      const r = await fetchJsonTimeout('/api/geo/ip', 4500);
      if (!r.ok) return null;
      const j = await r.json();
      if (!j || !j.ok) return null;
      const zp = String(j.postalCode || j.zip || '').match(/\b(\d{5})\b/)?.[1] || '';
      return {
        ok: true,
        postalCode: zp,
        city: String(j.city || '').trim(),
        regionCode: String(j.regionCode || j.state || '').trim(),
        lat: (typeof j.lat === 'number' && Number.isFinite(j.lat)) ? j.lat : null,
        lon: (typeof j.lon === 'number' && Number.isFinite(j.lon)) ? j.lon : null,
        metro: String(j.metro || '').trim(),
        source: String(j.source || 'ip').trim(),
        mode: 'ip',
      };
    } catch {
      return null;
    }
  }

  function renderGeoLocationBars() {
    const L = (en, zh, es) => (CUR_LANG === 'zh-CN' ? zh : CUR_LANG === 'es' ? es : en);
    const hosts = ['shopGeoLocBar', 'matchGeoLocBar', 'alertsGeoLocBar'];
    const hasGpsApi = Boolean(navigator.geolocation);
    const hasActiveZip = Boolean(!geoClearedByUser && activeGeo && activeGeo.postalCode);

    hosts.forEach((hostId) => {
      const host = document.getElementById(hostId);
      if (!host) return;
      if (!hasActiveZip && !geoClearedByUser && !geoResolvedOnce) {
        host.innerHTML = '';
        return;
      }
      if (!hasActiveZip && !geoClearedByUser && !hasGpsApi) {
        host.innerHTML = '';
        return;
      }

      const isEditing = geoEditingHostId === hostId;
      if (isEditing) {
        const curZipVal = (activeGeo && activeGeo.postalCode) ? activeGeo.postalCode : '';
        host.innerHTML = '<div class="geo-pill-bar">' +
          '<div class="geo-pill-left"><span>📍 ' + escH(L('Set active 5-digit US ZIP:', '设置当前 5 位美国邮编：', 'Definir código ZIP activo:')) + '</span>' +
            '<input type="text" class="geo-zip-inline" data-geo-zip-inp="' + hostId + '" maxlength="5" inputmode="numeric" placeholder="ZIP" value="' + escH(curZipVal) + '">' +
            '<button type="button" class="geo-act on" data-geo-zip-save="' + hostId + '">' + escH(L('Apply', '应用', 'Aplicar')) + '</button>' +
            '<button type="button" class="geo-act mut" data-geo-zip-cancel="' + hostId + '">' + escH(L('Cancel', '取消', 'Cancelar')) + '</button>' +
          '</div></div>';
        const zInp = host.querySelector('[data-geo-zip-inp]');
        const applyEdit = () => {
          const z = String(zInp?.value || '').replace(/\D/g, '').slice(0, 5);
          if (z.length !== 5) { zInp?.focus(); return; }
          geoEditingHostId = null;
          applyDetectedGeoDefaults({ postalCode: z, city: '', regionCode: '', lat: null, lon: null, mode: 'manual' }, { userInitiated: true });
        };
        host.querySelector('[data-geo-zip-save]')?.addEventListener('click', applyEdit);
        zInp?.addEventListener('keydown', (ev) => {
          if (ev.key === 'Enter') { ev.preventDefault(); applyEdit(); }
          else if (ev.key === 'Escape') { geoEditingHostId = null; renderGeoLocationBars(); }
        });
        host.querySelector('[data-geo-zip-cancel]')?.addEventListener('click', () => {
          geoEditingHostId = null;
          renderGeoLocationBars();
        });
        setTimeout(() => { try { zInp?.focus(); zInp?.select(); } catch {} }, 20);
        return;
      }

      if (!hasActiveZip) {
        host.innerHTML = '<div class="geo-pill-bar">' +
          '<div class="geo-pill-left"><span>📍 ' + escH(L('Browsing all indexed areas', '当前浏览全部已收录区域', 'Explorando todas las zonas indexadas')) + '</span></div>' +
          '<div class="geo-pill-actions">' +
            '<button type="button" class="geo-act" data-geo-edit="' + hostId + '">' + escH(L('Set ZIP', '输入邮编', 'Elegir ZIP')) + '</button>' +
            (hasGpsApi ? '<button type="button" class="geo-act" data-geo-precise="' + hostId + '">◎ ' + escH(L('Use Precise Location', '使用精确定位', 'Usar ubicación precisa')) + '</button>' : '') +
          '</div></div>';
      } else {
        const isShop = hostId === 'shopGeoLocBar';
        const zipActiveInShop = isShop && (((shopLocQuickInput?.value || '').trim() === activeGeo.postalCode) || shopSelectedZips.has(activeGeo.postalCode));
        const cityZip = [activeGeo.city, activeGeo.postalCode].filter(Boolean).join(' ');
        const shopTogglesHtml = isShop
          ? ('<button type="button" class="geo-act' + (zipActiveInShop ? ' on' : '') + '" data-geo-scope-zip="' + hostId + '">ZIP ' + escH(activeGeo.postalCode) + '</button>' +
             '<button type="button" class="geo-act' + (!zipActiveInShop ? ' on' : '') + '" data-geo-scope-all="' + hostId + '">' + escH(L('All Indexed Areas', '全部已收录区域', 'Todas las zonas')) + '</button>' +
             (shopDetectedZipZeroLocal ? '<button type="button" class="geo-act on" data-geo-sync-detected="' + hostId + '">⟳ ' + escH(L('Sync listings for ' + cityZip, '同步 ' + cityZip + ' 房源', 'Sincronizar ' + cityZip)) + '</button>' : ''))
          : '';
        host.innerHTML = '<div class="geo-pill-bar">' +
          '<div class="geo-pill-left"><span>' + escH(formatGeoHeadline(activeGeo)) + '</span></div>' +
          '<div class="geo-pill-actions">' +
            shopTogglesHtml +
            '<button type="button" class="geo-act" data-geo-edit="' + hostId + '">' + escH(L('Change ZIP', '更换邮编', 'Cambiar ZIP')) + '</button>' +
            (hasGpsApi && activeGeo.mode !== 'gps' ? '<button type="button" class="geo-act" data-geo-precise="' + hostId + '">◎ ' + escH(L('Use Precise Location', '使用精确定位', 'Usar ubicación precisa')) + '</button>' : '') +
            '<button type="button" class="geo-act mut" data-geo-clear="' + hostId + '" title="' + escH(L('Clear ZIP & browse all areas', '清除邮编并浏览全部区域', 'Limpiar ZIP y ver todas las zonas')) + '">× ' + escH(L('Clear', '清除', 'Limpiar')) + '</button>' +
          '</div></div>';
      }

      host.querySelector('[data-geo-edit]')?.addEventListener('click', () => {
        geoEditingHostId = hostId;
        renderGeoLocationBars();
      });
      host.querySelector('[data-geo-clear]')?.addEventListener('click', () => {
        clearDetectedGeo();
      });
      host.querySelector('[data-geo-scope-zip]')?.addEventListener('click', () => {
        if (!activeGeo || !activeGeo.postalCode) return;
        if (shopLocQuickInput) shopLocQuickInput.value = activeGeo.postalCode;
        shopSelectedZips.clear();
        shopScopeMode = 'zip';
        renderGeoLocationBars();
        loadShopCatalog({ keepZipWhenEmpty: true });
      });
      host.querySelector('[data-geo-scope-all]')?.addEventListener('click', () => {
        if (shopLocQuickInput) shopLocQuickInput.value = '';
        shopSelectedZips.clear();
        shopSelectedCities.clear();
        shopScopeMode = 'all';
        renderGeoLocationBars();
        loadShopCatalog({ keepZipWhenEmpty: true });
      });
      host.querySelector('[data-geo-sync-detected]')?.addEventListener('click', (ev) => {
        if (!activeGeo || !activeGeo.postalCode) return;
        syncRealZipAndReload(activeGeo.postalCode, document.getElementById('shopGeoBannerMsg'), ev.currentTarget);
      });
      host.querySelector('[data-geo-precise]')?.addEventListener('click', async (ev) => {
        const btn = ev.currentTarget;
        if (btn) btn.disabled = true;
        const gps = await resolveBrowserGpsLocation(8000);
        if (btn) btn.disabled = false;
        if (gps && (gps.postalCode || (gps.lat != null && gps.lon != null))) {
          applyDetectedGeoDefaults(gps, { userInitiated: true });
        } else {
          const ipFallback = await fetchIpLocation();
          if (ipFallback && (ipFallback.postalCode || (ipFallback.lat != null && ipFallback.lon != null))) {
            applyDetectedGeoDefaults(ipFallback, { userInitiated: true });
          } else {
            renderGeoLocationBars();
          }
        }
      });
    });
  }

  async function initDefaultLocationDetection() {
    const hasExplicitUrlOrFilterZip = Boolean(
      initZip.length === 5 ||
      /\b\d{5}\b/.test(initQueryAddr) ||
      ((shopLocQuickInput?.value || '').trim().length > 0) ||
      shopSelectedZips.size > 0
    );
    if (hasExplicitUrlOrFilterZip) {
      geoResolvedOnce = true;
      renderGeoLocationBars();
      return;
    }
    if (geoClearedByUser) {
      geoResolvedOnce = true;
      renderGeoLocationBars();
      return;
    }

    let permState = 'prompt';
    try {
      if (navigator.permissions && typeof navigator.permissions.query === 'function') {
        const p = await navigator.permissions.query({ name: 'geolocation' });
        if (p && typeof p.state === 'string') permState = p.state;
      }
    } catch {}

    // Never trigger an unsolicited browser permission prompt on load: only call GPS automatically when already 'granted'.
    if (permState === 'granted') {
      const gps = await resolveBrowserGpsLocation(4500);
      if (gps && (gps.postalCode || (gps.lat != null && gps.lon != null))) {
        geoResolvedOnce = true;
        applyDetectedGeoDefaults(gps, { userInitiated: false });
        return;
      }
    }

    const ipGeo = await fetchIpLocation();
    geoResolvedOnce = true;
    if (ipGeo && (ipGeo.postalCode || (ipGeo.lat != null && ipGeo.lon != null))) {
      applyDetectedGeoDefaults(ipGeo, { userInitiated: false });
    } else {
      // Offline / unresolvable: keep ZERO hardcoded fallback ZIPs
      renderGeoLocationBars();
    }
  }

  window.__initMatchStudio = initMatchStudio;
  applyShopStaticLabels();
  if (uiState.lang !== CUR_LANG) {
    uiState = profileStore.saveUi({ lang: CUR_LANG });
    applyStaticUiLabels();
    applyDomTranslations();
  }
  if (initTab === 'match' || openMatchByRoute) initMatchStudio();
  initDefaultLocationDetection();
})();
"###;

const JOB_TEMPLATE: &str = r#"<!doctype html><html lang="{{ html_lang }}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{{ t_job_title }}</title>{{ favicon_tags | safe }}
<style>body{margin:0;background:#f5f6f8;color:#111827;font:15px/1.55 Inter,system-ui,sans-serif;display:grid;place-items:center;min-height:100vh}.box{background:#fff;border:1px solid #e5e7eb;border-radius:16px;padding:28px 32px;max-width:540px;width:92%;box-shadow:0 10px 30px -18px rgba(0,0,0,.18)}.step{margin-top:16px;padding:11px 14px;background:#effcf8;border:1px solid #c7efe4;border-radius:10px;font-weight:700;color:#0f766e;font-size:13.8px;display:flex;align-items:center;gap:10px}.step.err{background:#fef2f2;border-color:#fecaca;color:#b91c1c}.spin{width:14px;height:14px;border-radius:50%;border:2.2px solid #99f6e4;border-top-color:#0f766e;animation:sp .8s linear infinite;flex:0 0 auto}.step.err .spin{display:none}@keyframes sp{to{transform:rotate(360deg)}}.netwarn{display:none;margin-top:12px;padding:10px 13px;background:#fffbeb;border:1px solid #fde68a;border-radius:10px;color:#92400e;font-size:12.8px;font-weight:600;line-height:1.45}.netwarn.on{display:block}.bgnote{margin:12px 0 0;font-size:12.5px;color:#6b7280}.back{display:none;margin-top:12px;color:#0f766e;font-weight:700;font-size:13.5px;text-decoration:none}</style></head>
<body><div class="box"><h1 style="font-size:20px;margin:0 0 6px">{{ t_job_heading }}</h1><p style="margin:0;color:#6b7280;font-size:14px">{{ t_job_subtitle }}</p><div class="step" id="stwrap"><span class="spin"></span><span id="st">{{ step }}</span></div><div class="netwarn" id="netWarn" role="status" aria-live="polite">{{ t_job_net_warn }}</div><p class="bgnote">{{ t_job_bg_note }}</p><a href="/" class="back" id="backLink">{{ t_job_back_link }}</a></div>
<script>
const id={{ job_id_json | safe }},slug={{ slug_json | safe }},rawSlug={{ raw_slug_json | safe }},addr={{ addr_json | safe }};
const RECENT_KEY='openestate:recent_reports:v1',PENDING_KEY='openestate:pending_reports:v1';
let curSlug=slug,curAddr=addr,netFailures=0,polling=false,pollTimer=null,finished=false;
function readPendingList(){
  try{
    const a=JSON.parse(localStorage.getItem(PENDING_KEY)||'[]');
    return Array.isArray(a)?a.filter(x=>x&&x.address&&(x.rawSlug||x.slug||x.jobId)):[];
  }catch{return[];}
}
function matchPending(x,targetSlug){
  return Boolean(x&&((id&&x.jobId===id)||(targetSlug&&(x.slug===targetSlug||x.rawSlug===targetSlug))||(slug&&(x.slug===slug||x.rawSlug===slug))||(rawSlug&&(x.rawSlug===rawSlug||x.slug===rawSlug))));
}
function savePendingState(targetSlug,targetAddr,stepTxt){
  try{
    const prev=readPendingList();
    const ex=prev.find(x=>matchPending(x,targetSlug));
    const entry={
      jobId:id||(ex&&ex.jobId)||null,
      address:targetAddr||addr,
      slug:targetSlug||slug||null,
      rawSlug:(ex&&ex.rawSlug)||rawSlug||slug,
      step:stepTxt||(ex&&ex.step)||null,
      startedAt:(ex&&ex.startedAt)||Date.now()
    };
    const rest=prev.filter(x=>!matchPending(x,targetSlug));
    localStorage.setItem(PENDING_KEY,JSON.stringify([entry,...rest].slice(0,12)));
  }catch{}
}
function clearPendingState(targetSlug){
  try{
    const rest=readPendingList().filter(x=>!matchPending(x,targetSlug));
    if(rest.length)localStorage.setItem(PENDING_KEY,JSON.stringify(rest));
    else localStorage.removeItem(PENDING_KEY);
  }catch{}
}
function syncRecentSlug(targetSlug,targetAddr){
  try{
    const prev=JSON.parse(localStorage.getItem(RECENT_KEY)||'[]').filter(x=>x&&x.slug&&x.address);
    const ex=prev.find(x=>x.slug===targetSlug);
    if(!ex||ex.value==null){
      const cleaned=prev.filter(x=>x.slug!==targetSlug&&x.slug!==slug&&x.slug!==rawSlug);
      localStorage.setItem(RECENT_KEY,JSON.stringify([{slug:targetSlug,address:targetAddr||addr,value:null,overall:null,verdict:'Generating in background…',pending:true,viewedAt:new Date().toISOString()},...cleaned].slice(0,12)));
    }
  }catch{}
}
savePendingState(slug,addr,document.getElementById('st')?document.getElementById('st').textContent:null);
syncRecentSlug(slug,addr);

function schedulePoll(ms){
  if(finished)return;
  clearTimeout(pollTimer);
  pollTimer=setTimeout(poll,ms);
}
async function fetchWithTimeout(url,ms){
  const c=new AbortController();
  const t=setTimeout(()=>c.abort(),ms);
  try{return await fetch(url,{cache:'no-store',signal:c.signal});}
  finally{clearTimeout(t);}
}
async function poll(){
  if(polling||finished)return;
  polling=true;
  const stEl=document.getElementById('st'),wrap=document.getElementById('stwrap'),netWarn=document.getElementById('netWarn');
  try{
    const lookupKeys=[...new Set([id,curSlug,rawSlug].filter(Boolean))];
    let j=null,got404=false;
    for(const k of lookupKeys){
      const r=await fetchWithTimeout('/api/job/'+encodeURIComponent(k)+'?t='+Date.now(),6000);
      if(r.ok){j=await r.json();break;}
      if(r.status===404){got404=true;continue;}
      throw new Error('HTTP_'+r.status);
    }
    if(!j&&got404&&(curSlug||rawSlug)){
      const rr=await fetchWithTimeout('/r/'+encodeURIComponent(curSlug||rawSlug)+'/report.json?t='+Date.now(),6000);
      if(rr.ok){
        const rep=await rr.json();
        const readySlug=(rep&&rep.meta&&rep.meta.slug)||curSlug||rawSlug;
        j={ready:true,done:true,slug:readySlug,address:(rep&&rep.meta&&rep.meta.displayAddress)||curAddr};
      }
    }
    netFailures=0;
    netWarn.classList.remove('on');
    if(j){
      if(j.error){
        finished=true;
        clearPendingState(j.slug||curSlug);
        wrap.classList.add('err');
        stEl.textContent=j.error==='ADDRESS_NOT_FOUND'?'Could not match that US street address — please check the street number, city, or ZIP code.':j.error;
        document.getElementById('backLink').style.display='inline-block';
        return;
      }
      if(j.slug)curSlug=j.slug;
      if(j.address)curAddr=j.address;
      if(j.step)stEl.textContent=j.step;
      savePendingState(curSlug,curAddr,j.step||stEl.textContent);
      if(curSlug)syncRecentSlug(curSlug,curAddr);
      if((j.ready||j.done)&&j.slug){
        finished=true;
        clearPendingState(j.slug);
        syncRecentSlug(j.slug,curAddr);
        stEl.textContent='Opening your property report…';
        location.replace('/r/'+encodeURIComponent(j.slug)+'/');
        return;
      }
    }
    schedulePoll(750);
  }catch{
    netFailures++;
    netWarn.classList.add('on');
    const backoff=netFailures===1?1500:netFailures===2?3000:5000;
    schedulePoll(backoff);
  }finally{
    polling=false;
  }
}
window.addEventListener('online',()=>{netFailures=0;schedulePoll(50);});
window.addEventListener('offline',()=>{const nw=document.getElementById('netWarn');if(nw)nw.classList.add('on');});
document.addEventListener('visibilitychange',()=>{if(document.visibilityState==='visible'){netFailures=0;schedulePoll(50);}});
window.addEventListener('pageshow',()=>{netFailures=0;schedulePoll(50);});
schedulePoll(500);
</script></body></html>"#;

const UNSUB_TEMPLATE: &str = r#"<!doctype html><html lang="{{ html_lang }}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{{ t_unsub_title }}</title>{{ favicon_tags | safe }}
<style>body{margin:0;background:#f5f6f8;color:#111827;font:15px/1.55 Inter,system-ui,sans-serif;display:grid;place-items:center;min-height:100vh}.box{background:#fff;border:1px solid #e5e7eb;border-radius:16px;padding:28px 32px;max-width:520px;width:92%;box-shadow:0 10px 30px -18px rgba(17,24,39,.18)}.actions{display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;margin-top:18px}.mbtn{display:inline-flex;align-items:center;gap:6px;background:#effcf8;border:1px solid #0f766e;color:#0f766e;font-weight:700;font-size:13.5px;text-decoration:none;padding:8px 14px;border-radius:9px}.mbtn:hover{background:#d1fae5}</style></head>
<body><div class="box"><h1 style="font-size:20px;margin:0 0 8px">{{ t_heading }}</h1>
<p style="margin:0 0 8px;color:#4b5563">{{ t_body }}</p>
<div class="actions"><a href="/" style="color:#4b5563;font-weight:600;text-decoration:none">{{ t_return_link }}</a><a class="mbtn" href="{{ manage_url }}">{{ t_manage_link }}</a></div></div></body></html>"#;

static JINJA_ENV: Lazy<Environment<'static>> = Lazy::new(|| {
    let mut env = Environment::new();
    env.add_template("home", HOME_TEMPLATE).unwrap();
    env.add_template("job", JOB_TEMPLATE).unwrap();
    env.add_template("unsub", UNSUB_TEMPLATE).unwrap();
    env
});

pub fn render_home_html(
    ai_active: bool,
    db_dialect: &str,
    db_driver: &str,
    smtp_active: bool,
    active_mls_names: &[String],
) -> String {
    render_home_html_for_lang(
        ai_active,
        db_dialect,
        db_driver,
        smtp_active,
        active_mls_names,
        "en-US",
    )
}

pub fn render_home_html_for_lang(
    ai_active: bool,
    db_dialect: &str,
    db_driver: &str,
    smtp_active: bool,
    active_mls_names: &[String],
    lang: &str,
) -> String {
    render_home_html_with_options(
        ai_active,
        db_dialect,
        db_driver,
        smtp_active,
        active_mls_names,
        lang,
        "Open Estate",
        "open-estate.local",
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_home_html_with_options(
    _ai_active: bool,
    _db_dialect: &str,
    _db_driver: &str,
    _smtp_active: bool,
    _active_mls_names: &[String],
    lang: &str,
    brand_name: &str,
    brand_domain: &str,
) -> String {
    let eff_brand = if brand_name.trim().is_empty() { "Open Estate" } else { brand_name.trim() };
    let eff_domain = if brand_domain.trim().is_empty() { "open-estate.local" } else { brand_domain.trim() };
    let brand_wordmark = format_brand_wordmark_html(eff_brand);
    let brand_json = safe_inline_json(
        &serde_json::to_string(&serde_json::json!({
            "name": eff_brand,
            "domain": eff_domain,
        }))
        .unwrap_or_else(|_| r#"{"name":"Open Estate","domain":"open-estate.local"}"#.into()),
    );
    let lang = resolve_lang_str(lang);
    let tmpl = JINJA_ENV.get_template("home").unwrap();
    let init_lang = bcp47_code(&lang);
    let zh = is_zh(&lang);
    let es = is_es(&lang);
    let pick = |en: &'static str, zh_s: &'static str, es_s: &'static str| -> &'static str {
        if zh {
            zh_s
        } else if es {
            es_s
        } else {
            en
        }
    };

    let bq3_a_str = if zh {
        "综合同套历史成交指数回推、特征修正可比成交与县税务评估生成真实估值与区间，并附历史涨跌压力测试。".to_string()
    } else if es {
        "Estimación Real y rango de valor calculados con historial de ventas repetidas, ventas comparables ajustadas y registros fiscales del condado, con escenarios históricos al alza y de caída.".to_string()
    } else {
        format!("{eff_brand}’s True Estimate and value range from repeat-sales history, adjusted comparable sales, and county tax rolls — plus historical upside and drawdown stress scenarios.")
    };

    let brand_name_json = safe_inline_json(
        &serde_json::to_string(eff_brand).unwrap_or_else(|_| "\"Open Estate\"".into()),
    );
    let rendered_home_js = HOME_CLIENT_JS.replace("__HOME_BRAND_NAME_JSON__", &brand_name_json);
    tmpl.render(context! {
        favicon_tags => FAVICON_HEAD_TAGS.as_str(),
        logo_mark => logo_mark_img(36),
        brand_wordmark => brand_wordmark,
        brand_name => eff_brand,
        brand_domain => eff_domain,
        brand_json => brand_json,
        html_lang => init_lang,
        is_zh => zh,
        is_es => es,
        t_home_title => t_brand(&lang, "home-title", eff_brand),
        t_deal_room => t_brand(&lang, "deal-room-title", eff_brand).split('—').next().unwrap_or("Deal Room").trim(),
        t_hero_heading => t(&lang, "home-hero-heading"),
        hero_subtitle => pick(
            "Instant valuation, natural hazard & environmental screening, real monthly cost, public schools & neighborhood life, 3D LiDAR roof & sun model, and AI inspection & title review for any US home.",
            "为美国任一住宅提供真实估值、每月真实持有成本、自然灾害与环境筛查、公立学区与生活圈、3D 屋顶日照模型、传统堪舆风水及验房披露文件 AI 审阅。",
            "Estimación Real, costo mensual completo, riesgos naturales y ambientales, escuelas públicas y vecindario, modelo 3D LiDAR de techo y sol, y revisión con IA de inspecciones y título para cualquier vivienda en EE. UU."
        ),
        t_tab_report => t(&lang, "tab-report"),
        t_tab_shop => pick("Shop Homes", "条件选房", "Comprar Casas"),
        t_tab_match => t(&lang, "tab-match"),
        t_tab_alerts => t(&lang, "tab-alerts"),
        t_search_placeholder => t(&lang, "search-placeholder"),
        t_generate_btn => t(&lang, "generate-report-btn"),
        area_onsale_label => pick("See onsale property in your area:", "查看您所在区域的在售房源：", "Ver propiedades en venta en su zona:"),
        area_zip_placeholder => pick("5-digit US ZIP", "输入 5 位美国邮编", "Código ZIP de 5 dígitos"),
        area_zip_btn => pick("Find homes", "查找房源", "Buscar casas"),
        area_geo_btn => pick("📍 Use my location", "📍 使用当前位置", "📍 Usar mi ubicación"),
        bq_kicker => pick("What your report answers", "一份报告解答买房 5 大核心问题", "Lo que responde su informe"),
        bq_heading => pick(
            "Know the real monthly cost and hidden risks before you make an offer",
            "下 Offer 前，先算清真实月供与潜在隐患",
            "Conozca el costo mensual real y los riesgos ocultos antes de ofertar"
        ),
        bq1_q => pick("What will it really cost each month?", "每个月真正要花多少钱？", "¿Cuánto costará realmente cada mes?"),
        bq1_a => pick(
            "Mortgage principal & interest, property taxes reassessed to your purchase price (never the seller’s old bill), homeowners & hazard insurance, maintenance reserve, and HOA — compared against market rent.",
            "涵盖房贷本息、按您成交价重估的房产税（绝非卖家旧税单）、房屋与灾害保险、维护预留金及 HOA 月费，并与周边市场月租直观对比。",
            "Capital e intereses hipotecarios, impuesto predial reevaluado al precio de compra (no el recibo antiguo del vendedor), seguro de hogar y riesgos, reserva de mantenimiento y HOA — comparado con la renta de mercado."
        ),
        bq2_q => pick("What could go wrong — or wasn’t disclosed?", "有哪些潜在隐患或未披露问题？", "¿Qué podría salir mal o no fue revelado?"),
        bq2_a => pick(
            "FEMA flood zone, USGS earthquake shaking & active faults, wildfire zones, EPA radon, Superfund & cleanup sites, water safety & traffic noise — plus upload inspection, disclosure, HOA, or title PDFs to cross-check against public records.",
            "逐项核查 FEMA 洪泛区、USGS 地震动与活断层、山火等级区、EPA 氡气、超级基金与污染场地、水质记录与交通噪音；支持上传验房、卖家披露、HOA 与产权 PDF 与公开记录交叉核对。",
            "Zona de inundación FEMA, sacudida sísmica y fallas activas USGS, incendios forestales, radón EPA, sitios Superfund, agua potable y ruido vial — más carga de PDFs de inspección, divulgaciones, HOA o título para cruzarlos con registros públicos."
        ),
        bq3_q => pick("Is the asking price right?", "挂牌价格合理吗？", "¿El precio de venta es justo?"),
        bq3_a => bq3_a_str,
        bq4_q => pick("What is it like to live there?", "住在这里日常体验如何？", "¿Cómo es vivir allí día a día?"),
        bq4_a => pick(
            "Assigned public schools from official state academic records, EPA walkability, groceries, parks & transit within a mile, drive times to your own places, and city crime context.",
            "采用州政府官方公立学校学业数据、EPA 步行便利指数、1 英里内超市/公园/公交统计、自选常去地点车程及城市治安背景。",
            "Escuelas públicas asignadas con datos académicos estatales oficiales, caminabilidad EPA, supermercados, parques y transporte a 1 milla, tiempos de traslado a sus lugares importantes y contexto de seguridad."
        ),
        bq5_q => pick("What can you do with the property?", "房屋与地块有哪些改造潜力？", "¿Qué se puede hacer con la propiedad?"),
        bq5_a => pick(
            "Interactive 3D LiDAR model of roof planes, tree overhang & year-round sun/shadow, zoning & ADU/lot context, a 50-second video tour, and side-by-side Deal Room comparison.",
            "基于激光雷达（LiDAR）生成交互式 3D 屋顶坡面、树木遮挡与全年日照阴影模型，结合分区与 ADU 规则、50 秒视频导览及多房源对比室。",
            "Modelo 3D LiDAR interactivo de planos de techo, árboles y sol/sombra todo el año, zonificación y potencial de ADU, video tour de 50 segundos y comparación lado a lado en Sala de Comparación."
        ),
        anat_kicker => pick("Inside every report", "报告内容概览", "Dentro de cada informe"),
        anat_heading => pick("What you get for any US street address", "输入任一美国街道地址即可获得", "Lo que obtiene para cualquier dirección en EE. UU."),
        anat_sub => pick(
            "Every figure is computed from official public records and cited sources — nothing invented.",
            "所有数据均实时取自联邦、州与县级公开记录并附原始出处，绝不虚构任何数字。",
            "Cada cifra proviene de registros públicos oficiales con fuentes citadas — sin datos inventados."
        ),
        anat1_t => pick("1 · All-in monthly cost breakdown vs. market rent", "1 · 全口径每月持有成本 vs. 市场月租", "1 · Desglose del costo mensual real vs. renta de mercado"),
        anat1_d => pick(
            "Mortgage P&I, purchase-price reassessed property tax, homeowners & seismic/flood insurance, maintenance reserve, and HOA.",
            "房贷本息、按成交价重估地税、房屋与地震/洪水险、维护预留金与 HOA 明细拆解。",
            "Hipoteca, impuesto predial reevaluado al comprar, seguros, reserva de mantenimiento y cuota HOA."
        ),
        anat2_t => pick("2 · Parcel-level hazard, environmental & document review", "2 · 地块级灾害、环境筛查与披露文件 AI 核验", "2 · Riesgos de la parcela, ambiente y revisión de documentos con IA"),
        anat2_d => pick(
            "FEMA, USGS, CAL FIRE / USFS, EPA Superfund, SDWIS water & US DOT noise — plus AI cross-check of uploaded inspection, disclosure & title PDFs.",
            "覆盖 FEMA、USGS、山火、EPA 超级基金、饮用水与道路噪音，并可上传验房/披露/产权 PDF 交叉比对。",
            "FEMA, USGS, incendios, EPA Superfund, agua SDWIS y ruido US DOT — más cruce de PDFs de inspección, divulgación y título."
        ),
        anat3_t => pick("3 · True Estimate, adjusted comps & downside stress test", "3 · 真实估值、可比成交修正与下行压力测试", "3 · Estimación Real, comparables ajustados y prueba de estrés"),
        anat3_d => pick(
            "Repeat-sales indexation, nearby sold comps, county tax roll history, and 5-year historical return bands.",
            "融合同套复售指数、周边近期成交、县评估记录与 5 年历史收益分布区间。",
            "Índice de ventas repetidas, cierres cercanos, historial fiscal del condado y bandas históricas a 5 años."
        ),
        anat4_t => pick("4 · 3D LiDAR lot model, 50s video tour & neighborhood life", "4 · 3D 激光雷达模型、50 秒视频导览与生活圈", "4 · Modelo 3D LiDAR, video tour de 50s y vida diaria"),
        anat4_d => pick(
            "USGS 3DEP roof/tree/sun massing, official state school data, EPA walkability, and custom commute routing.",
            "USGS 3DEP 屋顶/树木/日照建模、州政府官方学区数据、EPA 步行便利指数与自定义通勤测算。",
            "Volumetría USGS 3DEP de techo/árboles/sol, datos escolares oficiales, caminabilidad EPA y rutas propias."
        ),
        anat_foot => pick("Federal, state & county public records", "联邦、州与县级权威公开记录", "Registros públicos federales, estatales y del condado"),
        anat_cta => pick("Try an address above ↑", "在上方输入地址体验 ↑", "Probar una dirección arriba ↑"),
        alerts_csv_desc => pick(
            "Paste a public MLS or county ZIP CSV export to seed or refresh local homes immediately:",
            "粘贴公开房源列表 CSV 导出内容，即可立即导入或刷新本地房源库：",
            "Pegue un archivo CSV público de viviendas por código ZIP para cargar o actualizar casas al instante:"
        ),
        footer_note => pick(
            "Public-record due-diligence research for US homes · Not a licensed appraisal or physical inspection",
            "基于美国公开记录的购房尽职调查研究 · 不构成执业估价或现场验房报告",
            "Investigación con registros públicos de EE. UU. · No sustituye una tasación oficial ni una inspección física"
        ),
        t_match_label => t(&lang, "match-label"),
        t_match_placeholder => t(&lang, "match-placeholder"),
        t_match_chip_1 => t(&lang, "match-chip-1"),
        t_match_chip_2 => t(&lang, "match-chip-2"),
        t_match_chip_3 => t(&lang, "match-chip-3"),
        t_status_auto => t(&lang, "match-status-auto"),
        t_status_active => t(&lang, "match-status-active"),
        t_status_both => t(&lang, "match-status-both"),
        t_status_sold => t(&lang, "match-status-sold"),
        t_match_alert_btn => t(&lang, "match-alert-btn"),
        t_match_search_btn => t(&lang, "match-search-btn"),
        t_alerts_title => t(&lang, "alerts-title"),
        t_alerts_subtitle => t(&lang, "alerts-subtitle"),
        t_alerts_email => t(&lang, "alerts-email-label"),
        t_alerts_zips => t(&lang, "alerts-zips-label"),
        t_alerts_req => t(&lang, "alerts-req-label"),
        t_alerts_req_ph => t(&lang, "alerts-req-placeholder"),
        t_alerts_notify_new => t(&lang, "alerts-notify-new"),
        t_alerts_notify_sold => t(&lang, "alerts-notify-sold"),
        t_alerts_freq_label => t(&lang, "alerts-freq-label"),
        t_alerts_freq_instant => t(&lang, "alerts-freq-instant"),
        t_alerts_freq_daily => t(&lang, "alerts-freq-daily"),
        t_alerts_freq_weekly => t(&lang, "alerts-freq-weekly"),
        t_alerts_manage_btn => t(&lang, "alerts-manage-btn"),
        t_alerts_submit_btn => t(&lang, "alerts-submit-btn"),
        t_alerts_saved_title => t(&lang, "alerts-saved-title"),
        t_alerts_saved_summary => t(&lang, "alerts-saved-summary"),
        t_alerts_sync_btn => t(&lang, "alerts-sync-btn"),
        t_alerts_csv_summary => t(&lang, "alerts-csv-summary"),
        t_alerts_csv_btn => t(&lang, "alerts-csv-btn"),
        i18next_js => safe_inline_js(I18NEXT_JS),
        init_lang_json => serde_json::to_string(init_lang).unwrap_or_else(|_| "\"en\"".into()),
        lang_cookie_json => serde_json::to_string(LANG_COOKIE_NAME).unwrap_or_else(|_| "\"open_estate_lang\"".into()),
        i18n_en_json => safe_inline_json(&localize_i18n_json_for_brand(I18N_EN_JSON, eff_brand, eff_domain)),
        i18n_zh_json => safe_inline_json(&localize_i18n_json_for_brand(I18N_ZH_JSON, eff_brand, eff_domain)),
        i18n_es_json => safe_inline_json(&localize_i18n_json_for_brand(I18N_ES_JSON, eff_brand, eff_domain)),
        home_client_js => rendered_home_js,
        core_ui_js => safe_open_estate_core_js(),
    })
    .unwrap_or_default()
}

pub fn render_job_html(
    job_id: &str,
    address: &str,
    slug: &str,
    raw_slug: &str,
    step: &str,
) -> String {
    render_job_html_for_lang(job_id, address, slug, raw_slug, step, "en-US")
}

pub fn render_job_html_for_lang(
    job_id: &str,
    address: &str,
    slug: &str,
    raw_slug: &str,
    step: &str,
    lang: &str,
) -> String {
    render_job_html_with_brand(job_id, address, slug, raw_slug, step, lang, "Open Estate")
}

pub fn render_job_html_with_brand(
    job_id: &str,
    address: &str,
    slug: &str,
    raw_slug: &str,
    step: &str,
    lang: &str,
    brand_name: &str,
) -> String {
    let eff_brand = if brand_name.trim().is_empty() { "Open Estate" } else { brand_name.trim() };
    let lang = resolve_lang_str(lang);
    let tmpl = JINJA_ENV.get_template("job").unwrap();
    tmpl.render(context! {
        favicon_tags => FAVICON_HEAD_TAGS.as_str(),
        html_lang => short_code(&lang),
        address => address,
        step => humanize_progress_step(step),
        t_job_title => t_brand_args(&lang, "job-title", eff_brand, &[("address", address)]),
        t_job_heading => t_args(&lang, "job-heading", &[("address", address)]),
        t_job_subtitle => t(&lang, "job-subtitle"),
        t_job_net_warn => t_brand(&lang, "job-net-warn", eff_brand),
        t_job_bg_note => t_brand(&lang, "job-bg-note", eff_brand),
        t_job_back_link => t(&lang, "job-back-link"),
        job_id_json => serde_json::to_string(job_id).unwrap_or_else(|_| "\"\"".into()),
        slug_json => serde_json::to_string(slug).unwrap_or_else(|_| "\"\"".into()),
        raw_slug_json => serde_json::to_string(raw_slug).unwrap_or_else(|_| "\"\"".into()),
        addr_json => serde_json::to_string(address).unwrap_or_else(|_| "\"\"".into()),
    })
    .unwrap_or_default()
}

pub fn render_unsub_html(found: bool, email: &str, zips: &[String]) -> String {
    render_unsub_html_for_lang(found, email, zips, "en-US")
}

pub fn render_unsub_html_for_lang(found: bool, email: &str, zips: &[String], lang: &str) -> String {
    render_unsub_html_with_brand(found, email, zips, lang, "Open Estate")
}

pub fn render_unsub_html_with_brand(
    found: bool,
    email: &str,
    zips: &[String],
    lang: &str,
    brand_name: &str,
) -> String {
    render_unsub_html_with_token_and_brand(found, email, zips, lang, brand_name, None)
}

pub fn render_unsub_html_with_token_and_brand(
    found: bool,
    email: &str,
    zips: &[String],
    lang: &str,
    brand_name: &str,
    token: Option<&str>,
) -> String {
    let eff_brand = if brand_name.trim().is_empty() { "Open Estate" } else { brand_name.trim() };
    let lang = resolve_lang_str(lang);
    let tmpl = JINJA_ENV.get_template("unsub").unwrap();
    let zips_joined = zips.join(", ");
    let heading = if found {
        t(&lang, "unsub-found-heading")
    } else {
        t(&lang, "unsub-missing-heading")
    };
    let body = if found {
        t_args(
            &lang,
            "unsub-found-body",
            &[("email", email), ("zips", &zips_joined)],
        )
    } else {
        t(&lang, "unsub-missing-body")
    };
    let clean_tok = token.map(str::trim).filter(|s| !s.is_empty());
    let manage_url = match (found, clean_tok) {
        (true, Some(tok)) => format!("/alerts?token={}", urlencoding::encode(tok)),
        _ => "/alerts".to_string(),
    };
    tmpl.render(context! {
        favicon_tags => FAVICON_HEAD_TAGS.as_str(),
        html_lang => short_code(&lang),
        t_unsub_title => t_brand(&lang, "unsub-title", eff_brand),
        t_heading => heading,
        t_body => body,
        t_return_link => t_brand(&lang, "unsub-return-link", eff_brand),
        t_manage_link => t(&lang, "unsub-manage-link"),
        manage_url => manage_url,
    })
    .unwrap_or_default()
}
