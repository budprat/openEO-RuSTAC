//! Read-window and overlap primitives.

use serde::{Deserialize, Serialize};

/// Offset `(row, col)` into a raster — signed to allow partial-block reads
/// where a window starts before the raster origin (e.g. overlap windows).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Offset {
    /// Row offset (may be negative for overlap windows).
    pub rows: isize,
    /// Column offset (may be negative for overlap windows).
    pub cols: isize,
}

/// Dimensions in array coordinates, signed for overlap arithmetic.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    /// Number of rows.
    pub rows: isize,
    /// Number of columns.
    pub cols: isize,
}

/// A window into a parent raster — what to read from disk for one block.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReadWindow {
    /// Top-left offset into the source raster.
    pub offset: Offset,
    /// Width × height of the window.
    pub size: Size,
}

impl ReadWindow {
    /// True iff `rows × cols > 0`. Doesn't check signedness — a negative
    /// dimension is treated as empty.
    #[must_use]
    pub const fn is_non_empty(&self) -> bool {
        self.size.rows > 0 && self.size.cols > 0
    }
}

/// Overlap pixels added to each side of a block for context-aware workers.
///
/// Convolution kernels (median filter, morphological ops) read the overlap
/// region for clean edge output; the overlap is trimmed before write-back.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Overlap {
    /// Overlap pixels on the top edge.
    pub top: usize,
    /// Overlap pixels on the bottom edge.
    pub bottom: usize,
    /// Overlap pixels on the left edge.
    pub left: usize,
    /// Overlap pixels on the right edge.
    pub right: usize,
}

impl Overlap {
    /// Symmetric overlap of `n` pixels on every side.
    #[must_use]
    pub const fn uniform(n: usize) -> Self {
        Self { top: n, bottom: n, left: n, right: n }
    }

    /// True iff every edge has zero overlap.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.top == 0 && self.bottom == 0 && self.left == 0 && self.right == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_uniform_sets_all_edges_to_n() {
        let o = Overlap::uniform(3);
        assert_eq!(o.top, 3);
        assert_eq!(o.bottom, 3);
        assert_eq!(o.left, 3);
        assert_eq!(o.right, 3);
    }

    #[test]
    fn overlap_default_is_zero() {
        assert!(Overlap::default().is_zero());
    }

    #[test]
    fn overlap_uniform_zero_is_zero() {
        assert!(Overlap::uniform(0).is_zero());
    }

    #[test]
    fn overlap_nonzero_edge_breaks_is_zero() {
        let o = Overlap { top: 0, bottom: 0, left: 0, right: 1 };
        assert!(!o.is_zero());
    }

    #[test]
    fn read_window_non_empty_requires_positive_dims() {
        let w = ReadWindow {
            offset: Offset { rows: 0, cols: 0 },
            size: Size { rows: 10, cols: 10 },
        };
        assert!(w.is_non_empty());
        let w0 = ReadWindow {
            offset: Offset { rows: 0, cols: 0 },
            size: Size { rows: 0, cols: 10 },
        };
        assert!(!w0.is_non_empty());
    }

    #[test]
    fn negative_offset_supported_for_overlap_windows() {
        let w = ReadWindow {
            offset: Offset { rows: -2, cols: -2 },
            size: Size { rows: 6, cols: 6 },
        };
        assert!(w.is_non_empty());
        assert_eq!(w.offset.rows, -2);
    }

    #[test]
    fn serde_roundtrip_read_window() {
        let w = ReadWindow {
            offset: Offset { rows: 10, cols: 20 },
            size: Size { rows: 256, cols: 256 },
        };
        let json = serde_json::to_string(&w).unwrap();
        let back: ReadWindow = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
    }
}
