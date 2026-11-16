# Multi-stage 100% Pure Rust container build (zero Node.js or Python server runtime)
# Supports native linux/arm64 and linux/amd64.

FROM rust:1-bookworm AS builder
WORKDIR /build

# `assets/` (template.html, vendored i18next, locale JSON), `locales/` (Fluent .ftl bundles),
# `extensions/` (YAML manifests & OpenAPI specs), and `proto/` (via `build.rs`) are compiled
# directly into the Rust binary via `include_str!`, `static_loader!`, and `prost-build`.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY extensions ./extensions
COPY assets ./assets
COPY locales ./locales
COPY src ./src

RUN cargo build --release --locked --bin open-estate-server

FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.title="Open Estate" \
      org.opencontainers.image.description="Open Estate — Self-Hosted US Residential, CRE, Land & Farmland Research Server (Personal Research & Non-Commercial Edition)" \
      org.opencontainers.image.licenses="LicenseRef-Open-Estate-Personal-Research-NC-1.0"

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      tini \
      curl \
      osmium-tool \
      poppler-utils \
      tesseract-ocr \
      tesseract-ocr-eng \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 1000 openestate \
    && useradd -u 1000 -g 1000 -m -s /usr/sbin/nologin openestate \
    && mkdir -p /app/reports /data/datasets /data/http-cache \
    && chown -R 1000:1000 /app /data

WORKDIR /app
COPY --from=builder /build/target/release/open-estate-server /usr/local/bin/open-estate-server

# `assets/` and `locales/` are compiled into the binary; keep runtime filesystem minimal.
# SQLite WAL and HTTP/dataset caches live under `/data` (mount a Docker volume at `/data`).
ENV HOST=0.0.0.0 \
    PORT=8080 \
    BRAND_NAME="Open Estate" \
    BRAND_DOMAIN=open-estate.local \
    PUBLIC_DATA_MODE=strict \
    REPORTS_DIR=/app/reports \
    DATA_DIR=/data/datasets \
    DATABASE_PROVIDER=sqlite \
    SQLITE_DB_PATH=/data/open-estate.db \
    SQLITE_PATH=/data/open-estate.db \
    HTTP_CACHE_DIR=/data/http-cache \
    AUTO_SYNC_ENABLED=false \
    SYNC_INTERVAL_MIN=0 \
    FREE_PROXY_POOL_ENABLED=false \
    PROXY_POOL_STRICT=false \
    OMP_THREAD_LIMIT=1 \
    RUST_LOG=info

USER 1000:1000
EXPOSE 8080

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/open-estate-server"]
