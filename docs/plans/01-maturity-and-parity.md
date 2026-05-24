# Orbit-rs Maturity & Parity Plan

> **Status**: Active. Started 2026-05-22.
> **Scope**: Bring `mvp/orbit-etl` to feature parity with `eors_workspace` on (a) breadth of STAC/OpenEO/cloud-mask integrations, (b) performance of geo kernels, (c) maturity / production use, (d) functional completeness — without copying eors code.
> **License posture**: `eors_workspace` is LGPL; `mvp/orbit-etl` is MIT/Apache. Every change in this plan must be implementable from public references (specs, papers, Apache-2 sources) so the resulting code is provably independent.

---

## Part 1 — Strategic Foundation

### 1.1 Clean-Room Protocol

| Activity | Source materials allowed | Source materials forbidden |
|---|---|---|
| Writing behavioural specs | eors source ✅ (describing observable behaviour) | — |
| Writing tests | Public docs, papers, golden datasets ✅ | eors source ❌ |
| Writing implementation | Specs + tests + public docs ✅ | eors source ❌ |
| Reviewing PRs | Specs + tests + diff against orbit ✅ | eors source ❌ (introduces taint) |

**Solo-dev rule**: 24-hour wash period between reading eors and writing orbit on the same topic. Specs + tests first from public materials, never the eors source.

### 1.2 License Hygiene Artefacts

- `LICENSE.MIT`, `LICENSE.APACHE-2.0`
- `NOTICE.md` — credits public references (STAC spec, GDAL, rasterio, sentinel-hub/sentinel2-cloud-detector, etc.) with versions/links
- `THIRD_PARTY.md` — explicit attribution for non-derived sources
- `docs/clean-room-protocol.md` — the table above

### 1.3 Architecture Invariants (anti-mimicry rules)

| Invariant | Why it differs from eors |
|---|---|
| Typed `thiserror::Error` with `#[from]` in libraries; no `anyhow::Result` in lib signatures | eors uses `anyhow` in libs |
| Async-first I/O at every public layer; sync only via explicit `spawn_blocking` | eors mixes sync + async unpredictably |
| Each public type lives in a concept-named module, not a file-named one | eors `rasterdataset/` god-module |
| Stage-based pipeline (Ingest → Plan → Execute → Sink) with explicit traits | eors composes inside builders |
| No `Arc<Mutex<Vec<_>>>` for parallel collection; rayon `collect_into_vec` or channels | eors uses Arc<Mutex<Vec>> |
| Errors carry structured context (`stage`, `asset`, `source`) | eors `Error::Other(String)` swallows source |
| Verb-noun naming (`fetch_assets`, `compose_mosaic`) | eors uses `apply_*` |

---

## Part 2 — Workspace Re-Layout

```
mvp/orbit-etl/crates/
├── orbit-core/             ✅ keep — error, ids, model
├── orbit-proto/            ✅ keep — generated protobuf
├── orbit-etl/              ✅ keep — file→Polars→SQLite
│
├── eo-core/                🆕 RasterType, BlockSize, Dimension, GeoTransform,
│                              RasterRegion, LayerMapping, NoData, CrsCode
│                              (zero-I/O; pure data types)
│
├── eo-io/                  🆕 GDAL bindings, COG read/write, async-tiff,
│                              VSI helpers, GeoTransform↔projection plumbing
│
├── eo-kernel/              🆕 Block-parallel apply/reduce, masked variants,
│                              SIMD reductions, rayon orchestration
│
├── eo-catalog/             🆕 STAC client, local STAC index (parquet+datafusion),
│                              auth providers (PC SAS, Earth Search S3, EarthData OIDC)
│
├── eo-process/             🆕 OpenEO API client + local process executor
│
├── eo-mask/                🆕 fmask, s2cloudless port, scl, qa_pixel
│
├── eo-vector/              🆕 Vector ops, rasterization, zonal stats
│
├── eo-dsl/                 🆕 ImageQueryBuilder + planner
│
└── orbit-geo/              ✂  shrinks to a façade re-exporting from the
                                new crates; deprecate over 2 minor versions

Plus roadmap:
- orbit-cache             moka + on-disk content-addressed store
- orbit-observability     tracing + prometheus + OTLP
- orbit-resilience        tower::retry + failsafe + bulkheads
- orbit-config            figment-based 12-factor config
```

---

## Part 3 — Four Axes

### Axis A — Breadth of STAC / OpenEO / Cloud-Mask

#### A.1 STAC (`eo-catalog`)

| Capability | Public reference | Independence note |
|---|---|---|
| Search v1.0.0 (POST /search, paging, conformance) | STAC API spec | Use the `stac` crate (BSD-3); don't re-impl item types |
| CQL2-text + CQL2-json filter | OGC API Features Part 3, CQL2 spec | Hand-rolled parser with `nom` or `chumsky` |
| Asset auth: PC SAS, Earth Search S3 SigV4, EarthData URS Bearer | Provider docs | Centralise behind `AssetSigner` trait |
| Local STAC index (Parquet-backed) | `stac-geoparquet` spec; datafusion docs | Replace broken `stac-duckdb 0.3.7` shell-out |
| Item-level caching with ETag/Last-Modified | RFC 7232 | Conditional GETs; eors does not have this |
| Stable token-based pagination | STAC API spec | Async stream, lazy materialisation |
| Catalog reachability check (`/conformance`) | STAC API spec | New surface |

#### A.2 OpenEO (`eo-process`)

| Capability | Public reference | Independence note |
|---|---|---|
| Remote backend client | OpenEO API 1.2 | Generate from OpenAPI schema (`openapi-generator`) |
| Auth: OIDC device code, Basic, Bearer | OpenEO Auth spec | Reuse `openid` crate |
| Local-side process executor (load_collection, filter_temporal/spatial, mask, reduce_dimension, save_result) | OpenEO Processes spec | Hand-rolled AST interpreter |
| Batch-job orchestration | OpenEO API | tower-retry retry layer |

#### A.3 Cloud-Mask (`eo-mask`)

| Algorithm | Public reference | Notes |
|---|---|---|
| Fmask 4.x | Zhu et al. 2012, 2015 | Re-implement from paper |
| s2cloudless | sentinel-hub/sentinel2-cloud-detector (Apache 2.0) | Port with attribution |
| SCL band decoder (S2 L2A) | Sentinel-2 Product Definition | Pure lookup table |
| QA_PIXEL decoder (Landsat C2) | USGS Landsat C2 Product Guide | Bitfield parser |
| CFMask | Foga et al. 2017 | Same approach as Fmask |

Single trait — eors does not abstract these:
```rust
pub trait CloudMaskProvider {
    type Sensor: SensorTag;
    fn mask(&self, scene: &Scene<Self::Sensor>) -> Result<MaskBand>;
    fn confidence(&self) -> MaskConfidence;
}
```

---

### Axis B — Performance (target: beat eors)

#### B.1 Allocation discipline

| Tactic | Where | Expected delta |
|---|---|---|
| `Vec::with_capacity` where size is knowable | `eo-kernel`, `eo-io` | 10–20% fewer allocs |
| `FxHashMap<Box<str>, _>` for band/asset/sensor keys | All | 15–30% faster lookups |
| `bytes::Bytes` end-to-end for asset payloads | `eo-catalog` → `eo-io` | Zero-copy slicing |
| `Cow<'static, str>` for builder fields | `eo-dsl` | 30–40% fewer string allocs |
| Thread-local `Array2<T>` arena for block buffers | `eo-kernel` | Eliminates malloc per block |
| Pool `String` / `PathBuf` in composite-mosaic loops | `eo-kernel` | Removes the audit-flagged clone storm |

#### B.2 Concurrency

| Tactic | Where |
|---|---|
| `rayon::par_iter().collect_into_vec` instead of `Arc<Mutex<Vec<_>>>` | every parallel collect |
| `tokio::sync::Semaphore` per-host for asset fetches | `eo-catalog` |
| `tokio::sync::mpsc<chunk=64>` pipeline channels for Scan → Mask → Reduce → Sink | `eo-kernel` |
| `tokio::task::JoinSet` for fan-out + cancellation | every async fan-out |
| Pinned worker threads for GDAL | `eo-io` |

#### B.3 GDAL & I/O tuning

```rust
gdal::config::set_config_option("GDAL_CACHEMAX",       "512")?;
gdal::config::set_config_option("VSI_CACHE",           "TRUE")?;
gdal::config::set_config_option("VSI_CACHE_SIZE",      "268435456")?; // 256 MiB
gdal::config::set_config_option("CPL_VSIL_CURL_CHUNK_SIZE", "1048576")?;
gdal::config::set_config_option("CPL_VSIL_CURL_USE_HEAD", "NO")?;
gdal::config::set_config_option("GDAL_HTTP_MULTIPLEX", "YES")?;
gdal::config::set_config_option("GDAL_HTTP_VERSION",   "2")?;
gdal::config::set_config_option("CPL_TMPDIR",          "/dev/shm")?;
```

#### B.4 Numerics
- `std::simd` portable SIMD for reductions
- Write COGs with ZSTD + `OVERVIEW_RESAMPLING=GAUSS`
- Store datetime as INTEGER unix-epoch in SQLite + index on `started_at`

#### B.5 Caching (`orbit-cache`)
- Two-tier: `moka::sync::Cache<Key, Bytes>` (in-mem LRU+TTL) + on-disk SHA-256-addressed store
- Manifest journal with checksum + ETag per asset
- Prometheus metrics: `cache_entries`, `cache_bytes`, `cache_hit_total`, `cache_miss_total`

#### B.6 Continuous performance measurement
- `criterion` benches per kernel + I/O path (synthetic 256² + 5490²)
- Bench dataset matrix in CI: small / medium / large
- PR fails if any bench regresses > 10%

---

### Axis C — Maturity / Production Use

#### C.1 Compatibility matrix (gated `#[cfg(feature = "live-datasets")]`)

| Dataset | Provider | Fingerprint |
|---|---|---|
| Landsat 8 C2 L2 SR | USGS / Planetary Computer | Single-scene NDVI mean ±0.001 |
| Sentinel-2 L2A | Element84 Earth Search | Cloud-mask coverage % ±0.1% |
| Sentinel-2 L1C → s2cloudless | Element84 | Probability-band hash matches |
| HLS | NASA LP DAAC | Monthly mosaic count = expected |
| Copernicus DEM 30 m | OpenTopography STAC | Pixel-area sum ±0.01% |
| MODIS MOD13A1 | NASA LAADS / PC | NDVI median = reference |
| NAIP | Planetary Computer | Auth-flow exercise (SAS) |

#### C.2 Edge-case taxonomy (mined from public bug reports)

| Class | Example |
|---|---|
| Metadata | item with `proj:wkt2` but no `proj:epsg` → resolve via `crs-definitions` |
| Geometry | AOI crossing multiple UTM zones → reproject to single CRS |
| IO | redirect chain on asset URL with auth header forwarded |
| Numerics | NODATA-only block → return NODATA, not NaN |
| Sensors | Sentinel-2 jp2 with subdatasets → enumerate before opening |
| Time | year-boundary monthly composite UTC vs local |
| Concurrency | concurrent writes to same output COG → file lock + atomic rename |
| Corruption | partial download → checksum mismatch → re-fetch |

#### C.3 Operational surface

| Feature | Crate |
|---|---|
| `tracing` spans on every public entry; `job_id`/`request_id`/`asset_url` | `orbit-observability` |
| Prometheus exporter: `requests_total`, `bytes_in/out`, `errors_total{kind}`, `kernel_block_seconds`, `cache_hit_ratio`, `gdal_open_seconds` | `orbit-observability` |
| OpenTelemetry (OTLP) | `orbit-observability` |
| `/healthz` + `/readyz` | `orbit-server` |
| Graceful shutdown with checkpoint persistence | `orbit-etl::engine` |
| `orbit-cli doctor` — GDAL/PROJ/network preflight | `orbit-cli` |
| Structured `orbit.toml` config (figment + env override) | `orbit-config` |
| Real sqlx migrations + roundtrip tests | `orbit-etl/migrations` |
| Idempotency + checkpoint resume | `orbit-etl/engine` |
| DLQ (`orbit_dlq` table; malformed rows captured) | `orbit-etl/engine` |

#### C.4 Resilience (`orbit-resilience`)
```rust
let asset_svc = tower::ServiceBuilder::new()
    .layer(tower::timeout::TimeoutLayer::new(Duration::from_secs(60)))
    .layer(tower::limit::ConcurrencyLimitLayer::new(per_host_cap))
    .layer(retry_layer_with_jitter(retry_policy()))
    .layer(circuit_breaker(failsafe::Config::new()))
    .service(http);
```

---

### Axis D — Functional Completeness / Parity

#### D.1 Surface diff (mechanical, not reading source)

`tools/api-diff/` — `syn`-based walker, emits CSV:
```
crate, module, kind, name, signature_hash, in_eorst, in_orbit_geo
```
`signature_hash` is over pretty-printed signatures without parameter names.

#### D.2 Prioritisation rubric
1-5 scoring per: customer demand × engineering cost × independence risk × performance opportunity.

#### D.3 Likely-missing items (priority order)
1. Mosaic compositing strategies (most-recent, median, max-NDVI, percentile-N)
2. Resampling beyond nearest/bilinear (cubic, average, mode, max, min)
3. Zonal statistics from vector polygons
4. Cross-sensor harmonisation (BOA↔TOA)
5. Time-binned composites (monthly, seasonal, custom-window)
6. Per-pixel time series
7. Vector rasterization
8. Output formats: NetCDF, Zarr v3
9. Pan-sharpening (Brovey, Gram-Schmidt)
10. Pixel-area-weighted reductions
11. Scene-date metadata extraction

#### D.4 Deprecation of `orbit-geo` monolith
After `eo-*` crates populated, `orbit-geo` becomes a façade with `#[deprecated]` re-exports; remove in two minor versions.

---

## Part 4 — Execution Sequence (12 weeks)

### Week 1 — Foundations
- [ ] Extract `eo-core` (pure data types, no I/O)
- [ ] `tools/api-diff/` → `docs/parity/SURFACE.md`
- [ ] Clean-room protocol doc + LICENSE notices
- [ ] CI matrix: stable, beta, MSRV; `cargo audit`; `cargo deny`; criterion dry-run

### Weeks 2-3 — Splitting + perf foundation
- [ ] Extract `eo-io` (GDAL + COG + async-tiff)
- [ ] Extract `eo-kernel`; replace `Arc<Mutex<Vec>>` patterns
- [ ] Extract `orbit-cache` (moka + on-disk)
- [ ] First criterion benches landed
- [ ] Workspace `[lints]` propagated to all new crates

### Weeks 4-5 — STAC + Cache
- [ ] `eo-catalog` search + asset resolution + PC + Earth Search auth
- [ ] Local STAC index over parquet
- [ ] Compatibility tests vs Landsat C2 + Sentinel-2 L2A
- [ ] `orbit-resilience` retry/timeout/circuit breaker

### Weeks 6-7 — Cloud-mask + Vector
- [ ] `eo-mask` with Fmask 4 (paper-derived), SCL decoder, QA_PIXEL decoder
- [ ] `eo-vector` with rasterization + zonal stats
- [ ] s2cloudless port with attribution
- [ ] Mosaic composites (median, max-NDVI, most-recent)

### Weeks 8-9 — OpenEO + Operational
- [ ] `eo-process` skeleton: capabilities + processes + local executor
- [ ] `orbit-observability`: tracing + Prometheus + OTLP
- [ ] `orbit-config`: figment-based
- [ ] sqlx migrations + DLQ + checkpoint resume

### Weeks 10-11 — Maturity push
- [ ] Compatibility matrix: nightly CI vs all 7 dataset families
- [ ] Edge-case taxonomy: 30+ tests from public bug archaeology
- [ ] Output formats: NetCDF + Zarr v3
- [ ] Perf gate enforced (10% regression budget)

### Week 12 — Cleanup + Release
- [ ] `orbit-geo` → façade with `#[deprecated]`
- [ ] Public docs + tutorials + migration guide
- [ ] First minor release; benchmark publication
- [ ] Spike: pan-sharpening + time-series extraction

---

## Part 5 — Definition of Done

| Axis | Done when… |
|---|---|
| **STAC** | Search + auth + asset resolution against ≥3 public catalogs; local index materially faster for repeats; integration tests green |
| **OpenEO** | Non-trivial process graph (load_collection → mask → reduce) runs both local + on a public OpenEO backend with byte-identical output |
| **Cloud-mask** | Fmask + s2cloudless + SCL + QA_PIXEL pass cloud-coverage-fingerprint test within ±0.5% of vendor |
| **Performance** | Criterion shows orbit ≥1.5× faster than fresh eors on medium dataset; alloc/block within 20% of theoretical minimum |
| **Maturity** | All 7 dataset families pass nightly compat tests for 14 consecutive days; ≥30 edge-case tests; zero lib `unwrap()`; `cargo deny` + `cargo audit` clean |
| **Parity** | Surface diff ≤5% of eors public items missing AND every miss documented as a conscious rejection |

---

## Part 6 — Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| LGPL contamination via accidental peek | Med | Critical | Clean-room protocol; wash period; per-PR sign-off |
| Algorithm correctness drift | High | Med | Anchor on vendor-reported coverage % not eors output |
| Performance regressions during refactor | Med | High | Criterion gate; per-crate rollback |
| STAC catalog churn | Med | Med | Replay fixtures (`wiremock`); live tests gated |
| Crate sprawl | Low | Med | Path-scoped CI; small but cohesive crates |
| MSRV churn | Low | Low | Stay one rustc behind for `eo-*` if needed |
| Solo-dev burnout | Med | High | Each crate independently shippable; week boundaries are safe pause points |
