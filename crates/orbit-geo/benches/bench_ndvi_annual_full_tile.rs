//! **T4.7 bench: NDVI mean over 12 monthly synthetic tiles** at full-tile size.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::SpatialRef;
use gdal::DriverManager;
use ndarray::{s, Array2, Array3, Axis};
use orbit_geo::{
    block::RasterDataBlock, builder::RasterDatasetBuilder,
    dataset::{LayerMapping, RasterDataset},
    types::{BlockSize, Dimension, ImageResolution, RasterType},
    writer::BlockWriter,
};
use std::sync::Arc;
use tempfile::{Builder, TempPath};

fn tiny_gtiff(rows: usize, cols: usize, fill: i16) -> TempPath {
    let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
    let p = tmp.into_temp_path();
    std::fs::remove_file(&p).ok();
    let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
    let opts = RasterCreationOptions::from_iter(["TILED=YES", "BLOCKXSIZE=512", "BLOCKYSIZE=512"]);
    let mut ds = drv.create_with_band_type_with_options::<i16, _>(&p, cols, rows, 1, &opts).unwrap();
    ds.set_geo_transform(&[0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]).unwrap();
    ds.set_spatial_ref(&SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let mut band = ds.rasterband(1).unwrap();
    let data: Vec<i16> = vec![fill; rows * cols];
    let mut buf = Buffer::new((cols, rows), data);
    band.write::<i16>((0, 0), (cols, rows), &mut buf).unwrap();
    drop(band); drop(ds);
    p
}

struct DiscardWriter;
impl<V: RasterType> BlockWriter<V> for DiscardWriter {
    fn write_block(&self, _d: &Array3<V>, _w: orbit_geo::types::ReadWindow) -> orbit_geo::Result<()> { Ok(()) }
}

fn ndvi_mean(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> Array3<i16> {
    let (n_t, _, rows, cols) = rdb.data.dim();
    let mut sum = Array2::<f64>::zeros((rows, cols));
    for time_slice in rdb.data.axis_iter(Axis(0)) {
        let red = time_slice.slice(s![0, .., ..]).mapv(|e| e as f32);
        let nir = time_slice.slice(s![1, .., ..]).mapv(|e| e as f32);
        let denom = &nir + &red + 1e-10_f32;
        let ndvi_t = (&nir - &red) / &denom * 10_000.0_f32;
        for (acc, &v) in sum.iter_mut().zip(ndvi_t.iter()) { *acc += f64::from(v); }
    }
    sum.mapv(|s| (s / n_t as f64) as i16).insert_axis(Axis(0))
}

fn bench_ndvi_annual(c: &mut Criterion) {
    let reds: Vec<TempPath> = (0..12).map(|t| tiny_gtiff(2048, 2048, (100 + t) as i16)).collect();
    let nirs: Vec<TempPath> = (0..12).map(|t| tiny_gtiff(2048, 2048, (200 + t) as i16)).collect();
    let mut all_paths = Vec::new();
    all_paths.extend(reds.iter().map(|p| p.to_path_buf()));
    all_paths.extend(nirs.iter().map(|p| p.to_path_buf()));
    let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&all_paths).unwrap()
        .resolution(ImageResolution { x: 10.0, y: -10.0 })
        .block_size(BlockSize { rows: 512, cols: 512 })
        .build().unwrap();
    rds.metadata.shape.times = 12;
    rds.metadata.shape.layers = 2;
    let mut mappings = Vec::new();
    for (t, p) in reds.iter().enumerate() {
        mappings.push(LayerMapping { source: p.to_path_buf(), time_pos: t, layer_pos: 0, band: 1 });
    }
    for (t, p) in nirs.iter().enumerate() {
        mappings.push(LayerMapping { source: p.to_path_buf(), time_pos: t, layer_pos: 1, band: 1 });
    }
    rds.layer_mappings = mappings;
    let writer = Arc::new(DiscardWriter);

    c.bench_function("ndvi_annual/2048x2048_12t_512blk_4thr", |b| {
        b.iter(|| {
            rds.apply_reduction_to_writer::<i16, _, _>(
                Arc::clone(&writer), ndvi_mean, Dimension::Time, black_box(4),
            ).unwrap();
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).measurement_time(std::time::Duration::from_secs(20));
    targets = bench_ndvi_annual
}
criterion_main!(benches);
