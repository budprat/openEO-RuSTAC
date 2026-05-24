//! **eo-vector** — polygon ops for Earth-observation pipelines.
//!
//! Implementations:
//! - [`PolygonRing`] — minimal polygon ring + bbox.
//! - [`ScanlineRasterizer`] — Sutherland–Hodgman-style point-in-polygon
//!   rasteriser using the even-odd rule (Foley & van Dam 1990).
//! - [`SimpleZonalStats`] — per-polygon mean over a 2-D raster, gated by
//!   the rasterised mask.
//!
//! All algorithms operate in pixel coordinates (the raster's local
//! integer grid), so callers must translate from CRS → pixel before
//! calling. That keeps this crate free of GDAL / proj.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::erasing_op, clippy::identity_op))]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─────────────────────────────────────────────────────────────────────
// PolygonRing
// ─────────────────────────────────────────────────────────────────────

/// A minimal polygon ring in *pixel coordinates*.
///
/// Outer ring; holes come later. May be open (last != first) — the
/// scanline algorithm closes implicitly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolygonRing {
    /// Vertices `(x, y)`, ≥ 3 points.
    pub vertices: Vec<(f64, f64)>,
}

impl PolygonRing {
    /// True iff the ring has at least 3 vertices.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.vertices.len() >= 3
    }

    /// Axis-aligned bounding box `[minx, miny, maxx, maxy]`. `None` if empty.
    #[must_use]
    pub fn bbox(&self) -> Option<[f64; 4]> {
        let mut it = self.vertices.iter().copied();
        let (mut minx, mut miny) = it.next()?;
        let (mut maxx, mut maxy) = (minx, miny);
        for (x, y) in it {
            if x < minx { minx = x; }
            if x > maxx { maxx = x; }
            if y < miny { miny = y; }
            if y > maxy { maxy = y; }
        }
        Some([minx, miny, maxx, maxy])
    }
}

// ─────────────────────────────────────────────────────────────────────
// Errors + traits
// ─────────────────────────────────────────────────────────────────────

/// Errors from rasteriser / zonal stats.
#[derive(Debug, Error)]
pub enum VectorError {
    /// Ring had fewer than 3 vertices.
    #[error("invalid polygon: ring has {0} vertex(es), need ≥ 3")]
    InvalidRing(usize),
    /// Raster slice length didn't match `rows × cols`.
    #[error("raster size mismatch: expected {expected} pixels, got {actual}")]
    SizeMismatch {
        /// Expected pixel count.
        expected: usize,
        /// Actual pixel count.
        actual: usize,
    },
}

/// Rasterise polygons onto a binary mask aligned to a reference raster.
pub trait Rasterizer {
    /// Backend error type.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Rasterise the polygon onto `rows × cols`. Pixel byte is 1 inside,
    /// 0 outside. Row-major.
    fn rasterise(
        &self,
        ring: &PolygonRing,
        rows: usize,
        cols: usize,
    ) -> Result<Vec<u8>, Self::Error>;
}

/// Compute per-polygon statistics.
pub trait ZonalStats {
    /// Backend error type.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Mean of raster values inside each polygon. Result index matches
    /// `rings` index.
    fn mean<T>(
        &self,
        rings: &[PolygonRing],
        raster: &[T],
        rows: usize,
        cols: usize,
    ) -> Result<Vec<f64>, Self::Error>
    where
        T: Copy + Into<f64>;
}

// ─────────────────────────────────────────────────────────────────────
// ScanlineRasterizer — even-odd rule
// ─────────────────────────────────────────────────────────────────────

/// Scanline rasteriser implementing the **even-odd rule** for
/// point-in-polygon classification.
///
/// For each pixel center `(x + 0.5, y + 0.5)` the algorithm counts how
/// many polygon edges cross a horizontal ray to the right; odd → inside.
/// Edge-on-vertex ambiguity is resolved by the standard half-open rule
/// (treat the lower of two endpoints as on-edge, the higher as off).
///
/// O(rows × edges) — good for moderate-vertex polygons against tile-size
/// rasters. For very high-vertex polygons consider an edge-table
/// sweepline impl in a future revision.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanlineRasterizer;

impl Rasterizer for ScanlineRasterizer {
    type Error = VectorError;

    fn rasterise(
        &self,
        ring: &PolygonRing,
        rows: usize,
        cols: usize,
    ) -> Result<Vec<u8>, Self::Error> {
        if !ring.is_valid() {
            return Err(VectorError::InvalidRing(ring.vertices.len()));
        }
        let mut out = vec![0u8; rows * cols];
        if rows == 0 || cols == 0 { return Ok(out); }

        let n = ring.vertices.len();
        // Reason: `is_valid()` above guarantees `vertices.len() >= 3`, so
        // `bbox()` returns `Some`. Using `unreachable!` keeps the invariant
        // documented without an `expect_used` lint.
        let bbox = match ring.bbox() {
            Some(b) => b,
            None => unreachable!("is_valid() guarantees a non-empty ring"),
        };
        let y_start = (bbox[1].floor().max(0.0) as usize).min(rows.saturating_sub(1));
        let y_end = (bbox[3].ceil() as usize).min(rows);

        for y in y_start..y_end {
            let yc = y as f64 + 0.5;
            // Find x-intersections of horizontal line y=yc with edges.
            let mut xs: Vec<f64> = Vec::with_capacity(8);
            for i in 0..n {
                let (x0, y0) = ring.vertices[i];
                let (x1, y1) = ring.vertices[(i + 1) % n];
                // Half-open: include the lower endpoint, exclude the upper.
                let (ya, yb) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
                if yc < ya || yc >= yb { continue; }
                if (y1 - y0).abs() < f64::EPSILON { continue; } // horizontal
                let t = (yc - y0) / (y1 - y0);
                xs.push(x0 + t * (x1 - x0));
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // Fill between pairs.
            let mut i = 0;
            while i + 1 < xs.len() {
                let x_lo = xs[i];
                let x_hi = xs[i + 1];
                let x_start = (x_lo.ceil().max(0.0) as usize).min(cols);
                let x_end = (x_hi.floor() as usize + 1).min(cols);
                for x in x_start..x_end {
                    // Pixel center inside [x_lo, x_hi]?
                    let xc = x as f64 + 0.5;
                    if xc >= x_lo && xc < x_hi {
                        out[y * cols + x] = 1;
                    }
                }
                i += 2;
            }
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────────────────────────────
// SimpleZonalStats
// ─────────────────────────────────────────────────────────────────────

/// Per-polygon zonal mean using [`ScanlineRasterizer`] for the mask.
#[derive(Debug, Default, Clone, Copy)]
pub struct SimpleZonalStats;

impl ZonalStats for SimpleZonalStats {
    type Error = VectorError;

    fn mean<T>(
        &self,
        rings: &[PolygonRing],
        raster: &[T],
        rows: usize,
        cols: usize,
    ) -> Result<Vec<f64>, Self::Error>
    where
        T: Copy + Into<f64>,
    {
        if raster.len() != rows * cols {
            return Err(VectorError::SizeMismatch {
                expected: rows * cols,
                actual: raster.len(),
            });
        }
        let raster_f: Vec<f64> = raster.iter().map(|&v| v.into()).collect();
        let mut out = Vec::with_capacity(rings.len());
        let rasteriser = ScanlineRasterizer;
        for ring in rings {
            let mask = rasteriser.rasterise(ring, rows, cols)?;
            let (mut sum, mut count) = (0.0_f64, 0u64);
            for (m, v) in mask.iter().zip(raster_f.iter()) {
                if *m == 1 {
                    sum += *v;
                    count += 1;
                }
            }
            out.push(if count > 0 { sum / count as f64 } else { f64::NAN });
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn count_set(mask: &[u8]) -> usize {
        mask.iter().filter(|&&b| b == 1).count()
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> PolygonRing {
        PolygonRing {
            vertices: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
        }
    }

    // ── PolygonRing ────────────────────────────────────────────────

    #[test]
    fn ring_is_valid_requires_three_vertices() {
        let two = PolygonRing { vertices: vec![(0.0, 0.0), (1.0, 0.0)] };
        assert!(!two.is_valid());
        let triangle = PolygonRing { vertices: vec![(0.0, 0.0), (1.0, 0.0), (0.5, 1.0)] };
        assert!(triangle.is_valid());
    }

    #[test]
    fn ring_bbox_finds_extremes() {
        let r = PolygonRing { vertices: vec![(-1.0, 2.0), (3.0, -4.0), (5.0, 6.0)] };
        assert_eq!(r.bbox(), Some([-1.0, -4.0, 5.0, 6.0]));
    }

    #[test]
    fn ring_bbox_none_for_empty() {
        let r = PolygonRing { vertices: vec![] };
        assert_eq!(r.bbox(), None);
    }

    #[test]
    fn ring_serde_roundtrip() {
        let r = rect(0.0, 0.0, 4.0, 4.0);
        let s = serde_json::to_string(&r).unwrap();
        let back: PolygonRing = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    // ── ScanlineRasterizer ─────────────────────────────────────────

    #[test]
    fn raster_rejects_degenerate_ring() {
        let two = PolygonRing { vertices: vec![(0.0, 0.0), (1.0, 0.0)] };
        let r = ScanlineRasterizer.rasterise(&two, 10, 10);
        assert!(matches!(r, Err(VectorError::InvalidRing(2))));
    }

    #[test]
    fn raster_axis_aligned_rectangle_exact_area() {
        // Rectangle covering pixels (1..4, 1..4) → 9 pixels.
        let r = rect(1.0, 1.0, 4.0, 4.0);
        let mask = ScanlineRasterizer.rasterise(&r, 8, 8).unwrap();
        assert_eq!(count_set(&mask), 9, "3x3 area should be 9 pixels");
        // Spot-check corners and outside.
        assert_eq!(mask[1 * 8 + 1], 1);
        assert_eq!(mask[3 * 8 + 3], 1);
        assert_eq!(mask[0 * 8 + 0], 0);
        assert_eq!(mask[4 * 8 + 4], 0);
    }

    #[test]
    fn raster_full_grid_polygon() {
        let r = rect(0.0, 0.0, 4.0, 4.0);
        let mask = ScanlineRasterizer.rasterise(&r, 4, 4).unwrap();
        assert_eq!(count_set(&mask), 16);
    }

    #[test]
    fn raster_triangle_approximates_half_area() {
        // Right triangle filling the lower-left half of a 10×10 grid.
        let tri = PolygonRing { vertices: vec![(0.0, 0.0), (10.0, 0.0), (0.0, 10.0)] };
        let mask = ScanlineRasterizer.rasterise(&tri, 10, 10).unwrap();
        let n = count_set(&mask);
        // Expect ≈ 50 (lower triangle on a 10×10 grid). Allow ±5 for
        // discretisation. Half-open edges may exclude the diagonal.
        assert!((40..=55).contains(&n), "got {n}");
    }

    #[test]
    fn raster_clamps_to_image_bounds() {
        // Polygon partly outside the raster — must not write OOB.
        let r = rect(-2.0, -2.0, 3.0, 3.0);
        let mask = ScanlineRasterizer.rasterise(&r, 5, 5).unwrap();
        assert_eq!(mask.len(), 25);
        // Inside the overlap region (0..3, 0..3) → 9 pixels.
        assert_eq!(count_set(&mask), 9);
    }

    #[test]
    fn raster_polygon_fully_outside_image_is_empty() {
        let r = rect(100.0, 100.0, 110.0, 110.0);
        let mask = ScanlineRasterizer.rasterise(&r, 10, 10).unwrap();
        assert_eq!(count_set(&mask), 0);
    }

    #[test]
    fn raster_empty_dimensions_returns_empty_vec() {
        let r = rect(0.0, 0.0, 4.0, 4.0);
        assert!(ScanlineRasterizer.rasterise(&r, 0, 5).unwrap().is_empty());
        assert!(ScanlineRasterizer.rasterise(&r, 5, 0).unwrap().is_empty());
    }

    // ── SimpleZonalStats ───────────────────────────────────────────

    #[test]
    fn zonal_mean_uniform_raster_equals_constant() {
        // 4×4 raster of all 7.0; rectangle covers 2×2 region.
        let raster = vec![7.0_f64; 16];
        let r = rect(1.0, 1.0, 3.0, 3.0);
        let means = SimpleZonalStats
            .mean(&[r], &raster, 4, 4)
            .unwrap();
        assert_eq!(means.len(), 1);
        assert!((means[0] - 7.0).abs() < 1e-9);
    }

    #[test]
    fn zonal_mean_with_size_mismatch_errors() {
        let raster = vec![0u8; 9]; // claim 4×4 → 16 expected
        let r = rect(0.0, 0.0, 4.0, 4.0);
        let e = SimpleZonalStats
            .mean(&[r], &raster, 4, 4)
            .unwrap_err();
        assert!(matches!(e, VectorError::SizeMismatch { expected: 16, actual: 9 }));
    }

    #[test]
    fn zonal_mean_multiple_polygons() {
        let raster: Vec<f64> = (0..16).map(|i| i as f64).collect();
        let upper_left = rect(0.0, 0.0, 2.0, 2.0);   // pixels (0,0),(1,0),(0,1),(1,1) → 0,1,4,5 → mean 2.5
        let lower_right = rect(2.0, 2.0, 4.0, 4.0);  // pixels (2,2),(3,2),(2,3),(3,3) → 10,11,14,15 → mean 12.5
        let means = SimpleZonalStats
            .mean(&[upper_left, lower_right], &raster, 4, 4)
            .unwrap();
        assert!((means[0] - 2.5).abs() < 1e-9, "got {}", means[0]);
        assert!((means[1] - 12.5).abs() < 1e-9, "got {}", means[1]);
    }

    #[test]
    fn zonal_mean_polygon_outside_image_is_nan() {
        let raster = vec![1.0_f64; 16];
        let r = rect(100.0, 100.0, 110.0, 110.0);
        let means = SimpleZonalStats.mean(&[r], &raster, 4, 4).unwrap();
        assert!(means[0].is_nan(), "outside-poly should yield NaN");
    }
}
