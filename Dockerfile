# syntax=docker/dockerfile:1

# Build the web UI and the Linux server in one reproducible image build.
FROM node:22-bookworm-slim AS web-builder
WORKDIR /workspace
RUN corepack enable && corepack prepare pnpm@11.18.0 --activate
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
# updater-config.json is imported by src/lib/updater-sources.ts at build time.
COPY index.html vite.config.ts tsconfig.json tsconfig.node.json src-tauri/updater-config.json ./
COPY public ./public
COPY src ./src
# The JSON import path crosses the src-tauri boundary, so place it at
# <root>/src-tauri/updater-config.json rather than alongside tsconfig.
RUN mkdir -p src-tauri && mv updater-config.json src-tauri/
RUN pnpm build

FROM rust:1.88-bookworm AS server-builder
WORKDIR /workspace
# Tauri remains a dependency of the shared library, so keep its Linux build
# dependencies until the server binary is split from the desktop entry point.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev libsqlite3-dev \
    libgtk-3-dev libwebkit2gtk-4.1-dev \
    libayatana-appindicator3-dev librsvg2-dev libxdo-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY src-tauri ./src-tauri
COPY --from=web-builder /workspace/dist ./dist
RUN cargo build --release --manifest-path src-tauri/Cargo.toml --bin waliapi-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl libssl3 libsqlite3-0 \
    libgtk-3-0 libwebkit2gtk-4.1-0 \
    libayatana-appindicator3-1 librsvg2-2 libxdo3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /nonexistent \
       --shell /usr/sbin/nologin waliapi
WORKDIR /app
COPY --from=server-builder /workspace/src-tauri/target/release/waliapi-server /app/waliapi-server
COPY --from=web-builder /workspace/dist /app/dist
RUN mkdir -p /data && chown -R waliapi:waliapi /app /data
ENV WALIAPI_HOST=0.0.0.0 \
    WALIAPI_PORT=8777 \
    WALIAPI_DATA_DIR=/data \
    WALIAPI_TARGET_HOME=/data/managed-home \
    WALIAPI_WEB_DIR=/app/dist
USER waliapi
EXPOSE 8777
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl --fail --silent http://127.0.0.1:${WALIAPI_PORT}/health || exit 1
ENTRYPOINT ["/app/waliapi-server"]
