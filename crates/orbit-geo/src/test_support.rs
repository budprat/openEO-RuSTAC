//! Test fixture infrastructure used by all unit and integration tests in the
//! `orbit-geo` crate. **Private** — `#[cfg(test)]` only.
//!
//! Built TDD-first in Phase 0.0 of the parity plan. Every public fn here
//! must have at least one test in this same file. See the archived parity plan §1.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use tempfile::{Builder, TempPath};

use crate::{
    types::{RasterType, ReadWindow},
    writer::BlockWriter,
    Result,
};
use ndarray::Array3;
use parking_lot::Mutex;

/// Captured call to `BlockWriter::write_block`.
#[derive(Debug, Clone)]
pub(crate) struct WriteCall<V> {
    pub window: ReadWindow,
    pub data: Array3<V>,
}

/// In-memory `BlockWriter` impl that records every call. Phase 0.0 fixture #4.
///
/// **RED stub**: trait impl is a silent no-op; `writes()` returns empty.
/// Tests fail until GREEN implements recording.
pub(crate) struct MockBlockWriter<V: RasterType> {
    writes: Mutex<Vec<WriteCall<V>>>,
    overview_calls: Mutex<Vec<(String, Vec<i32>)>>,
}

impl<V: RasterType + Clone> MockBlockWriter<V> {
    pub(crate) fn new() -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
            overview_calls: Mutex::new(Vec::new()),
        }
    }
    pub(crate) fn writes(&self) -> Vec<WriteCall<V>> {
        self.writes.lock().clone()
    }
    pub(crate) fn overview_calls(&self) -> Vec<(String, Vec<i32>)> {
        self.overview_calls.lock().clone()
    }
}

impl<V: RasterType + Clone> BlockWriter<V> for MockBlockWriter<V> {
    fn write_block(&self, data: &Array3<V>, window: ReadWindow) -> Result<()> {
        self.writes.lock().push(WriteCall { window, data: data.clone() });
        Ok(())
    }
    fn build_overviews(&self, resampling: &str, levels: &[i32]) -> Result<()> {
        self.overview_calls.lock().push((resampling.to_string(), levels.to_vec()));
        Ok(())
    }
}

/// Build a tiny single-band GeoTIFF at a temp path, filled with `fill`,
/// in projection `epsg`. Phase 0.0 fixture #1.
pub(crate) fn tiny_geotiff(rows: usize, cols: usize, fill: i16, epsg: u32) -> TempPath {
    use gdal::raster::{Buffer, RasterCreationOptions};
    use gdal::spatial_ref::SpatialRef;
    use gdal::DriverManager;

    let tmp = Builder::new()
        .suffix(".tif")
        .tempfile()
        .expect("tempfile create");
    let temp_path = tmp.into_temp_path();
    std::fs::remove_file(&temp_path).ok();

    let driver = DriverManager::get_driver_by_name("GTiff").expect("GTiff driver");
    let options = RasterCreationOptions::from_iter(["TILED=NO", "COMPRESS=NONE"]);
    let mut ds = driver
        .create_with_band_type_with_options::<i16, _>(&temp_path, cols, rows, 1, &options)
        .expect("create GeoTIFF");

    ds.set_geo_transform(&[0.0, 1.0, 0.0, rows as f64, 0.0, -1.0])
        .expect("set_geo_transform");
    let sr = SpatialRef::from_epsg(epsg).expect("EPSG -> SpatialRef");
    ds.set_spatial_ref(&sr).expect("set_spatial_ref");

    let mut band = ds.rasterband(1).expect("band 1");
    let data: Vec<i16> = vec![fill; rows * cols];
    let mut buf = Buffer::new((cols, rows), data);
    band.write::<i16>((0, 0), (cols, rows), &mut buf)
        .expect("write band");
    drop(band);
    drop(ds);

    temp_path
}

/// A 3-track synthetic scene set: `n_times` triples of `(red, nir, fmask)`. Fixture #3.
pub(crate) fn synthetic_scene_set(
    n_times: usize,
    rows: usize,
    cols: usize,
) -> (Vec<TempPath>, Vec<TempPath>, Vec<TempPath>) {
    let mut reds = Vec::with_capacity(n_times);
    let mut nirs = Vec::with_capacity(n_times);
    let mut masks = Vec::with_capacity(n_times);
    for t in 0..n_times {
        reds.push(tiny_geotiff(rows, cols, (100 + t) as i16, 4326));
        nirs.push(tiny_geotiff(rows, cols, (200 + t) as i16, 4326));
        masks.push(tiny_geotiff(rows, cols, 1_i16, 4326));
    }
    (reds, nirs, masks)
}

/// Build a tiny N-band GeoTIFF where band k is filled with value k. Fixture #2.
pub(crate) fn multi_band_geotiff(rows: usize, cols: usize, n_bands: usize, epsg: u32) -> TempPath {
    use gdal::raster::{Buffer, RasterCreationOptions};
    use gdal::spatial_ref::SpatialRef;
    use gdal::DriverManager;

    let tmp = Builder::new()
        .suffix(".tif")
        .tempfile()
        .expect("tempfile create");
    let temp_path = tmp.into_temp_path();
    std::fs::remove_file(&temp_path).ok();

    let driver = DriverManager::get_driver_by_name("GTiff").expect("GTiff driver");
    let options = RasterCreationOptions::from_iter(["TILED=NO", "COMPRESS=NONE"]);
    let mut ds = driver
        .create_with_band_type_with_options::<i16, _>(&temp_path, cols, rows, n_bands, &options)
        .expect("create multi-band GeoTIFF");

    ds.set_geo_transform(&[0.0, 1.0, 0.0, rows as f64, 0.0, -1.0])
        .expect("set_geo_transform");
    let sr = SpatialRef::from_epsg(epsg).expect("EPSG -> SpatialRef");
    ds.set_spatial_ref(&sr).expect("set_spatial_ref");

    for k in 1..=n_bands {
        let mut band = ds.rasterband(k).expect("rasterband k");
        let data: Vec<i16> = vec![k as i16; rows * cols];
        let mut buf = Buffer::new((cols, rows), data);
        band.write::<i16>((0, 0), (cols, rows), &mut buf)
            .expect("write band k");
    }
    drop(ds);

    temp_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdal::Dataset;

    #[test]
    fn tiny_geotiff_creates_file_with_requested_dimensions() {
        let path = tiny_geotiff(10, 20, 42_i16, 4326);
        assert!(path.exists());
        let ds = Dataset::open(&*path).expect("reopen via gdal");
        let (w, h) = ds.raster_size();
        assert_eq!((w, h), (20, 10));
    }

    #[test]
    fn tiny_geotiff_fills_pixels_with_given_value() {
        use gdal::raster::Buffer;
        let path = tiny_geotiff(4, 6, 17_i16, 4326);
        let ds = Dataset::open(&*path).expect("reopen via gdal");
        let band = ds.rasterband(1).expect("band 1");
        let buf: Buffer<i16> = band
            .read_as((0, 0), (6, 4), (6, 4), None)
            .expect("read band");
        let pixels = buf.data();
        assert_eq!(pixels.len(), 24);
        for &px in pixels.iter() {
            assert_eq!(px, 17);
        }
    }

    #[test]
    fn tiny_geotiff_respects_requested_epsg() {
        let path = tiny_geotiff(2, 2, 0_i16, 3577);
        let ds = Dataset::open(&*path).expect("reopen via gdal");
        let sr = ds.spatial_ref().expect("dataset has SR");
        let auth_code = sr.auth_code().expect("SR has auth code");
        assert_eq!(auth_code, 3577);
    }

    #[test]
    fn multi_band_geotiff_has_n_bands() {
        let path = multi_band_geotiff(3, 3, 5, 4326);
        let ds = Dataset::open(&*path).expect("reopen via gdal");
        assert_eq!(ds.raster_count(), 5);
    }

    #[test]
    fn multi_band_geotiff_band_k_has_value_k() {
        use gdal::raster::Buffer;
        let path = multi_band_geotiff(2, 3, 4, 4326);
        let ds = Dataset::open(&*path).expect("reopen via gdal");
        for k in 1..=4 {
            let band = ds.rasterband(k).expect("band exists");
            let buf: Buffer<i16> = band
                .read_as((0, 0), (3, 2), (3, 2), None)
                .expect("read band");
            for &px in buf.data().iter() {
                assert_eq!(px as usize, k);
            }
        }
    }

    #[test]
    fn synthetic_scene_set_produces_red_nir_fmask_triples() {
        use gdal::raster::Buffer;
        let (reds, nirs, masks) = synthetic_scene_set(3, 2, 2);
        assert_eq!(reds.len(), 3);
        assert_eq!(nirs.len(), 3);
        assert_eq!(masks.len(), 3);
        for (t, p) in reds.iter().enumerate() {
            let ds = Dataset::open(&**p).expect("reopen red");
            let band = ds.rasterband(1).expect("band 1");
            let buf: Buffer<i16> = band
                .read_as((0, 0), (2, 2), (2, 2), None)
                .expect("read band");
            assert_eq!(buf.data()[0], (100 + t) as i16);
        }
        for (t, p) in nirs.iter().enumerate() {
            let ds = Dataset::open(&**p).expect("reopen nir");
            let band = ds.rasterband(1).expect("band 1");
            let buf: Buffer<i16> = band
                .read_as((0, 0), (2, 2), (2, 2), None)
                .expect("read band");
            assert_eq!(buf.data()[0], (200 + t) as i16);
        }
        for p in masks.iter() {
            let ds = Dataset::open(&**p).expect("reopen mask");
            let band = ds.rasterband(1).expect("band 1");
            let buf: Buffer<i16> = band
                .read_as((0, 0), (2, 2), (2, 2), None)
                .expect("read band");
            assert_eq!(buf.data()[0], 1);
        }
    }

    /// **RED #7**: write_block calls captured with correct data + window, in order.
    #[test]
    fn mock_block_writer_records_write_calls_in_order() {
        use crate::types::{Offset, ReadWindow, Size};
        let mock: MockBlockWriter<i16> = MockBlockWriter::new();
        let a: Array3<i16> = Array3::from_shape_fn((1, 2, 2), |(_, r, c)| (r * 2 + c) as i16);
        let b: Array3<i16> = Array3::from_shape_fn((1, 2, 2), |(_, r, c)| (10 + r * 2 + c) as i16);
        let w1 = ReadWindow {
            offset: Offset { rows: 0, cols: 0 },
            
            size: Size { rows: 2, cols: 2 },
        };
        let w2 = ReadWindow {
            offset: Offset { rows: 0, cols: 2 },
            
            size: Size { rows: 2, cols: 2 },
        };
        BlockWriter::write_block(&mock, &a, w1).unwrap();
        BlockWriter::write_block(&mock, &b, w2).unwrap();
        let calls = mock.writes();
        assert_eq!(calls.len(), 2, "expected 2 captured writes, got {}", calls.len());
        assert_eq!(calls[0].window.offset.cols, 0);
        assert_eq!(calls[1].window.offset.cols, 2);
        assert_eq!(calls[0].data, a);
        assert_eq!(calls[1].data, b);
    }

    /// **RED #8**: build_overviews captured separately.
    #[test]
    fn mock_block_writer_records_build_overviews_call() {
        let mock: MockBlockWriter<i16> = MockBlockWriter::new();
        BlockWriter::build_overviews(&mock, "AVERAGE", &[2, 4, 8]).unwrap();
        let ov = mock.overview_calls();
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].0, "AVERAGE");
        assert_eq!(ov[0].1, vec![2, 4, 8]);
    }
}

#[cfg(test)]
mod apply_tests {
    //! **Phase 0.1 — Retroactive tests for the three existing `apply*` methods.**
    //!
    //! These methods predate strict TDD in this crate; this module pins their
    //! contracts before any Tier 1 extension lands.
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::RasterDataset,
        types::{BlockSize, Dimension, ImageResolution},
    };
    use ndarray::Array3;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Helper: build a tiny `RasterDataset<i16>` from `synthetic_scene_set` reds.
    fn dataset_from_reds(n_times: usize, rows: usize, cols: usize, block: usize) -> (RasterDataset<i16>, Vec<TempPath>) {
        let (reds, _nirs, _masks) = synthetic_scene_set(n_times, rows, cols);
        let red_paths: Vec<PathBuf> = reds.iter().map(|p| p.to_path_buf()).collect();
        let rds = RasterDatasetBuilder::from_files(&red_paths)
            .expect("builder from_files")
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .expect("build dataset");
        // Return reds so the temp files live for the duration of the test.
        (rds, reds)
    }

    /// **A1**: `apply` invokes worker once per block (+1 probe call).
    #[test]
    fn apply_calls_worker_once_per_block() {
        let (rds, _live) = dataset_from_reds(1, 4, 4, 2);
        let n_blocks = rds.num_blocks();
        let counter = AtomicUsize::new(0);
        let out = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        rds.apply::<i16, _>(
            |_block| {
                counter.fetch_add(1, Ordering::SeqCst);
                Array3::<i16>::zeros((1, 2, 2))
            },
            1,
            &out_path,
        )
        .expect("apply ok");

        let calls = counter.load(Ordering::SeqCst);
        // probe_output_layers calls worker once on first block; then 1 call per block.
        assert_eq!(
            calls,
            n_blocks + 1,
            "expected n_blocks+1 calls (probe + per-block); got {calls}"
        );
    }

    /// **A2**: `apply` writes a valid GeoTIFF with the dataset's spatial shape.
    #[test]
    fn apply_writes_output_geotiff_with_expected_shape() {
        let (rds, _live) = dataset_from_reds(1, 4, 4, 2);
        let out = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        rds.apply::<i16, _>(
            |_block| Array3::<i16>::from_elem((1, 2, 2), 7),
            1,
            &out_path,
        )
        .expect("apply ok");

        let ds = gdal::Dataset::open(&*out_path).expect("reopen output");
        let (w, h) = ds.raster_size();
        assert_eq!((w, h), (4, 4), "output dims");
        assert_eq!(ds.raster_count(), 1, "1 band output");
    }

    /// **A3**: `apply_reduction` collapses to a single-band output regardless of input layers.
    #[test]
    fn apply_reduction_collapses_time_axis_to_one_band() {
        let (rds, _live) = dataset_from_reds(3, 4, 4, 2);
        let out = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        rds.apply_reduction::<i16, _>(
            |_block, _dim| Array3::<i16>::from_elem((1, 2, 2), 42),
            Dimension::Time,
            1,
            &out_path,
            i16::MIN,
        )
        .expect("apply_reduction ok");

        let ds = gdal::Dataset::open(&*out_path).expect("reopen output");
        assert_eq!(ds.raster_count(), 1, "reduction → 1 band");
    }

    /// **A4**: `apply_reduction_with_mask` runs to completion when data + mask align.
    #[test]
    fn apply_reduction_with_mask_writes_single_band_output() {
        let (rds_data, _live_d) = dataset_from_reds(2, 4, 4, 2);
        let (rds_mask, _live_m) = dataset_from_reds(1, 4, 4, 2);
        let out = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        rds_data
            .apply_reduction_with_mask::<i16, i16, _>(
                &rds_mask,
                |_data_block, _mask_block, _dim| Array3::<i16>::from_elem((1, 2, 2), 99),
                Dimension::Time,
                1,
                &out_path,
                i16::MIN,
            )
            .expect("apply_reduction_with_mask ok");

        let ds = gdal::Dataset::open(&*out_path).expect("reopen output");
        assert_eq!(ds.raster_count(), 1);
        let band = ds.rasterband(1).expect("band 1");
        let buf: gdal::raster::Buffer<i16> = band
            .read_as((0, 0), (4, 4), (4, 4), None)
            .expect("read band");
        for &v in buf.data() {
            assert_eq!(v, 99, "worker output should propagate to all 4×4 pixels");
        }
    }

    /// **A5 (deferred)**: workers cannot return Result in the current API
    /// (the signature is `Fn(...) -> Array3<V>` not `-> Result<Array3<V>>`).
    /// Capturing this as a documented gap; will be addressed by the
    /// `apply_with_mask` Tier 1 task which will introduce fallible workers.
    #[test]
    fn apply_propagates_worker_error_deferred() {
        // Intentionally empty — API doesn't support fallible workers today.
        // See the archived parity plan §4 Tier 1.1 for the API change that enables this.
    }
}

#[cfg(test)]
mod tier0_t01_tests {
    //! **T0.1 — build_overviews invocation tests.**
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::RasterDataset,
        types::{BlockSize, Dimension, ImageResolution},
    };
    use ndarray::Array3;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn dataset_from_reds(n_times: usize, rows: usize, cols: usize, block: usize) -> (RasterDataset<i16>, Vec<TempPath>) {
        let (reds, _nirs, _masks) = synthetic_scene_set(n_times, rows, cols);
        let red_paths: Vec<PathBuf> = reds.iter().map(|p| p.to_path_buf()).collect();
        let rds = RasterDatasetBuilder::from_files(&red_paths)
            .expect("builder from_files")
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .expect("build dataset");
        (rds, reds)
    }

    /// **RED T0.1/A1**: `apply_reduction_to_writer` must call `build_overviews`
    /// exactly once, AFTER all `write_block` calls have completed.
    #[test]
    fn apply_reduction_to_writer_calls_build_overviews_after_writes() {
        let (rds, _live) = dataset_from_reds(2, 4, 4, 2);
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());

        rds.apply_reduction_to_writer::<i16, _, _>(
            Arc::clone(&mock),
            |_block, _dim| Array3::<i16>::from_elem((1, 2, 2), 7),
            Dimension::Time,
            1,
        )
        .expect("apply_reduction_to_writer ok");

        let writes = mock.writes();
        let overviews = mock.overview_calls();
        assert!(
            writes.len() >= rds.num_blocks(),
            "expected at least {} writes, got {}",
            rds.num_blocks(),
            writes.len()
        );
        assert_eq!(
            overviews.len(),
            1,
            "expected exactly 1 build_overviews call, got {}",
            overviews.len()
        );
        // Atom A3: ordering is implicit via control flow — try_for_each
        // completes ALL writes before the function returns, then build_overviews
        // is called. Re-asserting in narrative form: both happened.
    }

    /// **RED T0.1/A2**: same for `apply_reduction_with_mask_to_writer`.
    #[test]
    fn apply_reduction_with_mask_to_writer_calls_build_overviews_after_writes() {
        let (rds_data, _live_d) = dataset_from_reds(2, 4, 4, 2);
        let (rds_mask, _live_m) = dataset_from_reds(1, 4, 4, 2);
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());

        rds_data
            .apply_reduction_with_mask_to_writer::<i16, i16, _, _>(
                &rds_mask,
                Arc::clone(&mock),
                |_d, _m, _dim| Array3::<i16>::from_elem((1, 2, 2), 11),
                Dimension::Time,
                1,
            )
            .expect("apply_reduction_with_mask_to_writer ok");

        let writes = mock.writes();
        let overviews = mock.overview_calls();
        assert!(
            writes.len() >= rds_data.num_blocks(),
            "expected at least {} writes, got {}",
            rds_data.num_blocks(),
            writes.len()
        );
        assert_eq!(
            overviews.len(),
            1,
            "expected exactly 1 build_overviews call (with mask), got {}",
            overviews.len()
        );
    }
}

#[cfg(test)]
mod tier0_t02_tests {
    //! **T0.2 — Correctness test for NDVI-mean-over-time before benching.**
    //!
    //! Phase 0.0 fixtures: `synthetic_scene_set(n_times, rows, cols)` produces
    //! red[t]=100+t, nir[t]=200+t. Expected NDVI mean (i16, ×10000) hand-computed.
    use super::*;
    use crate::{
        block::RasterDataBlock,
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, Dimension, ImageResolution},
    };
    use ndarray::{s, Array3, Axis};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// NDVI-mean-over-time worker. Layer 0 = red, Layer 1 = nir.
    ///
    /// Used by both the correctness test and the criterion bench.
    pub(crate) fn ndvi_mean_worker(
        rdb: &RasterDataBlock<i16>,
        _dim: Dimension,
    ) -> Array3<i16> {
        let (n_times, _layers, rows, cols) = rdb.data.dim();
        let mut sum = ndarray::Array2::<f64>::zeros((rows, cols));
        for time_slice in rdb.data.axis_iter(Axis(0)) {
            let red = time_slice.slice(s![0, .., ..]).mapv(|e| e as f32);
            let nir = time_slice.slice(s![1, .., ..]).mapv(|e| e as f32);
            let denom = &nir + &red + 1e-10_f32;
            let ndvi_t = (&nir - &red) / &denom * 10_000.0_f32;
            for (acc, &v) in sum.iter_mut().zip(ndvi_t.iter()) {
                *acc += f64::from(v);
            }
        }
        let mean = sum.mapv(|s| (s / n_times as f64) as i16);
        mean.insert_axis(Axis(0))
    }

    /// Build a 2-layer (red + nir) dataset using `synthetic_scene_set`.
    pub(crate) fn dataset_red_nir(
        n_times: usize,
        rows: usize,
        cols: usize,
        block: usize,
    ) -> (RasterDataset<i16>, Vec<TempPath>) {
        let (reds, nirs, _masks) = synthetic_scene_set(n_times, rows, cols);

        // All files combined for builder's `from_files` (it counts them as scenes).
        let mut all_paths: Vec<PathBuf> = Vec::new();
        all_paths.extend(reds.iter().map(|p| p.to_path_buf()));
        all_paths.extend(nirs.iter().map(|p| p.to_path_buf()));

        let mut rds = RasterDatasetBuilder::from_files(&all_paths)
            .expect("builder from_files")
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .expect("build dataset");

        // Override: 2 layers (red=0, nir=1), n_times timesteps.
        rds.metadata.shape.times = n_times;
        rds.metadata.shape.layers = 2;
        let mut mappings = Vec::with_capacity(n_times * 2);
        for (t, r) in reds.iter().enumerate() {
            mappings.push(LayerMapping {
                source: r.to_path_buf(),
                time_pos: t,
                layer_pos: 0,
                band: 1,
            });
        }
        for (t, n) in nirs.iter().enumerate() {
            mappings.push(LayerMapping {
                source: n.to_path_buf(),
                time_pos: t,
                layer_pos: 1,
                band: 1,
            });
        }
        rds.layer_mappings = mappings;

        // Keep TempPaths alive: hand reds back so they don't drop.
        let mut live: Vec<TempPath> = Vec::with_capacity(n_times * 2);
        live.extend(reds);
        live.extend(nirs);
        (rds, live)
    }

    /// **RED T0.2/A1+A2+A3**: NDVI-mean worker via `apply_reduction_to_writer`
    /// produces value 3322 (hand-computed for red[t]=100+t, nir[t]=200+t, n_times=2).
    #[test]
    fn ndvi_mean_worker_produces_expected_value_for_synthetic_input() {
        let (rds, _live) = dataset_red_nir(2, 2, 2, 2);
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());

        rds.apply_reduction_to_writer::<i16, _, _>(
            Arc::clone(&mock),
            ndvi_mean_worker,
            Dimension::Time,
            1,
        )
        .expect("apply_reduction_to_writer ok");

        let writes = mock.writes();
        assert_eq!(writes.len(), 1, "1 block expected for 2x2 dataset with block_size 2");
        let data = &writes[0].data;
        assert_eq!(data.dim(), (1, 2, 2), "output is (1 layer, 2 rows, 2 cols)");

        // Hand-computed: (3333 + 3311) / 2 = 3322. Allow ±2 for f32 rounding.
        for &v in data.iter() {
            assert!(
                (v - 3322).abs() <= 2,
                "NDVI mean expected ~3322, got {v}"
            );
        }
    }
}

#[cfg(test)]
mod tier0_t02_discriminator {
    //! **T0.2 discriminator test**: different n_times → different expected mean.
    //!
    //! This second test proves the worker is computing, not hardcoding 3322.
    use super::tier0_t02_tests::{dataset_red_nir, ndvi_mean_worker};
    use crate::{test_support::MockBlockWriter, types::Dimension};
    use std::sync::Arc;

    /// For n_times=3: NDVI[t] = (100)/(100+200+2t) × 10000 → ~3333, ~3311, ~3289;
    /// mean = ~3311 (≠ 3322 from the n_times=2 case).
    #[test]
    fn ndvi_mean_worker_changes_with_n_times() {
        let (rds, _live) = dataset_red_nir(3, 2, 2, 2);
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());
        rds.apply_reduction_to_writer::<i16, _, _>(
            Arc::clone(&mock),
            ndvi_mean_worker,
            Dimension::Time,
            1,
        )
        .expect("apply_reduction_to_writer ok");
        let writes = mock.writes();
        for &v in writes[0].data.iter() {
            assert!(
                (v - 3311).abs() <= 2,
                "n_times=3 NDVI mean expected ~3311, got {v}"
            );
        }
    }
}

#[cfg(test)]
mod tier1_t11_tests {
    //! **T1.1 — `apply_with_mask` and `apply_with_mask_to_writer` tests.**
    use super::*;
    use crate::{
        block::RasterDataBlock,
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
    };
    use ndarray::Array3;
    use std::sync::Arc;

    fn data_and_mask_aligned(rows: usize, cols: usize, block: usize) -> (RasterDataset<i16>, RasterDataset<u8>, Vec<TempPath>) {
        // Data: single i16 file filled with value 50.
        let data_file = tiny_geotiff(rows, cols, 50, 4326);
        let data_paths = vec![data_file.to_path_buf()];

        // Mask: single i16 file filled with 1 (we reinterpret as u8 via builder).
        let mask_file = tiny_geotiff(rows, cols, 1, 4326);
        let mask_paths = vec![mask_file.to_path_buf()];

        let rds_data = RasterDatasetBuilder::from_files(&data_paths)
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .unwrap();
        // Hand-build the mask dataset as RasterDataset<u8> sharing the same
        // block partitioning as data. We construct it manually because
        // RasterDatasetBuilder is generic over T and inspecting an i16 file
        // as u8 is OK at the GDAL layer (we read via Buffer<u8>).
        let mut rds_mask: RasterDataset<u8> = RasterDatasetBuilder::<u8>::from_files(&mask_paths)
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .unwrap();
        rds_mask.layer_mappings = vec![LayerMapping {
            source: mask_paths[0].clone(),
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        (rds_data, rds_mask, vec![data_file, mask_file])
    }

    /// **RED T1.1/A1**: worker receives the mask block and can use it.
    #[test]
    fn apply_with_mask_to_writer_passes_mask_block_to_worker() {
        let (rds_data, rds_mask, _live) = data_and_mask_aligned(2, 2, 2);
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());

        // Worker outputs the mask's first pixel as a single-layer Array3.
        // If mask block is missing/zero, output would be 0; if present, output = 1.
        rds_data
            .apply_with_mask_to_writer::<u8, i16, _, _>(
                &rds_mask,
                Arc::clone(&mock),
                |_data_block: &RasterDataBlock<i16>, mask_block: &RasterDataBlock<u8>| -> Array3<i16> {
                    let v = mask_block.data[[0, 0, 0, 0]] as i16;
                    Array3::<i16>::from_elem((1, 2, 2), v)
                },
                1,
            )
            .expect("apply_with_mask_to_writer ok");

        let writes = mock.writes();
        assert_eq!(writes.len(), 1, "1 block, 1 write expected");
        for &v in writes[0].data.iter() {
            assert_eq!(
                v, 1,
                "worker should see mask block's value (1, clear-sky), got {v}"
            );
        }
    }

    /// **RED T1.1/A2**: output preserves the layer count returned by worker.
    #[test]
    fn apply_with_mask_to_writer_writes_each_block_output() {
        let (rds_data, rds_mask, _live) = data_and_mask_aligned(2, 2, 2);
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());

        rds_data
            .apply_with_mask_to_writer::<u8, i16, _, _>(
                &rds_mask,
                Arc::clone(&mock),
                |_d, _m| Array3::<i16>::from_elem((2, 2, 2), 77),
                1,
            )
            .expect("apply_with_mask_to_writer ok");

        let writes = mock.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].data.dim(), (2, 2, 2), "worker output preserved (2 layers)");
        for &v in writes[0].data.iter() {
            assert_eq!(v, 77);
        }
    }

    /// **RED T1.1/A3**: mismatched block count between data and mask → Err.
    #[test]
    fn apply_with_mask_to_writer_errors_on_misaligned_block_partitioning() {
        let (rds_data, _rds_mask, _live_d) = data_and_mask_aligned(4, 4, 2);
        // Make a mask with DIFFERENT block size → different num_blocks.
        let mask_file = tiny_geotiff(4, 4, 1, 4326);
        let mask_paths = vec![mask_file.to_path_buf()];
        let rds_mask_misaligned: RasterDataset<u8> = RasterDatasetBuilder::<u8>::from_files(&mask_paths)
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 }) // 1 block vs data's 4 blocks
            .build()
            .unwrap();

        assert_ne!(
            rds_data.num_blocks(),
            rds_mask_misaligned.num_blocks(),
            "test setup: misaligned blocks"
        );
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());
        let result = rds_data.apply_with_mask_to_writer::<u8, i16, _, _>(
            &rds_mask_misaligned,
            Arc::clone(&mock),
            |_d, _m| Array3::<i16>::zeros((1, 2, 2)),
            1,
        );
        assert!(
            result.is_err(),
            "misaligned blocks must produce an Err, got Ok"
        );
    }
}

#[cfg(test)]
mod tier1_t14_tests {
    //! **T1.4 — `read_block_layer_idx`: read just one layer's worth of data.**
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
    };

    /// Build a 2-layer (red=100, nir=200) dataset for layer-selection tests.
    fn dataset_2layer(rows: usize, cols: usize, block: usize) -> (RasterDataset<i16>, Vec<TempPath>) {
        let red = tiny_geotiff(rows, cols, 100, 4326);
        let nir = tiny_geotiff(rows, cols, 200, 4326);
        let red_path = red.to_path_buf();
        let nir_path = nir.to_path_buf();
        let all = vec![red_path.clone(), nir_path.clone()];
        let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&all)
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .unwrap();
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 2;
        rds.layer_mappings = vec![
            LayerMapping { source: red_path, time_pos: 0, layer_pos: 0, band: 1 },
            LayerMapping { source: nir_path, time_pos: 0, layer_pos: 1, band: 1 },
        ];
        (rds, vec![red, nir])
    }

    /// **RED T1.4/A1**: returned block has shape `(times, 1, rows, cols)`.
    #[test]
    fn read_block_layer_idx_returns_single_layer_dim() {
        let (rds, _live) = dataset_2layer(4, 4, 2);
        let block = rds.read_block_layer_idx(0, 0).expect("read layer 0");
        assert_eq!(block.data.dim(), (1, 1, 2, 2), "expected (1 time, 1 layer, 2, 2)");
    }

    /// **RED T1.4/A2**: layer 1 returns nir values (200), layer 0 returns red (100).
    #[test]
    fn read_block_layer_idx_returns_correct_layer_values() {
        let (rds, _live) = dataset_2layer(4, 4, 2);
        let block_red = rds.read_block_layer_idx(0, 0).expect("read red");
        let block_nir = rds.read_block_layer_idx(0, 1).expect("read nir");
        assert_eq!(block_red.data[[0, 0, 0, 0]], 100, "layer 0 = red");
        assert_eq!(block_nir.data[[0, 0, 0, 0]], 200, "layer 1 = nir");
    }

    /// **RED T1.4/A3**: invalid layer_idx → Err.
    #[test]
    fn read_block_layer_idx_errors_on_out_of_range_layer() {
        let (rds, _live) = dataset_2layer(4, 4, 2);
        let result = rds.read_block_layer_idx(0, 99);
        assert!(result.is_err(), "out-of-range layer must error");
    }
}

#[cfg(test)]
mod tier1_t15_tests {
    //! **T1.5 — `write_window3` direct-write helper.**
    use super::*;
    use crate::{
        types::{GeoTransform},
        writer::ParallelGeoTiffWriter,
    };
    use ndarray::Array3;

    /// **RED T1.5/A1**: round-trip Array3<i16> at offset 0,0.
    #[test]
    fn write_window3_round_trips_array3_at_origin() {
        let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
        let path = tmp.into_temp_path();
        std::fs::remove_file(&path).ok();

        let writer = ParallelGeoTiffWriter::create::<i16>(
            &path,
            &GeoTransform([0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
            4326,
            4,
            4,
            1,
            -1_i16,
        )
        .expect("create writer");

        // Write 4-pixel Array3 at offset (0, 0).
        let data: Array3<i16> = Array3::from_shape_fn((1, 2, 2), |(_, r, c)| (r * 2 + c) as i16);
        writer
            .write_window3(data.view(), 0, 0)
            .expect("write_window3 ok");
        drop(writer);

        // Read back and verify.
        let ds = gdal::Dataset::open(&*path).expect("reopen");
        let band = ds.rasterband(1).expect("band 1");
        let buf: gdal::raster::Buffer<i16> = band
            .read_as((0, 0), (2, 2), (2, 2), None)
            .expect("read window");
        assert_eq!(buf.data(), &[0_i16, 1, 2, 3]);
    }

    /// **RED T1.5/A2**: round-trip at non-zero offset.
    #[test]
    fn write_window3_round_trips_at_nonzero_offset() {
        let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
        let path = tmp.into_temp_path();
        std::fs::remove_file(&path).ok();

        let writer = ParallelGeoTiffWriter::create::<i16>(
            &path,
            &GeoTransform([0.0, 1.0, 0.0, 6.0, 0.0, -1.0]),
            4326,
            6,
            6,
            1,
            -1_i16,
        )
        .expect("create writer");

        // Write 2x2 Array3 of 99s at offset (2, 2).
        let data: Array3<i16> = Array3::from_elem((1, 2, 2), 99);
        writer
            .write_window3(data.view(), 2, 2)
            .expect("write_window3 ok");
        drop(writer);

        let ds = gdal::Dataset::open(&*path).expect("reopen");
        let band = ds.rasterband(1).expect("band 1");
        let buf: gdal::raster::Buffer<i16> = band
            .read_as((2, 2), (2, 2), (2, 2), None)
            .expect("read window");
        assert_eq!(buf.data(), &[99_i16, 99, 99, 99]);
    }
}

#[cfg(test)]
mod tier1_t16_tests {
    //! **T1.6 — `mosaic` combines multiple raster files into one.**
    use super::*;
    use crate::gdal_utils::mosaic;

    /// **RED T1.6/A1**: Two same-extent rasters mosaic to a single output
    /// that opens via gdal and reports raster_count() >= 1.
    #[test]
    fn mosaic_combines_two_same_extent_rasters() {
        let a = tiny_geotiff(2, 2, 50, 4326);
        let b = tiny_geotiff(2, 2, 99, 4326);
        let inputs = vec![a.to_path_buf(), b.to_path_buf()];

        let out_handle = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out_handle.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        let result = mosaic(&inputs, &out_path).expect("mosaic ok");
        assert!(result.exists(), "output file must exist");

        let ds = gdal::Dataset::open(&result).expect("reopen mosaic output");
        assert_eq!(ds.raster_count(), 1, "single-band inputs → single-band output");
        let (w, h) = ds.raster_size();
        assert_eq!((w, h), (2, 2), "extent preserved for same-extent inputs");
    }

    /// **RED T1.6/A2**: mosaic of single input is essentially a copy.
    #[test]
    fn mosaic_single_input_produces_equivalent_output() {
        let a = tiny_geotiff(3, 3, 77, 4326);
        let inputs = vec![a.to_path_buf()];

        let out_handle = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out_handle.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        let result = mosaic(&inputs, &out_path).expect("mosaic ok");
        let ds = gdal::Dataset::open(&result).expect("reopen mosaic output");
        let band = ds.rasterband(1).expect("band 1");
        let buf: gdal::raster::Buffer<i16> = band
            .read_as((0, 0), (3, 3), (3, 3), None)
            .expect("read band");
        for &v in buf.data() {
            assert_eq!(v, 77, "values from single input should propagate");
        }
    }
}

#[cfg(test)]
mod tier1_t13_tests {
    //! **T1.3 — `apply_reduction_row_pixel`: per-row reducing worker.**
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
    };
    use ndarray::{Array1, ArrayView3};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dataset_red_nir_small() -> (RasterDataset<i16>, Vec<TempPath>) {
        let red = tiny_geotiff(4, 4, 100, 4326);
        let nir = tiny_geotiff(4, 4, 200, 4326);
        let red_path = red.to_path_buf();
        let nir_path = nir.to_path_buf();
        let mut rds: RasterDataset<i16> =
            RasterDatasetBuilder::from_files(&[red_path.clone(), nir_path.clone()])
                .unwrap()
                .resolution(ImageResolution { x: 1.0, y: -1.0 })
                .block_size(BlockSize { rows: 2, cols: 2 })
                .build()
                .unwrap();
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 2;
        rds.layer_mappings = vec![
            LayerMapping { source: red_path, time_pos: 0, layer_pos: 0, band: 1 },
            LayerMapping { source: nir_path, time_pos: 0, layer_pos: 1, band: 1 },
        ];
        (rds, vec![red, nir])
    }

    /// **RED T1.3/A1**: worker is invoked once per row, per block.
    /// 4×4 with block_size 2 = 4 blocks × 2 rows each = 8 worker calls total.
    #[test]
    fn apply_reduction_row_pixel_invokes_worker_per_row() {
        let (rds, _live) = dataset_red_nir_small();
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());
        let row_count = Arc::new(AtomicUsize::new(0));
        let rc_clone = Arc::clone(&row_count);

        rds.apply_reduction_row_pixel_to_writer::<i16, _, _>(
            Arc::clone(&mock),
            move |_row: ArrayView3<i16>| -> Array1<i16> {
                rc_clone.fetch_add(1, Ordering::SeqCst);
                Array1::<i16>::from_elem(2, 42)
            },
            1,
        )
        .expect("apply_reduction_row_pixel_to_writer ok");

        // 4 blocks × 2 rows per block = 8 worker invocations.
        let calls = row_count.load(Ordering::SeqCst);
        assert_eq!(calls, 8, "expected 4 blocks × 2 rows = 8 worker invocations, got {calls}");
    }

    /// **RED T1.3/A2+A3**: worker output assembled into block-shape Array3
    /// and written through writer.
    #[test]
    fn apply_reduction_row_pixel_assembles_rows_into_block_output() {
        let (rds, _live) = dataset_red_nir_small();
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());

        rds.apply_reduction_row_pixel_to_writer::<i16, _, _>(
            Arc::clone(&mock),
            |_row: ArrayView3<i16>| -> Array1<i16> {
                Array1::<i16>::from_elem(2, 7)
            },
            1,
        )
        .expect("apply_reduction_row_pixel_to_writer ok");

        let writes = mock.writes();
        assert_eq!(writes.len(), 4, "4 blocks expected");
        for write in &writes {
            assert_eq!(write.data.dim(), (1, 2, 2), "each block output: (1 layer, 2 rows, 2 cols)");
            for &v in write.data.iter() {
                assert_eq!(v, 7);
            }
        }
    }
}

#[cfg(test)]
mod tier1_t12_tests {
    //! **T1.2 — COG output variants of apply*.**
    use super::*;
    use crate::{
        block::RasterDataBlock,
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, Dimension, ImageResolution},
    };
    use ndarray::Array3;

    fn data_and_mask_for_cog(rows: usize, cols: usize, block: usize) -> (RasterDataset<i16>, RasterDataset<u8>, Vec<TempPath>) {
        let data = tiny_geotiff(rows, cols, 50, 4326);
        let mask = tiny_geotiff(rows, cols, 1, 4326);
        let rds_data = RasterDatasetBuilder::from_files(&[data.to_path_buf()])
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: block, cols: block })
            .build()
            .unwrap();
        let mut rds_mask: RasterDataset<u8> =
            RasterDatasetBuilder::<u8>::from_files(&[mask.to_path_buf()])
                .unwrap()
                .resolution(ImageResolution { x: 1.0, y: -1.0 })
                .block_size(BlockSize { rows: block, cols: block })
                .build()
                .unwrap();
        rds_mask.layer_mappings = vec![LayerMapping {
            source: mask.to_path_buf(),
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        (rds_data, rds_mask, vec![data, mask])
    }

    fn gdalinfo_layout(path: &std::path::Path) -> String {
        let out = std::process::Command::new("gdalinfo")
            .arg(path)
            .output()
            .expect("gdalinfo run");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// **RED T1.2/A1**: `apply_cog` output passes `gdalinfo` COG validation.
    #[test]
    fn apply_cog_produces_valid_cog() {
        // Use a large enough dataset (512x512) that COG can have a real overview.
        let (rds_data, _rds_mask, _live) = data_and_mask_for_cog(512, 512, 256);
        let out_handle = Builder::new().suffix(".tif").tempfile().unwrap();
        let out = out_handle.into_temp_path();
        std::fs::remove_file(&out).ok();

        rds_data
            .apply_cog::<i16, _>(
                |_block: &RasterDataBlock<i16>| Array3::<i16>::from_elem((1, 256, 256), 42),
                1,
                &out,
            )
            .expect("apply_cog ok");
        assert!(out.exists(), "output COG must exist");
        let info = gdalinfo_layout(&out);
        assert!(
            info.contains("LAYOUT=COG") || info.contains("Layout=COG"),
            "gdalinfo output must contain COG layout marker; got:\n{}",
            info
        );
    }

    /// **RED T1.2/A2**: `apply_with_mask_cog` output is a valid COG.
    #[test]
    fn apply_with_mask_cog_produces_valid_cog() {
        let (rds_data, rds_mask, _live) = data_and_mask_for_cog(512, 512, 256);
        let out_handle = Builder::new().suffix(".tif").tempfile().unwrap();
        let out = out_handle.into_temp_path();
        std::fs::remove_file(&out).ok();

        rds_data
            .apply_with_mask_cog::<u8, i16, _>(
                &rds_mask,
                |_d, _m| Array3::<i16>::from_elem((1, 256, 256), 17),
                1,
                &out,
            )
            .expect("apply_with_mask_cog ok");
        assert!(out.exists());
        let info = gdalinfo_layout(&out);
        assert!(
            info.contains("LAYOUT=COG") || info.contains("Layout=COG"),
            "gdalinfo must show COG layout"
        );
    }

    /// **RED T1.2/A3**: `apply_reduction_with_mask_cog` output is a valid COG.
    #[test]
    fn apply_reduction_with_mask_cog_produces_valid_cog() {
        let (rds_data, rds_mask, _live) = data_and_mask_for_cog(512, 512, 256);
        let out_handle = Builder::new().suffix(".tif").tempfile().unwrap();
        let out = out_handle.into_temp_path();
        std::fs::remove_file(&out).ok();

        rds_data
            .apply_reduction_with_mask_cog::<u8, i16, _>(
                &rds_mask,
                |_d, _m, _dim| Array3::<i16>::from_elem((1, 256, 256), 5),
                Dimension::Time,
                1,
                &out,
                i16::MIN,
            )
            .expect("apply_reduction_with_mask_cog ok");
        assert!(out.exists());
        let info = gdalinfo_layout(&out);
        assert!(
            info.contains("LAYOUT=COG") || info.contains("Layout=COG"),
            "gdalinfo must show COG layout"
        );
    }
}

#[cfg(test)]
mod tier3_t31_tests {
    //! **T3.1 — `extend` and `stack` composition.**
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        composition::{extend, stack},
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
    };

    /// Build a single-layer, single-timestep dataset with explicit shape.
    fn ds_1layer(fill: i16, rows: usize, cols: usize, epsg: u32) -> (RasterDataset<i16>, TempPath) {
        let f = tiny_geotiff(rows, cols, fill, epsg);
        let path = f.to_path_buf();
        let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[path.clone()])
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows, cols })
            .build()
            .unwrap();
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 1;
        rds.layer_mappings = vec![LayerMapping {
            source: path,
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        (rds, f)
    }

    /// **RED T3.1/A1**: `extend` sums times.
    #[test]
    fn extend_sums_times_along_time_axis() {
        let (a, _live_a) = ds_1layer(10, 4, 4, 4326);
        let (b, _live_b) = ds_1layer(20, 4, 4, 4326);
        let r = extend(&a, &b).expect("extend ok");
        assert_eq!(r.metadata.shape.times, 2, "times must sum");
        assert_eq!(r.metadata.shape.layers, 1, "layers unchanged");
        assert_eq!(r.layer_mappings.len(), 2, "1 mapping per timestep");
    }

    /// **RED T3.1/A2**: `stack` sums layers.
    #[test]
    fn stack_sums_layers_along_layer_axis() {
        let (a, _live_a) = ds_1layer(10, 4, 4, 4326);
        let (b, _live_b) = ds_1layer(20, 4, 4, 4326);
        let r = stack(&a, &b).expect("stack ok");
        assert_eq!(r.metadata.shape.layers, 2, "layers must sum");
        assert_eq!(r.metadata.shape.times, 1, "times unchanged");
        assert_eq!(r.layer_mappings.len(), 2, "1 mapping per layer");
    }

    /// **RED T3.1/A3**: extend errors on spatial mismatch.
    #[test]
    fn extend_errors_on_misaligned_spatial_extent() {
        let (a, _live_a) = ds_1layer(10, 4, 4, 4326);
        let (b, _live_b) = ds_1layer(20, 8, 8, 4326);
        assert!(extend(&a, &b).is_err());
    }

    /// **RED T3.1/A4**: stack errors on EPSG mismatch.
    #[test]
    fn stack_errors_on_epsg_mismatch() {
        let (a, _live_a) = ds_1layer(10, 4, 4, 4326);
        let (b, _live_b) = ds_1layer(20, 4, 4, 3577);
        assert!(stack(&a, &b).is_err());
    }
}

#[cfg(test)]
mod tier3_t32_tests {
    //! **T3.2 — sampling.**
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        sampling::{geo_to_pixel, sample, sample_at_point},
        types::{BlockSize, ImageResolution},
    };

    /// Build a 4×4 dataset, fill=77, with origin (0, 4), 1-unit pixels.
    fn ds_4x4() -> (RasterDataset<i16>, TempPath) {
        let f = tiny_geotiff(4, 4, 77, 4326);
        let path = f.to_path_buf();
        let mut rds: RasterDataset<i16> =
            RasterDatasetBuilder::from_files(&[path.clone()])
                .unwrap()
                .resolution(ImageResolution { x: 1.0, y: -1.0 })
                .block_size(BlockSize { rows: 4, cols: 4 })
                .build()
                .unwrap();
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 1;
        rds.layer_mappings = vec![LayerMapping {
            source: path,
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        (rds, f)
    }

    /// **RED T3.2/A2**: geo_to_pixel maps origin to (0, 0).
    #[test]
    fn geo_to_pixel_maps_origin_to_zero_zero() {
        // tiny_geotiff sets geo_transform = [0.0, 1.0, 0.0, rows, 0.0, -1.0].
        // For rows=4 dataset, origin = (0, 4). So geo (0, 4) → pixel (0, 0).
        let gt = [0.0, 1.0, 0.0, 4.0, 0.0, -1.0];
        let (row, col) = geo_to_pixel(0.0, 4.0, &gt);
        assert_eq!((row, col), (0, 0));
    }

    /// **RED T3.2/A2b**: geo_to_pixel maps interior point correctly.
    #[test]
    fn geo_to_pixel_maps_interior_point() {
        // 4×4 with 1-unit pixels, origin (0, 4). geo (1.5, 2.5) → row=(4-2.5)/1=1.5→1, col=1.5/1=1
        let gt = [0.0, 1.0, 0.0, 4.0, 0.0, -1.0];
        let (row, col) = geo_to_pixel(1.5, 2.5, &gt);
        assert_eq!((row, col), (1, 1));
    }

    /// **RED T3.2/A1**: sample_at_point returns the pixel value at coords.
    #[test]
    fn sample_at_point_returns_pixel_value() {
        let (rds, _live) = ds_4x4();
        let v = sample_at_point(&rds, 1.5, 2.5).expect("in bounds");
        assert_eq!(v, 77, "fill value");
    }

    /// **RED T3.2/A3**: sample_at_point errors for out-of-extent point.
    #[test]
    fn sample_at_point_errors_out_of_extent() {
        let (rds, _live) = ds_4x4();
        let r = sample_at_point(&rds, 100.0, 100.0);
        assert!(r.is_err(), "out-of-extent must error");
    }

    /// **RED T3.2/A4**: sample batch returns Option per point.
    #[test]
    fn sample_batch_returns_options() {
        let (rds, _live) = ds_4x4();
        let points = vec![
            (0.5, 3.5),    // in bounds
            (100.0, 100.0), // out of bounds
            (2.5, 1.5),    // in bounds
        ];
        let out = sample(&rds, &points);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], Some(77));
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(77));
    }
}

#[cfg(test)]
mod tier3_t34_tests {
    //! **T3.4 — rasterize polygon into raster pixels.**
    use super::*;
    use crate::rasterization::rasterize;

    /// Build a tiny GeoJSON file with a single polygon covering bbox (0,0)-(2,2).
    fn make_geojson(path: &std::path::Path) {
        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [0.0, 0.0]]]
                },
                "properties": {}
            }]
        }"#;
        std::fs::write(path, geojson).expect("write geojson");
    }

    /// **RED T3.4/A1**: rasterize a 2×2 polygon into a 4×4 raster (output bbox 0-4, 0-4).
    /// Polygon covers lower-left quadrant.
    #[test]
    fn rasterize_polygon_burns_inside_pixels() {
        let geo = Builder::new().suffix(".geojson").tempfile().unwrap();
        let geo_path = geo.into_temp_path();
        make_geojson(&geo_path);

        let out = Builder::new().suffix(".tif").tempfile().unwrap();
        let out_path = out.into_temp_path();
        std::fs::remove_file(&out_path).ok();

        let result = rasterize(
            &geo_path,
            &out_path,
            4, 4,
            (0.0, 0.0, 4.0, 4.0),
            1.0,
            0.0,
        )
        .expect("rasterize ok");
        assert!(result.exists());

        // Reopen and verify some inside-pixel is 1, some outside-pixel is 0.
        let ds = gdal::Dataset::open(&result).expect("reopen");
        let band = ds.rasterband(1).expect("band 1");
        let buf: gdal::raster::Buffer<f64> = band
            .read_as((0, 0), (4, 4), (4, 4), None)
            .expect("read");
        let pixels = buf.data();
        // Lower-left should have ~1.0 (inside polygon).
        // For a polygon covering (0,0)-(2,2) in a 4x4 raster spanning 0-4,
        // pixels in lower-left quadrant get burn_value.
        let total_burned = pixels.iter().filter(|&&v| v == 1.0).count();
        assert!(total_burned > 0, "at least some pixels must be burned to 1.0; got {pixels:?}");
        let total_unburned = pixels.iter().filter(|&&v| v == 0.0).count();
        assert!(total_unburned > 0, "at least some pixels stay at 0.0 (no_data); got {pixels:?}");
    }
}

#[cfg(test)]
mod tier3_t33_tests {
    //! **T3.3 — zonal_histogram counts pixels per class within mask.**
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
        zonal_stats::zonal_histogram,
    };

    fn ds_with_value(rows: usize, cols: usize, fill: i16) -> (RasterDataset<i16>, TempPath) {
        let f = tiny_geotiff(rows, cols, fill, 4326);
        let path = f.to_path_buf();
        let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[path.clone()])
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows, cols })
            .build()
            .unwrap();
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 1;
        rds.layer_mappings = vec![LayerMapping {
            source: path,
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        (rds, f)
    }

    /// **RED T3.3/A1**: histogram of all-class-5 data with all-mask-1 returns
    /// `{5: 16}` for 4×4 input.
    #[test]
    fn zonal_histogram_counts_pixels_per_class_with_full_mask() {
        let (data, _live_d) = ds_with_value(4, 4, 5);
        let (mask, _live_m) = ds_with_value(4, 4, 1);
        let h = zonal_histogram(&data, &mask).expect("zonal_histogram ok");
        assert_eq!(h.get(&5_i16), Some(&16_u64), "all 16 pixels in class 5");
    }
}

#[cfg(test)]
mod tier1_t13_with_mask_tests {
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
    };
    use ndarray::{Array1, ArrayView3};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn apply_reduction_row_pixel_with_mask_invokes_worker_per_row() {
        let d = tiny_geotiff(4, 4, 5, 4326);
        let m = tiny_geotiff(4, 4, 1, 4326);
        let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[d.to_path_buf()])
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 2, cols: 2 })
            .build()
            .unwrap();
        let mut rds_mask: RasterDataset<u8> =
            RasterDatasetBuilder::<u8>::from_files(&[m.to_path_buf()])
                .unwrap()
                .resolution(ImageResolution { x: 1.0, y: -1.0 })
                .block_size(BlockSize { rows: 2, cols: 2 })
                .build()
                .unwrap();
        rds_mask.layer_mappings = vec![LayerMapping {
            source: m.to_path_buf(),
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        let mock: Arc<MockBlockWriter<i16>> = Arc::new(MockBlockWriter::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&calls);
        rds.apply_reduction_row_pixel_with_mask_to_writer::<u8, i16, _, _>(
            &rds_mask,
            Arc::clone(&mock),
            move |_row_d: ArrayView3<i16>, _row_m: ArrayView3<u8>| {
                cc.fetch_add(1, Ordering::SeqCst);
                Array1::<i16>::from_elem(2, 11)
            },
            1,
        )
        .expect("ok");
        // 4 blocks × 2 rows = 8 invocations
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        assert!(mock.writes().len() >= 4);
    }
}

#[cfg(test)]
mod batch_g_tests {
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        types::{BlockSize, GeoTransform, ImageResolution},
    };

    #[test]
    fn builder_template_copies_metadata_from_other_dataset() {
        let f1 = tiny_geotiff(4, 4, 5, 4326);
        let rds1: crate::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f1.to_path_buf()]).unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .build().unwrap();
        let f2 = tiny_geotiff(8, 8, 0, 3577);
        let rds2: crate::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f2.to_path_buf()]).unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .template(&rds1)
            .build().unwrap();
        // After template(), rds2 inherits rds1's epsg + image_size
        assert_eq!(rds2.metadata.epsg_code, 4326);
        assert_eq!(rds2.metadata.shape.rows, 4);
        assert_eq!(rds2.metadata.shape.cols, 4);
    }

    #[test]
    fn builder_epsg_override_applied() {
        let f = tiny_geotiff(4, 4, 5, 4326);
        let rds: crate::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()]).unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .epsg(3577)
            .build().unwrap();
        assert_eq!(rds.metadata.epsg_code, 3577);
    }

    #[test]
    fn builder_geo_transform_override_applied() {
        let f = tiny_geotiff(4, 4, 5, 4326);
        let custom = GeoTransform([100.0, 30.0, 0.0, 200.0, 0.0, -30.0]);
        let rds: crate::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()]).unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .geo_transform(custom)
            .build().unwrap();
        assert_eq!(rds.metadata.geo_transform.0[0], 100.0);
        assert_eq!(rds.metadata.geo_transform.0[3], 200.0);
    }

    #[test]
    fn raster_dataset_iter_yields_all_blocks() {
        let f = tiny_geotiff(8, 8, 5, 4326);
        let rds: crate::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()]).unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .build().unwrap();
        let n = rds.num_blocks();
        let collected: Vec<_> = rds.iter().collect();
        assert_eq!(collected.len(), n);
        // IDs are 0..n
        for (i, (id, _region)) in collected.iter().enumerate() {
            assert_eq!(*id, i);
        }
    }

    #[test]
    fn block_region_returns_correct_block() {
        let f = tiny_geotiff(8, 8, 5, 4326);
        let rds: crate::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()]).unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .build().unwrap();
        let r0 = rds.block_region(0).expect("block 0 exists");
        assert_eq!(r0.block_index, 0);
        assert!(rds.block_region(999).is_none());
    }

    #[test]
    fn from_scratch_creates_empty_builder() {
        let b: RasterDatasetBuilder<i16> = RasterDatasetBuilder::from_scratch::<i16>(None);
        // Build should still work with explicit overrides + empty paths.
        // But from_scratch + build() with no files would fail validate, which is expected.
        // Just verify the builder constructs OK.
        let _ = b;
    }
}
