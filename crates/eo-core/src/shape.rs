//! Shape and dimension descriptors for raster tensors.

use serde::{Deserialize, Serialize};

/// Shape of a 4-D raster tensor: `(times, layers, rows, cols)`.
///
/// `times` collapses to 1 for single-scene rasters; `layers` is the number
/// of spectral bands or feature channels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RasterShape {
    /// Number of time steps.
    pub times: usize,
    /// Number of layers / spectral bands.
    pub layers: usize,
    /// Number of rows.
    pub rows: usize,
    /// Number of columns.
    pub cols: usize,
}

impl RasterShape {
    /// Total element count: `times × layers × rows × cols`. Saturates on overflow.
    #[must_use]
    pub const fn total_elements(&self) -> usize {
        self.times
            .saturating_mul(self.layers)
            .saturating_mul(self.rows)
            .saturating_mul(self.cols)
    }

    /// True iff every dimension is non-zero.
    #[must_use]
    pub const fn is_non_empty(&self) -> bool {
        self.times > 0 && self.layers > 0 && self.rows > 0 && self.cols > 0
    }
}

/// Axis a reduction collapses.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dimension {
    /// Reduce across the layer axis (e.g. per-pixel band index).
    #[default]
    Layer,
    /// Reduce across the time axis (e.g. mean composite across timesteps).
    Time,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shape_has_all_zeroes() {
        let s = RasterShape::default();
        assert_eq!(s.times, 0);
        assert!(!s.is_non_empty());
    }

    #[test]
    fn total_elements_multiplies_all_axes() {
        let s = RasterShape { times: 2, layers: 3, rows: 4, cols: 5 };
        assert_eq!(s.total_elements(), 2 * 3 * 4 * 5);
    }

    #[test]
    fn total_elements_saturates_on_overflow() {
        let s = RasterShape { times: usize::MAX, layers: 2, rows: 2, cols: 2 };
        assert_eq!(s.total_elements(), usize::MAX);
    }

    #[test]
    fn is_non_empty_requires_every_axis() {
        let s = RasterShape { times: 1, layers: 1, rows: 1, cols: 0 };
        assert!(!s.is_non_empty());
        let s = RasterShape { times: 1, layers: 1, rows: 1, cols: 1 };
        assert!(s.is_non_empty());
    }

    #[test]
    fn dimension_default_is_layer() {
        assert_eq!(Dimension::default(), Dimension::Layer);
    }

    #[test]
    fn dimension_serde_uses_variant_name() {
        let json = serde_json::to_string(&Dimension::Time).unwrap();
        assert!(json.contains("Time"));
    }
}
