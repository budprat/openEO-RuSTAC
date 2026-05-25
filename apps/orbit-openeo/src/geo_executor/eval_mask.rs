//! openEO `mask` + `mask_scl_dilation` processes.

use std::path::{Path, PathBuf};

use ndarray::Array3;
use orbit_geo::types::{BlockSize, Dimension, ImageResolution};
use orbit_geo::{LayerMapping, RasterDataBlock, RasterDataset, RasterDatasetBuilder};
use serde_json::Value;

use crate::data_cube::DataCube;
use crate::executor::ExecError;

use super::GeoExecutor;

/// Smallest difference in pixel size we consider a genuine resolution
/// mismatch. Sub-decimetre noise after `-projwin` snapping is not.
const RESOLUTION_TOLERANCE_M: f64 = 0.1;

/// Resample a categorical SCL raster onto the data band's pixel grid.
///
/// Returns the original `scl_path` when SCL and data already share the
/// same `(cols, rows)` (within tolerance). Otherwise spawns `gdalwarp -r near`
/// (nearest neighbour preserves class boundaries 0..=11) into `out_path`
/// and returns that. `out_path` is reused if it already exists with the
/// expected dims — cheap re-call cache for chained mask passes.
fn resample_scl_to_data_grid(
    scl_path: &Path,
    data_path: &Path,
    out_path: &Path,
) -> Result<PathBuf, ExecError> {
    let data_ds = gdal::Dataset::open(data_path).map_err(|e| {
        ExecError::Backend(format!("mask_scl: open data {}: {e}", data_path.display()))
    })?;
    let scl_ds = gdal::Dataset::open(scl_path).map_err(|e| {
        ExecError::Backend(format!("mask_scl: open scl {}: {e}", scl_path.display()))
    })?;
    let (data_cols, data_rows) = data_ds.raster_size();
    let (scl_cols, scl_rows) = scl_ds.raster_size();
    let data_gt = data_ds.geo_transform().map_err(|e| {
        ExecError::Backend(format!("mask_scl: geo_transform data: {e}"))
    })?;
    let scl_gt = scl_ds.geo_transform().map_err(|e| {
        ExecError::Backend(format!("mask_scl: geo_transform scl: {e}"))
    })?;
    // Skip the resample when SCL already matches data dims AND has
    // comparable pixel size — the happy path on legacy/aligned inputs.
    if scl_cols == data_cols
        && scl_rows == data_rows
        && (data_gt[1] - scl_gt[1]).abs() <= RESOLUTION_TOLERANCE_M
        && (data_gt[5] - scl_gt[5]).abs() <= RESOLUTION_TOLERANCE_M
    {
        return Ok(scl_path.to_path_buf());
    }
    // Cheap cache: if a prior call already wrote out_path with the right
    // dims, reuse it instead of re-spawning gdalwarp.
    if out_path.exists() {
        if let Ok(prev) = gdal::Dataset::open(out_path) {
            let (pc, pr) = prev.raster_size();
            if pc == data_cols && pr == data_rows {
                return Ok(out_path.to_path_buf());
            }
        }
    }
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ExecError::Backend(format!("mask_scl: mkdir {}: {e}", parent.display()))
            })?;
        }
    }
    // Pixel-precise target window from data's geo_transform.
    let xmin = data_gt[0];
    let ymax = data_gt[3];
    let xmax = data_gt[0] + data_gt[1] * data_cols as f64;
    let ymin = data_gt[3] + data_gt[5] * data_rows as f64;
    let xres = data_gt[1].abs();
    let yres = data_gt[5].abs();
    let mut argv: Vec<String> = vec![
        "gdalwarp".into(),
        "-q".into(),
        "-overwrite".into(),
        "-r".into(),
        "near".into(),
        "-te".into(),
        xmin.min(xmax).to_string(),
        ymin.min(ymax).to_string(),
        xmin.max(xmax).to_string(),
        ymin.max(ymax).to_string(),
        "-tr".into(),
        xres.to_string(),
        yres.to_string(),
    ];
    // Carry the data raster's projection through so SCL pixels are
    // resampled into the data's CRS even if PROJ thinks they differ.
    let data_proj = data_ds.projection();
    if !data_proj.is_empty() {
        argv.push("-t_srs".into());
        argv.push(data_proj);
    }
    // P1-8: no `--` sentinel; option-injection defence is upstream.
    argv.push(scl_path.display().to_string());
    argv.push(out_path.display().to_string());
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| ExecError::Backend(format!(
            "mask_scl: spawn `{}`: {e} — is gdal installed and on PATH?",
            argv[0]
        )))?;
    if !status.success() {
        return Err(ExecError::Backend(format!(
            "mask_scl: gdalwarp exited with status {status} (scl={}, out={})",
            scl_path.display(), out_path.display()
        )));
    }
    Ok(out_path.to_path_buf())
}

impl GeoExecutor {
    /// openEO `mask_scl_dilation(data, scl_band_name?, kernel1_size?, kernel2_size?, mask1_values?, mask2_values?, erosion_kernel_size?)`.
    ///
    /// **Spec note**: `mask_scl_dilation` is not in the standard openEO 1.3.0 process
    /// catalog — it's a VITO / openeo-processes-experimental extension that wraps the
    /// generic [`mask(data, mask, replacement?)`] process by building the binary mask
    /// from Sentinel-2 SCL classes internally. Semantically it must behave like `mask`:
    /// **applies to every data band** in the cube, not just red/nir.
    pub(super) async fn eval_mask_scl_dilation(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        // SCL band name is configurable (default "SCL"). It is the
        // mask source and MUST be present in the input cube's `bands`.
        let scl_band_name: String = args
            .get("scl_band_name")
            .and_then(|v| v.as_str())
            .unwrap_or("SCL")
            .to_string();
        // B1: scl_band_name flows into scratch_dir.join via masked_bands.
        super::identifier::validate_identifier(&scl_band_name, "mask_scl_dilation.scl_band_name")?;
        let data = args
            .remove("data")
            .ok_or_else(|| ExecError::InvalidGraph("mask_scl_dilation: missing `data`".into()))?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "mask_scl_dilation: input is not a downloaded cube (call load_collection with spatial_extent first): {e}"
            ))
        })?;
        let scl_paths: Vec<PathBuf> = cube.take_band(&scl_band_name).map_err(|_| {
            ExecError::InvalidGraph(format!(
                "mask_scl_dilation: __cube.bands has no `{scl_band_name}` band — \
                 `load_collection.bands` must include `\"{scl_band_name}\"`"
            ))
        })?;
        // Band-agnostic discovery: every remaining band feeds the masker
        // (SCL was already removed above). Move by-value — no clones.
        let band_paths: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::mem::take(&mut cube.bands);
        if band_paths.is_empty() {
            return Err(ExecError::InvalidGraph(
                "mask_scl_dilation: __cube.bands has no data bands (only the SCL mask was found) — \
                 apply mask BEFORE `ndvi` per openEO spec".into(),
            ));
        }
        for (band_key, paths) in &band_paths {
            if paths.len() != scl_paths.len() {
                return Err(ExecError::Backend(format!(
                    "mask_scl_dilation: scene count mismatch ({band_key}={}, scl={})",
                    paths.len(),
                    scl_paths.len()
                )));
            }
        }

        // Parse mask2_values (the "mask out" set). Default: shadows + clouds + snow.
        let mask2: std::collections::BTreeSet<u8> = args
            .get("mask2_values")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64())
                    .filter(|v| *v <= 255)
                    .map(|v| v as u8)
                    .collect()
            })
            .unwrap_or_else(|| [3u8, 8, 9, 10, 11].iter().copied().collect());

        // v1: kernel parameters are accepted but no-op (no morph dilation yet).
        let k1 = args.get("kernel1_size").and_then(|v| v.as_u64()).unwrap_or(0);
        let k2 = args.get("kernel2_size").and_then(|v| v.as_u64()).unwrap_or(0);
        let ker_e = args.get("erosion_kernel_size").and_then(|v| v.as_u64()).unwrap_or(0);
        if k1 > 0 || k2 > 0 || ker_e > 0 {
            tracing::warn!(
                kernel1_size = k1, kernel2_size = k2, erosion_kernel_size = ker_e,
                "mask_scl_dilation v1 ignores kernel sizes — morph dilation not yet implemented"
            );
        }

        // Sentinel-2 SCL is published at 20 m while B04/B08/etc. are at 10 m.
        // Passing the raw SCL into apply_reduction_with_mask trips the
        // num_blocks consistency check ("data has 9 blocks, mask has 4")
        // because block partitioning is cols/rows / block_size.
        //
        // **BUG-001 fix (2026-05-24)**: resample SCL **per data band**, not
        // just to the first band's grid. The pre-fix code picked
        // `band_paths.values().next()` (first band alphabetically) and
        // reused that grid for every band's mask. When the load mixed 10 m
        // bands (B04, B08) with 20 m bands (B11, B12), the 20 m bands
        // misaligned with the now-10 m mask and fell over with
        // "data has 4 blocks, mask has 9".
        //
        // Per-band resample produces one SCL-resampled file per
        // (band, scene) combination, indexed by band key. Disk cost: 4×
        // for a 4-band load vs the prior 1× — acceptable since SCL crops
        // are typically <100 KB each and the executor's Drop GC reclaims
        // scratch at process exit (Task #38).
        //
        // P0-5: gdalwarp shells out and blocks — wrap in spawn_blocking
        // so the async runtime keeps progressing other jobs.
        let mut effective_scl_paths_per_band: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for (band_key, band_paths_for_band) in &band_paths {
            super::identifier::validate_identifier(band_key, "mask_scl_dilation.bands")?;
            let mut per_band_scls: Vec<PathBuf> = Vec::with_capacity(scl_paths.len());
            for (t, scl_path) in scl_paths.iter().enumerate() {
                let data_path = band_paths_for_band.get(t).cloned().ok_or_else(|| {
                    ExecError::Backend(format!(
                        "mask_scl: missing band `{band_key}` path at scene t={t}"
                    ))
                })?;
                let out_path = self
                    .scratch_dir
                    .join(format!("scl_resampled_{band_key}_t{t}.tif"));
                let scl_path_clone = scl_path.clone();
                let resampled = tokio::task::spawn_blocking(move || {
                    resample_scl_to_data_grid(&scl_path_clone, &data_path, &out_path)
                })
                .await
                .map_err(|e| {
                    ExecError::Backend(format!(
                        "mask_scl: resample join {band_key} t={t}: {e}"
                    ))
                })??;
                per_band_scls.push(resampled);
            }
            effective_scl_paths_per_band.insert(band_key.clone(), per_band_scls);
        }

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Per-pixel masking worker. Input cube is (T=1, layers=1, R, C),
        // mask cube is (T=1, layers=1, R, C). Output: same shape, with
        // SENTINEL_NDVI_NA where SCL ∈ mask2_values.
        //
        // We close over `mask2` via a `Box<[u8]>` snapshot so the worker
        // is `Send + Sync` and free of lifetimes.
        let mask_set: std::sync::Arc<[u8]> = mask2.iter().copied().collect();
        // Per-band, per-scene mask worker — i16 input, i16 output. Pixels
        // where SCL ∈ mask2 → 0 (i16 nodata sentinel matching what GDAL
        // emits for "no data" in Sentinel-2 surface reflectance products).
        // Pixels outside the mask are passed through unchanged.
        let mask_band = |band_path: &PathBuf,
                         scl_path: &PathBuf,
                         out_path: &PathBuf,
                         label: &str,
                         t: usize|
              -> Result<(), ExecError> {
            let mut data_rds: RasterDataset<i16> =
                RasterDatasetBuilder::<i16>::from_files(&[band_path.clone()])
                    .map_err(|e| ExecError::Backend(format!("mask_scl: {label} builder t={t}: {e}")))?
                    .resolution(ImageResolution { x: 10.0, y: -10.0 })
                    .block_size(BlockSize {
                        rows: self.crop_size as usize,
                        cols: self.crop_size as usize,
                    })
                    .build()
                    .map_err(|e| ExecError::Backend(format!("mask_scl: {label} build t={t}: {e}")))?;
            data_rds.metadata.shape.times = 1;
            data_rds.metadata.shape.layers = 1;
            data_rds.layer_mappings = vec![LayerMapping {
                source: band_path.clone(),
                time_pos: 0,
                layer_pos: 0,
                band: 1,
            }];

            let mut mask_rds: RasterDataset<u8> =
                RasterDatasetBuilder::<u8>::from_files(&[scl_path.clone()])
                    .map_err(|e| ExecError::Backend(format!("mask_scl: scl builder t={t}: {e}")))?
                    .resolution(ImageResolution { x: 10.0, y: -10.0 })
                    .block_size(BlockSize {
                        rows: self.crop_size as usize,
                        cols: self.crop_size as usize,
                    })
                    .build()
                    .map_err(|e| ExecError::Backend(format!("mask_scl: scl build t={t}: {e}")))?;
            mask_rds.metadata.shape.times = 1;
            mask_rds.metadata.shape.layers = 1;
            mask_rds.layer_mappings = vec![LayerMapping {
                source: scl_path.clone(),
                time_pos: 0,
                layer_pos: 0,
                band: 1,
            }];

            let mask_for_worker = mask_set.clone();
            let worker = move |rdb: &RasterDataBlock<i16>,
                               mblock: &RasterDataBlock<u8>,
                               _dim: Dimension|
                  -> Array3<i16> {
                let r = rdb.rows();
                let c = rdb.cols();
                let mut out = Array3::<i16>::zeros((1, r, c));
                // Mask cube may be off by 1 pixel after UTM reprojection from
                // a WGS84 bbox — clamp iteration to the intersection of dims.
                let mr = mblock.rows().min(r);
                let mc = mblock.cols().min(c);
                for row in 0..mr {
                    for col in 0..mc {
                        let scl = mblock.data[[0, 0, row, col]];
                        if mask_for_worker.contains(&scl) {
                            continue; // masked out → 0 (i16 nodata sentinel)
                        }
                        out[[0, row, col]] = rdb.data[[0, 0, row, col]];
                    }
                }
                out
            };

            data_rds
                .apply_reduction_with_mask::<u8, i16, _>(
                    &mask_rds,
                    worker,
                    Dimension::Layer,
                    n_threads,
                    out_path,
                    0_i16,
                )
                .map_err(|e| {
                    ExecError::Backend(format!("mask_scl: {label} apply_reduction_with_mask t={t}: {e}"))
                })?;
            Ok(())
        };

        // Mask every discovered band, per-scene. Output paths follow the
        // pattern `<band>_masked_t{t}.tif` (e.g. B04_masked_t0.tif).
        // BUG-001 fix: zip each band against its OWN resampled SCL grid
        // (was: every band zipped against the same first-band grid).
        let mut masked_bands: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for (band_key, paths) in &band_paths {
            // identifier already validated above during per-band SCL resample.
            let scl_paths_for_band = effective_scl_paths_per_band
                .get(band_key)
                .ok_or_else(|| ExecError::Backend(format!(
                    "mask_scl: missing resampled SCL for band `{band_key}`"
                )))?;
            let mut masked: Vec<PathBuf> = Vec::with_capacity(paths.len());
            for (t, (band_path, scl_path)) in
                paths.iter().zip(scl_paths_for_band.iter()).enumerate()
            {
                let out_path = self.scratch_dir.join(format!("{band_key}_masked_t{t}.tif"));
                mask_band(band_path, scl_path, &out_path, band_key, t)?;
                masked.push(out_path);
            }
            masked_bands.insert(band_key.clone(), masked);
        }

        // Emit a new band cube. All discovered bands replaced with masked
        // versions; SCL forwarded unchanged (original paths, NOT the
        // resampled grid) so additional mask passes / audit can still
        // reach the classification layer at its native resolution.
        let sentinel_scene_count = scl_paths.len() as u64;
        let masked_band_names: Vec<String> = masked_bands.keys().cloned().collect();
        let mut out_cube = DataCube::new();
        for (band_key, masked) in masked_bands {
            out_cube.bands.insert(band_key, masked);
        }
        out_cube.bands.insert(scl_band_name.clone(), scl_paths);
        out_cube.bbox = cube.bbox.clone();
        out_cube.scene_count = Some(sentinel_scene_count);
        out_cube.masked_bands = Some(masked_band_names);
        out_cube.masked_by = Some("mask_scl_dilation".into());
        Ok(out_cube.to_envelope())
    }

    /// openEO standard `mask(data, mask, replacement?)`.
    pub(super) async fn eval_mask(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        // B3: openEO spec says `replacement: null` means "no data" — must
        // write the i16 NA sentinel (i16::MIN per workspace convention,
        // orbit-geo/src/lib.rs), NOT 0 (which downstream ops treat as real data).
        // `replacement` omitted → default to NA. Explicit number → use it.
        let replacement: i16 = match args.get("replacement") {
            None | Some(Value::Null) => i16::MIN,
            Some(Value::Number(n)) => n
                .as_f64()
                .map(|f| f.clamp(i16::MIN as f64, i16::MAX as f64) as i16)
                .ok_or_else(|| ExecError::InvalidGraph("mask: replacement number out of range".into()))?,
            Some(other) => return Err(ExecError::InvalidGraph(format!(
                "mask: replacement must be a number or null, got {other}"
            ))),
        };

        let data_outer = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("mask: missing `data.__cube` (run load_collection first)".into())
        })?;
        let mut data_cube = DataCube::from_envelope_owned(data_outer).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "mask: missing `data.__cube` (run load_collection first): {e}"
            ))
        })?;
        let bbox_passthrough = data_cube.bbox.clone();
        // Indices to skip — these are f32, not i16, so passing them
        // through the i16 mask worker would corrupt the data.
        const F32_INDEX_BANDS: &[&str] = &["ndvi", "ndmi", "ndwi", "evi", "savi", "msavi"];
        // Preserve SCL entries for passthrough on output (drain by value).
        let mut scl_passthrough: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for k in ["SCL", "scl"] {
            if let Some(v) = data_cube.bands.remove(k) {
                scl_passthrough.insert(k.to_string(), v);
            }
        }
        let mut data_bands: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for (k, v) in std::mem::take(&mut data_cube.bands) {
            if F32_INDEX_BANDS.contains(&k.as_str()) {
                continue;
            }
            data_bands.insert(k, v);
        }
        if data_bands.is_empty() {
            return Err(ExecError::InvalidGraph(
                "mask: data.__cube has no maskable bands (need ≥1 i16 band in `bands`, \
                 excluding SCL and index bands like ndvi/ndmi)".into(),
            ));
        }

        // Mask cube must have exactly one band in its `bands` map.
        let mask_outer = args.remove("mask").ok_or_else(|| {
            ExecError::InvalidGraph("mask: missing `mask.__cube` (mask must be a downloaded cube)".into())
        })?;
        let mut mask_cube = DataCube::from_envelope_owned(mask_outer).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "mask: missing `mask.__cube` (mask must be a downloaded cube): {e}"
            ))
        })?;
        if mask_cube.bands.len() != 1 {
            return Err(ExecError::InvalidGraph(format!(
                "mask: mask.__cube must have exactly one band, found {}",
                mask_cube.bands.len()
            )));
        }
        // Drain the single entry by value.
        let (_mask_key, mask_paths): (String, Vec<PathBuf>) =
            std::mem::take(&mut mask_cube.bands)
                .into_iter()
                .next()
                .ok_or_else(|| ExecError::InvalidGraph("mask: mask.__cube has no bands".into()))?;

        // Scene-count alignment.
        for (band_key, paths) in &data_bands {
            if paths.len() != mask_paths.len() {
                return Err(ExecError::Backend(format!(
                    "mask: scene count mismatch ({band_key}={}, mask={})",
                    paths.len(),
                    mask_paths.len()
                )));
            }
        }

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Per-band, per-scene worker. Mask cube is u8: > 0 → replace, else copy data.
        let apply_one = |band_path: &PathBuf,
                         mask_path: &PathBuf,
                         out_path: &PathBuf,
                         label: &str,
                         t: usize|
              -> Result<(), ExecError> {
            let mut data_rds: RasterDataset<i16> =
                RasterDatasetBuilder::<i16>::from_files(&[band_path.clone()])
                    .map_err(|e| ExecError::Backend(format!("mask: {label} data builder t={t}: {e}")))?
                    .resolution(ImageResolution { x: 10.0, y: -10.0 })
                    .block_size(BlockSize {
                        rows: self.crop_size as usize,
                        cols: self.crop_size as usize,
                    })
                    .build()
                    .map_err(|e| ExecError::Backend(format!("mask: {label} data build t={t}: {e}")))?;
            data_rds.metadata.shape.times = 1;
            data_rds.metadata.shape.layers = 1;
            data_rds.layer_mappings = vec![LayerMapping {
                source: band_path.clone(), time_pos: 0, layer_pos: 0, band: 1,
            }];

            let mut mask_rds: RasterDataset<u8> =
                RasterDatasetBuilder::<u8>::from_files(&[mask_path.clone()])
                    .map_err(|e| ExecError::Backend(format!("mask: mask builder t={t}: {e}")))?
                    .resolution(ImageResolution { x: 10.0, y: -10.0 })
                    .block_size(BlockSize {
                        rows: self.crop_size as usize,
                        cols: self.crop_size as usize,
                    })
                    .build()
                    .map_err(|e| ExecError::Backend(format!("mask: mask build t={t}: {e}")))?;
            mask_rds.metadata.shape.times = 1;
            mask_rds.metadata.shape.layers = 1;
            mask_rds.layer_mappings = vec![LayerMapping {
                source: mask_path.clone(), time_pos: 0, layer_pos: 0, band: 1,
            }];

            let rep = replacement;
            let worker = move |rdb: &RasterDataBlock<i16>,
                               mblock: &RasterDataBlock<u8>,
                               _dim: Dimension|
                  -> Array3<i16> {
                let r = rdb.rows();
                let c = rdb.cols();
                let mut out = Array3::<i16>::from_elem((1, r, c), rep);
                // Mask cube may be off by 1 pixel after UTM reprojection from
                // a WGS84 bbox — clamp iteration to the intersection of dims.
                let mr = mblock.rows().min(r);
                let mc = mblock.cols().min(c);
                for row in 0..mr {
                    for col in 0..mc {
                        // Spec: replace where mask is truthy (non-zero / non-null).
                        if mblock.data[[0, 0, row, col]] > 0 {
                            continue; // already `rep` from from_elem
                        }
                        out[[0, row, col]] = rdb.data[[0, 0, row, col]];
                    }
                }
                out
            };

            data_rds
                .apply_reduction_with_mask::<u8, i16, _>(
                    &mask_rds,
                    worker,
                    Dimension::Layer,
                    n_threads,
                    out_path,
                    replacement,
                )
                .map_err(|e| ExecError::Backend(format!(
                    "mask: {label} apply_reduction_with_mask t={t}: {e}"
                )))?;
            Ok(())
        };

        let mut masked_bands: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for (band_key, paths) in &data_bands {
            super::identifier::validate_identifier(band_key, "mask.bands")?;
            let mut masked = Vec::with_capacity(paths.len());
            for (t, (band_path, mask_path)) in paths.iter().zip(mask_paths.iter()).enumerate() {
                let out_path = self.scratch_dir.join(format!("{band_key}_mask_t{t}.tif"));
                apply_one(band_path, mask_path, &out_path, band_key, t)?;
                masked.push(out_path);
            }
            masked_bands.insert(band_key.clone(), masked);
        }

        // Build the output cube: every input band replaced with its masked
        // variant. SCL (if present on the input) forwarded unchanged so
        // chained mask passes still see it. Move by value — no clone.
        let scene_count = mask_paths.len() as u64;
        let masked_band_names: Vec<String> = masked_bands.keys().cloned().collect();
        let mut out_cube = DataCube::new();
        for (band_key, masked) in masked_bands {
            out_cube.bands.insert(band_key, masked);
        }
        for (k, v) in scl_passthrough {
            out_cube.bands.insert(k, v);
        }
        out_cube.bbox = bbox_passthrough;
        out_cube.scene_count = Some(scene_count);
        out_cube.masked_bands = Some(masked_band_names);
        out_cube.masked_by = Some("mask".into());
        out_cube.replacement = Some(replacement as i64);
        Ok(out_cube.to_envelope())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_temp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "orbit-geoexec-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    use serde_json::json;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_scl_dilation_rejects_when_cube_missing_scl_paths() {
        let exe = GeoExecutor::new();
        // Band cube (B04+B08) but no SCL — should reject.
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({
            "__cube": {
                "bands": {
                    "B04": ["/tmp/red_t0.tif"],
                    "B08": ["/tmp/nir_t0.tif"]
                },
                "bbox": [0.0, 0.0, 1.0, 1.0]
            }
        }));
        let r = exe.eval_mask_scl_dilation(args).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
            "expected InvalidGraph when SCL band is missing, got {r:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_scl_dilation_rejects_when_cube_missing_band_paths() {
        let exe = GeoExecutor::new();
        // SCL but no other bands — mask should reject (cannot apply to bands
        // that aren't there; mask MUST come BEFORE ndvi per openEO spec).
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({
            "__cube": {
                "bands": { "SCL": ["/tmp/scl_t0.tif"] }
            }
        }));
        let r = exe.eval_mask_scl_dilation(args).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
            "expected InvalidGraph when no data bands present, got {r:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_scl_dilation_rejects_when_scene_counts_mismatch() {
        let scratch = local_temp_root("mask-mismatch");
        std::fs::create_dir_all(&scratch).ok();
        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone());
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({
            "__cube": {
                "bands": {
                    "B04": ["/tmp/r0.tif", "/tmp/r1.tif"],
                    "B08": ["/tmp/n0.tif", "/tmp/n1.tif"],
                    "SCL": ["/tmp/s0.tif"]
                }
            }
        }));
        let r = exe.eval_mask_scl_dilation(args).await;
        assert!(matches!(r, Err(ExecError::Backend(ref m)) if m.contains("scene count mismatch")),
            "expected Backend scene-count mismatch, got {r:?}");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_rejects_when_mask_cube_has_zero_band_paths() {
        let exe = GeoExecutor::new();
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({"__cube": {
            "bands": { "B04": ["/tmp/r0.tif"] }
        }}));
        args.insert("mask".into(), json!({"__cube": {
            "bbox": [0.0, 0.0, 1.0, 1.0]
        }}));
        let r = exe.eval_mask(args).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(ref m)) if m.contains("exactly one")),
            "expected InvalidGraph on zero mask bands, got {r:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_rejects_when_replacement_is_not_a_number_or_null() {
        let exe = GeoExecutor::new();
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({"__cube": {
            "bands": { "B04": ["/tmp/r0.tif"] }
        }}));
        args.insert("mask".into(), json!({"__cube": {
            "bands": { "mask": ["/tmp/m0.tif"] }
        }}));
        args.insert("replacement".into(), json!("not-a-number"));
        let r = exe.eval_mask(args).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(ref m)) if m.contains("replacement")),
            "expected InvalidGraph on bad replacement, got {r:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_rejects_when_scene_counts_mismatch() {
        let exe = GeoExecutor::new();
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({"__cube": {
            "bands": { "B04": ["/tmp/r0.tif", "/tmp/r1.tif"] }
        }}));
        args.insert("mask".into(), json!({"__cube": {
            "bands": { "mask": ["/tmp/m0.tif"] }
        }}));
        let r = exe.eval_mask(args).await;
        assert!(matches!(r, Err(ExecError::Backend(ref m)) if m.contains("scene count mismatch")),
            "expected Backend scene-count error, got {r:?}");
    }

    /// RED → GREEN: regression for the "data has 9 blocks, mask has 4"
    /// production failure against real Sentinel-2 L2A inputs (SCL @ 20 m,
    /// B04/B08 @ 10 m). The legacy path fed the SCL raster directly into
    /// `apply_reduction_with_mask`, which rejected on `num_blocks` mismatch.
    /// After the auto-resample, mask_scl_dilation must accept rasters at
    /// 2× resolution offset and produce an output cube whose bands match
    /// the data band's pixel grid.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mask_scl_dilation_resamples_scl_when_resolution_differs() {
        let scratch = local_temp_root("mask-scl-resample");
        // Write a 10x10 i16 B04 raster at 10 m, and a 5x5 u8 SCL raster
        // at 20 m. Both share the same UTM bbox (origin 500000, 5000000;
        // 100 m square in EPSG:32633). Block_size is forced to 4 via
        // `with_crop(0, 4)` so num_blocks differs (data=9, mask=4) — the
        // exact failure mode `tail /tmp/orbit-real-s2.log` reproduced.
        let data_path = scratch.join("B04_t0.tif");
        let scl_path = scratch.join("SCL_t0.tif");
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        // Data at 10 m, 10×10.
        {
            let mut ds = driver
                .create_with_band_type::<i16, _>(&data_path, 10, 10, 1)
                .unwrap();
            // [originX, pxW, 0, originY, 0, pxH(neg)]
            ds.set_geo_transform(&[500_000.0, 10.0, 0.0, 5_000_100.0, 0.0, -10.0]).unwrap();
            let srs = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
            ds.set_spatial_ref(&srs).unwrap();
            let mut band = ds.rasterband(1).unwrap();
            let mut buf = gdal::raster::Buffer::new((10, 10), vec![1234i16; 100]);
            band.write::<i16>((0, 0), (10, 10), &mut buf).unwrap();
        }
        // SCL at 20 m, 5×5, same bbox. Mostly class 4 (vegetation, kept).
        // One pixel of class 9 (high cloud, masked) at (0,0).
        {
            let mut ds = driver
                .create_with_band_type::<u8, _>(&scl_path, 5, 5, 1)
                .unwrap();
            ds.set_geo_transform(&[500_000.0, 20.0, 0.0, 5_000_100.0, 0.0, -20.0]).unwrap();
            let srs = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
            ds.set_spatial_ref(&srs).unwrap();
            let mut data = vec![4u8; 25];
            data[0] = 9; // top-left masked
            let mut band = ds.rasterband(1).unwrap();
            let mut buf = gdal::raster::Buffer::new((5, 5), data);
            band.write::<u8>((0, 0), (5, 5), &mut buf).unwrap();
        }

        let exe = GeoExecutor::new()
            .with_scratch_dir(scratch.clone())
            .with_crop(0, 4); // block_size=4 → data 9 blocks, mask 4 blocks
        let mut args = std::collections::BTreeMap::new();
        args.insert("data".into(), json!({
            "__cube": {
                "bands": {
                    "B04": [data_path.to_str().unwrap()],
                    "SCL": [scl_path.to_str().unwrap()]
                },
                "bbox": [0.0, 0.0, 1.0, 1.0]
            }
        }));
        let r = exe.eval_mask_scl_dilation(args).await;
        assert!(
            r.is_ok(),
            "expected mask_scl_dilation to succeed after auto-resampling SCL to data grid, got {r:?}"
        );
        let env = r.unwrap();
        // Output cube must carry a masked B04 sized 10×10 (data grid), not 5×5 (SCL).
        let out_path: PathBuf = env["__cube"]["bands"]["B04"][0]
            .as_str()
            .map(PathBuf::from)
            .expect("masked B04 path");
        let out_ds = gdal::Dataset::open(&out_path).unwrap();
        assert_eq!(
            out_ds.raster_size(),
            (10, 10),
            "masked output must match data grid, not SCL grid"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
