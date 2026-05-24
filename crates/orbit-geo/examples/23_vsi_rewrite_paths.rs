use orbit_geo::providers::vsi_rewrite;
fn main() {
    let https = "https://example.com/foo.tif";
    let s3 = "s3://bucket/foo.tif";
    println!("https → {}", vsi_rewrite(https));
    println!("s3 → {}", vsi_rewrite(s3));
    println!("Example 23: VSI path rewriting demoed");
}
