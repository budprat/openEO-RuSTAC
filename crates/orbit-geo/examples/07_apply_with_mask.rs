use ndarray::Array3;
use orbit_geo::{block::RasterDataBlock, builder::RasterDatasetBuilder, dataset::{LayerMapping, RasterDataset}, types::{BlockSize, ImageResolution}};

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
    let d = tiny_gtiff(4, 4, 5);
    let m = tiny_gtiff(4, 4, 1);
    let rds_d: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[d.to_path_buf()])?
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 4, cols: 4 })
        .build()?;
    let mut rds_m: RasterDataset<u8> = RasterDatasetBuilder::<u8>::from_files(&[m.to_path_buf()])?
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 4, cols: 4 })
        .build()?;
    rds_m.layer_mappings = vec![LayerMapping { source: m.to_path_buf(), time_pos: 0, layer_pos: 0, band: 1 }];
    let out = tempfile::Builder::new().suffix(".tif").tempfile()?.into_temp_path();
    std::fs::remove_file(&out).ok();
    rds_d.apply_with_mask::<u8, i16, _>(&rds_m, |_d: &RasterDataBlock<i16>, _m: &RasterDataBlock<u8>| {
        Array3::<i16>::from_elem((1, 4, 4), 7)
    }, 1, &out)?;
    println!("Example 07: apply_with_mask → {}", out.display());
    Ok(())
}
