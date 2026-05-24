use orbit_geo::providers::configure_anonymous_s3;
fn main() {
    configure_anonymous_s3();
    println!("Example 26: GDAL configured for anonymous S3 (AWS_NO_SIGN_REQUEST etc.)");
}
