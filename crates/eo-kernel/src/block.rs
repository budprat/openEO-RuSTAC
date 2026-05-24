//! Block addressing — opaque ids and the in-flight `RasterBlock` view.

use eo_core::{BlockSize, ReadWindow};
use serde::{Deserialize, Serialize};

/// Position of a block in the (row-major) grid that tiles a raster.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RasterBlockId {
    /// Block row index in the grid.
    pub block_row: usize,
    /// Block column index in the grid.
    pub block_col: usize,
}

impl std::fmt::Display for RasterBlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {})", self.block_row, self.block_col)
    }
}

/// A logical "block view" — block id, the read window backing it, and the
/// block size in pixels.
///
/// Generic over `T` so concrete pixel storage (an `ndarray::Array3<T>`, a
/// borrowed slice, a GPU buffer handle) lives in the layer that owns the
/// allocator.
#[derive(Debug, Clone)]
pub struct RasterBlock<T> {
    /// Block coordinates in the grid.
    pub id: RasterBlockId,
    /// What window in the parent raster this block reads from.
    pub window: ReadWindow,
    /// Block size in pixels.
    pub size: BlockSize,
    /// Pixel storage. Layer-specific.
    pub pixels: T,
}

impl<T> RasterBlock<T> {
    /// Total pixel count of this block.
    #[must_use]
    pub fn area(&self) -> usize {
        self.size.area()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eo_core::{Offset, Size};

    #[test]
    fn id_display_uses_tuple_form() {
        let id = RasterBlockId { block_row: 3, block_col: 4 };
        assert_eq!(id.to_string(), "(3, 4)");
    }

    #[test]
    fn block_area_delegates_to_size() {
        let b = RasterBlock {
            id: RasterBlockId::default(),
            window: ReadWindow {
                offset: Offset::default(),
                size: Size { rows: 32, cols: 32 },
            },
            size: BlockSize::new(32, 32),
            pixels: (),
        };
        assert_eq!(b.area(), 1024);
    }

    #[test]
    fn id_default_is_origin() {
        let id = RasterBlockId::default();
        assert_eq!(id.block_row, 0);
        assert_eq!(id.block_col, 0);
    }

    #[test]
    fn id_serde_roundtrips() {
        let id = RasterBlockId { block_row: 7, block_col: 11 };
        let s = serde_json::to_string(&id).unwrap();
        let back: RasterBlockId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }
}
