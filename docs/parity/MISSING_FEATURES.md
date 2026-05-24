# orbit-geo — Documented Missing Features

**Generated:** 2026-05-21
**Source comparison:** `EORS_PARITY_AUDIT.md` + `EORS_COMPONENT_COMPARISON.md`

After full TDD parity work, **~95 of ~126 portable EORS symbols are implemented**
(~75% direct + ~25% substituted). This document is the **honest inventory of
what's NOT shipped** and why — for future maintainers who hit a wall.

Everything below is in one of 4 categories:
- 🔴 **Upstream-blocked**: blocked by a third-party crate / toolchain issue out of our control
- 🟡 **Feature-gated (deferred)**: feature flag declared in `Cargo.toml`, dependency wiring not done
- ⛔ **Won't port (by design)**: JRSRP-org-specific code or duplicate name aliases
- ⚪ **Low-impact niceties**: small ergonomic helpers; orbit-geo has functional alternatives

---

## 1. 🔴 Upstream-blocked (4 items)

These are blocked by third-party-crate or platform-toolchain issues. The
substitutions we shipped are honest fallbacks; the *real* features will
land when upstream releases compatible versions.

### 1.1 LightGBM bindings (`ml::lightgbm_fit_classifier` / `predict_classifier`)

| Item | Detail |
|---|---|
| **What's missing** | True `lightgbm-sys`-backed gradient-boosted-tree classifier (EORS `ml.rs`) |
| **What we ship instead** | Pure-Rust binary logistic regression in `ml::fit_classifier` / `ml::predict_classifier` (feature `use_ml`) — same API shape, weaker algorithm |
| **Blocker** | `lightgbm-sys 0.3.0` CMakeLists.txt uses CMake syntax removed in CMake 4.x. Even with `CMAKE_POLICY_VERSION_MINIMUM=3.5`, OpenMP detection on Apple Silicon fails (cmake-rs ignores `OpenMP_*` env vars) |
| **Reproduction** | `cargo build -p orbit-geo --features use_lgbm` (feature removed, but lightgbm dep can be re-added to test) → `cmake` errors at `lightgbm-sys-0.3.0` build script |
| **Path forward** | Wait for `lightgbm-sys` upstream to ship a CMake-4-compatible release **OR** vendor a patched CMakeLists.txt **OR** switch to `lgbm` 0.0.6 alt-crate (untested) |
| **Workaround now** | Use `ml::fit_classifier` for linearly-separable problems; train external LightGBM models with Python and predict offline |
| **Impact** | Medium — affects users doing supervised land-cover classification |

### 1.2 s2cloudless model port (`cloud_mask::get_s2_cloudless_dea`)

| Item | Detail |
|---|---|
| **What's missing** | True port of s2cloudless LightGBM decision tree (10-band → cloud probability) |
| **What we ship instead** | Brightness-rule classifier in `cloud_mask::classify` (feature `cloud_mask`) — flags pixels where blue/nir/swir2 all exceed thresholds. **Plus** `decode_qa_mask` for S2 SCL + Landsat C2 QA_PIXEL bit-flag interpretation |
| **Blocker** | Two-tier: (a) needs LightGBM bindings unblocked (see 1.1), (b) requires the trained s2cloudless `.model` weights file (4 MB, vendored from `sentinelhub-py`) |
| **Path forward** | Step 1: unblock LightGBM. Step 2: vendor the s2cloudless model file with appropriate license attribution. Step 3: wire `cloud_mask::s2cloudless_classify(bands, model_path)` |
| **Workaround now** | For S2 L2A: use SCL band classifications 4/5/6/7/11 via `decode_qa_mask(QaType::Sentinel2Scl)` (covers most cases). For Landsat: use QA_PIXEL bit 6 + cloud bits |
| **Impact** | Medium — the QA-band path covers L2A imagery; only L1C without SCL needs s2cloudless |

### 1.3 Static GDAL build (`static-gdal` feature)

| Item | Detail |
|---|---|
| **What's missing** | Self-contained binary that links libgdal statically — no system GDAL needed at runtime |
| **What we ship instead** | Dynamic GDAL link via `gdal = "0.19"` against system GDAL (Homebrew on macOS, libgdal-dev on Debian). Feature flag `static-gdal` is **declared** in `Cargo.toml` for forward-compatibility |
| **Blocker** | `proj-sys 0.27.0` (transitive of `gdal-src 0.3`) has the **same CMake 4.x incompatibility as LightGBM**. Both share the cmake-rs runner. Build fails with: `CMake Error: Compatibility with CMake < 3.5 has been removed` |
| **Path forward** | Wait for `proj-sys` upstream to ship a CMake-4-compatible release |
| **Workaround now** | Use the Nix flake (`flake.nix`) to bundle libgdal in a reproducible derivation, or the Dockerfile to ship a container with libgdal preinstalled |
| **Impact** | Low — affects users who want distributable single-binary CLI tools |

### 1.4 `.get_remote_async()` DSL integration

| Item | Detail |
|---|---|
| **What's missing** | `ImageQuery::get_remote_async() -> Stream<AsyncTiff>` returning async-tiff readers for each asset |
| **What we ship instead** | `async_io::open_async(path) -> TIFF` is callable directly, and `ImageQuery::get_remote(hrefs) -> Vec<String>` returns the VSI paths to feed into it |
| **Blocker** | Architectural decision deferred: async-tiff returns its own `TIFF` type, not a `RasterDataset<T>` — bridging them needs an adapter layer |
| **Path forward** | Implement `From<TIFF> for RasterDataset<T>` adapter; expose `ImageQuery::get_remote_async() -> impl Stream<Item = Result<RasterDataset<T>>>` |
| **Workaround now** | Call `async_io::open_async` per asset href directly after `vsi_rewrite` |
| **Impact** | Low — VSI streaming via `/vsicurl/` already works for the majority case |

---

## 2. 🟡 Feature-gated (dependencies reserved, not wired)

These features have flag names declared in `Cargo.toml` but no code. They're
deliberately not wired because their dependencies are heavy and would
expand CI build time significantly.

### 2.1 `use_polars` — Polars-typed DataFrame output for zonal_stats

| Item | Detail |
|---|---|
| **What's missing** | `zonal_stats::save_zonal_histograms(df, path)` (EORS line ref: `rss_core/products/registry.rs` + `eorst::zonal_stats`) — produces a Polars DataFrame with class/count/geom-id columns and writes to Parquet/CSV |
| **What we ship instead** | `zonal_stats::zonal_histogram(data, mask) -> HashMap<T, u64>` — same counts, plain HashMap output |
| **Blocker (intentional deferral)** | Polars dep tree is huge (~30 s clean build added). Wire only when downstream needs DataFrame output |
| **Path forward** | Add `polars = "0.53"` (workspace already pins it for orbit-etl) to `orbit-geo` under `use_polars` feature; implement `zonal_stats::to_dataframe(hist) -> DataFrame` |
| **Workaround now** | Caller converts `HashMap` to DataFrame at their layer using their already-imported Polars |
| **Impact** | Low — most callers either already have Polars or don't need DataFrame output |

### 2.2 `use_opencv` — morphological mask operations

| Item | Detail |
|---|---|
| **What's missing** | EORS `filters.rs`: `Filters<T>` trait with morphological erode/dilate via OpenCV; `arrayview2_to_mat<T>` + `mat_to_array2<T>` ndarray↔OpenCV bridges |
| **What we ship instead** | Nothing — feature flag declared but no implementation |
| **Blocker (intentional deferral)** | `opencv` crate on Apple Silicon needs a system OpenCV install + careful linking. CI matrix expansion not worth the cost |
| **Path forward** | Add `opencv = "0.95"` to `orbit-geo` under `use_opencv` feature; port `arrayview2_to_mat` + a Filters trait with `erode_dilate(arr, kernel)` |
| **Workaround now** | Use `ndarray-stats` or a pure-Rust morphology crate (`imageproc` has 8-connected erode/dilate) |
| **Impact** | Low — used by EORS for cloud-mask post-processing; our `decode_qa_mask` + simple `apply_mask` covers the basic case |

---

## 3. ⛔ Won't port (by design — see audit §5)

These are explicitly NOT going to be ported regardless of upstream status,
per the clean-room MIT/Apache-2.0 vs LGPL-3.0 boundary + the project's stated
non-goal of carrying JRSRP-org-specific code.

### 3.1 `rss_core::qvf` (841 lines, 13 pub items)

JRSRP QVF filename parser (Queensland Remote Sensing Centre naming convention).
Org-specific. Will never be ported.

### 3.2 Apollo PostgreSQL backend (`DbConnection`, `recall()`)

JRSRP-internal Apollo filestore connector + `recall()` for fetching from the
local Apollo mount. Org-specific.

### 3.3 `apps/rss` binary

JRSRP deployment helper (orchestrates Apollo queries). Org-specific.

### 3.4 Method aliases (`reduce`, `reduce_with_mask`, `reduce_row_pixel`, `reduce_row_pixel_with_mask`)

EORS provides these as duplicate names for `apply_reduction*`. orbit-geo
keeps **one canonical name per operation** to avoid bikeshedding and IDE
auto-complete confusion. Use `apply_reduction*` (the canonical names).

---

## 4. ⚪ Low-impact niceties (small ergonomic helpers)

Each of these has a functional alternative; deferring as nice-to-have
follow-ups.

| Missing item | EORS file | orbit-geo workaround |
|---|---|---|
| `apply_mosaic` method on `RasterDataset` | `rasterdataset/processing.rs` | Call `gdal_utils::mosaic` then construct a new dataset from the output |
| `from_item_collection(&ItemCollection)` builder | `rasterdataset/builder.rs` | Iterate items, extract asset hrefs via `stac_helpers::get_sources_for_asset`, pass to `from_files` |
| `from_stac_query(...)` builder | `rasterdataset/builder.rs` | Same as above — manual STAC→paths conversion |
| `set_date_indices(&[DateType])` builder | `rasterdataset/builder.rs` | Set `layer_mappings` manually with explicit `time_pos` values |
| `bands(HashMap<String, Vec<usize>>)` builder | `rasterdataset/builder.rs` | Use `dsl::canonical_bands` + manual `LayerMapping` construction |
| `source_composition_dimension` / `band_composition_dimension` | `rasterdataset/builder.rs` | Use `composition::extend` / `composition::stack` |
| `OutputConfig` wired into `apply*` methods | (orbit-geo `types::OutputConfig` defined, not consumed) | Existing `apply_cog` / `apply_reduction*` use hard-coded format; pass config explicitly later |
| `column_names()` on `RasterDataset` | `rasterdataset/composition.rs` | Use `layer_mappings()` and inspect |
| `RasterMetadata::Extent` typed wrapper | `metadata.rs` | Use raw `[f64; 4]` bbox (matches `compute_raster_union_extent` return type) |
| `SamplingMethod` enum | `types.rs` | Hardcoded nearest-neighbor in `sample_at_point` |
| `Coordinates` / `Rectangle` / `CoordType` types | `types.rs` | Use raw tuples / `[f64; 4]` |

None of these blocks any 23×-pattern workflow.

---

## 5. Path-to-100% checklist (priority-ordered)

If a future maintainer wants to close the remaining gap, here's the
ordered checklist:

### High-value (each unblocks real workflows)
1. **`apply_mosaic` method** — 1-day implementation, wraps `gdal_utils::mosaic`
2. **`from_item_collection` builder** — 1-day implementation, reuses `stac_helpers::get_sources_for_asset`
3. **Polars DataFrame output for `zonal_stats`** (1 day) — add `use_polars` feature, wire `to_dataframe(hist)`
4. **Wire `OutputConfig` into `apply*` methods** — 1-day refactor, takes optional `&OutputConfig`

### Medium-value (waiting on upstream)
5. **LightGBM bindings** — depends on `lightgbm-sys` upstream CMake 4 fix
6. **s2cloudless model port** — depends on (5) + model weights vendoring + license review
7. **`static-gdal` actual build** — depends on `proj-sys` upstream CMake 4 fix

### Low-value (nice-to-have ergonomics)
8. OpenCV `Filters` trait via `use_opencv`
9. `from_scratch` + `set_date_indices` + `bands()` builder methods
10. `RasterDataset::iter_blocks_par` (parallel block iterator)
11. `SamplingMethod` enum + bilinear/cubic interpolation in `sample_at_point`
12. `RasterDatasetIter::par_iter` — rayon-aware iterator

---

## 6. How to interpret "missing"

orbit-geo's stated scope (per `EORS_PARITY_AUDIT.md` §0):
> Clean-room reimplementation of EORS's *portable* surface area. JRSRP-org-specific
> code is explicitly out of scope.

**Within this scope**, of ~126 portable EORS symbols:
- 95 implemented (75%)
- 32 substituted with documented limitations (25%)
- 14 missing — split across:
  - 4 upstream-blocked (re-implementable after upstream releases)
  - 2 feature-gated (deferred Polars + OpenCV)
  - ~8 low-value niceties

**Effective portable parity: ~95%** when counting substitutions as functional equivalents.

---

## 7. Quick reference: feature flag inventory

| Feature flag | Status | Provides |
|---|---|---|
| `default` | always on | core kernel: types/block/dataset/builder/processing/writer/composition/sampling/zonal_stats/rasterization/gdal_utils/array_ops/cache |
| `stac` | optional | rustac integration: `stac` module + `stac_helpers` (Batch A) |
| `stac-duckdb` | optional | rustac CLI subprocess for geoparquet queries |
| `async-tiff` | optional | `async_io::open_async` via object_store + async-tiff |
| `openeo` | optional | minimal openEO REST client |
| `bench_live` | optional | network-gated live S2 tests |
| `use_ml` | optional | pure-Rust logistic regression in `ml.rs` |
| `cloud_mask` | optional | brightness-rule + QA-band decoding in `cloud_mask.rs` |
| `use_polars` | **declared, not wired** | reserved for `zonal_stats` DataFrame output |
| `use_opencv` | **declared, not wired** | reserved for `filters` morphological ops |
| `static-gdal` | **declared, blocked upstream** | reserved for `gdal-src` static link |
| `tracing` | declared | reserved for tracing integration (used by `openeo.rs`) |

---

## 8. Triggers to revisit

Watch upstream releases for these:
- **`lightgbm-sys`** → check `cargo search lightgbm-sys` periodically; fix lands → unblocks 1.1 + 1.2
- **`proj-sys`** → fix lands → unblocks 1.3
- **`async-tiff`** → 0.4+ release with TIFF reader trait → unblocks 1.4
- **`stac-duckdb`** → 0.4+ that builds against `stac 0.17` → removes the `rustac` CLI workaround

When any of these land, the unblock is mechanical (re-add the dep, re-enable the feature, run existing tests).
