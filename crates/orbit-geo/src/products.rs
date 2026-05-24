//! Product registry — typed catalog of known Earth-observation product
//! types (Sentinel-2 L2A, Landsat C2 L2, …) shared across the orbit-rs
//! workspace.
//!
//! Originally a `routes::products` module inside `orbit-openeo`; promoted
//! here so non-HTTP consumers (CLI, future gRPC server) can reuse the
//! same catalog without depending on the openEO façade. The openEO
//! crate keeps its `/products` endpoint by re-exporting [`Product`] +
//! [`known_products`].

use serde::{Deserialize, Serialize};

/// One known product entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Product {
    /// Stable product id (matches the openEO `collection` id where possible).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Source platform / instrument tag.
    pub platform: String,
    /// Native resolution in metres (best of the bundled bands).
    pub native_resolution_m: f64,
    /// Mask classifier the geo executor recognises for this product.
    pub mask_kind: MaskKind,
    /// Asset key aliases that resolve to the red/nir/swir2/scl bands.
    pub band_aliases: BandAliases,
}

/// Mask classifier the geo executor will use for this product.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskKind {
    /// Sentinel-2 Sen2Cor Scene Classification Layer (u8 classes).
    Sentinel2Scl,
    /// Landsat Collection 2 QA_PIXEL (u16 bitfield).
    LandsatC2QaPixel,
    /// No bundled mask — fall back to brightness `cloud_mask::classify`.
    None,
}

/// Asset-key aliases per band. Mirrors what STAC asset pickers expect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BandAliases {
    /// Red-band asset keys (tried in order).
    pub red: Vec<String>,
    /// NIR-band asset keys.
    pub nir: Vec<String>,
    /// SWIR2-band asset keys (optional — empty if not bundled).
    #[serde(default)]
    pub swir2: Vec<String>,
    /// SCL/QA mask asset keys.
    #[serde(default)]
    pub mask: Vec<String>,
}

/// Static catalog of products the orbit-rs geo stack supports.
#[must_use]
pub fn known_products() -> Vec<Product> {
    vec![
        Product {
            id: "sentinel-2-l2a".into(),
            title: "Sentinel-2 Level-2A".into(),
            platform: "Sentinel-2".into(),
            native_resolution_m: 10.0,
            mask_kind: MaskKind::Sentinel2Scl,
            band_aliases: BandAliases {
                red:   vec!["red".into(),   "B04".into(), "b04".into()],
                nir:   vec!["nir".into(),   "B08".into(), "b08".into()],
                swir2: vec!["swir22".into(),"B12".into(), "b12".into()],
                mask:  vec!["scl".into(),   "SCL".into()],
            },
        },
        Product {
            id: "sentinel-2-l1c".into(),
            title: "Sentinel-2 Level-1C (Top-of-Atmosphere)".into(),
            platform: "Sentinel-2".into(),
            native_resolution_m: 10.0,
            mask_kind: MaskKind::None,
            band_aliases: BandAliases {
                red:   vec!["red".into(),   "B04".into()],
                nir:   vec!["nir".into(),   "B08".into()],
                swir2: vec!["swir22".into(),"B12".into()],
                mask:  vec![],
            },
        },
        Product {
            id: "landsat-c2-l2".into(),
            title: "Landsat Collection 2 Level-2 Surface Reflectance".into(),
            platform: "Landsat-8/9".into(),
            native_resolution_m: 30.0,
            mask_kind: MaskKind::LandsatC2QaPixel,
            band_aliases: BandAliases {
                red:   vec!["red".into(),   "SR_B4".into()],
                nir:   vec!["nir08".into(), "SR_B5".into()],
                swir2: vec!["swir22".into(),"SR_B7".into()],
                mask:  vec!["qa_pixel".into(), "QA_PIXEL".into()],
            },
        },
        Product {
            id: "cop-dem-glo-30".into(),
            title: "Copernicus DEM Global 30 m".into(),
            platform: "TanDEM-X".into(),
            native_resolution_m: 30.0,
            mask_kind: MaskKind::None,
            band_aliases: BandAliases {
                red:   vec!["data".into()],
                nir:   vec!["data".into()],
                swir2: vec![],
                mask:  vec![],
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_products_includes_canonical_set() {
        let ids: Vec<String> = known_products().into_iter().map(|p| p.id).collect();
        for required in ["sentinel-2-l2a", "sentinel-2-l1c", "landsat-c2-l2", "cop-dem-glo-30"] {
            assert!(ids.iter().any(|i| i == required), "missing {required}");
        }
    }

    #[test]
    fn sentinel2_l2a_carries_scl_mask_kind() {
        let p = known_products().into_iter()
            .find(|p| p.id == "sentinel-2-l2a").unwrap();
        assert_eq!(p.mask_kind, MaskKind::Sentinel2Scl);
        assert!(p.band_aliases.mask.iter().any(|m| m == "scl"));
    }

    #[test]
    fn landsat_carries_qa_pixel_mask_kind() {
        let p = known_products().into_iter()
            .find(|p| p.id == "landsat-c2-l2").unwrap();
        assert_eq!(p.mask_kind, MaskKind::LandsatC2QaPixel);
        assert!(p.band_aliases.mask.iter().any(|m| m == "qa_pixel"));
    }

    #[test]
    fn mask_kind_serialises_snake_case() {
        let j = serde_json::to_string(&MaskKind::Sentinel2Scl).unwrap();
        assert_eq!(j, r#""sentinel2_scl""#);
        let j = serde_json::to_string(&MaskKind::LandsatC2QaPixel).unwrap();
        assert_eq!(j, r#""landsat_c2_qa_pixel""#);
        let j = serde_json::to_string(&MaskKind::None).unwrap();
        assert_eq!(j, r#""none""#);
    }

    #[test]
    fn product_round_trips_through_serde_json() {
        let p = Product {
            id: "x".into(),
            title: "X".into(),
            platform: "test".into(),
            native_resolution_m: 10.0,
            mask_kind: MaskKind::None,
            band_aliases: BandAliases {
                red: vec!["r".into()], nir: vec!["n".into()],
                swir2: vec![], mask: vec![],
            },
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: Product = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }
}
