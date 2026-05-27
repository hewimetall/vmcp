# syntax=docker/dockerfile:1.7
#
# Production deploy image for the Rust vmcp gateway.
#
#   Stage 1: build the vmcp binary (Rust, multi-crate workspace).
#   Stage 2: runtime — Debian slim + Node 22 + Python/uv + container-use,
#            so vmcp can spawn the three demo upstream MCP servers as
#            child processes (transport-child-process):
#              - container-use  →  `cu stdio`              (Dagger sandboxes)
#              - time           →  `uvx mcp-server-time`   (Python)
#              - postgres       →  `npx -y @modelcontextprotocol/server-postgres` (Node)
#
# Build context is the repo root (see infra/compose/docker-compose.yml).

# Workspace MSRV is 1.80 but recent transitive deps (cpufeatures 0.3+)
# require edition2024 → Rust >= 1.85. Pin to a comfortably newer stable.
ARG RUST_VERSION=1.90
ARG DEBIAN_VERSION=bookworm

# ------------------------------------------------------------------
# Stage 1: build vmcp (workspace binary `vmcp`).
#
# Layered with cargo-chef so that the (expensive) dep compilation step
# is its OWN docker layer keyed on Cargo.toml/Cargo.lock — workspace
# source edits invalidate only the final `cargo build` step. Without
# this every `git pull && deploy.sh` re-compiled ~600 deps from scratch.
#
# Three sub-stages:
#   chef     — base image with the cargo-chef binary
#   planner  — generates a recipe.json describing the full dep graph
#   rust-builder — cooks deps (cached), then builds the workspace
# ------------------------------------------------------------------

FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS chef
# Install cargo-chef once, reuse it in both downstream stages.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo install cargo-chef --locked --version ^0.1
WORKDIR /build

FROM chef AS planner
# The planner reads ONLY manifest files — its recipe.json captures the
# dep tree without caring about your actual source. Changes to crates/
# still invalidate this stage's COPY layer, but `chef prepare` itself
# is cheap (~1s) and the resulting recipe is deterministic per
# Cargo.lock so the downstream cook step's cache key stays stable.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY mcp ./mcp
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS rust-builder
# Release profile pins `lto = "thin"` + `codegen-units = 1` for runtime
# perf. That combo has a high peak-memory cost when rustc reaches the big
# leaves (sqlparser, async-graphql, rmcp) — on the 2 GB prod box the
# OOM-killer SIGKILLs rustc mid-build (observed on first v0.4.0 deploy).
# Pin cargo to a single job so only one heavy rustc instance lives at a
# time; total build is slower but fits in memory without touching the
# release profile (and without provisioning swap on prod). Override at
# build-time via `--build-arg CARGO_JOBS=N` when building on a fat box.
ARG CARGO_JOBS=1

# --- COOK: compile all deps based on the recipe. This is the heavy
#     step (~80% of build time). Its cache key is the recipe.json,
#     which only changes when Cargo.toml or Cargo.lock change — so
#     source-only edits keep this layer warm.
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,id=vmcp-infra-target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json -j "${CARGO_JOBS}"

# --- BUILD: bring in real source. Deps are already compiled in
#     target/release/deps/, so this step only recompiles workspace
#     crates (~20% of build time).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY mcp ./mcp
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,id=vmcp-infra-target,sharing=locked \
    cargo build --release --locked --bin vmcp -j "${CARGO_JOBS}" \
 && mkdir -p /out \
 && cp target/release/vmcp /out/vmcp

# ------------------------------------------------------------------
# Stage 2: runtime with vmcp + upstream-MCP tooling.
# ------------------------------------------------------------------
FROM debian:${DEBIAN_VERSION}-slim AS runtime

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    UV_TOOL_DIR=/opt/uv/tools \
    UV_TOOL_BIN_DIR=/usr/local/bin \
    PATH=/root/.local/bin:/usr/local/bin:/usr/bin:/bin

# Base toolchain: curl/ca-certificates for downloads, python3 for the
# time MCP server (uvx pulls a venv but needs a system interpreter to
# bootstrap), node for the postgres MCP, dumb-init as PID 1 (clean
# signal handling for child processes vmcp spawns).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg dumb-init \
        python3 python3-venv \
 && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && apt-get purge -y gnupg \
 && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/*

# uv (astral.sh) — provides `uvx` for one-shot Python MCP launches.
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
 && mv /root/.local/bin/uv /root/.local/bin/uvx /usr/local/bin/ \
 && uv --version && uvx --version

# Pre-warm uv tool cache for mcp-server-time so first vmcp tools/list
# call doesn't pay the cold-start network hit on a 1 GB-RAM server.
RUN uv tool install mcp-server-time

# Pre-warm mcp-atlassian — large Python package (~100 MB of deps from
# atlassian-python-api + httpx + pydantic stack). Without this
# `uvx mcp-atlassian` cold-start would blow past spawn_timeout_ms = 30000
# on the prod box. We ship it without credentials in the demo image, so
# tools/list works but real calls return "auth required" until the
# operator sets JIRA_URL/JIRA_API_TOKEN in the registry env block.
RUN uv tool install mcp-atlassian

# container-use (`cu`) — Dagger's MCP server for containerized agent
# environments. Pinned via build arg for reproducibility; bump and
# rebuild to upgrade. Goreleaser asset naming keeps the `v` prefix in
# the version segment and uses debian arch names (amd64/arm64) verbatim.
ARG CONTAINER_USE_VERSION=v0.4.2
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64|arm64) : ;; \
        *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/cu.tgz \
        "https://github.com/dagger/container-use/releases/download/${CONTAINER_USE_VERSION}/container-use_${CONTAINER_USE_VERSION}_linux_${arch}.tar.gz"; \
    tar -C /usr/local/bin -xzf /tmp/cu.tgz container-use; \
    ln -s container-use /usr/local/bin/cu; \
    rm /tmp/cu.tgz; \
    cu --version

# Pre-warm npx cache for the postgres MCP so first call is instant.
RUN npm install -g @modelcontextprotocol/server-postgres \
 && npm cache clean --force

# vmcp binary itself.
COPY --from=rust-builder /out/vmcp /usr/local/bin/vmcp

# Default config bundle. Compose overlays mount real registry/vmcp.toml
# on top of these for runtime config; the bake-in keeps the image
# self-sufficient for `docker run vmcp:source` smoke tests.
WORKDIR /app
COPY vmcp.toml /app/vmcp.toml
COPY registry.json /app/registry.json
COPY specs /app/specs
COPY skills /app/skills
# Admin UI static assets (served via /admin/static/*). The runtime
# discovery in vmcp-admin tries "crates/vmcp-admin/static" first, then
# "./static" — that second lookup matches when cwd is /app, which is
# what the entrypoint runs under.
COPY crates/vmcp-admin/static /app/static

ENV VMCP_HOST=0.0.0.0 \
    VMCP_PORT=8765 \
    VMCP_CONFIG=/app/vmcp.toml \
    VMCP_ADMIN_STATIC_DIR=/app/static

EXPOSE 8765

ENTRYPOINT ["/usr/bin/dumb-init", "--", "/usr/local/bin/vmcp"]
