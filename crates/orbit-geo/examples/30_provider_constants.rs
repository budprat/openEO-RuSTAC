use orbit_geo::providers::Provider;
fn main() {
    println!("Earth Search v1: {}", Provider::EARTH_SEARCH_V1);
    println!("Planetary Computer: {}", Provider::PLANETARY_COMPUTER);
    println!("USGS Landsat Look: {}", Provider::USGS_LANDSAT_LOOK);
    println!("DEA: {}", Provider::DEA);
    println!("Example 30: 4 provider endpoints demoed");
}
