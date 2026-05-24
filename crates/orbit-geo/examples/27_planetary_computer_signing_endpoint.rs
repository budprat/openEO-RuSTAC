use orbit_geo::providers::planetary_computer_sign_endpoint;
fn main() {
    let url = "https://sentinel2l2a01.blob.core.windows.net/foo/B04.tif";
    let signed = planetary_computer_sign_endpoint(url);
    println!("Sign endpoint: {signed}");
    println!("Example 27: PC SAS-signing endpoint construction demoed");
}
