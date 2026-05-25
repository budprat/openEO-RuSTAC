# Changelog

All notable changes to `orbit-etl` will be documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **openEO process set 8 → 69** (strictly per the 1.3.0 spec): 31 scalar math/logic ops, 9 array processes, cube-metadata ops (`filter_bands`/`rename_labels`/`add_dimension`/`drop_dimension`), `merge_cubes` (band-axis join + `overlap_resolver`), arbitrary `reduce_dimension` callbacks + a `bands` axis. Authoritative list: `apps/orbit-openeo/src/geo_executor/registry.rs::register_defaults`.
- **P2-full download path is now the default**: async-tiff + `object_store` streaming + STAC `band_metadata` hint, shared S3 connection pool, cross-CRS reprojection via `proj`. Opt out to in-process GDAL with `ORBIT_INPROCESS_DOWNLOADER=1`.
- DN→reflectance scaling from STAC `raster:bands.scale`/`offset`.
- Job lifecycle: orphan recovery on startup + `ORBIT_JOB_TIMEOUT_SECS`.
- Observability: `ORBIT_SCRATCH_DIR` (pin/preserve scratch for value-verification) + per-job phase-timing logs (download/mask/compute split); `ORBIT_DOWNLOAD_CONCURRENCY`, `ORBIT_S3_*` tuning.
- `apps/orbit-openeo/BACKEND-SCOPE.md` operational scope contract (MAY / WILL NOT).
- Example process graphs under `apps/orbit-openeo/examples/` (10/15-node + branching-diamond + environmental).

### Changed
- `apps/orbit-openeo` dispatch moved to a `ProcessRegistry` + `ProcessHandler` trait (superseding the planned typed-`ProcessNode`-enum-only dispatcher, G1.6).
- `merge_cubes` corrected from spatial-mosaic-at-averaged-resolution to a true band-axis join (openEO Cases 1/2/3).
- Default downloader flipped from in-process GDAL (P1) to P2-full.
- `13-geo-satellite/04-openeo-strategic-analysis.md` declares **Approach D** (client adapter + reference backend), superseding the 2026-05-21 Approach C commitment.

### Fixed
- BUG-001..005 (diamond-DAG): `mask_scl_dilation` mixed-resolution, `reduce_dimension` index allow-list, `apply` post-`merge_cubes`, `reduce_dimension(bands)`, `apply` multi-band.

### Removed
- The `docs/` tree (plans, parity, perf, archive, README) removed from version control; build/run/perf guidance consolidated into `CLAUDE.md` (incl. §9 deferred work). `docs/` and `.claude/` are now git-ignored.
- `RELEASE_NOTES.md` retired in favour of this Keep-a-Changelog `CHANGELOG.md`.

---

## [0.1.0] - 2026-05-21

First implementation milestone — `orbit-geo` moved from ~30–35% upstream raster engine feature parity to ~85% in one continuous TDD-driven session. 80 unit tests passing, 5 CLI integration tests, 1 live Sentinel-2 smoke test, 0 regressions.

### Added

#### Engine (`crates/orbit-geo`)

- **Tier 0 — Validation foundations**
  - `build_overviews` invoked automatically after every `apply_reduction*`.
  - Writer-generic variants: `apply_reduction_to_writer<W: BlockWriter<V>>`, `apply_reduction_with_mask_to_writer`.
  - First Criterion bench (`apply_reduction`): 1.93 ms small / 10.28 ms medium synthetic baselines.
  - Live S2 smoke test against `S2B_55HBV_20241225_0_L2A` from Element 84 Earth Search (NDVI mean 256×256 in 7.6 s end-to-end).

- **Tier 1 — Core kernel parity**
  - `apply_with_mask` + `apply_with_mask_to_writer`.
  - COG output trio: `apply_cog`, `apply_with_mask_cog`, `apply_reduction_with_mask_cog`.
  - `apply_reduction_row_pixel_to_writer` for memory-constrained per-row dispatch.
  - `read_block_layer_idx` (single-layer block read), `write_window3` (direct-write helper).
  - `gdal_utils::mosaic`, `gdal_utils::convert_to_cog` (subprocess wrappers).

- **Tier 2 — Declarative DSL**
  - `Collection` enum (Sentinel2 / Landsat 5/7/8) with `id_for(provider)` resolution.
  - `Intersects` enum (Bbox / Scene / Geometry) with feature-gated `apply_to(&mut SearchParams)`.
  - `Cmp` enum + `cloudcover_filter` → STAC `{"lt": v}` JSON.
  - `ImageQueryBuilder` + `ImageQuery` with typestate-light validation.
  - `canonical_bands(name, collection)` covering 17 provider-portable band mappings.
  - `ImageQuery::get_remote(hrefs)` with VSI rewrite for direct GDAL open.

- **Tier 3 — Auxiliary modules**
  - `composition::{extend, stack}`, `sampling::{sample, sample_at_point, geo_to_pixel}`.
  - `zonal_stats::zonal_histogram`, `rasterization::rasterize`.
  - `async_io::open_async` (async-tiff + object_store, no libgdal at read time).
  - `cloud_mask` module (rule-based brightness classifier, `cloud_mask` feature).
  - `ml` module (pure-Rust logistic regression, `use_ml` feature).

- **Tier 4 — CLI, examples, benches**
  - 5 `orbit geo *` CLI subcommands: `rasterize`, `mosaic`, `sample`, `warp`, `get-imagery` (via `apps/orbit-cli`).
  - 30 upstream-API-mirroring examples in `crates/orbit-geo/examples/01..30_*.rs`.
  - 5 Criterion benches: `apply_reduction`, `bench_apply`, `bench_apply_with_mask`, `bench_ndvi_annual_full_tile`, `bench_get_vs_get_remote` (`bench_live` feature).

- **Tier 5 — Build & deploy**
  - `flake.nix` Nix dev shell + `buildRustPackage` for the `orbit` CLI with GDAL/openssl/sqlite system deps.
  - `Dockerfile` multi-stage Rust 1.95 + libgdal-dev → debian:bookworm-slim runtime.
  - `.github/workflows/ci.yml` matrix (Ubuntu × macOS × stable × 1.95.0) plus feature-specific test jobs, doc build, and fmt check.

#### openEO surface

- `crates/orbit-geo/src/openeo.rs` — minimal openEO client (5 functional endpoints + OIDC Bearer / Basic / None auth) behind the `openeo` feature.
- `DataSource::OpenEO` variant in `crates/orbit-geo/src/source.rs`.
- `apps/orbit-openeo` — Axum-based openEO 1.3.0 reference backend with `SqliteJobStore`, JSON-Schema validation against shipped `spec/openapi.json`, and OIDC device-flow + Basic + Bearer auth.

### Fixed

- `DataSource::Files::validate()` no longer rejects `/vsicurl/...` and `/vsis3/...` paths via `Path::exists()`. Three regression tests added (caught while running the live S2 smoke test).
- `orbit-proto` build script migrated to `tonic 0.14`: `tonic_build::configure()` removed in 0.14, prost integration moved to `tonic-prost-build`. Added `tonic-prost-build` build-dep + `tonic-prost` runtime dep. Without this the whole workspace was uncompilable.

### Deferred (declared, not yet shipped)

- **True LightGBM bindings** — `lightgbm-sys 0.3` CMakeLists uses CMake syntax removed in CMake 4.x; OpenMP detection fails on Apple Silicon. Pure-Rust logistic regression ships as a substitute under `use_ml`. API shape preserved for later swap-in.
- **True s2cloudless port** — needs LightGBM bindings + the trained `.model` weights file. Rule-based brightness classifier ships under `cloud_mask` as a substitute.
- **`static-gdal` feature** — declared but blocked by `proj-sys 0.27` CMake 4.x incompatibility (same root cause as LightGBM). Workaround: use Nix flake or Dockerfile.
- **`get_remote_async()` DSL integration** — async-tiff `TIFF` type not bridged to `RasterDataset<T>`; architectural decision deferred.
- **`use_polars`** — feature declared, `zonal_stats::save_zonal_histograms` Polars DataFrame output not wired.
- **`use_opencv`** — feature declared, `Filters<T>` morphological trait and ndarray↔OpenCV bridges not implemented.
- **Full `stac_helpers` module** — all 7 functions absent (`get_asset_href`, `get_items_for_date`, `unique_datetimes_in_range`, `swap_coordinates`, etc.).

### Known issues

- 7 `missing_docs` warnings on public DSL fields (`ImageQuery.{provider, collection, ...}`). Deferred to a clippy/rustfmt cleanup pass.
- A few unused-import warnings in example files.

### Performance baselines (locked)

| Workload | Time |
|---|---|
| Synthetic NDVI 256×256 × 9 timesteps × 128px blocks × 1 thread (`DiscardWriter`) | 1.93 ms |
| Synthetic NDVI 1024×1024 × 9 timesteps × 256px blocks × 4 threads | 10.28 ms |
| `apply` 512×512 → real GeoTIFF on disk | 4.74 ms |
| `apply_with_mask` 512×512 → `DiscardWriter` | 397 µs |
| Live S2 NDVI mean 256×256 (Element 84 anonymous, full STAC + VSI download + compute) | 7.6 s |

CI does not currently fail on bench regressions. The 10% regression budget lands in 02-net-new-gaps.md task G7.1.

---

[Unreleased]: https://github.com/budprat/openEO-RuSTAC/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/budprat/openEO-RuSTAC/releases/tag/v0.1.0
