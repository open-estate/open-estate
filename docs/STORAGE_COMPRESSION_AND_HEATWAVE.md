# Open Estate Storage Compression, `v3` Protobuf Schema, Runtime SSR & OCI MySQL HeatWave Reference

This engineering reference documents Open Estate's storage and query architecture: the clean-slate `v3` **44-table Protobuf-first (`openestate.db.v1`) relational schema** (zero `JSON`, zero `LONGTEXT`, vertical 1:1 domain slicing, 27-section `cached_report_sections` zstd frames, `BINARY(32)` content keys, and HeatWave Rapid-safe `DIV/MOD` bitmasks), `report_zstd` compression (`256 KiB` trained Zstandard dictionary + `ZipNorm` + `Geo6dec` + mirror deduplication + runtime HTML SSR), historical multi-engine and codec evaluations, HTML template hardening (`CompiledReportTemplate` + `minijinja`), OCI `MySQL.Free` / `HeatWave.Free` operational runbooks, and the apples-to-apples `--release` **Before (`v2`) vs. After (`v3` SQLite WAL / `v3` OCI MySQL InnoDB / `v3` HeatWave Rapid)** storage, latency, and `25K -> 150K` analytical scale-up benchmark gate (`Seed = 0x0E57A7E`).

> **Security Note:** All database passwords and credentials are redacted (`***`) in this document. Live connection parameters belong exclusively in gitignored, `chmod 600` local files such as `.cache/oci-mysql-free.env`.

---

## 1. Executive Summary & Shipped Architecture

### 1.1 Legacy Storage Problem
Previously, `cached_reports` persisted both `report_json` (`~282–584 KiB` raw JSON) and a pre-rendered `rendered_html` string (`~2.40–2.43 MB` per row). Every `rendered_html` row duplicated **`2,113,030 B` (`~2.015 MiB` / `2.11 MB`)** of identical static HTML, inline JS bundles, and i18n locale JSONs alongside a second escaped copy of `report_json`:

- **Massive DB & RAM bloat:** At 1M homes, legacy rows required **`2.68–2.97 TB`** (`~16,258–20,013` homes inside a `50 GiB` OCI `MySQL.Free` volume) and up to **`~1.2 GB` RAM** in the 512-entry `moka` L1 cache (`CachedReportRecord.rendered_html`).
- **Stale frozen UI:** Pre-baked HTML froze `assets/template.html`, JS bundles, locale files, white-label branding, `/admin` dynamic module toggles (`apply_runtime_module_filter`), and presentation normalizers (`normalize_stored_report`) at generation time.
- **Wasted write I/O:** Live `GET /r/:slug` (`handle_rendered_report`, `src/web/server.rs:2936-3025`) already bypassed `rec.rendered_html` and re-rendered dynamically via `render_report_html_with_brand` so live likes, refresh badges, parcel upgrades, and runtime module toggles stayed accurate.

### 1.2 Shipped Architecture (`256 KiB` Zstd Dict + `ZipNorm` + `Geo6dec` + Runtime SSR)
The shipped storage and serving pipeline eliminates `rendered_html` persistence, compacts report payloads structurally, compresses them with a deterministic trained `256 KiB` Zstandard dictionary (`zstd -9`), and compiles the HTML shell once for sub-7 ms runtime SSR:

1. **Trained `256 KiB` Zstandard Dictionary at `zstd -9` (`assets/report_zstd.dict`):**
   - Embedded via `include_bytes!("../../assets/report_zstd.dict")` (`src/db/mod.rs:77`): deterministic **`262,144 B` (`256.0 KiB`)** dictionary (`dict_id = 949758255`, `sha256 = 8e1f85a6ffca117bf77ba81a03cc0d04920e7a4e8649a370f90dbd4e6394405c`), trained via `tests/storage_compression_bench.rs` on `864` synthetic multi-vertical samples from `540` varied US reports (`Synth*` names, zero `.cache` data).
   - `REPORT_ZSTD_LEVEL = 9` (`src/db/mod.rs:74`) with prepared `Lazy<EncoderDictionary>` / `Lazy<DecoderDictionary>` (`src/db/mod.rs:86-94`) and thread-local `Compressor` / `Decompressor` reuse (`src/db/mod.rs:96-175`).
   - Added `zstd = "0.14"` (`Cargo.toml:51`, resolving to `zstd 0.14.3` / `libzstd 1.5.7`), binary parameter/row plumbing (`SqlParam::Bytes(Vec<u8>)`, `DbRowExt::opt_bytes` in `src/db/mod.rs`), and nullable `report_zstd` (`BLOB` at `src/db/schema.rs:224`; `LONGBLOB NULL` at `src/db/schema.rs:647`), upgraded idempotently via `CACHED_REPORTS_UPGRADE_COLS` (`src/db/schema.rs:1373-1374`) and `ensure_cached_reports_upgrades_sqlite` / `ensure_cached_reports_upgrades_mysql` (`src/db/migrate.rs:780-878`).
2. **Content-Addressed `ZipNorm` Shared Market Snapshots (`zip_market_snapshots`):**
   - Added the 26th schema table `zip_market_snapshots(snapshot_hash PRIMARY KEY, zip, payload_zstd, updated_at)` to both `SQLITE_DDL_STMTS` and `MYSQL_DDL_STMTS` (`src/db/schema.rs:35, 452-458, 875-881`) plus `MIGRATION_TABLES` (`src/db/migrate.rs:208`) and `l1_market_snapshots: Cache<String, Value>` (`src/db/mod.rs:362`).
   - `put_cached_report` (`src/db/mod.rs:1543-1653`) strips transient private fields (`inspection`, `titleReview`, `creReview`, `agent_profile`, `coBranding`), then calls `extract_market_snapshot_bundle` (`src/db/mod.rs:263-295`) to pull heavy invariant `modules.market` subtrees (`series`, `historicalMonthlySeries`, `fhfa`, `acs` when `>= 1024 B`), SHA-256 hash them, `INSERT IGNORE` the dictionary-compressed `payload_zstd` once into `zip_market_snapshots`, and store `{"__marketSnapshot": {"__snapshotRef": "<sha256>"}}` inside the report row. Because snapshots are content-addressed by SHA-256, newer market revisions in the same ZIP create new snapshot rows without mutating older cached reports, while every house in the same ZIP/month shares a single market blob.
   - `rehydrate_market_snapshots_batch` (`src/db/mod.rs:1377-1481`) resolves `__snapshotRef` markers across `get_cached_report`, `list_osrm_degraded_reports`, and `list_cached_reports_by_zip` via `l1_market_snapshots` or batched `WHERE snapshot_hash IN (...)` queries, returning the 100% lossless reconstructed JSON.
3. **Precision-Safe `Geo6dec` + Mirror Deduplication:**
   - `round_geojson_coordinates_6dec` (`src/shared/helpers.rs:77-95`, `src/shared.rs:3184`, `src/modules/environment.rs:2139`, `src/web/orchestrator.rs:1322`) rounds GeoJSON `"coordinates"` arrays to 6 decimal places (`~11 cm` WGS84 precision) to strip incompressible 15-digit IEEE-754 float noise while leaving financial, tax, ratio, and score floats untouched.
   - Mirror deduplication (`src/web/orchestrator.rs:1219-1224`, `src/modules/environment.rs:2454`, `src/modules/fengshui.rs:3004, 3024`, `src/modules/hazards.rs:2354`) removes redundant duplicate subtrees (`modules.remodel.data`, `environment.layers.mapLayers`, `fengshui.sitePlan`/`layers.facing`, `hazards.layers.list`) that frontend fallback getters already resolve transparently.
4. **Seamless Backward Compatibility & Streaming Migration:**
   - `decode_cached_report_json(report_zstd, legacy_report_json)` (`src/db/mod.rs:195-221`) inspects `zstd_safe::get_dict_id_from_frame` and transparently decodes all three formats: (a) current dictionary frames (`dict_id == 949758255`), (b) legacy dict-less `zstd -3` frames (`dict_id == None`), and (c) legacy uncompressed `report_json` strings, while explicitly rejecting unknown dictionary IDs.
   - `migrate_database_with_batch` (`src/db/migrate.rs:721-776`) migrates `zip_market_snapshots` (`normalize_zip_market_snapshot_migration_row`) and `cached_reports` (`normalize_cached_report_migration_row`), zero-copy streaming current-dict rows while auto-upgrading dict-less `zstd -3` and legacy `report_json` rows (`report_json = ""`, `rendered_html = NULL`).
5. **Runtime `CompiledReportTemplate` SSR (`6.41 ms` p50 in `--release`):**
   - `CompiledReportTemplate` (`src/web/render.rs:406-579`) folds all 9 static JS/locale assets into 5 static `Literal` chunks plus 4 dynamic per-request slots (`Title`, `Desc`, `BrandWordmark`, `ReportJson`).
   - Live dev overrides (`OPENESTATE_ASSETS_DIR` or `OPENESTATE_TEMPLATE_DEV_RELOAD=1`) share `LiveTemplateCache` (`src/web/render.rs:653-694`), keyed by `(PathBuf, [(len, modified); 11])` metadata so steady-state GETs perform zero recompiles and edits reload once immediately.
   - In-place sanitization + `contains_ascii_ci` fast paths (`sanitize_html_value_recursive`, `src/web/render.rs:976`) and single-pass UTF-8 scanning in `safe_inline_json` (`src/web/render.rs:316`) keep `render_report_html_with_brand` at **`6.41 ms` p50** (`7.30 ms` p95 / `9.49 ms` p99).
6. **Template-Digest `ETag` Invalidation & Admin/Ops Console Accuracy:**
   - `compute_report_etag` (`src/web/cdn_cache.rs:69-89`) folds `report_template_bundle_hash()` (`src/web/render.rs:698-704`) into `"html"` ETags so asset updates immediately rotate browser/CDN caches (`max-age=300`, `s-maxage=86400`) with zero DB migrations.
   - `AdminReportSummaryRow.compressed` (`src/db/schema.rs`), `RecentReportSummary.{runtime_ssr, compressed}` (`src/web/ops_dashboard.rs`), `/admin` `Runtime SSR` + `zstd` pills (`src/web/admin_console.rs`), and `"runtimeSsr": true` accurately surface runtime SSR status.
7. **Clean-Slate `v3` (`openestate.db.v1`) 44-Table Protobuf-First Relational Refactor (Section 7):**
   - Replaces the `v2` 26-table wide-`LONGTEXT`/`VARCHAR(180)` schema with **44 tightly normalized, Protobuf-contracted tables** (`proto/openestate/db/v1/*.proto` compiled via `prost-build` in `build.rs`): **0 `JSON` columns, 0 `LONGTEXT` columns, 0 `FOREIGN KEY` constraints** (62-bit deterministic `listing_sk`/`subscription_sk`/`report_id` surrogate keys via `deterministic_sk`), and **0 runtime `ALTER TABLE` upgrade shims**.
   - Splits 1-house single-domain lookups into typed 1:1 tables (`listings` 42-col narrow core, `listing_schools`, `listing_utilities`, `listing_hazards`, `listing_features`, `listing_engagement`, `listing_descriptions`, `listing_factor_provenance`) and decomposes each cached report into a scalar `cached_reports` header + up to **27 `ReportSectionKind` dictionary-zstd frames** in `cached_report_sections`, cutting single-feature/single-section reads by **`46x–257x`** (SQLite), admin/OSRM scans by **`18.4x–535x`**, `rank_area` by **`86.6x`** (MySQL), `Q2_facets` to a single SQL pass (`3.43x` faster SQLite, `299.1 ms` Stage B Rapid), and total DB footprint by **`37.4%` (SQLite) / `43.7%` (MySQL)** while fixing the `v2` 500-row `/api/listings/shop` facet truncation bug (`100%` exact catalog coverage).

### 1.3 Summary Impact (`#44`, `#78`, `#171`)

| Dimension | Legacy (`report_json` + `rendered_html`) | Phase 1 (`zstd -3`, no dict) | Shipped Final (`256 KiB` dict `-9` + `ZipNorm` + `Geo6dec` + MirrorDedup) | Improvement vs. Legacy |
|---|---|---|---|---|
| **Compressed Blob Size** (`N = 500`, `409.1 KB` avg raw) | `2,902.2 KB` (HTML+JSON) | `95.4 KB` (`4.29x`) | **`34.9 KB` (`11.72x`, `63.4%` smaller vs `zstd -3`)** | **`~83.2x` smaller payload** |
| **SQLite WAL @ 500 Homes** (`--release`, `#78` -> `#171`) | `1,417.11 MB` (`~1,485.9 MB`) | `48.23 MiB` (`50.57 MB`) | **`19.59 MB` (`40.1 KB/home`)** | **`72.3x` smaller on disk** |
| **Extrapolated 1M Homes (SQLite WAL)** | `~2,971.9 GB` (`~2.97 TB`) | `~101.15 GB` | **`~41.09 GB`** | **`72.3x` smaller** |
| **Homes in `45 GiB` Usable (`50 GiB` `MySQL.Free` w/ headroom)** | `~16,258` homes | `~477,706` homes | **`~1,175,885` homes** | **`72.3x` capacity** |
| **Pure Compress / Decompress p50 (`--release`, `N = 500`)** | N/A | `1.71 ms` / `0.41 ms` | **`2.65 ms` / `0.18 ms`** | **`0.18 ms` decode** |
| **Cold-L1 `put_cached_report` / E2E Serve p50 (`N = 500`)** | `26.87–27.94 ms` write | `4.95 ms` / `19.18 ms` | **`7.37 ms` (`131.0 ops/s`) / `9.98 ms` (`88.7 ops/s`)** | **`3.6–3.8x` faster write; `~2x` faster E2E vs Phase 1** |
| **16-Reader + Batch Writer Throughput (`N = 500`)** | `SQLITE_BUSY` timeouts | `325.6 ops/s` (`0` errors) | **`483.7 read ops/s` (`0` errors)** | **`+48.6%` vs Phase 1** |
| **`report_zstd_runtime_html_test` (`[zstd-metrics]`, ~450 KB)** | `3,012 KB` HTML+JSON | `101,562 B` | **`35,295 B` (`12.74x`; `85.3x` DB row reduction)** | **`85.3x` row reduction** |

- **Verified `--offline --locked` Test Suites (`#171`, `Stage 3B`):**
  - Historical compression/SSR milestone (`#171`): `178` passed across `--lib` + `10` suites.
  - Current `v3` Protobuf schema + integration + OSS audit + benchmark suites: see Section 7 for the full Before (`v2`) vs. After (`v3` SQLite WAL / OCI MySQL InnoDB / HeatWave Rapid) matrix (`tests/schema_v3_perf_bench.rs`), `brand_independence_test`, and `oss_license_and_secret_audit_test`.
  - Clean `v3` release benchmark runner: `OCI_MYSQL_ENV_FILE=.cache/oci-mysql-free.env cargo test --release --offline --locked --test schema_v3_perf_bench -- --ignored --nocapture` (uses isolated ephemeral `openestate_bench_v3` schema, auto-unloads Rapid & drops cleanly).

---

## 2. Historical Multi-Engine & Phase-1 Baseline Benchmarks (`#44`, `#50`, `#78`)

### 2.1 Test Environments & Anti-Cheat Datasets
- **Multi-Engine Host (`#44`):** 20-core / 28-thread x86_64 host, 30 GiB RAM, Linux 7.0, real `ext4` LVM filesystem (`fdatasync` and sparse hole punching enabled). Engines tested: SQLite `3.46.1` (`SQLITE_ENABLE_DBSTAT_VTAB`), MySQL `8.4.13` LTS (`mysql:8.4`, 16 KiB InnoDB pages, `utf8mb4_0900_ai_ci`), PostgreSQL `17.12` (`postgres:17-alpine`, 8 KiB pages, `liblz4` + `libzstd`), `zstd` `v1.5.7`, `liblz4` `v1.10.0`, `gzip` `v1.14`.
- **`N = 1,000` High-Entropy Dataset (`#44`):** Exact `SQLITE_DDL_STMTS` and `MYSQL_DDL_STMTS` from `src/db/schema.rs` (`listings`: 85 columns + PK + 9 secondary indexes; `listing_factors`: 16 columns + PK + 2 secondary indexes = 101 catalog columns + 11 secondary indexes). Reports modeled on full 321-month ZHVI/ZORI series + 50-yr FHFA HPI + 7-level ACS, `assets/parcel_layers.json`, rooftop GeoJSONs, comps, schools, ComCat earthquakes + fault `LineString`s, EPA sites + 30-day AQI, OSM POIs, 30-yr amortization, Flying Star grids, RSMeans items, LiDAR transects, and AI prose:
  - Test cohort (`seed=20261116`, `SHA-256[:16]=8856649779fc0a68`): total `281,685,945 B` | min `238,538 B` | p10 `247,881 B` | p50 `281,136 B` | **mean `281,685.9 B` (`275.1 KiB`)** | p90 `314,344 B` | max `323,254 B` | mean Shannon entropy **`5.317 bits/byte`** (`5.303–5.334`).
  - Dictionary training cohort: **200 disjoint reports** (`seed=99991116`, zero shared slugs).
  - Real `rendered_html`: assembled from `assets/template.html` + 7 JS bundles + 3 locale JSONs = `2,112,868 B` (`2.015 MiB`) static shell + escaped `report_json` = **`2,394,771.2 B` (`2.284 MiB` / `2.395 MB`)** mean.
- **`N = 500` Rust `--release` Dataset (`#50` / `#78`, `tests/storage_compression_bench.rs`):** 500 unique reports across 25 metros (**mean `418,874.5 B` / `409.1 KiB`**, min `398.3 KiB`, max `427.5 KiB`; mean HTML `2,546,662.5 B` / `2.43 MB`; Phase-1 dict-less `zstd -3` BLOB `97,723.2 B` / `95.4 KiB`, `4.29x` ratio) + `12,000` `listings` rows + `500` `listing_factors` rows.

### 2.2 Phase-1 Codec Microbenchmark (`N = 1,000` Held-Out Reports, Raw = `281,685,945 B`, `#44`)

| Codec | Total Compressed (B) | Mean / Home | Ratio | Compress p50 / p95 / p99 (µs) | Compress Throughput | Decompress p50 / p95 / p99 (µs) | Decompress Throughput |
|---|---:|---:|---:|---:|---:|---:|---:|
| Uncompressed `report_json` | 281,685,945 | 281.7 KB | 1.00x | — | — | — | — |
| `lz4` (`liblz4` 1.10.0 default) | 115,399,271 | 115.4 KB | **2.44x** | 426 / 590 / 722 µs | 603.8 MiB/s | **81 / 96 / 106 µs** | **3,292.6 MiB/s** |
| `zstd -1` (`v1.5.7`) | 71,984,585 | 72.0 KB | **3.91x** | 604 / 710 / 966 µs | 446.4 MiB/s | 190 / 215 / 251 µs | 1,401.6 MiB/s |
| `zstd -3` (`v1.5.7`, Phase-1 baseline) | 73,987,959 | 74.0 KB | 3.81x | 857 / 1,021 / 1,205 µs | 307.7 MiB/s | 218 / 240 / 257 µs | 1,223.9 MiB/s |
| `zstd -9` (`v1.5.7`) | 68,808,926 | 68.8 KB | 4.09x | 3,295 / 4,230 / 4,844 µs | 78.8 MiB/s | 216 / 293 / 322 µs | 1,208.7 MiB/s |
| `zstd -3 + 110KB dict` (`zstd --train`) | 69,108,082 | 69.1 KB | 4.08x | 1,549 / 1,796 / 2,297 µs | 170.5 MiB/s | 254 / 310 / 365 µs | 1,044.1 MiB/s |
| `zstd -3 + 64KB dict` (held-out) | 64,733,381 | 64.7 KB | 4.35x | 1,429 / 1,862 / 2,128 µs | 183.0 MiB/s | 236 / 265 / 319 µs | 1,134.3 MiB/s |
| `gzip -6` (`zlib`) | 72,584,351 | 72.6 KB | 3.88x | 14,190 / 18,552 / 19,068 µs | 22.0 MiB/s | 1,272 / 1,508 / 1,565 µs | 218.5 MiB/s |

*(Key observation: `zstd -3` compresses **`16.5x` faster** and decompresses **`5.8x` faster** than `gzip -6`, while avoiding inflated synthetic-text ratios on genuine `5.317 bits/B` JSON float arrays.)*

### 2.3 SQLite 3.46.1 WAL Exact On-Disk File Size (`N = 1,000` Full Homes, 4 KiB Pages, `wal_checkpoint(TRUNCATE)`, `#44`)

| Variant (`listings` + `listing_factors` + `cached_reports`) | Exact `.sqlite` Bytes (`N=1,000`) | Bytes / Home | Extrapolated **1M Homes** (GB / GiB) | **50 GiB** Capacity (Homes) |
|---|---:|---:|---:|---:|
| `(0a)` Empty schema + all indexes | 421,888 B (0.40 MiB) | — | — | — |
| `(0b)` `listings` only (85 cols + 9 secondary idx) | 2,043,904 B (1.95 MiB) | 1,667.1 B | **1.67 GB (1.55 GiB)** | 32,203,881 |
| `(0c)` Catalog only (`listings` + `listing_factors`) | 2,519,040 B (2.40 MiB) | 2,142.2 B | **2.14 GB (2.00 GiB)** | 25,061,661 |
| **(a) Legacy (`report_json` + 2.40 MB `rendered_html`)** | **2,682,540,032 B (2,558.27 MiB)** | **2,682,540.0 B** | **2,682.54 GB (2,498.31 GiB)** | **20,013** |
| **(b) Drop HTML only (`rendered_html = NULL`, raw JSON)** | **285,433,856 B (272.21 MiB)** | **285,433.9 B** | **285.43 GB (265.83 GiB)** | **188,089** |
| `(c.1)` No HTML + `zstd -1` `BLOB` | 75,530,240 B (72.03 MiB) | 75,530.2 B | **75.53 GB (70.34 GiB)** | **710,802** |
| `(c.2)` No HTML + `zstd -3` `BLOB` (Phase-1 baseline) | 77,582,336 B (73.99 MiB) | 77,582.3 B | 77.58 GB (72.25 GiB) | 692,001 |
| `(c.3)` No HTML + `zstd -9` `BLOB` | 72,306,688 B (68.96 MiB) | 72,306.7 B | 72.31 GB (67.34 GiB) | 742,491 |
| `(c.4a)` No HTML + `zstd -3 + 64KB dict` `BLOB` | 68,288,512 B (65.12 MiB) | 68,288.5 B | 68.29 GB (63.60 GiB) | 786,180 |
| `(c.4b)` No HTML + `zstd -3 + 110KB dict` (`zstd --train`) | 72,646,656 B (69.28 MiB) | 72,646.7 B | 72.65 GB (67.66 GiB) | 739,016 |
| `(c.5)` No HTML + `lz4` `BLOB` | 118,968,320 B (113.46 MiB) | 118,968.3 B | 118.97 GB (110.80 GiB) | 451,272 |
| `(c.6)` No HTML + `gzip -6` `BLOB` | 76,144,640 B (72.62 MiB) | 76,144.6 B | 76.14 GB (70.92 GiB) | 705,067 |

**Exact `dbstat` 4 KiB-Page Breakdown for `(c.2)` (`zstd -3`):**
- `listings` table B-tree (`1,372,160 B`, `335` pages) + 9 secondary & PK indexes (`294,912 B`, `72` pages) = **`1,667.1 B/home`**
- `listing_factors` table + indexes: `491,520 B` (`120` pages; net `475.1 B/home`)
- `cached_reports` table + overflow pages: `74,964,992 B` (`18,302` pages vs `73,987,959 B` raw zstd payload = **only `1.32%` SQLite page/overflow overhead**) + indexes (`114,688 B`, `28` pages).

### 2.4 Cross-Engine On-Disk Storage Comparison (`N = 1,000` Full Homes, `#44`)

| Engine & Storage Mode | Catalog (`listings` + `factors`) | `cached_reports` (`DATA+IDX` / PG Total) | `.ibd` Allocated on `ext4` (`st_blocks*512`) | **1M Homes** Steady-State (`GB` / `GiB`) | **50 GiB** Capacity (Homes) |
|---|---:|---:|---:|---:|---:|
| **MySQL 8.4 InnoDB**: Legacy (`LONGTEXT` JSON + 2.40 MB HTML) | 4,374,528 B | 2,737,520,640 B | 2,743,083,008 B | **2,741.90 GB (2,553.59 GiB)** | **19,580** |
| **MySQL 8.4 InnoDB**: Drop HTML (`LONGTEXT` raw JSON) | 4,374,528 B | 309,018,624 B | 314,576,896 B | **313.39 GB (291.87 GiB)** | **171,309** |
| **MySQL 8.4 InnoDB**: Native `JSON` column (binary JSON) | 4,374,528 B | 379,273,216 B | 381,685,760 B | **383.65 GB (357.30 GiB)** | **139,938** |
| **MySQL 8.4 InnoDB**: Transparent Page `COMPRESSION='lz4'` | 4,374,528 B | 309,018,624 B (logical) | **170,995,712 B** (sparse) | **175.37 GB (163.33 GiB)** | **306,136** |
| **MySQL 8.4 InnoDB**: Transparent Page `COMPRESSION='zlib'` | 4,374,528 B | 309,018,624 B (logical) | **128,667,648 B** (sparse) | **133.04 GB (123.90 GiB)** | **403,534** |
| **MySQL 8.4 InnoDB**: `LONGBLOB zstd -3` (Phase-1 baseline) | 4,374,528 B | 83,574,784 B | 92,278,784 B | 87.95 GB (81.91 GiB) *(raw `.ibd`: 104.7 GB)* | 610,432 *(512,660 w/ `.ibd` tail)* |
| **MySQL 8.4 InnoDB**: `LONGBLOB zstd -3 + 64KB dict` | 4,374,528 B | 74,137,600 B | 83,890,176 B | 78.51 GB (73.12 GiB) | 683,806 *(557,302 w/ `.ibd` tail)* |
| **PostgreSQL 17.12**: Legacy (`TEXT` JSON + HTML, `TOAST pglz`) | 2,523,136 B | 1,153,351,680 B | — | 1,155.87 GB (1,076.49 GiB) | 46,447 *(0.5GB Free: **432**)* |
| **PostgreSQL 17.12**: Legacy (`TEXT` JSON + HTML, `TOAST lz4`) | 2,523,136 B | 1,237,000,192 B | — | 1,239.52 GB (1,154.40 GiB) | 43,312 *(0.5GB Free: **403**)* |
| **PostgreSQL 17.12**: `JSONB` + `TOAST pglz` | 2,523,136 B | 116,260,864 B (`114.6M` toast) | — | 118.78 GB (110.63 GiB) | 451,972 *(0.5GB Free: **4,209**)* |
| **PostgreSQL 17.12**: `JSONB` + `TOAST lz4` (`target=2040`/`128`) | 2,523,136 B | 127,139,840 B (`127.2M` @ t128) | — | 129.66 GB (120.76 GiB) | 414,051 *(0.5GB Free: **3,856**)* |
| **PostgreSQL 17.12**: `TEXT` + `TOAST lz4` | 2,523,136 B | 120,692,736 B | — | 123.22 GB (114.75 GiB) | 435,715 *(0.5GB Free: **4,057**)* |
| **PostgreSQL 17.12**: `BYTEA zstd -3` (`EXTERNAL` / `EXTENDED`) | 2,523,136 B | 77,684,736 B (`76.5M` toast) | — | 80.21 GB (74.70 GiB) | 669,349 *(0.5GB Free: **6,233**)* |
| **PostgreSQL 17.12**: `BYTEA zstd -3 + 64KB dict` (`EXTERNAL`) | 2,523,136 B | 68,083,712 B (`67.0M` toast) | — | 70.61 GB (65.76 GiB) | 760,366 *(0.5GB Free: **7,081**)* |

- **InnoDB Extent Accounting:** At `N = 1,000`, `listings` reports `DATA_LENGTH = 2,637,824 B` + `INDEX_LENGTH = 917,504 B` = `3,555,328 B` (`3.56 KB/home`), while its `.ibd` is `11,538,432 B` due to fixed 1 MiB extent pre-allocation across 10 B-tree segments (`~8.0 MB` `DATA_FREE` tail). At 1M homes, fixed extent tails amortize to ~0.
- **Why `JSONB` Underperforms:** Postgres `JSONB + TOAST lz4` (`129.66 GB`) is `61.7%` larger than `BYTEA zstd -3` (`80.21 GB`) because PG slices every ~282 KB `JSONB` into ~142 independent 2 KiB TOAST chunks (losing cross-chunk context) on top of binary `JSONB` JEntry header bloat. MySQL native `JSON` (`383.65 GB`) is `22%` larger than `LONGTEXT` (`313.39 GB`).

### 2.5 Cross-Engine Query Latency Comparison (`1,000` Random Queries, Cold App Cache, Warm Buffer Pool, `#44`)

| Operation | Engine & Storage Variant | p50 (µs) | p95 (µs) | p99 (µs) | Mean (µs) | Verified Index / Plan |
|---|---|---:|---:|---:|---:|---|
| **Point Lookup (`WHERE slug = ?`)** | **SQLite WAL**: Legacy 2.40 MB HTML | 4,917.6 | 10,733.6 | 14,802.1 | 5,735.5 | `sqlite_autoindex_cached_reports_1` |
| | **SQLite WAL**: Uncompressed JSON + parse | 3,651.8 | 6,327.2 | 8,590.4 | 3,854.1 | `sqlite_autoindex_cached_reports_1` |
| | **SQLite WAL: `zstd -3` BLOB + decode + parse** | 3,385.5 | 5,715.3 | 7,020.3 | 3,589.0 | `sqlite_autoindex_cached_reports_1` |
| | **SQLite WAL**: `zstd -3 + dict` + decode + parse | **2,863.6** | **5,504.2** | 7,410.8 | **3,195.1** | `sqlite_autoindex_cached_reports_1` |
| | **MySQL 8.4** (`127.0.0.1` TCP): Legacy 2.40 MB HTML | 18,196.1 | 37,347.6 | 269,278.9 | 24,118.7 | `PRIMARY` |
| | **MySQL 8.4** (`127.0.0.1` TCP): `LONGTEXT` JSON + parse | 4,119.1 | 9,187.2 | 99,908.1 | 6,257.7 | `PRIMARY` |
| | **MySQL 8.4** (`127.0.0.1` TCP): Native `JSON` + parse | 8,380.7 | 15,498.5 | 104,512.4 | 10,577.8 | `PRIMARY` |
| | **MySQL 8.4 (`127.0.0.1` TCP): `LONGBLOB zstd -3` + decode + parse** | **3,464.3** | **6,066.8** | **8,665.3** | **3,762.0** | `PRIMARY` *(**5.25x p50 / 31x p99** vs Legacy)* |
| | **PostgreSQL 17** (`libpq` prepared): `JSONB` `TOAST pglz` + parse | 4,758.4 | 9,006.6 | 12,774.6 | 5,465.1 | `PK Index Scan` |
| | **PostgreSQL 17** (`libpq` prepared): `JSONB` `TOAST lz4` + parse | 4,782.7 | 10,122.8 | 16,077.1 | 5,558.6 | `PK Index Scan` |
| | **PostgreSQL 17** (`libpq` prepared): `TEXT` `TOAST lz4` + parse | 3,025.0 | 5,669.9 | 7,217.4 | 3,314.2 | `PK Index Scan` |
| | **PostgreSQL 17** (`libpq` prepared): `BYTEA zstd -3` + decode + parse | **3,175.3** | **5,929.5** | **8,851.9** | **3,570.5** | `PK Index Scan` *(33.6% faster than JSONB lz4)* |
| **Indexed Search (`WHERE zip=? AND status='Active' ORDER BY price LIMIT 50`)** | **SQLite WAL** (in-process) | **39.3** | **67.2** | 1,251.0 | ~52 | `USING INDEX idx_listings_zip_status_price` |
| | **PostgreSQL 17** (`127.0.0.1` `libpq` prepared) | **134.2** | **238.2** | **315.0** | ~150 | `Index Scan on idx_listings_zip_status_price` |
| | **MySQL 8.4** (`127.0.0.1` `pymysql`) | **396.5** | **768.8** | **1,108.9** | ~445 | `key: idx_listings_zip_status_price` |

### 2.6 SQLite WAL 8-Reader + 200-Row Harvester Write Contention (`#44`)
- **Setup:** `8` concurrent OS reader processes (50% point report fetch + decode + `json.loads`, 50% indexed ZIP search) + `1` live interactive writer (`BEGIN IMMEDIATE` `property_likes` upsert every `15 ms`) + `1` continuous background harvester batch-upserting `200` `listings` + `200` `cached_reports` per transaction (`busy_timeout=10000`, `synchronous=NORMAL`, `wal_autocheckpoint=1000`).

| Metric | WITH 2.40 MB `rendered_html` (Legacy) *(6 batches = 1,200 upserts)* | WITHOUT `rendered_html` (`zstd -3` ~74 KB BLOB) *(20 batches = 4,000 upserts)* | Empirical Improvement |
|---|---:|---:|---|
| **Harvester 200-Row Write-Lock Hold Time** | p50 = **12,404.9 ms** / p95 = **18,785.0 ms** / max = **19,557.1 ms** | p50 = **17.58 ms** / p95 = **60.09 ms** / max = **61.05 ms** | **`705x` shorter p50 (`320x` max)** |
| **`SQLITE_BUSY` Lock Timeout Errors (`>10s`)** | **1 error** *(blew past `busy_timeout=10000`)* | **0 errors** *(61 ms max = `163x` safety margin)* | **Eliminates `SQLITE_BUSY`** |
| **Harvester Write Throughput** | `16.6 homes/sec` | **`602.0 homes/sec`** | **`36.3x` faster ingestion** |
| **Peak `.sqlite-wal` File Size on Disk** | `2,194,246,112 B` (`2.04 GiB`) | **`5,368,392 B` (`5.12 MiB`)** | **`408.7x` (`~409x`) smaller WAL** |
| **8 Concurrent Readers Throughput** | `1,459 read ops/sec` | **`4,389 read ops/sec`** | **`3.01x` read throughput** |
| **Concurrent Point Report Fetch Latency** | p50 `8,835.7 µs` / p99.9 `123.6 ms` / max `4,117.7 ms` | p50 **`3,279.4 µs`** / p99.9 **`10.9 ms`** / max **`13.1 ms`** | **`2.69x` p50, `11.4x` p99.9, `314x` max** |
| **Concurrent Indexed ZIP Search Latency** | p50 `223.6 µs` / p99 `671.8 µs` / max `481.0 ms` | p50 **`109.5 µs`** / p99 **`256.0 µs`** / max **`0.58 ms`** | **`2.04x` p50, `830x` max tail** |
| **Concurrent Interactive User Write Latency** | p50 `0.16 ms` / p99 `52.42 ms` / max `11,035.4 ms` *(timeout)* | p50 **`0.08 ms`** / p99 **`19.14 ms`** / max **`648.4 ms`** | **No 10s write lockouts** |

### 2.7 Pre- vs. Post-Review (`#53`) & Final Shipped (`#171`) `--release` Rust Benchmarks (`N = 500`, `409.1 KiB` Raw Reports)

| Storage Mode (`N = 500` Full Homes + `12,000` `listings`) | Exact `.sqlite` Bytes | Per-Home Disk | Extrapolated 1M Homes | Homes in `45 GiB` Usable |
|---|---:|---:|---:|---:|
| **Catalog Only** (`listings` + `listing_factors`) | `1,318,912 B` (`1.32 MB`) | `2.6 KB / home` | `~2.64 GB` | `~18.3M` listings |
| **(a) Legacy** (`report_json` + 2.43 MB `rendered_html`) | `1,485,930,496 B` (`1,417.11 MB`) | `~2,902 KB / home` | `~2,971.86 GB` (`~2.97 TB`) | `~16,258` homes |
| **(b) Drop HTML Only** (`rendered_html = NULL`, raw JSON) | `211,333,120 B` (`201.54 MiB`) | `412.8 KB / home` | `~422.67 GB` | `~114,318` homes |
| **(c) Phase 1 (`zstd -3` BLOB, no dict)** | `50,573,312 B` (`48.23 MiB`) | `98.8 KB / home` | `~101.15 GB` | `~477,706` homes (`29.4x`) |
| **(d) Final Shipped (`256 KiB` dict `-9` + `ZipNorm` + `Geo6dec` + MirrorDedup)** | **`19,590,000 B` (`19.59 MB`)** | **`40.1 KB / home`** (`34.9 KB` blob) | **`~41.09 GB`** | **`~1,175,885` homes (`72.3x`)** |

**`--release` Latency Evolution (`tests/storage_compression_bench.rs`, Cold L1 Cache):**

| Operation (`--release`, Real SQLite WAL + `open_estate`) | Before `#53` (`#50`) | Phase 1 Post-`#53` (`#78`) | Final Shipped (`#171`, `256 KiB` dict `-9` + `ZipNorm`) |
|---|---:|---:|---:|
| **`render_report_html_with_brand`** (`409 KB` -> `2.43 MB`) | `14.43 ms` p50 / `23.33 ms` p99 | **`6.41 ms` p50 / `9.49 ms` p99 (`154 ops/s`)** | **`~6.4 ms` p50** (`2.25x` faster vs `#50`) |
| **Pure Compress** (`409.1 KB` -> blob) | `1.71 ms` p50 (`95.4 KB`) | `1.71 ms` p50 (`95.4 KB`) | **`2.65 ms` p50 (`34.9 KB`, `11.72x`)** |
| **Pure Decompress** (blob -> `409.1 KB`) | `482.6 µs` p50 | `414.5 µs` p50 | **`0.18 ms` p50 (`180 µs`)** |
| **`put_cached_report` Write** | `4.29 ms` p50 | `4.95 ms` p50 | **`7.37 ms` p50 (`131.0 ops/s`)** *(vs Legacy `26.9–27.9 ms`)* |
| **Cold-L1 Serve E2E** (DB + zstd + rehydrate + render) | `17.71 ms` p50 | `19.18 ms` p50 | **`9.98 ms` p50 (`88.7 ops/s`)** |
| **16 Concurrent Readers + Background Batch Writer** | `39.40 ms` p50 (`383.6 ops/s`) | `41.85 ms` p50 (`325.6 ops/s`) | **`483.7 read ops/s` (`0` `SQLITE_BUSY` errors)** |

### 2.8 Why Dual-Dialect SQLite + OCI MySQL (`MySQL.Free`, `OCI_HEATWAVE_ENABLE_RAPID=false`) Beats PostgreSQL

1. **Zero Engineering Porting Cost (`#25`):** Adding PostgreSQL is not a driver toggle — it requires **`~900–1,400 LOC`** across **12 `src/` files + 7 `tests/` files** (`~2–3` engineering days): adding `postgres` to `sqlx`/`sea-query` (`Cargo.toml:33-34`), updating **70 dialect dispatch points**, rewriting **82 `?`-parameterized SQL queries/builders** in `src/` (plus **23** in `tests/`) to `$1..$N`, maintaining a 3rd 26-table DDL, and resolving 4 semantic traps (`BOOLEAN` vs `0/1` bindings, `INSERT IGNORE` vs `ON CONFLICT DO NOTHING`, case-sensitive `LIKE` vs MySQL `utf8mb4_0900_ai_ci`, and `BYTEA zstd` forfeiting `JSONB` GIN indexes anyway).
2. **`50x–100x` Larger `$0/mo` Managed Free Tier & Smaller Storage (`#25`, `#44`, `#52`):**

| Dimension | **(1) OCI `MySQL.Free` + Ampere A1 VM** | **(2) Managed PostgreSQL** (Free-Tier Cloud Services / OCI PG) | **(3) Self-Hosted SQLite WAL on 200 GB Block Vol** |
|---|---|---|---|
| **$0/mo Free Quota** | **50 GiB** data + **50 GiB** backup (`MySQL.Free` 1 ECPU / 8 GiB RAM) | **0.5 GB–1 GB** on free-tier Postgres hosts (cold-starts/pauses; **OCI PG has NO Always Free**) | **200 GB** Block Vol (`~170 GB` usable) + **2 OCPU / 12 GB** free tier (**4 OCPU / 24 GB** Always Free) |
| **$0 Full Home Capacity** (`256K` dict + `ZipNorm`) | **`~1.176M` homes** (at `~40 KB/home` within `45 GiB`) | **`~400 – 7,081` homes** on 0.5 GB free tier | **`~4.2M+` homes** on 170 GB usable (single-node writer) |
| **Latency & Concurrency** | Multi-pod InnoDB row locking; `3.46 ms` local TCP / `~6–8 ms` private VCN/IPSec | Serverless spin-down/cold starts on free tiers | `~0.05–3.3 ms` in-process; single-writer lock (`busy_timeout=10s`) |
| **Paid Scale-Up (>50 GiB)** | Paid OCI MySQL Base via `open-estate-server migrate` | Managed PG paid tiers (`~$45–$280/mo`) | Block Storage (`~$0.0255/GB-mo` = `~$5/mo` per `+200 GB`) |

3. **Why `OCI_HEATWAVE_ENABLE_RAPID=false` Must Stay Disabled by Default (`#25`, verified `src/db/mod.rs`):**
   - `src/db/mod.rs:541, 1049, 1056-1059` sets `is_heatwave` only when `DATABASE_PROVIDER=heatwave` (auto-detected `MYSQL_URL` defaults to `"mysql"`) and parses `OCI_HEATWAVE_ENABLE_RAPID` with `.eq_ignore_ascii_case("true")` (`=1` evaluates `false`).
   - `src/db/mod.rs:1255-1262` executes `ALTER TABLE listings SECONDARY_ENGINE = RAPID` and `SECONDARY_LOAD` discarding errors via `let _ =`, causing `ER_SECONDARY_ENGINE` errors on every subsequent boot once loaded.
   - `migrate_database_with_batch` runs `dst.ensure_schema()` *before* bulk inserts (`src/db/migrate.rs:729`), which would force row-by-row InnoDB -> RAPID change propagation across every 200-row batch on a 1-ECPU node.
   - Selective OLTP B-tree lookups (`WHERE zip IN (...) AND status IN (...) LIMIT 50..500`) across 9 `listings` indexes (`src/db/schema.rs:548-556`) stay below MySQL's `secondary_engine_cost_threshold` (0% RAPID offload), while un-annotated `LONGTEXT` columns (`features_text`, `raw_json`, `tags_json` at `src/db/schema.rs:507-508, 543`) would waste `16 GiB` `HeatWave.Free` columnar RAM unless refactored (see Section 6).

---

## 3. Codec Alternatives, Dictionary Sizing & Structural Deduplication (`#95`, `#103`, `#127`, `#171`)

### 3.1 Empirical Report JSON Byte Breakdown (`#95`)
Measured by linking directly against `open_estate` release artifacts across two cohorts:

- **Cohort 1 (`tests/storage_compression_bench.rs` generator, `N = 250`):** mean **`415,068.5 B` (`405.34 KiB`)** raw -> **`97,070.1 B` (`94.80 KiB`)** `zstd -3` (**`4.28x`**):
  - **Top sections (raw -> standalone `zstd -3`):** `modules` `137,344 B` (`33.1%`) -> `28,738 B` (`4.78x`) [`modules.valuation` `59,829 B` / `14.4%` -> `11,143 B`; `modules.market` `49,514 B` / `11.9%` -> `9,952 B`; `modules.fengshui` `18,529 B` / `4.5%` -> `4,201 B`; `environment` `3,551 B`; `civic` `3,275 B`; `amenities` `1,214 B`; `hazards` `1,094 B`; `access` `244 B`]; `remodelAndRebuild` `70,942 B` (`17.1%`) -> `11,930 B` (`5.95x`); `facts` `38,760 B` (`9.3%`) -> `7,835 B`; `propertyFeatures` `21,975 B` (`5.3%`) -> `4,579 B`; `buyerDiligence` `21,057 B` (`5.1%`) -> `7,750 B`; `insuranceQuotes` `18,496 B` (`4.5%`) -> `7,476 B`; `translations` `17,418 B` (`4.2%`) -> `8,304 B` (`2.10x`, lowest ratio due to UTF-8 CJK/ES prose); `sellerSuite` `15,559 B`; `buyerLivingIntel` `11,662 B`; `hoaProfile` `10,461 B`; `agentSuite` `8,825 B`; `summary` `8,435 B`; `walkthroughSlides` `7,506 B`; `titleLegalRadar` `7,198 B`; `ai` `6,914 B`.
  - **Data types (Cohort 1):** Strings `216,920 B` (`52.3%`), JSON keys `134,066 B` (`32.3%`), syntax `{}[],:` `29,623 B` (`7.1%`), ints `15,501 B` (`3.7%`), floats `<=6 dec` `13,969 B` (`3.4%`), bools `2,044 B` (`0.5%`), `null`s `1,764 B` (`0.4%`, `441` fields/home), floats `>=7 dec` `1,037 B` (`0.2%`), empty `[]` `144 B`.
- **Cohort 2 (Production-Full GeoJSON + 321-mo Zillow + `orchestrator.rs` mirrors, `N = 250`):** mean **`598,204.2 B` (`584.18 KiB`)** raw -> **`117,464.8 B` (`114.71 KiB`)** `zstd -3` (**`5.09x`**):
  - `house3d` LiDAR scenes already live outside `cached_reports` via `GET /api/house3d/:slug` (`src/modules/house3d/mod.rs:10`).
  - Unrounded 15-digit IEEE-754 GeoJSON coordinates added **`49,649 B` (`8.3%`)** raw bloat, and duplicate subtree mirrors added **`~98.5 KiB`** raw bloat (both eliminated in the shipped `Geo6dec` + mirror-dedup changes).

### 3.2 MySQL 8.4 InnoDB `16 KiB` LOB Page Step-Function (`N = 500`, `ROW_FORMAT=DYNAMIC`, `#95`)
Because InnoDB stores off-page `LONGBLOB`s (`> ~8 KB`) in integer multiples of **`16,384 B` LOB pages** (`~16,280 B` usable/page), capacity inside `50 GiB` (`53,687,091,200 B`) moves in discrete step-function jumps whenever compressed payload size crosses a `16 KiB` boundary:

| Compressed Payload Size | Allocated `16 KiB` LOB Pages | Measured Table Size (`N = 500` rows) |
|---:|---:|---:|
| `15 KB` | 1 LOB page | `10.09 MB` |
| `20 / 25 / 30 KB` | 2 LOB pages | `18.48 MB` *(identical)* |
| `35 / 45 KB` | 3 LOB pages | `25.82 MB` *(identical)* |
| `55 / 64 KB` | 4 LOB pages | `34.21 MB` *(identical)* |
| `74 KB` | 5 LOB pages | `42.60 MB` |
| `85 / 95 KB` | 6 LOB pages | `49.94 MB` *(identical)* |

*(At `34.9 KB` average compressed blob with the shipped `256 KiB` dict + `ZipNorm` + `Geo6dec`, payloads drop squarely into the `2–3` InnoDB LOB page tier!)*

### 3.3 Alternative Codecs vs. `zstd` (`#103`, `#127`)

Before committing to `zstd -9 + 256 KiB dict + ZipNorm + Geo6dec`, we empirically benchmarked `gzip`, raw `deflate`, `bzip2`, `brotli` (with and without shared custom dictionaries via `BrotliEncoderAttachPreparedDictionary`), `xz` (`LZMA2`), `lzma1` (`lzip`-equivalent), `zstd` dictionaries from `64 KiB` to `1 MiB` (`--train-fastcover=k=512,d=8`), `zstd --long=27`, and binary JSON alternatives (`MessagePack` / `CBOR`).

#### A. Standard Codecs vs. `zstd` on `N = 250` Cohort 1 (`409.11 KiB` Raw, `#103`)

| Codec / Backend | Mean Size (KiB / KB) | Ratio | Enc p50 / p95 (ms) | Dec p50 / p95 (ms) | Avg `16 KiB` LOB Pgs | 1M InnoDB GB | Max Homes in `50 GiB` |
|---|---:|---:|---:|---:|---:|---:|---:|
| `gzip -1` (`flate2` `zlib-rs`) | 134.48 KiB (137.71 KB) | 3.04x | 1.82 / 2.01 ms | 1.05 / 1.14 ms | 9.00 p | 153.66–154.48 GB | 347,533–349,385 |
| `gzip -6` (`flate2` `zlib-rs`) | 99.31 KiB (101.70 KB) | 4.12x | 8.31 / 9.04 ms | 0.75 / 0.82 ms | 7.00 p | 120.89–121.71 GB | 441,097–444,086 |
| `gzip -9` (`flate2` `zlib-rs`) | 99.12 KiB (101.50 KB) | 4.13x | 9.92 / 10.92 ms | 0.74 / 0.81 ms | 7.00 p | 120.89–121.71 GB | 441,097–444,086 |
| `deflate -9` (raw RFC 1951, `flate2`) | 99.11 KiB (101.49 KB) | 4.13x | 9.94 / 10.85 ms | 0.82 / 0.90 ms | 7.00 p | 120.89–121.71 GB | 441,097–444,086 |
| `bzip2 -9` (`libbz2.so.1.0`) | 84.54 KiB (86.56 KB) | 4.84x | 20.28 / 22.34 ms | 6.73 / 7.30 ms | 6.00 p | 104.51–105.33 GB | 509,710–513,706 |
| `brotli -6` (`libbrotlienc` text) | 85.45 KiB (87.51 KB) | 4.79x | 6.08 / 6.61 ms | 0.65 / 0.90 ms | 6.00 p | 104.51–105.33 GB | 509,710–513,706 |
| `brotli -11` (`libbrotlienc` text) | 73.57 KiB (75.33 KB) | 5.56x | 373.84 / 421.68 ms | 0.71 / 0.78 ms | 5.00 p | 88.13–88.94 GB | 603,601–609,212 |
| `xz -6` (`liblzma.so.5` LZMA2) | 76.56 KiB (78.40 KB) | 5.34x | 53.62 / 66.02 ms | 2.32 / 2.54 ms | 5.34 p | 93.63–94.45 GB | 568,420–573,394 |
| `zstd -3` (no dict) | 95.44 KiB (97.73 KB) | 4.29x | 1.21 / 2.11 ms | 0.22 / 0.23 ms | 6.34 p | 110.01–110.83 GB | 484,393–488,000 |
| `zstd -3 + 112KB dict` (`114,688 B`) | 57.96 KiB (59.35 KB) | 7.06x | 0.89 / 1.00 ms | 0.19 / 0.20 ms | 4.17 p | 74.49–75.31 GB | 712,852–720,691 |
| `zstd -9 + 112KB dict` (`114,688 B`) | 50.36 KiB (51.56 KB) | 8.12x | 3.66 / 4.04 ms | 0.18 / 0.19 ms | 3.67 p | 66.30–67.12 GB | 799,854–809,737 |

#### B. Larger Dictionaries, Shared-Dict Brotli, Extreme LZMA & Structural Dedup (`N = 100` C1 `348.26 KiB` / C2 `559.53 KiB`, `#127`)

| Codec / Strategy | C1 Blob | C1 Ratio | C1 Enc / Dec p50 | C1 `50 GiB` Homes | C2 Blob | C2 Ratio | C2 Enc / Dec p50 | C2 `50 GiB` Homes |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `gzip -9` (`zlib`) | 75.71 KiB | 4.60x | 10.39 / 0.770 ms | 614,839 | 123.61 KiB | 4.53x | 17.53 / 1.001 ms | 393,396 |
| `zstd -3` (no dict) | 71.34 KiB | 4.88x | 0.76 / 0.202 ms | 614,839 | 98.38 KiB | 5.69x | 0.91 / 0.231 ms | 447,068 |
| `zstd -19` / `-22` / `--long=27` | 55.77–55.81 KiB | 6.24x | 64.9–72.8 / 0.22 ms | 756,849 | 77.09–77.23 KiB | 7.25–7.26x | 69.9–81.5 / 0.26 ms | 614,839 |
| `zstd -9 + 112 KiB dict` | 59.42 KiB | 5.86x | 5.47 / 0.245 ms | 756,849 | 81.30 KiB | 6.88x | 6.05 / 0.278 ms | 520,993 |
| **`zstd -9 + 256 KiB dict`** | **56.17 KiB** | **6.20x** | **4.25 / 0.228 ms** | **756,849** | **76.35 KiB** | **7.33x** | **5.13 / 0.309 ms** | **614,839** *(+18%)* |
| `zstd -9 + 512 KiB dict` / `1 MiB` (`fc k=512`) | 54.29–54.98 KiB | 6.33–6.41x | 5.46–5.64 / 0.29–0.35 ms | 756,849 | 72.15–74.18 KiB | 7.54–7.76x | 4.81–6.20 / 0.30–0.43 ms | 614,839 |
| `brotli -9 + 112 KiB` / `256 KiB` shared dict | 57.48–58.74 KiB | 5.93–6.06x | 16.45–16.64 / 0.51–0.53 ms | 756,849 | 77.76–79.34 KiB | 7.05–7.20x | 18.35–18.60 / 0.61–0.62 ms | 565,987–602,405 |
| `brotli -11 + 112 KiB shared dict` | 45.73 KiB | 7.62x | 318.48 / 0.530 ms | 984,163 | 61.59 KiB | 9.08x | 393.98 / 0.729 ms | 756,849 |
| `xz -9e` (`LZMA2`) / `lzma1 -9e` | 51.39–51.44 KiB | 6.77–6.78x | 60.0–61.7 / 1.84–1.89 ms | 756,849 | 72.22–72.27 KiB | 7.74–7.75x | 88.7–88.9 / 2.77–2.79 ms | 614,839 |
| `bzip2 -9` (`libbz2`) | 58.58 KiB | 5.95x | 16.79 / 5.001 ms | 756,849 | 94.35 KiB | 5.93x | 29.92 / 8.850 ms | 517,700 |
| `DedupMirrors + Geo6dec` + `zstd -9+112K` | 59.01 KiB | 5.90x | 4.41 / 0.188 ms | 756,849 | 72.49 KiB | 7.72x | 5.19 / 0.225 ms | 614,839 |
| **`Dedup+6dec+ZipNorm` + `zstd -9+256K` (Shipped)** | **46.75 KiB** | **7.45x** | **3.47 / 0.162 ms** | **984,163** | **53.04 KiB** | **10.55x** | **3.68 / 0.175 ms** | **756,849** |
| `Dedup+6dec+ZipNorm` + `zstd -19+256K` | 41.35 KiB | 8.42x | 50.90 / 0.169 ms | 984,163 | 46.96 KiB | 11.92x | 50.96 / 0.177 ms | 984,163 |

*(Also tested in `#95`: `MessagePack(Dedup+Slim6)` [`60.43 KiB`] and `CBOR(Dedup+Slim6)` [`59.91 KiB`] save only `~0.4 ms` serde CPU while compressing slightly worse than JSON [`56.22 KiB`] after `zstd -19+112Kd`.)*

#### Key Decisions from Empirical Codec Findings
1. **Why not `gzip` / `deflate` / `bzip2` / `brotli` / `xz` for DB storage?**
   - **`gzip -6/-9` & `deflate -9`:** Cross the InnoDB `16 KiB` LOB page boundary (`7.00` pages vs. `6.34` on dict-less `zstd -3` and `2–3` pages on `256 KiB` dict), compress **`6.9x–11x` slower**, and decompress **`3.4x–4.3x` slower** (and raw deflate drops CRC32 framing).
   - **`bzip2 -9`:** Decodes **`37x–50x` slower** (`5.00–8.85 ms` vs. `0.16–0.31 ms` for `zstd`) while producing larger payloads.
   - **`brotli` (`-6` / `-9` / `-11`, even with shared dictionary):** `brotli -9 + 256 KiB` trails `zstd -9 + 256 KiB` on size (`57.48`/`77.76 KiB` vs. `56.17`/`76.35 KiB`) while encoding **`3.2x–3.9x` slower** and decoding **`2.0x` slower**. `brotli -11` costs **`318–424 ms` encode** (`~100x` slower than `zstd -9 + dict`).
   - **`xz -6/-9e` / `lzma1 -9e`:** Encode at `54–89 ms` and decode **`10x–20x` slower** (`1.8–4.6 ms` vs. `~0.18 ms`), while still larger than `ZipNorm + Geo6dec + zstd -9 + 256 KiB`.
   - **`zstd --long=27`:** Yields `0.00 KiB` improvement because individual `~350–600 KiB` reports already fit inside Zstd's default `4–8 MiB` window.
2. **Why `256 KiB` Dict + `ZipNorm` + `Geo6dec` Was Selected (`#127`, `#171`):**
   - Increasing dictionary size from `112 KiB` to `256 KiB` (`--train-fastcover` / `864` samples) crossed the C2 `81,400 B` (`5 * 16,280 B`) InnoDB 5-vs-6 LOB page cliff (`520,993 -> 614,839` homes, `+18%`), whereas `512 KiB–1 MiB` added binary weight with zero extra InnoDB LOB page savings at `-9`.
   - Combining `256 KiB` dict at `zstd -9` with **`Geo6dec`** (`-14.9 KiB` raw / `-8.4 KiB` compressed C2 float noise) and **`ZipNorm`** (`zip_market_snapshots` deduplicating `~98 KiB` raw / `~24 KiB` compressed per-ZIP market series across all houses in a ZIP, **`~918 MiB` total for all 39,000 US ZIPs**) hits **`34.9 KB` avg (`11.72x`) / `40.1 KB/home` on disk (`~1.176M` homes in `45 GiB`)** at **`2.65 ms` encode / `0.18 ms` decode**.
   - *(Future note: Meta's format-aware **OpenZL** engine is worth watching once Rust FFI matures; **Parquet** fits cold batch analytics/exports well, not random single-row `GET /r/:slug` OLTP point lookups.)*
3. **HTTP Edge Transport vs. DB Storage Compression:**
   - `src/web/server.rs:123, 860` (`Cargo.toml:23` `tower-http 0.6`, `compression-full`) enables `CompressionLayer::new()`, automatically negotiating `Accept-Encoding: br / zstd / gzip / deflate` over HTTP. Keep `br`/`gzip`/`zstd` at the HTTP edge for browsers & CDNs, while keeping `zstd` (`256 KiB` dict) inside `cached_reports` (`LONGBLOB`/`BLOB`).

---

## 4. HTML Template Engine Evaluation (`CompiledReportTemplate`, `askama`, `minijinja` Hardening) (`#24`, `#95`, `#103`, `#127`, `#135`)

### 4.1 Usage Split Across `assets/template.html` & `src/`
- **Interactive Client SPA Shell (`assets/template.html`):** `assets/template.html` (`1,053,392 B`, 11,762 lines) + 7 inline JS bundles (`report_workspace.js`, `house3d.js`, `report_motion.js`, `report_dashboard.js`, `walkin_media.js`, `vendor/i18next.min.js`, `shared/open_estate_core.js`) + 3 locale JSONs (`en.json`, `zh-CN.json`, `es.json`) total **`2,113,030 B`** of static SPA code. Client hydration (`JSON.parse(document.getElementById('report-data').textContent)`, `i18next.init`, `L.map`, `echarts.init`) runs in the browser.
- **`CompiledReportTemplate` (`src/web/render.rs:406-579`):** Has **13 placeholders total**: **9** static asset/locale injections pre-folded at load time into 5 `Literal` chunks + **only 4 dynamic per-request slots** (`__REPORT_TITLE__`, `__REPORT_DESC__`, `__REPORT_BRAND_WORDMARK__`, `__REPORT_JSON__`). Context-specific escaping (`encode_double_quoted_attribute`, `safe_inline_json`, `safe_inline_js`) executes in a single zero-rescan pass (`~0.13 ms` assembly; almost all of `6.41 ms` SSR is `sanitize_report_for_html` + `serde_json::to_string` + `safe_inline_json`).
- **Where `minijinja 3.1.0` Is Used (`Cargo.toml:54`, `features = ["builtins"]`, 0 transitive crate deps):**
  - `src/web/ui.rs` (`/`, `/report/:id` progress job page, `/api/subscriptions/unsubscribe` with `fluent-templates`)
  - `src/web/alerts_ui.rs` (`GET /alerts` subscription manager UI)
  - `src/search/alerts.rs` (HTML alert, welcome-confirm, and manage-link emails)
  - `src/web/admin_console.rs` & `src/extensions/yaml_runner.rs` (runtime-stored/YAML declarative connector URL, header, query, and body template evaluation)

### 4.2 Default Delimiter Collision (`jinja2 3.1.6` vs. `minijinja 3.1.0`) & Ecosystem Matrix (`#95`, `#103`)
Scanning all 11 static report assets (`2,113,030 B`) proves why default Jinja delimiters fail immediately on `assets/template.html`:
- **Hard Lexer Crash on `{#` CSS ID Selectors (`assets/template.html:428-430, 435`):** Contains **4 `{#` sequences and `0` `#}` sequences** (`@media (max-width:1520px){#hdrMeta{display:none}}`, `#printPdfBtn`, `#shareReportBtn`, `#refreshBtn`). Both Python `jinja2 3.1.6` (`TemplateSyntaxError: Missing end of comment tag`) and Rust `minijinja 3.1.0` crash unless custom delimiters (`SyntaxConfig`) are configured.
- **Silent `i18next` `{{...}}` Wipeout:** Across the 11 assets there are **`47` `{{`**, **`118` `}}`**, and **`20` `%}`** sequences — including **`45` `i18next` runtime placeholders** (`15` each in `assets/locales/{en,zh-CN,es}.json`, e.g. `{{hrs}}`, `{{likes}}`, `{{tier}}`) plus `2` `{{` / `38` `}}` in `assets/vendor/i18next.min.js`, which default Jinja delimiters silently erase to `""`.
- **Wrong `<script>` Escaping:** Standard HTML auto-escaping converts `"` to `&quot;` and `&` to `&amp;`, breaking inline JS/JSON; `| safe` still requires `safe_inline_json` / `safe_inline_js` to block `</script>` breakouts.

| Engine / Lang | Status & Delimiters | Compile / Render Overhead | HTML Escaping | Fit for Open Estate |
|---|---|---|---|---|
| **`CompiledReportTemplate`** *(Implemented for `/r/:slug`)* | Custom `__UPPER_SNAKE__` (13 slots; 4 dynamic) | Compile once; **`~0.13 ms`** join (`6.41 ms` total SSR) | Context-specific (`safe_inline_json` / attr) | **Best for `/r/:slug`**; supports `OPENESTATE_TEMPLATE_DEV_RELOAD=1` + `ETag` rotation |
| **`minijinja 3.1.0`** *(Implemented for SSR UI, emails, connectors)* | `{{ }}` `{% %}` `{# #}`; built-in `SyntaxConfig::builder()` | `Lazy<Environment>` one-time compile; `~0.15 ms` render | Explicit `AutoEscape::Html` on `.html` | **Already in `Cargo.toml:54`** (0 transitive deps); handles both static views and runtime connector strings |
| **`askama 0.16.1`** *(Evaluated & rejected, `#127`)* | `{{ }}` `{% %}` `{# #}` (or `askama.toml` `[[syntax]]`) | Proc-macro rebuild of `73 MiB` binary on template edit | Auto HTML escape by default | **Rejected** (see Section 4.3) |
| **`tera 2.4.0`** | `{{ }}` (`Tera::set_delimiters` in v2) | Runtime env | Auto on `.html`/`.htm`/`.xml` | Redundant vs. lighter `minijinja` |
| **`sailfish 0.11.8`** / **`maud 0.27.0`** | `<% %>` / Rust `html! {}` macro (0 `{{}}` collision) | Compile-time Rust macros | Auto escaped | Good for pure compile-time HTML rewrites; poor fit for runtime YAML strings or `1 MB` SPA shell |
| **`handlebars` / `liquid` / `ructe`** | `{{ }}` (collisions) / `@name` | Runtime / `build.rs` | `liquid` unescaped by default | Avoid (`{{}}` collisions or maintenance/safety gaps) |
| **Python `jinja2 3.1.6` (`pyo3` / sidecar)** | Same `{#`/`{{` crash as `minijinja` | **`195 ms`** compile + **`5.51 ms`** JSON IPC + **CPython GIL** / `+27–50 MB` bloat | HTML only unless configured | **Never adopt** (adds GIL contention or IPC bloat with zero benefit) |

### 4.3 Why `askama` Was Rejected (`#127`)
1. **Cannot Remove `minijinja` Anyway:** `src/extensions/yaml_runner.rs:178-201` (`render_query_template`) and `src/web/admin_console.rs:268-285, 863` evaluate runtime YAML/OpenAPI and DB-stored admin connector URL/header/body strings that cannot be static compile-time `#[derive(Template)]` structs. Adding `askama` would force carrying **both** `askama` and `minijinja` in `Cargo.toml`.
2. **Regresses `/r/:slug` Runtime Reload & Delimiters:** Replacing `CompiledReportTemplate` (`src/web/render.rs:406-694`) with `askama` breaks `OPENESTATE_TEMPLATE_DEV_RELOAD=1` / `OPENESTATE_ASSETS_DIR` hot-reloading, buys `~0 ms` over the `~0.13 ms` 4-slot join, requires custom `askama.toml` `[[syntax]]` delimiters to dodge `assets/template.html:428` `{#` CSS selectors and `i18next` `{{...}}` keys, and grants no type-safety over the `1 MB` client JS SPA.
3. **Heavy Proc-Macro Cost for Zero-Loop Pages:** `ui.rs`, `alerts_ui.rs`, and `alerts.rs` pass small scalar strings/booleans and pre-escaped JSON slots with essentially zero template loops. Adopting `askama_derive` introduces `syn`/`quote`/`basic-toml`/`glob` macro expansion inside `open_estate` and forces a full re-link of the `73 MiB` server binary on minor copy edits.

### 4.4 `minijinja` Auto-Escaping & `Lazy<Environment>` Hardening (`#127`, `#135`)
An audit (`#127`) found that `minijinja`'s `default_auto_escape_callback` enables `AutoEscape::Html` only for `.html`/`.htm`/`.xml` template names: `alerts.rs` used `*.html` names, whereas `ui.rs` (`"home"`, `"job"`, `"unsub"`) and `alerts_ui.rs` (`"alerts"`) omitted `.html`, leaving user-supplied values (such as `?address=` and `email`) unescaped (`AutoEscape::None`), while email/connector callers re-created `Environment::new()` on every call.

Fixed across all 5 `minijinja` modules (`#135`):
- **`src/web/ui.rs:5521-5536` (`UI_JINJA_ENV`):** Converted to `static UI_JINJA_ENV: Lazy<Environment<'static>>` with explicit `set_auto_escape_callback` (`AutoEscape::Html` for `.html`) and `.html` names (`home.html`, `job.html`, `unsub.html`). Preserved `| safe` solely for trusted HTML/SVG/JS/JSON slots (`SAFE_HOME_CLIENT_JS`, `safe_inline_json` for `job_id_json`/`slug_json`/`raw_slug_json`/`addr_json` to block `</script>` breakouts), passed `manage_url` in `unsub.html` through `safe_html_url_val`, and auto-escaped untrusted fields (`address`, `email`, `zips`, `step`, `brand_name`, `brand_domain`).
- **`src/web/alerts_ui.rs:991, 1050` (`ALERTS_UI_JINJA_ENV`):** Converted to `Lazy<Environment<'static>>` with explicit `set_auto_escape_callback` + `"alerts.html"`, auto-escaping `brand_name`, `brand_domain`, `page_title`, and UI text while keeping `| safe` JS/JSON/logo slots intact.
- **`src/search/alerts.rs:289, 495, 557, 845` (`ALERT_EMAIL_JINJA_ENV`):** Consolidated per-email `Environment::new()` parses (`match_alert.html`, `welcome_confirm.html`, `manage_link.html`) into one `Lazy<Environment<'static>>` with `AutoEscape::Html`, shared `resolve_origin_and_brand`, exported `pub(crate) fn safe_html_url_val`, and extracted `build_welcome_confirm_email_with_brand` / `build_manage_link_email_with_brand`.
- **`src/web/admin_console.rs:268, 863` (`ADMIN_EXPR_JINJA_ENV`) & `src/extensions/yaml_runner.rs:178, 199` (`YAML_EXPR_JINJA_ENV`):** Replaced per-call `Environment::new()` with `Lazy<Environment<'static>>`, intentionally preserving `AutoEscape::None` for URL/query/JSON connector expressions.
- **`tests/minijinja_escaping_test.rs`:** Direct render, `node --check`, and Axum `oneshot` (`/`, `/alerts`, `/api/subscriptions/unsubscribe`) tests verify XSS payloads (`"><script>alert(1)</script>&'` and `</script>` breakout), `| safe` JSON/JS integrity, and unescaped connector URL preservation.

---

## 5. OCI Always Free `MySQL.Free` Deployment, TLS & Operational Runbooks

> **Environment Isolation:** Specific tenancy names, OCIDs, security-list rules, and private network routing configs belong in local gitignored `.cache/oci-mysql-free.env` / `.cache/oci-infrastructure-notes.md` (`chmod 600`), never in tracked repository files.

### 5.1 OCI `MySQL.Free` Guardrails & `50 GiB` Backup Overlap Rule
- **Always Free Quota:** OCI `MySQL.Free` provides **`50 GiB` data storage + `50 GiB` backup storage** per region (`1` ECPU, `8 GiB` RAM, `@@innodb_buffer_pool_size = 4096 MiB`, `@@max_connections = 200`).
- **Locked Backup Policy on `MySQL.Free`:** Automatic daily backups are OCI-enforced (`is_enabled: true`, `retention_in_days: 1`, `soft_delete: DISABLED`, `pitr_policy: false`, manual backups disabled).
- **Daily Backup Overlap Headroom (`<= ~40–45 GiB`):** During the daily backup window, OCI completes `CREATE_BACKUP` before rotating yesterday's backup to `DELETE_SCHEDULED` (`~3–4` minutes where two backups briefly coexist). On PAYG accounts using Always Free resources, keep active database storage **`<= ~40–45 GiB`** (and clean up `RETAIN`ed backups if deleting a DB system) so overlapping daily snapshots never exceed the `50 GiB` free backup ceiling.
- **Cold Snapshot Supplement:** Periodically export portable SQLite WAL snapshots via `open-estate-server migrate --from "$MYSQL_URL" --to ...` for long-term off-box retention.

### 5.2 X.509 v1 Server Certificates & Transport Encryption
- **Driver Constraint (`sqlx 0.8` `tls-rustls`):** OCI MySQL `26.10.0-u1-cloud` auto-generates an **X.509 `Version: 1 (0x0)`** server certificate (`CN=MySQL_Endpoint_Server`). While OpenSSL CLI (`--ssl-mode=REQUIRED`) negotiates `TLSv1.3`, `rustls-webpki` (`Cargo.toml:33`) rejects X.509 v1 certificates (`UnsupportedCertVersion`).
- **Safe Operating Pattern:**
  - Never expose port `3306` on a public subnet (`0.0.0.0/0`). Bind the DB system to a private VCN subnet (`prohibit_public_ip_on_vnic = true`) reachable only from private VCN/peered CIDRs, site-to-site IPSec tunnels, or an SSH Bastion port-forward (`127.0.0.1:13306 -> <mysql-private-ip>:3306`).
  - Set `DATABASE_PROVIDER=mysql`, `OCI_HEATWAVE_ENABLE_RAPID=false`, and `?ssl-mode=DISABLED` in `MYSQL_URL` over the already encrypted VCN/IPSec/SSH-tunnel transport (`MYSQL_SSL_MODE=disabled`), where `caching_sha2_password` still protects credentials with RSA key exchange. *(If unencrypted in-VCN pods ever require MySQL-layer TLS against X.509 v1 certs, switch `sqlx` in `Cargo.toml:33` from `tls-rustls` to `tls-native-tls`.)*

### 5.3 Generic Operational Runbook Commands

#### 1. Migrate SQLite WAL -> OCI `MySQL.Free` & Start Server (Direct Private/IPSec Path)
```bash
set -a && source .cache/oci-mysql-free.env && set +a
# Ensure DATABASE_PROVIDER=mysql and OCI_HEATWAVE_ENABLE_RAPID=false are set
./target/release/open-estate-server migrate \
  --from .cache/openestate-search.sqlite \
  --to "$MYSQL_URL"
./target/release/open-estate-server serve
```

#### 2. Reverse Snapshot / Backup Export (OCI `MySQL.Free` -> Local SQLite WAL)
```bash
set -a && source .cache/oci-mysql-free.env && set +a
./target/release/open-estate-server migrate \
  --from "$MYSQL_URL" \
  --to ".cache/backup-openestate-$(date -u +%Y%m%d).sqlite"
```

#### 3. OCI Bastion SSH Tunnel Fallback (`127.0.0.1:13306`)
```bash
# Opens local SSH port-forward 127.0.0.1:13306 -> <mysql-private-ip>:3306 via OCI Bastion
.cache/oci-mysql-tunnel.sh 13306

set -a && source .cache/oci-mysql-free.env && set +a
MYSQL_URL="$MYSQL_TUNNEL_URL" ./target/release/open-estate-server migrate \
  --from .cache/openestate-search.sqlite \
  --to "$MYSQL_TUNNEL_URL"
MYSQL_URL="$MYSQL_TUNNEL_URL" ./target/release/open-estate-server serve
```

#### 4. Live Template Dev Reload (Hot UI Edits Without Rebuilding Binary)
```bash
# Caches Arc<CompiledReportTemplate> by file len+mtime; invalidates on edit + rotates ETag
OPENESTATE_TEMPLATE_DEV_RELOAD=1 ./target/release/open-estate-server serve
```

#### 5. Check `50 GiB` Storage & Backup Overlap Headroom
```sql
SELECT
  table_name,
  table_rows,
  ROUND((data_length + index_length) / 1048576, 2) AS total_mib,
  ROUND(data_free / 1048576, 2) AS free_mib
FROM information_schema.TABLES
WHERE table_schema = 'openestate'
ORDER BY (data_length + index_length) DESC;
```

---

## 6. Historical `LONGTEXT` Audit (`v2`, `#151`) & Superseded Schema Plan

An end-to-end audit (`#151`) of the legacy `v2` 26-table schema (`src/db/schema.rs`, `src/db/mod.rs`, `src/db/migrate.rs`, `src/search/{matcher,alerts,sync,rank,light_scan}.rs`, `src/web/{server,match_api}.rs`) identified the structural causes of byte overfetch and WAN MySQL latency that drove the `v3` clean-slate redesign in Section 7:

1. **Wide 85-Column `listings` + `LONGTEXT` Blobs (`SELECT *` Everywhere):** Every `listings` reader issued `SELECT *` across 85 columns including `features_text`, `raw_json`, and `tags_json` (`LONGTEXT`), sending **`3,701 B` SQL / `18,499 B` MySQL wire** just to read 16 B (`price`, `status`).
2. **Embedded JSON in Hot Tables:** `listing_factors.factors_json LONGTEXT` (`3.7 KB`), `subscriptions.{zips_json,cities_json,parsed_filters_json,digest_pending_json} LONGTEXT` (`~2 KB`), and monolithic `cached_reports.report_zstd LONGBLOB` (`~452 KB` uncompressed) forced full JSON/zstd parsing for 2 scalars (`crime_score`, `monthly_total`), admin header tables, or 1 report section.
3. **Hot-GET Unindexed Scan & Unindexed `report_slug`:** `GET /api/listings` and `GET /api/listings/shop` executed `backfill_missing_shopping_facets` (`WHERE electric_carrier IS NULL OR fiber_available IS NULL LIMIT 400`), scanning all 25,000 wide rows on every GET (`Handler_read_rnd_next = 25,001`). `list_listings_for_engagement` scanned all 25,000 rows on unindexed `report_slug`.
4. **Silent 500-Row `/api/listings/shop` Facet Truncation:** Fetching `2x LIMIT 500` `ListingRow`s to aggregate facets in Rust transferred `3.53 MiB` over TCP while dropping `97.78%` (`22,000 / 22,500`) of active catalog counts.
5. **Sequential N+1 Disasters Over WAN MySQL:** `rank_area` fired `1,015` sequential queries (`11.52 s` MySQL p50) and `evaluate_alerts_for_listings` fired `12,697` sequential queries (`157.25 s` / `2.62 min` MySQL p50).
6. **Natural `VARCHAR(180)` utf8mb4 PK Bloat & Non-Idempotent Rapid DDL:** Duplicating 180-char utf8mb4 IDs across all 9 secondary indexes bloated `INDEX_LENGTH` (`20.64 MiB` at 25K rows), while `ALTER TABLE ... SECONDARY_ENGINE = RAPID` / `SECONDARY_LOAD` lacked idempotency checks and `migrate` unloading.

All six bottlenecks above were resolved in the clean-slate `v3` Protobuf-first refactor documented in Section 7.

---

## 7. Shipped `v3` Protobuf-First Schema (`openestate.db.v1`) & Empirical Benchmark Gate (`SEED = 0x0E57A7E`)

### 7.1 Clean-Slate 44-Table Architecture (`proto/openestate/db/v1/*.proto` + `src/db/`)

The `v3` schema compiles five `.proto` contracts (`enums.proto`, `listing.proto`, `factors.proto`, `subscription.proto`, `report.proto` under `proto/openestate/db/v1/`) via `prost-build` (`build.rs`) and maps them cleanly across **44 relational tables** in both `SQLITE_DDL_STMTS` and `MYSQL_DDL_STMTS` (`src/db/schema.rs`):

| Domain Group | Tables (`44` Total) | Key `v3` Storage & Access Design |
|---|---|---|
| **A. Property & Listing (`10`)** | `listings` *(Rapid)*, `listing_schools` *(Rapid)*, `listing_utilities` *(Rapid)*, `listing_hazards` *(Rapid)*, `listing_features` *(Rapid)*, `listing_tags` *(Rapid)*, `listing_engagement`, `listing_portal_signals`, `listing_descriptions`, `listing_events` | Narrow 42-col searchable core `listings` (`listing_sk BIGINT` PK, `external_id`/`report_slug`/`zip`/`state_code` in `ascii_bin`, integer enums `TINYINT`, epoch days `INT`, signed 62-bit `tag_bits`/`known_bits BIGINT`). Large URLs, remarks, and 1:1 sub-domains sit in dedicated `listing_sk`-keyed tables (`idx_listings_report_slug` eliminates `Q0a_slug` full scans). |
| **B. Scoring Factors & Schools (`5`)** | `listing_factors` *(Rapid)*, `school_metrics` *(Rapid)*, `listing_factor_provenance`, `zip_school_cache`, `zip_school_cache_entries` | `listing_factors` promotes all 67 scalar `HomeFactors` columns cleanly (`HomeFactorsRowProto`, 0 JSON!) for direct SQL/Rapid batch scoring, isolating verbose citations and style clues (`FactorProvenanceProto` protobuf) into cold `listing_factor_provenance`. |
| **C. Reports & Section Frames (`5`)** | `cached_reports`, `cached_report_sections`, `zip_market_snapshots`, `report_build_queue`, `report_refresh_log` | `cached_reports` stores crisp indexed headers (`report_id BIGINT` PK, `slug`, `lat`/`lon`, `overall_score`, `estimated_value`, `verdict`, `ai_ready`, `osrm_degraded`, `section_mask`) so `/admin` & OSRM scans touch `0` zstd blobs. Full report JSON is decomposed into up to **27 `ReportSectionKind` dictionary-compressed frames** (`cached_report_sections`, PK `(report_id, section_kind)`) so 1-section reads (`get_report_section`) and updates (`put_report_section`) touch only the target frame. `zip_market_snapshots` uses `BINARY(32)` SHA-256 keys (`snapshot_hash`) in MySQL (`1.63x` smaller). |
| **D. Subscriptions, Alerts & Sync (`8`)** | `watched_zips`, `subscriptions`, `subscription_zips`, `subscription_cities`, `subscription_keywords`, `subscription_digest_queue`, `subscription_notifications`, `alert_manage_tokens` | Replaces 4 `LONGTEXT` JSON columns with typed scalar filter bounds, signed `required_tag_bits`/`excluded_tag_bits BIGINT`, normalized `subscription_zips` / `subscription_cities` / `subscription_keywords` join tables, atomic `subscription_digest_queue(subscription_sk, listing_sk)` upserts (no RMW JSON arrays), `BINARY(32)` `alert_manage_tokens`, and batched `event_key IN (...)` deduplication. |
| **E. Engagement & Match (`6`)** | `property_likes`, `property_platform_signals`, `user_profiles`, `user_profile_places`, `match_feedback`, `user_search_history` | Normalized `UserProfileProto` scalar columns + `user_profile_places`, 11-dimension feedback weights (`match_feedback`), `WITHOUT ROWID` SQLite primary keys for compact clustered lookup. |
| **F. Marketplace, Admin & Binary Caches (`10`)** | `agent_match_requests`, `recommended_agents`, `recommended_agent_areas`, `recommended_agent_tags`, `system_modules`, `installed_extensions`, `app_users`, `admin_audit_log`, `llm_cache`, `client_doc_reviews` | `BINARY(32)` SHA-256 keys + dictionary-zstd Protobuf envelopes (`LlmCacheEntryProto`, `ClientDocReviewProto`, `InstalledExtensionManifestProto`). |

**Enforced `v3` Structural Invariants (`schema_v3_perf_bench` & `src/db/schema.rs`):**
- **Zero `JSON` columns** and **zero `LONGTEXT` columns** across all 44 tables in both SQLite WAL and MySQL `26.10.0-u1-cloud`.
- **Zero `FOREIGN KEY` constraints:** Application-deterministic 62-bit surrogate keys (`deterministic_sk` = truncated SHA-256 masked to `0x3FFF_FFFF_FFFF_FFFF`) enable lock-free parallel bulk seeding and 200-row streaming migration across arbitrary table orders.
- **HeatWave Rapid-Safe `DIV / MOD` Bitmasks (`HW_ER_1014` workaround):** Live OCI MySQL `26.10.0-u1-cloud` HeatWave Rapid rejects bitwise `&` (`HW_ER_1014`). `sql_bit_is_set_expr` (`src/db/schema.rs`) emits `((col DIV pow2) MOD 2) = 1` on MySQL (`(col & pow2) = pow2` on SQLite) for canonical `FeatureTag` bits `1..=35` inside signed 64-bit `BIGINT` (`tag_bits`), offloading natively into Rapid hash/aggregation plans.
- **Idempotent Rapid Lifecycle & `migrate` Integration (`src/db/mod.rs`, `src/db/migrate.rs`):** Only the `8` pure-scalar/short-VARCHAR tables (`RAPID_TABLE_NAMES`) carry `SECONDARY_ENGINE = RAPID` (`0` `BLOB`/`TEXT` columns). `ensure_rapid_tables_loaded` inspects `performance_schema.rpd_tables` (`AVAIL_RPDGSTABSTATE`), and `migrate_database_with_batch` automatically calls `unload_rapid_tables_if_loaded()`, streams all 44 tables via `AS new_row ON DUPLICATE KEY UPDATE`, runs `ANALYZE TABLE`, and reloads Rapid via `analyze_and_reload_rapid_after_bulk()`.

---

### 7.2 Empirical Storage Footprint (`SEED = 0x0E57A7E`, Before `v2` vs. After `v3`)

Measured with `tests/schema_v3_perf_bench.rs` (`--release --offline --locked`, Stage 4 final pass) against the identical deterministic corpus generated in `v2` (`25,000` listings across `50` ZIPs / `25` metros, `25,000` `listing_factors`, `5,000` active subscriptions, `10,000` notifications, `200` full reports `avg = 451,976 B` raw across `4,210` section frames, and `150` `zip_market_snapshots`), using `dbstat` + `wal_checkpoint(TRUNCATE)` on **SQLite `3.46.0` WAL** and `ANALYZE TABLE` + `INFORMATION_SCHEMA.TABLES` + `performance_schema.rpd_tables` inside isolated ephemeral **`openestate_bench_v3`** on **OCI MySQL `26.10.0-u1-cloud` (`MySQL.Free` / `HeatWave.Free` `16 GiB`)**:

| Schema / Table Slice (`25K` Corpus) | Rows (`v3`) | `v2` SQLite WAL | **`v3` SQLite WAL** | `v2` MySQL InnoDB | **`v3` MySQL InnoDB** *(Data / Idx)* | **`v3` HeatWave Rapid RAM** | Net Storage Gain (`v2 -> v3`) |
|---|---:|---:|---:|---:|---:|---:|---|
| **`listings` (hot searchable core)** | `25,000` | `103.37 MiB` | **`13.19 MiB`** *(7.46 / 5.73)* | `149.56 MiB` *(128.9 / 20.6)* | **`22.64 MiB`** *(10.52 / 12.12)* | **`8.00 MiB`** | **`7.84x` SQLite / `6.61x` MySQL smaller** *(index bloat `-41%`)* |
| `listing_utilities` | `25,000` | *(in `listings`)* | `8.06 MiB` | *(in `listings`)* | `11.55 MiB` *(8.52 / 3.03)* | `8.00 MiB` | Isolated utility & broadband slice |
| `listing_tags` *(normalized tags)* | `153,849` | *(in `tags_json`)* | `7.90 MiB` | *(in `tags_json`)* | `19.06 MiB` *(11.55 / 7.52)* | `8.00 MiB` | Indexed `(listing_sk, tag_code)` |
| `listing_engagement` / `descriptions` | `25,000` ea | *(in `listings`)* | `7.00` + `5.94 MiB` | *(in `listings`)* | `9.03` + `9.03 MiB` | `0` *(InnoDB only)* | Cold portal URLs & MLS remarks isolated off hot scan path |
| `listing_features` / `schools` / `hazards` | `25,000` ea | *(in `listings`)* | `3.56` + `4.45` + `1.76 MiB` | *(in `listings`)* | `7.03` + `6.52` + `4.03 MiB` | `4.00` + `4.00` + `4.00 MiB` | Per-domain 1:1 vertical slicing (`Q0b`) |
| **`listing_factors` (hot 67-col core)** | `25,000` | `100.46 MiB` | **`10.68 MiB`** *(7.42 / 3.26)* | `188.95 MiB` *(183.9 / 5.03)* | **`21.12 MiB`** *(13.55 / 7.58)* | **`12.00 MiB`** | **`9.41x` SQLite / `8.95x` MySQL smaller** |
| `listing_factor_provenance` *(cold citations)* | `25,000` | *(in `factors_json`)* | `54.57 MiB` | *(in `factors_json`)* | `69.61 MiB` *(69.61 / 0.00)* | `0` *(InnoDB only)* | Cold audit protobuf blobs never loaded during `rank_area` |
| `cached_reports` + `cached_report_sections` | `200` / `4,210` | `6.00 MiB` | `0.14` + `11.57 MiB` | `7.55 MiB` | `0.19` + `10.52 MiB` | `0` *(InnoDB only)* | `0.19 MiB` zero-decode header scan + 27-section frames |
| `subscriptions` *(+ normalized children)* | `5,000` | `11.49 MiB` | `3.21` + `1.45 MiB` | `14.86 MiB` | `4.70` + `3.19 MiB` | `0` | Replaces 4 `LONGTEXT` JSON blobs + pushdown `subscription_zips`/`cities` |
| `subscription_notifications` + `digest_queue` | `10,000` / `500` | `3.71 MiB` | `4.50` + `2.25 MiB` | `6.55 MiB` | `6.58` + `1.53 MiB` | `0` | Dedicated atomic `subscription_digest_queue` |
| `zip_market_snapshots` | `150` | `0.83 MiB` | `0.86 MiB` | `2.53 MiB` | **`1.55 MiB`** | `0` | **`1.63x` smaller MySQL** via `BINARY(32)` `snapshot_hash` |
| **Total Database (`25K` Corpus)** | **44 tables** | **`226.21 MiB`** | **`141.51 MiB`** (`148.4 MB`) | **`370.72 MiB`** | **`208.86 MiB`** (`219.0 MB`) | **`48.00 MiB`** (`0.3%` node) | **`-37.4%` (`1.60x`) SQLite / `-43.7%` (`1.77x`) MySQL** |
| **Scale-Up Analytical (`150K` rows)** | `150K` x 6 tbl | — | — | — | **`237.00 MiB DATA` (`356.81 MiB` tot)** | **`160.00 MiB`** (`1.0%` node) | Fits cleanly inside `HeatWave.Free` (`1.84 / 15.00 GiB` node RAM) |

---

### 7.3 All 17 Operations (`Q0a–Q6_rmw`): Before (`v2`) vs. After (`v3` Stage 4 Polish: SQLite WAL / InnoDB / HeatWave Rapid)

- **Network Calibration During Stage 4 Benchmark (`v3_stage4.json`):** Home client -> OCI private VCN over IPSec (`MTU 1400`) measured **`11.15 ms` (`Stage A`) / `10.95 ms` (`Stage B`)** p50 1-RTT (`SELECT 1`, vs. `~6.1 ms` during `v2` afternoon baseline); `DbStore::ping` (`test_before_acquire(true)`) measured **`24.79 ms` (`min 9.09 ms`)** in Stage A (elevated early-run VPN jitter on first TCP flows) and **`12.54 ms` (`min 9.50 ms`)** in Stage B. Because `sql_q`, wire bytes (`wire_rx`), and handler scans (`rnd_scan`) are network-invariant, both latency percentiles (`p50 / p95 / p99`) and wire/RTT counts are reported below.
- **Stage 4 Multi-RTT & Rapid Gate Optimizations Included Here:**
  1. Single-pass `compute_shop_facets_sql` (`src/db/mod.rs`) replaces 7 separate table scans with 1 SQL pass (`sql_q: 14 -> 4`, SQLite `347.2 -> 101.3 ms`, Stage A InnoDB `855.9 -> 492.7 ms`, Stage B Rapid `474.7 -> 299.1 ms`).
  2. Cost-gated `DbStore::rapid_query_hint_for_scan(zip_count, has_exact_city)` + removal of `rapid_query_hint()` from PK `IN (...)` lookups (`src/search/light_scan.rs`) eliminates all Stage B OLTP regressions (`Q1_pure_sql` `56.94 -> 16.37 ms`, `rpd_off = 0`).
  3. `tokio::try_join!` + single-JOIN `hydrate_listing_cards` (`Q1_hot_get` `sql_q: 8 -> 3`), parallel/joined `build_engagement_for_report` (`Q0a_slug` `sql_q: 7 -> 2`), and 2-statement `put_report_section` (`Q6_rmw` `sql_q: 6 -> 3`, InnoDB `82.8 -> 56.5 ms`).

#### A. Wire Payload, JSON Decompression & SQL Plan Discipline (`v2` vs. `v3` Stage 4)

| Op ID | `v2` Defect Eliminated | SQL Payload (`v2 -> v3`) | Decompressed/Parsed (`v2 -> v3`) | MySQL Wire RX (`v2 -> v3`) | `sql_q` (`v2 -> v3`) | `rnd_scan` (`v2 -> v3`) |
|---|---|---|---|---|---|---|
| **`Q0a`** | 85-col `SELECT *` for `price + status` | `3,701 B -> 48 B` (**`77.1x`**) | `3,701 B -> 0 B` | `18,499 B -> 2,065 B` (**`9.0x`**) | `1 -> 1` | `0 -> 0` |
| **`Q0a_slug`** | Unindexed 25K scan on `report_slug` | `3,701 B -> 136 B` (**`27.2x`**) | `3,701 B -> 0 B` | `18,528 B -> 5,194 B` (**`3.6x`**) | `1 -> 2` | **`25,001 -> 1`** |
| **`Q0b`** | 85-col `SELECT *` for `utilities`/`hazards` | `3,701 B -> 141 B` (**`26.2x`**) | `3,701 B -> 0 B` | `11,238 B -> 4,693 B` (**`2.4x`**) | `1 -> 1` | `0 -> 0` |
| **`Q0c`** | `factors_json LONGTEXT` for 2 scalars | `3,817 B -> 24 B` (**`159x`**) | `3,675 B -> 0 B` | `6,903 B -> 1,120 B` (**`6.2x`**) | `1 -> 1` | `0 -> 0` |
| **`Q0d`** | Full 441 KiB report decode for 1 section | `35,887 B -> 1,540 B` (**`23.3x`**) | `451,976 B -> 13,037 B` (**`34.7x`**) | `37,721 B -> 1,194 B` (**`31.6x`**) | `2 -> 1` | `0 -> 0` |
| **`Q1_pure_sql`** | `LOWER(tags_json) LIKE` + 85-col `SELECT *` | `89,460 B -> 4,032 B` (**`22.2x`**) | `89,460 B -> 0 B` | `103,838 B -> 14,229 B` (**`7.3x`**) | `1 -> 1` | `0 -> 0` |
| **`Q1_highlights`** | 12x N+1 cold 452 KB report decodes | `520,104 B -> 21,910 B` (**`23.7x`**) | **`5,513,172 B -> 0 B`** | `546,752 B -> 65,197 B` (**`8.4x`**) | `25 -> 4` | `0 -> 0` |
| **`Q1_hot_get`** | Unindexed 25K backfill scan on every GET | `89,460 B -> 20,808 B` (**`4.3x`**) | `89,460 B -> 0 B` | `110,672 B -> 44,933 B` (**`2.5x`**) | `2 -> 3` | **`25,001 -> 0`** |
| **`Q2_facets`** | 2x `LIMIT 500` Rust facets (**2.2% sample bug**) | `3.53 MiB -> 12,544 B` (**`295x`**) | `3.53 MiB -> 0 B` | `3,668,798 B -> 116,382 B` (**`31.5x`**) | `2 -> 4` | **`21,250/21,250` = 100%** *(active, fixed!)* |
| **`Q3_factors`** | 500x `factors_json LONGTEXT` parse | `1,908,370 B -> 92,000 B` (**`20.7x`**) | `1,837,701 B -> 0 B` | `1,849,980 B -> 208,600 B` (**`8.9x`**) | `3 -> 1` | `0 -> 0` |
| **`Q3_rank_e2e`** | 1,015 N+1 `find_cached_report_for_listing` | `4,971,216 B -> 183,920 B` (**`27.0x`**) | `8,229,259 B -> 0 B` | `8,462,208 B -> 765,724 B` (**`11.1x`**) | **`1,015 -> 17`** | `0 -> 2,010` |
| **`Q4_subs_scan`** | 5,000x full-table JSON scan/parse | `10,044,024 B -> 1,275,591 B` (**`7.9x`**) | `8,292,312 B -> 0 B` | `10,024,749 B -> 2,814,278 B` (**`3.6x`**) | `1 -> 4` | `0 -> 5,502` *(all-50-ZIP batch)* |
| **`Q4_fanout`** | 12,697 N+1 queries + digest JSON RMW | `10.64 MB -> 1.32 MB` (**`8.1x`**) | `8,842,312 B -> 0 B` | `24,129,297 B -> 2,645,198 B` (**`9.1x`**) | **`12,697 -> 823`** | `0 -> 5,502` |
| **`Q5_admin`** | 200x full-report zstd+JSON decode | `6,161,424 B -> 28,679 B` (**`215x`**) | **`90,395,350 B -> 0 B`** | `6,165,050 B -> 32,900 B` (**`187x`**) | `2 -> 2` | `201 -> 201` |
| **`Q5_osrm`** | 200x full-report zstd+JSON rehydrate | `6,421,424 B -> 77,099 B` (**`83.3x`**) | **`90,395,350 B -> 0 B`** | `6,969,835 B -> 82,360 B` (**`84.6x`**) | `3 -> 1` | `201 -> 201` |
| **`Q6_ssr`** | Parity check (`2.45 MiB` HTML SSR) | `35,887 B -> 38,491 B` (`1.07x`) | `451,976 B -> 457,986 B` | `36,787 B -> 51,904 B` *(header cols x21)* | `2 -> 2` | `0 -> 0` |
| **`Q6_rmw`** | Full 452 KB report level-9 zstd RMW | `35,887 B -> 240 B` + `2.6 KB wr` | `451,976 B -> 22,074 B` (**`20.5x`**) | `67.7 KB -> 6,063 B rx + 2.6 KB tx` (**`11.2x rx`**) | `3 -> 3` | `0 -> 0` |

#### B. End-to-End Latency (`p50 / p95 / p99`, ms) Across SQLite WAL, OCI MySQL InnoDB & HeatWave Rapid

| Op ID | `v2` SQLite WAL (`p50 / p95 / p99`) | **`v3` SQLite WAL (`p50 / p95 / p99`)** | SQLite Gain | `v2` MySQL InnoDB (`p50 / p95 / p99`) | **`v3` Stage A InnoDB (`p50 / p95 / p99`)** | InnoDB Gain | **`v3` Stage B Rapid (`p50 / p95 / p99`)** | `rpd_off` (Stage B) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **`Q0a`** | `0.031 / 0.033 / 0.035` | **`0.014 / 0.022 / 0.027`** | **`2.18x`** | `11.84 / 14.60 / 17.90` | `24.51 / 31.35 / 36.86` *(min `9.16`)* | *(1-RTT; VPN `24.8`ms)* | **`12.24 / 26.96 / 29.48`** | `0` |
| **`Q0a_slug`** | `16.601 / 17.866 / 18.113` | **`0.065 / 0.170 / 0.205`** | **`257x`** | `71.01 / 120.23 / 125.25` | **`26.53 / 41.46 / 49.04`** | **`2.68x`** | **`15.62 / 30.20 / 50.00`** | `0` *(`4.55x` vs `v2`)* |
| **`Q0b`** | `0.031 / 0.048 / 0.104` | **`0.015 / 0.024 / 0.033`** | **`2.10x`** | `11.46 / 13.90 / 17.05` | `24.43 / 38.47 / 42.27` *(min `9.33`)* | *(1-RTT; VPN `24.8`ms)* | **`11.49 / 25.88 / 30.22`** | `0` |
| **`Q0c`** | `0.023 / 0.031 / 0.043` | **`0.012 / 0.019 / 0.023`** | **`1.88x`** | `11.55 / 16.17 / 18.33` | `24.62 / 37.18 / 38.72` *(min `8.98`)* | *(1-RTT; VPN `24.8`ms)* | **`11.20 / 25.67 / 28.87`** | `0` |
| **`Q0d`** | `3.425 / 4.622 / 4.926` | **`0.074 / 0.103 / 0.157`** | **`46.3x`** | `35.52 / 44.65 / 51.67` | **`24.95 / 39.09 / 40.12`** | **`1.42x`** | **`11.48 / 26.30 / 34.71`** | `0` *(`3.09x` vs `v2`)* |
| **`Q1_pure_sql`** | `2.855 / 3.145 / 3.577` | **`1.394 / 1.808 / 2.154`** | **`2.05x`** | `35.50 / 44.83 / 47.32` | **`28.03 / 43.31 / 44.99`** | **`1.27x`** | **`16.37 / 31.79 / 33.71`** | **`0`** *(`2.17x` vs `v2`; gated!)* |
| **`Q1_highlights`** | `10.012 / 36.502 / 122.08` | **`4.902 / 5.821 / 6.445`** | **`2.04x`** *(`18.9x` p99)* | `98.66 / 281.04 / 331.74` | **`94.96 / 134.09 / 179.28`** | **`1.04x`** *(`1.85x` p99)* | `97.99 / 152.38 / 219.03` | `1` *(12-ZIP outer)* |
| **`Q1_hot_get`** | `20.253 / 20.859 / 21.322` | **`1.981 / 2.242 / 2.328`** | **`10.2x`** | `107.52 / 170.72 / 171.30` | **`56.89 / 79.24 / 92.98`** | **`1.89x`** | **`42.43 / 54.14 / 59.01`** | **`0`** *(`2.53x` vs `v2`)* |
| **`Q2_facets`** | `86.851` *(2.2% cap bug)* | `101.35 / 136.19 / 139.85` *(100%)* | Fixes `97.8%` bug | `429.51` *(2.2% cap bug)* | `492.67 / 1,013.4 / 1,351.6` *(100%)* | Fixes `97.8%` bug | **`299.10 / 491.93 / 497.32`** | **`2`** *(`1.65x` vs A; faster than `v2`!)* |
| **`Q3_factors`** | `6.169 / 6.325 / 6.540` | `6.581 / 7.500 / 8.027` | `0.94x` *(`20.7x` < B)* | `129.25 / 164.77 / 182.48` | **`77.73 / 104.45 / 107.48`** | **`1.66x`** | **`35.75 / 56.66 / 101.28`** | **`0`** *(`3.61x` vs `v2`)* |
| **`Q3_rank_e2e`** | `103.355 / 109.521 / 125.7` | **`31.372 / 35.910 / 41.055`** | **`3.29x`** | `11,521.38 / 12,156.9 / 12,156.9` | **`133.03 / 216.98 / 221.55`** | **`86.6x`** | **`127.90 / 239.19 / 244.03`** | **`0`** *(`90.1x` vs `v2`)* |
| **`Q4_subs_scan`** | `40.657` *(flat scan)* | `47.723 / 56.365 / 56.991` *(50-ZIP)* | Matches `2,264` w/ profiles | `251.55 / 483.68 / 733.92` | `287.62 / 381.75 / 389.86` *(3.6x < wire)* | `0.87x` | `358.71 / 623.79 / 646.81` | `0` |
| **`Q4_fanout`** | `1,040.79 / 1,188.1 / 1,384.3` | **`323.82 / 338.87 / 339.55`** | **`3.21x`** | `157,252.9 / 163,315.1` (`2.62m`) | **`26,547.4 / 28,721.3`** (`26.5s`) | **`5.92x`** | **`22,953.1 / 24,195.0`** (`23.0s`) | `0` *(`6.85x` vs `v2`)* |
| **`Q5_admin`** | `370.289 / 421.01 / 448.60` | **`0.692 / 0.918 / 1.096`** | **`535x`** | `1,174.75 / 1,217.34 / 1,235.95` | **`63.92 / 89.79 / 94.62`** | **`18.4x`** | **`65.13 / 90.30 / 98.39`** | `0` |
| **`Q5_osrm`** | `358.366 / 379.48 / 427.13` | **`1.030 / 1.375 / 1.391`** | **`348x`** | `862.44 / 979.79 / 1,035.55` | **`32.78 / 69.68 / 73.06`** | **`26.3x`** | **`55.19 / 71.98 / 73.29`** | `0` |
| **`Q6_ssr`** | **`10.308 / 11.497 / 12.687`** | `12.004 / 14.381 / 16.536` | **`0.86x` (`+1.7 ms`)** | **`44.64 / 54.34 / 56.95`** | `94.50 / 136.71 / 139.59` *(2 RTT; VPN)* | `0.47x` | `97.35 / 118.27 / 137.93` | `0` |
| **`Q6_rmw`** | `11.314 / 27.290 / 33.373` | **`0.489 / 0.927 / 14.573`** | **`23.1x`** | `57.07 / 63.72 / 64.95` | **`56.52 / 93.71 / 116.00`** | **`1.01x`** | `79.35 / 122.50 / 125.37` | `0` |

**Key Stage 4 Optimization Outcomes:**
- **`Q2_facets` Single-Scan Collapse:** Rewriting `compute_shop_facets_sql` from 7 sequential scans into 1 disjunctive conditional aggregation scan cut SQL queries from `14 -> 4`, speeding up SQLite **`3.43x` (`347.2 -> 101.35 ms`)**, Stage A InnoDB **`1.74x` (`855.9 -> 492.67 ms`)**, and Stage B Rapid **`1.59x` (`474.7 -> 299.10 ms`)** — beating even `v2`'s broken 2.2%-sample cache while counting `100%` of active catalog homes (`21,250 / 21,250`).
- **Smart Rapid Cost Gate Eliminates All Stage B Regressions:** `Q1_pure_sql` dropped from `56.94 ms` (`rpd_off = 1`) down to **`16.37 ms` (`rpd_off = 0`, `2.17x` vs `v2`)**, `Q1_hot_get` dropped from `70.58 -> 42.43 ms` (`rpd_off = 0`, `2.53x` vs `v2`), and `Q3_factors` dropped to **`35.75 ms` (`rpd_off = 0`, `3.61x` vs `v2`)**.
- **`Q6_rmw`, `Q0a_slug`, `Q1_hot_get` & `Q4_subs_scan` Round-Trip Reductions:** Collapsing `put_report_section` to 2 SQL statements (`sql_q: 6 -> 3`) brought MySQL `Q6_rmw` to **`56.52 ms` (parity with `v2`'s `57.07 ms`)** and SQLite to **`0.489 ms` (`23.1x`)**; parallelizing/joining `hydrate_listing_cards`, `engagement`, and `match_subscriptions_for_events` cut SQLite `Q4_subs_scan` **`5.23x` (`249.5 -> 47.72 ms`)**, `Q0a_slug` to **`0.065 ms` (`257x`) / `15.62 ms` (Stage B)**, and `Q1_hot_get` to **`1.981 ms` (`10.2x`)**.
- **`Q6_ssr`:** Uses single-RTT `cached_reports cr JOIN cached_report_sections crs` (`12.00 ms` SQLite, within `1.7 ms` of `v2`) while eliminating `rendered_html` persistence completely; Stage A `Q6_ssr` reflects the `~24.8 ms` multi-RTT IPSec jitter (`~44 ms` in 11ms steady-state windows).

---

### 7.4 `25,000` vs. `150,000` Scale-Up Analytical Gate (`Stage A InnoDB` vs. `Stage B HeatWave.Free` 1-Core Rapid)

With all `8` `RAPID_TABLE_NAMES` loaded (`48.00 MiB` Rapid RAM @ 25K, `160.00 MiB` @ 150K; `237.00 MiB` InnoDB `DATA_LENGTH` / `356.81 MiB` total @ 150K):

| Gate Query (`OFF` vs. `FORCED` unless noted) | Candidate Rows (`25K`) | `25K` InnoDB p50 | **`25K` Rapid p50** (`Speedup`) | Candidate Rows (`150K`) | `150K` InnoDB p50 | **`150K` Rapid p50** (`Speedup`) | Parity / Notes |
|---|---:|---:|---:|---:|---:|---:|---|
| **`AG1_shop_facets_15bucket`** *(single-scan, auto-offload)* | `25,000` | `248.80 ms` *(`rpd_off=1`)* | `269.33 ms` (`0.92x`) | `150,000` | `900.25 ms` *(`rpd_off=1`)* | `860.65 ms` (`1.05x`) | Stage 4 single-scan `compute_shop_facets_sql` (`1` query, auto-offloaded to Rapid once loaded); pure InnoDB Stage A `Q2_facets` (`492.67 ms`) vs Stage B Rapid (`299.10 ms`) @ 25K = **`1.65x` speedup**, `100%` match |
| **`AG2_cross_state_5table_agg`** *(5-tbl `GROUP BY`)* | `25,000` | **`56.60 ms`** | `213.41 ms` (`0.27x`) | `150,000` | `1,156.38 ms` | **`290.09 ms` (`3.99x`)** | **25K**: exact match. **150K CORRECTNESS ANOMALY**: InnoDB returned `72` unique `(state_code, property_category)` rows (`sum=86,916`); Rapid `26.10.0-u1-cloud` returned **`360` rows (`5 x 72`, `sum=86,916`)** due to unmerged per-segment hash aggregates! |
| **`AG3_join_top200_01zips`** (`1` ZIP) | `~500` | **`23.32 ms`** | `43.57 ms` (`0.54x`) | `~3,000` | `55.66 ms` | **`46.39 ms` (`1.20x`)** | At `< 5K` rows InnoDB's `idx_listings_zip_status_price` avoids Rapid offload overhead (`0.54x` @ 25K) |
| **`AG3_join_top200_02zips`** (`2` ZIPs) | `~1,000` | **`31.16 ms`** | `54.83 ms` (`0.57x`) | `~6,000` | **`55.50 ms`** | `59.04 ms` (`0.94x`) | Below `5`-ZIP Rapid cost-gate threshold (`use_secondary_engine=OFF`) |
| **`AG3_join_top200_05zips`** (`5` ZIPs) | `~2,500` | **`48.16 ms`** | `61.52 ms` (`0.78x`) | `~15,000` | `88.59 ms` | **`53.12 ms` (`1.67x`)** | **Crossover threshold** (`zip_count >= 5`): Rapid pulls **`1.67x` ahead** past `~10K–15K` rows |
| **`AG3_join_top200_10zips`** (`10` ZIPs) | `~5,000` | `59.71 ms` | **`48.41 ms` (`1.23x`)** | `~30,000` | `157.80 ms` | **`62.63 ms` (`2.52x`)** | **Rapid wins `2.52x`** at `30K` candidate rows |
| **`AG3_join_top200_25zips`** (`25` ZIPs) | `~12,500` | `117.01 ms` | **`53.69 ms` (`2.18x`)** | `~75,000` | `613.83 ms` | **`77.01 ms` (`7.97x`)** | **Rapid wins `7.97x` (`613.8 -> 77.0 ms`)** |
| **`AG3_join_top200_50zips`** (`50` ZIPs) | `25,000` | `124.10 ms` | **`48.82 ms` (`2.54x`)** | `150,000` | `657.24 ms` | **`57.85 ms` (`11.36x`)** | **Peak Rapid win: `11.36x` faster (`657.2 -> 57.9 ms`)** |
| **`AG4_pk_batch_in500`** (`IN 500 PKs`) | `500` | **`46.15 ms`** | `90.76 ms` (`0.51x`) | `500` | **`68.02 ms`** | `100.22 ms` (`0.68x`) | **InnoDB clustered PK B-tree ALWAYS wins (`1.5x–2.0x`)** -> Rapid hint removed in Stage 4 |

#### Strict Benchmark Gate Verdict & Shipped Stage 4 Rapid Cost Gate
1. **Clustered-PK Removal + Cost-Gated `rapid_query_hint_for_scan` (Shipped in Stage 4):**
   - `DbStore::rapid_query_hint_for_scan(&self, zip_count: usize, has_exact_city: bool)` (`src/db/mod.rs`) emits `/*+ SET_VAR(use_secondary_engine=FORCED) */ ` **only when `rapid_online && (zip_count >= 5 || (zip_count == 0 && !has_exact_city))`**, emitting `/*+ SET_VAR(use_secondary_engine=OFF) */ ` for narrow `1–4` ZIP or exact-city OLTP queries, while `src/search/light_scan.rs` no longer attaches Rapid hints to PK/ID `IN (...)` batch queries.
   - Result: Stage B eliminates the `Q1_pure_sql` regression completely (`56.94 -> 16.37 ms`, `rpd_off = 0`) and accelerates `Q1_hot_get` (`42.43 ms`, `rpd_off = 0`), `Q3_factors` (`35.75 ms`, `rpd_off = 0`), and `Q2_facets` (`299.10 ms`, `rpd_off = 2`) simultaneously.
2. **Default Deployment & Known Engine Caveat:**
   - **Small catalogs (`< 50K` listings) on `MySQL.Free`:** `OCI_HEATWAVE_ENABLE_RAPID=false` remains a zero-overhead default; enabling `OCI_HEATWAVE_ENABLE_RAPID=true` is now Pareto-safe for OLTP thanks to `rapid_query_hint_for_scan`, unlocking **`1.65x`** on `Q2_facets` and **`1.67x–11.36x`** on wide multi-ZIP scans past `>= 10K–15K` candidate rows.
   - **Multi-table `GROUP BY` verification required on Rapid `26.10.0-u1-cloud`:** Any custom analytical multi-table `GROUP BY` offloaded to Rapid must be parity-checked (`AG2 @ 150K` returned `360` unmerged rows vs `72` in InnoDB).
