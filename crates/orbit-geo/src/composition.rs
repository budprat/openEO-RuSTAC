//! Compose datasets along the time or layer axis.
//!
//! `extend` glues two aligned datasets along time. `stack` glues them along
//! layer. Both validate spatial alignment (rows, cols, EPSG) first.

use crate::dataset::{DatasetMetadata, LayerMapping, RasterDataset};
use crate::error::{Error, Result};
use crate::types::{RasterShape, RasterType};
use std::marker::PhantomData;

/// Concatenate two datasets along the **time** axis.
///
/// Result times = `a.times + b.times`. Spatial extent and layer count
/// must match between `a` and `b`.
///
pub fn extend<T: RasterType>(a: &RasterDataset<T>, b: &RasterDataset<T>) -> Result<RasterDataset<T>> {
    check_spatial_alignment(a, b)?;
    if a.metadata.shape.layers != b.metadata.shape.layers {
        return Err(Error::InconsistentMetadata(format!(
            "extend: layer mismatch a={} b={}",
            a.metadata.shape.layers, b.metadata.shape.layers
        )));
    }
    let new_shape = RasterShape {
        times: a.metadata.shape.times + b.metadata.shape.times,
        layers: a.metadata.shape.layers,
        rows: a.metadata.shape.rows,
        cols: a.metadata.shape.cols,
    };
    let mut new_mappings = a.layer_mappings.clone();
    for m in &b.layer_mappings {
        new_mappings.push(LayerMapping {
            source: m.source.clone(),
            time_pos: m.time_pos + a.metadata.shape.times,
            layer_pos: m.layer_pos,
            band: m.band,
        });
    }
    let mut new_source_files = a.source_files.clone();
    new_source_files.extend(b.source_files.iter().cloned());
    Ok(RasterDataset {
        metadata: DatasetMetadata {
            shape: new_shape,
            epsg_code: a.metadata.epsg_code,
            geo_transform: a.metadata.geo_transform,
        },
        blocks: a.blocks.clone(),
        layer_mappings: new_mappings,
        source_files: new_source_files,
        _t: PhantomData,
    })
}

/// Concatenate two datasets along the **layer** axis.
///
/// Result layers = `a.layers + b.layers`. Spatial extent, time axis, and
/// EPSG must match between `a` and `b`.
///
pub fn stack<T: RasterType>(a: &RasterDataset<T>, b: &RasterDataset<T>) -> Result<RasterDataset<T>> {
    check_spatial_alignment(a, b)?;
    if a.metadata.shape.times != b.metadata.shape.times {
        return Err(Error::InconsistentMetadata(format!(
            "stack: time mismatch a={} b={}",
            a.metadata.shape.times, b.metadata.shape.times
        )));
    }
    let new_shape = RasterShape {
        times: a.metadata.shape.times,
        layers: a.metadata.shape.layers + b.metadata.shape.layers,
        rows: a.metadata.shape.rows,
        cols: a.metadata.shape.cols,
    };
    let mut new_mappings = a.layer_mappings.clone();
    for m in &b.layer_mappings {
        new_mappings.push(LayerMapping {
            source: m.source.clone(),
            time_pos: m.time_pos,
            layer_pos: m.layer_pos + a.metadata.shape.layers,
            band: m.band,
        });
    }
    let mut new_source_files = a.source_files.clone();
    new_source_files.extend(b.source_files.iter().cloned());
    Ok(RasterDataset {
        metadata: DatasetMetadata {
            shape: new_shape,
            epsg_code: a.metadata.epsg_code,
            geo_transform: a.metadata.geo_transform,
        },
        blocks: a.blocks.clone(),
        layer_mappings: new_mappings,
        source_files: new_source_files,
        _t: PhantomData,
    })
}

/// Internal helper: assert spatial alignment between two datasets.
fn check_spatial_alignment<T: RasterType, U: RasterType>(
    a: &RasterDataset<T>,
    b: &RasterDataset<U>,
) -> Result<()> {
    if a.metadata.shape.rows != b.metadata.shape.rows
        || a.metadata.shape.cols != b.metadata.shape.cols
    {
        return Err(Error::InconsistentMetadata(format!(
            "spatial extent mismatch: a=({}, {}), b=({}, {})",
            a.metadata.shape.rows,
            a.metadata.shape.cols,
            b.metadata.shape.rows,
            b.metadata.shape.cols
        )));
    }
    if a.metadata.epsg_code != b.metadata.epsg_code {
        return Err(Error::InconsistentMetadata(format!(
            "EPSG mismatch: a={}, b={}",
            a.metadata.epsg_code, b.metadata.epsg_code
        )));
    }
    Ok(())
}

// Re-export the helper so other modules can use it if needed.

#[allow(dead_code)]
fn _phantom_keep<T: RasterType>() -> PhantomData<T> {
    PhantomData
}
#[allow(dead_code)]
fn _meta_construct() -> DatasetMetadata {
    panic!()
}
#[allow(dead_code)]
fn _layer_mapping_construct() -> LayerMapping {
    panic!()
}
#[allow(dead_code)]
fn _shape_construct() -> RasterShape {
    panic!()
}

// ─────────────────────────────────────────────────────────────────────
// Batch F — Layer-selection trait machinery for RasterDataBlock.
// ─────────────────────────────────────────────────────────────────────

use crate::block::RasterDataBlock;
use ndarray::{s, Array3, Axis};

/// Select a single layer's data from a `RasterDataBlock<T>`. Returns shape `(times, rows, cols)`.
pub trait Select<T: RasterType> {
    /// Select layer at `idx`, returning a 3-D Array shaped (times, rows, cols).
    fn select_layer(&self, idx: usize) -> Array3<T>;
}

impl<T: RasterType> Select<T> for RasterDataBlock<T> {
    fn select_layer(&self, idx: usize) -> Array3<T> {
        self.data.slice(s![.., idx, .., ..]).to_owned()
    }
}

/// Sum across a specified axis of a `RasterDataBlock<T>`.
pub trait SumDimension<T: RasterType> {
    /// Sum across the given axis (0 = time, 1 = layer). Returns `Array3<f64>`.
    fn sum_dim(&self, axis: usize) -> ndarray::ArrayD<f64>;
}

impl<T: RasterType + Into<f64> + Copy> SumDimension<T> for RasterDataBlock<T> {
    fn sum_dim(&self, axis: usize) -> ndarray::ArrayD<f64> {
        let data_f64 = self.data.mapv(|v| {
            let f: f64 = num_traits::cast::cast(v).unwrap_or(0.0);
            f
        });
        data_f64.sum_axis(Axis(axis)).into_dyn()
    }
}

#[cfg(test)]
mod batch_f_select_tests {
    use super::*;
    use crate::block::RasterRegion;
    use crate::types::{GeoTransform, Offset, RasterShape, ReadWindow, Size};
    use ndarray::Array4;

    #[test]
    fn select_layer_returns_single_layer_slice() {
        let data: Array4<i16> = Array4::from_shape_fn((1, 3, 2, 2), |(_t, l, _r, _c)| l as i16);
        let region = RasterRegion {
            block_index: 0,
            read_window: ReadWindow { offset: Offset { rows: 0, cols: 0 }, size: Size { rows: 2, cols: 2 } },
            overlap: Default::default(),
            geo_transform: GeoTransform([0.0, 1.0, 0.0, 2.0, 0.0, -1.0]),
            epsg_code: 4326,
        };
        let block = RasterDataBlock {
            data,
            shape: RasterShape { times: 1, layers: 3, rows: 2, cols: 2 },
            no_data: 0_i16,
            region,
        };
        let sel = block.select_layer(1);
        assert_eq!(sel.dim(), (1, 2, 2));
        assert!(sel.iter().all(|&v| v == 1));
    }
}
