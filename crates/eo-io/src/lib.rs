//! **eo-io** — I/O primitives for Earth-observation pipelines.
//!
//! Everything that touches the disk or libgdal lives here. Pure data types
//! (block size, geo-transform, …) live one level down in [`eo_core`].
//!
//! # Layout
//!
//! - [`atomic`] — atomic-replace + staging-path helpers. Pure filesystem,
//!   no GDAL dependency. Always available.
//! - [`raster_type`] — the `RasterType` trait (feature-gated `gdal`).
//!
//! Subsequent weeks add: VSI path helpers, COG reader (`async-tiff`),
//! parallel writer, GDAL config tuning, asset signing.
//!
//! See `docs/plans/01-maturity-and-parity.md`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod atomic;
pub mod vsi;

#[cfg(feature = "gdal")]
pub mod raster_type;

#[cfg(feature = "gdal")]
pub use raster_type::RasterType;
