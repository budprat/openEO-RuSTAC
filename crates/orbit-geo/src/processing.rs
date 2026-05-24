//! Parallel block-processing entry points on [`RasterDataset`].
//!
//! The three public methods are:
//!
//! - [`RasterDataset::apply`] — element-preserving map; output shape equals input
//! - [`RasterDataset::apply_reduction`] — collapse the `Dimension::Layer` or
//!   `Dimension::Time` axis, producing a single output band per block
//! - [`RasterDataset::apply_reduction_with_mask`] — same as `apply_reduction`,
//!   but the worker also receives an aligned block from a *second* dataset
//!   (the mask)
//!
//! ## How the parallelism works
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  RasterDataset                                              │
//! │   blocks: [R0, R1, R2, …, Rn]   ← block metadata only       │
//! └──────────────────────┬──────────────────────────────────────┘
//!                        │
//!                        ▼   rayon::par_iter
//!     ┌──────────────────┴──────────────────┐
//!     ▼                                     ▼
//! [worker thread A]                  [worker thread B]
//!   read block R_i (GDAL VSI)         read block R_j
//!   worker(&data, &mask, dim)         worker(&data, &mask, dim)
//!   trim overlap                      trim overlap
//!   write_block(&writer, …) ←Mutex→   write_block(&writer, …)
//! ```
//!
//! All threads share **one** [`crate::writer::ParallelGeoTiffWriter`].
//! Contention on the writer mutex is measurably modest at 2048×2048 blocks
//! and is dominated by GDAL's internal tile lock anyway.

use crate::{
    block::{RasterDataBlock, RasterRegion},
    dataset::RasterDataset,
    error::{Error, Result},
    types::{Dimension, RasterShape, RasterType},
    writer::{BlockWriter, ParallelGeoTiffWriter},
};
use ndarray::Array3;
use rayon::prelude::*;
use std::path::Path;
use std::sync::Arc;

impl<R: RasterType> RasterDataset<R> {
    /// Apply `worker` to each block; write results to `out`.
    ///
    /// `worker` returns `Array3<V>` shaped `(layers_out, rows, cols)` where
    /// `layers_out` may differ from the input layer count.
    pub fn apply<V, F>(&self, worker: F, n_threads: usize, out: &Path) -> Result<()>
    where
        V: RasterType,
        F: Fn(&RasterDataBlock<R>) -> Array3<V> + Send + Sync,
    {
        let n_bands = probe_output_layers(self, &worker)?;
        let writer = Arc::new(ParallelGeoTiffWriter::create::<V>(
            out,
            &self.metadata.geo_transform,
            self.metadata.epsg_code,
            self.metadata.shape.rows,
            self.metadata.shape.cols,
            n_bands,
            V::zero(),
        )?);

        run_blocks(self, n_threads, &writer, |block| Ok(worker(block)))
    }

    /// Apply `worker` collapsing one axis; write a single-band output per block.
    ///
    /// This is the **canonical block-parallel reduction entry point**. As of T0.1 (Tier 0),
    /// the output GeoTIFF gets a `[2, 4, 8, 16]` overview pyramid built with
    /// AVERAGE resampling automatically after the last block lands.
    pub fn apply_reduction<V, F>(
        &self,
        worker: F,
        dim: Dimension,
        n_threads: usize,
        out: &Path,
        na_value: V,
    ) -> Result<()>
    where
        V: RasterType,
        F: Fn(&RasterDataBlock<R>, Dimension) -> Array3<V> + Send + Sync,
    {
        let writer = Arc::new(ParallelGeoTiffWriter::create::<V>(
            out,
            &self.metadata.geo_transform,
            self.metadata.epsg_code,
            self.metadata.shape.rows,
            self.metadata.shape.cols,
            /* bands = */ 1,
            na_value,
        )?);

        // T0.1: delegate to the writer-generic variant which handles
        // build_overviews. Keeps both APIs in sync.
        self.apply_reduction_to_writer(writer, worker, dim, n_threads)
    }

    /// Like [`apply_reduction`](Self::apply_reduction) but `worker` also receives
    /// an aligned block from `mask`. Both datasets must share extent + block
    /// partitioning — typically constructed via the same builder with the
    /// same `block_size`.
    ///
    /// **This is the canonical block-parallel reduction entry point**:
    /// rayon par_iter → read block (data + mask) → worker → write result
    /// directly into the output GeoTIFF (no intermediate files).
    /// As of T0.1, overview pyramid `[2, 4, 8, 16]` is built automatically.
    pub fn apply_reduction_with_mask<U, V, F>(
        &self,
        mask: &RasterDataset<U>,
        worker: F,
        dim: Dimension,
        n_threads: usize,
        out: &Path,
        na_value: V,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>, Dimension) -> Array3<V> + Send + Sync,
    {
        let writer = Arc::new(ParallelGeoTiffWriter::create::<V>(
            out,
            &self.metadata.geo_transform,
            self.metadata.epsg_code,
            self.metadata.shape.rows,
            self.metadata.shape.cols,
            /* bands = */ 1,
            na_value,
        )?);

        // T0.1: delegate to the writer-generic variant which handles
        // alignment validation + parallel iteration + build_overviews.
        self.apply_reduction_with_mask_to_writer(mask, writer, worker, dim, n_threads)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Tier 0 / T0.1 — `_to_writer` variants accepting any `BlockWriter<V>`.
    //
    // These are the *primary* TDD targets. The existing `apply_reduction*`
    // path-based methods will delegate to these once T0.1 GREEN lands the
    // `build_overviews` call below.
    // ─────────────────────────────────────────────────────────────────────────

    /// Like [`apply_reduction`](Self::apply_reduction) but writes through any
    /// `BlockWriter<V>` impl. Useful for testing, alternative output formats,
    /// and in-memory pipelines.
    ///
    /// **T0.1 stub (RED phase)**: does NOT yet call `build_overviews` —
    /// see the archived parity plan §3 T0.1.
    pub fn apply_reduction_to_writer<V, W, F>(
        &self,
        writer: Arc<W>,
        worker: F,
        dim: Dimension,
        n_threads: usize,
    ) -> Result<()>
    where
        V: RasterType,
        W: BlockWriter<V> + Send + Sync + 'static,
        F: Fn(&RasterDataBlock<R>, Dimension) -> Array3<V> + Send + Sync,
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads.max(1))
            .build()
            .map_err(|e| Error::Other(format!("rayon pool: {e}")))?;

        pool.install(|| -> Result<()> {
            self.blocks.par_iter().try_for_each(|region| -> Result<()> {
                let block = read_block::<R>(self, *region)?;
                let result = worker(&block, dim);
                let trimmed = trim_overlap(&result, region);
                let w: &dyn BlockWriter<V> = writer.as_ref();
                w.write_block(&trimmed, region.write_window())?;
                Ok(())
            })
        })?;

        // T0.1 GREEN: build pyramid overviews after all blocks written.
        // Default levels [2, 4, 8, 16] match upstream convention; AVERAGE
        // resampling is appropriate for continuous data (mean reductions).
        let w: &dyn BlockWriter<V> = writer.as_ref();
        w.build_overviews("AVERAGE", &[2, 4, 8, 16])?;

        Ok(())
    }

    /// Like [`apply_reduction_with_mask`](Self::apply_reduction_with_mask) but
    /// writes through any `BlockWriter<V>` impl.
    ///
    /// **T0.1 stub (RED phase)**: does NOT yet call `build_overviews`.
    pub fn apply_reduction_with_mask_to_writer<U, V, W, F>(
        &self,
        mask: &RasterDataset<U>,
        writer: Arc<W>,
        worker: F,
        dim: Dimension,
        n_threads: usize,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        W: BlockWriter<V> + Send + Sync + 'static,
        F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>, Dimension) -> Array3<V> + Send + Sync,
    {
        if self.num_blocks() != mask.num_blocks() {
            return Err(Error::InconsistentMetadata(format!(
                "data has {} blocks, mask has {}",
                self.num_blocks(),
                mask.num_blocks()
            )));
        }

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads.max(1))
            .build()
            .map_err(|e| Error::Other(format!("rayon pool: {e}")))?;

        pool.install(|| -> Result<()> {
            self.blocks
                .par_iter()
                .zip(mask.blocks.par_iter())
                .try_for_each(|(region, mask_region)| -> Result<()> {
                    let data_block = read_block::<R>(self, *region)?;
                    let mask_block = read_block::<U>(mask, *mask_region)?;
                    let result = worker(&data_block, &mask_block, dim);
                    let trimmed = trim_overlap(&result, region);
                    let w: &dyn BlockWriter<V> = writer.as_ref();
                    w.write_block(&trimmed, region.write_window())?;
                    Ok(())
                })
        })?;

        // T0.1 GREEN: build pyramid overviews after all blocks written.
        // Default levels [2, 4, 8, 16] match upstream convention; AVERAGE
        // resampling is appropriate for continuous data (mean reductions).
        let w: &dyn BlockWriter<V> = writer.as_ref();
        w.build_overviews("AVERAGE", &[2, 4, 8, 16])?;

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────
    // Tier 1 / T1.1 — apply_with_mask (worker gets data + mask blocks,
    // preserves layer count — no reduction).
    // ─────────────────────────────────────────────────────────────────────

    /// Like [`apply`](Self::apply) but `worker` also receives an aligned mask
    /// block. Both datasets must share extent + block partitioning.
    pub fn apply_with_mask_to_writer<U, V, W, F>(
        &self,
        mask: &RasterDataset<U>,
        writer: Arc<W>,
        worker: F,
        n_threads: usize,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        W: BlockWriter<V> + Send + Sync + 'static,
        F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>) -> Array3<V> + Send + Sync,
    {
        if self.num_blocks() != mask.num_blocks() {
            return Err(Error::InconsistentMetadata(format!(
                "data has {} blocks, mask has {}",
                self.num_blocks(),
                mask.num_blocks()
            )));
        }

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads.max(1))
            .build()
            .map_err(|e| Error::Other(format!("rayon pool: {e}")))?;

        pool.install(|| -> Result<()> {
            self.blocks
                .par_iter()
                .zip(mask.blocks.par_iter())
                .try_for_each(|(region, mask_region)| -> Result<()> {
                    let data_block = read_block::<R>(self, *region)?;
                    let mask_block = read_block::<U>(mask, *mask_region)?;
                    let result = worker(&data_block, &mask_block);
                    let trimmed = trim_overlap(&result, region);
                    let w: &dyn BlockWriter<V> = writer.as_ref();
                    w.write_block(&trimmed, region.write_window())?;
                    Ok(())
                })
        })?;

        // T1.1: also build overviews (consistent with reduction variants).
        let w: &dyn BlockWriter<V> = writer.as_ref();
        w.build_overviews("AVERAGE", &[2, 4, 8, 16])?;

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────
    // Tier 1 / T1.3 — apply_reduction_row_pixel: worker processes ONE row
    // at a time per block, useful when full block doesn't fit in RAM.
    // ─────────────────────────────────────────────────────────────────────

    /// Apply a per-row reducing worker. For each block, the rayon worker
    /// reads the block then dispatches the worker once per row, collecting
    /// 1-D row outputs into a 2-D block result.
    ///
    /// Worker signature: `Fn(ArrayView3<R>) -> Array1<V>` where the input
    /// is one row of shape `(times, layers, cols)`. Output `Array1<V>` of
    /// length `cols`.
    ///
    pub fn apply_reduction_row_pixel_to_writer<V, W, F>(
        &self,
        writer: Arc<W>,
        worker: F,
        n_threads: usize,
    ) -> Result<()>
    where
        V: RasterType,
        W: BlockWriter<V> + Send + Sync + 'static,
        F: Fn(ndarray::ArrayView3<R>) -> ndarray::Array1<V> + Send + Sync,
    {
        use ndarray::{s, Array3, Axis};

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads.max(1))
            .build()
            .map_err(|e| Error::Other(format!("rayon pool: {e}")))?;

        pool.install(|| -> Result<()> {
            self.blocks.par_iter().try_for_each(|region| -> Result<()> {
                let block = read_block::<R>(self, *region)?;
                let (_t, _l, rows, cols) = block.data.dim();
                let mut out: Array3<V> = Array3::from_elem((1, rows, cols), V::zero());
                for r in 0..rows {
                    let row_view = block.data.slice(s![.., .., r, ..]);
                    let row_out = worker(row_view);
                    debug_assert_eq!(row_out.len(), cols, "worker must return Array1 of length cols");
                    out.slice_mut(s![0, r, ..]).assign(&row_out);
                }
                let trimmed = trim_overlap(&out, region);
                let w: &dyn BlockWriter<V> = writer.as_ref();
                w.write_block(&trimmed, region.write_window())?;
                Ok(())
            })
        })?;

        let w: &dyn BlockWriter<V> = writer.as_ref();
        w.build_overviews("AVERAGE", &[2, 4, 8, 16])?;
        let _ = Axis(0); // silence unused if compiled
        Ok(())
    }

    // T1.3 follow-up — apply_reduction_row_pixel_with_mask_to_writer.
    /// Like [`apply_reduction_row_pixel_to_writer`](Self::apply_reduction_row_pixel_to_writer)
    /// but worker also receives the mask block (per-row view).
    pub fn apply_reduction_row_pixel_with_mask_to_writer<U, V, W, F>(
        &self,
        mask: &RasterDataset<U>,
        writer: Arc<W>,
        worker: F,
        n_threads: usize,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        W: BlockWriter<V> + Send + Sync + 'static,
        F: Fn(ndarray::ArrayView3<R>, ndarray::ArrayView3<U>) -> ndarray::Array1<V> + Send + Sync,
    {
        use ndarray::{s, Array3};
        if self.num_blocks() != mask.num_blocks() {
            return Err(Error::InconsistentMetadata(format!(
                "data has {} blocks, mask has {}",
                self.num_blocks(),
                mask.num_blocks()
            )));
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads.max(1))
            .build()
            .map_err(|e| Error::Other(format!("rayon pool: {e}")))?;

        pool.install(|| -> Result<()> {
            self.blocks
                .par_iter()
                .zip(mask.blocks.par_iter())
                .try_for_each(|(region, mask_region)| -> Result<()> {
                    let data = read_block::<R>(self, *region)?;
                    let mask_block = read_block::<U>(mask, *mask_region)?;
                    let (_t, _l, rows, cols) = data.data.dim();
                    let mut out: Array3<V> = Array3::from_elem((1, rows, cols), V::zero());
                    for r in 0..rows {
                        let row_d = data.data.slice(s![.., .., r, ..]);
                        let row_m = mask_block.data.slice(s![.., .., r, ..]);
                        let row_out = worker(row_d, row_m);
                        out.slice_mut(s![0, r, ..]).assign(&row_out);
                    }
                    let trimmed = trim_overlap(&out, region);
                    let w: &dyn BlockWriter<V> = writer.as_ref();
                    w.write_block(&trimmed, region.write_window())?;
                    Ok(())
                })
        })?;
        let w: &dyn BlockWriter<V> = writer.as_ref();
        w.build_overviews("AVERAGE", &[2, 4, 8, 16])?;
        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────
    // Tier 1 / T1.2 — COG output variants. Run the same kernel as the
    // non-cog method, write to a tempfile, then post-process via
    // gdal_translate -of COG.
    // ─────────────────────────────────────────────────────────────────────

    /// COG-output variant of [`apply`](Self::apply). Runs the standard kernel
    /// to a temporary GeoTIFF, then post-processes via `gdal_translate -of COG`.
    pub fn apply_cog<V, F>(
        &self,
        worker: F,
        n_threads: usize,
        out: &Path,
    ) -> Result<()>
    where
        V: RasterType,
        F: Fn(&RasterDataBlock<R>) -> Array3<V> + Send + Sync,
    {
        let tmp = tempfile::Builder::new()
            .suffix(".tif")
            .tempfile()
            .map_err(|e| Error::Other(format!("tempfile create: {e}")))?;
        let tmp_path = tmp.into_temp_path();
        std::fs::remove_file(&tmp_path).ok();

        self.apply::<V, _>(worker, n_threads, &tmp_path)?;
        crate::gdal_utils::convert_to_cog(&tmp_path, out)?;
        Ok(())
    }

    /// COG-output variant of [`apply_with_mask`](Self::apply_with_mask).
    pub fn apply_with_mask_cog<U, V, F>(
        &self,
        mask: &RasterDataset<U>,
        worker: F,
        n_threads: usize,
        out: &Path,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>) -> Array3<V> + Send + Sync,
    {
        let tmp = tempfile::Builder::new()
            .suffix(".tif")
            .tempfile()
            .map_err(|e| Error::Other(format!("tempfile create: {e}")))?;
        let tmp_path = tmp.into_temp_path();
        std::fs::remove_file(&tmp_path).ok();

        self.apply_with_mask::<U, V, _>(mask, worker, n_threads, &tmp_path)?;
        crate::gdal_utils::convert_to_cog(&tmp_path, out)?;
        Ok(())
    }

    /// COG-output variant of [`apply_reduction_with_mask`](Self::apply_reduction_with_mask).
    pub fn apply_reduction_with_mask_cog<U, V, F>(
        &self,
        mask: &RasterDataset<U>,
        worker: F,
        dim: Dimension,
        n_threads: usize,
        out: &Path,
        na_value: V,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>, Dimension) -> Array3<V> + Send + Sync,
    {
        let tmp = tempfile::Builder::new()
            .suffix(".tif")
            .tempfile()
            .map_err(|e| Error::Other(format!("tempfile create: {e}")))?;
        let tmp_path = tmp.into_temp_path();
        std::fs::remove_file(&tmp_path).ok();

        self.apply_reduction_with_mask::<U, V, _>(mask, worker, dim, n_threads, &tmp_path, na_value)?;
        crate::gdal_utils::convert_to_cog(&tmp_path, out)?;
        Ok(())
    }

    /// Path-based wrapper around [`apply_with_mask_to_writer`].
    ///
    /// **T1.1 stub (RED phase)** — not yet implemented; tests against
    /// `_to_writer` are the primary TDD target.
    pub fn apply_with_mask<U, V, F>(
        &self,
        mask: &RasterDataset<U>,
        worker: F,
        n_threads: usize,
        out: &Path,
    ) -> Result<()>
    where
        U: RasterType,
        V: RasterType,
        F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>) -> Array3<V> + Send + Sync,
    {
        // Probe band count, then delegate.
        let n_bands = probe_output_layers_with_mask(self, mask, &worker)?;
        let writer = Arc::new(ParallelGeoTiffWriter::create::<V>(
            out,
            &self.metadata.geo_transform,
            self.metadata.epsg_code,
            self.metadata.shape.rows,
            self.metadata.shape.cols,
            n_bands,
            V::zero(),
        )?);
        self.apply_with_mask_to_writer(mask, writer, worker, n_threads)
    }
}

impl<R: RasterType> RasterDataset<R> {
    // ─────────────────────────────────────────────────────────────────────
    // Tier 1 / T1.4 — read_block_layer_idx (read just one layer's worth).
    // ─────────────────────────────────────────────────────────────────────

    /// Read a single block, but only include the data from `layer_idx`.
    ///
    /// Output shape: `(times, 1, rows, cols)` regardless of the dataset's
    /// total layer count. Useful for selective reads when only one band is
    /// needed (e.g. computing NDVI from B04+B08 in a 13-band scene).
    ///
    pub fn read_block_layer_idx(
        &self,
        block_id: usize,
        layer_idx: usize,
    ) -> Result<RasterDataBlock<R>> {
        use crate::types::RasterShape;
        use ndarray::{s, Array4};

        if layer_idx >= self.metadata.shape.layers {
            return Err(Error::InconsistentMetadata(format!(
                "layer_idx={layer_idx} out of range (dataset has {} layers)",
                self.metadata.shape.layers
            )));
        }
        let region = self
            .blocks
            .get(block_id)
            .ok_or_else(|| Error::Other(format!("block_id {block_id} out of range")))?;
        let rows = region.read_window.size.rows as usize;
        let cols = region.read_window.size.cols as usize;
        let shape = RasterShape {
            times: self.metadata.shape.times,
            layers: 1, // collapsed to the single requested layer
            rows,
            cols,
        };
        let mut data: Array4<R> = Array4::zeros((shape.times, 1, shape.rows, shape.cols));
        let window_offset = (region.read_window.offset.cols, region.read_window.offset.rows);
        let window_size = (cols, rows);

        for mapping in self.layer_mappings().iter().filter(|m| m.layer_pos == layer_idx) {
            let arr_2d = read_window_cached::<R>(
                &mapping.source,
                mapping.band,
                window_offset,
                window_size,
            )?;
            if mapping.time_pos >= shape.times {
                return Err(Error::InconsistentMetadata(format!(
                    "layer_mapping time_pos={} out of bounds for shape.times={}",
                    mapping.time_pos, shape.times
                )));
            }
            data.slice_mut(s![mapping.time_pos, 0, .., ..]).assign(&arr_2d);
        }

        Ok(RasterDataBlock {
            data,
            shape,
            no_data: R::zero(),
            region: *region,
        })
    }
}

/// Probe output band count by running worker on first block of (data, mask).
fn probe_output_layers_with_mask<R, U, V, F>(
    rds: &RasterDataset<R>,
    mask: &RasterDataset<U>,
    worker: &F,
) -> Result<usize>
where
    R: RasterType,
    U: RasterType,
    V: RasterType,
    F: Fn(&RasterDataBlock<R>, &RasterDataBlock<U>) -> Array3<V> + Send + Sync,
{
    let data_region = rds
        .blocks
        .first()
        .ok_or_else(|| Error::Other("dataset has no blocks".into()))?;
    let mask_region = mask
        .blocks
        .first()
        .ok_or_else(|| Error::Other("mask has no blocks".into()))?;
    let data_block = read_block::<R>(rds, *data_region)?;
    let mask_block = read_block::<U>(mask, *mask_region)?;
    let out = worker(&data_block, &mask_block);
    Ok(out.dim().0)
}

/// Generic block-iteration helper used by `apply` and `apply_reduction`.
///
/// Spawns a rayon pool of `n_threads`, iterates `rds.blocks` in parallel,
/// reads each block, hands it to `per_block`, trims overlap, and writes
/// through `writer`.
fn run_blocks<R, V, F>(
    rds: &RasterDataset<R>,
    n_threads: usize,
    writer: &Arc<ParallelGeoTiffWriter>,
    per_block: F,
) -> Result<()>
where
    R: RasterType,
    V: RasterType,
    F: Fn(&RasterDataBlock<R>) -> Result<Array3<V>> + Send + Sync,
{
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_threads.max(1))
        .build()
        .map_err(|e| Error::Other(format!("rayon pool: {e}")))?;

    pool.install(|| -> Result<()> {
        rds.blocks.par_iter().try_for_each(|region| -> Result<()> {
            let block = read_block::<R>(rds, *region)?;
            let result = per_block(&block)?;
            let trimmed = trim_overlap(&result, region);
            let w: &dyn BlockWriter<V> = writer.as_ref();
            w.write_block(&trimmed, region.write_window())?;
            Ok(())
        })
    })
}

/// Probe the output band count by running the worker on the first block.
fn probe_output_layers<R, V, F>(rds: &RasterDataset<R>, worker: &F) -> Result<usize>
where
    R: RasterType,
    V: RasterType,
    F: Fn(&RasterDataBlock<R>) -> Array3<V> + Send + Sync,
{
    let region = rds
        .blocks
        .first()
        .ok_or_else(|| Error::Other("dataset has no blocks".into()))?;
    let probe = read_block::<R>(rds, *region)?;
    let out = worker(&probe);
    Ok(out.dim().0)
}

// Per-worker cache of opened `gdal::Dataset` handles, keyed by file path.
// `gdal::Dataset` is `!Send`, so we cannot share opened handles across
// rayon worker threads. Each worker keeps its own thread-local map via
// `thread_local!`. First block on a worker opens the scene; later blocks
// reuse the handle — eliminates `n_blocks × n_scenes` redundant
// `Dataset::open` calls.
thread_local! {
    static DATASET_CACHE: std::cell::RefCell<std::collections::HashMap<std::path::PathBuf, gdal::Dataset>>
        = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Read a single GDAL window via the per-thread Dataset cache.
fn read_window_cached<R: RasterType>(
    source: &std::path::Path,
    band_idx: usize,
    window_offset: (isize, isize),
    window_size: (usize, usize),
) -> Result<ndarray::Array2<R>> {
    DATASET_CACHE.with(|cell| -> Result<ndarray::Array2<R>> {
        let mut map = cell.borrow_mut();
        if !map.contains_key(source) {
            let ds = gdal::Dataset::open(source).map_err(|e| {
                Error::Other(format!("open {} (cached): {e}", source.display()))
            })?;
            map.insert(source.to_path_buf(), ds);
        }
        let ds = map.get(source).ok_or_else(|| {
            Error::Other(format!("cached dataset missing immediately after insert: {}", source.display()))
        })?;
        let band = ds
            .rasterband(band_idx)
            .map_err(|e| Error::Other(format!("rasterband {band_idx} of {}: {e}", source.display())))?;
        let buffer = band
            .read_as::<R>(window_offset, window_size, window_size, None)
            .map_err(|e| {
                Error::Other(format!(
                    "read_as {} band {band_idx} window={:?} size={:?}: {e}",
                    source.display(),
                    window_offset,
                    window_size
                ))
            })?;
        buffer
            .to_array()
            .map_err(|e| Error::Other(format!("buffer.to_array for {}: {e}", source.display())))
    })
}

/// Read one block into memory.
///
/// Iterates the dataset's `layer_mappings`, reads the per-block window
/// from each (file, band) pair via the **thread-local Dataset handle cache**
/// (so repeated reads of the same file on the same worker don't reopen),
/// and assigns each resulting `Array2<R>` into the 4-D output at
/// `[mapping.time_pos, mapping.layer_pos, .., ..]`.
///
/// `/vsis3/` paths work automatically because GDAL handles them via its
/// virtual filesystem layer when the path string starts with that prefix.
fn read_block<R: RasterType>(
    rds: &RasterDataset<R>,
    region: RasterRegion,
) -> Result<RasterDataBlock<R>> {
    use crate::types::RasterShape;
    use ndarray::{s, Array4};

    let rows = region.read_window.size.rows as usize;
    let cols = region.read_window.size.cols as usize;
    let shape = RasterShape {
        times: rds.metadata.shape.times,
        layers: rds.metadata.shape.layers,
        rows,
        cols,
    };

    let mut data: Array4<R> = Array4::zeros((shape.times, shape.layers, shape.rows, shape.cols));

    let window_offset = (region.read_window.offset.cols, region.read_window.offset.rows);
    let window_size = (cols, rows);

    for mapping in rds.layer_mappings() {
        // Cached read — first hit opens the file, later blocks on the same
        // thread reuse the handle.
        let array_2d = read_window_cached::<R>(
            &mapping.source,
            mapping.band,
            window_offset,
            window_size,
        )?;

        // Bounds-check that the layout matches our 4-D destination slot.
        if mapping.time_pos >= shape.times || mapping.layer_pos >= shape.layers {
            return Err(Error::InconsistentMetadata(format!(
                "layer_mapping time_pos={}, layer_pos={} out of bounds for shape times={}, layers={}",
                mapping.time_pos, mapping.layer_pos, shape.times, shape.layers
            )));
        }

        data.slice_mut(s![mapping.time_pos, mapping.layer_pos, .., ..])
            .assign(&array_2d);
    }

    Ok(RasterDataBlock {
        data,
        shape,
        no_data: R::zero(),
        region,
    })
}

/// Strip overlap pixels from a worker's `Array3` so only the inner block
/// data is written. With `Overlap::default()` (all zeros), this is a no-op
/// returning the original array.
fn trim_overlap<V: RasterType>(data: &Array3<V>, region: &RasterRegion) -> Array3<V> {
    use ndarray::s;
    let ov = region.overlap;
    if ov.top == 0 && ov.bottom == 0 && ov.left == 0 && ov.right == 0 {
        return data.clone();
    }
    let dim = data.dim();
    let rows = dim.1;
    let cols = dim.2;
    let row_end = rows.saturating_sub(ov.bottom);
    let col_end = cols.saturating_sub(ov.right);
    data.slice(s![.., ov.top..row_end, ov.left..col_end]).to_owned()
}

/// Convenience shape accessor.
#[allow(dead_code)]
fn data_shape<T: RasterType>(block: &RasterDataBlock<T>) -> RasterShape {
    block.shape
}
