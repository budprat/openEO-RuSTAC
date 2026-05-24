//! Block executor — drive a [`BlockWorker`] over a tiled grid using rayon.
//!
//! No `Arc<Mutex<Vec<_>>>`. The grid is partitioned into deterministic
//! `(block_row, block_col)` ids; `rayon::par_iter().map(...).collect()`
//! does the heavy lifting and gives us a `Vec<Result<Output, KernelError>>`
//! at no synchronisation cost.

use rayon::prelude::*;

use crate::block::{RasterBlock, RasterBlockId};
use crate::worker::{BlockWorker, KernelError, Result};
use eo_core::{BlockSize, Offset, ReadWindow, Size};

/// Total shape of the raster the grid tiles.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct GridShape {
    /// Total number of rows in the parent raster.
    pub rows: usize,
    /// Total number of cols in the parent raster.
    pub cols: usize,
}

impl GridShape {
    /// Construct a grid shape; debug-asserts non-zero dims.
    #[must_use]
    pub const fn new(rows: usize, cols: usize) -> Self {
        debug_assert!(rows > 0, "GridShape: rows must be > 0");
        debug_assert!(cols > 0, "GridShape: cols must be > 0");
        Self { rows, cols }
    }
}

/// Iterate the block ids that tile a raster of `shape` with `block`.
///
/// Block grid is row-major. Edge blocks are clipped to the raster bounds —
/// their `ReadWindow.size` is smaller than `block` where appropriate.
#[must_use]
pub fn enumerate_blocks(shape: GridShape, block: BlockSize) -> Vec<(RasterBlockId, ReadWindow)> {
    let n_block_rows = shape.rows.div_ceil(block.rows);
    let n_block_cols = shape.cols.div_ceil(block.cols);
    let mut out = Vec::with_capacity(n_block_rows * n_block_cols);
    for br in 0..n_block_rows {
        for bc in 0..n_block_cols {
            let row_off = br * block.rows;
            let col_off = bc * block.cols;
            let rows = block.rows.min(shape.rows - row_off);
            let cols = block.cols.min(shape.cols - col_off);
            out.push((
                RasterBlockId { block_row: br, block_col: bc },
                ReadWindow {
                    offset: Offset {
                        rows: row_off as isize,
                        cols: col_off as isize,
                    },
                    size: Size {
                        rows: rows as isize,
                        cols: cols as isize,
                    },
                },
            ));
        }
    }
    out
}

/// Drive a worker over a grid in parallel. The `read` closure reads pixel
/// storage for a `(id, window)` tuple; this keeps the executor I/O-free.
///
/// Returns outputs in deterministic block order (row-major). Any worker
/// failure short-circuits with [`KernelError::Worker`] carrying the
/// offending block id.
pub fn apply_blocks<Pixels, Output, R, W>(
    shape: GridShape,
    block: BlockSize,
    read: R,
    worker: &W,
) -> Result<Vec<Output>>
where
    R: Fn(RasterBlockId, &ReadWindow) -> std::result::Result<Pixels, Box<dyn std::error::Error + Send + Sync>>
        + Sync,
    W: BlockWorker<RasterBlock<Pixels>, Output>,
    Pixels: Send,
    Output: Send,
{
    let grid = enumerate_blocks(shape, block);

    // par_iter + map + collect — no shared mutable state.
    let results: Vec<Result<Output>> = grid
        .into_par_iter()
        .map(|(id, window)| {
            let pixels = read(id, &window).map_err(|e| KernelError::Worker {
                block_id: id.to_string(),
                source: e,
            })?;
            let blk = RasterBlock { id, window, size: block, pixels };
            worker
                .process(&blk)
                .map_err(|e| KernelError::Worker {
                    block_id: id.to_string(),
                    source: e,
                })
        })
        .collect();

    // First error wins, in deterministic block order.
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_full_tile_grid() {
        let shape = GridShape::new(64, 64);
        let block = BlockSize::new(32, 32);
        let grid = enumerate_blocks(shape, block);
        assert_eq!(grid.len(), 4);
        let ids: Vec<_> = grid.iter().map(|(i, _)| (i.block_row, i.block_col)).collect();
        assert_eq!(ids, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
    }

    #[test]
    fn enumerate_edge_blocks_clip_to_shape() {
        // 50×50 raster with 32×32 blocks → 2×2 grid, edge blocks are 18×18.
        let shape = GridShape::new(50, 50);
        let block = BlockSize::new(32, 32);
        let grid = enumerate_blocks(shape, block);
        assert_eq!(grid.len(), 4);
        let edge = &grid[3]; // (1, 1) corner
        assert_eq!(edge.0, RasterBlockId { block_row: 1, block_col: 1 });
        assert_eq!(edge.1.size.rows, 18);
        assert_eq!(edge.1.size.cols, 18);
    }

    #[test]
    fn enumerate_single_block_grid() {
        let shape = GridShape::new(10, 10);
        let block = BlockSize::new(32, 32);
        let grid = enumerate_blocks(shape, block);
        assert_eq!(grid.len(), 1);
        assert_eq!(grid[0].1.size.rows, 10);
        assert_eq!(grid[0].1.size.cols, 10);
    }

    #[test]
    fn apply_blocks_in_order_and_processes_each_block() {
        let shape = GridShape::new(64, 64);
        let block = BlockSize::new(32, 32);
        // Read closure: just return the block id back as pixels.
        let read = |id: RasterBlockId, _: &ReadWindow| -> std::result::Result<RasterBlockId, Box<dyn std::error::Error + Send + Sync>> {
            Ok(id)
        };
        // Worker: produce (block_row, block_col) tuple from the carried id.
        let worker = |blk: &RasterBlock<RasterBlockId>| -> std::result::Result<(usize, usize), Box<dyn std::error::Error + Send + Sync>> {
            Ok((blk.pixels.block_row, blk.pixels.block_col))
        };
        let out = apply_blocks(shape, block, read, &worker).unwrap();
        assert_eq!(out, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
    }

    #[test]
    fn apply_blocks_propagates_worker_error_with_block_id() {
        let shape = GridShape::new(64, 64);
        let block = BlockSize::new(32, 32);
        let read = |_: RasterBlockId, _: &ReadWindow| -> std::result::Result<i32, Box<dyn std::error::Error + Send + Sync>> {
            Ok(0)
        };
        // Worker fails on (0, 1).
        let worker = |blk: &RasterBlock<i32>| -> std::result::Result<i32, Box<dyn std::error::Error + Send + Sync>> {
            if blk.id.block_row == 0 && blk.id.block_col == 1 {
                Err("simulated failure".into())
            } else {
                Ok(blk.pixels)
            }
        };
        match apply_blocks(shape, block, read, &worker) {
            Err(KernelError::Worker { block_id, .. }) => {
                assert_eq!(block_id, "(0, 1)");
            }
            other => panic!("expected Worker error, got {other:?}"),
        }
    }

    #[test]
    fn apply_blocks_propagates_read_error() {
        let shape = GridShape::new(32, 32);
        let block = BlockSize::new(32, 32);
        let read = |_: RasterBlockId, _: &ReadWindow| -> std::result::Result<i32, Box<dyn std::error::Error + Send + Sync>> {
            Err("disk i/o".into())
        };
        let worker = |blk: &RasterBlock<i32>| -> std::result::Result<i32, Box<dyn std::error::Error + Send + Sync>> {
            Ok(blk.pixels)
        };
        let r = apply_blocks(shape, block, read, &worker);
        assert!(matches!(r, Err(KernelError::Worker { .. })));
    }

    #[test]
    fn apply_blocks_supports_send_pixels_across_threads() {
        // Demonstrates the type-bound: Pixels=Vec<u8> crosses the rayon
        // boundary fine. Worker sums the bytes.
        let shape = GridShape::new(8, 8);
        let block = BlockSize::new(4, 4);
        let read = |_: RasterBlockId, _: &ReadWindow| -> std::result::Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![1u8; 16])
        };
        let worker = |blk: &RasterBlock<Vec<u8>>| -> std::result::Result<u32, Box<dyn std::error::Error + Send + Sync>> {
            Ok(blk.pixels.iter().map(|b| u32::from(*b)).sum())
        };
        let out = apply_blocks(shape, block, read, &worker).unwrap();
        assert_eq!(out, vec![16, 16, 16, 16]);
    }

    #[test]
    #[allow(clippy::erasing_op)] // Reason: `x & 0` is intentional verifiability restore after CPU burn
    fn apply_blocks_concurrent_load_does_not_corrupt() {
        // 64 blocks with a worker that briefly busy-waits. Each output is
        // the block id squared. We check every result.
        let shape = GridShape::new(80, 80);
        let block = BlockSize::new(10, 10);
        let read = |id: RasterBlockId, _: &ReadWindow| -> std::result::Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            Ok(id.block_row * 8 + id.block_col)
        };
        let worker = |blk: &RasterBlock<usize>| -> std::result::Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            let mut x = blk.pixels;
            // tiny CPU burn to encourage thread interleaving.
            for _ in 0..1000 { x = x.wrapping_mul(2654435761).wrapping_add(1); }
            // restore for verifiability.
            Ok(blk.pixels.wrapping_mul(blk.pixels) ^ (x & 0))
        };
        let out = apply_blocks(shape, block, read, &worker).unwrap();
        assert_eq!(out.len(), 64);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, i.wrapping_mul(i), "block {i} produced wrong value");
        }
    }
}
