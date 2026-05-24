use orbit_geo::dsl::Collection;
use orbit_geo::providers::Provider;
fn main() {
    println!("Sentinel-2 on Earth Search v1: {}", Collection::Sentinel2.id_for(Provider::EARTH_SEARCH_V1));
    println!("Sentinel-2 on Planetary Computer: {}", Collection::Sentinel2.id_for(Provider::PLANETARY_COMPUTER));
    println!("Sentinel-2 on DEA: {}", Collection::Sentinel2.id_for(Provider::DEA));
    println!("Example 21: provider-portable collection IDs demoed");
}
