//! Spatial / item-id filter for STAC searches.
//!
//! Pure-Rust enum — no `stac-client` dep — so it can be constructed without
//! enabling the `stac` feature. The `apply_to` conversion is feature-gated.

use serde_json::Value as JsonValue;

/// What to filter the STAC search by.
#[derive(Debug, Clone, PartialEq)]
pub enum Intersects {
    /// Bounding box: `[min_x, min_y, max_x, max_y]` (lon/lat in EPSG:4326).
    Bbox([f64; 4]),
    /// Explicit list of item IDs to retrieve (overrides spatial filters).
    Scene(Vec<String>),
    /// Arbitrary GeoJSON geometry to intersect with.
    Geometry(JsonValue),
}

impl Intersects {
    /// Returns `Some(&bbox)` if this is a `Bbox` variant.
    pub fn as_bbox(&self) -> Option<&[f64; 4]> {
        match self {
            Intersects::Bbox(b) => Some(b),
            _ => None,
        }
    }
    /// Returns `Some(&[String])` of item IDs if this is a `Scene` variant.
    pub fn as_scene(&self) -> Option<&[String]> {
        match self {
            Intersects::Scene(s) => Some(s),
            _ => None,
        }
    }
    /// Returns `Some(&JsonValue)` if this is a `Geometry` variant.
    pub fn as_geometry(&self) -> Option<&JsonValue> {
        match self {
            Intersects::Geometry(g) => Some(g),
            _ => None,
        }
    }
}

#[cfg(feature = "stac")]
impl Intersects {
    /// Apply this filter to a `stac_client::SearchParams` instance.
    pub fn apply_to(&self, params: &mut stac_client::SearchParams) {
        match self {
            Intersects::Bbox(b) => params.bbox = Some(b.to_vec()),
            Intersects::Scene(ids) => params.ids = Some(ids.clone()),
            Intersects::Geometry(g) => params.intersects = Some(g.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **RED T2.2/A1**: `Bbox(b).as_bbox() == Some(&b)`, others None.
    #[test]
    fn bbox_variant_has_bbox_only() {
        let b = Intersects::Bbox([148.0, -29.0, 149.0, -28.0]);
        assert_eq!(b.as_bbox(), Some(&[148.0, -29.0, 149.0, -28.0]));
        assert_eq!(b.as_scene(), None);
        assert_eq!(b.as_geometry(), None);
    }

    /// **RED T2.2/A2**: `Scene(v).as_scene() == Some(&v)`, others None.
    #[test]
    fn scene_variant_has_scene_only() {
        let ids = vec!["S2B_55HBV_20241225_0_L2A".to_string()];
        let s = Intersects::Scene(ids.clone());
        assert_eq!(s.as_scene(), Some(ids.as_slice()));
        assert_eq!(s.as_bbox(), None);
        assert_eq!(s.as_geometry(), None);
    }

    /// **RED T2.2/A3**: `Geometry(g).as_geometry() == Some(&g)`, others None.
    #[test]
    fn geometry_variant_has_geometry_only() {
        let g = json!({"type": "Point", "coordinates": [148.5, -28.5]});
        let geo = Intersects::Geometry(g.clone());
        assert_eq!(geo.as_geometry(), Some(&g));
        assert_eq!(geo.as_bbox(), None);
        assert_eq!(geo.as_scene(), None);
    }
}
