//! **T4.7 bench: `apply` (no reduction)** against synthetic data.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::SpatialRef;
use gdal::DriverManager;
use ndarray::Array3;
use orbit_geo::{
    block::RasterDataBlock, builder::RasterDatasetBuilder, dataset::RasterDataset,
    types::{BlockSize, ImageResolution, RasterType},
    writer::BlockWriter,
};

use tempfile::{Builder, TempPath};

fn tiny_gtiff(rows: usize, cols: usize, fill: i16) -> TempPath {
    let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
    let p = tmp.into_temp_path();
    std::fs::remove_file(&p).ok();
    let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
    let opts = RasterCreationOptions::from_iter(["TILED=YES", "BLOCKXSIZE=128", "BLOCKYSIZE=128"]);
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

#[allow(dead_code)]
struct DiscardWriter;
impl<V: RasterType> BlockWriter<V> for DiscardWriter {
    fn write_block(&self, _d: &Array3<V>, _w: orbit_geo::types::ReadWindow) -> orbit_geo::Result<()> { Ok(()) }
}

fn bench_apply_512(c: &mut Criterion) {
    let f = tiny_gtiff(512, 512, 42);
    let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()]).unwrap()
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 128, cols: 128 })
        .build().unwrap();

    c.bench_function("apply/identity/512x512_128blk", |b| {
        b.iter(|| {
            let out = Builder::new().suffix(".tif").tempfile().unwrap().into_temp_path();
            std::fs::remove_file(&out).ok();
            rds.apply::<i16, _>(|rdb: &RasterDataBlock<i16>| {
                Array3::<i16>::from_elem((1, rdb.shape.rows, rdb.shape.cols), 7)
            }, black_box(1), &out).unwrap();
        })
    });
}

criterion_group!(benches, bench_apply_512);
criterion_main!(benches);
