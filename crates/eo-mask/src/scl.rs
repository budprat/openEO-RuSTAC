//! Sentinel-2 L2A SCL (Scene Classification Layer) decoder.
//!
//! Reference: Sentinel-2 L2A Product Definition (ESA, S2 MSI). The SCL band
//! encodes a 12-class scene classification in u8 values 0–11.

use serde::{Deserialize, Serialize};

use crate::provider::MaskValue;

/// Sentinel-2 L2A SCL class.
///
/// Values match the official ESA encoding so the decoder is a direct
/// `match` without a lookup table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ScenSceneClass {
    /// 0 — Saturated or no-data.
    NoData = 0,
    /// 1 — Saturated / defective.
    Saturated = 1,
    /// 2 — Dark area / topographic shadow.
    DarkArea = 2,
    /// 3 — Cloud shadows.
    CloudShadows = 3,
    /// 4 — Vegetation.
    Vegetation = 4,
    /// 5 — Bare soil / desert.
    BareSoil = 5,
    /// 6 — Water.
    Water = 6,
    /// 7 — Unclassified.
    Unclassified = 7,
    /// 8 — Cloud medium probability.
    CloudMedium = 8,
    /// 9 — Cloud high probability.
    CloudHigh = 9,
    /// 10 — Thin cirrus.
    Cirrus = 10,
    /// 11 — Snow or ice.
    Snow = 11,
}

impl ScenSceneClass {
    /// Decode a raw SCL byte into a typed class. Unknown values become
    /// [`ScenSceneClass::Unclassified`] (the official catch-all).
    #[must_use]
    pub const fn from_u8(b: u8) -> Self {
        match b {
            0 => Self::NoData,
            1 => Self::Saturated,
            2 => Self::DarkArea,
            3 => Self::CloudShadows,
            4 => Self::Vegetation,
            5 => Self::BareSoil,
            6 => Self::Water,
            8 => Self::CloudMedium,
            9 => Self::CloudHigh,
            10 => Self::Cirrus,
            11 => Self::Snow,
            // 7 explicitly + any other unexpected value
            _ => Self::Unclassified,
        }
    }

    /// Project the SCL class into the generic [`MaskValue`] taxonomy.
    ///
    /// Cloud-shadow → Shadow, water → Water, snow → Snow, cloud (any
    /// confidence) → Cloud, vegetation / bare soil / unclassified → Clear,
    /// saturated → Saturated, no-data → NoData, cirrus → Cloud
    /// (conservative).
    #[must_use]
    pub const fn to_mask_value(self) -> MaskValue {
        match self {
            Self::NoData => MaskValue::NoData,
            Self::Saturated => MaskValue::Saturated,
            Self::CloudShadows => MaskValue::Shadow,
            Self::Vegetation | Self::BareSoil | Self::Unclassified | Self::DarkArea => {
                MaskValue::Clear
            }
            Self::Water => MaskValue::Water,
            Self::CloudMedium | Self::CloudHigh | Self::Cirrus => MaskValue::Cloud,
            Self::Snow => MaskValue::Snow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_decode() {
        assert_eq!(ScenSceneClass::from_u8(0), ScenSceneClass::NoData);
        assert_eq!(ScenSceneClass::from_u8(4), ScenSceneClass::Vegetation);
        assert_eq!(ScenSceneClass::from_u8(9), ScenSceneClass::CloudHigh);
        assert_eq!(ScenSceneClass::from_u8(11), ScenSceneClass::Snow);
    }

    #[test]
    fn unknown_value_falls_through_to_unclassified() {
        assert_eq!(ScenSceneClass::from_u8(12), ScenSceneClass::Unclassified);
        assert_eq!(ScenSceneClass::from_u8(99), ScenSceneClass::Unclassified);
        assert_eq!(ScenSceneClass::from_u8(7), ScenSceneClass::Unclassified);
    }

    #[test]
    fn cloud_classes_project_to_cloud() {
        for c in [
            ScenSceneClass::CloudMedium,
            ScenSceneClass::CloudHigh,
            ScenSceneClass::Cirrus,
        ] {
            assert_eq!(c.to_mask_value(), MaskValue::Cloud);
        }
    }

    #[test]
    fn shadow_projects_to_shadow() {
        assert_eq!(ScenSceneClass::CloudShadows.to_mask_value(), MaskValue::Shadow);
    }

    #[test]
    fn vegetation_projects_to_clear() {
        assert_eq!(ScenSceneClass::Vegetation.to_mask_value(), MaskValue::Clear);
    }

    #[test]
    fn nodata_projects_to_nodata() {
        assert_eq!(ScenSceneClass::NoData.to_mask_value(), MaskValue::NoData);
    }
}
