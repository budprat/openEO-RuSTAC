# orbit-geo

> Block-parallel raster processing kernel for the `orbit-rs` ecosystem.
> Clean-room implementation of the `apply_reduction_with_mask` API shape.
> MIT OR Apache-2.0.

## Why this exists

The upstream raster engine measured a **significant speedup over Python ODC/STAC** on the canonical EO benchmark: NDVI-mean-over-time with FMask cloud masking across 9 Sentinel-2 timesteps.

| Stack | Cached run |
|---|---|
| Python ODC/STAC + rioxarray + dask | 301 s |
| Rust upstream raster engine (`apply_reduction_with_mask`) | **12 s** |

The pattern that produced that result:

```
STAC query        →   ~1 s
Download + cache  →   ~93 s   (cached re-run: 22 ms)
Build dataset     →   ~0.7 s   (RasterDatasetBuilder, 36 blocks of 2048×2048)
apply_reduction   →   ~10 s    (rayon par_iter over blocks → ParallelGeoTiffWriter)
                ─────────
        Total:    12 s cached / 105 s first-run
```

`orbit-geo` is the **`orbit-rs` re-implementation of that compute kernel**, designed so that the orbit framework (ETL, LLM agent, satellite, distributed) shares one canonical raster API rather than maintaining a fork of the upstream engine.

## Acknowledgement & legal posture

This crate's **public API shape** intentionally mirrors the upstream raster engine's primary crate (LGPL-3.0); see `NOTICE.md` for attribution. It was inspired by reading the well-documented public surface and benchmark transcripts; **no implementation code was copied**.

- **API shape** (function names, parameter order, semantics) — same as upstream so users can mentally port between engines.
- **Implementation** — independent, written from the public-API contract.
- **License** — MIT OR Apache-2.0 (this crate). The upstream engine remains LGPL-3.0.
- **Credit** — the upstream maintainers deserve credit for proving the block-parallel speedup pattern at scale. This crate exists because that work showed the pattern is the right design; we want the same ergonomics inside `orbit-rs` with a permissive license.

If you need a battle-tested production implementation **today**, use the upstream raster engine. `orbit-geo` is the version that integrates with the `orbit-rs` `Processor`/`Pipeline`/`Cluster` abstractions and a permissive license.

## Public API at a glance

```rust
use orbit_geo::{
    DataSource, DataSourceBuilder, RasterDataset, RasterDatasetBuilder,
    RasterDataBlock,
    types::{BlockSize, Dimension, ImageResolution},
};
use ndarray::Array3;
use std::path::PathBuf;

// 1. Acquire scene files (downloaded from STAC, or already on disk).
let scenes: Vec<PathBuf> = vec![
    "/cache/S2A_T56JNS_20260415_red.tif".into(),
    "/cache/S2A_T56JNS_20260415_nir.tif".into(),
    "/cache/S2A_T56JNS_20260415_fmask.tif".into(),
];

// 2. Build an aligned dataset.
let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&scenes)?
    .resolution(ImageResolution { x: 10.0, y: -10.0 })
    .block_size(BlockSize { rows: 2048, cols: 2048 })
    .build()?;

// 3. Define a worker — pure Rust, runs in parallel across blocks.
fn ndvi_mean_masked(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> Array3<i16> {
    // ... compute NDVI per timestep, filter by FMask, mean over time.
    Array3::zeros((1, rdb.rows(), rdb.cols()))
}

// 4. Apply, writing results directly to a single output GeoTIFF.
rds.apply_reduction::<i16>(
    ndvi_mean_masked,
    Dimension::Layer,
    8,                                  // n_threads
    &"output_ndvi_mean.tif".into(),
    i16::MIN,                           // no-data sentinel
)?;
```

## Module layout

```
src/
├── lib.rs           ← public re-exports + crate docs
├── types.rs         ← RasterShape, BlockSize, Dimension, GeoTransform, Offset, …
├── error.rs         ← Error enum (thiserror) + Result alias
├── source.rs        ← DataSource enum + builder; optional STAC cache helper
├── block.rs         ← RasterRegion (block metadata) + RasterDataBlock<T> (worker input)
├── dataset.rs       ← RasterDataset<T> struct
├── builder.rs       ← RasterDatasetBuilder<T>
├── processing.rs    ← apply, apply_reduction, apply_reduction_with_mask
└── writer.rs        ← ParallelGeoTiffWriter (Mutex<Dataset> behind a trait)
```

## Three public entry points on `RasterDataset<R>`

| Method | When to use | Worker signature |
|---|---|---|
| `apply` | Output shape matches input layers (filter, mask) | `Fn(&RasterDataBlock<R>) → Array3<V>` |
| `apply_reduction` | Collapse layer or time axis into a single band per block (NDVI mean, max composite) | `Fn(&RasterDataBlock<R>, Dimension) → Array3<V>` |
| `apply_reduction_with_mask` | Same as above, with a parallel-aligned mask dataset (clouds, water, AOI) | `Fn(&RasterDataBlock<R>, &RasterDataBlock<U>, Dimension) → Array3<V>` |

All three:
- Pre-create the output GeoTIFF once
- Iterate `blocks` via `rayon::par_iter` on a dedicated `ThreadPool`
- Read each block via GDAL VSI (or async-tiff with feature flag — see [§5 of `13-geo-satellite/02`](../../../13-geo-satellite/02-stac-sentinel2-2026.md))
- Hand the block to the worker
- Trim overlap pixels
- Write the inner window into the shared `ParallelGeoTiffWriter`

No intermediate files. No subprocess mosaic step. **That is the block-parallel speedup pattern.**

## openEO as a `DataSource` (Approach C, committed Phase-5)

orbit-geo treats openEO backends (CDSE, VITO, EODC, …) as **one of several data sources**. Build a `DataSource::OpenEO`, hand it to the same `RasterDatasetBuilder`, and run the same `apply_reduction_with_mask` worker:

```rust
use orbit_geo::{
    DataSource, RasterDataset, RasterDatasetBuilder,
    types::{BlockSize, Dimension},
    openeo::{OpenEoAuth, PollConfig, submit_and_download},
};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Process graph (use the openeo Python client or write JSON directly).
    let graph = json!({
        "loadcollection1": {
            "process_id": "load_collection",
            "arguments": {
                "id": "SENTINEL2_L2A",
                "spatial_extent": { "west": 13.0, "south": 52.0, "east": 14.0, "north": 53.0 },
                "temporal_extent": ["2026-04-01", "2026-04-30"],
                "bands": ["B04", "B08"]
            }
        },
        // … reduce_dimension, save_result, etc.
    });

    let cache = PathBuf::from("./openeo-cache");
    let scenes = submit_and_download(
        "https://openeo.dataspace.copernicus.eu",
        &graph,
        &OpenEoAuth::OidcBearer(std::env::var("CDSE_TOKEN")?),
        &cache,
        PollConfig::default(),
    ).await?;

    // 2. Same orbit-geo kernel — openEO is just an upstream source now.
    let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&scenes)?
        .block_size(BlockSize { rows: 2048, cols: 2048 })
        .build()?;

    rds.apply_reduction::<i16>(
        |block, _dim| ndarray::Array3::zeros((1, block.rows(), block.cols())),
        Dimension::Layer,
        8,
        &"out.tif".into(),
        i16::MIN,
    )?;
    Ok(())
}
```

Build with: `cargo build -p orbit-geo --features openeo`.

The `openeo` feature pulls in `reqwest` (rustls), `sha2`, `hex`, `url`. No `openeo` Rust crate exists on crates.io (verified May 2026), so orbit-geo ships its own focused ~280-line client covering the 7 endpoints needed (`POST /jobs`, `POST /jobs/{id}/results`, `GET /jobs/{id}`, `GET /jobs/{id}/results`, `DELETE /jobs/{id}`).

The full design rationale for **not** building an openEO *backend* — only a client — is in [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../13-geo-satellite/04-openeo-strategic-analysis.md).

## STAC integration (rustac stack — **wired, compile-verified**)

The `stac` feature pulls in the real rustac crates and `src/stac.rs` calls their actual public APIs. Verified with `cargo check -p orbit-geo --features stac,stac-duckdb,openeo` — clean build.

Crate versions pinned and verified against crates.io 2026-05-21:

| Crate | Pin | Used for |
|---|---|---|
| `stac` 0.17 | core | `Item`, `Catalog`, `SelfHref` re-exports |
| `stac-client` 0.3 | API client | `Client::new`, `get_collections`, `search(&SearchParams)` |
| `stac-io` 0.2 | format I/O | `read::<T>(href)`, `write(path, value)` for JSON/NDJSON/GeoParquet |
| `stac-validate` 0.6 | schema validation | `Validator::new().await` + `.validate(&v).await` |

What works today:

```rust
use orbit_geo::stac::{StacClient, download_items, DownloadOpts, validate};

// 1. Query a real STAC API
let client = StacClient::new("https://earth-search.aws.element84.com/v1")?;
let collections = client.collections().await?;

let mut params = stac_client::SearchParams::default();
// (populate params per stac_client::SearchParams docs)
let items = client.search(&params).await?;

// 2. Download assets locally
let paths = download_items(
    &items.features,                 // &[stac::Item]
    std::path::Path::new("./cache"),
    &DownloadOpts {
        asset_keys: Some(vec!["B04".into(), "B08".into()]),
        timeout_secs: 300,
    },
).await?;

// 3. Validate a STAC value against its JSON schema
validate(&serde_json::json!({ "type": "Feature", /* ... */ })).await?;

// 4. Snapshot to geoparquet for offline reuse
orbit_geo::stac::write("snapshot.parquet", &items)?;

// 5. Hand local paths to the orbit-geo kernel
let rds = RasterDatasetBuilder::from_files(&paths)?
    .block_size(BlockSize { rows: 2048, cols: 2048 })
    .build()?;
```

### `stac-duckdb` — version-skew workaround

`stac-duckdb 0.3.7` internally depends on `stac 0.16` while orbit-geo pins `stac 0.17` (to match `stac-client 0.3`). The two `stac::api::ItemCollection` types are distinct, so a direct binding doesn't type-check.

Workaround: the `stac-duckdb` feature flag shells out to the `rustac` CLI (`cargo install rustac`) for offline geoparquet queries. The wrapper returns parsed `Vec<stac::Item>` from the CLI's stdout JSON — same shape as direct queries would return. When `stac-duckdb` ships a `stac 0.17`-compatible release, this wrapper will switch to a direct binding without breaking the public API.

```rust
use orbit_geo::stac::StacGeoParquetReader;

let reader = StacGeoParquetReader::open("snapshot.parquet")?;
let items: Vec<stac::Item> = reader.search(&["--query", r#"{"eo:cloud_cover":{"lt":5}}"#])?;
```

### Workaround that works **today**

Use the production-grade `rustac` CLI from your orchestration code (or `rustac-py` from Python). Save items to `stac-geoparquet` on local disk. Point orbit-geo at the resulting files with `DataSource::Files`:

```bash
# 1. Use the real rustac CLI for STAC heavy-lifting
rustac search https://earth-search.aws.element84.com/v1 \
    --collections sentinel-2-l2a --bbox 13,52,14,53 \
    --datetime 2026-04-01/2026-04-30 \
    --max-items 100 \
    items.parquet

# 2. Download assets you need (any HTTP client, or rustac CLI helpers)
# 3. Pass the local paths to orbit-geo
```

```rust
let rds = RasterDatasetBuilder::from_files(&downloaded_paths)?
    .block_size(BlockSize { rows: 2048, cols: 2048 })
    .build()?;
rds.apply_reduction_with_mask(&mask, worker, Dimension::Layer, 8, &out, na)?;
```

The block-parallel reduction pattern works fine via this path. Only the inline STAC convenience layer is stubbed pending Phase-4 follow-up.

```rust
use orbit_geo::stac::{StacClient, download_items, DownloadOpts};
use orbit_geo::{DataSource, RasterDatasetBuilder, types::BlockSize};
use stac_api::Search;
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Search a remote STAC API — typed ItemCollection back.
    let client = StacClient::new("https://earth-search.aws.element84.com/v1")?;
    let items = client.search(
        Search::default()
            .collections(vec!["sentinel-2-l2a".into()])
            .bbox(vec![13.0, 52.0, 14.0, 53.0])
            .datetime("2026-04-01/2026-04-30")
            .query(json!({ "eo:cloud_cover": { "lt": 20.0 } }))
            .limit(50),
    ).await?;

    // 2. Download the assets we care about — cache by URL hash.
    let cache = PathBuf::from("./stac-cache");
    let paths = download_items(
        &items,
        &cache,
        &DownloadOpts {
            asset_keys: Some(vec!["B04".into(), "B08".into(), "SCL".into()]),
            ..Default::default()
        },
    ).await?;

    // 3. Same orbit-geo kernel.
    let rds = RasterDatasetBuilder::from_files(&paths)?
        .block_size(BlockSize { rows: 2048, cols: 2048 })
        .build()?;
    // ... rds.apply_reduction_with_mask(...)
    Ok(())
}
```

### Offline STAC queries via DuckDB (the "secret weapon")

For frameworks that periodically refresh a catalog: snapshot the API output to stac-geoparquet, then query locally. **No network on the hot path.**

```bash
# 1. Snapshot once (via the rustac CLI, or programmatically)
rustac search https://earth-search.aws.element84.com/v1 \
    --collections sentinel-2-l2a --bbox 13,52,14,53 \
    --datetime 2026-04-01/2026-04-30 \
    --max-items 500 \
    items.parquet
```

```rust
// 2. From orbit-geo (feature = "stac-duckdb")
use orbit_geo::stac::query_geoparquet;
let items = query_geoparquet(
    "items.parquet",
    Search::default().query(json!({ "eo:cloud_cover": { "lt": 5.0 } })),
)?;
```

DuckDB pushes predicates into Parquet — millions of items, sub-second queries.

### Microsoft Planetary Computer support

PC asset URLs need SAS-token signing. orbit-geo ships a one-call helper that fits cleanly into `DownloadOpts::url_rewriter`:

```rust
use orbit_geo::stac::sign_planetary_computer_url;
use std::sync::Arc;

let opts = DownloadOpts {
    url_rewriter: Some(Arc::new(|url| Box::pin(sign_planetary_computer_url(&url)))),
    ..Default::default()
};
```

### Build commands

```bash
cargo build -p orbit-geo --features stac                    # STAC API client + asset download
cargo build -p orbit-geo --features stac,stac-duckdb        # + offline GeoParquet queries
cargo build -p orbit-geo --features stac,openeo             # STAC + openEO together
cargo build -p orbit-geo --features stac,stac-duckdb,openeo,static-gdal  # the full kitchen
```

## Implementation status (Phase-4 MVP + STAC wiring + Phase-5 openEO adapter)

This crate is the **API + design scaffold**. The block reader, parallel writer, and STAC asset download are stubbed with `unimplemented!()` calls clearly marked `Phase-4 stub`. They will be filled in when Phase 4 of the orbit-rs roadmap starts — see [`../../../11-framework-design/00-blueprint.md`](../../../11-framework-design/00-blueprint.md) §"Phase 4 — Geo".

What is complete here:
- ✅ Full public-API surface (`RasterDataset`, `RasterDatasetBuilder`, three `apply_*` methods)
- ✅ Type system (`RasterShape`, `BlockSize`, `Dimension`, `RasterRegion`, `RasterDataBlock`, …)
- ✅ Builder with block partitioning logic
- ✅ Error type (thiserror)
- ✅ Module-level rustdoc explaining each piece
- ✅ All Cargo features compile: `static-gdal`, `stac`, `stac-duckdb`, `async-tiff`, `tracing`, `openeo`
- ✅ `src/stac.rs` (~312 lines): real rustac integration (CROSS_CHECK Pass 6)
- ✅ `src/openeo.rs` (~388 lines): minimal openEO client (7 endpoints) + `submit_and_download`
- ✅ `src/processing.rs` (~323 lines): real `read_block` via GDAL VSI; `apply`, `apply_reduction`, `apply_reduction_with_mask` all wire through the parallel writer (CROSS_CHECK Pass 7)
- ✅ `src/writer.rs` (~222 lines): `ParallelGeoTiffWriter` with `Mutex<Option<gdal::Dataset>>`, lazy open in update mode, LZW+TILED+BIGTIFF pre-creation, COG-pyramid `build_overviews`
- 🟡 `apply` / `apply_reduction` / `apply_reduction_with_mask` skeletons that wire up rayon + writer
- 🟡 `ParallelGeoTiffWriter` design sketch
- ✅ Block reader via GDAL — Phase-4 done
- ✅ Parallel GeoTIFF writer — Phase-4 done
- ✅ STAC cache helper (`download_items`) — Phase-4 done
- ❌ Live integration test against a real Sentinel-2 COG — Phase-4 follow-up
- ❌ Multi-band scene auto-mapping (currently `band = 1` default) — Phase-4 follow-up
- ❌ GDAL Dataset handle cache across blocks (re-opens per block today) — Phase-5 optimisation

## Versions (mid-2026)

```toml
gdal           = "0.19"
gdal-src       = "0.3"          # optional, for static-linked binary
async-tiff     = "0.3"          # optional, cloud-native async path
object_store   = "0.13"
ndarray        = "0.16"
rayon          = "1"

# Optional STAC integration
stac           = "0.17"
stac-client    = "0.4"
stac-api       = "0.8"
```

See [`13-geo-satellite/02-stac-sentinel2-2026.md`](../../../13-geo-satellite/02-stac-sentinel2-2026.md) for the deeper rationale on each choice.

## Differences vs upstream raster engine (intentional)

| upstream engine | orbit-geo | Why we differ |
|---|---|---|
| `anyhow::Error` everywhere | `thiserror::Error` typed | Libraries should return typed errors; lets callers match precisely |
| `lazy_static`, `log + env_logger` | `LazyLock` (std), `tracing` | Modern Rust 2024 stack |
| Methods take `fn(...)` pointers | Methods take `Fn(...)` closures | Allows closures that capture environment (e.g. shared model handle) |
| Edition 2021, resolver 2 | Edition 2024, resolver 3 | Modern defaults |
| LGPL-3.0-or-later | MIT OR Apache-2.0 | Permissive, no static-linking friction |
| GDAL-only writer | Trait-based `BlockWriter<V>` | Allows swapping in Zarr/COG/PMTiles writers later |
| `.expect()` / `.unwrap()` in hot paths | `Result<T>` throughout | Forces explicit error handling |
| Single resolver: GDAL VSI | Pluggable: GDAL VSI + async-tiff feature-flagged | Future-proofs the cloud-native path |

The **public API shape stays compatible** — a downstream user can write their worker once and run it against either engine.

## Roadmap to feature parity with the upstream raster engine

1. **Block reader** (`processing::read_block`) — Phase 4, ~1 week. Wraps `gdal::Dataset::open` + per-band `RasterBand::read_as::<R>` into the 4-D ndarray.
2. **Parallel writer** (`writer::ParallelGeoTiffWriter::write_block`) — Phase 4, ~3 days. Type-dispatch on `T: 'static` against `gdal::raster::Buffer<T>`.
3. **STAC download cache** (`source::cache::download_assets`) — Phase 4, ~3 days. Uses `object_store` + idempotent puts.
4. **`async-tiff` reader feature flag** — Phase 5, optional cloud-native path.
5. **Zonal stats / sampling / rasterization** — Phase 6, mirror upstream's `zonal_stats`, `sampling`, `rasterization` modules.
6. **Composition** (`stack`, `extend`) — Phase 6, mirror upstream's `composition` module.
7. **ML helpers** (`feature_extraction`, `classify`) — Phase 6, feature-gated behind `lgbm`/`candle`.

Once items 1–3 land, the crate runs the same `bench_ndvi_annual_full_tile_masked` workload as the upstream engine, with the same architecture and same expected speedup.

## How to run the equivalent benchmark

Today (using the upstream raster engine directly):
```bash
cd /Users/macbookpro/Rust/upstream-raster-engine
cargo run --release --example bench_ndvi_annual_full_tile_masked \
    --features=use_rss -- \
    --scene 56jns --start 2023-06-01 --end 2023-06-30 \
    --max-cloud 30 --threads 8 --block-size 2048 \
    --output /tmp/ndvi_upstream.tif
```

After orbit-geo Phase-4 completion:
```bash
cd /Users/macbookpro/Rust/mvp/orbit-etl
cargo run --release -p orbit-geo --example bench_ndvi_annual -- \
    --scene 56jns --start 2023-06-01 --end 2023-06-30 \
    --max-cloud 30 --threads 8 --block-size 2048 \
    --output /tmp/ndvi_orbit.tif
```

Both should produce numerically identical NDVI outputs and similar wall-clock times (within ~10% — the gap will be Rust-vs-Rust noise rather than ecosystem-level).

## Links

- Upstream raster engine (production reference) — see `NOTICE.md` for attribution.
- Comparison report and design rationale: see the `13-geo-satellite/` library docs.
- STAC + Sentinel-2 deep research: [`../../../13-geo-satellite/02-stac-sentinel2-2026.md`](../../../13-geo-satellite/02-stac-sentinel2-2026.md)
- orbit-rs framework blueprint: [`../../../11-framework-design/00-blueprint.md`](../../../11-framework-design/00-blueprint.md)

## License

MIT OR Apache-2.0 — same as the rest of orbit-rs. The upstream raster engine reference implementation is LGPL-3.0-or-later under separate licence (and is the recommended choice if you need production-tested code today).
