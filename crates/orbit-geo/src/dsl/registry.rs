//! Product registry: maps **canonical** band names (`"red"`, `"nir"`, etc.)
//! to provider-specific asset names (`"B04"`, `"SR_B4"`, etc.).
//!
//! The registry is currently hard-coded for the 4 collections × 3 most-common
//! bands. A future revision may load from YAML at compile time
//! (`include_str!("products.yaml")` + serde_yaml parse) for extensibility.

use crate::dsl::Collection;
use crate::error::{Error, Result};

/// Resolve a canonical band name to provider-specific asset name.
pub fn canonical_bands(name: &str, collection: Collection) -> Result<&'static str> {
    let resolved: Option<&'static str> = match (collection, name) {
        (Collection::Sentinel2, "blue") => Some("B02"),
        (Collection::Sentinel2, "green") => Some("B03"),
        (Collection::Sentinel2, "red") => Some("B04"),
        (Collection::Sentinel2, "nir") => Some("B08"),
        (Collection::Sentinel2, "swir1") => Some("B11"),
        (Collection::Sentinel2, "swir2") => Some("B12"),
        (Collection::Landsat8, "red") => Some("red"),
        (Collection::Landsat8, "green") => Some("green"),
        (Collection::Landsat8, "blue") => Some("blue"),
        (Collection::Landsat8, "nir") => Some("nir08"),
        (Collection::Landsat8, "swir1") => Some("swir16"),
        (Collection::Landsat8, "swir2") => Some("swir22"),
        (Collection::Landsat7 | Collection::Landsat5, "red") => Some("red"),
        (Collection::Landsat7 | Collection::Landsat5, "green") => Some("green"),
        (Collection::Landsat7 | Collection::Landsat5, "blue") => Some("blue"),
        (Collection::Landsat7 | Collection::Landsat5, "nir") => Some("nir08"),
        _ => None,
    };
    resolved.ok_or_else(|| {
        Error::Other(format!("canonical_bands: unknown ({name:?}, {collection:?})"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **RED T2.5/A1**: `red` on Sentinel-2 → `B04`.
    #[test]
    fn canonical_red_on_sentinel2_resolves_to_b04() {
        assert_eq!(canonical_bands("red", Collection::Sentinel2).unwrap(), "B04");
    }

    /// **RED T2.5/A2**: `nir` on Sentinel-2 → `B08`.
    #[test]
    fn canonical_nir_on_sentinel2_resolves_to_b08() {
        assert_eq!(canonical_bands("nir", Collection::Sentinel2).unwrap(), "B08");
    }

    /// **RED T2.5/A3**: `red` on Landsat-8 (Earth Search) → `red`.
    #[test]
    fn canonical_red_on_landsat8_resolves_to_red() {
        assert_eq!(canonical_bands("red", Collection::Landsat8).unwrap(), "red");
    }

    /// **RED T2.5/A4**: unknown band → Err.
    #[test]
    fn canonical_unknown_band_errors() {
        let r = canonical_bands("not_a_band", Collection::Sentinel2);
        assert!(r.is_err());
    }
}
