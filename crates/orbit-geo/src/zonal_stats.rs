//! Zonal statistics — aggregate raster pixel values within polygon zones.
//!
//! Current scope: per-class histogram (count of pixels per class value)
//! within a single mask raster. Polars DataFrame output deferred to a
//! `use_polars` feature.

use crate::dataset::RasterDataset;
use crate::error::Result;
use crate::types::RasterType;
use std::collections::HashMap;

/// Compute a per-class pixel-count histogram of `data` for pixels where
/// `mask` is non-zero. Both datasets must share extent + block partitioning.
///
/// Returns `HashMap<class_value, count>`.
///
pub fn zonal_histogram<
    T: RasterType + std::hash::Hash + std::cmp::Eq,
    U: RasterType + std::cmp::PartialEq + num_traits::Zero,
>(
    data: &RasterDataset<T>,
    mask: &RasterDataset<U>,
) -> Result<HashMap<T, u64>> {
    use crate::error::Error;
    if data.num_blocks() != mask.num_blocks() {
        return Err(Error::InconsistentMetadata(format!(
            "zonal_histogram: data has {} blocks, mask has {}",
            data.num_blocks(),
            mask.num_blocks()
        )));
    }
    let mut hist: HashMap<T, u64> = HashMap::new();
    for (block_id, _region) in data.blocks.iter().enumerate() {
        let data_block = data.read_block_layer_idx(block_id, 0)?;
        let mask_block = mask.read_block_layer_idx(block_id, 0)?;
        let zero = U::zero();
        for (&d, &m) in data_block.data.iter().zip(mask_block.data.iter()) {
            if m != zero {
                *hist.entry(d).or_insert(0) += 1;
            }
        }
    }
    Ok(hist)
}
