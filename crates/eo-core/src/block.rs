//! Block-size primitives for chunked raster processing.

use serde::{Deserialize, Serialize};

/// Block size for chunked raster operations.
///
/// Smaller blocks → finer parallelism + more I/O overhead.
/// Larger blocks → fewer reads + larger per-worker memory footprint.
/// The 2048×2048 default is the empirical sweet spot for Sentinel-2 / Landsat
/// scenes on consumer hardware; tune to GDAL's natural block size where the
/// source dataset advertises one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockSize {
    /// Block height in pixels.
    pub rows: usize,
    /// Block width in pixels.
    pub cols: usize,
}

impl BlockSize {
    /// Construct a block size; panics in debug builds if either dimension is zero.
    #[must_use]
    pub const fn new(rows: usize, cols: usize) -> Self {
        debug_assert!(rows > 0, "BlockSize::new: rows must be > 0");
        debug_assert!(cols > 0, "BlockSize::new: cols must be > 0");
        Self { rows, cols }
    }

    /// Total pixel count of the block.
    #[must_use]
    pub const fn area(&self) -> usize {
        self.rows.saturating_mul(self.cols)
    }

    /// True iff this block is square.
    #[must_use]
    pub const fn is_square(&self) -> bool {
        self.rows == self.cols
    }
}

impl Default for BlockSize {
    fn default() -> Self {
        Self { rows: 2048, cols: 2048 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_2048_square() {
        let b = BlockSize::default();
        assert_eq!(b.rows, 2048);
        assert_eq!(b.cols, 2048);
        assert!(b.is_square());
    }

    #[test]
    fn area_returns_rows_times_cols() {
        assert_eq!(BlockSize::new(4, 5).area(), 20);
        assert_eq!(BlockSize::new(2048, 2048).area(), 2048 * 2048);
    }

    #[test]
    fn area_saturates_on_overflow() {
        // usize::MAX × 2 must not panic; saturate to MAX.
        let b = BlockSize::new(usize::MAX, 2);
        assert_eq!(b.area(), usize::MAX);
    }

    #[test]
    fn is_square_distinguishes_rect_from_square() {
        assert!(BlockSize::new(512, 512).is_square());
        assert!(!BlockSize::new(512, 256).is_square());
    }

    #[test]
    fn equality_is_field_wise() {
        assert_eq!(BlockSize::new(4, 5), BlockSize::new(4, 5));
        assert_ne!(BlockSize::new(4, 5), BlockSize::new(5, 4));
    }

    #[test]
    fn serde_roundtrip_preserves_fields() {
        let original = BlockSize::new(1024, 512);
        let json = serde_json::to_string(&original).expect("serialize");
        let back: BlockSize = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }
}
