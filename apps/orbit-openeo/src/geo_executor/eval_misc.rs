//! Miscellaneous process arms — merge_cubes, aggregate_spatial_*,
//! zonal_histogram, resample_spatial, fit_classifier, predict_classifier.

use std::path::PathBuf;

use ndarray::Array3;
use orbit_geo::types::{BlockSize, Dimension, ImageResolution};
use orbit_geo::{LayerMapping, RasterDataBlock, RasterDataset, RasterDatasetBuilder};
use serde_json::{json, Value};

use crate::executor::ExecError;

use crate::data_cube::DataCube;

use super::eval_reduce::{apply_reducer, eval_reduce_subgraph, parse_reducer_subgraph, ReducerKind};
use super::{extract_raster_path, json_to_array1_u8, json_to_array2, GeoExecutor, SENTINEL_NDVI_NA};

impl GeoExecutor {
    /// openEO `merge_cubes` — joins two raster cubes per openEO 1.3.0 spec.
    /// Inputs may be `{cube1, cube2}` or `{data1, data2}` arg names.
    ///
    /// **2026-05-24 rewrite (BUG-003 follow-on)**: now implements the
    /// openEO spec's Case 1 (band-axis join) when both inputs are `__cube`
    /// envelopes with DISJOINT band names — produces a multi-band `__cube`
    /// preserving both inputs' bands. Falls back to the legacy spatial
    /// mosaic (`gdalbuildvrt + gdal_translate` → `__raster`) for `__raster`
    /// inputs or single-file mosaic cases.
    ///
    /// Spec cases (1.3.0):
    /// - Case 1 (disjoint bands) → band-axis join, no overlap_resolver needed
    /// - Case 2 (overlapping bands + overlap_resolver) → per-pixel resolve
    ///   (2026-05-25: implemented — resolver reuses the reducer machinery
    ///   over a 2-element `[cube1, cube2]` stack)
    /// - Case 3 (disjoint spatial / __raster) → spatial mosaic (legacy)
    pub(super) fn eval_merge_cubes(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let cube1_val = args.get("cube1").or_else(|| args.get("data1"))
            .ok_or_else(|| ExecError::InvalidGraph("merge_cubes: missing `cube1`".into()))?;
        let cube2_val = args.get("cube2").or_else(|| args.get("data2"))
            .ok_or_else(|| ExecError::InvalidGraph("merge_cubes: missing `cube2`".into()))?;

        // Cases 1 & 2 both require two __cube envelopes with band maps.
        if let (Some(c1_inner), Some(c2_inner)) = (cube1_val.get("__cube"), cube2_val.get("__cube")) {
            if let (Some(b1), Some(b2)) = (
                c1_inner.get("bands").and_then(|b| b.as_object()),
                c2_inner.get("bands").and_then(|b| b.as_object()),
            ) {
                let overlap = b1.keys().any(|k| b2.contains_key(k));

                // **Case 1: band-axis join** (DISJOINT bands) — union the
                // two band maps. openEO "merge along non-overlapping bands".
                if !overlap {
                    let mut joined = serde_json::Map::new();
                    for (k, v) in b1.iter().chain(b2.iter()) {
                        joined.insert(k.clone(), v.clone());
                    }
                    let mut out = serde_json::Map::new();
                    out.insert("bands".into(), Value::Object(joined));
                    for key in ["bbox", "collection", "scene_count"] {
                        if let Some(v) = c1_inner.get(key) {
                            out.insert(key.into(), v.clone());
                        }
                    }
                    return Ok(json!({ "__cube": Value::Object(out) }));
                }

                // **Case 2: overlap_resolver** (OVERLAPPING bands). Per
                // openEO, the resolver callback has reducer signature —
                // it receives `data` = the array of overlapping values
                // (here `[cube1_px, cube2_px]`). Reuse the reducer machinery
                // (#2) over a 2-element layer stack per (band, scene).
                if let Some(resolver_val) = args.get("overlap_resolver") {
                    return self.merge_cubes_resolve_overlap(b1, b2, c1_inner, resolver_val);
                }
                // Overlapping bands but NO resolver → spec says this is an
                // error; fall through to spatial mosaic (legacy lenient).
                tracing::warn!(
                    "merge_cubes: overlapping bands without overlap_resolver — \
                     falling back to spatial mosaic (spec would reject)"
                );
            }
        }

        // **Case 3 (legacy): spatial mosaic** for __raster inputs or
        // mixed/overlapping __cube inputs. Falls back to gdalbuildvrt +
        // gdal_translate per the pre-2026-05-24 behavior.
        let p1 = extract_raster_path(Some(cube1_val), "cube1")?;
        let p2 = extract_raster_path(Some(cube2_val), "cube2")?;
        let dst = self.scratch_dir.join(format!(
            "mosaic_{}.tif",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let out = orbit_geo::gdal_utils::mosaic(&[p1, p2], &dst)
            .map_err(|e| ExecError::Backend(format!("merge_cubes: {e}")))?;
        Ok(json!({
            "__raster": {
                "path": out,
                "media_type": "image/tiff",
                "produced_by": "merge_cubes",
            }
        }))
    }

    /// **Case 2 (2026-05-25)**: resolve OVERLAPPING bands between two cubes
    /// via the `overlap_resolver` callback. For each shared band + scene,
    /// stack `[cube1_raster, cube2_raster]` as two layers and reduce them
    /// per-pixel with the resolver (reducer signature: `data=[x,y]`).
    /// Bands present in only one cube pass through unchanged (union).
    fn merge_cubes_resolve_overlap(
        &self,
        b1: &serde_json::Map<String, Value>,
        b2: &serde_json::Map<String, Value>,
        c1_inner: &Value,
        resolver_val: &Value,
    ) -> Result<Value, ExecError> {
        let resolver = parse_reducer_subgraph(resolver_val)?;
        let n_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

        // Helper: parse a band's JSON path array into Vec<PathBuf>.
        let paths_of = |v: &Value| -> Vec<PathBuf> {
            v.as_array()
                .map(|a| a.iter().filter_map(|p| p.as_str().map(PathBuf::from)).collect())
                .unwrap_or_default()
        };

        let mut out_bands = serde_json::Map::new();
        // Iterate the union of band names; resolve overlaps, forward the rest.
        let all_keys: std::collections::BTreeSet<&String> =
            b1.keys().chain(b2.keys()).collect();
        for band in all_keys {
            match (b1.get(band), b2.get(band)) {
                (Some(v1), Some(v2)) => {
                    // Overlapping band → resolve per scene.
                    let p1 = paths_of(v1);
                    let p2 = paths_of(v2);
                    if p1.len() != p2.len() {
                        return Err(ExecError::InvalidGraph(format!(
                            "merge_cubes: band `{band}` scene-count mismatch ({} vs {})",
                            p1.len(), p2.len()
                        )));
                    }
                    let mut resolved: Vec<String> = Vec::with_capacity(p1.len());
                    for (t, (a, b_path)) in p1.iter().zip(p2.iter()).enumerate() {
                        let out_path = self.scratch_dir.join(format!(
                            "merge_resolve_{band}_t{t}_{}.tif",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos()).unwrap_or(0)
                        ));
                        // Two-layer f32 dataset: layer 0 = cube1, 1 = cube2.
                        let mut rds: RasterDataset<f32> =
                            RasterDatasetBuilder::<f32>::from_files(&[a.clone(), b_path.clone()])
                                .map_err(|e| ExecError::Backend(format!("merge_cubes resolve builder {band} t={t}: {e}")))?
                                .resolution(ImageResolution { x: 10.0, y: -10.0 })
                                .block_size(BlockSize { rows: self.crop_size as usize, cols: self.crop_size as usize })
                                .build()
                                .map_err(|e| ExecError::Backend(format!("merge_cubes resolve build {band} t={t}: {e}")))?;
                        rds.metadata.shape.times = 1;
                        rds.metadata.shape.layers = 2;
                        rds.layer_mappings = vec![
                            LayerMapping { source: a.clone(), time_pos: 0, layer_pos: 0, band: 1 },
                            LayerMapping { source: b_path.clone(), time_pos: 0, layer_pos: 1, band: 1 },
                        ];
                        let red = resolver.clone();
                        let worker = move |rdb: &RasterDataBlock<f32>, _dim: Dimension| -> Array3<f32> {
                            let r = rdb.rows();
                            let c = rdb.cols();
                            let mut out = Array3::<f32>::from_elem((1, r, c), SENTINEL_NDVI_NA);
                            let mut stack: Vec<f32> = Vec::with_capacity(2);
                            for row in 0..r {
                                for col in 0..c {
                                    stack.clear();
                                    for l in 0..rdb.layers() {
                                        let v = rdb.data[[0, l, row, col]];
                                        if v.is_finite() && v != SENTINEL_NDVI_NA {
                                            stack.push(v);
                                        }
                                    }
                                    if !stack.is_empty() {
                                        out[[0, row, col]] = match &red {
                                            ReducerKind::Builtin(rr) => apply_reducer(&mut stack, *rr),
                                            ReducerKind::SubGraph(pg) => {
                                                eval_reduce_subgraph(pg, &stack).unwrap_or(SENTINEL_NDVI_NA)
                                            }
                                        };
                                    }
                                }
                            }
                            out
                        };
                        rds.apply_reduction::<f32, _>(worker, Dimension::Layer, n_threads, &out_path, SENTINEL_NDVI_NA)
                            .map_err(|e| ExecError::Backend(format!("merge_cubes resolve {band} t={t}: {e}")))?;
                        resolved.push(out_path.to_string_lossy().into_owned());
                    }
                    out_bands.insert(band.clone(), Value::Array(resolved.into_iter().map(Value::String).collect()));
                }
                // Band only in one cube → forward unchanged (union).
                (Some(v), None) | (None, Some(v)) => {
                    out_bands.insert(band.clone(), v.clone());
                }
                (None, None) => unreachable!("band came from the union of b1/b2 keys"),
            }
        }

        let mut out = serde_json::Map::new();
        out.insert("bands".into(), Value::Object(out_bands));
        for key in ["bbox", "collection", "scene_count"] {
            if let Some(v) = c1_inner.get(key) {
                out.insert(key.into(), v.clone());
            }
        }
        Ok(json!({ "__cube": Value::Object(out) }))
    }

    /// orbit-extension `aggregate_spatial_point` — sample raster values
    /// at world-coordinate points via `orbit_geo::sampling::sample`.
    /// `data` is a __raster handle; `points` is an array of `[x, y]`.
    pub(super) fn eval_aggregate_spatial_point(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data_path = extract_raster_path(args.get("data"), "data")?;
        let points_arr = args
            .get("points")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ExecError::InvalidGraph(
                "aggregate_spatial_point: `points` must be an array".into(),
            ))?;
        let mut pts: Vec<(f64, f64)> = Vec::with_capacity(points_arr.len());
        for (i, p) in points_arr.iter().enumerate() {
            let pair = p.as_array().ok_or_else(|| ExecError::InvalidGraph(
                format!("points[{i}] must be [x,y]"),
            ))?;
            if pair.len() != 2 {
                return Err(ExecError::InvalidGraph(format!(
                    "points[{i}]: expected [x,y], got len={}", pair.len()
                )));
            }
            let x = pair[0].as_f64().ok_or_else(|| {
                ExecError::InvalidGraph(format!("points[{i}][0] not a number"))
            })?;
            let y = pair[1].as_f64().ok_or_else(|| {
                ExecError::InvalidGraph(format!("points[{i}][1] not a number"))
            })?;
            pts.push((x, y));
        }

        // Build a minimal RasterDataset around the single i16 raster.
        let mut rds: RasterDataset<i16> =
            RasterDatasetBuilder::<i16>::from_files(&[&data_path])
                .map_err(|e| ExecError::Backend(format!("agg_point: builder: {e}")))?
                .resolution(ImageResolution { x: 10.0, y: -10.0 })
                .block_size(BlockSize { rows: self.crop_size as usize, cols: self.crop_size as usize })
                .build()
                .map_err(|e| ExecError::Backend(format!("agg_point: build: {e}")))?;
        rds.metadata.shape.times = 1;
        rds.metadata.shape.layers = 1;
        rds.layer_mappings = vec![LayerMapping {
            source: data_path.clone(), time_pos: 0, layer_pos: 0, band: 1,
        }];

        let samples = orbit_geo::sampling::sample::<i16>(&rds, &pts);
        let samples_json: Vec<Value> = samples
            .into_iter()
            .map(|s| match s {
                Some(v) => Value::from(v as i64),
                None => Value::Null,
            })
            .collect();
        Ok(json!({
            "samples": samples_json,
            "count": pts.len(),
        }))
    }

    /// orbit-extension `aggregate_spatial_polygon` — per-polygon mean of
    /// raster values, via `eo_vector::{ScanlineRasterizer, SimpleZonalStats}`.
    ///
    /// Inputs:
    ///   - `data`: `__raster` handle pointing at a single-band i16 GeoTIFF
    ///   - `geometries`: JSON array of `{"coordinates": [[x,y], ...]}`
    ///     rings (GeoJSON-ish; coordinates are pixel-space for this v1)
    ///
    /// Output: `{"means": [f64, ...], "polygon_count": N}` indexed by
    /// the order of the input geometries.
    pub(super) fn eval_aggregate_spatial_polygon(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        use eo_vector::{PolygonRing, SimpleZonalStats, ZonalStats};
        let data_path = extract_raster_path(args.get("data"), "data")?;
        let geoms = args
            .get("geometries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ExecError::InvalidGraph(
                "aggregate_spatial_polygon: `geometries` must be an array".into(),
            ))?;
        if geoms.is_empty() {
            return Err(ExecError::InvalidGraph(
                "aggregate_spatial_polygon: at least one polygon required".into(),
            ));
        }
        // Decode each geometry into a PolygonRing.
        let mut rings: Vec<PolygonRing> = Vec::with_capacity(geoms.len());
        for (i, g) in geoms.iter().enumerate() {
            let coords = g
                .get("coordinates")
                .and_then(|c| c.as_array())
                .ok_or_else(|| ExecError::InvalidGraph(format!(
                    "geometries[{i}]: missing `coordinates` array"
                )))?;
            let mut verts: Vec<(f64, f64)> = Vec::with_capacity(coords.len());
            for (j, pair) in coords.iter().enumerate() {
                let p = pair.as_array().ok_or_else(|| ExecError::InvalidGraph(
                    format!("geometries[{i}].coordinates[{j}] must be [x,y]"),
                ))?;
                if p.len() != 2 {
                    return Err(ExecError::InvalidGraph(format!(
                        "geometries[{i}].coordinates[{j}]: expected [x,y], got len={}",
                        p.len()
                    )));
                }
                let x = p[0].as_f64().ok_or_else(|| {
                    ExecError::InvalidGraph(format!("geometries[{i}].coordinates[{j}][0] not a number"))
                })?;
                let y = p[1].as_f64().ok_or_else(|| {
                    ExecError::InvalidGraph(format!("geometries[{i}].coordinates[{j}][1] not a number"))
                })?;
                verts.push((x, y));
            }
            let ring = PolygonRing { vertices: verts };
            if !ring.is_valid() {
                return Err(ExecError::InvalidGraph(format!(
                    "geometries[{i}]: polygon needs ≥3 vertices"
                )));
            }
            rings.push(ring);
        }

        // Read the raster as a single i16 buffer via GDAL.
        let ds = gdal::Dataset::open(&data_path)
            .map_err(|e| ExecError::Backend(format!("open {}: {e}", data_path.display())))?;
        let band = ds.rasterband(1)
            .map_err(|e| ExecError::Backend(format!("rasterband: {e}")))?;
        let (cols, rows) = band.size();
        let buffer: gdal::raster::Buffer<i16> = band
            .read_as::<i16>((0, 0), (cols, rows), (cols, rows), None)
            .map_err(|e| ExecError::Backend(format!("read raster: {e}")))?;
        let raster: Vec<i16> = buffer.into_shape_and_vec().1;

        // Compute means.
        let means = SimpleZonalStats
            .mean::<i16>(&rings, &raster, rows, cols)
            .map_err(|e| ExecError::Backend(format!("zonal mean: {e}")))?;
        Ok(json!({
            "means": means,
            "polygon_count": rings.len(),
        }))
    }

    /// orbit-extension `fit_classifier` — train a binary logistic
    /// classifier via `orbit_geo::ml::fit_classifier`. Returns
    /// `{ "model": { "weights": [...], "bias": <f64> } }`.
    pub(super) fn eval_fit_classifier(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let x = json_to_array2(args.get("x"), "x")?;
        let y = json_to_array1_u8(args.get("y"), "y")?;
        let iterations = args.get("iterations").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
        let lr = args.get("lr").and_then(|v| v.as_f64()).unwrap_or(0.05);
        let model = orbit_geo::ml::fit_classifier(x.view(), y.view(), iterations, lr)
            .map_err(|e| ExecError::Backend(format!("fit_classifier: {e}")))?;
        Ok(json!({
            "model": {
                "weights": model.weights.to_vec(),
                "bias": model.bias,
            }
        }))
    }

    /// orbit-extension `predict_classifier` — apply a previously-fit
    /// `ClassifierModel` to feature matrix `x`. Returns
    /// `{ "predictions": [u8, ...] }`.
    pub(super) fn eval_predict_classifier(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let model_val = args
            .remove("model")
            .ok_or_else(|| ExecError::InvalidGraph("predict_classifier: missing `model`".into()))?;
        // Accept either { "model": { weights, bias } } or { weights, bias } directly.
        // Unwrap one level of `{ "model": ... }` nesting if present.
        let mut model_inner = match model_val {
            Value::Object(mut m) => {
                if let Some(inner) = m.remove("model") {
                    inner
                } else {
                    Value::Object(m)
                }
            }
            other => other,
        };
        let model_obj = model_inner.as_object_mut().ok_or_else(|| {
            ExecError::InvalidGraph("predict_classifier: model must be an object".into())
        })?;
        let weights_json = model_obj.remove("weights").ok_or_else(|| {
            ExecError::InvalidGraph("predict_classifier: model.weights missing".into())
        })?;
        let bias = model_obj
            .get("bias")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| ExecError::InvalidGraph("predict_classifier: model.bias must be a number".into()))?;
        let weights_vec: Vec<f64> = serde_json::from_value(weights_json)
            .map_err(|e| ExecError::InvalidGraph(format!("predict_classifier: weights: {e}")))?;
        let model = orbit_geo::ml::ClassifierModel {
            weights: ndarray::Array1::from(weights_vec),
            bias,
        };
        let x = json_to_array2(args.get("x"), "x")?;
        let preds = orbit_geo::ml::predict_classifier(&model, x.view());
        Ok(json!({ "predictions": preds.to_vec() }))
    }

    /// orbit-extension `zonal_histogram` — per-zone pixel counts via
    /// `orbit_geo::zonal_stats::zonal_histogram`. Inputs are two
    /// `__raster` handles (data + zone mask). Returns
    /// `{"histogram": {"<zone>": <count>, ...}, "total": N}`.
    pub(super) fn eval_zonal_histogram(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data_path = extract_raster_path(args.get("data"), "data")?;
        let mask_path = extract_raster_path(args.get("mask"), "mask")?;

        // Build minimal datasets around each path. Layer mapping: 1 band, 1 time.
        let mut data_rds: RasterDataset<i16> =
            RasterDatasetBuilder::<i16>::from_files(&[&data_path])
                .map_err(|e| ExecError::Backend(format!("zonal_histogram: data builder: {e}")))?
                .resolution(ImageResolution { x: 10.0, y: -10.0 })
                .block_size(BlockSize { rows: self.crop_size as usize, cols: self.crop_size as usize })
                .build()
                .map_err(|e| ExecError::Backend(format!("zonal_histogram: data build: {e}")))?;
        data_rds.metadata.shape.times = 1;
        data_rds.metadata.shape.layers = 1;
        data_rds.layer_mappings = vec![LayerMapping {
            source: data_path.clone(), time_pos: 0, layer_pos: 0, band: 1,
        }];
        let mut mask_rds: RasterDataset<u8> =
            RasterDatasetBuilder::<u8>::from_files(&[&mask_path])
                .map_err(|e| ExecError::Backend(format!("zonal_histogram: mask builder: {e}")))?
                .resolution(ImageResolution { x: 10.0, y: -10.0 })
                .block_size(BlockSize { rows: self.crop_size as usize, cols: self.crop_size as usize })
                .build()
                .map_err(|e| ExecError::Backend(format!("zonal_histogram: mask build: {e}")))?;
        mask_rds.metadata.shape.times = 1;
        mask_rds.metadata.shape.layers = 1;
        mask_rds.layer_mappings = vec![LayerMapping {
            source: mask_path.clone(), time_pos: 0, layer_pos: 0, band: 1,
        }];

        let hist = orbit_geo::zonal_stats::zonal_histogram::<i16, u8>(&data_rds, &mask_rds)
            .map_err(|e| ExecError::Backend(format!("zonal_histogram: {e}")))?;
        let mut by_key = serde_json::Map::new();
        let mut total: u64 = 0;
        // Sort keys for deterministic output (BTreeMap-ish ordering).
        let mut entries: Vec<(i16, u64)> = hist.into_iter().collect();
        entries.sort_by_key(|(k, _)| *k);
        for (k, v) in entries {
            by_key.insert(k.to_string(), Value::from(v));
            total += v;
        }
        Ok(json!({ "histogram": Value::Object(by_key), "total": total }))
    }

    /// openEO `resample_spatial` — reproject a cube to a target EPSG via
    /// `orbit_geo::gdal_utils::warp`. The output is a new GeoTIFF whose
    /// path is surfaced as a `__raster` handle. `save_result` can hand
    /// the bytes back directly; `ndvi`/other downstream processes accept
    /// a single-band raster handle as a degenerate one-scene cube.
    pub(super) fn eval_resample_spatial(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let target_epsg = args
            .get("projection")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ExecError::InvalidGraph(
                "resample_spatial: numeric `projection` (EPSG code) is required".into(),
            ))? as u32;
        let mut data = args
            .remove("data")
            .ok_or_else(|| ExecError::InvalidGraph("resample_spatial: missing `data`".into()))?;

        // Source path: either a __raster handle (single GeoTIFF) or the
        // first red_path of a __cube (we warp one band as a sentinel; a
        // full per-scene reproject lands when openEO `apply_dimension`
        // wires up batch warp). Take by value to avoid cloning the
        // band paths array.
        let src_path: PathBuf = if let Some(raster) = data
            .as_object_mut()
            .and_then(|m| m.remove("__raster"))
        {
            let path_val = raster
                .as_object()
                .and_then(|m| m.get("path"))
                .cloned()
                .ok_or_else(|| ExecError::InvalidGraph(
                    "resample_spatial: __raster missing `path`".into(),
                ))?;
            serde_json::from_value(path_val).map_err(|e| {
                ExecError::InvalidGraph(format!("resample_spatial: bad raster.path: {e}"))
            })?
        } else if data.get("__cube").is_some() {
            // Band-flexible: pick the first band's first scene from `bands`.
            let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
                ExecError::InvalidGraph(format!("resample_spatial: bad __cube: {e}"))
            })?;
            let (_first_band, paths) = std::mem::take(&mut cube.bands)
                .into_iter()
                .next()
                .ok_or_else(|| ExecError::Backend(
                    "resample_spatial: __cube.bands is empty".into(),
                ))?;
            paths.into_iter().next().ok_or_else(|| {
                ExecError::Backend("resample_spatial: empty band paths".into())
            })?
        } else {
            return Err(ExecError::InvalidGraph(
                "resample_spatial: `data` must be a __raster or __cube handle".into(),
            ));
        };
        let dst = self.scratch_dir.join(format!(
            "warp_epsg{target_epsg}_{}.tif",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let warped = orbit_geo::gdal_utils::warp(&src_path, &dst, target_epsg)
            .map_err(|e| ExecError::Backend(format!("resample_spatial: warp: {e}")))?;
        Ok(json!({
            "__raster": {
                "path": warped,
                "media_type": "image/tiff",
                "produced_by": "resample_spatial",
                "target_epsg": target_epsg,
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ProcessGraphExecutor;
    use std::path::Path;

    fn temp_root(tag: &str) -> std::path::PathBuf {
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
    fn graph(args: serde_json::Value) -> serde_json::Value {
        json!({ "process": { "process_graph": args } })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fit_then_predict_classifier_round_trip() {
        // Linearly separable 2-feature toy: y=0 below sum=10, y=1 above.
        let fit_body = graph(json!({
            "f": { "process_id": "fit_classifier",
                   "arguments": {
                       "x": [
                           [1.0,1.0],[1.0,2.0],[2.0,1.0],[2.0,2.0],
                           [8.0,8.0],[9.0,8.0],[8.0,9.0],[9.0,9.0]
                       ],
                       "y": [0,0,0,0,1,1,1,1],
                       "iterations": 500,
                       "lr": 0.1
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&fit_body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let weights = v["model"]["weights"].as_array().unwrap();
        assert_eq!(weights.len(), 2);
        // Predict on fresh points: (1,1)→0, (9,9)→1.
        let pred_body = graph(json!({
            "p": { "process_id": "predict_classifier",
                   "arguments": {
                       "model": v["model"].clone(),
                       "x": [[1.0,1.0], [9.0,9.0]]
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&pred_body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let preds: Vec<u64> = v["predictions"].as_array().unwrap()
            .iter().map(|x| x.as_u64().unwrap()).collect();
        assert_eq!(preds, vec![0, 1], "linearly-separable case must round-trip");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn predict_classifier_with_unwrapped_model_envelope_works() {
        // Caller may pass the model dict directly (not wrapped in {"model":…}).
        let body = graph(json!({
            "p": { "process_id": "predict_classifier",
                   "arguments": {
                       "model": {"weights": [1.0, 1.0], "bias": -10.0},
                       "x": [[1.0,1.0], [9.0,9.0]]
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        // (1+1−10)=−8 < 0 → 0; (9+9−10)=8 > 0 → 1.
        let preds: Vec<u64> = v["predictions"].as_array().unwrap()
            .iter().map(|x| x.as_u64().unwrap()).collect();
        assert_eq!(preds, vec![0, 1]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fit_classifier_missing_x_is_invalid_graph() {
        let body = graph(json!({
            "f": { "process_id": "fit_classifier",
                   "arguments": { "y": [0,1] },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn predict_classifier_missing_model_is_invalid_graph() {
        let body = graph(json!({
            "p": { "process_id": "predict_classifier",
                   "arguments": { "x": [[1.0]] },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merge_cubes_mosaics_two_geotiffs_via_gdal_utils() {
        let scratch = temp_root("merge");
        let a = scratch.join("a.tif");
        let b = scratch.join("b.tif");
        // Two overlapping 4×4 fixture rasters.
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        for (path, val) in [(&a, 11i16), (&b, 22i16)] {
            let mut ds = driver.create_with_band_type::<i16, _>(path, 4, 4, 1).unwrap();
            ds.set_geo_transform(&[12.0, 1.0, 0.0, 46.0, 0.0, -1.0]).unwrap();
            let srs = gdal::spatial_ref::SpatialRef::from_epsg(4326).unwrap();
            ds.set_spatial_ref(&srs).unwrap();
            let mut band = ds.rasterband(1).unwrap();
            let mut buf = gdal::raster::Buffer::new((4, 4), vec![val; 16]);
            band.write::<i16>((0, 0), (4, 4), &mut buf).unwrap();
        }
        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone());
        let body = graph(json!({
            "m": { "process_id": "merge_cubes",
                   "arguments": {
                       "cube1": { "__raster": { "path": a.to_str().unwrap() } },
                       "cube2": { "__raster": { "path": b.to_str().unwrap() } }
                   },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["__raster"]["produced_by"], "merge_cubes");
        let out_path = v["__raster"]["path"].as_str().unwrap();
        let bytes = std::fs::read(out_path).unwrap();
        assert!(bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// **BUG-003 follow-on (2026-05-24)**: when both inputs are `__cube`
    /// envelopes with DISJOINT band names, merge_cubes joins along the
    /// bands dimension and returns a `__cube` with the union of bands —
    /// matching openEO 1.3.0 Case 1 semantics.
    #[tokio::test(flavor = "current_thread")]
    async fn merge_cubes_band_axis_join_when_inputs_are_cubes_with_disjoint_bands() {
        let exe = GeoExecutor::new();
        let body = graph(json!({
            "m": { "process_id": "merge_cubes",
                   "arguments": {
                       "cube1": { "__cube": { "bands": { "ndvi":  ["/tmp/n0.tif", "/tmp/n1.tif"] } } },
                       "cube2": { "__cube": { "bands": { "gndvi": ["/tmp/g0.tif", "/tmp/g1.tif"] } } }
                   },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await.expect("merge_cubes band-axis join");
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        // Must be __cube (NOT __raster — that would be the legacy spatial mosaic).
        let cube = v.get("__cube").expect("output must be __cube envelope");
        let bands = cube.get("bands").and_then(|b| b.as_object()).expect("bands map");
        assert_eq!(bands.len(), 2, "union of disjoint bands must have 2 entries");
        assert!(bands.contains_key("ndvi"), "ndvi band preserved from cube1");
        assert!(bands.contains_key("gndvi"), "gndvi band preserved from cube2");
        // Time-series paths preserved per band.
        assert_eq!(bands["ndvi"].as_array().unwrap().len(), 2);
        assert_eq!(bands["gndvi"].as_array().unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn merge_cubes_overlap_resolver_sums_overlapping_band() {
        // Case 2 (2026-05-25): two cubes share band "ndvi". cube1=10,
        // cube2=32 everywhere. overlap_resolver=sum → output 42.
        let scratch = temp_root("merge-resolve");
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        let mk = |path: &std::path::Path, val: f32| {
            let mut ds = driver.create_with_band_type::<f32, _>(path, 4, 4, 1).unwrap();
            ds.set_geo_transform(&[300000.0, 10.0, 0.0, 5400000.0, 0.0, -10.0]).unwrap();
            let sr = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
            ds.set_spatial_ref(&sr).unwrap();
            let mut band = ds.rasterband(1).unwrap();
            let mut buf = gdal::raster::Buffer::new((4, 4), vec![val; 16]);
            band.write::<f32>((0, 0), (4, 4), &mut buf).unwrap();
        };
        let a = scratch.join("ndvi_c1.tif");
        let b = scratch.join("ndvi_c2.tif");
        mk(&a, 10.0);
        mk(&b, 32.0);
        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone());
        let body = graph(json!({
            "m": { "process_id": "merge_cubes",
                   "arguments": {
                       "cube1": { "__cube": { "bands": { "ndvi": [a.to_str().unwrap()] } } },
                       "cube2": { "__cube": { "bands": { "ndvi": [b.to_str().unwrap()] } } },
                       "overlap_resolver": { "process_graph": {
                           "s": { "process_id": "sum", "arguments": {"data": {"from_parameter": "data"}}, "result": true }
                       }}
                   },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await.expect("merge_cubes overlap resolve");
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        // Output is __cube (NOT __raster mosaic) with the resolved ndvi band.
        let out_path = v["__cube"]["bands"]["ndvi"][0].as_str().expect("resolved ndvi path");
        let ds = gdal::Dataset::open(out_path).unwrap();
        let band = ds.rasterband(1).unwrap();
        let buf: gdal::raster::Buffer<f32> = band.read_as((0,0),(4,4),(4,4),None).unwrap();
        // Every pixel = 10 + 32 = 42.
        assert!(buf.data().iter().all(|&p| (p - 42.0).abs() < 1e-3),
                "overlap_resolver=sum must yield 42 everywhere, got {:?}", &buf.data()[..4]);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn merge_cubes_missing_input_is_invalid_graph() {
        let body = graph(json!({
            "m": { "process_id": "merge_cubes",
                   "arguments": { "cube1": { "__raster": { "path": "/tmp/a.tif" } } },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aggregate_spatial_point_samples_values_via_sampling() {
        // 4×4 raster with rows of values 10,11,12,13. geo_transform
        // origin (0,0), pixel (1, -1) → world coord = pixel coord here.
        let scratch = temp_root("agg-point");
        let path = scratch.join("data.tif");
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = driver.create_with_band_type::<i16, _>(&path, 4, 4, 1).unwrap();
        ds.set_geo_transform(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]).unwrap();
        let mut band = ds.rasterband(1).unwrap();
        // Row-major data; row=0,1,2,3 each filled with 10,11,12,13.
        let data: Vec<i16> = (0..4).flat_map(|r| std::iter::repeat(10 + r as i16).take(4)).collect();
        let mut buf = gdal::raster::Buffer::new((4, 4), data);
        band.write::<i16>((0, 0), (4, 4), &mut buf).unwrap();
        drop(band); drop(ds);

        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone()).with_crop(0, 4);
        let body = graph(json!({
            "s": { "process_id": "aggregate_spatial_point",
                   "arguments": {
                       "data": { "__raster": { "path": path.to_str().unwrap() } },
                       // World y descends along rows. (0.5, -0.5) = row0, (0.5, -2.5) = row2.
                       "points": [[0.5, -0.5], [0.5, -2.5], [99.0, 99.0]]
                   },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["count"], 3);
        let s = v["samples"].as_array().unwrap();
        assert_eq!(s[0].as_i64().unwrap(), 10);
        assert_eq!(s[1].as_i64().unwrap(), 12);
        assert!(s[2].is_null(), "out-of-extent point must be null");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aggregate_spatial_point_bad_point_arity_is_invalid_graph() {
        let body = graph(json!({
            "s": { "process_id": "aggregate_spatial_point",
                   "arguments": {
                       "data": { "__raster": { "path": "/tmp/x.tif" } },
                       "points": [[1.0]]
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aggregate_spatial_polygon_computes_mean_per_polygon() {
        // 4×4 raster with values 1..=16 (row-major). One polygon covering
        // the top-left 2×2 should have mean = (1+2+5+6)/4 = 3.5.
        let scratch = temp_root("agg-poly");
        let path = scratch.join("data.tif");
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = driver.create_with_band_type::<i16, _>(&path, 4, 4, 1).unwrap();
        ds.set_geo_transform(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]).unwrap();
        let mut band = ds.rasterband(1).unwrap();
        let data: Vec<i16> = (1..=16i16).collect();
        let mut buf = gdal::raster::Buffer::new((4, 4), data);
        band.write::<i16>((0, 0), (4, 4), &mut buf).unwrap();
        drop(band); drop(ds);

        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone());
        let body = graph(json!({
            "a": { "process_id": "aggregate_spatial_polygon",
                   "arguments": {
                       "data": { "__raster": { "path": path.to_str().unwrap() } },
                       "geometries": [
                           // Polygon covers rows 0-1 / cols 0-1 in pixel space.
                           { "coordinates": [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [0.0, 0.0]] }
                       ]
                   },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["polygon_count"], 1);
        let means = v["means"].as_array().unwrap();
        let m = means[0].as_f64().unwrap();
        assert!((m - 3.5).abs() < 0.1, "expected ~3.5 got {m}");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aggregate_spatial_polygon_empty_geometries_is_invalid_graph() {
        let body = graph(json!({
            "a": { "process_id": "aggregate_spatial_polygon",
                   "arguments": {
                       "data": { "__raster": { "path": "/tmp/x.tif" } },
                       "geometries": []
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aggregate_spatial_polygon_bad_vertex_is_invalid_graph() {
        let body = graph(json!({
            "a": { "process_id": "aggregate_spatial_polygon",
                   "arguments": {
                       "data": { "__raster": { "path": "/tmp/x.tif" } },
                       "geometries": [{ "coordinates": [[0,0], [1,"bad"]] }]
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    fn write_constant_tiff_i16(path: &Path, value: i16, w: usize, h: usize) {
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = driver.create_with_band_type::<i16, _>(path, w, h, 1).unwrap();
        ds.set_geo_transform(&[12.0, 0.1, 0.0, 46.0, 0.0, -0.1]).unwrap();
        let mut band = ds.rasterband(1).unwrap();
        let mut buf = gdal::raster::Buffer::new((w, h), vec![value; w * h]);
        band.write::<i16>((0, 0), (w, h), &mut buf).unwrap();
    }

    fn write_split_mask_tiff_u8(path: &Path, w: usize, h: usize, zone_left: u8, zone_right: u8) {
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = driver.create_with_band_type::<u8, _>(path, w, h, 1).unwrap();
        ds.set_geo_transform(&[12.0, 0.1, 0.0, 46.0, 0.0, -0.1]).unwrap();
        let mut band = ds.rasterband(1).unwrap();
        let mut data = vec![0u8; w * h];
        for r in 0..h {
            for c in 0..w {
                data[r * w + c] = if c < w / 2 { zone_left } else { zone_right };
            }
        }
        let mut buf = gdal::raster::Buffer::new((w, h), data);
        band.write::<u8>((0, 0), (w, h), &mut buf).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zonal_histogram_counts_pixels_per_zone() {
        let scratch = temp_root("zonal");
        let data_path = scratch.join("data.tif");
        let mask_path = scratch.join("mask.tif");
        // 8×8 = 64 pixels. Data is constant=7. Mask splits 32 left (zone=1)
        // / 32 right (zone=2), with zero (background) excluded.
        write_constant_tiff_i16(&data_path, 7, 8, 8);
        write_split_mask_tiff_u8(&mask_path, 8, 8, 1, 2);

        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone()).with_crop(0, 8);
        let body = graph(json!({
            "z": { "process_id": "zonal_histogram",
                   "arguments": {
                       "data": { "__raster": { "path": data_path.to_str().unwrap() } },
                       "mask": { "__raster": { "path": mask_path.to_str().unwrap() } }
                   },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        // Every data pixel has value 7. zonal_histogram returns
        // HashMap<data_value, count>. With masks 1 & 2 both NON-ZERO,
        // both zones count → total 64 unmasked pixels.
        assert_eq!(v["histogram"]["7"].as_u64().unwrap(), 64);
        assert_eq!(v["total"].as_u64().unwrap(), 64);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zonal_histogram_missing_arg_is_invalid_graph() {
        let body = graph(json!({
            "z": { "process_id": "zonal_histogram",
                   "arguments": { "data": { "__raster": { "path": "/tmp/x" } } },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resample_spatial_missing_projection_is_invalid_graph() {
        let body = graph(json!({
            "r": { "process_id": "resample_spatial",
                   "arguments": { "data": { "__raster": { "path": "/tmp/x.tif" } } },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "got {r:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resample_spatial_non_handle_data_is_invalid_graph() {
        let body = graph(json!({
            "r": { "process_id": "resample_spatial",
                   "arguments": { "data": 42, "projection": 3857 },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "got {r:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resample_spatial_warps_real_geotiff_via_gdal_utils() {
        // Generate a small fixture EPSG:4326 GeoTIFF, then warp it to EPSG:3857.
        let scratch = temp_root("warp");
        let src = scratch.join("src.tif");
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = driver
            .create_with_band_type::<i16, _>(&src, 4, 4, 1).unwrap();
        ds.set_geo_transform(&[12.0, 0.1, 0.0, 46.0, 0.0, -0.1]).unwrap();
        let srs = gdal::spatial_ref::SpatialRef::from_epsg(4326).unwrap();
        ds.set_spatial_ref(&srs).unwrap();
        let mut band = ds.rasterband(1).unwrap();
        let mut buf = gdal::raster::Buffer::new((4, 4), vec![1i16; 16]);
        band.write::<i16>((0, 0), (4, 4), &mut buf).unwrap();
        drop(band); drop(ds);

        let exe = GeoExecutor::new().with_scratch_dir(scratch.clone());
        let body = graph(json!({
            "r": { "process_id": "resample_spatial",
                   "arguments": {
                       "data": { "__raster": { "path": src.to_str().unwrap() } },
                       "projection": 3857
                   },
                   "result": true }
        }));
        // Result node is resample_spatial (not save_result), so we expect JSON.
        let r = exe.run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "application/json");
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["__raster"]["produced_by"], "resample_spatial");
        assert_eq!(v["__raster"]["target_epsg"], 3857);
        let out_path = v["__raster"]["path"].as_str().unwrap();
        assert!(std::path::Path::new(out_path).exists(), "warp output missing");
        // The warp should produce a real TIFF.
        let bytes = std::fs::read(out_path).unwrap();
        assert!(bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"));
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
