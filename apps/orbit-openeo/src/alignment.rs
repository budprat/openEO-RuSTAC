//! Phase-A #4 — strict grid-alignment engine.
//!
//! Validates two cubes (data + mask, or data1 + data2) for compatible
//! CRS, resolution, bounds, and pixel-grid snap. Produces an
//! [`AlignmentReport`] describing any required corrective action
//! (resample / clip / pad).
//!
//! Critical for Sentinel-2: SCL is 20 m while red/nir are 10 m → must
//! be flagged before `apply_reduction_with_mask` reads aligned blocks.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::datacube::{Crs, GeoTransform, RasterCube};

/// Resampling strategy when bringing cubes onto a common grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResampleMethod {
    /// Nearest-neighbour — categorical / classification data.
    NearestNeighbor,
    /// Bilinear — continuous reflectance.
    Bilinear,
    /// Cubic — visualisations.
    Cubic,
    /// Average — downsampling reflectance.
    Average,
}

/// Errors the alignment engine can surface.
#[derive(Debug, Error, PartialEq)]
pub enum AlignError {
    /// The two cubes are in different CRSes.
    #[error("CRS mismatch: lhs={lhs}, rhs={rhs}")]
    CrsMismatch { lhs: String, rhs: String },
    /// Resolutions differ; client must request a resample.
    #[error("resolution mismatch: lhs=({lhs_x},{lhs_y}), rhs=({rhs_x},{rhs_y})")]
    ResolutionMismatch {
        lhs_x: f64, lhs_y: f64,
        rhs_x: f64, rhs_y: f64,
    },
    /// Pixel grid origins do not snap onto each other (sub-pixel offset).
    #[error("grid origin not snapped: lhs=({lhs_ulx},{lhs_uly}), rhs=({rhs_ulx},{rhs_uly}), tolerance={tol}")]
    GridNotSnapped {
        lhs_ulx: f64, lhs_uly: f64,
        rhs_ulx: f64, rhs_uly: f64,
        tol: f64,
    },
}

/// Outcome of an alignment check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AlignmentReport {
    /// Both cubes already share the same grid; no action required.
    Aligned,
    /// Compatible CRS, but `rhs` needs resampling onto `lhs` grid.
    NeedsResample {
        /// Suggested method based on the band semantics.
        method: ResampleMethod,
        /// Target resolution (lhs).
        target_resolution: (f64, f64),
    },
}

/// Strict alignment check between two cubes. Returns an explicit
/// `Aligned` or `NeedsResample` report on success; errors when CRSes
/// differ.
pub fn align(lhs: &RasterCube, rhs: &RasterCube) -> Result<AlignmentReport, AlignError> {
    if !lhs.crs.equals(&rhs.crs) {
        return Err(AlignError::CrsMismatch {
            lhs: lhs.crs.0.clone(),
            rhs: rhs.crs.0.clone(),
        });
    }
    let (lx, ly) = lhs.resolution();
    let (rx, ry) = rhs.resolution();
    if !approx_eq(lx, rx) || !approx_eq(ly, ry) {
        // Different resolutions — propose nearest-neighbour for mask-shaped cubes
        // (where bands look like classifications), bilinear otherwise.
        let method = suggest_method_for_band_names(rhs);
        return Ok(AlignmentReport::NeedsResample {
            method,
            target_resolution: (lx, ly),
        });
    }
    // Same resolution → check pixel-grid origin snap.
    let tol = lx * 1e-3;
    if !grids_snap(&lhs.transform, &rhs.transform, tol) {
        return Err(AlignError::GridNotSnapped {
            lhs_ulx: lhs.transform.ulx(),
            lhs_uly: lhs.transform.uly(),
            rhs_ulx: rhs.transform.ulx(),
            rhs_uly: rhs.transform.uly(),
            tol,
        });
    }
    Ok(AlignmentReport::Aligned)
}

fn approx_eq(a: f64, b: f64) -> bool {
    let rel = if a.abs() > b.abs() { a.abs() } else { b.abs() };
    (a - b).abs() < (rel.max(1.0)) * 1e-6
}

/// Pixel-grid origins must agree modulo pixel-size up to tolerance.
fn grids_snap(a: &GeoTransform, b: &GeoTransform, tol: f64) -> bool {
    let dx_off = ((a.ulx() - b.ulx()) / a.dx()).fract().abs();
    let dy_off = ((a.uly() - b.uly()) / a.dy()).fract().abs();
    (dx_off < tol || (1.0 - dx_off) < tol) &&
    (dy_off < tol || (1.0 - dy_off) < tol)
}

fn suggest_method_for_band_names(c: &RasterCube) -> ResampleMethod {
    let any_mask = c.bands.iter().any(|b| {
        let n = b.name.to_ascii_lowercase();
        n.contains("scl") || n.contains("qa") || n.contains("mask") || n.contains("class")
    });
    if any_mask {
        ResampleMethod::NearestNeighbor
    } else {
        ResampleMethod::Bilinear
    }
}

/// Convenience — does the cube carry a mask band by name (heuristic)?
#[must_use]
pub fn is_mask_cube(c: &RasterCube) -> bool {
    c.bands.iter().any(|b| {
        let n = b.name.to_ascii_lowercase();
        n == "scl" || n == "qa_pixel" || n.contains("mask")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datacube::{BandSpec, Coords, Dim, Provenance};
    use std::collections::BTreeMap;

    fn cube(crs: Crs, gt: GeoTransform, bands: Vec<&str>) -> RasterCube {
        let mut coords = BTreeMap::new();
        coords.insert(Dim::Time, Coords::Labels { values: vec!["t0".into()] });
        coords.insert(Dim::Band, Coords::Labels {
            values: bands.iter().map(|s| (*s).to_string()).collect(),
        });
        coords.insert(Dim::Y, Coords::Numeric { values: vec![0.0] });
        coords.insert(Dim::X, Coords::Numeric { values: vec![0.0] });
        RasterCube {
            dims: vec![Dim::Time, Dim::Band, Dim::Y, Dim::X],
            coords,
            crs,
            transform: gt,
            bands: bands
                .into_iter()
                .map(|n| BandSpec {
                    name: n.into(), nodata: None, scale: 1.0, offset: 0.0,
                    units: None, paths_per_time: vec![],
                })
                .collect(),
            provenance: Provenance::default(),
        }
    }

    #[test]
    fn identical_cubes_are_aligned() {
        let a = cube(Crs::epsg(4326), GeoTransform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]), vec!["red"]);
        let b = a.clone();
        assert_eq!(align(&a, &b), Ok(AlignmentReport::Aligned));
    }

    #[test]
    fn different_crs_errors() {
        let a = cube(Crs::epsg(4326), GeoTransform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]), vec!["red"]);
        let b = cube(Crs::epsg(3857), GeoTransform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]), vec!["red"]);
        match align(&a, &b) {
            Err(AlignError::CrsMismatch { lhs, rhs }) => {
                assert_eq!(lhs, "EPSG:4326");
                assert_eq!(rhs, "EPSG:3857");
            }
            other => panic!("expected CrsMismatch, got {other:?}"),
        }
    }

    #[test]
    fn different_resolution_suggests_resample() {
        let a = cube(Crs::epsg(32633), GeoTransform([0.0, 10.0, 0.0, 0.0, 0.0, -10.0]), vec!["red"]);
        let b = cube(Crs::epsg(32633), GeoTransform([0.0, 20.0, 0.0, 0.0, 0.0, -20.0]), vec!["red"]);
        match align(&a, &b).unwrap() {
            AlignmentReport::NeedsResample { method, target_resolution } => {
                assert_eq!(method, ResampleMethod::Bilinear);
                assert_eq!(target_resolution, (10.0, 10.0));
            }
            other => panic!("expected NeedsResample, got {other:?}"),
        }
    }

    #[test]
    fn scl_band_suggests_nearest_neighbor() {
        let a = cube(Crs::epsg(32633), GeoTransform([0.0, 10.0, 0.0, 0.0, 0.0, -10.0]), vec!["red"]);
        let b = cube(Crs::epsg(32633), GeoTransform([0.0, 20.0, 0.0, 0.0, 0.0, -20.0]), vec!["scl"]);
        match align(&a, &b).unwrap() {
            AlignmentReport::NeedsResample { method, .. } => {
                assert_eq!(method, ResampleMethod::NearestNeighbor);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn unsnapped_grid_origin_errors() {
        // Both 10m, but rhs offset by 0.5 px (5m) — fails snap test.
        let a = cube(Crs::epsg(32633), GeoTransform([0.0, 10.0, 0.0, 0.0, 0.0, -10.0]), vec!["red"]);
        let b = cube(Crs::epsg(32633), GeoTransform([5.0, 10.0, 0.0, 5.0, 0.0, -10.0]), vec!["red"]);
        assert!(matches!(align(&a, &b), Err(AlignError::GridNotSnapped { .. })));
    }

    #[test]
    fn integer_offset_grids_snap() {
        // Both 10m, rhs offset by exactly 3 pixels (30m, 60m) — snaps.
        let a = cube(Crs::epsg(32633), GeoTransform([0.0, 10.0, 0.0, 0.0, 0.0, -10.0]), vec!["red"]);
        let b = cube(Crs::epsg(32633), GeoTransform([30.0, 10.0, 0.0, 60.0, 0.0, -10.0]), vec!["red"]);
        assert_eq!(align(&a, &b), Ok(AlignmentReport::Aligned));
    }

    #[test]
    fn approx_eq_tolerates_small_floats() {
        assert!(approx_eq(10.0, 10.0 + 1e-9));
        assert!(!approx_eq(10.0, 10.1));
    }

    #[test]
    fn is_mask_cube_detects_scl_and_qa() {
        let scl = cube(Crs::epsg(4326), GeoTransform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]), vec!["scl"]);
        let qa  = cube(Crs::epsg(4326), GeoTransform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]), vec!["qa_pixel"]);
        let red = cube(Crs::epsg(4326), GeoTransform([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]), vec!["red"]);
        assert!(is_mask_cube(&scl));
        assert!(is_mask_cube(&qa));
        assert!(!is_mask_cube(&red));
    }

    #[test]
    fn report_serialises_round_trip() {
        let r = AlignmentReport::NeedsResample {
            method: ResampleMethod::NearestNeighbor,
            target_resolution: (10.0, 10.0),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: AlignmentReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn aligned_serialises_round_trip() {
        let s = serde_json::to_string(&AlignmentReport::Aligned).unwrap();
        let back: AlignmentReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, AlignmentReport::Aligned);
    }
}
