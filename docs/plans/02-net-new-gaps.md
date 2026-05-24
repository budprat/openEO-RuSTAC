# Orbit-rs Net-New Gaps Plan

> **Status**: Draft. Started 2026-05-22. Sibling to `01-maturity-and-parity.md`.
> **Scope**: Land the 11 production-grade gaps (N1–N11) identified by the 2026-05-22 verification audit. These are net-new relative to the 12-week roadmap in `01-maturity-and-parity.md`; this document tracks them with task IDs, done-when criteria, owner notes, and coordination points with the parent plan.
> **License posture**: Same clean-room protocol as `01-maturity-and-parity.md` §1.1. All implementation must be derivable from public references (specs, papers, Apache-2.0 sources) — no LGPL eors source.
> **Strategic basis**: `13-geo-satellite/04-openeo-strategic-analysis.md` §4.5 (Approach D — client adapter + reference backend). All gaps below are bounded by §4.5.3 (MAY/WILL NOT contract).

---

## Part 1 — Relationship to the 12-Week Plan

`01-maturity-and-parity.md` (`01-MP`) commits to **breadth + perf + maturity + parity** across 12 weeks. It does **not** commit to:

| Net-new gap | Why 01-MP doesn't cover it |
|---|---|
| N1 Provenance | Not in any axis; assumed for free |
| N2 OutputContract | 01-MP §D.3 lists Zarr v3 + NetCDF outputs but no trait abstraction |
| N3 ExecutionContext | 01-MP §C.4 has tower-retry at HTTP layer only; no compute-layer ExecCtx |
| N4 GeoZarr conventions | 01-MP §D.3 lists Zarr v3 but not GeoZarr spec compliance |
| N5 Typed IR dispatch | 01-MP §A.2 mentions "local executor" but not the wire-up from `process_graph_ir.rs` into `GeoExecutor` |
| N6 Scientific parity harness | 01-MP §C.1 has dataset compat matrix; no pixel-equal vs rioxarray/openEO |
| N7 `Crs`/`PixelSize`/`GraphHash` newtypes | 01-MP §1.3 invariants list typed errors but not these specific newtypes |
| N8 Process library breadth | 01-MP §D.3 lists mosaic composites + indices but no `pub fn ndvi()` / index registry |
| N9 Product schema governance | Not on 01-MP |
| N10 openEO client adapter tests | Not on 01-MP |
| N11 Hard clippy + semver-checks + SBOM + fuzzing | 01-MP §Part 4 Week 1 includes `cargo audit` + `cargo deny`; rest is net-new |

**Coordination rule**: tasks in this plan that touch a Week N deliverable in 01-MP must list it as a dependency under "Owner notes" and a PR comment must link both.

---

## Part 2 — Architecture Invariants (additive to 01-MP §1.3)

| Invariant | Why |
|---|---|
| Every output write goes through `OutputContract`; concrete writers are private impls | Future Zarr/NetCDF/GeoZarr writers plug in without API churn |
| Every long-running compute carries `&ExecutionContext`; no internal `spawn` without it | openEO `DELETE /jobs/{id}` must propagate to live kernels |
| `Provenance` is written on every `OutputContract::finalize()`; no opt-out | Reproducibility is non-negotiable for a "reference backend" |
| `process_graph_ir::ProcessNode` is the **only** dispatcher in `GeoExecutor`; no string-match fallback | Static validation of the bounded process set |
| Library-level scientific functions live in `eo-process/src/algos/`, not in tests or examples | Inline-in-tests is a smell; algos must be callable from CLI/REST/PyO3 |
| Product YAML must declare `schema_version: 1` and use `#[serde(deny_unknown_fields)]` | Silent field drops are a long-tail bug source |

---

## Part 3 — Tracks

### Track G1 — Foundational Contracts (blocks Weeks 8-11 of 01-MP)

Covers gaps **N1, N2, N3, N5, N7**.

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G1.1 | `Crs` / `PixelSize` / `GraphHash` newtypes in `eo-core` | `eo-core::types` exports the three newtypes with `#[serde(transparent)]`; `derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)`; round-trip tests | Move from `crates/orbit-geo/src/types.rs` (currently empty) to `eo-core`; mirrors 01-MP §Part 2 (`eo-core` extraction, Week 1) | 01-MP Week 1 |
| G1.2 | `Provenance` struct + builder | `eo-core::provenance::Provenance` carries: source STAC IDs (`Vec<String>`), asset hrefs (`Vec<Url>`), band mapping (`BTreeMap<CanonicalBand, String>`), `GraphHash`, `software_version: &'static str` (compile-time `env!("CARGO_PKG_VERSION")`), parameters (`serde_json::Value`), `Crs`, `GeoTransform`, nodata, scale/offset, `OffsetDateTime`; serde round-trip test | Required for G1.3, G3.1, G5.1 | G1.1 |
| G1.3 | `OutputContract` trait | `eo-io::output::OutputContract` with methods: `fn open(&mut self, ProvenanceBuilder) -> Result<()>`, `fn write_block(&mut self, &Block, BlockPos) -> Result<()>`, `fn finalize(self) -> Result<OutputManifest>`; manifest includes STAC Item JSON; one impl (`CogWriter`) ships in this PR | Wraps existing `convert_to_cog` + `gdal_to_stac_item` (today wired only in `qvf.rs:565`, never reached from `apply*`) | G1.2 |
| G1.4 | `ExecutionContext` skeleton | `eo-core::exec::ExecutionContext { cancel: CancellationToken, mem_budget: MemoryBudget, progress: mpsc::Sender<ProgressEvent>, retry: RetryPolicy, temp: TempGuard }`; `cancel` checked at every block boundary in `eo-kernel`; cooperative shutdown test (start 1000-block job, cancel at block 50, assert <100 blocks ran) | Foundation for G1.5; coordinates with 01-MP Axis C.4 (resilience at HTTP layer) but lives in compute layer | G1.1 |
| G1.5 | Cancellation propagation: `DELETE /jobs/{id}` → `ExecutionContext::cancel` | `apps/orbit-openeo` route handler resolves the job's `CancellationToken` from `SqliteJobStore` and calls `.cancel()`; running compute returns `Err(JobCancelled)` within 1 block period; integration test asserts deleted job's compute exits in <2 s | Today the route exists but compute can't be interrupted | G1.4 |
| G1.6 | Wire `process_graph_ir::ProcessNode` enum into `GeoExecutor` | `geo_executor.rs` dispatches via `match` on `ProcessNode` (no `process_id: String` lookups); compile-time exhaustive match on the 6 bounded nodes; old string-dispatch deleted; `cargo check -p orbit-openeo` clean | Today `process_graph_ir.rs` is "standalone — doesn't yet plug into GeoExecutor" (line 23) | G1.4 |
| G1.7 | Structured `RunReport` | `eo-core::exec::RunReport { job_id, graph_hash, started_at, finished_at, input_assets, output_assets, blocks_total, blocks_completed, peak_mem_mb, errors: Vec<StructuredError> }`; written to disk at job completion; one CLI surface (`orbit-cli show-run <job_id>`) | Coordinates with 01-MP §C.3 (`tracing` spans) but is a job-level summary, not span data | G1.4 |

### Track G2 — Process Library Breadth (covers N8)

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G2.1 | `eo-process::algos::ndvi` library function | `pub fn ndvi<T: Float>(red: ArrayView2<T>, nir: ArrayView2<T>, nodata: T) -> Array2<T>`; matches today's `test_support.rs:540` `ndvi_mean_worker` numerically; doc-test compiles | Lifts NDVI out of tests/examples per Architecture Invariant | — |
| G2.2 | Vegetation/water/burn index registry | `eo-process::algos::indices` exports `pub fn ndwi`, `pub fn nbr`, `pub fn evi`, `pub fn savi`; each cited to its public formula; one Criterion bench per index; `IndexRegistry::list()` for runtime discovery | All formulas from public refs (no eors peek) | G2.1 |
| G2.3 | Band-math expression evaluator | `eo-process::bandmath::Expr` parses `(B08 - B04) / (B08 + B04)` via `nom` or `chumsky`; evaluator emits an `ArrayMath` plan callable from `Apply` ProcessNode; precedence + parens + nodata-aware; fuzzing harness (G7.4) targets the parser | Coordinates with 01-MP Axis A.1 CQL2 parser tooling | G1.6 |
| G2.4 | Median + percentile composites | `eo-process::composites::{median, percentile_n}` over time axis; pivot-based partial sort; benches vs `apply_reduction`; **mirrors 01-MP Axis D.3 item 1** | Cross-link to 01-MP Weeks 6-7 mosaic composites | G2.1 |

### Track G3 — Scientific Parity Harness (covers N6)

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G3.1 | Golden-fixture infrastructure | `tests/golden/` with: input GeoTIFF fixtures (small, ≤5 MB each, redistributable license), expected outputs as `.npy` arrays, tolerance specs (`atol`, `rtol`) per algo, fixture manifest (SHA-256 keyed) | One fixture set per algo in G3.2 | G1.2 |
| G3.2 | NDVI parity vs rioxarray | Python `tools/parity/gen_rioxarray_ndvi.py` regenerates expected outputs; Rust test `parity_ndvi_vs_rioxarray` asserts `allclose(rust_out, rioxarray_out, atol=0, rtol=0)`; passes on stable + nightly | First parity test — establishes pattern | G2.1, G3.1 |
| G3.3 | SCL mask parity vs sen2cor | Same shape as G3.2 against sen2cor output; tolerated diff documented if any | Coordinates with 01-MP Weeks 6-7 (`eo-mask`) | G3.1, 01-MP Weeks 6-7 |
| G3.4 | Warp + resampling parity vs GDAL | Test on 4 resampling kernels (nearest, bilinear, cubic, average); GDAL is the reference (orbit-rs uses GDAL anyway, so this catches plumbing bugs) | Coordinates with 01-MP Axis D.3 item 2 | G3.1 |
| G3.5 | Full 6-node process graph parity vs openEO Python | `load_collection → filter_temporal → filter_spatial → mask → reduce_dimension(NDVI mean) → save_result`; rust + openEO Python client run identical graph; outputs byte-identical | This is the "byte-identical" claim in 01-MP §Part 5 OpenEO row | G1.6, G3.2, G3.3 |

### Track G4 — Output Format Compliance (covers N4)

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G4.1 | Zarr v3 writer behind `OutputContract` | `eo-io::output::ZarrV3Writer` implements `OutputContract`; `zarr_codecs` per chunk; one integration test writes a 256³ cube and re-reads via Python `zarr-python 3.x` | Coordinates with 01-MP Weeks 10-11 Zarr v3 output | G1.3, 01-MP Weeks 10-11 |
| G4.2 | GeoZarr CRS + spatial-transform compliance | Output passes `geozarr-validator` (CLI from spec repo, MIT); `.zattrs` includes `_CRS`, `_X` / `_Y` coordinates with CF standard names; CRS round-trips through PROJ | GeoZarr is spec on top of Zarr v3; G4.1 must land first | G4.1 |
| G4.3 | NetCDF writer behind `OutputContract` | `eo-io::output::NetCdfWriter` implements `OutputContract`; CF-1.10 conformance via `compliance-checker` Python tool; one integration test | Coordinates with 01-MP Weeks 10-11 NetCDF output | G1.3, 01-MP Weeks 10-11 |
| G4.4 | STAC Item emission on every `finalize()` | Every `OutputContract::finalize()` returns a valid STAC 1.0.0 Item; `stac-validate` passes; provenance fields land in `properties.processing:*` (STAC processing extension) | Already wired in `qvf.rs:565` but never reached — now reachable via G1.3 | G1.3 |

### Track G5 — Schema Governance (covers N9)

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G5.1 | `CanonicalBand` enum + registry | Replace freeform `String` in `products.rs` with `enum CanonicalBand { Red, Green, Blue, Nir, Swir1, Swir2, ... }`; `Display` + `FromStr`; tests for all current product YAMLs | Coordinates with 01-MP §A.3 sensor tags | — |
| G5.2 | Versioned product YAML | Every `data/products/*.yaml` gains `schema_version: 1`; loader rejects unknown versions; migration helper for v0→v1 | Drop-in: all current files | G5.1 |
| G5.3 | Strict deserialization | `ProductDef` and child structs use `#[serde(deny_unknown_fields)]`; one test asserts unknown field causes error | Prevents silent drops | G5.2 |
| G5.4 | MODIS + SAR product YAMLs | At least one product per family with reference test (load + canonical band resolution + integration with `eo-catalog` STAC search) | Coordinates with 01-MP Axis A.1 / C.1 | G5.3, 01-MP Weeks 4-5 |

### Track G6 — Test Coverage (covers N10)

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G6.1 | `wiremock-rs` harness for `crates/orbit-geo/src/openeo.rs` | `tests/openeo_client.rs` mocks all 5 endpoints (`POST /jobs`, `POST /jobs/{id}/results`, `GET /jobs/{id}`, `GET /jobs/{id}/results`, `DELETE /jobs/{id}`); tests cover happy path, auth failure, timeout, 404 on delete | Today the file has zero tests | — |
| G6.2 | Cancellation test (client → backend round-trip) | Submit job to `apps/orbit-openeo`, `DELETE` within 1 s, assert no result file written and job state = `cancelled` | Cross-validates G1.5 | G1.5, G6.1 |
| G6.3 | Retry policy test | Client retries 503 with exponential backoff up to N=3; assertion on attempts via wiremock counter | Aligns with 01-MP Axis C.4 (tower-retry) | G6.1 |

### Track G7 — Supply Chain Hardening (covers N11)

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G7.1 | Hard clippy gate | `.github/workflows/ci.yml` removes `continue-on-error: true` from clippy step; PR fails on any clippy warning at workspace level; `#[allow(...)]` requires inline comment | 01-MP Part 4 Week 1 mentions clippy in CI but current state is soft-fail | — |
| G7.2 | `cargo semver-checks` on releases | Release job runs `cargo semver-checks check-release` and blocks if a breaking change is detected without a major version bump | Coordinates with `orbit-geo` deprecation path (01-MP §D.4) | G7.1 |
| G7.3 | SBOM generation (CycloneDX) | `cargo cyclonedx --format json` runs in release job; SBOM published as release asset; license inventory cross-checked against `deny.toml` `[licenses]` allowlist | Acceptable license list documented | G7.1 |
| G7.4 | Fuzzing harness for parsers | `fuzz/` directory with `cargo-fuzz` targets for: CQL2 parser (01-MP §A.1), openEO process graph JSON, band-math `Expr` parser (G2.3), product YAML loader; one target per parser; nightly fuzz run in CI | Run 5 min per target on PR, 1 h nightly | G2.3, G5.3 |
| G7.5 | `secrecy`/`zeroize` for OIDC tokens | All bearer tokens / SAS signatures wrapped in `secrecy::Secret<String>`; `Debug` impls scrubbed; one unit test asserts `format!("{:?}", token)` does not leak content | Audit one-shot through `apps/orbit-openeo` auth code | G7.1 |

### Track G8 — Datacube / xarray-shape API (net-new beyond original 02-NG scope)

Triggered by the 2026-05-23 orbit-etl improvements landing `RasterCube` / `CubeSchema` with xarray-like operations. orbit-geo's current `RasterDataset` is array-shaped, not cube-shaped. This track defines the typed cube layer and the dimensional algebra on top of it.

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G8.1 | `RasterCube` + `CubeSchema` types in `eo-core` | Cube carries named dims (`time`, `band`, `y`, `x`), CRS, grid, provenance, band metadata; round-trip serde test; doc-tests compile | Lives in `eo-core` alongside G1.1 newtypes | G1.1, G1.2 |
| G8.2 | Label & numeric selection (`sel`, `isel`) | `cube.sel(time="2024-06-01", band="nir")` returns a sub-cube with reduced dims; numeric range selection (`sel(time=range)`); 8 happy-path tests + 4 misuse tests (out-of-range, unknown label) | Reference: xarray API surface (public doc) — no source peek | G8.1 |
| G8.3 | Groupby + rolling windows | `cube.groupby_time("month").reduce(mean)` and `cube.rolling_time(7).reduce(median)`; reductions delegate to `eo-kernel` not the cube layer | Coordinates with G2.4 (composites) | G8.1, G2.4 |
| G8.4 | Rechunk plans + broadcasting + joins | `cube.rechunk(new_chunks) -> RechunkPlan`; `inner_join` / `outer_join` along a named dim; broadcasting rules documented in code | Plan-not-execute semantics: returns an inspectable plan that G10 executes | G8.1, G10.1 |
| G8.5 | `attrs` / `encoding` round-trip via STAC + Zarr | `cube.attrs.insert(...)` survives `OutputContract::finalize()` to STAC `properties` and Zarr `.zattrs`; integration test reads back via Python `xarray` and asserts attribute parity | Coordinates with G1.2 (Provenance), G4.1 (Zarr v3) | G1.2, G4.1, G8.1 |

### Track G9 — Production Scheduler (net-new beyond original 02-NG scope)

Triggered by the orbit-etl `ProductionScheduler` landing with checkpoint store, spill-to-disk, work-stealing, and resumability. 01-MP §C.3 mentions checkpoint resume but not the scheduler internals.

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G9.1 | `ProductionScheduler` skeleton | Receives a `BlockExecutionPlan` (from G10) and drives it through `ExecutionContext` (G1.4); deterministic work-stealing across N threads; integration test: 1000 blocks across 4 threads, deterministic order under fixed RNG seed | Sits between G1.4 ExecCtx and the kernel | G1.4, G10.2 |
| G9.2 | Checkpoint store | Per-block checkpoint write on `BlockComplete` event; restart reads checkpoints and skips completed blocks; resumability test (kill job at 50%, restart, assert remaining 50% finishes) | sqlx-backed (coordinates with 01-MP Weeks 8-9 `orbit-etl/engine` idempotency) | G9.1 |
| G9.3 | Spill-to-disk store | Intermediate arrays >`mem_budget` spill to temp dir under `ExecutionContext::temp`; on read, mmap; spill paths surfaced in `RunReport.spilled_windows` | RAII cleanup via G1.4 `TempGuard` | G1.4, G9.1 |
| G9.4 | Scheduler trace events | Structured events: `BlockSkipped`, `BlockSpilled`, `BlockRetried`, `BlockFailed`; emitted via `tracing::event!` and via `ExecutionContext::progress` channel; Prometheus counters | Coordinates with 01-MP Axis C.3 observability | G1.4, G1.7 |
| G9.5 | NDVI / composite expose scheduler observability | `eors process ndvi` and `eors process composite` CLI commands expose `--show-trace`, `--show-spills`, `--show-skipped` flags; output is JSON-Lines on stderr | Surfaces the G9.4 events at the user boundary | G9.4, G2.1, G2.4 |

### Track G10 — Memory-Aware Chunk Planning (net-new beyond original 02-NG scope)

Triggered by orbit-etl's `ChunkPlan` with real COG tile-size discovery + planned `ChunkWindow` + operation profiles. orbit-geo today uses fixed `BlockSize` regardless of input geometry.

| Task ID | Title | Done-when | Owner notes | Depends on |
|---|---|---|---|---|
| G10.1 | COG / GDAL tile-size discovery | `eo-io::probe_tile_size(path) -> TileGeometry` returns native tile dims, block layout, overview pyramid; integration test against synthetic + real COG; falls back gracefully for non-tiled rasters | Reads via existing GDAL handle; no new IO | G1.1 |
| G10.2 | Memory-aware `ChunkPlan` | `ChunkPlan::for_op(&OpProfile, &MemoryBudget, &[TileGeometry]) -> BlockExecutionPlan` chooses `ChunkWindow` sized to fit budget while respecting native tile boundaries; deterministic given inputs (testable) | Removes the fixed-`BlockSize` smell | G1.1, G10.1 |
| G10.3 | Operation profiles | `OpProfile { fn_id, input_dtype_size, output_dtype_size, mem_per_pixel: usize, mem_per_window_overhead: usize }`; one profile per algo in `eo-process::algos::*` (NDVI, composites, mask, warp) | Profiles can be tuned offline by Criterion measurements | G2.1, G10.2 |
| G10.4 | Replace ad-hoc `block_size` callers | All `apply*` paths inside `eo-kernel` consume a `BlockExecutionPlan` from G10.2 instead of a raw `BlockSize`; legacy `block_size` arg deprecated with `#[deprecated]` and a one-version removal window | Coordinates with 01-MP Week 12 `orbit-geo` façade | G10.2 |

---

## Part 4 — Coordination With 12-Week Plan

This table maps the partial / planned items the user surfaced into matching 01-MP slots and lists what G-tasks they need:

| 01-MP item | 01-MP slot | G-tasks needed first |
|---|---|---|
| Local process executor for 6 nodes with byte-identical output | Weeks 8-9 (`eo-process`) | G1.6 (typed dispatch), G3.5 (parity test) |
| Tower retry / timeout / circuit breaker per host | Weeks 8-9 (`orbit-resilience`) | — (independent; coordinate via G6.3) |
| `tracing` + Prometheus + OTLP | Weeks 8-9 (`orbit-observability`) | G1.7 (`RunReport` shape informs metrics) |
| figment 12-factor config | Weeks 8-9 (`orbit-config`) | — |
| Zarr v3 + NetCDF output | Weeks 10-11 | **G1.3 (OutputContract)** is a hard prerequisite; G4.1 / G4.3 land inside this slot |
| Mosaic composites (median, max-NDVI, percentile-N) | Weeks 10-11 | G2.1 (NDVI lib fn) + G2.4 (median/percentile) |
| CQL2-text/json filter parsing | Weeks 4-5 (`eo-catalog`) | Coordinate parser style with G2.3 (`nom`/`chumsky` choice) |
| Stable token-based STAC pagination as async stream | Weeks 4-5 | — |
| `CloudMaskProvider` trait | Weeks 6-7 (`eo-mask`) | G5.1 (`CanonicalBand`) helps API shape |
| Perf gate (>10% bench regression fails CI) | Week 12 | G7.1 (hard CI gates land first) |

---

## Part 5 — Execution Sequence

The 9 prioritized slices from the 2026-05-22 verification report, expanded with task IDs:

| Order | Slice | Task IDs | Effort | Unblocks |
|---|---|---|---|---|
| 1 | Wire typed IR into `GeoExecutor` (replaces string-dispatch) | **G1.6** | S | 01-MP Weeks 8-9 byte-identical claim |
| 2 | `Provenance` + GeoTIFF metadata write + STAC item emission | **G1.1 → G1.2 → G4.4** | M | All future writers inherit; reproducibility unblocked |
| 3 | `OutputContract` trait + `CogWriter` first impl | **G1.3** | M | 01-MP Weeks 10-11 Zarr/NetCDF can land cleanly |
| 4 | `ExecutionContext` + cancellation propagation | **G1.4 → G1.5 → G1.7** | M-L | Backend `DELETE /jobs/{id}` becomes real |
| 5 | `wiremock-rs` test suite for openEO client | **G6.1** | S | Confidence for any Weeks 8-9 work |
| 6 | Scientific parity harness (NDVI first) | **G3.1 → G3.2** | M | 01-MP §Part 5 OpenEO done-when becomes defensible |
| 7 | Typed `CanonicalBand`, versioned schemas, deny-unknown-fields | **G5.1 → G5.2 → G5.3** | S each | Multi-provider confidence |
| 8 | NDVI lib fn + index registry + band-math parser | **G2.1 → G2.2 → G2.3** | M | 01-MP §D.3 indices addressable |
| 9 | Hard clippy + semver-checks + SBOM + fuzzing | **G7.1 → G7.2 → G7.3 → G7.4 → G7.5** | M | Supply-chain hygiene; precondition for first release |
| 10 | Memory-aware `ChunkPlan` + COG tile discovery | **G10.1 → G10.2 → G10.3 → G10.4** | M | Removes fixed-`BlockSize` smell; enables G9 |
| 11 | `ProductionScheduler` (checkpoint + spill + work-stealing) | **G9.1 → G9.2 → G9.3 → G9.4 → G9.5** | L | Resumable batch; depends on G1.4 + G10.2 |
| 12 | `RasterCube` + xarray-shape API | **G8.1 → G8.2 → G8.3 → G8.4 → G8.5** | L | Dimensional algebra on top of typed cube; user-facing API surface for openEO + Python |

GeoZarr (G4.1 → G4.2) and NetCDF (G4.3) land **inside** 01-MP Weeks 10-11, gated by G1.3.

**Note (2026-05-23)**: Per the orbit-etl improvements landing, several G1–G7 tasks may already be partially or fully complete in tree (`ExecutionContext`, `BlockExecutor`, `ProcessGraph` IR, COG output contract, STAC sidecars, Zarr/GeoZarr metadata, `eors_python` PyO3 crate). Before starting any G-task, run `cargo check -p <crate>` and grep for the target symbol to confirm status. Update this plan to mark completed tasks with `[done: <commit-sha>]`.

---

## Part 6 — Definition of Done

| Track | Done when… |
|---|---|
| **G1 Contracts** | `Provenance` written on every output; `OutputContract` is the only writer entry-point; `ExecutionContext` cancellable end-to-end with <2 s latency; `process_graph_ir::ProcessNode` is the only dispatcher (zero string-match fallbacks); `RunReport` written on every job |
| **G2 Process library** | NDVI/NDWI/NBR/EVI/SAVI callable from CLI, REST, and PyO3 (when bound); band-math `Expr` parses real-world expressions from rioxarray / openEO docs; median + percentile composites match G3 harness within tolerance |
| **G3 Parity** | At least 5 golden-fixture parity tests green on stable + nightly; Python regenerator scripts checked in and reproducible; full 6-node graph parity vs openEO Python: byte-identical |
| **G4 Output formats** | Zarr v3 reads back via Python `zarr 3.x`; GeoZarr passes `geozarr-validator`; NetCDF passes CF-1.10 `compliance-checker`; STAC Item emitted and validated on every `finalize()` |
| **G5 Schema governance** | All product YAMLs declare `schema_version: 1`, use `deny_unknown_fields`, and pass `CanonicalBand` enum resolution; MODIS + SAR products land with one parity test each |
| **G6 Test coverage** | `crates/orbit-geo/src/openeo.rs` reaches ≥80% line coverage; cancellation round-trip test green; retry policy test green |
| **G7 Supply chain** | Clippy hard-fail in CI; `cargo semver-checks` blocks release on undeclared breaks; CycloneDX SBOM published per release; nightly fuzz run for 1 h on each parser with zero new findings for 7 consecutive days; OIDC tokens never appear in `Debug` / logs |
| **G8 Datacube** | `RasterCube` round-trips through STAC + Zarr with attrs preserved; `sel`/`isel`/`groupby`/`rolling` covered by ≥20 tests; xarray Python round-trip parity test green; rechunk + broadcast + join plans serialize as inspectable JSON |
| **G9 Scheduler** | 1000-block job runs deterministically across N threads under fixed RNG seed; kill-restart resumability test green; spill paths recoverable from `RunReport`; trace events visible in Prometheus + as JSON-Lines on stderr |
| **G10 Chunk planning** | Native COG tile geometry probed before any kernel call; `ChunkPlan::for_op` is deterministic given `(OpProfile, MemoryBudget, TileGeometry)`; legacy `block_size` arg deprecated with one-version sunset window; perf bench shows zero regression vs. fixed-BlockSize baseline |

---

## Part 7 — Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Cancellation token plumbing leaks into hot path (every block check costs ns) | Med | Med | Benchmark before/after G1.4 lands; `Relaxed` atomic load; bail on 1-in-N blocks rather than every block if cost > 1% |
| `Provenance` schema churn breaks downstream consumers | Med | High | Lock `Provenance` serde schema to `schema_version: 1` from day one; add own version field; treat as public API once shipped |
| GeoZarr spec moves before G4.2 lands | Low | Med | Pin to spec version; document in `Provenance.parameters` |
| Hard clippy gate blocks unrelated PRs during transition | High | Low | Land in two passes: (1) fix all current warnings, (2) flip the gate |
| Golden fixtures grow unbounded | Med | Med | 5 MB per-fixture cap; LFS only if necessary; rotate fixtures yearly |
| openEO 1.3.0 deprecated upstream before release | Low | High | Doc'd in §4.5.2 of strategic analysis; backend explicitly non-certified; sunset path documented |
| Fuzzing finds CVEs in already-shipped parser surface | Med | High | Treat as security advisory; fix in same release; SBOM provides downstream blast radius visibility |
| Solo-dev burnout (per 01-MP Part 6) | Med | High | Tracks G1–G7 are independently shippable; G1 alone is a valid first release |

---

## Part 8 — Cross-References

- **Parent plan**: [`01-maturity-and-parity.md`](./01-maturity-and-parity.md) — 12-week roadmap
- **Strategic basis**: [`../../../../13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md) — Approach D
- **Verification audit (source of N1–N11)**: session transcript 2026-05-22
- **Surface diff**: [`../parity/SURFACE.md`](../parity/SURFACE.md) (auto-regen by `tools/api-diff/`)
- **Symbol-level audit**: [`../parity/EORS_COMPONENT_COMPARISON.md`](../parity/EORS_COMPONENT_COMPARISON.md)
- **Honest gap inventory**: [`../parity/MISSING_FEATURES.md`](../parity/MISSING_FEATURES.md)
- **Perf baseline (G7 gate compares against this)**: [`../perf/BENCHMARK_BASELINE.md`](../perf/BENCHMARK_BASELINE.md)
- **Historical execution records** (Tier 0-5 complete 2026-05-21): [`../archive/2026-05-21-parity-audit.md`](../archive/2026-05-21-parity-audit.md), [`../archive/2026-05-21-parity-plan.md`](../archive/2026-05-21-parity-plan.md)
- **Clean-room protocol**: [`../clean-room-protocol.md`](../clean-room-protocol.md)
- **License hygiene**: [`../../NOTICE.md`](../../NOTICE.md), [`../../THIRD_PARTY.md`](../../THIRD_PARTY.md)
- **Backend scope contract**: [`../../apps/orbit-openeo/BACKEND-SCOPE.md`](../../apps/orbit-openeo/BACKEND-SCOPE.md)
- **Docs index**: [`../README.md`](../README.md)
