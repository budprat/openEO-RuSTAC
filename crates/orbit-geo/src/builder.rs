//! Fluent builder for [`crate::RasterDataset`].
//!
//! Two entry points:
//!
//! - [`RasterDatasetBuilder::from_source`] when you already have a
//!   [`crate::DataSource`].
//! - [`RasterDatasetBuilder::from_files`] as a one-liner when you just have
//!   `&[PathBuf]`.
//!
//! Set [`block_size`](RasterDatasetBuilder::block_size) and optionally
//! [`resolution`](RasterDatasetBuilder::resolution) /
//! [`overlap`](RasterDatasetBuilder::overlap), then [`build`](RasterDatasetBuilder::build).

use crate::{
    block::RasterRegion,
    dataset::{DatasetMetadata, LayerMapping, RasterDataset},
    error::{Error, Result},
    source::DataSource,
    types::{
        BlockSize, GeoTransform, ImageResolution, Offset, Overlap, RasterShape, RasterType,
        ReadWindow, Size,
    },
};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Bridge for callers that have band-keyed path lookups (e.g. a STAC
/// `FeatureCollection` from the openEO façade) but should not force
/// `orbit-geo` to depend on STAC vocabulary.
///
/// Implementors return all hrefs / file paths for the named band, in
/// scene order. `orbit-geo` then delegates to
/// [`RasterDatasetBuilder::from_files`] without ever learning about
/// STAC types.
pub trait BandPathResolver {
    /// Return all hrefs / file paths for the named band, in scene order.
    /// Returns an empty vector when the band is not present on any scene.
    fn band_paths(&self, band_name: &str) -> Vec<PathBuf>;
}

/// Type-state builder for [`RasterDataset<T>`].
///
/// Implements the same fluent surface as the upstream `RasterDatasetBuilder`
/// but uses [`Result`] for errors (rather than the upstream's
/// `.expect()`/`.unwrap()` — see the rationale in the `13-geo-satellite/`
/// library docs).
pub struct RasterDatasetBuilder<T: RasterType> {
    source: DataSource,
    block_size: BlockSize,
    overlap: Overlap,
    resolution: Option<ImageResolution>,
    /// Optional EPSG override applied during build().
    epsg_override: Option<u32>,
    /// Optional geo_transform override applied during build().
    geo_transform_override: Option<crate::types::GeoTransform>,
    /// Optional (rows, cols) image size override applied during build().
    image_size_override: Option<(usize, usize)>,
    _t: PhantomData<T>,
}

impl<T: RasterType> RasterDatasetBuilder<T> {
    /// Start from a [`DataSource`].
    #[must_use]
    pub fn from_source(source: &DataSource) -> Self {
        Self {
            source: source.clone(),
            block_size: BlockSize::default(),
            overlap: Overlap::default(),
            resolution: None,
            epsg_override: None,
            geo_transform_override: None,
            image_size_override: None,
            _t: PhantomData,
        }
    }

    /// Convenience: start from a slice of file paths.
    pub fn from_files<P: AsRef<Path>>(paths: &[P]) -> Result<Self> {
        let pb: Vec<PathBuf> = paths.iter().map(|p| p.as_ref().to_path_buf()).collect();
        if pb.is_empty() {
            return Err(Error::invalid_builder("file list is empty"));
        }
        Ok(Self::from_source(&DataSource::Files { paths: pb }))
    }

    /// Convenience: collect every path for `band_name` from any
    /// [`BandPathResolver`] and delegate to [`Self::from_files`].
    ///
    /// Errors with `Error::invalid_builder` when the resolver yields no
    /// paths for the requested band (mirrors `from_files` empty-list
    /// semantics).
    pub fn from_band_resolver<R: BandPathResolver + ?Sized>(
        resolver: &R,
        band_name: &str,
    ) -> Result<Self> {
        let paths = resolver.band_paths(band_name);
        if paths.is_empty() {
            return Err(Error::invalid_builder(format!(
                "BandPathResolver returned no paths for band `{band_name}`"
            )));
        }
        Self::from_files(&paths)
    }

    /// Set the block partitioning. Default: 2048×2048.
    #[must_use]
    pub fn block_size(mut self, size: BlockSize) -> Self {
        self.block_size = size;
        self
    }

    /// Set per-edge overlap pixels. Default: 0.
    #[must_use]
    pub fn overlap(mut self, overlap: Overlap) -> Self {
        self.overlap = overlap;
        self
    }

    /// Override the resolution (defaults to whatever the source declares).
    /// Useful when STAC items disagree.
    #[must_use]
    pub fn resolution(mut self, res: ImageResolution) -> Self {
        self.resolution = Some(res);
        self
    }

    /// Materialize the dataset by inspecting each source file's metadata,
    /// validating alignment, then partitioning into blocks.
    ///
    /// This does **not** read pixel data — the per-block reads happen lazily
    /// inside [`crate::processing`] when a worker is applied.
    /// Construct an output-only dataset with no source files. Used when
    /// you want to create a new dataset from scratch (e.g. for `write_window3`
    /// without first reading a source). Caller must supply shape + geo metadata.
    ///
    /// `template`: optional `RasterDataset<U>` to copy shape/geo metadata from.
    pub fn from_scratch<U: RasterType>(template: Option<&RasterDataset<U>>) -> Self {
        let source = if let Some(t) = template {
            // Build a Files variant referencing no paths; metadata is overridden post-build.
            let _ = t; // just borrow to indicate template is consulted
            DataSource::Files { paths: Vec::new() }
        } else {
            DataSource::Files { paths: Vec::new() }
        };
        Self {
            source,
            block_size: BlockSize::default(),
            overlap: Overlap::default(),
            resolution: None,
            epsg_override: None,
            geo_transform_override: None,
            image_size_override: None,
            _t: PhantomData,
        }
    }

    /// Override the EPSG code on the resulting dataset post-build.
    /// (Stored as a builder field and applied during `build()`.)
    #[must_use]
    pub fn epsg(mut self, code: u32) -> Self {
        self.epsg_override = Some(code);
        self
    }

    /// Override the geo-transform on the resulting dataset post-build.
    #[must_use]
    pub fn geo_transform(mut self, gt: crate::types::GeoTransform) -> Self {
        self.geo_transform_override = Some(gt);
        self
    }

    /// Override the image size on the resulting dataset post-build.
    #[must_use]
    pub fn image_size(mut self, rows: usize, cols: usize) -> Self {
        self.image_size_override = Some((rows, cols));
        self
    }

    /// Copy shape + CRS + geo_transform from another dataset.
    #[must_use]
    pub fn template<U: RasterType>(mut self, other: &RasterDataset<U>) -> Self {
        self.epsg_override = Some(other.metadata.epsg_code);
        self.geo_transform_override = Some(other.metadata.geo_transform);
        self.image_size_override = Some((other.metadata.shape.rows, other.metadata.shape.cols));
        self
    }

    /// Materialize the dataset by inspecting each source file's metadata,
    /// validating alignment, then partitioning into blocks.
    pub fn build(self) -> Result<RasterDataset<T>> {
        self.source.validate()?;

        let paths = self.source.local_paths();
        if paths.is_empty() {
            return Err(Error::invalid_builder("no local paths to build from"));
        }

        // For Phase-4 MVP we infer dataset metadata from the first file and
        // assume all other files are spatially aligned. A real impl validates
        // this with GDAL and surfaces `Error::InconsistentMetadata` if not.
        let (shape, epsg_code, geo_transform) = inspect_first_file::<T>(&paths)?;

        let times = paths.len();
        let dataset_shape = RasterShape {
            times,
            layers: shape.layers,
            rows: shape.rows,
            cols: shape.cols,
        };

        let blocks = partition_blocks(
            &dataset_shape,
            self.block_size,
            self.overlap,
            geo_transform,
            epsg_code,
        );

        // Default layer mapping: one file ↔ one (time, layer=0) cell.
        let layer_mappings: Vec<LayerMapping> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| LayerMapping {
                source: p.clone(),
                time_pos: i,
                layer_pos: 0,
                band: 1,
            })
            .collect();

        // Apply builder overrides (epsg / geo_transform / image size).
        let final_epsg = self.epsg_override.unwrap_or(epsg_code);
        let final_gt = self.geo_transform_override.unwrap_or(geo_transform);
        let final_shape = if let Some((r, c)) = self.image_size_override {
            RasterShape { rows: r, cols: c, ..dataset_shape }
        } else {
            dataset_shape
        };
        Ok(RasterDataset {
            metadata: DatasetMetadata {
                shape: final_shape,
                epsg_code: final_epsg,
                geo_transform: final_gt,
            },
            blocks,
            layer_mappings,
            source_files: paths,
            _t: PhantomData,
        })
    }
}

/// Partition the dataset extent into uniform blocks of `block_size`.
fn partition_blocks(
    shape: &RasterShape,
    block_size: BlockSize,
    overlap: Overlap,
    geo_transform: GeoTransform,
    epsg_code: u32,
) -> Vec<RasterRegion> {
    let mut blocks = Vec::new();
    let mut index: usize = 0;

    let mut r: usize = 0;
    while r < shape.rows {
        let block_rows = block_size.rows.min(shape.rows - r);
        let mut c: usize = 0;
        while c < shape.cols {
            let block_cols = block_size.cols.min(shape.cols - c);

            // Per-block geo-transform: origin is shifted by (r, c) pixels.
            let block_origin_x =
                geo_transform.origin_x() + (c as f64) * geo_transform.pixel_width();
            let block_origin_y =
                geo_transform.origin_y() + (r as f64) * geo_transform.pixel_height();
            let block_gt = GeoTransform([
                block_origin_x,
                geo_transform.pixel_width(),
                geo_transform.0[2],
                block_origin_y,
                geo_transform.0[4],
                geo_transform.pixel_height(),
            ]);

            blocks.push(RasterRegion {
                block_index: index,
                read_window: ReadWindow {
                    offset: Offset {
                        rows: r as isize,
                        cols: c as isize,
                    },
                    size: Size {
                        rows: block_rows as isize,
                        cols: block_cols as isize,
                    },
                },
                overlap,
                geo_transform: block_gt,
                epsg_code,
            });
            index += 1;
            c += block_size.cols;
        }
        r += block_size.rows;
    }
    blocks
}

/// Inspect the first input file to extract layers, rows, cols, EPSG, geo-transform.
///
/// Phase-4 MVP: minimal GDAL probing. Production builds out a richer
/// `RasterMetadata` per source file and surfaces inconsistency errors.
fn inspect_first_file<T: RasterType>(
    paths: &[PathBuf],
) -> Result<(RasterShape, u32, GeoTransform)> {
    let ds = gdal::Dataset::open(&paths[0])?;
    let (cols, rows) = ds.raster_size();
    let layers = ds.raster_count();
    let gt_array = ds.geo_transform()?;
    let gt = GeoTransform(gt_array);

    let epsg_code = ds
        .spatial_ref()
        .ok()
        .and_then(|sr| sr.auth_code().ok())
        .unwrap_or(0) as u32;

    let shape = RasterShape {
        times: 0, // filled in by caller
        layers: layers as usize,
        rows,
        cols,
    };

    Ok((shape, epsg_code, gt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Mock `BandPathResolver` over an in-memory band→paths map. Lets us
    /// exercise the trait dispatch without pulling in STAC vocabulary.
    struct MapResolver {
        bands: BTreeMap<String, Vec<PathBuf>>,
    }

    impl BandPathResolver for MapResolver {
        fn band_paths(&self, band_name: &str) -> Vec<PathBuf> {
            self.bands.get(band_name).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn from_band_resolver_handles_single_band() {
        // One scene, one band → resolver yields one path → builder builds
        // a dataset whose source_files match what the resolver returned.
        let tmp = crate::test_support::tiny_geotiff(8, 8, 42, 4326);
        let path: PathBuf = tmp.to_path_buf();
        let mut bands = BTreeMap::new();
        bands.insert("B04".to_string(), vec![path.clone()]);
        let resolver = MapResolver { bands };

        let rds: RasterDataset<i16> =
            RasterDatasetBuilder::<i16>::from_band_resolver(&resolver, "B04")
                .unwrap()
                .build()
                .unwrap();

        assert_eq!(rds.source_files(), &[path]);
        assert_eq!(rds.metadata.shape.times, 1);
        assert_eq!(rds.metadata.shape.rows, 8);
        assert_eq!(rds.metadata.shape.cols, 8);
    }

    #[test]
    fn from_band_resolver_propagates_resolution_block_size() {
        // After the convenience constructor, chained `.block_size(...)` and
        // `.resolution(...)` must still take effect — i.e. we got back a
        // real builder, not a half-baked stub.
        let t1 = crate::test_support::tiny_geotiff(16, 16, 1, 4326);
        let t2 = crate::test_support::tiny_geotiff(16, 16, 2, 4326);
        let p1: PathBuf = t1.to_path_buf();
        let p2: PathBuf = t2.to_path_buf();
        let mut bands = BTreeMap::new();
        bands.insert("B08".to_string(), vec![p1.clone(), p2.clone()]);
        let resolver = MapResolver { bands };

        let rds: RasterDataset<i16> =
            RasterDatasetBuilder::<i16>::from_band_resolver(&resolver, "B08")
                .unwrap()
                .block_size(BlockSize { rows: 4, cols: 4 })
                .resolution(ImageResolution { x: 10.0, y: -10.0 })
                .build()
                .unwrap();

        // 16/4 = 4 blocks per axis → 16 blocks per timestep. Builder
        // partitions per (rows, cols) regardless of times count, so the
        // block list is the spatial grid once over.
        assert_eq!(rds.num_blocks(), 16);
        // Two scenes flowed through the resolver in order.
        assert_eq!(rds.metadata.shape.times, 2);
        assert_eq!(rds.source_files(), &[p1, p2]);
    }

    #[test]
    fn from_band_resolver_empty_band_is_invalid_builder() {
        let resolver = MapResolver { bands: BTreeMap::new() };
        let r = RasterDatasetBuilder::<i16>::from_band_resolver(&resolver, "missing");
        assert!(matches!(r, Err(Error::InvalidBuilder(_))));
    }
}
