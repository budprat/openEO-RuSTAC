//! **T0.2 — Baseline Criterion bench for `apply_reduction_to_writer` with an
//! NDVI-mean-over-time worker against synthetic 2-layer (red + nir) data.**
//!
//! Reproducible: no network, no external data. Output captured in
//! `BENCHMARK_BASELINE.md`. Bench parameters live below.
//!
//! Run with: `cargo bench -p orbit-geo --bench apply_reduction`

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::SpatialRef;
use gdal::DriverManager;
use ndarray::{s, Array2, Array3, Axis};
use orbit_geo::{
    block::RasterDataBlock,
    builder::RasterDatasetBuilder,
    dataset::{LayerMapping, RasterDataset},
    types::{BlockSize, Dimension, ImageResolution, RasterType},
    writer::BlockWriter,
    Result,
};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::{Builder, TempPath};

// ─────────────────────────────────────────────────────────────────────────────
// Worker (duplicated from test_support tier0_t02_tests::ndvi_mean_worker
// because test_support is cfg(test)-only and not visible to benches).
// ─────────────────────────────────────────────────────────────────────────────

fn ndvi_mean_worker(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> Array3<i16> {
    let (n_times, _layers, rows, cols) = rdb.data.dim();
    let mut sum = Array2::<f64>::zeros((rows, cols));
    for time_slice in rdb.data.axis_iter(Axis(0)) {
        let red = time_slice.slice(s![0, .., ..]).mapv(|e| e as f32);
        let nir = time_slice.slice(s![1, .., ..]).mapv(|e| e as f32);
        let denom = &nir + &red + 1e-10_f32;
        let ndvi_t = (&nir - &red) / &denom * 10_000.0_f32;
        for (acc, &v) in sum.iter_mut().zip(ndvi_t.iter()) {
            *acc += f64::from(v);
        }
    }
    sum.mapv(|s| (s / n_times as f64) as i16).insert_axis(Axis(0))
}

// ─────────────────────────────────────────────────────────────────────────────
// In-bench fixtures (similar to test_support but bench-scope).
// ─────────────────────────────────────────────────────────────────────────────

fn tiny_geotiff(rows: usize, cols: usize, fill: i16) -> TempPath {
    let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
    let temp_path = tmp.into_temp_path();
    std::fs::remove_file(&temp_path).ok();
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let options = RasterCreationOptions::from_iter(["TILED=YES", "BLOCKXSIZE=128", "BLOCKYSIZE=128", "COMPRESS=NONE"]);
    let mut ds = driver
        .create_with_band_type_with_options::<i16, _>(&temp_path, cols, rows, 1, &options)
        .unwrap();
    ds.set_geo_transform(&[0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]).unwrap();
    let sr = SpatialRef::from_epsg(4326).unwrap();
    ds.set_spatial_ref(&sr).unwrap();
    let mut band = ds.rasterband(1).unwrap();
    let data: Vec<i16> = vec![fill; rows * cols];
    let mut buf = Buffer::new((cols, rows), data);
    band.write::<i16>((0, 0), (cols, rows), &mut buf).unwrap();
    drop(band);
    drop(ds);
    temp_path
}

fn dataset_red_nir(n_times: usize, rows: usize, cols: usize, block: usize) -> (RasterDataset<i16>, Vec<TempPath>) {
    let mut reds = Vec::with_capacity(n_times);
    let mut nirs = Vec::with_capacity(n_times);
    for t in 0..n_times {
        reds.push(tiny_geotiff(rows, cols, (100 + t) as i16));
        nirs.push(tiny_geotiff(rows, cols, (200 + t) as i16));
    }
    let mut all_paths: Vec<PathBuf> = Vec::new();
    all_paths.extend(reds.iter().map(|p| p.to_path_buf()));
    all_paths.extend(nirs.iter().map(|p| p.to_path_buf()));

    let mut rds = RasterDatasetBuilder::from_files(&all_paths)
        .unwrap()
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: block, cols: block })
        .build()
        .unwrap();
    rds.metadata.shape.times = n_times;
    rds.metadata.shape.layers = 2;
    let mut mappings = Vec::with_capacity(n_times * 2);
    for (t, r) in reds.iter().enumerate() {
        mappings.push(LayerMapping { source: r.to_path_buf(), time_pos: t, layer_pos: 0, band: 1 });
    }
    for (t, n) in nirs.iter().enumerate() {
        mappings.push(LayerMapping { source: n.to_path_buf(), time_pos: t, layer_pos: 1, band: 1 });
    }
    rds.layer_mappings = mappings;
    let mut live: Vec<TempPath> = Vec::new();
    live.extend(reds);
    live.extend(nirs);
    (rds, live)
}

// ─────────────────────────────────────────────────────────────────────────────
// In-memory BlockWriter so the bench measures read + compute, NOT disk I/O.
// ─────────────────────────────────────────────────────────────────────────────

struct DiscardWriter;
impl<V: RasterType> BlockWriter<V> for DiscardWriter {
    fn write_block(&self, _data: &Array3<V>, _window: orbit_geo::types::ReadWindow) -> Result<()> {
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bench definitions
// ─────────────────────────────────────────────────────────────────────────────

fn bench_ndvi_mean_small(c: &mut Criterion) {
    // 9 timesteps × 256×256 with 128×128 blocks (4 blocks).
    let (rds, _live) = dataset_red_nir(9, 256, 256, 128);
    let writer = Arc::new(DiscardWriter);

    c.bench_function("apply_reduction_to_writer/ndvi_mean/256x256_9t_128blk", |b| {
        b.iter(|| {
            rds.apply_reduction_to_writer::<i16, _, _>(
                Arc::clone(&writer),
                ndvi_mean_worker,
                Dimension::Time,
                black_box(1),
            )
            .unwrap();
        })
    });
}

fn bench_ndvi_mean_medium(c: &mut Criterion) {
    // 9 timesteps × 1024×1024 with 256×256 blocks (16 blocks).
    let (rds, _live) = dataset_red_nir(9, 1024, 1024, 256);
    let writer = Arc::new(DiscardWriter);

    c.bench_function("apply_reduction_to_writer/ndvi_mean/1024x1024_9t_256blk", |b| {
        b.iter(|| {
            rds.apply_reduction_to_writer::<i16, _, _>(
                Arc::clone(&writer),
                ndvi_mean_worker,
                Dimension::Time,
                black_box(4),
            )
            .unwrap();
        })
    });
}

criterion_group!(benches, bench_ndvi_mean_small, bench_ndvi_mean_medium);
criterion_main!(benches);
