# syntax=docker/dockerfile:1.7
#
# Multi-stage, multi-target build for vmcp.
#
#   docker build --target export  --build-arg TARGET=x86_64-unknown-linux-gnu --output type=local,dest=./dist .
#   docker build --target export  --build-arg TARGET=x86_64-pc-windows-gnu     --output type=local,dest=./dist .
#   docker build --target runtime -t vmcp:latest .   # Linux runtime image
#
# Windows binaries are cross-compiled from Linux via mingw-w64 (GNU ABI).

ARG RUST_VERSION=1.95
ARG TARGET=x86_64-unknown-linux-gnu
# Extra cargo flags to select a build variant, e.g.
#   --build-arg FEATURES="--no-default-features"   # HTTP gateway without admin UI
# Empty (default) builds the full HTTP + admin gateway.
ARG FEATURES=""

FROM rust:${RUST_VERSION}-bookworm AS builder
ARG TARGET
ARG FEATURES

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        gcc-mingw-w64-x86-64 \
 && rm -rf /var/lib/apt/lists/*

RUN rustup target add ${TARGET}

ENV CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_AR=x86_64-w64-mingw32-ar

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,id=vmcp-target-${TARGET},sharing=locked \
    set -eux; \
    cargo build --release --locked --bin vmcp --target ${TARGET} ${FEATURES}; \
    mkdir -p /out; \
    for f in target/${TARGET}/release/vmcp target/${TARGET}/release/vmcp.exe; do \
        if [ -f "$f" ]; then cp "$f" /out/; fi; \
    done; \
    ls -la /out

# ------------------------------------------------------------------
# Export stage: scratch image containing only the built binary.
# Use with `docker build --target export --output type=local,dest=./dist`.
# ------------------------------------------------------------------
FROM scratch AS export
COPY --from=builder /out/ /

# ------------------------------------------------------------------
# Runtime image (Linux only). Skip this stage when TARGET is Windows.
# ------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/vmcp /usr/local/bin/vmcp

# The admin UI (`/admin/static/*`) serves CSS/JS from disk via `ServeDir` at
# runtime — those files are NOT compiled into the binary. Ship them in the
# image and point the router at them explicitly, so the assets resolve
# regardless of the container's working directory.
COPY --from=builder /build/crates/vmcp-admin/static /usr/local/share/vmcp/admin/static
ENV VMCP_ADMIN_STATIC_DIR=/usr/local/share/vmcp/admin/static

ENTRYPOINT ["/usr/local/bin/vmcp"]
