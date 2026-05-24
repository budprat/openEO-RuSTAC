//! **Tier 3.5 — pure-Rust cloud detection** for Sentinel-2 reflectance bands.
//!
//! **What this is**: A simple brightness-based rule classifier. A pixel is
//! flagged as cloud if all three of `blue`, `nir`, `swir2` exceed a
//! brightness threshold (clouds reflect brightly across the visible-to-SWIR
//! spectrum, whereas land surfaces, water, and shadow do not).
//!
//! **What this is NOT**: an s2cloudless port. A faithful s2cloudless
//! implementation requires the LightGBM-trained decision-tree forest model
//! (thousands of trees, 10 input bands, Gaussian-blur preprocessing). That
//! port is deferred — see the archived parity plan §6 T3.5 Option B notes.
//!
//! **Use case**: A baseline cloud heuristic for orbit-geo workflows that
//! don't need s2cloudless accuracy. Replace with a model-based classifier
//! when higher accuracy is required.

#![cfg(feature = "cloud_mask")]

use crate::error::{Error, Result};
use ndarray::{Array2, ArrayView2};

/// Cloud detection thresholds. Default values target Sentinel-2 L2A
/// reflectance × 10000 scaling (so 2000 ≈ 0.20 reflectance).
#[derive(Debug, Clone, Copy)]
pub struct CloudThresholds {
    /// Minimum blue (B02) reflectance × 10000 to flag as cloud.
    pub blue_min: i16,
    /// Minimum NIR (B08) reflectance × 10000 to flag as cloud.
    pub nir_min: i16,
    /// Minimum SWIR2 (B12) reflectance × 10000 to flag as cloud.
    pub swir2_min: i16,
}

impl Default for CloudThresholds {
    fn default() -> Self {
        Self { blue_min: 2000, nir_min: 2000, swir2_min: 1500 }
    }
}

/// Classify each pixel as 0 (clear) or 1 (cloud) using the brightness rule.
///
/// `blue`, `nir`, `swir2`: 2-D views of the corresponding Sentinel-2 bands,
/// all at the same resolution + shape. Returns `Array2<u8>` of the same
/// `(rows, cols)`.
pub fn classify(
    blue: ArrayView2<i16>,
    nir: ArrayView2<i16>,
    swir2: ArrayView2<i16>,
    thresh: &CloudThresholds,
) -> Result<Array2<u8>> {
    let dims = blue.dim();
    if nir.dim() != dims || swir2.dim() != dims {
        return Err(Error::Other(format!(
            "cloud_mask::classify: shape mismatch blue={:?}, nir={:?}, swir2={:?}",
            dims,
            nir.dim(),
            swir2.dim()
        )));
    }
    let mut out: Array2<u8> = Array2::zeros(dims);
    for ((r, c), &b) in blue.indexed_iter() {
        let n = nir[[r, c]];
        let s = swir2[[r, c]];
        if b >= thresh.blue_min && n >= thresh.nir_min && s >= thresh.swir2_min {
            out[[r, c]] = 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

    /// **RED T3.5/A1**: bright pixels across blue + nir + swir2 → 1; others → 0.
    #[test]
    fn classify_flags_bright_cross_spectrum_pixels_as_cloud() {
        let blue = array![[500_i16, 3000], [800, 200]];
        let nir = array![[400_i16, 3500], [3000, 100]];
        let swir2 = array![[300_i16, 2500], [2000, 50]];
        let mask = classify(blue.view(), nir.view(), swir2.view(), &CloudThresholds::default()).unwrap();
        // Pixel [0,0]: blue=500 < 2000 → clear
        // Pixel [0,1]: blue=3000, nir=3500, swir2=2500 → all above → cloud
        // Pixel [1,0]: blue=800 < 2000 → clear (even though nir + swir2 above)
        // Pixel [1,1]: all very low → clear
        assert_eq!(mask, array![[0_u8, 1], [0, 0]]);
    }

    /// **RED T3.5/A2**: shape mismatch → Err.
    #[test]
    fn classify_errors_on_shape_mismatch() {
        let blue = array![[100_i16, 200]];
        let nir = array![[100_i16, 200, 300]];
        let swir2 = array![[100_i16, 200]];
        let r = classify(blue.view(), nir.view(), swir2.view(), &CloudThresholds::default());
        assert!(r.is_err());
    }

    /// **RED T3.5/A3**: custom thresholds change behavior.
    #[test]
    fn classify_respects_custom_thresholds() {
        let blue = array![[2500_i16]];
        let nir = array![[2500_i16]];
        let swir2 = array![[1800_i16]];
        // Default thresholds: all ≥ defaults → cloud
        let m1 = classify(blue.view(), nir.view(), swir2.view(), &CloudThresholds::default()).unwrap();
        assert_eq!(m1[[0, 0]], 1);
        // Higher thresholds: doesn't reach → clear
        let stricter = CloudThresholds { blue_min: 3000, nir_min: 3000, swir2_min: 2000 };
        let m2 = classify(blue.view(), nir.view(), swir2.view(), &stricter).unwrap();
        assert_eq!(m2[[0, 0]], 0);
    }
}

// ─────────────────────────────────────────────────────────────────────
// Batch F — QA bit-flag decoding (Landsat C2 + Sentinel-2 SCL)
// ─────────────────────────────────────────────────────────────────────

/// QA-band semantics for a specific sensor product.
#[derive(Debug, Clone, Copy)]
pub enum QaType {
    /// Sentinel-2 L2A SCL (0=nodata, 4=veg, 5=not-veg, 6=water, 7=unclass, 8/9=cloud, 10=cirrus, 11=snow).
    Sentinel2Scl,
    /// Landsat Collection 2 Level 2 QA_PIXEL bit-field.
    LandsatC2QaPixel,
}

/// Decode a QA-band view into a boolean clear-sky mask (`true` = clear).
///
/// `qa`: 2-D array of QA pixel values.
/// `qa_type`: which sensor's QA semantics to apply.
pub fn decode_qa_mask(qa: ndarray::ArrayView2<u16>, qa_type: QaType) -> Array2<bool> {
    let dims = qa.dim();
    let mut out = Array2::<bool>::from_elem(dims, false);
    for ((r, c), &v) in qa.indexed_iter() {
        let clear = match qa_type {
            QaType::Sentinel2Scl => {
                // SCL: 4=veg, 5=not-veg, 6=water, 7=unclass, 11=snow → clear
                matches!(v, 4 | 5 | 6 | 7 | 11)
            }
            QaType::LandsatC2QaPixel => {
                // QA_PIXEL bit 6 = clear; bits 1-5 = dilated cloud/cirrus/cloud/shadow/snow/water
                // Treat "clear" as: bit 6 set AND no cloud/cirrus/cloud-shadow bits set.
                let bit6_clear = (v & (1 << 6)) != 0;
                let cloud_bits = (v & 0b0011_1100) != 0; // bits 2-5
                bit6_clear && !cloud_bits
            }
        };
        out[[r, c]] = clear;
    }
    out
}

/// Apply a boolean clear-sky mask to a typed array: clear → keep; masked → `na`.
pub fn apply_mask<T: Copy>(
    result: ndarray::ArrayViewMut2<T>,
    mask: ndarray::ArrayView2<bool>,
    na: T,
) {
    let mut result = result;
    for ((r, c), keep) in mask.indexed_iter() {
        if !keep {
            result[[r, c]] = na;
        }
    }
}

#[cfg(test)]
mod batch_f_tests {
    use super::*;
    use ndarray::{array, Array2};

    #[test]
    fn decode_qa_mask_s2_scl_keeps_clear_codes() {
        let qa: Array2<u16> = array![[0, 4, 5, 8], [9, 11, 7, 6]];
        let mask = decode_qa_mask(qa.view(), QaType::Sentinel2Scl);
        // Clear: 4, 5, 6, 7, 11. Cloud: 0 (nodata), 8, 9
        assert_eq!(mask, array![[false, true, true, false], [false, true, true, true]]);
    }

    #[test]
    fn decode_qa_mask_landsat_clear_bit() {
        // QA_PIXEL with only bit 6 set = 64 → clear
        let qa: Array2<u16> = array![[64, 0, 64 + 4]]; // clear, nodata, clear+cloud
        let mask = decode_qa_mask(qa.view(), QaType::LandsatC2QaPixel);
        assert_eq!(mask, array![[true, false, false]]);
    }

    #[test]
    fn apply_mask_replaces_masked_pixels() {
        let mut data: Array2<i16> = array![[10_i16, 20], [30, 40]];
        let mask: Array2<bool> = array![[true, false], [false, true]];
        apply_mask(data.view_mut(), mask.view(), -9999);
        assert_eq!(data, array![[10, -9999], [-9999, 40]]);
    }
}
