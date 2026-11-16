---
name: property-search
description: Natural-language home finder, dialect-portable SQLite WAL + MySQL/HeatWave database (`sqlx` + `sea-query`), background ZIP-code active/sold home sync, and `lettre` SMTP email alert subscriptions powered by `gpt-4o-mini` (with deterministic heuristic fallback).
---

# Property Search & Area Alerts (`property-search`)

Allows homebuyers to describe their ideal home in plain language (price, ZIP/city, beds/baths, square footage, lot size, HOA, public school data, wildfire/flood filters, features), translates those requirements into parameterized SQL via `gpt-4o-mini` (with a deterministic heuristic parser fallback), ranks matches from the database, and lets buyers subscribe to 5-digit ZIP codes for continuous background active/sold listing ingestion and SMTP email alerts.

## Architecture (Rust, `open-estate-server`)

1. **Portable `DbStore` Database Layer (`src/db/mod.rs`, `src/db/schema.rs`, `src/db/migrate.rs`)**
   - Uses Rust `sqlx` supporting **SQLite WAL** (`--db-file` / `:memory:`) and **MySQL 8.4/9.0 / OCI HeatWave** (`MYSQL_URL` / `DATABASE_URL` with TLS/SSL options), plus L1 `moka` caches for reports and LLM outputs.
   - Dialect-portable DDL (`sqlite_ddl` / `mysql_ddl`), live streaming migration (`open-estate-server migrate --from <src> --to <dst>`), HeatWave-safe upserts (`ON CONFLICT DO UPDATE` on SQLite vs `AS new_row ON DUPLICATE KEY UPDATE` on MySQL), and 100% bound parameters via `sea_query`.

2. **Query Planner, Fair Housing Guardrails & Match Ranker (`src/search/matcher.rs`, `src/guardrails.rs`)**
   - Primary model chain: `gpt-4o-mini` (configurable via `LLM_MATCH_MODEL` and `LLM_FALLBACK_MODEL`), with deterministic heuristic parsing (`parse_requirements_heuristic`) when no LLM is configured.
   - Stage 1 validates and translates buyer requirements (`parse_requirements_to_query`) into `SearchFilters`, compiled deterministically by `compile_filters_to_sql` (`sea_query`, zero SQL-literal leakage; wildfire exclusion checks normalized `wildfire_zone` labels case-insensitively).
   - Stage 2 computes deterministic baseline scores (`score_listing_deterministic`) and optionally ranks candidates (`rank_candidates_with_llm`) into `matchScore`, `matchTier`, grounded `whyItMatches`, and `tradeoffs`. Under the two-score compliance rules (`us-property-report/references/compliance.md` and `scoring.md`), the LLM never sets the Open Estate Score or any deterministic Your Match score.
   - Strict Fair Housing Act and prompt-injection guardrails (`validate_search_requirements`, demographic keyword scrubbing + `FAIR_HOUSING_NOTICE_TEXT`, `<untrusted_buyer_requirements>` XML isolation, `inspect_output_guardrail`).

3. **Background ZIP Watcher & Report Harvester (`src/search/sync.rs`, `src/search/alerts.rs`)**
   - Runs in `open-estate-server`'s background loop (`run_background_sync_cycle`, default every 15 min via `--sync-interval-min`) and on-demand via `POST /api/sync`.
   - Pluggable multi-source ingestion for **for-sale**, **pending**, and **sold** homes per ZIP (`sync_connector_status`):
     - **RESO Web API 2.0** (`RESO_API_URL` + `RESO_BEARER_TOKEN`), **SimplyRETS** (`SIMPLYRETS_AUTH`), **ATTOM** (`ATTOM_API_KEY`)
     - **Operator HTTP / CSV feeds** (`PUBLIC_LISTING_FEED_URL` and `POST /api/listings/ingest` via `csv`)
     - **Open Estate Reports & Comps** (`extract_listings_from_report`, `ingest_report_into_db`, `seed_reports_from_dir` from `cached_reports` / `reports/`, excluding synthetic benchmarks via `is_synthetic_comp_entry`) — extracts `wildfire_zone` from CAL FIRE FHSZ / USFS WHP and `walk_score` strictly on a 0–100 scale (`facts.walkScore.walk`, else `civic.scores.walkability`, never the 1–20 EPA index).
   - School context uses **public state and federal data only** (NCES EDGE directory and official state/federal performance data via CAASPP / EDFacts with source and year; GreatSchools, Niche, SchoolDigger, and the HUD School Proficiency Index are never scored, and student–teacher ratios are never shown as letter grades — see `us-property-report/references/compliance.md`).

4. **SMTP Email Alerts (`src/search/mailer.rs` + `src/search/alerts.rs`)**
   - Async MIME/SMTP client built on `lettre` (`Tokio1Executor` + `rustls`), supporting `SMTP_SECURE=none` (local testing), `STARTTLS` (ports 587/2525), implicit TLS (port 465), `AUTH PLAIN` / `AUTH LOGIN`, `minijinja` + `html_escape` multipart HTML + plain-text bodies, and RFC 8058 one-click `List-Unsubscribe` + `List-Unsubscribe-Post` headers.
   - Idempotent deduplication via `subscription_notifications.event_key` so subscribers are never emailed twice for the same listing state.

## HTTP Endpoints & Tests
- Endpoints (`src/web/server.rs`): `POST /api/match`, `GET /api/listings`, `POST /api/listings/ingest`, `GET /api/watched-zips`, `POST /api/sync`, `GET|POST /api/subscriptions`, `GET|POST /api/subscriptions/unsubscribe`.
- Tests: `tests/db_heatwave_search_test.rs` and `tests/phase0_fixes_test.rs`.
