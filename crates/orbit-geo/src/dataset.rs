//! The central [`RasterDataset`] type.
//!
//! A `RasterDataset<T>` carries:
//! - The dataset *metadata* (shape, CRS/EPSG, geo-transform)
//! - A list of `RasterRegion`s describing the block partitioning
//! - A handle to the source files (so blocks can be read on demand)
//!
//! Use [`crate::RasterDatasetBuilder`] to construct one — direct construction
//! is intentionally private.

use crate::{
    block::RasterRegion,
    types::{GeoTransform, RasterShape, RasterType},
};
use std::marker::PhantomData;
use std::path::PathBuf;

/// Probe a scene file and emit one [`LayerMapping`] per band, mapping the
/// file's bands to consecutive `layer_pos` slots at a given timestep.
///
/// Use this when each scene file is a multi-band raster (e.g. a true
/// multi-band COG with red+NIR+SWIR in one file) — saves callers from
/// opening the file to count bands themselves.
///
/// `time_pos` is the index of this scene along the time axis.
/// Returns `Err` if the file cannot be opened or has zero bands.
pub fn layer_mappings_for_scene(
    path: impl AsRef<std::path::Path>,
    time_pos: usize,
) -> crate::Result<Vec<LayerMapping>> {
    let path_ref = path.as_ref();
    let ds = ::gdal::Dataset::open(path_ref).map_err(|e| {
        crate::Error::Other(format!(
            "open {} for band probe: {e}",
            path_ref.display()
        ))
    })?;
    let n_bands = ds.raster_count();
    if n_bands == 0 {
        return Err(crate::Error::InconsistentMetadata(format!(
            "{} reports zero bands",
            path_ref.display()
        )));
    }
    Ok((1..=n_bands)
        .map(|b| LayerMapping {
            source: path_ref.to_path_buf(),
            time_pos,
            layer_pos: (b - 1),
            band: b,
        })
        .collect())
}

/// Same as [`layer_mappings_for_scene`] but across many scenes, each
/// becoming a distinct timestep. Bands within each scene become layers
/// (so all scenes must have the same band count — verified, errors otherwise).
///
/// ```rust,ignore
/// // 9 Sentinel-2 scenes, each 4-band (B02, B03, B04, B08) → 9×4 = 36 mappings
/// let mappings = layer_mappings_for_scenes(&scene_paths)?;
/// assert_eq!(mappings.len(), 36);
/// ```
pub fn layer_mappings_for_scenes(
    paths: &[impl AsRef<std::path::Path>],
) -> crate::Result<Vec<LayerMapping>> {
    let mut all = Vec::new();
    let mut expected_bands: Option<usize> = None;
    for (t, p) in paths.iter().enumerate() {
        let mappings = layer_mappings_for_scene(p, t)?;
        match expected_bands {
            None => expected_bands = Some(mappings.len()),
            Some(n) if n == mappings.len() => {}
            Some(n) => {
                return Err(crate::Error::InconsistentMetadata(format!(
                    "scene {} has {} bands; expected {} (matching scene 0)",
                    p.as_ref().display(),
                    mappings.len(),
                    n
                )));
            }
        }
        all.extend(mappings);
    }
    Ok(all)
}

/// Mapping of one `(file, band)` pair to a `(time, layer)` slot in the 4-D
/// raster tensor.
///
/// The block reader iterates the dataset's `layer_mappings`, opens each
/// referenced file, reads the per-block window, and assigns into the
/// 4-D `Array4<T>` at `[time_pos, layer_pos, .., ..]`.
///
/// `RasterDatasetBuilder::from_files(&paths)` creates a default mapping
/// where each path becomes `(time_pos = index, layer_pos = 0, band = 1)`.
/// More complex mappings (multi-band scenes, multi-layer stacks) can be
/// constructed by hand.
#[derive(Clone, Debug)]
pub struct LayerMapping {
    /// Source raster file (must be GDAL-openable).
    pub source: PathBuf,
    /// Time index in the 4-D tensor (axis 0).
    pub time_pos: usize,
    /// Layer index in the 4-D tensor (axis 1).
    pub layer_pos: usize,
    /// 1-based GDAL band index within `source`.
    pub band: usize,
}

/// Aligned, block-partitioned raster dataset over element type `T`.
///
/// `T` is the *input* element type read from disk (e.g. `i16` for DEA
/// Sentinel-2 ARD). The output type of a reduction is chosen independently
/// by the worker function — see [`crate::processing`].
pub struct RasterDataset<T: RasterType> {
    /// Aggregated metadata (4-D shape, CRS, geo-transform).
    pub metadata: DatasetMetadata,
    /// Block partitioning — one entry per block.
    pub blocks: Vec<RasterRegion>,
    /// Layer mappings: how to populate each `(time, layer)` slot.
    /// `pub` so examples and downstream code can supply custom mappings
    /// (e.g. one timestep × multiple bands per scene).
    pub layer_mappings: Vec<LayerMapping>,
    /// Source files in scene order — used for cached local-paths access.
    pub source_files: Vec<PathBuf>,
    /// Phantom marker so `T` participates in trait selection.
    pub _t: PhantomData<T>,
}

/// Dataset-level metadata shared by all blocks.
#[derive(Clone, Debug)]
pub struct DatasetMetadata {
    /// 4-D shape across all blocks.
    pub shape: RasterShape,
    /// CRS as an EPSG code.
    pub epsg_code: u32,
    /// Top-level geo-transform.
    pub geo_transform: GeoTransform,
}

impl<T: RasterType> RasterDataset<T> {
    /// Number of blocks in this dataset.
    #[must_use]
    pub fn num_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Source files in order — useful for the parallel reader.
    #[must_use]
    pub fn source_files(&self) -> &[PathBuf] {
        &self.source_files
    }

    /// Layer mappings: how each `(time, layer)` slot is sourced.
    #[must_use]
    pub fn layer_mappings(&self) -> &[LayerMapping] {
        &self.layer_mappings
    }

    /// Iterator over `(block_id, RasterRegion)` pairs. Useful for sequential
    /// inspection without invoking the apply* machinery.
    pub fn iter(&self) -> RasterDatasetIter<'_, T> {
        RasterDatasetIter { rds: self, pos: 0 }
    }
}

/// Iterator yielding `(block_id, RasterRegion)` pairs over a `RasterDataset`.
pub struct RasterDatasetIter<'a, T: RasterType> {
    rds: &'a RasterDataset<T>,
    pos: usize,
}

impl<'a, T: RasterType> Iterator for RasterDatasetIter<'a, T> {
    type Item = (usize, &'a RasterRegion);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.rds.blocks.len() {
            return None;
        }
        let id = self.pos;
        self.pos += 1;
        Some((id, &self.rds.blocks[id]))
    }
}

impl<T: RasterType> RasterDataset<T> {
    /// Get the `RasterRegion` (block metadata) at a given block id.
    pub fn block_region(&self, block_id: usize) -> Option<&RasterRegion> {
        self.rds_check_block_id(block_id);
        self.blocks.get(block_id)
    }

    fn rds_check_block_id(&self, _block_id: usize) {
        // hook for future bounds-checking; intentionally no-op for now
    }
}
