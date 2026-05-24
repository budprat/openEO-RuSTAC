//! **STAC item-list helpers** — production STAC ergonomics ported from the upstream raster crate.
//!
//! All 7 functions operate on `Vec<stac::Item>` slices (or `stac_client::ItemCollection`)
//! and produce derived collections: asset hrefs, sorted datetimes, items for
//! a date, etc. Used by downstream pipelines that need to slice and filter
//! STAC search results before passing them to `RasterDatasetBuilder`.

#![cfg(feature = "stac")]

use stac::Item;
use std::path::PathBuf;

/// Get the unique asset names across a list of STAC items.
///
/// Useful for discovering what bands are available across a heterogeneous
/// search result (e.g. Landsat scenes with different processing levels).
pub fn get_asset_names(items: &[Item]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for item in items {
        for k in item.assets.keys() {
            names.insert(k.clone());
        }
    }
    names.into_iter().collect()
}

/// Get the asset href for a specific (item, asset_name).
///
/// Returns `None` if the asset isn't present on this item.
pub fn get_asset_href(item: &Item, asset_name: &str) -> Option<String> {
    item.assets.get(asset_name).map(|a| a.href.clone())
}

/// Get all asset paths/hrefs across a list of items for a given asset name.
///
/// Useful for building a multi-timestep dataset from N items, where each
/// item contributes one band at the same name (e.g. "red" across 9 scenes).
pub fn get_sources_for_asset(items: &[Item], asset_name: &str) -> Vec<PathBuf> {
    items
        .iter()
        .filter_map(|item| get_asset_href(item, asset_name).map(PathBuf::from))
        .collect()
}

/// Sort items by their `datetime` property.
///
/// Returns a vec of `(item_index, datetime_string)` pairs in chronological
/// order. Items without `datetime` are filtered out.
pub fn get_sorted_datetimes(items: &[Item]) -> Vec<(usize, String)> {
    let mut pairs: Vec<(usize, String)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            item.properties
                .additional_fields
                .get("datetime")
                .and_then(|v| v.as_str())
                .map(|s| (i, s.to_string()))
        })
        .collect();
    pairs.sort_by(|a, b| a.1.cmp(&b.1));
    pairs
}

/// Return all items whose `datetime` exactly equals `date_iso` (string match).
pub fn get_items_for_date<'a>(items: &'a [Item], date_iso: &str) -> Vec<&'a Item> {
    items
        .iter()
        .filter(|item| {
            item.properties
                .additional_fields
                .get("datetime")
                .and_then(|v| v.as_str())
                == Some(date_iso)
        })
        .collect()
}

/// Deduplicate datetimes within a date range; returns sorted unique values.
pub fn unique_datetimes_in_range(
    items: &[Item],
    start_iso: &str,
    end_iso: &str,
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        if let Some(dt) = item
            .properties
            .additional_fields
            .get("datetime")
            .and_then(|v| v.as_str())
        {
            if dt >= start_iso && dt <= end_iso {
                seen.insert(dt.to_string());
            }
        }
    }
    seen.into_iter().collect()
}

/// Swap (x, y) coordinates in a GeoJSON-style polygon ring.
///
/// Many STAC bbox/geometries are `[lon, lat]` ordered; some tools expect
/// `[lat, lon]`. This helper flips an array of `[x, y]` pairs in place.
pub fn swap_coordinates(coords: &[[f64; 2]]) -> Vec<[f64; 2]> {
    coords.iter().map(|c| [c[1], c[0]]).collect()
}

/// Filter items by tile id (e.g. Sentinel-2 MGRS tile in `mgrs:utm_zone`/`utm_band`/`grid_square`).
pub fn filter_items_by_tile(items: Vec<Item>, tile_id: &str) -> Vec<Item> {
    items
        .into_iter()
        .filter(|item| {
            // S2 L2A on Earth Search has "mgrs:utm_zone", "mgrs:latitude_band", "mgrs:grid_square"
            let zone = item
                .properties
                .additional_fields
                .get("mgrs:utm_zone")
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .map(|z| z.to_string());
            let band = item
                .properties
                .additional_fields
                .get("mgrs:latitude_band")
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            let square = item
                .properties
                .additional_fields
                .get("mgrs:grid_square")
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            if let (Some(z), Some(b), Some(s)) = (zone, band, square) {
                format!("{z}{b}{s}") == tile_id
            } else {
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_item(id: &str, datetime: &str, assets: &[(&str, &str)]) -> Item {
        let mut item = Item::new(id);
        item.properties
            .additional_fields
            .insert("datetime".into(), json!(datetime));
        for (k, href) in assets {
            item.assets.insert(
                (*k).to_string(),
                stac::Asset::new((*href).to_string()),
            );
        }
        item
    }

    #[test]
    fn get_asset_names_collects_distinct() {
        let items = vec![
            fake_item("a", "2024-01-01T00:00:00Z", &[("red", "x"), ("nir", "y")]),
            fake_item("b", "2024-02-01T00:00:00Z", &[("red", "p"), ("swir1", "q")]),
        ];
        let names = get_asset_names(&items);
        assert_eq!(names, vec!["nir".to_string(), "red".into(), "swir1".into()]);
    }

    #[test]
    fn get_asset_href_returns_specific_url() {
        let item = fake_item("a", "2024-01-01T00:00:00Z", &[("red", "https://x/B04.tif")]);
        assert_eq!(
            get_asset_href(&item, "red"),
            Some("https://x/B04.tif".into())
        );
        assert_eq!(get_asset_href(&item, "missing"), None);
    }

    #[test]
    fn get_sources_for_asset_collects_across_items() {
        let items = vec![
            fake_item("a", "2024-01-01T00:00:00Z", &[("red", "/p1/B04.tif")]),
            fake_item("b", "2024-02-01T00:00:00Z", &[("red", "/p2/B04.tif")]),
        ];
        let srcs = get_sources_for_asset(&items, "red");
        assert_eq!(srcs.len(), 2);
        assert_eq!(srcs[0], PathBuf::from("/p1/B04.tif"));
    }

    #[test]
    fn get_sorted_datetimes_chronological() {
        let items = vec![
            fake_item("c", "2024-03-01T00:00:00Z", &[]),
            fake_item("a", "2024-01-01T00:00:00Z", &[]),
            fake_item("b", "2024-02-01T00:00:00Z", &[]),
        ];
        let sorted = get_sorted_datetimes(&items);
        assert_eq!(sorted[0].1, "2024-01-01T00:00:00Z");
        assert_eq!(sorted[1].1, "2024-02-01T00:00:00Z");
        assert_eq!(sorted[2].1, "2024-03-01T00:00:00Z");
    }

    #[test]
    fn get_items_for_date_exact_match() {
        let items = vec![
            fake_item("a", "2024-01-01T00:00:00Z", &[]),
            fake_item("b", "2024-01-01T00:00:00Z", &[]),
            fake_item("c", "2024-02-01T00:00:00Z", &[]),
        ];
        let on_jan = get_items_for_date(&items, "2024-01-01T00:00:00Z");
        assert_eq!(on_jan.len(), 2);
    }

    #[test]
    fn unique_datetimes_in_range_dedups_and_filters() {
        let items = vec![
            fake_item("a", "2024-01-01T00:00:00Z", &[]),
            fake_item("b", "2024-01-01T00:00:00Z", &[]), // dup
            fake_item("c", "2024-06-01T00:00:00Z", &[]),
            fake_item("d", "2025-01-01T00:00:00Z", &[]), // out of range
        ];
        let unique = unique_datetimes_in_range(
            &items,
            "2024-01-01T00:00:00Z",
            "2024-12-31T23:59:59Z",
        );
        assert_eq!(unique.len(), 2);
        assert_eq!(unique[0], "2024-01-01T00:00:00Z");
    }

    #[test]
    fn swap_coordinates_flips_xy() {
        let lonlat = [[148.0, -29.0], [149.0, -28.0]];
        let latlon = swap_coordinates(&lonlat);
        assert_eq!(latlon, vec![[-29.0, 148.0], [-28.0, 149.0]]);
    }

    /// Suppress unused warning on `filter_items_by_tile` — minimal test.
    #[test]
    fn filter_items_by_tile_smoke() {
        let _ = filter_items_by_tile(Vec::new(), "55HBV");
    }
}
