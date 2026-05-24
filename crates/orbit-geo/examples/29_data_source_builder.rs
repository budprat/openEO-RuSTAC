use orbit_geo::source::{DataSource, DataSourceBuilder};
fn main() {
    let ds: DataSource = DataSourceBuilder::new().file("/tmp/a.tif").file("/tmp/b.tif").build();
    println!("Example 29: DataSource paths: {}", ds.local_paths().len());
}
