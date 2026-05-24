//! Input adapters for [`crate::RasterDataset`].
//!
//! A [`DataSource`] is an immutable handle to one or more raster scenes that
//! the [`crate::RasterDatasetBuilder`] can convert into an aligned dataset.
//!
//! The crate ships with two built-in source kinds:
//!
//! - [`DataSource::Files`] — a list of local file paths
//! - [`DataSource::Stac`] — a STAC feature collection (requires the `stac` feature)
//!
//! Third-party crates can implement custom backends by mapping them to one of
//! these (e.g. a Zarr datacube can synthesize a virtual file set, or you can
//! resolve a STAC search to local paths first).

use crate::error::{Error, Result};
#[cfg(feature = "stac")]
use crate::types::ImageResolution;
use std::path::{Path, PathBuf};

/// An immutable description of where raster data comes from.
///
/// orbit-geo accepts three source kinds (more may be added later):
/// local files, a STAC feature collection, or an openEO backend. All three
/// converge on a list of local file paths that the block-parallel kernel
/// reads via GDAL VSI — see `crate::processing`.
#[derive(Debug, Clone)]
pub enum DataSource {
    /// Scenes live on a local filesystem path.
    Files {
        /// Paths to scene files (GeoTIFF, COG, etc.).
        paths: Vec<PathBuf>,
    },

    /// Scenes come from a STAC catalog. **Phase-4 stub** — the variant
    /// exists so downstream code can pattern-match on it, but conversion
    /// to local paths requires the not-yet-implemented `crate::stac`
    /// helpers. As a workaround, run `rustac search … items.parquet` from
    /// the CLI, then point `DataSource::Files` at the resulting parquet
    /// after running `rustac translate items.parquet items.ndjson` or
    /// after downloading assets manually.
    ///
    /// See `crate::stac` module for the stub surface and the workaround
    /// path documented in `13-geo-satellite/04-openeo-strategic-analysis.md`.
    #[cfg(feature = "stac")]
    Stac {
        /// Path to a local `stac-geoparquet` file or NDJSON dump of items.
        items_file: PathBuf,

        /// Optional explicit resolution if STAC items disagree.
        resolution: Option<ImageResolution>,
    },

    /// Scenes are the result of an **openEO** job. orbit-geo submits the
    /// process graph to a backend (e.g. CDSE, VITO, EODC), polls until
    /// completion, downloads the result assets into `cache_dir`, then hands
    /// the local paths to the same block-parallel kernel. See
    /// [`crate::openeo`] for the client implementation and
    /// [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md)
    /// for the design rationale (Approach C).
    #[cfg(feature = "openeo")]
    OpenEO {
        /// Backend base URL, e.g. `https://openeo.dataspace.copernicus.eu`
        backend_url: String,
        /// openEO process graph (JSON object — see openeo-processes spec)
        process_graph: serde_json::Value,
        /// Authentication credentials
        auth: crate::openeo::OpenEoAuth,
        /// Local directory for downloaded result assets (deduplicated by URL hash)
        cache_dir: PathBuf,
    },
}

impl DataSource {
    /// Quick constructor for a single local file.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self::Files {
            paths: vec![path.into()],
        }
    }

    /// Constructor for multiple local files.
    pub fn from_files<P, I>(paths: I) -> Self
    where
        P: Into<PathBuf>,
        I: IntoIterator<Item = P>,
    {
        Self::Files {
            paths: paths.into_iter().map(Into::into).collect(),
        }
    }

    /// Validates that all referenced files exist and are readable.
    ///
    /// GDAL Virtual File System paths (anything starting with `/vsi`, e.g.
    /// `/vsicurl/`, `/vsis3/`, `/vsigs/`, `/vsiaz/`) are accepted without
    /// a local filesystem check — they're resolved at GDAL open time.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Files { paths } => {
                for p in paths {
                    let is_vsi = p.to_str().is_some_and(|s| s.starts_with("/vsi"));
                    if !is_vsi && !p.exists() {
                        return Err(Error::SourceNotFound(p.clone()));
                    }
                }
                Ok(())
            }

            #[cfg(feature = "stac")]
            Self::Stac { .. } => Ok(()),

            #[cfg(feature = "openeo")]
            Self::OpenEO { backend_url, .. } => {
                if backend_url.is_empty() {
                    return Err(Error::invalid_builder("openEO backend_url is empty"));
                }
                Ok(())
            }
        }
    }

    /// Iterate over the locally-accessible file paths for this source.
    ///
    /// For STAC and openEO sources, the caller is expected to have **already**
    /// resolved the source into local files (see [`cache`] /
    /// [`crate::openeo::submit_and_download`]) — this method only yields paths
    /// that exist on the local filesystem.
    pub fn local_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::Files { paths } => paths.clone(),

            #[cfg(feature = "stac")]
            Self::Stac { .. } => Vec::new(),

            #[cfg(feature = "openeo")]
            Self::OpenEO { .. } => Vec::new(),
        }
    }
}

/// Fluent builder for [`DataSource`].
#[derive(Debug, Default)]
pub struct DataSourceBuilder {
    paths: Vec<PathBuf>,
}

impl DataSourceBuilder {
    /// Start a builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a single file path.
    #[must_use]
    pub fn file(mut self, path: impl AsRef<Path>) -> Self {
        self.paths.push(path.as_ref().to_path_buf());
        self
    }

    /// Add a slice of paths.
    #[must_use]
    pub fn files<P: AsRef<Path>>(mut self, paths: &[P]) -> Self {
        for p in paths {
            self.paths.push(p.as_ref().to_path_buf());
        }
        self
    }

    /// Finalize into a [`DataSource`].
    #[must_use]
    pub fn build(self) -> DataSource {
        DataSource::Files { paths: self.paths }
    }
}

// Note: the STAC asset-download helper that previously lived here as a
// `source::cache` stub has been moved to [`crate::stac::download_items`] and
// is now fully implemented (typed `stac::ItemCollection` input, sha256 URL
// cache, PC URL rewriter support). See `src/stac.rs`.

#[cfg(test)]
mod source_validate_tests {
    //! **T0.3 bug fix**: `DataSource::Files::validate` was rejecting
    //! `/vsi*/` paths because they don't satisfy `Path::exists()`.
    use super::*;
    use std::path::PathBuf;

    /// **RED**: VSI paths must not be rejected by existence check.
    #[test]
    fn files_validate_accepts_vsi_paths() {
        let ds = DataSource::Files {
            paths: vec![PathBuf::from(
                "/vsicurl/https://example.com/foo.tif",
            )],
        };
        ds.validate().expect("VSI path must pass validation");
    }

    /// **RED**: same for `/vsis3/` (anonymous S3 path).
    #[test]
    fn files_validate_accepts_vsis3_paths() {
        let ds = DataSource::Files {
            paths: vec![PathBuf::from(
                "/vsis3/sentinel-cogs/sentinel-s2-l2a-cogs/55/H/BV/2024/12/foo/B04.tif",
            )],
        };
        ds.validate().expect("VSIS3 path must pass validation");
    }

    /// Regression guard: real-but-missing local path still rejected.
    #[test]
    fn files_validate_rejects_missing_local_paths() {
        let ds = DataSource::Files {
            paths: vec![PathBuf::from("/nonexistent/path/to/foo.tif")],
        };
        assert!(ds.validate().is_err(), "missing local path must fail validation");
    }
}
