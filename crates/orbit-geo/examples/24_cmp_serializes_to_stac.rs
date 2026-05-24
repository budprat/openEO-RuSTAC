use orbit_geo::dsl::{cloudcover_filter, Cmp};
fn main() {
    let (key, pred) = cloudcover_filter(Cmp::Less, 20.0);
    println!("STAC query property = {key}");
    println!("STAC query predicate = {pred}");
    println!("Example 24: cloudcover filter demoed");
}
