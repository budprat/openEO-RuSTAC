//! **T3.7 — async_io tests** (feature-gated).
#![cfg(feature = "async-tiff")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::SpatialRef;
use gdal::DriverManager;
use orbit_geo::async_io::open_async;
use tempfile::{Builder, TempPath};

fn tiny_gtiff(rows: usize, cols: usize, fill: i16) -> TempPath {
    let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
    let temp_path = tmp.into_temp_path();
    std::fs::remove_file(&temp_path).ok();
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let options = RasterCreationOptions::from_iter(["TILED=YES", "BLOCKXSIZE=64", "BLOCKYSIZE=64"]);
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

/// **RED T3.7/A1+A2**: open a local GeoTIFF via async-tiff, get ≥1 IFD.
#[tokio::test]
async fn open_async_returns_tiff_with_at_least_one_ifd() {
    let path = tiny_gtiff(128, 128, 42);
    let tiff = open_async(&path).await.expect("open_async ok");
    assert!(!tiff.ifds().is_empty(), "TIFF must have at least one IFD");
}
