# CLAUDE.md — orbit-etl

Project-specific guidance for AI assistants working in this tree. Read once
per session; surfaces the verified e2e flows and the audited foot-guns.

---

## 1. What this workspace is

`orbit-etl` is the MVP for the `orbit-rs` framework — a multi-domain Rust
platform (ETL · LLM agent · satellite/geo · parallel compute). The two
load-bearing apps for satellite/geo work are:

| Crate / app | Role |
|---|---|
| `crates/orbit-geo` | Raster kernel — GDAL bindings, block executor, NDVI, mask/zonal stats, sampling, ML classifier |
| `apps/orbit-openeo` | openEO 1.3.0 HTTP façade — collections, jobs, results, process_graphs |

The openEO façade is **reference-not-certified**: scope is bounded by
`apps/orbit-openeo/BACKEND-SCOPE.md`.

---

## 2. End-to-End NDVI mean-time test (verified working)

The canonical proof-of-life: submit a real openEO 1.3.0 NDVI mean-time
process graph and watch it download Sentinel-2 COGs, compute NDVI, reduce
over time, and write a georeferenced GeoTIFF.

### 2.1 Prereqs (one-time)

```bash
# GDAL is required for the geo-kernel feature gate.
brew install gdal                  # macOS — supplies gdal-config + libgdal
gdal-config --version              # expect 3.10+; 3.12.1 verified
```

The `gdal` Rust crate (v0.19) auto-discovers headers via `gdal-config`. No
`PKG_CONFIG_PATH` tweaking required on macOS Homebrew.

### 2.2 Build

```bash
# Workspace root: /Users/macbookpro/Rust_Sentinel/mvp/orbit-etl
cargo build -p orbit-openeo --features geo-kernel
# Cold build ≈ 4 minutes; warm rebuild ≈ 8 seconds.
```

### 2.3 Test

```bash
cargo test -p orbit-geo --lib                                    # 134 tests
cargo test -p orbit-openeo --features geo-kernel --lib           # 515 tests (hermetic; no synthetic-S2 fixtures)
cargo test -p orbit-openeo --features geo-kernel --test ndvi_mean_time_e2e             # 3 integration tests
cargo test -p orbit-openeo --features geo-kernel --test complex_pipeline_verification  # 4 integration tests
cargo test -p orbit-openeo --features geo-kernel --test mask_from_values_e2e           # 3 integration tests
cargo test -p orbit-openeo --features geo-kernel --test apply_callback_e2e             # 3 integration tests
```

Aggregate: **~662 tests, 0 failed** across orbit-geo lib (134) + orbit-openeo
lib (515) + 4 integration test files (13). If `orbit-geo` shows 0 passed too,
re-run with `--lib` explicitly — `cargo test -p orbit-geo` alone runs the
default harness which can be empty for the bin target.

**Note (2026-05-24)**: synthetic-S2 fixtures (`FixtureDownloader` /
`FakeSearcher` / `test_support.rs` / `band_flexible_cube_a9.rs`) were
deleted. Tests that depended on them were removed; the real-S2 path
(`--executor geo` + live STAC) is now the only end-to-end validation
path. See §2.4 for the live HTTP roundtrip.

### 2.4 Live HTTP roundtrip

```bash
./apps/orbit-openeo/examples/test_ndvi_e2e.sh                              # LocalExecutor — job ends in `error` (expected; LocalExecutor doesn't know `ndvi`)
ORBIT_OPENEO_EXECUTOR=geo ./apps/orbit-openeo/examples/test_ndvi_e2e.sh    # GeoExecutor — fetches real S2 COGs, writes GeoTIFF, status=finished
```

The geo path is **~30 s** end-to-end: STAC search against Element84,
`gdal_translate -projwin` cropped downloads from `sentinel-cogs.s3.us-west-2`,
per-block NDVI compute, time reduction, GeoTIFF write to
`--files-dir`. Expected manifest:

```json
{
  "id": "job-XXXXXXXX",
  "stac_version": "1.0.0",
  "type": "Feature",
  "assets": {
    "result.tif": { "href": "/jobs/.../results/result.tif", "type": "image/tiff", "file:size": <N> }
  }
}
```

### 2.4.1 Real-S2 PNG roundtrip (verified 2026-05-24)

The bash script above bakes in `ndvi_mean_time.json` + `GTiff`. To pull a
real Sentinel-2 NDVI as a PNG (or a masked NDVI), the exact procedure that
worked end-to-end:

```bash
# 1. Build with geo-kernel and stage the geo binary.
cargo build -p orbit-openeo --features geo-kernel
cp target/debug/orbit-openeo target/debug/orbit-openeo-geo

# 2. Start the server with persistent files dir + live Element84 STAC.
FILES_DIR=/tmp/orbit-real-s2-$(date +%s) && mkdir -p "$FILES_DIR"
./target/debug/orbit-openeo-geo \
  --bind 127.0.0.1:9083 \
  --executor geo \
  --files-dir "$FILES_DIR" \
  --stac-url https://earth-search.aws.element84.com/v1 \
  > /tmp/orbit-real-s2.log 2>&1 &
SRV=$!
# Wait for readiness (~2 ticks)
until curl -sf http://127.0.0.1:9083/.well-known/openeo >/dev/null; do sleep 0.5; done

# 3. POST the graph and capture the openeo-identifier header.
RESP=$(curl -s -i -X POST http://127.0.0.1:9083/jobs \
  -H 'content-type: application/json' \
  --data-binary @apps/orbit-openeo/examples/masked_ndvi_png_real_s2.json)
ID=$(echo "$RESP" | grep -i '^openeo-identifier:' | awk '{print $2}' | tr -d '\r\n')

# 4. Start the runner and poll for finished/error.
curl -s -X POST "http://127.0.0.1:9083/jobs/$ID/results" -o /dev/null
while :; do
  ST=$(curl -s "http://127.0.0.1:9083/jobs/$ID" \
       | python3 -c "import sys,json;print(json.load(sys.stdin).get('status','?'))")
  [[ "$ST" == "finished" || "$ST" == "error" ]] && break
  sleep 1
done

# 5. Copy the persisted PNG.
cp "$FILES_DIR/$ID/result.png" ~/Desktop/real_s2_masked_ndvi.png
kill $SRV
```

#### Two ready-made graphs in `apps/orbit-openeo/examples/`:

| File | Pipeline | Output |
|---|---|---|
| `ndvi_mean_png_real_s2.json` | load(B04+B08) → ndvi → reduce_dimension(mean) → save(PNG) | unmasked NDVI ~25 s |
| `masked_ndvi_png_real_s2.json` | load(B04+B08+SCL) → **mask_scl_dilation** → ndvi → reduce_dimension(mean) → save(PNG) | cloud-masked NDVI ~55 s |

Both pin: Wien bbox `[16.30, 48.18, 16.40, 48.24]`, June 2024, `sentinel-2-l2a`.
Output dimensions are **755 × 681** at S2's native 10 m resolution. PNG sizes
~300–400 KB (real entropy from natural variation; a 1×1 fallback would be
~70 B — see `finalise_save_result` PNG arm).

The **masked** variant depends on the **SCL-20m** invariant
(§4): SCL is published at 20 m vs B04/B08 at 10 m, so `eval_mask.rs::resample_scl_to_data_grid`
auto-resamples SCL with `gdalwarp -r near` before applying. Without that
auto-resample, the runner errors with `inconsistent metadata: data has N
blocks, mask has M`.

#### What gets persisted

```
{FILES_DIR}/{job_id}/result.png   ← raster PNG (image/png, 8-bit grayscale)
```

The `/jobs/{id}/results` manifest contains a STAC Feature with:
```json
{ "assets": { "result.png": { "href": "/jobs/{id}/results/result.png",
                              "type": "image/png", "file:size": <N> } } }
```
asset bytes also retrievable via `GET /jobs/{id}/results/result.png`.

### 2.4.2 P1 vs P2 download paths — detailed runbook (post-Task #34, 2026-05-24)

The geo executor has **three** download paths. P2 is now the **runtime default**
when the `async-tiff-downloader` feature is built; P1 is the diagnostic
fallback. Pick the right one based on the table at the bottom.

> Full matrix in `docs/perf/FEATURE_FLAG_MATRIX.md` (local-only).
> Performance progression: `docs/perf/P2_P3_OPTIMIZATION_PROGRESS.md` (local-only).

#### P1 — in-process libgdal eager download (stable fallback)

**What it does**: per (scene, band) calls `gdal::Dataset::open(/vsicurl/<href>)`
on the main runtime via `spawn_blocking`, applies `-projwin -projwin_srs`,
writes a cropped local `.tif` into `scratch_dir`, then `block_executor`
reads from the local files.

**Wall on 12 MP Wien S2 NDVI (B04+B08+SCL, June 2024, masked)**: 71 / 76 / 88 s
across 3 runs = **78 s avg**. Tight variance, no outliers.

**Build + run**:
```bash
cargo build -p orbit-openeo --features geo-kernel,async-tiff-downloader
cp target/debug/orbit-openeo target/debug/orbit-openeo-geo

FILES_DIR=/tmp/orbit-p1-$(date +%s) && mkdir -p "$FILES_DIR"
ORBIT_INPROCESS_DOWNLOADER=1 RUST_LOG=info \
./target/debug/orbit-openeo-geo \
  --bind 127.0.0.1:9083 \
  --executor geo \
  --files-dir "$FILES_DIR" \
  --stac-url https://earth-search.aws.element84.com/v1 \
  > /tmp/orbit-p1.log 2>&1 &
SRV=$!
until curl -sf http://127.0.0.1:9083/.well-known/openeo >/dev/null; do
  kill -0 $SRV || { tail -30 /tmp/orbit-p1.log; exit 1; }
  sleep 0.5
done
# POST + poll as in §2.4.1
```

**Confirm P1 is in use**: log line `downloader: in-process gdal::Dataset (P1, opt-out from P2 default)`.

#### P2 — async-tiff + object_store (Opt 1 + Opt 2 + STAC hint; runtime default)

**What it does**:
- `async-tiff` + `object_store::aws::AmazonS3` does HTTP/2 multiplexed range
  reads instead of libgdal `/vsicurl/`.
- **Opt 1 (proj 0.31)**: cross-CRS bbox reproject via libproj FFI — no fallback
  to libgdal for the common EPSG-to-EPSG case.
- **Opt 2 (shared S3 pool)**: `static S3_POOL_CACHE: Lazy<RwLock<HashMap<(bucket,region), Arc<dyn ObjectStore>>>>`
  so all concurrent downloads share one HTTP/2 connection pool.
- **STAC hint (Task #34)**: `download_via_async_tiff_with_crs_and_meta`
  pre-projects the bbox in parallel with the IFD fetch using STAC
  `proj:epsg` (the hint short-circuits the IFD's GeoKeyDirectory lookup).
  Telemetry: `P2: STAC band_metadata hint — dispatches with hint vs without
  hint_dispatched=N hint_missing=M`.

**Wall on 12 MP Wien S2 NDVI (same workload)**: 53 / 76 / 88 s across 3 successful
runs = **76 s median**, **53 s best**. **1/4 reps timed out >300 s** on an S3
body-error retry storm — known tail risk, see below.

**Build + run**:
```bash
cargo build -p orbit-openeo --features geo-kernel,async-tiff-downloader
cp target/debug/orbit-openeo target/debug/orbit-openeo-geo

FILES_DIR=/tmp/orbit-p2-$(date +%s) && mkdir -p "$FILES_DIR"
# No env var needed — P2-full is the default when the feature is built.
RUST_LOG=info \
./target/debug/orbit-openeo-geo \
  --bind 127.0.0.1:9083 \
  --executor geo \
  --files-dir "$FILES_DIR" \
  --stac-url https://earth-search.aws.element84.com/v1 \
  > /tmp/orbit-p2.log 2>&1 &
SRV=$!
until curl -sf http://127.0.0.1:9083/.well-known/openeo >/dev/null; do
  kill -0 $SRV || { tail -30 /tmp/orbit-p2.log; exit 1; }
  sleep 0.5
done
# POST + poll as in §2.4.1
# After the job finishes:
grep -E "STAC band_metadata hint|object_store::client" /tmp/orbit-p2.log
```

**Confirm P2 is in use**: log line `downloader: async-tiff + object_store + STAC hint (P2-full, default; set ORBIT_INPROCESS_DOWNLOADER=1 to opt-out to P1)`.

**Download concurrency tuning (Task #43 — shipped 2026-05-24)**: the
`download_sem` semaphore bounds simultaneous COG fetches. Default is
**6** (lowered from 8 after a 5-point sweep on 12 MP Wien S2). The 8/12
defaults over-saturated the shared S3 connection pool on 3-scene × 2-band
workloads (= 6 COGs in flight); N=4–6 hit the **EORS warm-cache parity
~14 s wall** on cold-network reps. Override with `ORBIT_DOWNLOAD_CONCURRENCY=<N>`
(N≥1; clamped). Log line at startup: `download concurrency override permits=N`.

| 10-node sweep | N=2 | N=4 | N=6 | N=8 | N=12 |
|---|---|---|---|---|---|
| Wall (s, healthy S3) | 91 | **14** | 16 | 43 | 40 |

| 15-node sweep | N=2 | N=4 | N=6 | N=8 | N=12 |
|---|---|---|---|---|---|
| Wall (s, healthy S3) | 23 | 26 | 15 | **14** | 17 |

Rule of thumb: pick `N ≈ min(N_cogs, 4–6)` where `N_cogs = scenes × bands`.

**Tail-latency risk (Task #39 — shipped 2026-05-24)**: when
`object_store::client::get: HTTP error: request or response body error.
Retrying in <N>s` appears in the log, the S3 connection pool has hit a
body-error storm. object_store's defaults (`max_retries=10`, `retry_timeout=180s`)
let this storm extend to 30+ minutes. `shared_s3_for` now applies a tuned
default + env-var overrides:

| Env var | Default | Effect |
|---|---|---|
| `ORBIT_S3_MAX_RETRIES` | `3` | Cap on retries per request (object_store default = 10). |
| `ORBIT_S3_RETRY_TIMEOUT_SECS` | `60` | Total time budget for the retry loop per request (default = 180). |
| `ORBIT_S3_REQUEST_TIMEOUT_SECS` | `120` | Per-request wall clock (reqwest `timeout`). |
| `ORBIT_S3_CONNECT_TIMEOUT_SECS` | `10` | TCP connect timeout (reqwest `connect_timeout`). |

With defaults, the **per-request** tail is bounded by `~max(retry_timeout, max_retries × request_timeout)` ≈ 360 s worst case, but in practice
the retry budget caps at 60 s. The 300+ s rep-3 P2 timeout reproduced
above should now bound to **≤ 120 s per file** instead of unbounded.

Verify the config is applied: bench log will include
```
async_tiff: built S3 client with tuned retry/timeout config
  bucket=sentinel-cogs region=us-west-2 max_retries=3 retry_timeout_secs=60
  request_timeout_secs=120 connect_timeout_secs=10
```
(visible at `RUST_LOG=debug` or above for `orbit_geo`).

#### When to pick which

| Scenario | Pick |
|---|---|
| Greenfield prod deploy on Element84 STAC | **P2 (default)** — competitive median, instrumented hint plumbing |
| S3 transport instability suspected (body errors in log) | **P1** via `ORBIT_INPROCESS_DOWNLOADER=1` |
| Linux deploy without `libproj-dev` | Build w/o `async-tiff-downloader` → P1 is the only path |
| Hard SLA on tail latency (p99 < 2× median) | **P1** (P2 tail is currently unbounded) |
| Debugging download throughput | A/B run both; P2 emits `hint_dispatched=N` and `ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN` counters |

#### Quick A/B benchmark script

`/tmp/bench_p1_p2.sh` (pattern; not in repo yet):
```bash
for VARIANT in P1 P2; do
  case $VARIANT in
    P1) EXTRA="ORBIT_INPROCESS_DOWNLOADER=1";;
    P2) EXTRA="";;
  esac
  for i in 1 2 3; do
    FILES_DIR=/tmp/orbit-bench-$VARIANT-$i && mkdir -p "$FILES_DIR"
    LOG=/tmp/orbit-bench-$VARIANT-$i.log
    eval "$EXTRA RUST_LOG=info ./target/debug/orbit-openeo-geo \
      --bind 127.0.0.1:9099 --executor geo --files-dir $FILES_DIR \
      --stac-url https://earth-search.aws.element84.com/v1 > $LOG 2>&1 &"
    SRV=$!
    until curl -sf http://127.0.0.1:9099/.well-known/openeo >/dev/null; do
      kill -0 $SRV || break; sleep 0.5
    done
    T0=$(date +%s)
    ID=$(curl -s -i -X POST http://127.0.0.1:9099/jobs -H 'content-type: application/json' \
         --data-binary @apps/orbit-openeo/examples/masked_ndvi_png_real_s2.json \
         | grep -i '^openeo-identifier:' | awk '{print $2}' | tr -d '\r\n')
    curl -s -X POST "http://127.0.0.1:9099/jobs/$ID/results" -o /dev/null
    while :; do
      ST=$(curl -s "http://127.0.0.1:9099/jobs/$ID" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("status","?"))' 2>/dev/null)
      [[ "$ST" == "finished" || "$ST" == "error" ]] && break
      kill -0 $SRV || { echo "$VARIANT $i: SERVER DIED"; break; }
      sleep 1
    done
    T1=$(date +%s)
    echo "$VARIANT rep$i wall=$((T1-T0))s status=$ST"
    kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
    rm -rf "$FILES_DIR"
  done
done
```

### 2.4.3 P2-full **BEST-PRACTICE E2E** — the 14 s recipe (Task #43, 2026-05-24)

**Result**: on a 10-node Wien NDVI graph (`complex_10node_nomask_real_s2.json`,
3 scenes × 2 bands = 6 COGs), the orbit P2-full backend hits **14 s wall
on cold-network** — within 2 s of the EORS public Rust benchmark warm-cache
target of ~12 s. Combines every optimisation shipped this session:

#### One-shot build + run

```bash
# 1. Build with the async-tiff feature (P2-full needs this).
cargo build -p orbit-openeo --features geo-kernel,async-tiff-downloader
cp target/debug/orbit-openeo target/debug/orbit-openeo-geo

# 2. Disk pre-flight: each fresh server writes ~30 MB scratch GTiffs per
#    job. Drop GC (Task #38) cleans them at process exit, but a hung crash
#    leaves them. Keep ≥1 GB free.
df -h /System/Volumes/Data | tail -1

# 3. Boot the server with EVERY perf knob set to the sweet spot.
FILES_DIR=/tmp/orbit-best-$(date +%s) && mkdir -p "$FILES_DIR"
ORBIT_DOWNLOAD_CONCURRENCY=4 \
ORBIT_S3_MAX_RETRIES=3 \
ORBIT_S3_RETRY_TIMEOUT_SECS=60 \
ORBIT_S3_REQUEST_TIMEOUT_SECS=120 \
ORBIT_S3_CONNECT_TIMEOUT_SECS=10 \
RUST_LOG=info \
./target/debug/orbit-openeo-geo \
  --bind 127.0.0.1:9106 \
  --executor geo \
  --files-dir "$FILES_DIR" \
  --stac-url https://earth-search.aws.element84.com/v1 \
  > /tmp/orbit-p2-best.log 2>&1 &
SRV=$!
until curl -sf http://127.0.0.1:9106/.well-known/openeo >/dev/null; do
  kill -0 $SRV || { tail -30 /tmp/orbit-p2-best.log; exit 1; }
  sleep 0.5
done

# 4. POST the graph, capture the openeo-identifier, trigger, poll.
T0=$(date +%s)
ID=$(curl -s -i -X POST http://127.0.0.1:9106/jobs \
  -H 'content-type: application/json' \
  --data-binary @apps/orbit-openeo/examples/complex_10node_nomask_real_s2.json \
  | grep -i '^openeo-identifier:' | awk '{print $2}' | tr -d '\r\n')
curl -s -X POST "http://127.0.0.1:9106/jobs/$ID/results" -o /dev/null
while :; do
  ST=$(curl -s "http://127.0.0.1:9106/jobs/$ID" \
       | python3 -c 'import sys,json;print(json.load(sys.stdin).get("status","?"))')
  [[ "$ST" == "finished" || "$ST" == "error" ]] && break
  sleep 1
done
T1=$(date +%s); echo "WALL=$((T1-T0))s STATUS=$ST"

# 5. Persisted PNG.
ls -lh "$FILES_DIR/$ID/result.png"
kill $SRV
```

Expected `WALL=14s STATUS=finished` on a healthy S3 day. Errors out to
`error` in ≤120 s on an S3-storm day (Task #39 retry bound).

#### Configuration matrix that achieved it

| Layer | Knob | Value used | Reason | Shipped in |
|---|---|---|---|---|
| Cargo feature | `async-tiff-downloader` | on | enables `AsyncTiffDownloader` + `proj` + `object_store` | pre-existing |
| Cargo feature | `geo-kernel` | on (default) | enables GDAL `GeoExecutor` | pre-existing |
| Runtime — downloader | (no env var) | P2-full default | flipped 2026-05-24 from P1 | Task #37 |
| Runtime — STAC hint | (automatic) | always on when P2 active | parallel bbox project + IFD fetch | Task #34 |
| Runtime — cross-CRS | (automatic) | pure-Rust `proj 0.31` | eliminates libgdal fallback | Task (Opt 1) |
| Runtime — S3 pool | (automatic) | shared `Arc<AmazonS3>` per (bucket, region) | one HTTP/2 pool across all 6 COGs | Task (Opt 2) |
| Env — concurrency | `ORBIT_DOWNLOAD_CONCURRENCY` | `4` (or default 6) | sweep showed 4-6 = sweet spot for 6-COG workloads | Task #43 |
| Env — S3 retries | `ORBIT_S3_MAX_RETRIES` | `3` | bounds tail vs upstream default 10 | Task #39 |
| Env — S3 retry budget | `ORBIT_S3_RETRY_TIMEOUT_SECS` | `60` | bounds tail vs upstream default 180 | Task #39 |
| Env — S3 per-req timeout | `ORBIT_S3_REQUEST_TIMEOUT_SECS` | `120` | reqwest `timeout` | Task #39 |
| Env — S3 connect timeout | `ORBIT_S3_CONNECT_TIMEOUT_SECS` | `10` | reqwest `connect_timeout` | Task #39 |
| Ops — scratch GC | `Drop` on `GeoExecutor` | automatic | cleans `orbit-geoexec-*` at process exit | Task #38 |
| Debug — pin scratch | `ORBIT_SCRATCH_DIR=<path>` | unset | pins scratch to a user-owned dir; **preserved** on exit (owns_scratch=false) so intermediate GeoTIFFs survive for `gdalinfo` value-verification | 2026-05-25 |

#### Why this combination wins

1. **STAC hint shortcuts the IFD round-trip on cross-CRS** — `proj:epsg` is read from STAC at search time, so `download_via_async_tiff_with_crs_and_meta` can `tokio::join!` the bbox projection with the IFD fetch (parallel instead of serial). Saves ~3-5 s × 6 COGs ≈ ~25 s wall when amortised across the concurrent fetch pool.
2. **Shared S3 pool reuses one HTTP/2 connection** for all 6 COGs to the same bucket. Without it, 6 separate `AmazonS3Builder` instances would each build a fresh `reqwest::Client` with its own connection — no multiplexing, more TLS handshakes, larger curl pools.
3. **Concurrency cap matches workload** — N=4 lets 4 of 6 COGs fetch in parallel batch 1, then 2 more in batch 2. Higher N (8, 12) over-saturated the pool and triggered S3 to RST connections under burst.
4. **Tuned retry budget fails fast** — when S3 misbehaves, we error in ≤120 s with a clean stack trace instead of stalling 30 min on upstream defaults.

#### Verifying you're on the fast path

The bench log should contain in this order, within the first ~10 s:

```
INFO orbit_openeo: downloader: async-tiff + object_store + STAC hint (P2-full, default; ...)
INFO orbit_openeo: download concurrency override permits=4   ← only if you set ORBIT_DOWNLOAD_CONCURRENCY
DEBUG orbit_geo::async_download: built S3 client with tuned retry/timeout config bucket=sentinel-cogs region=us-west-2 max_retries=3 retry_timeout_secs=60 request_timeout_secs=120 connect_timeout_secs=10
INFO orbit_openeo::geo_executor::eval_load: P2: STAC band_metadata hint — dispatches with hint vs without hint_dispatched=6 hint_missing=0
```

`hint_dispatched=N` proves the STAC hint plumbing is live. `hint_missing > 0` would mean some scenes had no `proj:epsg` from the STAC backend (rare for Element84; common for hand-rolled STAC servers).

#### Measuring the download / compute split

At the **end** of every job, `evaluate` logs a per-node-category wall breakdown (2026-05-25). Nodes run sequentially in topo order, so the categories sum to the graph wall:

```
INFO orbit_openeo::geo_executor: phase timing — per-node-category wall (download=load_collection, mask=mask_scl_dilation, compute=rest) download_s=51.2 mask_s=1.8 compute_s=0.9 total_s=53.9 nodes=15
```

- `download_s` = time inside `load_collection` (STAC search + COG range-reads — the I/O-bound phase, dominates on P2).
- `mask_s` = `mask_scl_dilation` (SCL fetch + per-band resample).
- `compute_s` = everything else (ndvi/reduce/apply/merge/save) — on small AOI crops this is typically **sub-second**, confirming wall is download-bound, not CPU-bound.
- Per-node detail is at `DEBUG` (`node=… process=… ms=…`).

This is the canonical way to answer "why did this graph take Ns" — read `download_s` vs `compute_s` instead of guessing. More bands × scenes ⇒ more COGs ⇒ larger `download_s`; the rest is S3 latency variance.

#### When NOT to use P2-full

| Symptom | Fall back to | Env var |
|---|---|---|
| Repeated `object_store::client::get: HTTP error: request or response body error` in logs | **P1** (in-process libgdal) | `ORBIT_INPROCESS_DOWNLOADER=1` |
| Building on Linux without `libproj-dev` | P1 (build without async-tiff-downloader) | feature flip |
| Hard SLA on p99 wall time | **P1** (bounded by libgdal `/vsicurl/` retry) | `ORBIT_INPROCESS_DOWNLOADER=1` |
| Debugging a STAC backend that omits `proj:*` extensions | P1 (no benefit from P2's hint path) | `ORBIT_INPROCESS_DOWNLOADER=1` |

#### Empirical performance summary (Task #43 sweep, 2026-05-24)

| Workload | Backend | Best wall | Median wall | Notes |
|---|---|---|---|---|
| 10-node Wien NDVI nomask | orbit P2-full N=4 | **14 s** | 16 s | 5/5 success on the sweep |
| 15-node Wien NDVI nomask | orbit P2-full N=8 | **14 s** | 15 s | 5/5 success on the sweep |
| 10-node Wien NDVI nomask | JonaAI Python+Dask | 63 s | 75-93 s | needs 2 patches to `ndvi()` |
| EORS public Rust (warm cache) | reference | ~12 s | — | unavailable cold-network number |
| 12 MP Wien S2 masked NDVI (4-node) | orbit P1 | 71 s | 78 s | stable baseline |
| 12 MP Wien S2 masked NDVI (4-node) | JonaAI | — | 141 s | post-patches |

### 2.5 Sample graph

`apps/orbit-openeo/examples/ndvi_mean_time.json` is the canonical openEO
1.3.0 graph the test suite + bash script share. Four nodes:

```
load1 (sentinel-2-l2a, B04+B08, JJAS 2024, Wien bbox)
  → ndvi1 (nir=B08, red=B04)
  → reduce1 (dimension=t, reducer.process_graph.mean1)
  → save1 (format=GTiff, result=true)
```

The `reducer.process_graph` sub-callback uses `from_parameter: "data"` (the
inner namespace). Outer topological walks **must not** descend into this
sub-graph — see §4 "Audit P0-3".

---

## 3. Server boot reference

```bash
target/debug/orbit-openeo \
    --bind 127.0.0.1:9080 \
    --executor geo \
    --files-dir /tmp/orbit-files \
    --db-url 'sqlite:///tmp/orbit-jobs.db?mode=rwc' \
    --stac-url https://earth-search.aws.element84.com/v1
```

Flags worth knowing:

| Flag | Default | Notes |
|---|---|---|
| `--bind` | `127.0.0.1:9080` | Non-loopback bind requires `--auth-token` or refuses to start |
| `--executor` | **`geo`** (was `local` pre-A9) | `local` mode forces JSON-only execution. `geo-kernel` is now a default cargo feature; use `--no-default-features` to strip GDAL. |
| `--files-dir` | in-memory | Persist `/files` and job assets to disk |
| `--db-url` | in-memory | SQLite for jobs — survives restart |
| `--stac-url` | Element84 | Empty string → empty in-mem catalog |
| `--auth-token` | none (loopback only) | `Bearer <token>` enforced when set |

Element84 STAC collection IDs are **lowercase-hyphenated** (`sentinel-2-l2a`),
**not** the openEO-conventional `SENTINEL2_L2A`. Use the right one or the
job fails with `backend: CollectionNotFound`.

---

**Docker**: the repo `Dockerfile` builds and ships `orbit-openeo` (the openEO server) on a Debian-slim runtime — see the README "Docker deployment" section. A non-loopback `--bind` (0.0.0.0) requires `--auth-token`.

---

## 4. Audited foot-guns (don't re-introduce)

The codebase has cleared an 8-agent security/correctness audit. The
following invariants are load-bearing:

| Audit | Location | Invariant |
|---|---|---|
| **P0-1** | `routes/credentials.rs` | Tokens via `OsRng::try_fill_bytes` → base64url-no-pad. Never xor-shift or deterministic. |
| **P0-2** | `lib.rs::build_router` | Body cap: `RequestBodyLimitLayer::new(128 MiB)` |
| **P0-3** | `process_graph.rs::collect_in_value` | Outer topological walks **short-circuit at `process_graph` keys** (sub-callbacks live in inner namespace) |
| **P0-4** | `geo_executor.rs` `"apply"` arm | Reject with `InvalidGraph` when `process` callback is supplied — no silent pass-through |
| **P0-5 / P1-9** | `geo_executor.rs`, `routes/jobs.rs` | All blocking GDAL work goes through `tokio::task::spawn_blocking`; downloads + job spawns are semaphore-bounded |
| **P1-6** | `file_store.rs::validate_path` | Reject `..`, `\\`, NUL, percent-encoded traversal, drive letters, leading `-`/`~`, control chars — after percent-decode-once |
| **P1-7** | `url_policy.rs` | Default-deny: only `https`; reject loopback / IMDS / RFC1918 / link-local / ULA / CGNAT / v4-mapped IPv6 |
| **P1-8** | `crates/orbit-geo/src/providers.rs` | **Do NOT add `--` sentinel to gdal_translate argv** — GDAL 3.x rejects it (`ERROR 1: Unknown argument: --`). Option-injection defense lives at `UrlPolicy::OptionInjection` (refuses any href starting with `-`) |
| **P1-12** | `process_graph.rs` | `{from_node, from_parameter}` siblings are still a valid link — don't gate on `obj.len() == 1` |
| **A3** | `geo_executor/eval_ndvi.rs` | `ndvi` is **single-purpose** — no fused temporal mean / SCL mask. Mask before, reduce after. Don't re-introduce the all-in-one worker. |
| **A4+A5** | `geo_executor/eval_reduce.rs` | `reduce_dimension` requires a real `reducer.process_graph` sub-callback (10 reducers supported: mean/min/max/sum/median/count/first/last/sd/variance). Not a metadata pass-through. |
| **A7+A8** | `geo_executor/eval_mask.rs` | Standard `mask(data, mask)` requires a **binary truthy** mask cube. Raw SCL has non-zero clear-sky classes → masks everything. Use `mask_from_values` to build the binary cube first, or `mask_scl_dilation` for the S2 shortcut. |
| **A9** | `geo_executor/stac.rs`, all `eval_*` | `StacScene.bands: BTreeMap<String, String>` and `__cube.bands: {<name>: [paths]}` — never reintroduce `red_href`/`nir_href`/`scl_href`/`red_paths`/`nir_paths`/`scl_paths`/`ndvi_paths` flat keys. `ndvi(nir, red, target_band)` args MUST be honored, not hardcoded. |
| **A12** | `crates/orbit-geo/src/providers.rs::build_gdal_translate_argv` | `-projwin_srs <crs>` is mandatory whenever the bbox is in a CRS different from the COG's native (default openEO EPSG:4326 vs S2 UTM). Without it, degree-scale bboxes collapse to 1×1 output. |
| **UTM-1px** | `geo_executor/eval_mask.rs` two workers | Data + mask cubes can differ by ±1 px after UTM reprojection. Iterate using `min(rdb.dims, mblock.dims)` — never `rdb.dims` blindly. |
| **SCL-20m** | `geo_executor/eval_mask.rs::resample_scl_to_data_grid` | S2 SCL is published at 20 m; B04/B08 are 10 m. `mask_scl_dilation` MUST auto-resample SCL to data resolution via `gdalwarp -r near` (nearest-neighbour — SCL is categorical) before applying. Without this the inner kernel rejects with `inconsistent metadata: data has N blocks, mask has M`. **GAP (BUG-001, 2026-05-24)**: this auto-resample only fires on the FIRST band's grid; if the load mixes 10 m (B04/B08) AND 20 m (B11/B12) bands, the 20 m data still misaligns with the now-10 m mask. See `docs/perf/KNOWN_BUGS.md#BUG-001` (local-only). |
| **multi-band cube ops** | `geo_executor/eval_apply.rs`, `eval_reduce.rs` (`bands` dim) | After `merge_cubes` joins two cubes with arbitrary `target_band` names, downstream `apply` and `reduce_dimension(dimension="bands")` reject the cube with `__cube.bands has no usable band` / `has no recognised index band (expected ndvi/ndmi/ndwi/etc.)`. Workaround: use only recognised index names (`ndvi`/`ndmi`/`ndwi`) AND apply BEFORE merge. See `docs/perf/KNOWN_BUGS.md#BUG-002` and `#BUG-003` (local-only). **(All FIXED 2026-05-25 — apply iterates all bands; reduce(bands) implemented; merge_cubes does band-axis join.)** |
| **reflectance scaling** | `data_cube.rs::band_scales`, `eval_load.rs`, `eval_apply.rs` | **Option B (2026-05-25)**: S2 COGs store Int16 DN; openEO load_collection should return reflectance. `load_collection` harvests `raster:bands.scale`/`offset` into `cube.band_scales` (per-band). `apply` converts `v*scale+offset` BEFORE the user's sub-graph so absolute math sees reflectance. **`ndvi` is scale-invariant (ratio cancels) and deliberately does NOT scale.** SCL/QA and derived-index bands have no scale entry (identity). Output bands are physical units → `band_scales` cleared on apply output. If a future workflow does absolute math on a raw band WITHOUT going through `apply`, it would see DN — only `apply` honors the scale today. |

---

## 5. API surface notes

### `Job.process` is the inner `Process` object

`POST /jobs` accepts `{"process": {"process_graph": {...}}, "title": "..."}`
but **stores only the inner `Process` object** as `JobRecord.process`. Don't
re-introduce the body-clone bug that double-wraps the response as
`process.process.process_graph`. See `routes/jobs.rs::create_job`.

### Runner emits typed errors via tracing

On executor failure the runner now logs
`ERROR orbit_openeo::runner: executor failed job_id=... error=...`. Grep
`/tmp/orbit-openeo.log` for `executor failed` when diagnosing a job that
went to `status=error`.

---

## 6. Workspace layout

```
mvp/orbit-etl/
├── apps/
│   └── orbit-openeo/
│       ├── BACKEND-SCOPE.md             — MAY / WILL-NOT contract
│       ├── spec/openapi.json            — openEO 1.3.0 spec we serve from
│       ├── src/
│       │   ├── lib.rs                   — build_router + body-limit layer
│       │   ├── main.rs                  — bin entrypoint, CLI args
│       │   ├── routes/                  — per-endpoint handlers
│       │   ├── geo_executor/            — GDAL-backed ProcessGraphExecutor (split into 10 files post-A7)
│       │   │   ├── mod.rs                — GeoExecutor struct + dispatcher
│       │   │   ├── stac.rs               — StacSearcher trait + HttpStacSearcher (eo:cloud_cover wired, A1+A2)
│       │   │   ├── download.rs           — Downloader trait + GdalTranslateDownloader + AssetSigners
│       │   │   ├── eval_load.rs          — load_collection (honors bands + spatial_extent.crs, A9+A12)
│       │   │   ├── eval_ndvi.rs          — ndvi pure per-scene (A3, honors nir/red/target_band per A9)
│       │   │   ├── eval_mask.rs          — mask + mask_scl_dilation (band-agnostic, A7+A8)
│       │   │   ├── eval_mask_from_values.rs — mask_from_values binary cube builder (A10)
│       │   │   ├── eval_reduce.rs        — reduce_dimension w/ 10 dispatched reducers (A4+A5)
│       │   │   ├── eval_misc.rs          — resample/zonal/aggregate/merge/classifier
│       │   │   ├── registry.rs            — ProcessRegistry (A2)
│       │   │   ├── sub_graph.rs           — SubGraphEvaluator + require_subgraph (A3)
│       │   │   └── identifier.rs          — validate_identifier path-traversal allowlist (B1)
│       │   ├── executor.rs              — JSON LocalExecutor
│       │   ├── process_graph.rs         — topo walker, P0-3 short-circuit
│       │   ├── url_policy.rs            — P1-7 SSRF guard
│       │   ├── file_store.rs            — P1-6 hardened paths
│       │   └── runner.rs                — lifecycle, events, error tracing
│       ├── examples/
│       │   ├── ndvi_mean_time.json      — canonical openEO 1.3.0 graph
│       │   └── test_ndvi_e2e.sh         — live HTTP roundtrip
│       └── tests/
│           └── ndvi_mean_time_e2e.rs    — 3 integration tests
└── crates/
    └── orbit-geo/
        ├── src/providers.rs             — gdal_translate argv builder, vsicurl rewrites
        └── src/...                      — block executor, NDVI, masks, zonal stats
```

---

## 7. Quick troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `ERROR 1: Unknown argument: --` from `gdal_translate` | `--` sentinel re-introduced in argv builder | Remove sentinel; rely on `UrlPolicy::OptionInjection` instead |
| `backend: CollectionNotFound: SENTINEL2_L2A` | Used openEO-naming against Element84 STAC | Use `sentinel-2-l2a` (lowercase, hyphenated) |
| Job stuck at `running progress=10` then `error` | Look in `/tmp/orbit-openeo.log` for `executor failed job_id=... error=...` | Real error message is now logged (post-2026-05-23 fix) |
| `POST /jobs` succeeds but `GET /jobs/{id}.process` is `{process: {process_graph: ...}}` (double wrap) | Route is storing whole submission body instead of inner `process` | See `routes/jobs.rs::create_job` — must persist `body["process"]` |
| `build_gdal_translate_argv_minimal` test fails with `left: 5, right: 4` | Spurious arg in argv (likely re-added `--`) | Argv shape: `["gdal_translate", "-q", <src>, <dst>]` |
| `gdal-sys` build error | Missing `gdal-config` on PATH | `brew install gdal` or set `GDAL_HOME` |
| Test count drops after running `cargo test -p orbit-geo` (shows 0 passed) | Default harness ran without `--lib` | Use `cargo test -p orbit-geo --lib` |
| `STATISTICS_VALID_PERCENT = 0%` / GeoTIFF all NA | Mask cube has `> 0` truthy values everywhere (e.g. raw SCL fed to generic `mask`) | Use `mask_from_values(data, band="SCL", values=[3,8,9,10,11])` to build a binary cube first |
| GeoTIFF is 1×1 pixel | `-projwin_srs` missing or wrong (degree-bbox vs UTM COG) | Verify `crates/orbit-geo/src/providers.rs::build_gdal_translate_argv` adds `-projwin_srs <crs>` (defaults to `EPSG:4326`) |
| `ndarray: index … out of bounds for shape [1,1,R,C]` in mask | Data/mask cubes differ by 1 px after UTM reprojection | Use `min(rdb.rows(), mblock.rows())` (same for cols) for iteration bounds in mask workers |
| `ndvi: band 'X' not loaded` | Client requested `nir=X` but `load_collection.bands` doesn't include X | Add X to the `bands` list (or override `nir`/`red` args to match what's loaded) |
| `mask_scl_dilation: __cube has no 'SCL' band` | `load_collection.bands` excludes `"SCL"` so the mask process can't reach the classification layer | Add `"SCL"` to `load_collection.bands` (or, if using `mask_from_values`, pass `band="<name>"` matching whatever you actually loaded) |
| `spatial_extent.crs invalid: …` | Client sent a non-number, non-string CRS value | Pass either an integer EPSG code (`4326`), an `"EPSG:<n>"` string, or omit the field (defaults to 4326) |

---

## 8. House conventions

- **Clean-room discipline** — do not copy source from the upstream raster
  engine workspace (see `NOTICE.md` for attribution). Identify capabilities,
  reimplement.
- **TDD** — RED → GREEN → REFACTOR. Watch the test fail before
  implementing.
- **Edition** — workspace uses Rust 2024.
- **Terse comments** — one-line, no multi-paragraph docstrings unless
  documenting a public API.
- **No emojis in source files** unless explicitly requested.
- **Confirm before destructive ops** — `git reset --hard`, `git push
  --force`, dropping tables, killing other people's processes.

## 9. Deferred opportunities (not shipped — captured for future work)

### 9.1 Consolidate the openEO STAC searcher onto `orbit-geo::StacClient` (rustac)

**Finding (verified 2026-05-25): the repo has TWO STAC implementations.**

1. **rustac-based, compiled in via `geo-kernel` but unused at runtime** — `crates/orbit-geo/Cargo.toml:26`
   declares a `stac` feature pulling `stac` 0.17 + `stac-api` 0.8 + `stac-io` +
   `stac-extensions` + `stac-client` + `stac-validate`. Implemented in
   `crates/orbit-geo/src/stac.rs` (`StacClient` wrapping `stac_client::Client` —
   `collections()` / `search()` + `stac_validate::Validator`) and
   `crates/orbit-geo/src/stac_helpers.rs` (7 helpers over `Vec<stac::Item>`).
   **Correction (audit 2026-06-03)**: the openEO app's `geo-kernel` feature DOES
   enable `orbit-geo/stac` (`apps/orbit-openeo/Cargo.toml:16` — base `orbit-geo`
   features `["cloud_mask","use_ml"]` PLUS `orbit-geo/stac`), so `StacClient` + the
   rustac crates **compile in by default**. But the runtime search path still uses
   the hand-rolled `HttpStacSearcher` (below), so `StacClient` AND the
   `apps/orbit-openeo/src/typed_stac.rs` wrapper (which uses `orbit_geo::stac::Item`)
   are **compiled but never called** — dead at runtime, not compiled out.
2. **Hand-rolled, what actually runs** — `apps/orbit-openeo/src/geo_executor/stac.rs`
   `HttpStacSearcher` (`reqwest` + `serde_json::Value`) extracts exactly
   `proj:epsg/transform/shape` + `raster:bands.scale/offset/nodata` into a lean
   `BandMetadata` / `StacScene` to feed the P2 download hint.

**Opportunity — consolidate the openEO searcher onto the existing `StacClient`:**
- **Gain**: kills the duplicate STAC impl; adds `stac-validate`; typed
  `stac-extensions` parsing (Projection + Raster ext structs instead of
  `serde_json::Value` digging); pagination / conformance handling.
- **Cost**: write an adapter `stac_client::ItemCollection → StacScene/BandMetadata`
  (the `stac` feature is already on via geo-kernel) + delete the dead
  `typed_stac.rs` wrapper; re-verify the P2 hint path still gets `proj:epsg` /
  `raster:bands.scale`.
- **Risk**: extra dep weight + the async-client model in the request path; the
  current hand-rolled extractor is leaner for the narrow hint use-case.

**Spike before committing**: feature-flag an alternate searcher and A/B it against
`HttpStacSearcher` (correctness of `BandMetadata` + wall time) before deleting the
hand-rolled path.

**No Rust openEO process engine exists** (Jan-2026 knowledge): reference impls are
Python (`openeo-processes-dask`, `openeo-python-driver`) + Java
(`openeo-geotrellis`). Hand-rolling orbit's process registry/evaluator + file-backed
`DataCube` is the only realistic path — there is no `xarray`-equivalent labeled N-d
array crate to lean on either.

> ⚠️ **Confidence**: in-repo facts (paths, versions, feature wiring) verified directly.
> The "no Rust openEO engine" + "rustac is the standard" claims are to Jan-2026
> knowledge; NOT web-verified (search hit a usage-credits error). Re-confirm current
> rustac versions + scan crates.io for any new openEO Rust effort before acting.
