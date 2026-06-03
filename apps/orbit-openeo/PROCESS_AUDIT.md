# openEO Process Implementation Audit

> Scope: `apps/orbit-openeo` — the openEO 1.3.0 reference backend.
> Date: 2026-06-03. Authoritative process set: `geo_executor/registry.rs::register_defaults`.
> Status: audit complete; all findings below remediated in the same change-set
> (see "Remediation" column). Verified with `cargo test --features geo-kernel`
> on a libgdal host (GDAL 3.8.4).

This document audits whether the implemented openEO processes are spec-faithful
and whether they **actually transform the input data** for a process graph
POSTed to `/jobs` / `/result`, versus passing it through unchanged.

---

## 1. Methodology

The execution path for a submitted graph is:

```
POST /jobs|/result
  → parse_graph                          (geo_executor/mod.rs)
  → ProcessGraphAnalysis::build          (process_graph.rs — petgraph topo sort,
                                            cycle + depth guards, P0-3 callback
                                            namespace isolation)
  → evaluate(): for each node in topo order
        resolve from_node args
        ProcessRegistry::get(process_id)  (geo_executor/registry.rs)
        handler.handle(self, args)        (one eval_* per process)
  → finalise_save_result                 (bytes for save_result)
```

I inventoried every dispatcher arm (`register_defaults`), read every
`eval_*.rs`, the three sub-callback evaluators (`apply`, `reduce_dimension`,
`merge_cubes overlap_resolver`), and the discovery / validation routes.

## 2. Implemented process inventory (68 after this change)

| Group | Processes |
|---|---|
| Cube pipeline | `load_collection`, `save_result`, `reduce_dimension`, `apply`, `mask`, `merge_cubes`, `ndvi` |
| Filters | `filter_temporal`, `filter_spatial`, `filter_bbox`, `filter_bands` |
| Cube metadata | `rename_labels`, `add_dimension`, `drop_dimension` |
| Reproject / aggregate | `resample_spatial`, `aggregate_spatial` (NEW) |
| S2 mask conveniences | `mask_scl_dilation`, `mask_from_values` |
| Orbit raster extensions | `aggregate_spatial_polygon`, `aggregate_spatial_point`, `zonal_histogram`, `fit_classifier`, `predict_classifier` |
| Arithmetic | `add`, `subtract`, `multiply`, `divide` |
| Unary/binary math | `absolute`, `sqrt`, `power`, `exp`, `ln`, `log`, `sgn`, `floor`, `ceil`, `int`, `round`, `mod`, `clip`, `normalized_difference` |
| Trigonometry | `cos`, `sin`, `tan`, `arccos`, `arcsin`, `arctan`, `arctan2` |
| Comparison | `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `between` |
| Logical | `and`, `or`, `xor`, `not` |
| Arrays | `array_element`, `array_create`, `array_concat`, `array_append`, `array_contains`, `array_find`, `count`, `order`, `sort` |

`process_catalog.rs` is the single non-gated source of truth for this set; a
`geo-kernel` test (`registry_matches_process_catalog`) asserts it equals the
runtime registry so the two cannot drift.

## 3. Findings & remediation

| ID | Severity | Finding | Location | Remediation |
|---|---|---|---|---|
| **H1** | High | `GET /processes` returned `{"processes": []}` — 67 implemented processes were undiscoverable, so openEO clients saw an empty backend. | `routes/catalogs.rs:53` | `list_processes` now returns `process_catalog::process_descriptions()` (id, summary, parameters, returns, categories). Test `list_processes_advertises_implemented_set`. |
| **H2** | High | `filter_temporal` / `filter_spatial` / `filter_bbox` were **silent no-ops** — they tagged `_orbit_meta.applied` and forwarded `data` unchanged, so a graph relying on them to subset got UNFILTERED data back with no error. | `registry.rs` + `mod.rs::filter_passthrough` | All three now perform **real filtering**: `filter_temporal` prunes scenes by STAC `datetime` (newly carried onto the cube); `filter_bbox`/`filter_spatial` re-crop every (band, scene) raster to the new extent via the in-process GDAL crop. Inputs that can't be filtered are now **rejected**, not silently passed. `eval_cube_ops.rs`. |
| **M1** | Med | Three divergent expression evaluators: `apply` and `reduce_dimension` callbacks rejected processes (`normalized_difference`, trig, `mod`, `floor`/`ceil`/`round`/`sgn`/`int`, `between`) that the top-level registry accepts. | `eval_apply.rs`, `eval_reduce.rs` | Both sub-evaluators extended to the same pure-numeric set as the registry. Tests `eval_apply_subgraph_supports_extended_math_set`. |
| **M2** | Med | `resample_spatial` warped only the **first band of the first scene** and dropped the rest; `resolution` silently ignored. | `eval_misc.rs:486` | Rewrote to warp **every (band, scene)** and return a band-preserving `__cube`; a non-zero `resolution` is now rejected (not silently ignored). |
| **M3** | Med | Standard `aggregate_spatial(data, geometries, reducer)` was missing — clients got `UnknownProcess`. | n/a | Implemented: reprojects GeoJSON polygons into the raster CRS, rasterises, and reduces inside-pixel values per geometry/scene with the reducer callback. `eval_misc.rs`. |
| **M4** | Med | `POST /validation` ran JSON-schema only and never checked process existence; unknown processes validated clean and failed only at run time. `POST /jobs` checked only graph shape. | `routes/validation.rs`, `routes/jobs.rs` | Added `process_graph::unsupported_process_ids` (checks TOP-LEVEL `process_id`s; P0-3 short-circuit leaves callback-only processes like `min`/`max`/`linear_scale_range` to run-time validation by `apply`/`reduce`). `/validation` emits `ProcessUnsupported`; `/jobs` rejects at submit. |
| **L1** | Low | `apply` callback bound the pixel only to a parameter literally named `x`; other names errored. | `eval_apply.rs:94` | Binds the pixel to whatever single `from_parameter` name the callback declares. |
| **L2** | Low | Project labels itself "openEO 1.3.0"; the latest official openEO **API** release is 1.2.0 (the *processes* spec is versioned separately). | `BACKEND-SCOPE.md`, `.well-known/openeo` | **Recommendation only** — reconcile the version label against openeo.org. No pin change (BACKEND-SCOPE §4 is change-controlled). |

## 4. Confirmed correct (no change needed)

- **Graph engine** (`process_graph.rs`): petgraph topo sort, cycle detection,
  argument-depth DoS guard, and the **P0-3** sub-callback namespace isolation
  (inner `process_graph` `from_node`s do not leak to the outer walk).
- **`reduce_dimension`**: 10 statistical reducers + arbitrary callback
  expressions, over both the `t` and `bands` axes; genuinely reduces pixels.
- **`merge_cubes`**: Case 1 (band-axis join), Case 2 (`overlap_resolver`),
  Case 3 (spatial mosaic).
- **`load_collection`**: honors `spatial_extent`(+CRS), `temporal_extent`,
  `bands`, `properties.eo:cloud_cover`; DN→reflectance scaling from STAC
  `raster:bands`; SSRF guard on asset hrefs; scene-limit clamp.
- **`ndvi` / `mask` / `mask_scl_dilation` / `mask_from_values`**: per the
  documented A3/A7/A8/SCL-20m invariants.
- **Scalar / array math semantics**: `mod` divisor-sign, banker's `round`,
  `eq` optional `delta`, `normalized_difference` zero-guard.
- **Cube-metadata ops**: `filter_bands` (BandNotAvailable), `rename_labels`,
  `add_dimension` / `drop_dimension`.

## 5. Residual limitations (documented, not bugs)

- `aggregate_spatial` v1: single statistical reducer (compound callbacks
  rejected); exterior rings only (holes ignored); operates on the cube's
  primary band.
- `resample_spatial`: projection-only (no `resolution` resampling).
- `aggregate_spatial_polygon` / `_point`: pixel/world-space extensions, not the
  spec `aggregate_spatial` — retained for back-compat.
- `save_result`: GTiff / PNG / JSON; NetCDF maps to TIFF; Zarr unimplemented.

## 6. Verification

- Non-GDAL surface (discovery, validation, graph walker, process catalog):
  unit tests in `process_catalog`, `process_graph`, `routes::{catalogs,validation}`.
- GDAL surface (`--features geo-kernel`): registry↔catalog consistency,
  extended apply/reduce math, real `filter_temporal` pruning, whole-cube
  `resample_spatial`, and `aggregate_spatial` zonal reduction.
- End-to-end (libgdal host): `GET /processes` returns the full set; a graph
  using `filter_bbox`+`filter_temporal` shrinks the output extent/scene-count;
  an `apply` graph using `normalized_difference`/`arctan` no longer errors.
