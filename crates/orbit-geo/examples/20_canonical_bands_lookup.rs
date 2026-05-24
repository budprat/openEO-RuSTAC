use orbit_geo::dsl::{canonical_bands, Collection};
fn main() -> anyhow::Result<()> {
    let red_s2 = canonical_bands("red", Collection::Sentinel2)?;
    let red_l8 = canonical_bands("red", Collection::Landsat8)?;
    println!("Example 20: 'red' on Sentinel-2 = {red_s2}, on Landsat-8 = {red_l8}");
    Ok(())
}
