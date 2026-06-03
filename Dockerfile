# syntax=docker/dockerfile:1.7
# Multi-stage build for the orbit-openeo server — the openEO 1.3.0 HTTP backend.
#
# Build:   docker build -t orbit-openeo:latest .
# Run:     docker run --rm -p 9080:9080 orbit-openeo:latest \
#            --bind 0.0.0.0:9080 --executor geo --auth-token "$TOKEN" \
#            --stac-url https://earth-search.aws.element84.com/v1
# Note:    a non-loopback --bind (0.0.0.0) REQUIRES --auth-token or the server
#          refuses to start (security default). For the fast P2 download path,
#          add `--features async-tiff-downloader` below + `libproj-dev` to the
#          builder apt list; the default build ships the stable P1 (libgdal) path.

# ─────────────────────────────────────────────────────────────────────────────
# Stage 1: builder — Rust toolchain + system GDAL (geo-kernel) + protobuf
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

# geo-kernel is a default feature; the GeoExecutor needs libgdal (installed above).
RUN cargo build --release -p orbit-openeo

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

COPY --from=builder /build/target/release/orbit-openeo /usr/local/bin/orbit-openeo

EXPOSE 9080
ENTRYPOINT ["orbit-openeo"]
CMD ["--help"]
