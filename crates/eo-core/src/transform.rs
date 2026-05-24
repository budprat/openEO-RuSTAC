//! Geo-transform and pixel-resolution descriptors.

use serde::{Deserialize, Serialize};

/// Pixel resolution in georeferenced units.
///
/// `y` is conventionally negative for north-up imagery (origin top-left).
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageResolution {
    /// Width of one pixel in CRS units.
    pub x: f64,
    /// Height of one pixel in CRS units (negative for north-up).
    pub y: f64,
}

/// GDAL-style 6-element affine geo-transform.
///
/// Format: `[origin_x, pixel_width, row_skew, origin_y, col_skew, -pixel_height]`.
/// For most analysis-ready EO products `row_skew` and `col_skew` are zero.
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GeoTransform(pub [f64; 6]);

impl GeoTransform {
    /// Construct from the six raw GDAL coefficients.
    #[must_use]
    pub const fn from_array(coeffs: [f64; 6]) -> Self {
        Self(coeffs)
    }

    /// X coordinate of the dataset origin (top-left corner).
    #[must_use]
    pub const fn origin_x(&self) -> f64 { self.0[0] }

    /// Y coordinate of the dataset origin (top-left corner).
    #[must_use]
    pub const fn origin_y(&self) -> f64 { self.0[3] }

    /// Pixel width in CRS units.
    #[must_use]
    pub const fn pixel_width(&self) -> f64 { self.0[1] }

    /// Pixel height in CRS units (typically negative for north-up).
    #[must_use]
    pub const fn pixel_height(&self) -> f64 { self.0[5] }

    /// True iff the transform has zero row/column skew (axis-aligned).
    #[must_use]
    pub fn is_axis_aligned(&self) -> bool {
        self.0[2] == 0.0 && self.0[4] == 0.0
    }

    /// Convert a raster (col, row) pixel index to (x, y) CRS coordinates of
    /// the pixel's upper-left corner.
    #[must_use]
    pub fn pixel_to_crs(&self, col: f64, row: f64) -> (f64, f64) {
        let x = self.0[0] + col * self.0[1] + row * self.0[2];
        let y = self.0[3] + col * self.0[4] + row * self.0[5];
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_transform() -> GeoTransform {
        // S2-like: 10 m pixels, north-up, origin at (300000, 5_700_000) UTM
        GeoTransform::from_array([300_000.0, 10.0, 0.0, 5_700_000.0, 0.0, -10.0])
    }

    #[test]
    fn accessors_read_the_right_index() {
        let gt = sample_transform();
        assert_eq!(gt.origin_x(), 300_000.0);
        assert_eq!(gt.origin_y(), 5_700_000.0);
        assert_eq!(gt.pixel_width(), 10.0);
        assert_eq!(gt.pixel_height(), -10.0);
    }

    #[test]
    fn is_axis_aligned_true_for_zero_skews() {
        assert!(sample_transform().is_axis_aligned());
    }

    #[test]
    fn is_axis_aligned_false_for_nonzero_skews() {
        let gt = GeoTransform::from_array([0.0, 1.0, 0.5, 0.0, 0.0, -1.0]);
        assert!(!gt.is_axis_aligned());
    }

    #[test]
    fn pixel_to_crs_at_origin() {
        let (x, y) = sample_transform().pixel_to_crs(0.0, 0.0);
        assert_eq!(x, 300_000.0);
        assert_eq!(y, 5_700_000.0);
    }

    #[test]
    fn pixel_to_crs_offsets_north_up_correctly() {
        // 100 cols east, 50 rows south of origin → (+1000, -500) in CRS units
        let (x, y) = sample_transform().pixel_to_crs(100.0, 50.0);
        assert_eq!(x, 301_000.0);
        assert_eq!(y, 5_699_500.0);
    }

    #[test]
    fn resolution_default_is_zero() {
        let r = ImageResolution::default();
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 0.0);
    }

    #[test]
    fn serde_roundtrip_preserves_coeffs() {
        let gt = sample_transform();
        let json = serde_json::to_string(&gt).unwrap();
        let back: GeoTransform = serde_json::from_str(&json).unwrap();
        assert_eq!(gt, back);
    }
}
