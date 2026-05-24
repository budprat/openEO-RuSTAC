//! Point sampling — extract per-pixel values at given geographic coordinates.

use crate::dataset::RasterDataset;
use crate::error::{Error, Result};
use crate::types::RasterType;

/// Sample a single pixel value at geographic coordinates `(geo_x, geo_y)`.
///
/// Returns `Ok(value)` if the point falls within the dataset's spatial extent,
/// `Err(...)` otherwise.
///
/// Currently only the first layer of the first timestep is sampled.
/// Multi-layer / multi-time sampling will be added later.
///
pub fn sample_at_point<T: RasterType>(
    rds: &RasterDataset<T>,
    geo_x: f64,
    geo_y: f64,
) -> Result<T> {
    let (row, col) = geo_to_pixel(geo_x, geo_y, &rds.metadata.geo_transform.0);
    let rows_total = rds.metadata.shape.rows as isize;
    let cols_total = rds.metadata.shape.cols as isize;
    if row < 0 || row >= rows_total || col < 0 || col >= cols_total {
        return Err(Error::Other(format!(
            "sample_at_point: ({geo_x}, {geo_y}) → pixel ({row}, {col}) out of extent {}x{}",
            rows_total, cols_total
        )));
    }
    // Use the first layer of the first timestep.
    let m = rds
        .layer_mappings
        .iter()
        .find(|m| m.time_pos == 0 && m.layer_pos == 0)
        .ok_or_else(|| Error::Other("sample_at_point: no (time_pos=0, layer_pos=0) mapping".into()))?;
    let ds = gdal::Dataset::open(&m.source)
        .map_err(|e| Error::Other(format!("open {} for sample: {e}", m.source.display())))?;
    let band = ds
        .rasterband(m.band)
        .map_err(|e| Error::Other(format!("rasterband {}: {e}", m.band)))?;
    let buf: gdal::raster::Buffer<T> = band
        .read_as((col, row), (1, 1), (1, 1), None)
        .map_err(|e| Error::Other(format!("read_as ({col}, {row}): {e}")))?;
    Ok(buf.data()[0])
}

/// Sample multiple points. Returns `Vec<Option<T>>` — `Some(value)` for
/// in-bounds points, `None` for out-of-extent points.
///
pub fn sample<T: RasterType>(
    rds: &RasterDataset<T>,
    points: &[(f64, f64)],
) -> Vec<Option<T>> {
    points
        .iter()
        .map(|(x, y)| sample_at_point(rds, *x, *y).ok())
        .collect()
}

/// Internal helper: convert geographic (x, y) into integer (row, col) using
/// the dataset's geo_transform.
///
/// GDAL geo_transform: `[origin_x, pixel_w, 0, origin_y, 0, -pixel_h]`.
/// Maps pixel `(c, r)` → geo `(origin_x + c * pixel_w, origin_y + r * (-pixel_h))`.
/// Inverse: `c = (geo_x - origin_x) / pixel_w`, `r = (origin_y - geo_y) / pixel_h`.
pub(crate) fn geo_to_pixel(
    geo_x: f64,
    geo_y: f64,
    geo_transform: &[f64; 6],
) -> (isize, isize) {
    let origin_x = geo_transform[0];
    let pixel_w = geo_transform[1];
    let origin_y = geo_transform[3];
    let pixel_h = geo_transform[5].abs();
    let col = ((geo_x - origin_x) / pixel_w).floor() as isize;
    let row = ((origin_y - geo_y) / pixel_h).floor() as isize;
    (row, col)
}

#[allow(dead_code)]
fn _import_used(e: Error) -> Error {
    e
}

// ─────────────────────────────────────────────────────────────────────
// Batch H — remaining upstream sampling helpers
// ─────────────────────────────────────────────────────────────────────


/// Convert a `(row, col)` index in the full dataset to `(block_id, local_row, local_col)`.
///
/// Walks the dataset's blocks to find which one contains the global pixel.
pub fn block_id_rowcol<T: crate::types::RasterType>(
    rds: &RasterDataset<T>,
    global_row: usize,
    global_col: usize,
) -> Option<(usize, usize, usize)> {
    for (id, region) in rds.iter() {
        let off_row = region.read_window.offset.rows as usize;
        let off_col = region.read_window.offset.cols as usize;
        let h = region.read_window.size.rows as usize;
        let w = region.read_window.size.cols as usize;
        if global_row >= off_row
            && global_row < off_row + h
            && global_col >= off_col
            && global_col < off_col + w
        {
            return Some((id, global_row - off_row, global_col - off_col));
        }
    }
    None
}

/// Block-wise batch extraction: group geo-points by containing block, then
/// read each block once and sample all points inside.
///
/// Returns `Vec<Option<T>>` in input order.
pub fn extract_blockwise<T: crate::types::RasterType>(
    rds: &RasterDataset<T>,
    points: &[(f64, f64)],
) -> Vec<Option<T>> {
    // For simplicity in this first impl: delegate to the single-point path.
    // The optimization is grouping by block + per-block-single-read.
    // We get the API shape right; downstream optimization to follow.
    points.iter().map(|(x, y)| sample_at_point(rds, *x, *y).ok()).collect()
}

/// Convert a list of `(geo_x, geo_y)` point geometries into global pixel
/// row/col indices using the dataset's geo_transform.
pub fn geoms_to_global_indices<T: crate::types::RasterType>(
    rds: &RasterDataset<T>,
    points: &[(f64, f64)],
) -> Vec<(isize, isize)> {
    points
        .iter()
        .map(|(x, y)| geo_to_pixel(*x, *y, &rds.metadata.geo_transform.0))
        .collect()
}

/// Classify a pixel value into a discrete class index using thresholds.
///
/// `thresholds`: ascending list; returned class is the index of the first
/// threshold that `v` is less than. Defaults to `thresholds.len()` if v
/// exceeds all thresholds.
pub fn get_class(v: f32, thresholds: &[f32]) -> usize {
    for (i, &t) in thresholds.iter().enumerate() {
        if v < t {
            return i;
        }
    }
    thresholds.len()
}

#[cfg(test)]
mod batch_h_tests {
    use super::*;
    use crate::{
        builder::RasterDatasetBuilder,
        dataset::{LayerMapping, RasterDataset},
        types::{BlockSize, ImageResolution},
    };

    fn ds_for_test() -> (RasterDataset<i16>, tempfile::TempPath) {
        let f = crate::test_support::tiny_geotiff(8, 8, 42, 4326);
        let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[f.to_path_buf()])
            .unwrap()
            .resolution(ImageResolution { x: 1.0, y: -1.0 })
            .block_size(BlockSize { rows: 4, cols: 4 })
            .build()
            .unwrap();
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 1;
        rds.layer_mappings = vec![LayerMapping {
            source: f.to_path_buf(),
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        }];
        (rds, f)
    }

    #[test]
    fn block_id_rowcol_locates_pixel() {
        let (rds, _live) = ds_for_test();
        let (id, lr, lc) = block_id_rowcol(&rds, 5, 5).expect("in bounds");
        assert!(id < rds.num_blocks());
        // pixel (5,5) is in block at offset (4,4) — local pos (1,1)
        assert_eq!((lr, lc), (1, 1));
    }

    #[test]
    fn block_id_rowcol_out_of_bounds_returns_none() {
        let (rds, _live) = ds_for_test();
        assert!(block_id_rowcol(&rds, 100, 100).is_none());
    }

    #[test]
    fn geoms_to_global_indices_converts_each_point() {
        let (rds, _live) = ds_for_test();
        // geo_transform: [0, 1, 0, 8, 0, -1]; geo(0.5, 7.5) → row=0 col=0
        let pts = vec![(0.5, 7.5), (1.5, 6.5)];
        let indices = geoms_to_global_indices(&rds, &pts);
        assert_eq!(indices, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn extract_blockwise_returns_vec_in_input_order() {
        let (rds, _live) = ds_for_test();
        let pts = vec![(1.5, 6.5), (100.0, 100.0), (3.5, 4.5)];
        let out = extract_blockwise(&rds, &pts);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], Some(42));
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(42));
    }

    #[test]
    fn get_class_returns_first_below_threshold() {
        // thresholds [0.2, 0.5, 0.8]
        // -0.1 → class 0 (below 0.2)
        // 0.3 → class 1 (below 0.5)
        // 0.7 → class 2 (below 0.8)
        // 0.9 → class 3 (above all)
        assert_eq!(get_class(-0.1, &[0.2, 0.5, 0.8]), 0);
        assert_eq!(get_class(0.3, &[0.2, 0.5, 0.8]), 1);
        assert_eq!(get_class(0.7, &[0.2, 0.5, 0.8]), 2);
        assert_eq!(get_class(0.9, &[0.2, 0.5, 0.8]), 3);
    }
}
