//! Phase-A #1 — typed `RasterCube` / `DataCube` model with named
//! dimensions and coordinates (xarray-style).
//!
//! Replaces the leaky `__cube` JSON envelope that `GeoExecutor` ships
//! between process arms today. Carries everything the alignment engine
//! (Phase-A #4) needs to validate compatibility: CRS, geo-transform,
//! resolution, nodata, units, scale/offset, plus per-dimension
//! coordinate vectors and provenance.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Named cube dimensions per openEO 1.3.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dim {
    /// Time axis.
    Time,
    /// Spectral band axis.
    Band,
    /// Spatial Y / latitude axis.
    Y,
    /// Spatial X / longitude axis.
    X,
}

impl Dim {
    /// Stable id string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Band => "band",
            Self::Y => "y",
            Self::X => "x",
        }
    }
}

/// Coordinates along one dimension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Coords {
    /// Numeric values (e.g. y/x axes in CRS units).
    Numeric { values: Vec<f64> },
    /// String labels (e.g. band names, ISO timestamps).
    Labels { values: Vec<String> },
}

impl Coords {
    /// Number of coordinate ticks.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Numeric { values } => values.len(),
            Self::Labels { values } => values.len(),
        }
    }
    /// True iff the axis has no ticks.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

/// Geo-transform — affine pixel→world mapping (GDAL convention).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeoTransform(pub [f64; 6]);

impl GeoTransform {
    /// Top-left origin x.
    #[must_use] pub fn ulx(&self) -> f64 { self.0[0] }
    /// Pixel width (positive).
    #[must_use] pub fn dx(&self) -> f64 { self.0[1] }
    /// Top-left origin y.
    #[must_use] pub fn uly(&self) -> f64 { self.0[3] }
    /// Pixel height (negative for north-up).
    #[must_use] pub fn dy(&self) -> f64 { self.0[5] }
}

/// Coordinate Reference System identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Crs(pub String);

impl Crs {
    /// EPSG-code constructor.
    #[must_use]
    pub fn epsg(code: u32) -> Self { Self(format!("EPSG:{code}")) }

    /// True iff this is the same CRS as `other`.
    #[must_use]
    pub fn equals(&self, other: &Self) -> bool { self.0 == other.0 }
}

/// Provenance — every cube records where it came from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Source STAC item ids contributing to the cube.
    pub stac_ids: Vec<String>,
    /// Hash of the process graph that produced the cube.
    pub graph_hash: Option<String>,
    /// Software version emitting the cube.
    pub software_version: Option<String>,
    /// Wall-clock processing timestamp (RFC 3339).
    pub processed_at: Option<String>,
}

/// Errors a cube construction or lookup can surface.
#[derive(Debug, Error, PartialEq)]
pub enum CubeError {
    /// A required band wasn't carried by this cube.
    #[error("missing band: {0}")]
    MissingBand(String),
    /// Coordinate length doesn't match the dim size.
    #[error("coord length mismatch on dim {dim}: expected {expected}, got {actual}")]
    CoordLenMismatch { dim: &'static str, expected: usize, actual: usize },
    /// A required dimension wasn't present.
    #[error("missing dimension: {0}")]
    MissingDim(&'static str),
}

/// Per-band metadata: nodata, scale/offset, units.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BandSpec {
    /// Logical band name (e.g. "red", "nir", "scl").
    pub name: String,
    /// IEEE float / int sentinel for missing pixels.
    pub nodata: Option<f64>,
    /// `value = raw * scale + offset`. Default 1.0.
    #[serde(default = "one")]
    pub scale: f64,
    /// `value = raw * scale + offset`. Default 0.0.
    #[serde(default)]
    pub offset: f64,
    /// SI unit string (e.g. "1", "K", "m").
    #[serde(default)]
    pub units: Option<String>,
    /// Local file path to the raster carrying this band (one per time
    /// step if needed — see `paths_per_time`).
    pub paths_per_time: Vec<PathBuf>,
}

fn one() -> f64 { 1.0 }

/// Typed raster datacube.
///
/// Carries named dimensions, per-dim coordinates, CRS, geo-transform,
/// nodata/scale/offset per band, and provenance — everything the
/// alignment engine + executor need to validate compatibility without
/// re-reading the source rasters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RasterCube {
    /// Ordered dimensions (typically `[Time, Band, Y, X]`).
    pub dims: Vec<Dim>,
    /// Coordinate vectors keyed by dim.
    pub coords: BTreeMap<Dim, Coords>,
    /// CRS.
    pub crs: Crs,
    /// Geo-transform (pixel → world).
    pub transform: GeoTransform,
    /// Per-band specs.
    pub bands: Vec<BandSpec>,
    /// Per-cube provenance.
    pub provenance: Provenance,
}

impl RasterCube {
    /// Validate that every dim in `dims` has matching coords whose
    /// length equals the band-list size for `Band`, and whose lengths
    /// for Time/Y/X are consistent.
    pub fn validate(&self) -> Result<(), CubeError> {
        for dim in &self.dims {
            let coords = self
                .coords
                .get(dim)
                .ok_or(CubeError::MissingDim(dim.as_str()))?;
            if *dim == Dim::Band && coords.len() != self.bands.len() {
                return Err(CubeError::CoordLenMismatch {
                    dim: dim.as_str(),
                    expected: self.bands.len(),
                    actual: coords.len(),
                });
            }
        }
        Ok(())
    }

    /// Look up a band by logical name.
    pub fn band(&self, name: &str) -> Result<&BandSpec, CubeError> {
        self.bands
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| CubeError::MissingBand(name.into()))
    }

    /// Spatial pixel size `(dx, |dy|)` from the geo-transform.
    #[must_use]
    pub fn resolution(&self) -> (f64, f64) {
        (self.transform.dx().abs(), self.transform.dy().abs())
    }

    /// Number of time steps (length of the Time coord, or 1 if absent).
    #[must_use]
    pub fn time_steps(&self) -> usize {
        self.coords.get(&Dim::Time).map(|c| c.len()).unwrap_or(1)
    }

    /// Number of bands.
    #[must_use]
    pub fn band_count(&self) -> usize {
        self.bands.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s2_cube() -> RasterCube {
        let mut coords = BTreeMap::new();
        coords.insert(Dim::Time, Coords::Labels {
            values: vec!["2024-06-01T00:00:00Z".into(), "2024-09-01T00:00:00Z".into()],
        });
        coords.insert(Dim::Band, Coords::Labels {
            values: vec!["red".into(), "nir".into()],
        });
        coords.insert(Dim::Y, Coords::Numeric { values: vec![46.0, 45.99, 45.98] });
        coords.insert(Dim::X, Coords::Numeric { values: vec![12.0, 12.01, 12.02] });
        RasterCube {
            dims: vec![Dim::Time, Dim::Band, Dim::Y, Dim::X],
            coords,
            crs: Crs::epsg(4326),
            transform: GeoTransform([12.0, 0.01, 0.0, 46.0, 0.0, -0.01]),
            bands: vec![
                BandSpec {
                    name: "red".into(),
                    nodata: Some(0.0),
                    scale: 0.0001,
                    offset: 0.0,
                    units: Some("1".into()),
                    paths_per_time: vec!["/tmp/red_t0.tif".into(), "/tmp/red_t1.tif".into()],
                },
                BandSpec {
                    name: "nir".into(),
                    nodata: Some(0.0),
                    scale: 0.0001,
                    offset: 0.0,
                    units: Some("1".into()),
                    paths_per_time: vec!["/tmp/nir_t0.tif".into(), "/tmp/nir_t1.tif".into()],
                },
            ],
            provenance: Provenance {
                stac_ids: vec!["S2A_x".into()],
                graph_hash: Some("abc123".into()),
                software_version: Some("orbit-openeo/0.1.0".into()),
                processed_at: Some("2026-05-23T00:00:00Z".into()),
            },
        }
    }

    #[test]
    fn dim_as_str_is_lowercase() {
        assert_eq!(Dim::Time.as_str(), "time");
        assert_eq!(Dim::Band.as_str(), "band");
        assert_eq!(Dim::Y.as_str(), "y");
        assert_eq!(Dim::X.as_str(), "x");
    }

    #[test]
    fn coords_len_works_for_both_variants() {
        assert_eq!(Coords::Labels { values: vec!["a".into(), "b".into()] }.len(), 2);
        assert_eq!(Coords::Numeric { values: vec![1.0; 5] }.len(), 5);
        assert!(Coords::Numeric { values: vec![] }.is_empty());
    }

    #[test]
    fn crs_epsg_constructs_canonical_string() {
        assert_eq!(Crs::epsg(3857).0, "EPSG:3857");
        assert!(Crs::epsg(4326).equals(&Crs("EPSG:4326".into())));
    }

    #[test]
    fn geo_transform_extracts_components() {
        let g = GeoTransform([12.0, 0.01, 0.0, 46.0, 0.0, -0.01]);
        assert_eq!(g.ulx(), 12.0);
        assert_eq!(g.dx(), 0.01);
        assert_eq!(g.uly(), 46.0);
        assert_eq!(g.dy(), -0.01);
    }

    #[test]
    fn cube_resolution_returns_abs_values() {
        let c = s2_cube();
        let (rx, ry) = c.resolution();
        assert!((rx - 0.01).abs() < 1e-9);
        assert!((ry - 0.01).abs() < 1e-9);
    }

    #[test]
    fn cube_validate_passes_for_consistent_cube() {
        assert_eq!(s2_cube().validate(), Ok(()));
    }

    #[test]
    fn cube_validate_detects_band_coord_length_mismatch() {
        let mut c = s2_cube();
        c.coords.insert(Dim::Band, Coords::Labels { values: vec!["red".into()] });
        match c.validate() {
            Err(CubeError::CoordLenMismatch { dim, expected, actual }) => {
                assert_eq!(dim, "band");
                assert_eq!(expected, 2);
                assert_eq!(actual, 1);
            }
            other => panic!("expected CoordLenMismatch, got {other:?}"),
        }
    }

    #[test]
    fn cube_validate_detects_missing_dim() {
        let mut c = s2_cube();
        c.coords.remove(&Dim::Y);
        assert!(matches!(c.validate(), Err(CubeError::MissingDim("y"))));
    }

    #[test]
    fn cube_band_lookup_finds_by_name() {
        let c = s2_cube();
        let red = c.band("red").unwrap();
        assert_eq!(red.scale, 0.0001);
        assert!(matches!(c.band("missing"), Err(CubeError::MissingBand(_))));
    }

    #[test]
    fn cube_time_steps_and_band_count() {
        let c = s2_cube();
        assert_eq!(c.time_steps(), 2);
        assert_eq!(c.band_count(), 2);
    }

    #[test]
    fn cube_serialises_and_round_trips_through_json() {
        let c = s2_cube();
        let s = serde_json::to_string(&c).unwrap();
        let back: RasterCube = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn provenance_default_is_empty() {
        let p = Provenance::default();
        assert!(p.stac_ids.is_empty());
        assert!(p.graph_hash.is_none());
        assert!(p.software_version.is_none());
        assert!(p.processed_at.is_none());
    }

    #[test]
    fn band_spec_scale_default_is_one_and_offset_zero() {
        let raw = serde_json::json!({
            "name": "red",
            "paths_per_time": ["/tmp/a.tif"]
        });
        let b: BandSpec = serde_json::from_value(raw).unwrap();
        assert_eq!(b.scale, 1.0);
        assert_eq!(b.offset, 0.0);
        assert!(b.nodata.is_none());
    }
}
