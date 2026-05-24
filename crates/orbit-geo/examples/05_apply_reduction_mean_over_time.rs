use ndarray::{s, Array2, Axis};
use orbit_geo::{block::RasterDataBlock, builder::RasterDatasetBuilder, dataset::{LayerMapping, RasterDataset}, types::{BlockSize, Dimension, ImageResolution}};

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
    let scenes: Vec<_> = (0..3).map(|t| tiny_gtiff(4, 4, (50 + t) as i16)).collect();
    let paths: Vec<_> = scenes.iter().map(|p| p.to_path_buf()).collect();
    let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&paths)?
        .resolution(ImageResolution { x: 1.0, y: -1.0 })
        .block_size(BlockSize { rows: 4, cols: 4 })
        .build()?;
    rds.metadata.shape.times = 3; rds.metadata.shape.layers = 1;
    rds.layer_mappings = paths.iter().enumerate().map(|(t, p)| LayerMapping { source: p.clone(), time_pos: t, layer_pos: 0, band: 1 }).collect();
    let out = tempfile::Builder::new().suffix(".tif").tempfile()?.into_temp_path();
    std::fs::remove_file(&out).ok();
    rds.apply_reduction::<i16, _>(|rdb: &RasterDataBlock<i16>, _dim: Dimension| {
        let mut sum: Array2<f64> = Array2::zeros((rdb.shape.rows, rdb.shape.cols));
        for ts in rdb.data.axis_iter(Axis(0)) {
            let layer = ts.slice(s![0, .., ..]).mapv(|v| v as f64);
            sum = &sum + &layer;
        }
        let mean = sum.mapv(|s| (s / rdb.shape.times as f64) as i16);
        mean.insert_axis(Axis(0))
    }, Dimension::Time, 1, &out, i16::MIN)?;
    println!("Example 05: mean over time → {}", out.display());
    Ok(())
}
