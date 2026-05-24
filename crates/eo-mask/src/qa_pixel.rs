//! Landsat Collection 2 `QA_PIXEL` bitfield decoder.
//!
//! Source: *Landsat 8-9 Collection 2 Level 2 Science Product Guide*
//! (USGS LSDS-1619), Table 7-3 ("Pixel Quality Assessment Band Values").
//!
//! Bit layout (LSB = bit 0):
//!
//! | bits  | meaning                              |
//! |------:|--------------------------------------|
//! | 0     | Fill                                  |
//! | 1     | Dilated Cloud                         |
//! | 2     | Cirrus (high confidence)              |
//! | 3     | Cloud                                 |
//! | 4     | Cloud Shadow                          |
//! | 5     | Snow                                  |
//! | 6     | Clear                                 |
//! | 7     | Water                                 |
//! | 8-9   | Cloud Confidence (0–3)                |
//! | 10-11 | Cloud Shadow Confidence (0–3)         |
//! | 12-13 | Snow / Ice Confidence (0–3)           |
//! | 14-15 | Cirrus Confidence (0–3)               |
//!
//! Confidence levels: `0=None, 1=Low, 2=Medium, 3=High`.

use serde::{Deserialize, Serialize};

use crate::provider::MaskValue;

/// Confidence ladder used across cloud/shadow/snow/cirrus.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QaConfidence {
    /// 0 — algorithm could not detect.
    #[default]
    None,
    /// 1 — low confidence.
    Low,
    /// 2 — medium confidence.
    Medium,
    /// 3 — high confidence.
    High,
}

impl QaConfidence {
    /// Decode a 2-bit confidence value into the typed enum.
    #[must_use]
    pub const fn from_2bits(v: u16) -> Self {
        match v & 0b11 {
            0 => Self::None,
            1 => Self::Low,
            2 => Self::Medium,
            _ => Self::High,
        }
    }

    /// True iff at least Medium confidence.
    #[must_use]
    pub const fn is_medium_or_high(self) -> bool {
        matches!(self, Self::Medium | Self::High)
    }
}

/// Decoded view of a single QA_PIXEL value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct QaPixel {
    /// Bit 0 — fill (no data).
    pub fill: bool,
    /// Bit 1 — dilated cloud.
    pub dilated_cloud: bool,
    /// Bit 2 — cirrus (high confidence).
    pub cirrus: bool,
    /// Bit 3 — cloud.
    pub cloud: bool,
    /// Bit 4 — cloud shadow.
    pub cloud_shadow: bool,
    /// Bit 5 — snow.
    pub snow: bool,
    /// Bit 6 — clear.
    pub clear: bool,
    /// Bit 7 — water.
    pub water: bool,
    /// Bits 8-9 — cloud confidence.
    pub cloud_confidence: QaConfidence,
    /// Bits 10-11 — cloud-shadow confidence.
    pub cloud_shadow_confidence: QaConfidence,
    /// Bits 12-13 — snow confidence.
    pub snow_confidence: QaConfidence,
    /// Bits 14-15 — cirrus confidence.
    pub cirrus_confidence: QaConfidence,
}

impl QaPixel {
    /// Decode the 16-bit QA_PIXEL value into a typed view.
    #[must_use]
    pub const fn from_u16(v: u16) -> Self {
        Self {
            fill:                    v & 0b1 != 0,
            dilated_cloud:           v >> 1  & 0b1 != 0,
            cirrus:                  v >> 2  & 0b1 != 0,
            cloud:                   v >> 3  & 0b1 != 0,
            cloud_shadow:            v >> 4  & 0b1 != 0,
            snow:                    v >> 5  & 0b1 != 0,
            clear:                   v >> 6  & 0b1 != 0,
            water:                   v >> 7  & 0b1 != 0,
            cloud_confidence:        QaConfidence::from_2bits(v >> 8),
            cloud_shadow_confidence: QaConfidence::from_2bits(v >> 10),
            snow_confidence:         QaConfidence::from_2bits(v >> 12),
            cirrus_confidence:       QaConfidence::from_2bits(v >> 14),
        }
    }

    /// Project the decoded QA into the unified [`MaskValue`].
    ///
    /// Precedence (matches what most downstream cloud-mask consumers
    /// expect): fill → cloud → cloud-shadow → cirrus → snow → water →
    /// clear → other.
    #[must_use]
    pub const fn to_mask_value(self) -> MaskValue {
        if self.fill { return MaskValue::NoData; }
        if self.cloud || self.dilated_cloud { return MaskValue::Cloud; }
        if self.cloud_shadow { return MaskValue::Shadow; }
        if self.cirrus { return MaskValue::Cloud; }
        if self.snow { return MaskValue::Snow; }
        if self.water { return MaskValue::Water; }
        if self.clear { return MaskValue::Clear; }
        // No bit set explicitly — caller may treat as unknown; we default
        // to Clear since the absence of cloud/shadow/snow is the
        // benign case.
        MaskValue::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a u16 from bit positions.
    fn bits(positions: &[u32]) -> u16 {
        positions.iter().fold(0u16, |a, p| a | (1 << *p))
    }

    /// Confidence helper: pack a 2-bit value at a 2-bit slot.
    fn conf(slot: u32, value: u16) -> u16 {
        (value & 0b11) << slot
    }

    #[test]
    fn confidence_decodes_all_levels() {
        assert_eq!(QaConfidence::from_2bits(0), QaConfidence::None);
        assert_eq!(QaConfidence::from_2bits(1), QaConfidence::Low);
        assert_eq!(QaConfidence::from_2bits(2), QaConfidence::Medium);
        assert_eq!(QaConfidence::from_2bits(3), QaConfidence::High);
    }

    #[test]
    fn confidence_medium_or_high() {
        assert!(!QaConfidence::None.is_medium_or_high());
        assert!(!QaConfidence::Low.is_medium_or_high());
        assert!(QaConfidence::Medium.is_medium_or_high());
        assert!(QaConfidence::High.is_medium_or_high());
    }

    #[test]
    fn fill_bit_decoded() {
        let p = QaPixel::from_u16(bits(&[0]));
        assert!(p.fill);
        assert_eq!(p.to_mask_value(), MaskValue::NoData);
    }

    #[test]
    fn cloud_bit_dominates_clear_bit() {
        // Both bit 3 (cloud) and bit 6 (clear) set — cloud must win.
        let p = QaPixel::from_u16(bits(&[3, 6]));
        assert!(p.cloud);
        assert!(p.clear);
        assert_eq!(p.to_mask_value(), MaskValue::Cloud);
    }

    #[test]
    fn dilated_cloud_maps_to_cloud() {
        let p = QaPixel::from_u16(bits(&[1]));
        assert!(p.dilated_cloud);
        assert_eq!(p.to_mask_value(), MaskValue::Cloud);
    }

    #[test]
    fn shadow_bit_decoded() {
        let p = QaPixel::from_u16(bits(&[4]));
        assert!(p.cloud_shadow);
        assert_eq!(p.to_mask_value(), MaskValue::Shadow);
    }

    #[test]
    fn snow_bit_decoded() {
        let p = QaPixel::from_u16(bits(&[5]));
        assert!(p.snow);
        assert_eq!(p.to_mask_value(), MaskValue::Snow);
    }

    #[test]
    fn water_bit_decoded() {
        let p = QaPixel::from_u16(bits(&[7]));
        assert!(p.water);
        assert_eq!(p.to_mask_value(), MaskValue::Water);
    }

    #[test]
    fn clear_bit_decoded() {
        let p = QaPixel::from_u16(bits(&[6]));
        assert!(p.clear);
        assert_eq!(p.to_mask_value(), MaskValue::Clear);
    }

    #[test]
    fn cloud_confidence_high() {
        let v = conf(8, 3); // bits 8-9 = high
        let p = QaPixel::from_u16(v);
        assert_eq!(p.cloud_confidence, QaConfidence::High);
    }

    #[test]
    fn cloud_shadow_confidence_medium() {
        let v = conf(10, 2);
        let p = QaPixel::from_u16(v);
        assert_eq!(p.cloud_shadow_confidence, QaConfidence::Medium);
    }

    #[test]
    fn snow_confidence_low() {
        let v = conf(12, 1);
        let p = QaPixel::from_u16(v);
        assert_eq!(p.snow_confidence, QaConfidence::Low);
    }

    #[test]
    fn cirrus_confidence_none() {
        let v = conf(14, 0);
        let p = QaPixel::from_u16(v);
        assert_eq!(p.cirrus_confidence, QaConfidence::None);
    }

    #[test]
    fn known_landsat8_clear_value() {
        // QA_PIXEL = 21824 (decimal) for clear-land per USGS documentation
        // examples (clear bit set + cloud-confidence "None" + others 0).
        // 21824 = 0b0101_0101_0100_0000 — clear (bit 6) + a particular
        // confidence combination. The exact magic number depends on the
        // product; verifying the bits we care about is enough.
        let v: u16 = 0b0101_0101_0100_0000;
        let p = QaPixel::from_u16(v);
        assert!(p.clear);
        assert!(!p.cloud);
        assert!(!p.cloud_shadow);
        assert!(!p.fill);
    }

    #[test]
    fn fill_value_zero_has_no_flags() {
        let p = QaPixel::from_u16(0);
        assert_eq!(p, QaPixel::default());
        // No bits set ⇒ default to Clear (benign).
        assert_eq!(p.to_mask_value(), MaskValue::Clear);
    }

    #[test]
    fn cirrus_classified_as_cloud() {
        let p = QaPixel::from_u16(bits(&[2]));
        assert!(p.cirrus);
        assert_eq!(p.to_mask_value(), MaskValue::Cloud);
    }

    #[test]
    fn fill_dominates_other_bits() {
        // Fill + cloud + shadow + clear all set — fill must win.
        let p = QaPixel::from_u16(bits(&[0, 3, 4, 6]));
        assert_eq!(p.to_mask_value(), MaskValue::NoData);
    }
}
