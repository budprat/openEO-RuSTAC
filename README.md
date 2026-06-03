# openEO-RuSTAC

![openEO](https://img.shields.io/badge/openEO-1.3.0-blue)
![Rust](https://img.shields.io/badge/Rust-2024_edition-orange)
![License](https://img.shields.io/badge/license-MIT-green)
![status](https://img.shields.io/badge/status-reference_backend_(not_certified)-yellow)

> A **Rust-native [openEO](https://openeo.org) 1.3.0 reference backend** that streams Sentinel-2
> Cloud-Optimized GeoTIFFs (COGs) straight from a STAC catalog and executes openEO process graphs
> with block-parallel raster compute — no Python, no JVM, no Dask.
>
> ⚠️ **Reference backend, _not_ certified.** Single-tenant, opinionated, pinned to openEO 1.3.0.
> It exists so the existing openEO Python/R/JS ecosystem can drive Rust geo-compute remotely.
> See [`apps/orbit-openeo/BACKEND-SCOPE.md`](apps/orbit-openeo/BACKEND-SCOPE.md) for the MAY / WILL-NOT contract.

The server lives in [`apps/orbit-openeo`](apps/orbit-openeo); the raster engine in [`crates/orbit-geo`](crates/orbit-geo);
the openEO process-graph AST in [`crates/eo-process`](crates/eo-process). The repo is a Cargo workspace that also
carries an ETL foundation (`orbit-etl` + CLI/gRPC server) — see [Monorepo layout](#-monorepo-layout).

> 🙏 **Credits.** The `orbit-geo` API surface is derived from the **JRSRP EORS Workspace**
> (`eorst` + `rss_core`, LGPL-3.0). EORS is credited as the upstream this work derives from.
> See [`NOTICE.md`](NOTICE.md) for full attribution.

---

## Table of contents

- [Highlights](#-highlights)
- [Architecture](#-architecture)
- [Quick start (end-to-end)](#-quick-start-end-to-end)
- [Configuration](#-configuration)
- [Download paths (P1 / P2 / P3)](#-download-paths-p1--p2--p3)
- [Supported processes](#-supported-processes)
- [HTTP API surface](#-http-api-surface)
- [Example process graphs](#-example-process-graphs)
- [Testing](#-testing)
- [Observability](#-observability)
- [Monorepo layout](#-monorepo-layout)
- [Scope, roadmap & license](#-scope-roadmap--license)

---

## ✨ Highlights

| | |
|---|---|
| **openEO 1.3.0 REST API** | Axum server; every request JSON-Schema-validated against the shipped `spec/openapi.json`. |
| **67 openEO processes** | Reducers + arbitrary callbacks, `merge_cubes` (band-join + overlap-resolve), per-pixel `apply`, 31 scalar math/logic + 9 array processes, cube-metadata ops. Authoritative list: `geo_executor/registry.rs::register_defaults`. |
| **P2-full streaming download (default)** | Pure-Rust [`async-tiff`](https://crates.io/crates/async-tiff) + [`object_store`](https://crates.io/crates/object_store) COG reads with a STAC `band_metadata` hint and a shared S3 connection pool — **no libGDAL on the hot read path**. |
| **Cross-CRS, no GDAL fallback** | bbox reprojection via pure-Rust [`proj`](https://crates.io/crates/proj). |
| **Block-parallel compute** | `RasterDataset<f32>` tiled, multi-threaded reduction kernels. |
| **DN → reflectance** | honours STAC `raster:bands.scale`/`offset` so absolute math sees real reflectance. |
| **Job lifecycle** | `SqliteJobStore`, orphan recovery on startup, per-job timeout. |
| **Observability** | per-job `download_s / mask_s / compute_s` phase timing + STAC-hint telemetry. |

---

## 🏗 Architecture

```
                       openEO client  (Python / R / JS / curl)
                                   │  HTTP — openEO 1.3.0 REST
                                   ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ apps/orbit-openeo — Axum HTTP server                                       │
│   • JSON-Schema validation vs spec/openapi.json (at request time)          │
│   • Bearer / Basic / OIDC auth · 128 MiB body cap · SqliteJobStore         │
│   • ProcessRegistry → ProcessHandler dispatch (+ "did you mean" hints)     │
└───────────────────────────────┬────────────────────────────────────────────┘
                                 │  process graph (eo-process AST)
                                 ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ GeoExecutor (geo_executor/) — evaluates each node in topological order     │
│   load_collection · mask_scl_dilation · ndvi · reduce_dimension ·          │
│   apply · merge_cubes · filter_bands · rename_labels · save_result · …      │
└───────────────┬───────────────────────────────────────┬────────────────────┘
                │ STAC search (Element84 Earth Search)    │ block-parallel compute
                ▼                                          ▼  (ndarray, N threads)
   ┌──────────────────────────────┐          ┌───────────────────────────────┐
   │ COG download                  │          │ crates/orbit-geo               │
   │   P2  async-tiff + S3 (default)│          │   RasterDataset<f32>, GDAL,    │
   │   P1  in-process libGDAL       │          │   proj, cloud_mask, STAC       │
   │   P3  /vsicurl/ range stream   │          └───────────────────────────────┘
   └──────────────┬───────────────┘
                  ▼   s3://sentinel-cogs (us-west-2) — Sentinel-2 L2A COGs
```

**Layering** (clean-room re-implementations from the openEO spec):

- `crates/eo-process` — openEO process-graph **AST** + executor trait surface.
- `crates/eo-kernel`, `eo-mask`, `eo-catalog`, `eo-vector`, `eo-io`, `eo-core` — compute/IO building blocks.
- `crates/orbit-geo` — GDAL/async-tiff raster engine, STAC client, providers, cloud-mask, ML.
- `apps/orbit-openeo` — the HTTP façade + `GeoExecutor` that wires it all together.

---

## 🚀 Quick start (end-to-end)

### Prerequisites

- **Rust** (2024 edition, stable toolchain) — install via [rustup](https://rustup.rs).
- **GDAL** (for the `geo-kernel` feature) — `brew install gdal` (macOS) / `apt-get install libgdal-dev` (Debian/Ubuntu).
- **PROJ** (for P2-full cross-CRS) — bundled with GDAL on most platforms; else `brew install proj` / `apt-get install libproj-dev`.

### 1. Build (with the P2-full streaming downloader)

```bash
cd path/to/orbit-etl
cargo build -p orbit-openeo --features async-tiff-downloader
# binary: ./target/debug/orbit-openeo
```

### 2. Run the server

```bash
mkdir -p /tmp/orbit-files
ORBIT_DOWNLOAD_CONCURRENCY=4 RUST_LOG=info \
./target/debug/orbit-openeo \
  --bind 127.0.0.1:9080 \
  --executor geo \
  --files-dir /tmp/orbit-files \
  --stac-url https://earth-search.aws.element84.com/v1
```

> Binding to a **loopback** address needs no auth. A non-loopback bind **refuses to start** without
> `--auth-token` / `ORBIT_OPENEO_AUTH_TOKEN` (security default).

### 3. Submit a process graph, poll, fetch the result

openEO batch jobs are **3 calls**: create → start → poll. The job id comes back in the
`OpenEO-Identifier` response header.

```bash
PORT=9080
GRAPH=apps/orbit-openeo/examples/complex_15node_environmental_real_s2.json

# create the job → capture the job id from the OpenEO-Identifier header
JOB=$(curl -s -i -X POST "http://127.0.0.1:$PORT/jobs" \
        -H 'content-type: application/json' --data-binary @"$GRAPH" \
      | grep -i '^openeo-identifier:' | awk '{print $2}' | tr -d '\r\n')
echo "job: $JOB"

# start it (async execution)
curl -s -X POST "http://127.0.0.1:$PORT/jobs/$JOB/results" -o /dev/null

# poll until finished / error
until [ "$(curl -s "http://127.0.0.1:$PORT/jobs/$JOB" | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')" \
        != "running" ]; do sleep 2; done

# the rendered PNG lands under <files-dir>/<job-id>/
open /tmp/orbit-files/$JOB/result.png   # macOS; use xdg-open on Linux
```

A full Sentinel-2 graph over a small AOI typically finishes in **~30–60 s** (download-bound — see
[Observability](#-observability)).

---

## ⚙ Configuration

### CLI flags (each also reads an env var)

| Flag | Env var | Default | Purpose |
|---|---|---|---|
| `--bind` | `ORBIT_OPENEO_BIND` | `127.0.0.1:9080` | HTTP bind address. |
| `--executor` | `ORBIT_OPENEO_EXECUTOR` | `geo` | `geo` (real raster compute) or `local` (JSON-only, CI). |
| `--files-dir` | `ORBIT_OPENEO_FILES_DIR` | _(in-memory)_ | Where job results (`<id>/result.png`) are written. |
| `--stac-url` | `ORBIT_OPENEO_STAC_URL` | Element84 Earth Search v1 | STAC API backing `/collections`. Empty = disabled. |
| `--auth-token` | `ORBIT_OPENEO_AUTH_TOKEN` | _(unset)_ | Bearer token; **required** for non-loopback binds. |
| `--backend-id` | `ORBIT_OPENEO_BACKEND_ID` | `orbit-rs` | Reported in the capabilities document. |
| `--db-url` | `ORBIT_OPENEO_DB_URL` | _(in-memory)_ | e.g. `sqlite://./jobs.db?mode=rwc` for persistent jobs. |

### Runtime tuning (environment only)

| Env var | Default | Effect |
|---|---|---|
| `ORBIT_INPROCESS_DOWNLOADER=1` | _(off)_ | Opt **out** of P2-full to the in-process libGDAL path (P1). |
| `ORBIT_DOWNLOAD_CONCURRENCY` | `8` | Max simultaneous COG fetches. **4–6** is the S3 sweet spot. |
| `ORBIT_S3_MAX_RETRIES` | `3` | Per-request retry cap (vs object_store default 10). |
| `ORBIT_S3_RETRY_TIMEOUT_SECS` | `60` | Retry-loop budget per request (vs default 180). |
| `ORBIT_S3_REQUEST_TIMEOUT_SECS` | `120` | Per-request wall-clock. |
| `ORBIT_S3_CONNECT_TIMEOUT_SECS` | `10` | TCP connect timeout. |
| `ORBIT_JOB_TIMEOUT_SECS` | `600` | Per-job execution timeout → marks the job `error`. |
| `ORBIT_SCRATCH_DIR` | _(auto temp)_ | Pin scratch to a dir; **preserved** on exit (debug / value-verification). |
| `ORBIT_VSICURL_STREAM=1` | _(off)_ | P3: skip downloads, emit `/vsicurl/` paths (block-level range reads). |

> 📖 The deep runbook (P1/P2/P3 recipes, the "14 s" best-practice config, foot-guns) lives in
> [`CLAUDE.md`](CLAUDE.md).

---

## ⬇ Download paths (P1 / P2 / P3)

| Path | How | Use when |
|---|---|---|
| **P2-full** _(default)_ | `async-tiff` + `object_store` streaming COG reads, STAC `band_metadata` hint (skips IFD round-trips), shared S3 pool, cross-CRS via `proj`. | The default — fastest, no libGDAL on the read path. |
| **P1** | In-process `gdal::Dataset` cropped reads. | S3 transport instability, hard p99 SLA, or a STAC backend without `proj:*` extensions. Set `ORBIT_INPROCESS_DOWNLOADER=1`. |
| **P3** | libGDAL `/vsicurl/` range reads from worker threads (no pre-download). | Experimental block-level streaming. Set `ORBIT_VSICURL_STREAM=1`. |

Confirm the active path in the logs: `downloader: async-tiff + object_store + STAC hint (P2-full, default; …)`.

---

## 🧩 Supported processes

**67 processes** as of 2026-05-25 (authoritative: `apps/orbit-openeo/src/geo_executor/registry.rs::register_defaults`):

| Category | Processes |
|---|---|
| **Data access** | `load_collection` (DN→reflectance), `save_result` (GeoTIFF, PNG) |
| **Cube structure** | `filter_bands`, `filter_temporal`, `filter_spatial`, `filter_bbox`, `rename_labels`, `add_dimension`, `drop_dimension`, `resample_spatial` |
| **Reduce / combine** | `reduce_dimension` (mean/min/max/sum/median/count/first/last/sd/variance **+ arbitrary callbacks**, over `t` **and** `bands`), `merge_cubes` (band-join · overlap-resolver · spatial mosaic), `aggregate_spatial_*` |
| **Per-pixel** | `apply` (sub-graph over all bands), `ndvi`, `normalized_difference` |
| **Masking** | `mask`, `mask_scl_dilation` (per-band SCL resample), `mask_from_values` |
| **Math (31)** | `absolute` `sqrt` `exp` `ln` `log` `power` `sgn` `floor` `ceil` `int` `round` `mod` `clip` `cos` `sin` `tan` `arccos` `arcsin` `arctan` `arctan2` … |
| **Logic / comparison** | `eq` `neq` `gt` `gte` `lt` `lte` `between` `and` `or` `xor` `not` |
| **Arrays (9)** | `array_element` `array_create` `array_concat` `array_append` `array_contains` `array_find` `count` `order` `sort` |
| **Analysis** | `zonal_histogram`, `fit_classifier`, `predict_classifier` |

Unknown processes return an `UnknownProcess` error **with a Levenshtein "did you mean" suggestion**.

---

## 🌐 HTTP API surface

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/.well-known/openeo` · `/` | Capabilities / version discovery |
| `GET` | `/collections` · `/collections/{id}` | STAC collections (proxied from `--stac-url`) |
| `GET` | `/processes` | Advertised process list |
| `GET` | `/output_formats` · `/service_types` · `/udf_runtimes` | Capability docs |
| `POST` | `/validation` | Validate a process graph without running it |
| `POST` | `/jobs` | Create a batch job (returns `OpenEO-Identifier` header) |
| `GET` | `/jobs/{id}` | Job status (`queued`/`running`/`finished`/`error`) |
| `POST` | `/jobs/{id}/results` | Start execution |
| `GET` | `/jobs/{id}/results` · `/jobs/{id}/results/{asset}` | Result assets |
| `GET` | `/jobs/{id}/estimate` | Cost/size estimate |
| `POST` | `/credentials/basic` · `/credentials/oidc/token` | Auth |
| `GET` | `/me` | Authenticated user info |

---

## 📂 Example process graphs

In [`apps/orbit-openeo/examples/`](apps/orbit-openeo/examples) (all hit live Sentinel-2 over a Vienna AOI):

| Graph | Demonstrates |
|---|---|
| `ndvi_mean_png_real_s2.json` | Minimal: load → NDVI → temporal mean → PNG |
| `masked_ndvi_png_real_s2.json` | + `mask_scl_dilation` cloud masking |
| `complex_15node_environmental_real_s2.json` | **Full tour**: `merge_cubes` (band-join + overlap-resolver), compound reducer, `reduce_dimension(bands)`, `rename_labels`, `apply`/`clip` |
| `complex_branching_diamond_real_s2.json` | Branching DAG (vegetation × moisture diamond) |

---

## 🧪 Testing

```bash
# full lib suite for the backend (515 tests)
cargo test -p orbit-openeo --features geo-kernel,async-tiff-downloader --lib

# whole workspace
cargo test --workspace
```

Discipline is **TDD** (RED → GREEN → REFACTOR) — see [`CLAUDE.md`](CLAUDE.md) §8.

---

## 📈 Observability

Every job logs a phase breakdown at `INFO` on completion:

```
phase timing — per-node-category wall (download=load_collection, mask=mask_scl_dilation, compute=rest)
  download_s=25.55 mask_s=1.77 compute_s=1.32 total_s=28.63 nodes=15
```

On a typical Sentinel-2 graph this shows **~89 % download / ~5 % compute** — wall time is S3-I/O-bound,
not CPU-bound. STAC-hint telemetry (`hint_dispatched=N hint_missing=0`) confirms the P2 fast path is live.

---

## 🐳 Docker deployment

The multi-stage [`Dockerfile`](Dockerfile) builds and ships the **`orbit-openeo`** server
(GDAL-backed, default `geo-kernel` feature) on a slim Debian runtime:

```bash
docker build -t orbit-openeo:latest .
docker run --rm -p 9080:9080 orbit-openeo:latest \
  --bind 0.0.0.0:9080 --executor geo --auth-token "$TOKEN" \
  --stac-url https://earth-search.aws.element84.com/v1
```

A non-loopback `--bind` (`0.0.0.0`) **requires** `--auth-token`, or the server refuses to start
(security default). The image ships the stable P1 (libgdal) download path; for the faster
P2-full streaming path, add `--features async-tiff-downloader` to the builder stage and
`libproj-dev` to its apt list.

---

## 🗂 Monorepo layout

This is a Cargo workspace (`resolver = "3"`, edition 2024). The openEO backend is the headline app;
the rest is the `orbit-rs` foundation it grew out of.

```
apps/
  orbit-openeo/   ← openEO 1.3.0 HTTP backend  (this project's star)
  orbit-server/   ← ETL gRPC server (Tonic)
  orbit-cli/      ← ETL + `orbit geo …` CLI (clap)
crates/
  orbit-geo/      ← raster engine: GDAL + async-tiff, STAC, proj, cloud-mask, ML
  eo-process/     ← openEO process-graph AST + executor trait
  eo-kernel/ eo-mask/ eo-catalog/ eo-vector/ eo-io/ eo-core/   ← EO building blocks
  orbit-etl/      ← Polars → SQLite ETL engine (Phase-1 MVP foundation)
  orbit-core/ orbit-proto/ orbit-cache/ orbit-resilience/ orbit-observability/ orbit-config/
```

The ETL foundation (`orbit-etl` + `orbit-server` + `orbit-cli`) runs a File → Polars → SQLite pipeline
with a gRPC service and a progress-bar CLi; see `crates/orbit-etl` and `apps/orbit-cli` for its usage.

---

## 📜 Scope, roadmap & license

- **Scope contract**: [`apps/orbit-openeo/BACKEND-SCOPE.md`](apps/orbit-openeo/BACKEND-SCOPE.md) — what the backend MAY and WILL NOT do. **Reference backend, not certified.**
- **Changelog**: [`CHANGELOG.md`](CHANGELOG.md) (Keep-a-Changelog).
- **Deferred work / decisions**: [`CLAUDE.md`](CLAUDE.md) §9 — e.g. consolidating the STAC searcher onto `orbit-geo::StacClient` (rustac), `apply_kernel`, `aggregate_temporal_period`.
- **Attribution**: clean-room re-implementations from the openEO spec — see [`NOTICE.md`](NOTICE.md) and [`THIRD_PARTY.md`](THIRD_PARTY.md).

**License**: [MIT](LICENSE).
