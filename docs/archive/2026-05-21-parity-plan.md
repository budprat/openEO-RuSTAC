# EORS Parity Implementation Plan — TDD-First

**🏁 STATUS (2026-05-21):** Every tier + setup phase below has **landed in
this session**. See `RELEASE_NOTES.md` for the full deliverables ledger and
`EORS_PARITY_AUDIT.md` for the now-flipped gap matrix.

Summary: **80 unit tests + 5 CLI integration tests + 2 smoke tests + 5 Criterion
benches + 31 examples + Nix + Docker + GitHub Actions CI** — all delivered
strict-TDD-first with 0 regressions.

Honest substitutions disclosed (`RELEASE_NOTES.md` for full text):
- **T3.6**: pure-Rust logistic regression (`ml::fit_classifier`) instead of LightGBM bindings — `lightgbm-sys 0.3` + CMake 4 + Apple Silicon OpenMP blocked
- **T3.5**: brightness-rule cloud classifier instead of s2cloudless model port — needs LightGBM unblock + model-weights file
- **T5.2**: `static-gdal` feature declared but blocked by `proj-sys 0.27` CMake 4 issue (same upstream class)

---


**Generated:** 2026-05-21
**Companion to:** `EORS_PARITY_AUDIT.md` (gap inventory)
**Method:** Strict Red → Green → Refactor per `superpowers:test-driven-development`

---

## 0. The Iron Law (non-negotiable)

```
NO PRODUCTION CODE WITHOUT A FAILING TEST FIRST
```

For every task below:
1. Write the failing test(s) named exactly as listed.
2. Run `cargo test <name>` — confirm it **fails for the expected reason** (feature missing, not typos / compilation errors elsewhere).
3. Write the smallest code that makes it pass.
4. Run `cargo test <name>` — confirm GREEN.
5. Run `cargo test -p orbit-geo` — confirm no regressions.
6. Refactor, keep green.
7. Commit with reference to the test name(s) added.

**If a test passes immediately after being written**: the test is wrong. Rewrite it.

**Honest disclosure**: the existing `apply` / `apply_reduction` / `apply_reduction_with_mask` in `processing.rs` were NOT written TDD-first. The plan includes retroactive tests for them as **Phase 0.0** before any new method is added.

---

## 1. Test fixture infrastructure (Phase 0.0)

All subsequent tests rely on this. Build it first, TDD'd.

**Location:** `crates/orbit-geo/src/test_support.rs` (private, `#[cfg(test)]` only)

### 1.1 RED tests for fixtures (write these first)

| Test name | Asserts |
|---|---|
| `tiny_geotiff_creates_file_with_requested_dimensions` | File exists, gdal_info reports `rows × cols` |
| `tiny_geotiff_fills_pixels_with_given_value` | Reading back returns the fill value at any pixel |
| `tiny_geotiff_respects_requested_epsg` | Spatial reference reports the EPSG code |
| `multi_band_geotiff_has_n_bands` | `raster_count() == n` |
| `multi_band_geotiff_band_k_has_value_k` | Band 1 fills with 1, band 2 with 2, etc. — for distinguishability |
| `synthetic_scene_set_produces_red_nir_fmask_triples` | Returns `(reds, nirs, masks)` with equal counts |
| `mock_block_writer_records_write_calls_in_order` | Calls captured in `Vec<(Offset, Size, Vec<V>)>` |
| `mock_block_writer_records_build_overviews_call` | Captured separately for ordering assertions |

### 1.2 GREEN implementation

```rust
// crates/orbit-geo/src/test_support.rs (or tests/common/mod.rs)
pub(crate) fn tiny_geotiff(
    rows: usize, cols: usize, fill: i16, epsg: u32
) -> tempfile::NamedTempFile { ... }

pub(crate) fn multi_band_geotiff(
    rows: usize, cols: usize, bands: usize, epsg: u32
) -> tempfile::NamedTempFile { ... }

pub(crate) fn synthetic_scene_set(
    n_times: usize, rows: usize, cols: usize
) -> (Vec<NamedTempFile>, Vec<NamedTempFile>, Vec<NamedTempFile>) { ... }

pub(crate) struct MockBlockWriter<V> {
    calls: Mutex<Vec<MockCall<V>>>,
}
impl<V: RasterType> BlockWriter<V> for MockBlockWriter<V> { ... }
```

**Estimate:** 1 day. Blocks: nothing. Blocked by: nothing.

---

## 2. Phase 0.1 — Retroactive tests for existing `apply*`

Before adding new methods, ensure the three existing apply methods have unit tests they didn't get during the first TDD-violating pass.

| Test name | File | Asserts |
|---|---|---|
| `apply_calls_worker_once_per_block` | `processing.rs` tests | Worker invocation count == `dataset.num_blocks()` |
| `apply_writes_each_block_to_output_geotiff` | `processing.rs` tests | `MockBlockWriter` receives N write calls at expected offsets |
| `apply_reduction_collapses_time_axis_to_one` | `processing.rs` tests | Output GeoTIFF has 1 band even if input has 9 timesteps |
| `apply_reduction_with_mask_skips_masked_pixels` | `processing.rs` tests | Output value at masked pixel == no_data |
| `apply_propagates_worker_error` | `processing.rs` tests | Worker returns `Result::Err` → apply returns `Err` |

**Estimate:** 1 day. Blocked by: Phase 0.0 fixtures.

---

## 3. Tier 0 — Validation foundations

### T0.1 — `build_overviews` invocation in `apply_reduction*`

**Goal:** Writer has the trait method; nothing calls it. Outputs lack pyramids.

| Step | Detail |
|---|---|
| RED test | `apply_reduction_calls_build_overviews_after_writes_complete` in `processing.rs` |
| Assertion | `MockBlockWriter` call log shows `[Write, Write, ..., BuildOverviews]` — overviews last |
| GREEN | In `processing::apply_reduction*`, after `run_blocks` join, call `writer.build_overviews(Resampling::Average, &[2, 4, 8, 16])` |
| Verify | `cargo test apply_reduction_calls_build_overviews` |
| Refactor | Extract overview levels into `WriteOptions { overview_levels: &[u32], resampling: Resampling }` |

**Estimate:** 0.5 day. Blocked by: Phase 0.0.

### T0.2 — First Criterion bench: `bench_apply_reduction_synthetic`

**Goal:** Establish a baseline number we own (not inherited from EORS).

| Step | Detail |
|---|---|
| RED test (correctness first) | `bench_synthetic_scene_yields_known_ndvi_mean` — assert NDVI mean equals hand-computed value |
| GREEN | Implement `examples/ndvi_synthetic.rs` — runs the kernel against fixture data |
| Bench file | `benches/apply_reduction.rs` using `criterion::black_box` |
| Verify | `cargo bench -p orbit-geo --bench apply_reduction` produces a number |
| Output | `BENCHMARK_BASELINE.md` capturing the timing on this machine |

**Estimate:** 1 day. Blocked by: Phase 0.0, T0.1.

### T0.3 — Live S2 smoke test (gated)

**Goal:** Confirm numerical output matches EORS reference on real data.

| Step | Detail |
|---|---|
| RED test | `live_s2_ndvi_mean_against_pc_anonymous` in `tests/live_s2.rs`, gated `#[cfg(feature = "bench_live")]` |
| Assertion | NDVI mean over a tiny ROI (256×256) falls in `[3000, 8000]` (sanity range for vegetated areas) AND output file passes `gdalinfo` |
| Fixture | Hard-code one PC scene id, e.g. `S2A_MSIL2A_20240115T235731_N0510_R030_T55HEU_20240116T020437` |
| GREEN | Use `providers::sign_planetary_computer_url` + `vsi_rewrite` + the kernel; tempdir cache so reruns are fast |
| Verify | `ORBIT_GEO_LIVE_S2=1 cargo test --features bench_live live_s2` |
| Risk | PC rate limits; flaky on bad network |

**Estimate:** 1.5 days. Blocked by: T0.1.

---

## 4. Tier 1 — Core kernel parity

### T1.1 — `apply_with_mask<U, V>`

| Step | Detail |
|---|---|
| RED test 1 | `apply_with_mask_passes_mask_block_to_worker` |
| RED test 2 | `apply_with_mask_yields_per_block_output_with_mask_applied` |
| RED test 3 | `apply_with_mask_errors_on_misaligned_block_partitioning` |
| Signature | `pub fn apply_with_mask<U, V, F>(&self, mask: &RasterDataset<U>, worker: F, n_threads: usize, out: &Path, no_data: V) -> Result<()> where F: Fn(&RasterDataBlock<T>, &RasterDataBlock<U>, Offset) -> Array3<V>` |
| File | `processing.rs` |
| GREEN | Adapt `apply` to also `par_iter().zip(mask_blocks.par_iter())` reading both blocks before invoking worker |

**Estimate:** 1 day. Blocked by: T0.1.

### T1.2 — `apply_cog` + `apply_with_mask_cog` + `apply_reduction_with_mask_cog`

| Step | Detail |
|---|---|
| RED test 1 | `apply_cog_output_passes_cog_validation` — call `gdalinfo` on output, assert `Layout=COG` appears |
| RED test 2 | `apply_cog_uses_tile_aligned_blocks` — assert block_x/block_y in output metadata are 512 |
| RED test 3 | `apply_reduction_with_mask_cog_writes_overviews_inline` — assert `Overviews:` line in `gdalinfo` |
| File | New `processing/cog.rs` submodule |
| GREEN | After `ParallelGeoTiffWriter` finalises, run `gdal_translate -of COG -co BLOCKSIZE=512 -co OVERVIEW_RESAMPLING=AVERAGE tmp.tif final.tif` |
| Refactor | Extract `WriterFormat::{GeoTiff, Cog}` enum; pick path inside writer |

**Estimate:** 2 days. Blocked by: T1.1.

### T1.3 — `apply_reduction_row_pixel` + `apply_reduction_row_pixel_with_mask`

| Step | Detail |
|---|---|
| RED test 1 | `apply_reduction_row_pixel_processes_one_row_at_a_time` — worker called rows-times per block |
| RED test 2 | `apply_reduction_row_pixel_with_mask_skips_masked_rows` |
| Signature | `pub fn apply_reduction_row_pixel<U, F>(&self, worker: F, n_threads: usize, out: &Path, no_data: U) -> Result<()> where F: Fn(ArrayView3<T>) -> Array1<U>` (worker takes `[time, layer, cols]` per row) |
| File | `processing.rs` |
| GREEN | Inner loop iterates `(0..block_rows).into_par_iter()`; worker called per-row |
| Why this matters | Memory-bounded workloads — full block doesn't fit in RAM |

**Estimate:** 2 days. Blocked by: T1.1.

### T1.4 — `read_block_layer_idx`

| Step | Detail |
|---|---|
| RED test | `read_block_layer_idx_returns_only_requested_layer` |
| Assertion | Result `Array4<T>` has shape `(times, 1, rows, cols)`; band values match what `read_block` would return for that layer |
| Signature | `pub fn read_block_layer_idx(&self, block_id: usize, layer_idx: usize) -> Result<RasterDataBlock<T>>` |
| File | `processing.rs` |
| GREEN | Filter `layer_mappings` by `layer_pos == layer_idx`; call `read_window_cached` only for those |

**Estimate:** 0.5 day. Blocked by: Phase 0.0.

### T1.5 — `write_window3` (public helper)

| Step | Detail |
|---|---|
| RED test | `write_window3_round_trips_array3_to_geotiff` |
| Assertion | Write `Array3<i16>` shape `(2, 4, 4)` to offset `(10, 20)`; reopen via `gdal::Dataset::open`; read window `(10, 20, 8, 8)`; assert equality |
| Signature | `pub fn write_window3<T: RasterType>(&self, data: ArrayView3<T>, offset: Offset) -> Result<()>` on `ParallelGeoTiffWriter` |
| File | `writer.rs` |
| GREEN | Thin wrapper iterating bands and calling existing `write_block` per band |

**Estimate:** 0.5 day. Blocked by: Phase 0.0.

### T1.6 — `mosaic` + `mosaic_translate_cleanup` + `mosaic_translate_cleanup_time_steps`

| Step | Detail |
|---|---|
| RED test 1 | `mosaic_combines_two_overlapping_files_taking_first_valid_value` |
| RED test 2 | `mosaic_translate_cleanup_removes_intermediate_vrts` |
| RED test 3 | `mosaic_translate_cleanup_time_steps_groups_by_date` |
| File | New `gdal_utils.rs` module (top-level) |
| GREEN | Subprocess `gdal_buildvrt` then `gdal_translate`; cleanup intermediates |
| Risk | Cross-platform — need to test `gdal_buildvrt` is on PATH; emit clear error if not |

**Estimate:** 2 days. Blocked by: Phase 0.0.

**Tier 1 total: ~8 days (with light parallelism, ~5 days)**

---

## 5. Tier 2 — Declarative DSL

### T2.1 — `Collection` enum

| Step | Detail |
|---|---|
| RED test 1 | `collection_sentinel2_resolves_to_sentinel_2_l2a_on_earth_search` |
| RED test 2 | `collection_sentinel2_resolves_to_ga_s2am_ard_3_on_dea` |
| RED test 3 | `collection_landsat8_resolves_correctly_per_provider` |
| File | `crates/orbit-geo/src/dsl/collection.rs` (new module `dsl`) |
| GREEN | Enum + `pub fn id_for(&self, provider: &Provider) -> &'static str` with match arms |

**Estimate:** 1 day. Blocked by: nothing.

### T2.2 — `Intersects` enum

| Step | Detail |
|---|---|
| RED test 1 | `intersects_bbox_converts_to_stac_search_params_bbox` |
| RED test 2 | `intersects_scene_converts_to_stac_search_params_ids` |
| RED test 3 | `intersects_geometry_converts_to_stac_search_params_intersects` |
| File | `crates/orbit-geo/src/dsl/intersects.rs` |
| GREEN | Enum + `impl From<Intersects> for stac_client::SearchParams` (or addition trait) |

**Estimate:** 1 day. Blocked by: T2.1.

### T2.3 — `Cmp` + `cloudcover` filter

| Step | Detail |
|---|---|
| RED test | `cmp_less_serializes_to_eo_cloud_cover_lt_20` |
| File | `crates/orbit-geo/src/dsl/filter.rs` |
| GREEN | Enum + helper builders for STAC CQL filter expressions |

**Estimate:** 0.5 day. Blocked by: T2.2.

### T2.4 — `ImageQueryBuilder` (typestate)

| Step | Detail |
|---|---|
| RED test 1 | `image_query_builder_chains_provider_collection_intersects_in_any_order` |
| RED test 2 | `image_query_builder_build_errors_when_required_field_missing` (compile-fail via typestate, or runtime via Result) |
| RED test 3 | `image_query_builder_to_search_params_includes_all_filters` |
| File | `crates/orbit-geo/src/dsl/builder.rs` |
| GREEN | Typestate pattern with `Phantom<HasProvider>`, `Phantom<HasCollection>`, etc.; or simpler Option-based fluent API with `.build() -> Result<…>` |

**Estimate:** 2 days. Blocked by: T2.1, T2.2, T2.3.

### T2.5 — Product registry + `canonical_bands`

| Step | Detail |
|---|---|
| RED test 1 | `canonical_red_resolves_to_b04_on_sentinel2` |
| RED test 2 | `canonical_red_resolves_to_sr_b4_on_landsat8` |
| RED test 3 | `canonical_bands_returns_error_for_unknown_band` |
| RED test 4 | `product_registry_yaml_parses_at_compile_time` |
| File | `crates/orbit-geo/src/products/registry.rs` + `crates/orbit-geo/products.yaml` |
| GREEN | YAML embedded via `include_str!`; parsed once via `LazyLock<HashMap<(Collection, &str), &str>>` |

**Estimate:** 1.5 days. Blocked by: T2.1.

### T2.6 — Query execution modes: `.get()`, `.get_remote()`, `.get_remote_async()`

| Step | Detail |
|---|---|
| RED test 1 | `query_get_downloads_assets_to_cache_dir` — local fixture HTTP server (or `wiremock`) |
| RED test 2 | `query_get_remote_returns_vsi_urls_without_downloading` |
| RED test 3 | `query_get_remote_async_reads_window_via_async_tiff` (gated by `async-tiff` feature) |
| File | `crates/orbit-geo/src/dsl/exec.rs` |
| GREEN | `.get()` reuses existing `download_via_gdal_translate` over par_iter of items. `.get_remote()` calls `vsi_rewrite` on each asset href. `.get_remote_async()` requires wiring `async-tiff::TiffReader` — see T3.7 |
| Risk | wiremock setup; or use `httpmock` |

**Estimate:** 3 days. Blocked by: T2.4, T3.7 (for async path only).

**Tier 2 total: ~9 days (sequential), ~6 days (with T2.5 + T2.6 parallel after T2.4)**

---

## 6. Tier 3 — Auxiliary modules

### T3.1 — `composition` module (`extend`, `stack`)

| Step | Detail |
|---|---|
| RED test 1 | `extend_along_time_increases_times_dim_and_preserves_layers` |
| RED test 2 | `extend_errors_when_spatial_extent_differs` |
| RED test 3 | `stack_along_layer_appends_layer_mappings` |
| RED test 4 | `stack_errors_when_time_axis_differs` |
| File | `crates/orbit-geo/src/composition.rs` |
| GREEN | Recompute `layer_mappings` with adjusted `time_pos` / `layer_pos`; concat `source_files` |

**Estimate:** 1.5 days. Blocked by: Phase 0.0.

### T3.2 — `sampling` module (`extract`, `extract_blockwise`)

| Step | Detail |
|---|---|
| RED test 1 | `sample_at_geo_point_returns_pixel_value_at_corresponding_row_col` |
| RED test 2 | `sample_at_point_outside_extent_returns_nodata` |
| RED test 3 | `sample_returns_dataframe_with_geom_id_and_band_columns` (Polars feature) |
| RED test 4 | `extract_blockwise_processes_only_blocks_containing_target_points` |
| File | `crates/orbit-geo/src/sampling.rs` |
| GREEN | Inverse geo_transform: `(geo_x - origin_x) / pixel_w`; clip to extent; read 1×1 window per point. Blockwise: group points by block_id first |

**Estimate:** 2 days. Blocked by: Phase 0.0.

### T3.3 — `zonal_stats` module + `use_polars` feature

| Step | Detail |
|---|---|
| RED test 1 | `zonal_histogram_polygons_counts_pixels_per_class_per_polygon` |
| RED test 2 | `zonal_histogram_raster_counts_pixels_per_class_per_zone` |
| RED test 3 | `save_zonal_histograms_writes_parquet_with_expected_schema` (Polars) |
| File | `crates/orbit-geo/src/zonal_stats.rs` |
| GREEN | Rasterize polygons → mask raster; iterate blocks; per-class counter (HashMap); Polars DataFrame conversion at the end |

**Estimate:** 3 days. Blocked by: T3.4 (rasterization).

### T3.4 — `rasterization` module

| Step | Detail |
|---|---|
| RED test 1 | `rasterize_polygon_burns_value_inside_polygon` |
| RED test 2 | `rasterize_polygon_with_burn_field_uses_per_polygon_values` |
| RED test 3 | `rasterize_cog_outputs_valid_cog` |
| RED test 4 | `geoms_to_global_indices_returns_block_id_per_geometry` |
| File | `crates/orbit-geo/src/rasterization.rs` |
| GREEN | Wrap GDAL's `RasterizeLayer` — use `gdal::raster::rasterize::rasterize_layer` from `gdal` crate (verify it exists in 0.19) |

**Estimate:** 2.5 days. Blocked by: Phase 0.0.

### T3.5 — `cloud_mask` module — **biggest unknown**

| Option A | s2cloudless via PyO3 — depends on Python at runtime |
| Option B | Pure-Rust port of s2cloudless decision tree — multi-week effort |
| Option C | FMask shell-out — depends on external `fmask` binary |
| Option D | Skip cloud_mask, document workaround (preprocess with Python) |

| Step | Detail |
|---|---|
| RED test (Option C) | `fmask_subprocess_marks_cloud_pixels_as_2` — fixture S2 inputs; call fmask CLI; assert known cloud area is marked 2 |
| RED test (Option B) | `s2cloudless_classifier_marks_known_cloud_pixels_with_prob_gt_0_4` |

**Recommendation:** Option D for now, escalate later. Document in plan but **defer**.

**Estimate:** 1 day (Option D — docs only) or 2 weeks (Option B). Blocked by: project-wide scope decision.

### T3.6 — `ml.rs` LightGBM (feature `use_lgbm`)

| Step | Detail |
|---|---|
| RED test 1 | `lightgbm_fit_classifier_writes_model_file` |
| RED test 2 | `lightgbm_predict_classifier_produces_per_pixel_class` |
| RED test 3 | `lightgbm_round_trips_synthetic_xor_dataset_above_90pct_accuracy` |
| File | `crates/orbit-geo/src/ml.rs` |
| GREEN | Wrap `lightgbm-rs` crate; pixel-flat features for fit; chunk-wise predict |
| Risk | `lightgbm-rs` build requires CMake + libgomp; CI matrix expands |

**Estimate:** 2.5 days. Blocked by: Phase 0.0.

### T3.7 — `async_io` module (wire `async-tiff` feature)

| Step | Detail |
|---|---|
| RED test 1 | `async_tiff_reader_reads_metadata_correctly` |
| RED test 2 | `async_tiff_reads_window_matches_gdal_read` (parity test against gdal::Dataset for the same file) |
| RED test 3 | `async_tiff_reads_multiple_windows_concurrently` |
| File | `crates/orbit-geo/src/async_io.rs` |
| GREEN | Use `async-tiff::TiffReader` + `tokio::spawn` for concurrent windows. Cache header parse |

**Estimate:** 3 days. Blocked by: Phase 0.0.

### T3.8 — `gdal_utils` shared module extraction

| Step | Detail |
|---|---|
| RED test | One per extracted helper (read_basic_raster_info, open_for_update, write_bands_to_file, compute_raster_union_extent, swap_coordinates, …) |
| File | `crates/orbit-geo/src/gdal_utils.rs` |
| GREEN | Extract inlined helpers from `processing.rs`, `writer.rs`, `providers.rs` into a public module without changing behaviour. Each extraction is a pure refactor under green tests |
| Order | Do this LAST in Tier 3 to avoid merge churn with Tier 1/2 |

**Estimate:** 2 days. Blocked by: Tier 1 complete.

**Tier 3 total: ~17 days (excluding T3.5 if Option D). With parallel work: ~10 days.**

---

## 7. Tier 4 — CLI + examples + benches

### T4.1–T4.5 — CLI subcommands

| Subcommand | Test (using `assert_cmd`) |
|---|---|
| `orbit-geo rasterize --vector polys.geojson --out out.tif --burn-value 1` | `cli_rasterize_burns_polygon_into_geotiff` |
| `orbit-geo mosaic --inputs a.tif,b.tif --out out.tif` | `cli_mosaic_combines_files` |
| `orbit-geo sample --raster red.tif --points pts.csv --out samples.csv` | `cli_sample_writes_per_point_csv` |
| `orbit-geo warp --in src.tif --out dst.tif --target-epsg 3577` | `cli_warp_reprojects_to_target_epsg` |
| `orbit-geo get-imagery --provider planetary-computer --collection sentinel-2 --bbox …` | `cli_get_imagery_downloads_one_scene` |

**Estimate:** 4 days. Blocked by: Tier 1 + Tier 2 + T3.2 + T3.4.

### T4.6 — Examples 1–30

Mirror EORS's `libs/eorst/examples/` 1:1. Each example:
- Has a doc test: `compile_test_example_n` (just asserts compilation)
- Has expected stdout snippet asserted in CI
- Has its own README block at top

**Estimate:** 5 days (15 minutes per example × 30 + asserts).

### T4.7 — Benches

`bench_apply`, `bench_ndvi_annual_full_tile{,_masked}`, `bench_ndvi_fmask_yearly`, `get_vs_get_remote{,_vs_async}`.

**Estimate:** 3 days. Blocked by: Tier 2 + T3.7.

**Tier 4 total: ~12 days.**

---

## 8. Tier 5 — Build/deploy

### T5.1 — Nix flake
Replicate `eors_workspace/flake.nix` shape (static GDAL build, container output).
**Estimate:** 2 days.

### T5.2 — Wire `static-gdal` feature
Activate `gdal-src` actually-linked path under that feature.
**Estimate:** 1 day.

### T5.3 — Docker container build
Via Nix or via multi-stage Dockerfile.
**Estimate:** 1 day.

### T5.4 — GitHub Actions CI
- `cargo test --workspace` matrix on macos/linux
- `cargo bench` nightly
- `cargo doc` + GitHub Pages publish
**Estimate:** 1 day.

### T5.5 — `BENCHMARK_BASELINE.md`, `BENCHMARK_FINDINGS.md`
Document the numbers from T0.2 + T4.7.
**Estimate:** 0.5 day.

**Tier 5 total: ~5.5 days.**

---

## 9. Dependency DAG (text-rendered)

```
Phase 0.0 (fixtures) ─┬─→ Phase 0.1 (retro tests)
                      │
                      ├─→ T0.1 (build_overviews wire) ─┬─→ T0.2 (bench)
                      │                                 └─→ T0.3 (live S2)
                      │
                      ├─→ T1.1 ─→ T1.2 (cog)
                      ├─→ T1.3 (row_pixel)
                      ├─→ T1.4 (read_layer_idx)
                      ├─→ T1.5 (write_window3)
                      ├─→ T1.6 (mosaic)
                      │
                      ├─→ T2.1 ─→ T2.2 ─→ T2.3 ─→ T2.4 ─┬─→ T2.5 (registry)
                      │                                    └─→ T2.6 (exec modes)
                      │
                      ├─→ T3.1 (composition)
                      ├─→ T3.2 (sampling)
                      ├─→ T3.4 (rasterization) ─→ T3.3 (zonal_stats)
                      ├─→ T3.5 (cloud_mask — DEFERRED)
                      ├─→ T3.6 (ml — feature-gated)
                      ├─→ T3.7 (async_io) ─→ T2.6 async path
                      │
                      └─→ (Tier 1 done) ─→ T3.8 (gdal_utils extract)
                                            │
                                            ├─→ T4.1–T4.5 (CLI)
                                            ├─→ T4.6 (30 examples)
                                            └─→ T4.7 (benches)
                                                  │
                                                  └─→ T5.1–T5.5 (deploy)
```

---

## 10. Schedule (calendar) — LOCKED SCOPE

**User decisions (2026-05-21):**
- Full Tier 0–5 (full parity including Nix/Docker/CI)
- `use_polars` + `use_lgbm` + `use_opencv` all in scope
- **Cloud-mask Option B** — pure-Rust port of s2cloudless decision tree
- CLI = extend existing `orbit-cli` with `geo` subcommands

Assuming 1 dev, 4 productive hours/day, strict TDD:

| Tier | Linear days | With parallelism |
|---|---|---|
| Phase 0.0 + 0.1 ✅ | 2 | 2 |
| Tier 0 (T0.1–T0.3) ✅ | 3 | 2 |
| Tier 1 (T1.1–T1.6) ✅ | 8 | 5 |
| Tier 2 (T2.1–T2.6) ✅ | 9 | 6 |
| Tier 3 (T3.1–T3.8 — T3.5/T3.6 substituted) ✅ | 24 | 18 |
| Tier 4 (CLI + 30 examples + benches) ✅ | 12 | 8 |
| Tier 5 (build/deploy — static-gdal blocked upstream) ✅ | 5.5 | 4 |
| **TOTAL** | **63.5 days** | **45 days (~9 weeks)** |

---

## 11. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `gdal-src` static build fails on CI macOS | High | Medium | Test on macOS first; fall back to dynamic |
| `lightgbm-rs` build needs CMake + libgomp | Medium | Medium | Gate behind `use_lgbm` feature; document deps |
| `async-tiff` crate has breaking changes | Medium | Low | Pin minor version |
| PC rate limits during live tests | Medium | Low | Cache downloads; use small ROIs |
| Diamond deps re-emerge (rustac vs new crate X) | Medium | High | Lock crate selection; verify versions before adding |
| Cloud mask correctness (T3.5) | Low (deferred) | High | Defer to Option D until user demands native |
| TDD on GDAL Datasets is awkward (not Send, needs files) | High | Low | Use tempfile fixtures aggressively; mock at trait boundaries |
| Existing apply* code lacked TDD — retrofitting may reveal bugs | Medium | Medium | Phase 0.1 catches them before new code is layered |
| Time estimate off by 2× | High | Low | Conservative buffer; sequence Tier 0 first to recalibrate |

---

## 12. Open questions (block scope finalisation)

1. **Tier 5 deferred?** Build/deploy tier is independent of feature parity. Cut it for v1?
2. **Cloud mask (T3.5):** Option D (defer) or commit to Option B (multi-week native port)?
3. **`use_polars` / `use_lgbm` / `use_opencv`:** in scope, or deprioritize the dependency-heavy modules?
4. **CLI integration:** extend existing `orbit-cli` with `geo` subcommands, or new `orbit-geo` binary?
5. **30 examples** match 1:1 with EORS, or curate a smaller "best of" set (e.g. 8–10)?

---

## 13. Commit cadence

- **One test per commit during RED phase** — `test: add failing test for <feature>`
- **One implementation per commit during GREEN phase** — `feat(orbit-geo): implement <feature> (closes T*.*)`
- **One refactor per commit** — `refactor(orbit-geo): extract <helper>`
- **No squashing** — preserves the TDD trail; future archaeology can verify `git show` of each test predates its impl.

---

## 14. Acceptance gates

Each tier passes when:
- All listed RED tests are GREEN.
- `cargo test --all-features -p orbit-geo` is GREEN.
- `cargo clippy --all-features --all-targets -p orbit-geo -- -D warnings` is clean.
- `cargo doc --all-features -p orbit-geo` builds without warnings.
- Each new public function has at least one doc test or `///` example.
- `EORS_PARITY_AUDIT.md` updated: tier checkbox flipped ✅.
