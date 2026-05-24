//! Typed STAC accessors — thin wrapper that lifts raw `serde_json::Value`
//! STAC responses into `rustac` typed `Item`s when the `geo-kernel`
//! feature is enabled.
//!
//! Why a wrapper? The hand-rolled `HttpStacSearcher` returns its scenes
//! through field-picking on `serde_json::Value`. That's fine for the
//! red/nir/scl asset pickers but loses the rest of the STAC item — eo
//! extension bands, proj epsg, datetime, geometry. With the `stac`
//! feature on, the same JSON can be lifted into `stac::Item` and queried
//! via `orbit_geo::stac_helpers::*` (which already exists in the kernel).
//!
//! Build slim? Without `--features geo-kernel`, this module is absent
//! and the orbit-openeo arithmetic build never sees the rustac deps.

use serde_json::Value;

use crate::executor::ExecError;

/// Parse a raw STAC item JSON document into a typed `stac::Item`.
pub fn parse_item(v: &Value) -> Result<orbit_geo::stac::Item, ExecError> {
    serde_json::from_value::<orbit_geo::stac::Item>(v.clone())
        .map_err(|e| ExecError::InvalidGraph(format!("STAC Item decode: {e}")))
}

/// Look up an asset href by name through `orbit_geo::stac_helpers`. Returns
/// `None` if the asset key is absent.
#[must_use]
pub fn asset_href(item: &orbit_geo::stac::Item, asset_name: &str) -> Option<String> {
    orbit_geo::stac_helpers::get_asset_href(item, asset_name)
}

/// Try a list of asset key aliases and return the first matching href.
/// Mirrors the hand-rolled `pick_asset_href` from `geo_executor` but
/// against the typed `Item` instead of raw JSON.
#[must_use]
pub fn pick_typed_asset_href(item: &orbit_geo::stac::Item, candidates: &[&str]) -> Option<String> {
    for k in candidates {
        if let Some(h) = asset_href(item, k) {
            return Some(h);
        }
    }
    None
}

/// Extract every asset name declared on the item.
#[must_use]
pub fn asset_names(items: &[orbit_geo::stac::Item]) -> Vec<String> {
    orbit_geo::stac_helpers::get_asset_names(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_item_json() -> Value {
        json!({
            "type": "Feature",
            "stac_version": "1.0.0",
            "id": "S2A_TEST_0001",
            "geometry": null,
            "properties": { "datetime": "2024-06-15T10:00:00Z" },
            "links": [],
            "assets": {
                "red":   { "href": "https://example.com/B04.tif",
                           "type": "image/tiff; application=geotiff",
                           "roles": ["data"] },
                "nir":   { "href": "https://example.com/B08.tif",
                           "roles": ["data"] },
                "scl":   { "href": "https://example.com/SCL.tif",
                           "roles": ["mask"] }
            }
        })
    }

    #[test]
    fn parse_item_lifts_value_into_typed_item() {
        let item = parse_item(&sample_item_json()).unwrap();
        assert_eq!(item.id, "S2A_TEST_0001");
        // The properties datetime (chrono `DateTime<Utc>`) survives.
        let dt = item.properties.datetime;
        assert!(dt.is_some(), "datetime should be present");
        assert_eq!(dt.unwrap().to_rfc3339()[..10], *"2024-06-15");
    }

    #[test]
    fn parse_item_rejects_malformed_json() {
        let bad = json!({ "type": "NotAFeature" });
        assert!(parse_item(&bad).is_err());
    }

    #[test]
    fn asset_href_returns_matching_url() {
        let item = parse_item(&sample_item_json()).unwrap();
        assert_eq!(
            asset_href(&item, "red"),
            Some("https://example.com/B04.tif".to_string())
        );
    }

    #[test]
    fn asset_href_returns_none_for_unknown_key() {
        let item = parse_item(&sample_item_json()).unwrap();
        assert_eq!(asset_href(&item, "swir22"), None);
    }

    #[test]
    fn pick_typed_asset_href_tries_aliases_in_order() {
        let item = parse_item(&sample_item_json()).unwrap();
        assert_eq!(
            pick_typed_asset_href(&item, &["nope", "red", "B04"]),
            Some("https://example.com/B04.tif".to_string())
        );
    }

    #[test]
    fn asset_names_collects_keys_across_items() {
        let item = parse_item(&sample_item_json()).unwrap();
        let names = asset_names(&[item]);
        assert!(names.contains(&"red".to_string()));
        assert!(names.contains(&"nir".to_string()));
        assert!(names.contains(&"scl".to_string()));
    }
}
