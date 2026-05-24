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
cargo test -p orbit-openeo --features geo-kernel --lib           # 468 tests (hermetic; no synthetic-S2 fixtures)
cargo test -p orbit-openeo --features geo-kernel --test ndvi_mean_time_e2e             # 3 integration tests
cargo test -p orbit-openeo --features geo-kernel --test complex_pipeline_verification  # 4 integration tests
cargo test -p orbit-openeo --features geo-kernel --test mask_from_values_e2e           # 3 integration tests
cargo test -p orbit-openeo --features geo-kernel --test apply_callback_e2e             # 3 integration tests
```

Aggregate: **~615 tests, 0 failed** across orbit-geo lib (134) + orbit-openeo
lib (468) + 4 integration test files (13). If `orbit-geo` shows 0 passed too,
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
| **SCL-20m** | `geo_executor/eval_mask.rs::resample_scl_to_data_grid` | S2 SCL is published at 20 m; B04/B08 are 10 m. `mask_scl_dilation` MUST auto-resample SCL to data resolution via `gdalwarp -r near` (nearest-neighbour — SCL is categorical) before applying. Without this the inner kernel rejects with `inconsistent metadata: data has N blocks, mask has M`. |

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
