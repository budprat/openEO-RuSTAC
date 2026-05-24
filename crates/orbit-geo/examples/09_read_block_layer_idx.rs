use orbit_geo::{builder::RasterDatasetBuilder, dataset::{LayerMapping, RasterDataset}, types::{BlockSize, ImageResolution}};

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
    let red = tiny_gtiff(4, 4, 100);
    let nir = tiny_gtiff(4, 4, 200);
    let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[red.to_path_buf(), nir.to_path_buf()])?
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 4, cols: 4 })
        .build()?;
    rds.metadata.shape.times = 1; rds.metadata.shape.layers = 2;
    rds.layer_mappings = vec![
        LayerMapping { source: red.to_path_buf(), time_pos: 0, layer_pos: 0, band: 1 },
        LayerMapping { source: nir.to_path_buf(), time_pos: 0, layer_pos: 1, band: 1 },
    ];
    let block = rds.read_block_layer_idx(0, 1)?;
    println!("Example 09: layer 1 (nir) value = {}", block.data[[0, 0, 0, 0]]);
    Ok(())
}
