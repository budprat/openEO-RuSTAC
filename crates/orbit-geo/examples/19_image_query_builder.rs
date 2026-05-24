use orbit_geo::dsl::{Collection, Cmp, ImageQueryBuilder, Intersects};
use orbit_geo::providers::Provider;
fn main() -> anyhow::Result<()> {
    let q = ImageQueryBuilder::new()
        .provider(Provider::EARTH_SEARCH_V1)
        .collection(Collection::Sentinel2)
        .intersects(Intersects::Bbox([148.0, -29.0, 149.0, -28.0]))
        .datetime("2024-01-01T00:00:00Z/2024-12-31T23:59:59Z")
        .cloudcover(Cmp::Less, 20.0)
        .limit(5)
        .build()?;
    println!("Example 19: query = {q:?}");
    Ok(())
}
