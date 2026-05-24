//! openEO `mask_from_values(data, band, values, target_band)` — produces a
//! binary u8 mask cube from membership of a categorical band's pixel values
//! in a discrete set.
//!
//! Canonical use: build a Sentinel-2 cloud/shadow/snow mask from SCL classes
//! without needing the bespoke `mask_scl_dilation`. Composes with the generic
//! [`mask(data, mask)`](super::GeoExecutor::eval_mask) so the canonical openEO
//! graph reads:
//!
//! ```text
//! load_collection (B04, B08, SCL)
//!   → binmask = mask_from_values(data=load, band="SCL", values=[3,8,9,10,11])
//!   → masked  = mask(data=load, mask=binmask)
//!   → ndvi → reduce_dimension(t, mean) → save_result
//! ```

use std::path::PathBuf;

use ndarray::Array3;
use orbit_geo::types::{BlockSize, Dimension, ImageResolution};
use orbit_geo::{LayerMapping, RasterDataBlock, RasterDataset, RasterDatasetBuilder};
use serde_json::Value;

use crate::data_cube::DataCube;
use crate::executor::ExecError;

use super::GeoExecutor;

/// Default Sentinel-2 SCL cloud-set: shadow (3), medium-prob cloud (8),
/// high-prob cloud (9), thin cirrus (10), snow/ice (11). Matches the
/// default `mask2_values` of `mask_scl_dilation`.
const DEFAULT_SCL_MASK_VALUES: &[u8] = &[3, 8, 9, 10, 11];

impl GeoExecutor {
    /// `mask_from_values(data, band?, values?, target_band?)`.
    ///
    /// Reads the named `band` (default `"SCL"`) from `data.__cube` and
    /// writes one u8 GeoTIFF per scene where each pixel is `1` if the
    /// source value is in `values` (default `[3, 8, 9, 10, 11]`), else
    /// `0`. Output is a new cube with exactly one band named
    /// `target_band` (default `"mask"`).
    pub(super) async fn eval_mask_from_values(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let band_name: String = args
            .get("band")
            .and_then(|v| v.as_str())
            .unwrap_or("SCL")
            .to_string();
        let target_band: String = args
            .get("target_band")
            .and_then(|v| v.as_str())
            .unwrap_or("mask")
            .to_string();
        // B1: both names flow into scratch_dir.join and band_name into __cube key.
        super::identifier::validate_identifier(&band_name, "mask_from_values.band")?;
        super::identifier::validate_identifier(&target_band, "mask_from_values.target_band")?;

        // L2: cap `values` length at 4096 (defense in depth — SCL has 12
        // classes, QA bitfields max 32). A 12M-int array would otherwise
        // burn O(R*C*N) memory per scene.
        const MAX_VALUES_LEN: usize = 4096;
        let values: Vec<u8> = match args.get("values") {
            None | Some(Value::Null) => DEFAULT_SCL_MASK_VALUES.to_vec(),
            Some(Value::Array(arr)) => {
                if arr.len() > MAX_VALUES_LEN {
                    return Err(ExecError::InvalidGraph(format!(
                        "mask_from_values: `values` array length {} exceeds maximum {MAX_VALUES_LEN}",
                        arr.len()
                    )));
                }
                let mut out: Vec<u8> = Vec::with_capacity(arr.len());
                for v in arr {
                    let n = v.as_u64().ok_or_else(|| ExecError::InvalidGraph(
                        "mask_from_values: `values` must be an array of non-negative integers".into(),
                    ))?;
                    if n > 255 {
                        return Err(ExecError::InvalidGraph(format!(
                            "mask_from_values: value {n} out of u8 range (SCL classes fit in 0..=255)"
                        )));
                    }
                    out.push(n as u8);
                }
                if out.is_empty() {
                    return Err(ExecError::InvalidGraph(
                        "mask_from_values: `values` must be a non-empty array".into(),
                    ));
                }
                out
            }
            Some(other) => {
                return Err(ExecError::InvalidGraph(format!(
                    "mask_from_values: `values` must be an array, got {other}"
                )))
            }
        };

        let data = args
            .remove("data")
            .ok_or_else(|| ExecError::InvalidGraph("mask_from_values: missing `data`".into()))?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "mask_from_values: input is not a downloaded cube (call load_collection with spatial_extent first): {e}"
            ))
        })?;
        // Take ownership of the band's Vec<PathBuf> so no clone happens.
        let src_paths: Vec<PathBuf> = cube.take_band(&band_name).map_err(|_| {
            ExecError::InvalidGraph(format!(
                "mask_from_values: __cube.bands has no `{band_name}` band — \
                 `load_collection.bands` must include `\"{band_name}\"`"
            ))
        })?;
        if src_paths.is_empty() {
            return Err(ExecError::InvalidGraph(format!(
                "mask_from_values: __cube.bands[{band_name}] is empty (no scenes loaded)"
            )));
        }

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // L2: switch from linear scan (O(N) per pixel) to HashSet O(1).
        // Wrapped in Arc so the worker closure is `Send + Sync` and free of lifetimes.
        let values_set: std::sync::Arc<std::collections::HashSet<u8>> =
            std::sync::Arc::new(values.iter().copied().collect());

        // Per-scene worker: read the single u8 band, emit u8 where 1 = in-set, 0 = not.
        let build_mask = |src_path: &PathBuf, out_path: &PathBuf, t: usize| -> Result<(), ExecError> {
            let mut src_rds: RasterDataset<u8> =
                RasterDatasetBuilder::<u8>::from_files(&[src_path.clone()])
                    .map_err(|e| ExecError::Backend(format!("mask_from_values: builder t={t}: {e}")))?
                    .resolution(ImageResolution { x: 10.0, y: -10.0 })
                    .block_size(BlockSize {
                        rows: self.crop_size as usize,
                        cols: self.crop_size as usize,
                    })
                    .build()
                    .map_err(|e| ExecError::Backend(format!("mask_from_values: build t={t}: {e}")))?;
            src_rds.metadata.shape.times = 1;
            src_rds.metadata.shape.layers = 1;
            src_rds.layer_mappings = vec![LayerMapping {
                source: src_path.clone(),
                time_pos: 0,
                layer_pos: 0,
                band: 1,
            }];

            let set_for_worker = values_set.clone();
            let worker = move |rdb: &RasterDataBlock<u8>, _dim: Dimension| -> Array3<u8> {
                let r = rdb.rows();
                let c = rdb.cols();
                let mut out = Array3::<u8>::zeros((1, r, c));
                for row in 0..r {
                    for col in 0..c {
                        let v = rdb.data[[0, 0, row, col]];
                        if set_for_worker.contains(&v) {
                            out[[0, row, col]] = 1;
                        }
                    }
                }
                out
            };

            src_rds
                .apply_reduction::<u8, _>(
                    worker,
                    Dimension::Layer,
                    n_threads,
                    out_path,
                    0u8,
                )
                .map_err(|e| ExecError::Backend(format!(
                    "mask_from_values: apply_reduction t={t}: {e}"
                )))?;
            Ok(())
        };

        let mut mask_paths: Vec<PathBuf> = Vec::with_capacity(src_paths.len());
        for (t, src_path) in src_paths.iter().enumerate() {
            let out_path = self
                .scratch_dir
                .join(format!("{target_band}_from_values_t{t}.tif"));
            build_mask(src_path, &out_path, t)?;
            mask_paths.push(out_path);
        }

        // Output cube: a single band named `target_band` carrying the
        // binary mask paths. This is exactly what `eval_mask` expects on
        // its `mask` argument (mask.__cube.bands must have exactly one band).
        let scene_count = mask_paths.len() as u64;
        let mut out_cube = DataCube::new();
        out_cube.bands.insert(target_band.clone(), mask_paths);
        out_cube.bbox = cube.bbox.clone();
        out_cube.scene_count = Some(scene_count);
        out_cube.layers = Some(1);
        out_cube.layer_names = Some(vec![target_band.clone()]);
        out_cube.source_band = Some(band_name);
        Ok(out_cube.to_envelope())
    }
}
