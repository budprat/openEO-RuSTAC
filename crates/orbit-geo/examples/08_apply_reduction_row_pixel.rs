use ndarray::{Array1, ArrayView3};
use orbit_geo::{builder::RasterDatasetBuilder, dataset::RasterDataset, types::{BlockSize, ImageResolution}};

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

struct DiscardWriter;
impl orbit_geo::writer::BlockWriter<i16> for DiscardWriter {
    fn write_block(&self, _data: &ndarray::Array3<i16>, _w: orbit_geo::types::ReadWindow) -> orbit_geo::Result<()> {
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    let d = tiny_gtiff(4, 4, 5);
    let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[d.to_path_buf()])?
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 2, cols: 2 })
        .build()?;
    let writer = std::sync::Arc::new(DiscardWriter);
    rds.apply_reduction_row_pixel_to_writer::<i16, _, _>(writer, |_row: ArrayView3<i16>| {
        Array1::<i16>::from_elem(2, 42)
    }, 1)?;
    println!("Example 08: row-pixel reduction OK");
    Ok(())
}
