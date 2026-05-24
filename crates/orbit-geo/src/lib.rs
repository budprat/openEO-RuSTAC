//! # orbit-geo — Block-Parallel Raster Processing Kernel
//!
//! Public surface for satellite imagery and Earth-observation pipelines in
//! the `orbit-rs` ecosystem. The crate provides a small, focused API for:
//!
//! 1. Building a [`RasterDataset`] from local files, a STAC query result, or
//!    other [`DataSource`] backends.
//! 2. Iterating that dataset as fixed-size blocks ([`RasterRegion`]).
//! 3. Applying a worker function to every block in parallel via [`rayon`],
//!    writing results directly to a single output GeoTIFF (no intermediate
//!    files, no serialize/deserialize round-trips).
//!
//! ## Empirical motivation
//!
//! The pattern is modelled on an **upstream raster engine** (see `NOTICE.md`
//! for attribution), which measured a significant speedup over Python
//! ODC/STAC for cached NDVI-mean across 9 Sentinel-2 timesteps. See the
//! comparison report and the benchmark transcript in the `13-geo-satellite/`
//! library docs.
//!
//! ## Quick example
//!
//! ```rust,ignore
//! use orbit_geo::{
//!     RasterDataset, RasterDatasetBuilder, DataSource,
//!     types::{BlockSize, Dimension, ImageResolution},
//!     RasterDataBlock,
//! };
//! use ndarray::{Array3, Axis, s};
//!
//! // 1. Acquire scene files (downloaded from STAC, or already on disk).
//! let scene_files = vec![/* PathBuf, … */];
//!
//! // 2. Build the dataset.
//! let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&scene_files)?
//!     .resolution(ImageResolution { x: 10.0, y: -10.0 })
//!     .block_size(BlockSize { rows: 2048, cols: 2048 })
//!     .build()?;
//!
//! // 3. Define the worker: NDVI mean over time with FMask cloud mask.
//! fn ndvi_mean(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> Array3<i16> {
//!     // ... compute NDVI per timestep, mask clouds via fmask layer, mean.
//!     Array3::zeros((1, rdb.rows(), rdb.cols()))
//! }
//!
//! // 4. Apply with parallel block writes.
//! rds.apply_reduction::<i16>(
//!     ndvi_mean,
//!     Dimension::Layer,
//!     8,                         // n_cpus
//!     &"output.tif".into(),
//!     i16::MIN,                  // no-data value
//! )?;
//! ```
//!
//! ## Module map
//!
//! ### Core kernel
//! - [`types`] — geometric & shape primitives (`BlockSize`, `RasterShape`, `Dimension`, …)
//! - [`block`] — per-block data carrier ([`RasterDataBlock`]) and metadata ([`RasterRegion`])
//! - [`dataset`] — [`RasterDataset`] and [`RasterDatasetBuilder`]
//! - [`source`] — input adapters ([`DataSource`])
//! - [`processing`] — `apply`, `apply_with_mask`, `apply_cog`, `apply_reduction`,
//!   `apply_reduction_with_mask`, `apply_reduction_row_pixel_to_writer`, `read_block_layer_idx`
//! - [`writer`] — parallel GeoTIFF writer + `write_window3` helper
//! - [`error`] — typed error and `Result` alias
//!
//! ### GDAL CLI subprocess helpers
//! - [`gdal_utils`] — `mosaic`, `convert_to_cog`, `warp`, re-exports `download_via_gdal_translate`
//!
//! ### Provider plumbing
//! - [`providers`] — `Provider` constants, `vsi_rewrite`, PC SAS signing,
//!   `configure_anonymous_s3`, `download_via_gdal_translate`
//!
//! ### Declarative imagery DSL (Tier 2)
//! - [`dsl`] — `Collection`, `Intersects`, `Cmp`, `ImageQueryBuilder`,
//!   `canonical_bands`, `cloudcover_filter`, `ImageQuery::get_remote`
//!
//! ### Auxiliary modules (Tier 3)
//! - [`composition`] — `extend`, `stack`
//! - [`sampling`] — `sample`, `sample_at_point`, `geo_to_pixel`
//! - [`zonal_stats`] — `zonal_histogram`
//! - [`rasterization`] — `rasterize` (vector → raster via `gdal_rasterize`)
//! - `async_io` — async-tiff reader path (feature `async-tiff`)
//! - `ml` — pure-Rust binary classifier (feature `use_ml`)
//! - `cloud_mask` — rule-based cloud detection (feature `cloud_mask`)
//!
//! ### Optional integrations
//! - `stac` — full rustac stack (feature `stac`)
//! - `openeo` — minimal openEO REST client (feature `openeo`)
//!
//! ## Cleanroom note
//!
//! The public-API *shape* of this crate matches the upstream raster crate
//! (LGPL-3.0; see `NOTICE.md` for attribution). The implementation here is
//! independently written and licensed MIT OR Apache-2.0. See
//! [`README.md`](https://docs.rs/orbit-geo) for the design rationale and
//! credit to the upstream maintainers for the original block-parallel
//! raster reduction pattern.

#![doc(html_root_url = "https://docs.rs/orbit-geo")]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]
#![allow(clippy::module_name_repetitions)]

pub mod block;
pub mod gdal_utils;
pub mod dsl;
pub mod composition;
pub mod sampling;
pub mod rasterization;
pub mod zonal_stats;
pub mod cache;
pub mod array_ops;
pub mod builder;
pub mod dataset;
pub mod error;
pub mod processing;
pub mod providers;
pub mod source;
pub mod types;
pub mod writer;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(feature = "stac")]
pub mod stac;

#[cfg(feature = "stac")]
pub mod stac_helpers;

#[cfg(feature = "openeo")]
pub mod openeo;

#[cfg(feature = "async-tiff")]
pub mod async_io;

#[cfg(feature = "async-tiff")]
pub mod async_download;

#[cfg(feature = "use_ml")]
pub mod ml;

#[cfg(feature = "cloud_mask")]
pub mod cloud_mask;

pub mod products;

pub use block::{RasterDataBlock, RasterRegion};
pub use products::{known_products, BandAliases, MaskKind, Product};
pub use builder::{BandPathResolver, RasterDatasetBuilder};
pub use dataset::{layer_mappings_for_scene, layer_mappings_for_scenes, LayerMapping, RasterDataset};
pub use error::{Error, Result};
pub use providers::{
    build_gdal_translate_argv, configure_anonymous_s3, download_via_gdal_translate,
    parse_signed_response, planetary_computer_sign_endpoint, vsi_rewrite, CropWindow, Provider,
};
#[cfg(feature = "openeo")]
pub use providers::sign_planetary_computer_url;
pub use source::{DataSource, DataSourceBuilder};
pub use types::{
    BlockSize, Dimension, GeoTransform, ImageResolution, Offset, RasterShape, RasterType,
    ReadWindow, Size,
};
