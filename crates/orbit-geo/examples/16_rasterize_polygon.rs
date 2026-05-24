use orbit_geo::rasterization::rasterize;
fn main() -> anyhow::Result<()> {
    let geo = tempfile::Builder::new().suffix(".geojson").tempfile()?;
    let geo_path = geo.into_temp_path();
    std::fs::write(&geo_path, r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]},"properties":{}}]}"#)?;
    let out = tempfile::Builder::new().suffix(".tif").tempfile()?.into_temp_path();
    std::fs::remove_file(&out).ok();
    rasterize(&geo_path, &out, 4, 4, (0.0, 0.0, 4.0, 4.0), 1.0, 0.0)?;
    println!("Example 16: rasterized polygon → {}", out.display());
    Ok(())
}
