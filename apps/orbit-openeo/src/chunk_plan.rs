//! Phase-B #2 — chunk planner.
//!
//! Decides block size from COG tile size, memory budget, thread count,
//! operation type, and output format. Replaces the hard-coded
//! `BlockSize { 256, 256 }` in `geo_executor`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors the planner can surface.
#[derive(Debug, Error, PartialEq)]
pub enum PlanError {
    /// Memory budget is too tight to satisfy minimum block.
    #[error("memory budget {budget} too small for {n_threads} threads × {min_block_bytes} B")]
    BudgetTooSmall { budget: usize, n_threads: usize, min_block_bytes: usize },
}

/// Hint about the operation's per-pixel memory cost.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// Cheap per-pixel arithmetic (NDVI, math).
    Pixelwise,
    /// Reduction over a dimension (mean/min/max over time).
    Reduction,
    /// Window / convolution kernel.
    Window,
}

impl OpKind {
    /// Bytes per pixel of intermediate state.
    #[must_use]
    pub fn bytes_per_pixel_overhead(self) -> usize {
        match self {
            Self::Pixelwise => 8,    // f32 in/out
            Self::Reduction => 16,   // accumulator + count
            Self::Window => 32,      // input + halo + scratch
        }
    }
}

/// Inputs the planner needs from each cube.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanInput {
    /// COG internal tile size (typically 512 or 256).
    pub cog_tile_size: u32,
    /// Output rows × cols.
    pub rows: usize,
    /// Output cols.
    pub cols: usize,
    /// Number of bands.
    pub bands: usize,
    /// Number of time steps.
    pub times: usize,
    /// Bytes per element (1 for u8, 2 for i16, 4 for f32).
    pub bytes_per_pixel: usize,
}

/// Selected block size + concurrency settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkPlan {
    /// Block height in pixels.
    pub block_rows: usize,
    /// Block width in pixels.
    pub block_cols: usize,
    /// Number of worker threads to use.
    pub n_threads: usize,
    /// Estimated peak memory in bytes given the chosen settings.
    pub est_peak_bytes: usize,
}

impl ChunkPlan {
    /// Plan a chunk strategy.
    ///
    /// Rules:
    /// 1. Prefer aligning block size to a multiple of the COG tile size.
    /// 2. Each block must fit in `memory_budget_bytes / n_threads`.
    /// 3. Don't go below 64×64 (too small wastes scheduler overhead).
    /// 4. Don't go above 4096×4096 (cache-unfriendly).
    pub fn plan(
        input: PlanInput,
        op: OpKind,
        n_threads: usize,
        memory_budget_bytes: usize,
    ) -> Result<Self, PlanError> {
        let n_threads = n_threads.max(1);
        let cog = input.cog_tile_size as usize;
        let per_pixel = input.bytes_per_pixel
            + op.bytes_per_pixel_overhead() * input.bands.max(1) * input.times.max(1);
        const MIN_BLOCK: usize = 64;
        const MAX_BLOCK: usize = 4096;
        let min_block_bytes = MIN_BLOCK * MIN_BLOCK * per_pixel;
        let budget_per_thread = memory_budget_bytes / n_threads;
        if budget_per_thread < min_block_bytes {
            return Err(PlanError::BudgetTooSmall {
                budget: memory_budget_bytes,
                n_threads,
                min_block_bytes,
            });
        }
        // Largest square that fits the per-thread budget.
        let max_side_by_budget = (budget_per_thread / per_pixel)
            .max(1)
            .min(MAX_BLOCK * MAX_BLOCK)
            .isqrt();
        let target = max_side_by_budget.clamp(MIN_BLOCK, MAX_BLOCK);
        // Snap down to nearest COG tile multiple if reasonable.
        let snapped = if cog >= MIN_BLOCK && cog <= target {
            (target / cog) * cog
        } else {
            target
        };
        let block = snapped
            .max(MIN_BLOCK)
            .min(MAX_BLOCK)
            .min(input.rows.max(MIN_BLOCK))
            .min(input.cols.max(MIN_BLOCK));
        Ok(Self {
            block_rows: block,
            block_cols: block,
            n_threads,
            est_peak_bytes: block * block * per_pixel * n_threads,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s2_input() -> PlanInput {
        PlanInput {
            cog_tile_size: 512,
            rows: 10980,
            cols: 10980,
            bands: 2,
            times: 1,
            bytes_per_pixel: 2,
        }
    }

    #[test]
    fn plan_clamps_to_min_block_floor() {
        let p = ChunkPlan::plan(
            s2_input(),
            OpKind::Pixelwise,
            8,
            512 * 1024 * 1024,
        ).unwrap();
        assert!(p.block_rows >= 64);
        assert!(p.block_cols >= 64);
    }

    #[test]
    fn plan_clamps_to_max_block_ceiling() {
        let p = ChunkPlan::plan(
            s2_input(),
            OpKind::Pixelwise,
            1,
            16 * 1024 * 1024 * 1024,
        ).unwrap();
        assert!(p.block_rows <= 4096);
    }

    #[test]
    fn plan_snaps_to_cog_tile_multiple_when_reasonable() {
        let p = ChunkPlan::plan(
            s2_input(),
            OpKind::Pixelwise,
            4,
            1 * 1024 * 1024 * 1024,
        ).unwrap();
        assert_eq!(p.block_rows % 512, 0, "block must align to COG tile");
    }

    #[test]
    fn plan_errors_on_starvation_budget() {
        let r = ChunkPlan::plan(
            s2_input(),
            OpKind::Window,
            16,
            16 * 1024,
        );
        assert!(matches!(r, Err(PlanError::BudgetTooSmall { .. })));
    }

    #[test]
    fn plan_respects_thread_count() {
        let p = ChunkPlan::plan(
            s2_input(),
            OpKind::Pixelwise,
            8,
            512 * 1024 * 1024,
        ).unwrap();
        assert_eq!(p.n_threads, 8);
    }

    #[test]
    fn op_kind_overhead_increases_with_complexity() {
        assert!(OpKind::Pixelwise.bytes_per_pixel_overhead() < OpKind::Reduction.bytes_per_pixel_overhead());
        assert!(OpKind::Reduction.bytes_per_pixel_overhead() < OpKind::Window.bytes_per_pixel_overhead());
    }

    #[test]
    fn plan_serialises_round_trip() {
        let p = ChunkPlan {
            block_rows: 512, block_cols: 512, n_threads: 4, est_peak_bytes: 1024,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: ChunkPlan = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn plan_smaller_raster_clamps_block_to_image_extent() {
        let mut input = s2_input();
        input.rows = 100;
        input.cols = 100;
        let p = ChunkPlan::plan(input, OpKind::Pixelwise, 1, 1024 * 1024 * 1024).unwrap();
        assert!(p.block_rows <= 100.max(64));
    }

    #[test]
    fn plan_zero_threads_treated_as_one() {
        let p = ChunkPlan::plan(s2_input(), OpKind::Pixelwise, 0, 1024 * 1024 * 1024).unwrap();
        assert_eq!(p.n_threads, 1);
    }
}
