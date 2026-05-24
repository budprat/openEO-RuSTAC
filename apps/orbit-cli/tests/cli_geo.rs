//! **Tier 4 — CLI geo subcommand integration tests.**
//!
//! Run with: `cargo test -p orbit-cli --test cli_geo`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::SpatialRef;
use gdal::DriverManager;
use tempfile::{Builder, TempPath};

fn tiny_gtiff(rows: usize, cols: usize, fill: i16) -> TempPath {
    let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
    let temp_path = tmp.into_temp_path();
    std::fs::remove_file(&temp_path).ok();
    let driver = DriverManager::get_driver_by_name("GTiff").unwrap();
    let opts = RasterCreationOptions::from_iter(["TILED=NO"]);
    let mut ds = driver
        .create_with_band_type_with_options::<i16, _>(&temp_path, cols, rows, 1, &opts)
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

/// **T4.1**: `orbit geo rasterize` produces a valid raster.
#[test]
fn cli_geo_rasterize_burns_polygon() {
    // Write GeoJSON polygon.
    let geo = Builder::new().suffix(".geojson").tempfile().unwrap();
    let geo_path = geo.into_temp_path();
    std::fs::write(
        &geo_path,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]},"properties":{}}]}"#,
    )
    .unwrap();

    let out = Builder::new().suffix(".tif").tempfile().unwrap();
    let out_path = out.into_temp_path();
    std::fs::remove_file(&out_path).ok();

    Command::cargo_bin("orbit")
        .unwrap()
        .args([
            "geo", "rasterize",
            "--vector", geo_path.to_str().unwrap(),
            "--output", out_path.to_str().unwrap(),
            "--width", "4", "--height", "4",
            "--bbox", "0", "0", "4", "4",
            "--burn-value", "1.0",
            "--no-data", "0.0",
        ])
        .assert()
        .success();
    assert!(out_path.exists(), "output raster must exist");
}

/// **T4.2**: `orbit geo mosaic` combines inputs into a single GeoTIFF.
#[test]
fn cli_geo_mosaic_combines_inputs() {
    let a = tiny_gtiff(2, 2, 50);
    let b = tiny_gtiff(2, 2, 99);

    let out = Builder::new().suffix(".tif").tempfile().unwrap();
    let out_path = out.into_temp_path();
    std::fs::remove_file(&out_path).ok();

    Command::cargo_bin("orbit")
        .unwrap()
        .args([
            "geo", "mosaic",
            "--inputs", a.to_str().unwrap(), b.to_str().unwrap(),
            "--output", out_path.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(out_path.exists(), "mosaic output must exist");

    let ds = gdal::Dataset::open(&*out_path).expect("reopen");
    assert_eq!(ds.raster_count(), 1, "single-band mosaic output");
}

/// **T4.3**: `orbit geo sample` returns pixel value at geo coordinates.
#[test]
fn cli_geo_sample_returns_pixel_value() {
    let raster = tiny_gtiff(4, 4, 42);

    let assert = Command::cargo_bin("orbit")
        .unwrap()
        .args([
            "geo", "sample",
            "--raster", raster.to_str().unwrap(),
            "--x", "1.5",
            "--y", "2.5",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("42"), "stdout should contain pixel value 42, got: {stdout}");
}

/// **T4.4**: `orbit geo warp` reprojects to target EPSG.
#[test]
fn cli_geo_warp_reprojects_to_target_epsg() {
    let src = tiny_gtiff(4, 4, 42);
    let dst = Builder::new().suffix(".tif").tempfile().unwrap();
    let dst_path = dst.into_temp_path();
    std::fs::remove_file(&dst_path).ok();

    Command::cargo_bin("orbit")
        .unwrap()
        .args([
            "geo", "warp",
            "--input", src.to_str().unwrap(),
            "--output", dst_path.to_str().unwrap(),
            "--target-epsg", "3577",
        ])
        .assert()
        .success();

    let ds = gdal::Dataset::open(&*dst_path).expect("reopen warped");
    let sr = ds.spatial_ref().expect("has SR");
    assert_eq!(sr.auth_code().expect("auth"), 3577, "warped to EPSG:3577");
}

/// **T4.5**: `orbit geo get-imagery --urls FILE` rewrites HTTP(S)/S3 to /vsi paths.
#[test]
fn cli_geo_get_imagery_rewrites_urls() {
    let urls_file = Builder::new().suffix(".txt").tempfile().unwrap();
    let urls_path = urls_file.into_temp_path();
    std::fs::write(
        &urls_path,
        "https://sentinel-cogs.s3.us-west-2.amazonaws.com/foo/B04.tif\n\
         s3://sentinel-cogs/bar/B08.tif\n",
    )
    .unwrap();

    let assert = Command::cargo_bin("orbit")
        .unwrap()
        .args(["geo", "get-imagery", "--urls", urls_path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("/vsicurl/"), "HTTPS should become /vsicurl/");
    assert!(stdout.contains("/vsis3/"), "s3:// should become /vsis3/");
}
