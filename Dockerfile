# syntax=docker/dockerfile:1.7
# Multi-stage build for the orbit CLI binary.
#
# Build:    docker build -t orbit-cli:latest .
# Run:      docker run --rm orbit-cli:latest --help
# Geo:      docker run --rm -v /tmp:/tmp orbit-cli:latest geo rasterize ...

# ─────────────────────────────────────────────────────────────────────────────
# Stage 1: builder — Rust toolchain + system GDAL + protobuf
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:1.95-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    pkg-config \
    libssl-dev \
    libgdal-dev \
    libsqlite3-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release -p orbit-cli --bin orbit

# ─────────────────────────────────────────────────────────────────────────────
# Stage 2: runtime — slim base + just the runtime GDAL libs
# ─────────────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    gdal-bin \
    libgdal36 \
    libsqlite3-0 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/orbit /usr/local/bin/orbit

ENTRYPOINT ["orbit"]
CMD ["--help"]
