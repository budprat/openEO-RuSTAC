//! `ndvi` process — pure per-scene NDVI worker.

use std::path::PathBuf;

use ndarray::{s, Array3};
use orbit_geo::types::{BlockSize, Dimension, ImageResolution};
use orbit_geo::{LayerMapping, RasterDataBlock, RasterDataset, RasterDatasetBuilder};
use crate::data_cube::DataCube;
use crate::executor::ExecError;
use serde_json::Value;

use super::{GeoExecutor, SENTINEL_NDVI_NA};

/// Pure NDVI worker — collapses Layer dim (2 layers: red, nir) into
/// a single NDVI value per pixel. Shape: input (1, 2, R, C) →
/// output (1, 1, R, C). NA when bands ≤ 0 or denom is degenerate.
fn pure_ndvi_worker(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> Array3<f32> {
    let r = rdb.rows();
    let c = rdb.cols();
    let mut out = Array3::<f32>::from_elem((1, r, c), SENTINEL_NDVI_NA);
    let red = rdb.data.slice(s![0, 0, .., ..]);
    let nir = rdb.data.slice(s![0, 1, .., ..]);
    for ((row, col), &rv) in red.indexed_iter() {
        let nv = nir[[row, col]];
        if rv <= 0 || nv <= 0 {
            continue;
        }
        let r_f = rv as f32;
        let n_f = nv as f32;
        let denom = r_f + n_f;
        if denom.abs() < 1.0 {
            continue;
        }
        out[[0, row, col]] = (n_f - r_f) / denom;
    }
    out
}

impl GeoExecutor {
    /// openEO `ndvi(data, nir, red, target_band)` — **pure** per-pixel NDVI.
    ///
    /// Computes `(NIR - RED) / (NIR + RED)` per pixel for **each** scene
    /// independently. **No temporal reduction. No masking. No aggregation.**
    /// Those are separate openEO processes (`reduce_dimension`, `mask`,
    /// `aggregate_temporal_period`, etc.) — keeping `ndvi` single-purpose
    /// makes the graph compositional and matches the openEO 1.3.0 spec.
    ///
    /// Input contract: `data.__cube` must carry a `bands` map with
    /// the requested `nir`/`red` keys (T scenes each).
    ///
    /// Output contract: returns a new `__cube` whose `bands` map holds
    /// a single entry keyed on `target_band` (default `"ndvi"`) → T
    /// single-band GeoTIFFs. Any OTHER bands present on the input
    /// cube (e.g. SCL) are forwarded unchanged so a downstream `mask`
    /// node can still find them.
    pub(super) async fn eval_ndvi(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        // Honor `nir`, `red`, `target_band` per openEO 1.3.0 spec.
        let nir_band = args
            .get("nir")
            .and_then(|v| v.as_str())
            .unwrap_or("B08")
            .to_string();
        let red_band = args
            .get("red")
            .and_then(|v| v.as_str())
            .unwrap_or("B04")
            .to_string();
        let target_band = args
            .get("target_band")
            .and_then(|v| v.as_str())
            .unwrap_or("ndvi")
            .to_string();

        // Decode the typed cube; takes ownership so the Vec<PathBuf>
        // band lists move out without cloning.
        let data = args
            .remove("data")
            .ok_or_else(|| ExecError::InvalidGraph("ndvi: missing `data`".into()))?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "ndvi: input is not a downloaded cube (call load_collection with spatial_extent first): {e}"
            ))
        })?;
        let reds: Vec<PathBuf> = cube.take_band(&red_band).map_err(|_| {
            ExecError::InvalidGraph(format!(
                "ndvi: band `{red_band}` not loaded — add it to load_collection.bands"
            ))
        })?;
        let nirs: Vec<PathBuf> = cube.take_band(&nir_band).map_err(|_| {
            ExecError::InvalidGraph(format!(
                "ndvi: band `{nir_band}` not loaded — add it to load_collection.bands"
            ))
        })?;
        if reds.len() != nirs.len() || reds.is_empty() {
            return Err(ExecError::Backend(format!(
                "ndvi: mismatched scene counts ({red_band}={}, {nir_band}={})",
                reds.len(),
                nirs.len()
            )));
        }

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let mut ndvi_paths: Vec<PathBuf> = Vec::with_capacity(reds.len());
        for (t, (red_path, nir_path)) in reds.iter().zip(nirs.iter()).enumerate() {
            // Per-scene cube: (T=1, layers=2: red, nir).
            let mut rds: RasterDataset<i16> =
                RasterDatasetBuilder::<i16>::from_files(&[red_path.clone(), nir_path.clone()])
                    .map_err(|e| ExecError::Backend(format!("ndvi: builder t={t}: {e}")))?
                    .resolution(ImageResolution { x: 10.0, y: -10.0 })
                    .block_size(BlockSize {
                        rows: self.crop_size as usize,
                        cols: self.crop_size as usize,
                    })
                    .build()
                    .map_err(|e| ExecError::Backend(format!("ndvi: build t={t}: {e}")))?;
            rds.metadata.shape.times = 1;
            rds.metadata.shape.layers = 2;
            rds.layer_mappings = vec![
                LayerMapping { source: red_path.clone(), time_pos: 0, layer_pos: 0, band: 1 },
                LayerMapping { source: nir_path.clone(), time_pos: 0, layer_pos: 1, band: 1 },
            ];

            let out_path = self.scratch_dir.join(format!("{target_band}_t{t}.tif"));
            rds.apply_reduction::<f32, _>(
                pure_ndvi_worker,
                Dimension::Layer,
                n_threads,
                &out_path,
                SENTINEL_NDVI_NA,
            )
            .map_err(|e| ExecError::Backend(format!("ndvi: apply_reduction t={t}: {e}")))?;
            ndvi_paths.push(out_path);
        }

        // Build the output cube: the new `target_band` entry, plus every
        // remaining band on the input (red/nir are already taken out, so
        // SCL or other auxiliary bands flow through to a downstream mask
        // node — moved by value, no clone).
        let scene_count = reds.len() as u64;
        let mut out_cube = DataCube::new();
        out_cube.bands.insert(target_band.clone(), ndvi_paths);
        for (k, v) in std::mem::take(&mut cube.bands) {
            if k == red_band || k == nir_band || k == target_band {
                continue;
            }
            out_cube.bands.insert(k, v);
        }
        out_cube.bbox = cube.bbox.clone();
        out_cube.scene_count = Some(scene_count);
        out_cube.times = Some(scene_count);
        out_cube.layers = Some(1);
        out_cube.layer_names = Some(vec![target_band.clone()]);
        Ok(out_cube.to_envelope())
    }
}
