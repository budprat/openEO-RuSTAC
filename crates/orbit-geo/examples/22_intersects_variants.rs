use orbit_geo::dsl::Intersects;
use serde_json::json;
fn main() {
    let b = Intersects::Bbox([148.0, -29.0, 149.0, -28.0]);
    let s = Intersects::Scene(vec!["S2B_55HBV_20241225_0_L2A".into()]);
    let g = Intersects::Geometry(json!({"type": "Point", "coordinates": [148.5, -28.5]}));
    println!("bbox={:?}\nscene={:?}\ngeom={:?}", b.as_bbox(), s.as_scene(), g.as_geometry());
    println!("Example 22: Intersects variants demoed");
}
