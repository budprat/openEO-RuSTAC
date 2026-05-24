//! Per-block data and metadata carriers.
//!
//! Two distinct types live here:
//!
//! - [`RasterRegion`] — *metadata* about a block (where it lives in the
//!   parent raster, its read window, overlap settings). This is what the
//!   dataset stores in a `Vec` to describe its block partitioning.
//!
//! - [`RasterDataBlock<T>`] — *actual data* for a block. This is what the
//!   parallel processing code constructs by reading the underlying file(s)
//!   for one block, then passes by reference to the worker function.

use crate::types::{GeoTransform, Offset, Overlap, RasterShape, RasterType, ReadWindow, Size};
use ndarray::Array4;
use serde::{Deserialize, Serialize};

/// Lightweight metadata about a single block within a [`crate::RasterDataset`].
///
/// `RasterRegion` is cheap to clone — it carries only `Copy` fields. The
/// per-block data array is materialized lazily by the processing code, not
/// stored here.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RasterRegion {
    /// Linear index of this block in the dataset's `blocks` vector.
    pub block_index: usize,

    /// Where to read from the source raster.
    pub read_window: ReadWindow,

    /// Overlap pixels on each edge (for context-aware workers like filters).
    pub overlap: Overlap,

    /// Geo-transform of *this block* (origin offset by `read_window`).
    pub geo_transform: GeoTransform,

    /// EPSG code shared with the parent dataset (cached here for convenience).
    pub epsg_code: u32,
}

impl RasterRegion {
    /// Returns the inner pixel count after stripping overlap from each side.
    #[must_use]
    pub fn inner_size(&self) -> Size {
        let inner_rows =
            self.read_window.size.rows - (self.overlap.top + self.overlap.bottom) as isize;
        let inner_cols =
            self.read_window.size.cols - (self.overlap.left + self.overlap.right) as isize;
        Size {
            rows: inner_rows,
            cols: inner_cols,
        }
    }

    /// Where this block's *inner* (non-overlap) data should be written into
    /// the output raster.
    #[must_use]
    pub fn write_window(&self) -> ReadWindow {
        ReadWindow {
            offset: Offset {
                rows: self.read_window.offset.rows + self.overlap.top as isize,
                cols: self.read_window.offset.cols + self.overlap.left as isize,
            },
            size: self.inner_size(),
        }
    }
}

/// Block data as it is handed to a worker function.
///
/// The four dimensions of `data` are `(times, layers, rows, cols)` — see
/// [`crate::types::RasterShape`]. For a single-scene dataset, `times == 1`.
///
/// The worker is free to interpret layers however it likes — e.g. with
/// Sentinel-2 it might pick `layers[0] = red`, `layers[1] = nir`,
/// `layers[2] = fmask` if the dataset was built with `bands: &["nbart_red",
/// "nbart_nir_1", "oa_fmask"]`.
#[derive(Debug)]
pub struct RasterDataBlock<T: RasterType> {
    /// 4-D data array `(times, layers, rows, cols)`.
    pub data: Array4<T>,
    /// Block-level shape (derived from `data.dim()`).
    pub shape: RasterShape,
    /// No-data sentinel for `T`.
    pub no_data: T,
    /// Reference to the region metadata.
    pub region: RasterRegion,
}

impl<T: RasterType> RasterDataBlock<T> {
    /// Number of time steps in this block (axis 0 of `data`).
    #[must_use]
    pub fn times(&self) -> usize {
        self.shape.times
    }

    /// Number of layers / bands.
    #[must_use]
    pub fn layers(&self) -> usize {
        self.shape.layers
    }

    /// Number of rows in this block.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.shape.rows
    }

    /// Number of columns in this block.
    #[must_use]
    pub fn cols(&self) -> usize {
        self.shape.cols
    }
}
