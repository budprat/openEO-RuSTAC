//! Reduction kernels — collapse a series of blocks into a single output
//! along the time, layer, or pixel axis.

use serde::{Deserialize, Serialize};

/// Built-in reduction strategies.
///
/// Each variant has a closed-form mathematical definition documented below.
/// Implementations live in `eo-io` (for GDAL-backed reductions) or in
/// downstream apps; this enum is just a tag for routing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReduceKind {
    /// Arithmetic mean of all non-NoData pixels.
    Mean,
    /// Median of all non-NoData pixels (50th percentile, type-preserving).
    Median,
    /// Minimum across non-NoData pixels.
    Min,
    /// Maximum across non-NoData pixels.
    Max,
    /// Sum of all non-NoData pixels (no overflow check; caller picks dtype).
    Sum,
    /// Count of non-NoData pixels.
    Count,
    /// Most-recent valid pixel (requires temporal axis).
    MostRecent,
    /// Pick the time step that maximises a derived index (e.g. max-NDVI).
    MaxIndex,
    /// Generic Nth percentile in [0, 100].
    Percentile {
        /// Percentile (0–100).
        p: u8,
    },
}

impl ReduceKind {
    /// True iff this reduction requires a temporal axis.
    #[must_use]
    pub const fn requires_time_axis(self) -> bool {
        matches!(self, Self::MostRecent | Self::MaxIndex)
    }
}

/// A reduction worker collapses `Vec<Input>` into `Output`.
///
/// Distinct from [`crate::BlockWorker`] because reductions own multiple
/// inputs whereas a worker is point-wise.
pub trait ReductionWorker<Input, Output>: Send + Sync {
    /// Reduce the input slice into a single output value.
    fn reduce(
        &self,
        inputs: &[Input],
    ) -> std::result::Result<Output, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_time_axis_only_for_temporal_kinds() {
        assert!(ReduceKind::MostRecent.requires_time_axis());
        assert!(ReduceKind::MaxIndex.requires_time_axis());
        assert!(!ReduceKind::Mean.requires_time_axis());
        assert!(!ReduceKind::Median.requires_time_axis());
        assert!(!ReduceKind::Sum.requires_time_axis());
        assert!(!ReduceKind::Count.requires_time_axis());
        assert!(!ReduceKind::Percentile { p: 50 }.requires_time_axis());
    }

    #[test]
    fn reduce_kind_serde() {
        let k = ReduceKind::Percentile { p: 95 };
        let s = serde_json::to_string(&k).unwrap();
        assert!(s.contains("Percentile"));
        assert!(s.contains("95"));
        let back: ReduceKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);
    }
}
