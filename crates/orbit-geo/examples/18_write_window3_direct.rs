use ndarray::Array3;
use orbit_geo::{types::GeoTransform, writer::ParallelGeoTiffWriter};
fn main() -> anyhow::Result<()> {
    let out = tempfile::Builder::new().suffix(".tif").tempfile()?.into_temp_path();
    std::fs::remove_file(&out).ok();
    let writer = ParallelGeoTiffWriter::create::<i16>(
        &out, &GeoTransform([0.0, 1.0, 0.0, 4.0, 0.0, -1.0]), 4326, 4, 4, 1, -1_i16,
    )?;
    let data: Array3<i16> = Array3::from_elem((1, 2, 2), 99);
    writer.write_window3(data.view(), 1, 1)?;
    println!("Example 18: write_window3 → {}", out.display());
    Ok(())
}
