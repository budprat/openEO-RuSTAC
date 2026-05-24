//! `CloudMaskProvider` trait + carriers.

use serde::{Deserialize, Serialize};

use crate::confidence::MaskConfidence;

/// Per-pixel mask value.
///
/// `Clear` and `Cloud` are the universal endpoints; `Shadow`, `Snow`,
/// `Water`, `Saturated`, and `Other` carry common per-sensor refinements.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaskValue {
    /// Pixel is clear / usable.
    Clear,
    /// Pixel is cloud.
    Cloud,
    /// Pixel is cloud shadow.
    Shadow,
    /// Pixel is snow / ice.
    Snow,
    /// Pixel is water.
    Water,
    /// Saturated / sensor defect.
    Saturated,
    /// No-data / fill.
    NoData,
    /// Sensor-specific class encoded as a u8.
    Other(u8),
}

/// A 2-D mask band — opaque storage left to the implementation.
///
/// Concrete pixel-storage types live in `eo-io` or downstream apps.
#[derive(Debug, Clone)]
pub struct MaskBand {
    /// Width × height shape.
    pub rows: usize,
    /// Width × height shape.
    pub cols: usize,
    /// Row-major u8 buffer where each byte is a `MaskValue` discriminant.
    pub pixels: Vec<u8>,
}

impl MaskBand {
    /// Construct a clear (all-Clear) mask of the given shape.
    #[must_use]
    pub fn clear(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            pixels: vec![mask_discriminant(MaskValue::Clear); rows * cols],
        }
    }

    /// Total pixel count.
    #[must_use]
    pub fn area(&self) -> usize {
        self.rows * self.cols
    }
}

/// Mapping `MaskValue` → wire-format byte. Stable across compilations
/// (used in cached results).
#[must_use]
pub const fn mask_discriminant(v: MaskValue) -> u8 {
    match v {
        MaskValue::NoData => 0,
        MaskValue::Clear => 1,
        MaskValue::Cloud => 2,
        MaskValue::Shadow => 3,
        MaskValue::Snow => 4,
        MaskValue::Water => 5,
        MaskValue::Saturated => 6,
        MaskValue::Other(b) => b,
    }
}

/// Compute a mask for a scene. Concrete implementations (Fmask,
/// s2cloudless, SCL, QA_PIXEL) implement this trait.
///
/// `Scene` is a placeholder for the eventual scene representation; the
/// associated type lets each impl bind to its sensor model.
pub trait CloudMaskProvider {
    /// Sensor / scene type this provider consumes.
    type Scene;
    /// Implementation-specific error.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Compute the mask.
    fn mask(&self, scene: &Self::Scene) -> std::result::Result<MaskBand, Self::Error>;

    /// Self-reported confidence band of this provider.
    fn confidence(&self) -> MaskConfidence {
        MaskConfidence::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_mask_is_all_clear() {
        let m = MaskBand::clear(4, 3);
        assert_eq!(m.area(), 12);
        assert!(m.pixels.iter().all(|&b| b == mask_discriminant(MaskValue::Clear)));
    }

    #[test]
    fn discriminants_are_unique() {
        let xs = [
            MaskValue::NoData,
            MaskValue::Clear,
            MaskValue::Cloud,
            MaskValue::Shadow,
            MaskValue::Snow,
            MaskValue::Water,
            MaskValue::Saturated,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in xs {
            assert!(seen.insert(mask_discriminant(v)), "duplicate disc for {v:?}");
        }
    }

    #[test]
    fn other_discriminant_passthrough() {
        assert_eq!(mask_discriminant(MaskValue::Other(99)), 99);
    }

    #[test]
    fn mask_value_serde() {
        let s = serde_json::to_string(&MaskValue::Cloud).unwrap();
        assert!(s.contains("Cloud"));
        let back: MaskValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, MaskValue::Cloud);
    }
}
