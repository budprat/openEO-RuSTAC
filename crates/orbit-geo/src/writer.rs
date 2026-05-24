//! Thread-safe writer for streaming block results into a single GeoTIFF.
//!
//! Each rayon worker calls [`ParallelGeoTiffWriter::write_block`] after its
//! per-block work returns. The writer holds one open `gdal::Dataset` behind
//! a `parking_lot::Mutex`; band writes are serialized by the lock.
//!
//! ## Lifecycle
//!
//! ```text
//! create(path, gt, epsg, rows, cols, bands, na_value)   // empty LZW-tiled BIGTIFF on disk
//!     │
//!     ▼
//! rayon par_iter over RasterRegions
//!     │
//!     │ each worker:
//!     │   ▼
//!     │   write_block(&data, window)   // takes mutex, writes bands at window
//!     │
//! (all workers complete)
//!     │
//!     ▼
//! drop(writer) → underlying Dataset closes & flushes
//! ```
//!
//! ## Design notes (clean-room, observed from upstream reference)
//!
//! - **Lazy open**: the `Dataset` field is `Mutex<Option<Dataset>>`. The
//!   create-step makes the file but does NOT keep it open; the first
//!   `write_block` call opens it in update mode. Avoids holding a file
//!   handle across the pre-create → first-write window.
//! - **Single mutex around the whole Dataset**: GDAL's tile cache is
//!   internally locked anyway; per-block contention is dominated by it.
//! - **`Buffer::new((cols, rows), Vec<T>)`**: GDAL's buffer order is
//!   (cols, rows), not (rows, cols). Easy to flip.
//! - **`band.write((col_off, row_off), (cols, rows), &mut buffer)`**: same
//!   col-first convention.
//! - We don't build overviews here. That's a separate post-pass
//!   ([`ParallelGeoTiffWriter::build_overviews`]) called once all writes
//!   are in.

use crate::{
    error::{Error, Result},
    types::{GeoTransform, ReadWindow, RasterType},
};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};

/// Trait abstracting over parallel-write strategies. Implemented by
/// [`ParallelGeoTiffWriter`] today; other backends (Zarr, COG-with-shards,
/// PMTiles) can be plugged in later behind feature flags.
pub trait BlockWriter<V: RasterType>: Send + Sync {
    /// Write `data` (shape `(layers, rows, cols)`) into the output at
    /// `window`. Workers may call concurrently; impls must be thread-safe.
    fn write_block(&self, data: &ndarray::Array3<V>, window: ReadWindow) -> Result<()>;

    /// Build optional overview pyramid. Called once after all writes.
    /// Default impl is a no-op.
    fn build_overviews(&self, _resampling: &str, _levels: &[i32]) -> Result<()> {
        Ok(())
    }
}

/// Parallel GeoTIFF writer.
///
/// Construct with [`Self::create`]; pass to rayon-parallel workers as `&Arc<Self>`;
/// they call [`BlockWriter::write_block`] to land their results.
pub struct ParallelGeoTiffWriter {
    /// Output file path.
    path: PathBuf,
    /// Total rows of the output (height).
    rows: usize,
    /// Total cols of the output (width).
    cols: usize,
    /// Number of bands written by workers.
    bands: usize,
    /// The GDAL dataset, opened lazily on first write.
    dataset: Mutex<Option<gdal::Dataset>>,
}

impl ParallelGeoTiffWriter {
    /// Pre-create the output GeoTIFF with LZW compression and 512×512 tiles.
    ///
    /// Sets geo-transform, spatial reference (EPSG), and a no-data value on
    /// every band. The created file is closed at the end of this call; the
    /// first `write_block` re-opens in update mode.
    pub fn create<V: RasterType>(
        path: &Path,
        geo_transform: &GeoTransform,
        epsg_code: u32,
        rows: usize,
        cols: usize,
        bands: usize,
        no_data: V,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let driver = gdal::DriverManager::get_driver_by_name("GTiff")
            .map_err(|e| Error::Other(format!("get_driver GTiff: {e}")))?;

        let options = gdal::raster::RasterCreationOptions::from_iter([
            "COMPRESS=LZW",
            "TILED=YES",
            "BLOCKXSIZE=512",
            "BLOCKYSIZE=512",
            "BIGTIFF=IF_SAFER",
        ]);

        let mut ds = driver
            .create_with_band_type_with_options::<V, _>(path, cols, rows, bands, &options)
            .map_err(|e| Error::Other(format!("create GTiff at {}: {e}", path.display())))?;

        ds.set_geo_transform(&geo_transform.0)
            .map_err(|e| Error::Other(format!("set_geo_transform: {e}")))?;

        if epsg_code > 0 {
            let sr = gdal::spatial_ref::SpatialRef::from_epsg(epsg_code)
                .map_err(|e| Error::Other(format!("EPSG {epsg_code}: {e}")))?;
            ds.set_spatial_ref(&sr)
                .map_err(|e| Error::Other(format!("set_spatial_ref: {e}")))?;
        }

        if let Some(nd) = no_data.to_f64() {
            for b in 1..=bands {
                let mut band = ds
                    .rasterband(b)
                    .map_err(|e| Error::Other(format!("rasterband {b}: {e}")))?;
                band.set_no_data_value(Some(nd))
                    .map_err(|e| Error::Other(format!("set_no_data_value: {e}")))?;
            }
        }

        // Drop the dataset here so it flushes; `write_block` will reopen lazily.
        drop(ds);

        Ok(Self {
            path: path.to_path_buf(),
            rows,
            cols,
            bands,
            dataset: Mutex::new(None),
        })
    }

    /// Output path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Output dimensions: `(rows, cols, bands)`.
    #[must_use]
    pub fn dims(&self) -> (usize, usize, usize) {
        (self.rows, self.cols, self.bands)
    }

    /// Open the dataset in update mode if not already open.
    fn ensure_open(&self, guard: &mut parking_lot::MutexGuard<'_, Option<gdal::Dataset>>) -> Result<()> {
        if guard.is_some() {
            return Ok(());
        }
        let opts = gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
            ..gdal::DatasetOptions::default()
        };
        let ds = gdal::Dataset::open_ex(&self.path, opts)
            .map_err(|e| Error::Other(format!("open_ex {}: {e}", self.path.display())))?;
        **guard = Some(ds);
        Ok(())
    }
}

impl<V: RasterType> BlockWriter<V> for ParallelGeoTiffWriter {
    fn write_block(&self, data: &ndarray::Array3<V>, window: ReadWindow) -> Result<()> {
        let mut guard = self.dataset.lock();
        self.ensure_open(&mut guard)?;
        let ds = guard.as_mut().ok_or_else(|| {
            Error::Other("dataset missing after ensure_open".into())
        })?;

        let dim = data.dim(); // (layers, rows, cols)
        let layers = dim.0;
        let block_rows = dim.1;
        let block_cols = dim.2;

        for band_idx in 0..layers {
            let mut band = ds
                .rasterband(band_idx + 1)
                .map_err(|e| Error::Other(format!("rasterband {}: {e}", band_idx + 1)))?;
            // Copy this band slice into a contiguous Vec<V> for GDAL's Buffer.
            let band_view = data.index_axis(ndarray::Axis(0), band_idx);
            let data_vec: Vec<V> = band_view.iter().copied().collect();
            let mut buffer = gdal::raster::Buffer::new((block_cols, block_rows), data_vec);
            band.write(
                (window.offset.cols, window.offset.rows),
                (block_cols, block_rows),
                &mut buffer,
            )
            .map_err(|e| {
                Error::Other(format!(
                    "band {} write at ({},{}) size ({},{}): {e}",
                    band_idx + 1,
                    window.offset.cols,
                    window.offset.rows,
                    block_cols,
                    block_rows,
                ))
            })?;
        }
        Ok(())
    }

    fn build_overviews(&self, resampling: &str, levels: &[i32]) -> Result<()> {
        let mut guard = self.dataset.lock();
        self.ensure_open(&mut guard)?;
        let ds = guard.as_mut().ok_or_else(|| {
            Error::Other("dataset missing after ensure_open".into())
        })?;
        ds.build_overviews(resampling, levels, &[])
            .map_err(|e| Error::Other(format!("build_overviews: {e}")))?;
        Ok(())
    }
}

impl ParallelGeoTiffWriter {
    /// **T1.5** — Direct-write helper. Writes an `Array3` of shape
    /// `(bands, rows, cols)` to the output at the given pixel `offset`
    /// `(row_off, col_off)`. Convenience wrapper over [`BlockWriter::write_block`]
    /// that constructs the `ReadWindow` from the array's dimensions.
    ///
    pub fn write_window3<V: RasterType>(
        &self,
        data: ndarray::ArrayView3<'_, V>,
        row_off: isize,
        col_off: isize,
    ) -> Result<()> {
        let dim = data.dim(); // (bands, rows, cols)
        let window = ReadWindow {
            offset: crate::types::Offset { rows: row_off, cols: col_off },
            size: crate::types::Size {
                rows: dim.1 as isize,
                cols: dim.2 as isize,
            },
        };
        // Delegate to the existing BlockWriter::write_block — same path
        // band-by-band write logic. write_block takes &Array3 not view,
        // so we materialize.
        let owned = data.to_owned();
        <Self as BlockWriter<V>>::write_block(self, &owned, window)
    }
}
