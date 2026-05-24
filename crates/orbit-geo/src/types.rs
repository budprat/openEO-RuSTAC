//! Backwards-compatible re-export shim.
//!
//! Pure data types have moved to the [`eo_core`] crate. The GDAL-bound
//! [`RasterType`] trait has moved to [`eo_io`]. This module exists only so
//! existing `use crate::types::*` imports keep compiling — new code should
//! depend on `eo-core` / `eo-io` directly.
//!
//! Will be removed in a future minor version once downstream call sites
//! migrate. See `docs/plans/01-maturity-and-parity.md`.

// Pure data types — eo-core.
pub use eo_core::{
    BlockSize, Dimension, GeoTransform, ImageResolution, Offset, OutputConfig, OutputFormat,
    Overlap, RasterShape, ReadWindow, Size,
};

// GDAL-bound trait — eo-io.
pub use eo_io::RasterType;
