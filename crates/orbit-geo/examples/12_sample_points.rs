use orbit_geo::{builder::RasterDatasetBuilder, dataset::{LayerMapping, RasterDataset}, sampling::sample, types::{BlockSize, ImageResolution}};

fn tiny_gtiff(rows: usize, cols: usize, fill: i16) -> tempfile::TempPath {
    use gdal::raster::{Buffer, RasterCreationOptions};
    use gdal::spatial_ref::SpatialRef;
    use gdal::DriverManager;
    let tmp = tempfile::Builder::new().suffix(".tif").tempfile().unwrap();
    let p = tmp.into_temp_path();
    std::fs::remove_file(&p).ok();
    let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
    let opts = RasterCreationOptions::from_iter(["TILED=NO"]);
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

fn main() -> anyhow::Result<()> {
    let f = tiny_gtiff(4, 4, 77);
    let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()])?
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 4, cols: 4 })
        .build()?;
    rds.metadata.shape.times = 1; rds.metadata.shape.layers = 1;
    rds.layer_mappings = vec![LayerMapping { source: f.to_path_buf(), time_pos: 0, layer_pos: 0, band: 1 }];
    let pts = vec![(1.5, 2.5), (0.5, 3.5), (100.0, 100.0)];
    let vals = sample(&rds, &pts);
    println!("Example 12: sample results = {vals:?}");
    Ok(())
}
