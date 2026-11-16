---
name: ai-analyst
description: Grounded dynamic LLM synthesis and interactive Q&A over a Open Estate (open-estate.local) us-property-report JSON using the OpenAI Responses API (`gpt-4.1` by default, falling back to `gpt-4o-mini`). Produces executive verdict, per-section analyst notes, 3x3 risk matrix, negotiation levers, seller questions, Chinese-only feng shui synthesis, and grounded buyer Q&A.
---

# AI Analyst (`ai-analyst`)

Synthesizes all deterministic modules of a `us-property-report` (`facts`, `valuation`, `market`, `hazards`, `environment`, `amenities`, `access`, `civic`, `fengshui`) into concise, institutional-grade commentary and interactive Q&A — strictly grounded on cited numbers in the report with Fair Housing guardrails. The LLM writes commentary only; it never sets the Open Estate Score or any other score.

Implementation (Rust, inside `open-estate-server`): `src/ai/analyst.rs` (`run_ai_analyst`, with a deterministic fallback), `src/ai/ask.rs` (`ask_property_question`), `src/ai/brief.rs` (compact Fair-Housing-scrubbed brief of the report sent to the model), `src/ai/translate.rs` (`translate_report`), `src/ai/llm.rs` (Responses API client) and `src/guardrails.rs`.

## Model & Provider Configuration

`src/ai/llm.rs` calls the OpenAI Responses API wire format (`POST {OPENAI_BASE_URL}/responses`) with strict structured outputs (JSON schema), a model fallback chain, and a SHA-256 cache over (schema, instructions, prompt) kept in memory and in the database (`llm_cache`):

| Env var | Default | Purpose |
|---|---|---|
| `OPENAI_API_KEY` | *(optional — enables AI)* | Bearer token for Responses API (never hardcoded or logged) |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | Responses API gateway (`POST {base}/responses`) |
| `LLM_MODEL` | `gpt-4.1` | Primary model (`name` or `name@effort`) |
| `LLM_EFFORT` | `none` | Reasoning effort for reasoning-capable models (`LLM_QA_EFFORT` defaults to `medium` when reasoning is enabled) |
| `LLM_FALLBACK_MODEL` | `gpt-4o-mini` | Automatic fallback chain on HTTP/timeout/schema failure |

## Multi-Layer AI Security & Purpose-Lock Guardrails (`src/guardrails.rs`, `src/ai/llm.rs`)

- **Zero Direct LLM Proxy Access**: every LLM call must name an allowlisted purpose (`ALLOWED_AI_PURPOSES`: `report_synthesis`, `property_qa`, `inspection_review`, `title_review`, `hoa_review`, `packet_review`, `repair_addendum`, `cre_review`, `report_translation`, `property_search`, `property_alert`, `walkthrough_slides`, `artifact_canvas`, `agent_chat`, `document_ocr`, `condition_assessment`) or it fails with `403 DIRECT_LLM_ACCESS_DENIED`, and an immutable purpose-lock preamble is prepended to the instructions. Direct proxy routes (`/v1/*`, `/responses`, `/chat/completions`, `/llm-proxy/*`, `/openai/*`, `/api/llm`, … — `DIRECT_LLM_ROUTE_RE`) return `403 DIRECT_LLM_BLOCKED`, and upstream URLs/keys/model chains are never exposed on `/api/health`, error payloads, or rendered HTML (`sanitize_report_for_html`).
- **Chinese-Only 风水 Synthesis**: `ai.fengShui` synthesis prompts produce Simplified Chinese (`zh`) commentary exclusively so the `#fengshui` section serves Chinese-reading buyers cleanly without duplicate English prose.
- **Server-Only Context Grounding**: HTTP endpoints (`POST /api/ask`, `/api/inspect`, `/api/title-review`, `/api/hoa-review`, `/api/packet-review`, `/api/repair-addendum`, `/api/condition-score`, `/api/cre-review`, `/api/translate`, `/api/report/:slug/slides`, `/api/report/:slug/artifact`) enforce strict top-level key allowlists (`enforce_allowed_body_keys`), same-origin checks (`verify_same_origin`: `Origin` / `Sec-Fetch-Site`), cached-report resolution (`resolve_cached_report`), `X-OpenEstate-Client-Id` isolation, and per-IP/subnet rate + concurrency limits (`HttpDefenseShield`), loading the report and saved document reviews exclusively from server storage by `slug`.
- **Anti-Hijacking & Coding Refusal**: Pre-LLM deterministic filters (`validate_buyer_question_with_context`, `sanitize_chat_history`, `validate_and_scrub_property_document`), `<untrusted_buyer_question>` / `<untrusted_document_text>` isolation (`wrap_untrusted_block`), the Q&A schema (`ask_schema`: `{ onTopic, answer, citations, followUps }`), and post-LLM output checks (`inspect_output_guardrail`) block coding/scripting/SQL/algorithm requests, prompt injections, fake document uploads, and off-topic tasks in 0 ms without consuming LLM tokens.

## Usage

### Through the server
- **Report synthesis** runs automatically: after `build_full_report` saves the deterministic report, it calls `run_ai_analyst` when `OPENAI_API_KEY` is set, unless the request passed `ai=0` (query) or `"ai": false` (JSON body). The result fills `report.ai` and `summary.narrative`; `meta.aiPending` is true until it finishes, and opening a report with pending AI work resumes it in the background.
- **Grounded follow-up question** (the report page's Q&A box sends this from the browser):
  ```http
  POST /api/ask
  Content-Type: application/json

  {"slug": "<report-slug>",
   "question": "How does the estimated value compare to nearby comps and property taxes?"}
  ```
  Returns `{ ok, answer, citations, followUps, refused, model, effort, fallbackUsed, cached, ms }`. Optional `history` carries earlier turns; findings the same browser uploaded through `/api/inspect` and `/api/title-review` (keyed by `X-OpenEstate-Client-Id`) are added to the context. Like the other AI POST endpoints it accepts only same-origin JSON requests: scripted user agents (`curl/`, `python-requests`, …) get `403 AUTOMATED_SCRAPER_BLOCKED`.

### Programmatic (Rust)
```rust
use open_estate::ai::{ask_property_question, run_ai_analyst};

let ai = run_ai_analyst(&http, &env, &report, None).await?;            // ModuleOutput { data, sources, warnings }
let qa = ask_property_question(&http, &env, &report, "What should I inspect first?",
                               None, None, None, None).await?;          // history, inspection, title review, cache
```
