//! `reduce_dimension` process + reducer enum / kernels.

use std::path::PathBuf;

use ndarray::Array3;
use orbit_geo::types::{BlockSize, Dimension, ImageResolution};
use orbit_geo::{LayerMapping, RasterDataBlock, RasterDataset, RasterDatasetBuilder};
use serde_json::{json, Value};

use crate::data_cube::DataCube;
use crate::executor::ExecError;

use super::sub_graph::{find_unique_result_node, require_subgraph, result_process_id};
use super::{GeoExecutor, SENTINEL_NDVI_NA};

/// openEO statistical reducers supported by [`GeoExecutor::eval_reduce_dimension`].
///
/// Each variant maps to a single openEO process whose `process_graph`
/// callback consumes `data: from_parameter("data")` (per openEO 1.3.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reducer {
    /// Arithmetic mean (NA-skipping).
    Mean,
    /// Minimum value.
    Min,
    /// Maximum value.
    Max,
    /// Sum.
    Sum,
    /// Median (50th percentile, no interpolation).
    Median,
    /// Count of non-NA values.
    Count,
    /// First non-NA value.
    First,
    /// Last non-NA value.
    Last,
    /// Sample standard deviation (n-1 divisor).
    Sd,
    /// Sample variance (n-1 divisor).
    Variance,
}

impl Reducer {
    /// Lower-case openEO process name (matches `process_id`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Reducer::Mean => "mean",
            Reducer::Min => "min",
            Reducer::Max => "max",
            Reducer::Sum => "sum",
            Reducer::Median => "median",
            Reducer::Count => "count",
            Reducer::First => "first",
            Reducer::Last => "last",
            Reducer::Sd => "sd",
            Reducer::Variance => "variance",
        }
    }

    pub(super) fn from_process_id(pid: &str) -> Option<Reducer> {
        Some(match pid {
            "mean" => Reducer::Mean,
            "min" => Reducer::Min,
            "max" => Reducer::Max,
            "sum" => Reducer::Sum,
            "median" => Reducer::Median,
            "count" => Reducer::Count,
            "first" => Reducer::First,
            "last" => Reducer::Last,
            "sd" | "stdev" => Reducer::Sd,
            "variance" | "var" => Reducer::Variance,
            _ => return None,
        })
    }
}

/// Apply the reducer to a non-empty stack of finite, non-NA values.
///
/// Caller must filter out NA / non-finite entries first. `stack` may be
/// mutated (`Median` sorts in place).
pub fn apply_reducer(stack: &mut [f32], reducer: Reducer) -> f32 {
    debug_assert!(!stack.is_empty());
    match reducer {
        Reducer::Mean => {
            let s: f32 = stack.iter().copied().sum();
            s / stack.len() as f32
        }
        Reducer::Min => stack.iter().copied().fold(f32::INFINITY, f32::min),
        Reducer::Max => stack.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        Reducer::Sum => stack.iter().copied().sum(),
        Reducer::Count => stack.len() as f32,
        Reducer::First => stack[0],
        Reducer::Last => stack[stack.len() - 1],
        Reducer::Median => {
            // Partial sort — IEEE total ordering avoids NaN issues (we
            // already filtered, but be safe).
            stack.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = stack.len();
            if n % 2 == 1 {
                stack[n / 2]
            } else {
                (stack[n / 2 - 1] + stack[n / 2]) / 2.0
            }
        }
        Reducer::Variance | Reducer::Sd => {
            let n = stack.len() as f32;
            if n < 2.0 {
                return 0.0;
            }
            let mean = stack.iter().copied().sum::<f32>() / n;
            let ss: f32 = stack.iter().copied().map(|v| (v - mean).powi(2)).sum();
            let var = ss / (n - 1.0);
            if reducer == Reducer::Sd { var.sqrt() } else { var }
        }
    }
}

/// A parsed reducer callback: either a single built-in statistical
/// reducer (the fast path — a tight numeric loop) or an arbitrary
/// compiled sub-graph (e.g. `subtract(max(data), min(data))` = range).
///
/// **#2 (2026-05-25)**: prior to this, only the 10 enum reducers were
/// accepted and any compound callback errored. Now non-enum callbacks
/// fall through to [`eval_reduce_subgraph`], reusing the per-pixel
/// mini-graph machinery that already powers `apply`.
#[derive(Clone, Debug)]
pub enum ReducerKind {
    /// A single openEO statistical reducer over `data: from_parameter("data")`.
    Builtin(Reducer),
    /// An arbitrary reducer expression — the raw `process_graph` node map.
    /// Evaluated per-pixel by [`eval_reduce_subgraph`].
    SubGraph(serde_json::Map<String, Value>),
}

/// Human-readable label for a [`ReducerKind`], used in `produced_by`.
fn reducer_kind_label(k: &ReducerKind) -> String {
    match k {
        ReducerKind::Builtin(r) => r.name().to_string(),
        ReducerKind::SubGraph(_) => "subgraph".to_string(),
    }
}

/// Parse an openEO `reducer` sub-process callback into a [`ReducerKind`].
///
/// Per spec, `reducer` is a Process object holding a `process_graph`.
/// Fast path: when the result node is a bare built-in reducer consuming
/// `data: from_parameter("data")`, return [`ReducerKind::Builtin`].
/// Otherwise return [`ReducerKind::SubGraph`] for general per-pixel eval.
///
/// **A4+A5 invariant**: extraction + result-node discovery delegates to
/// `sub_graph::{require_subgraph, find_unique_result_node, result_process_id}`
/// so the three eval sites share the same validation contract.
pub fn parse_reducer_subgraph(reducer: &Value) -> Result<ReducerKind, ExecError> {
    let pg = require_subgraph(reducer, "reduce_dimension.reducer")?;
    let (result_id, result_node) = find_unique_result_node(pg, "reduce_dimension.reducer")?;
    let pid = result_process_id(result_node, "reduce_dimension.reducer")?;
    // Fast path: the WHOLE callback is a single bare reducer node, i.e.
    // the result node is the only node and its `data` arg is
    // `from_parameter("data")`. Use the tight enum loop.
    if pg.len() == 1 {
        if let Some(r) = Reducer::from_process_id(pid) {
            return Ok(ReducerKind::Builtin(r));
        }
    }
    // General path: validate every node names a process we can evaluate
    // (array reducer OR scalar op), then keep the raw sub-graph.
    let _ = result_id;
    validate_reduce_subgraph_processes(pg)?;
    Ok(ReducerKind::SubGraph(pg.clone()))
}

/// Validate that every node in a reducer sub-graph names a process the
/// per-pixel evaluator supports (an array reducer or a scalar op), so a
/// bad graph fails at parse time, not silently per-pixel.
fn validate_reduce_subgraph_processes(
    pg: &serde_json::Map<String, Value>,
) -> Result<(), ExecError> {
    for (id, node) in pg {
        let pid = node.get("process_id").and_then(|v| v.as_str()).ok_or_else(|| {
            ExecError::InvalidGraph(format!("reduce_dimension.reducer: node `{id}` has no process_id"))
        })?;
        let is_array_reducer = Reducer::from_process_id(pid).is_some();
        let is_scalar_op = matches!(
            pid,
            "add" | "subtract" | "multiply" | "divide" | "power" | "absolute"
                | "clip" | "min" | "max" | "sqrt" | "exp" | "ln"
        );
        if !is_array_reducer && !is_scalar_op {
            return Err(ExecError::InvalidGraph(format!(
                "reduce_dimension.reducer: unsupported process `{pid}` in reducer callback \
                 (supported: statistical reducers + add/subtract/multiply/divide/power/absolute/clip/min/max/sqrt/exp/ln)"
            )));
        }
    }
    Ok(())
}

/// Per-pixel evaluator for a compound reducer sub-graph. `stack` is the
/// vector of finite, NA-filtered values along the reduced dimension.
/// Each node yields either a `Scalar` or the `Array` (= `stack` via
/// `from_parameter("data")`); array reducers collapse Array→Scalar,
/// scalar ops combine Scalars. Returns the result node's scalar.
pub fn eval_reduce_subgraph(
    pg: &serde_json::Map<String, Value>,
    stack: &[f32],
) -> Result<f32, ExecError> {
    let (result_id, _) = find_unique_result_node(pg, "reduce_dimension.reducer")?;
    let mut memo: std::collections::HashMap<String, RedVal> = std::collections::HashMap::new();
    let mut in_progress: Vec<String> = Vec::new();
    match eval_reduce_node(result_id, pg, stack, &mut memo, &mut in_progress)? {
        RedVal::Scalar(s) => Ok(s),
        RedVal::Array(_) => Err(ExecError::PerPixelComputation(
            "reduce_dimension.reducer: result node returned an array, expected a scalar".into(),
        )),
    }
}

/// Intermediate value in a reducer sub-graph: scalar or the data array.
#[derive(Clone, Debug)]
enum RedVal {
    Scalar(f32),
    Array(Vec<f32>),
}

fn eval_reduce_node(
    id: &str,
    pg: &serde_json::Map<String, Value>,
    stack: &[f32],
    memo: &mut std::collections::HashMap<String, RedVal>,
    in_progress: &mut Vec<String>,
) -> Result<RedVal, ExecError> {
    if let Some(v) = memo.get(id) {
        return Ok(v.clone());
    }
    if in_progress.iter().any(|n| n == id) {
        return Err(ExecError::InvalidGraph(format!(
            "reduce_dimension.reducer: cycle at node `{id}`"
        )));
    }
    in_progress.push(id.to_string());
    let node = pg.get(id).ok_or_else(|| {
        ExecError::InvalidGraph(format!("reduce_dimension.reducer: unknown node `{id}`"))
    })?;
    let pid = node.get("process_id").and_then(|v| v.as_str()).ok_or_else(|| {
        ExecError::InvalidGraph(format!("reduce_dimension.reducer: node `{id}` has no process_id"))
    })?;
    let args = node.get("arguments").and_then(|v| v.as_object());

    // Resolve one argument to a RedVal: literal number, from_parameter("data")
    // → Array, from_node → recurse.
    let resolve = |key: &str,
                   memo: &mut std::collections::HashMap<String, RedVal>,
                   in_progress: &mut Vec<String>|
     -> Result<RedVal, ExecError> {
        let a = args.and_then(|m| m.get(key)).ok_or_else(|| {
            ExecError::InvalidGraph(format!("reduce_dimension.reducer: node `{id}` missing arg `{key}`"))
        })?;
        if let Some(n) = a.as_f64() {
            return Ok(RedVal::Scalar(n as f32));
        }
        if let Some(obj) = a.as_object() {
            if let Some(Value::String(p)) = obj.get("from_parameter") {
                if p == "data" {
                    return Ok(RedVal::Array(stack.to_vec()));
                }
                return Err(ExecError::InvalidGraph(format!(
                    "reduce_dimension.reducer: only `data` parameter is bound (got `{p}`)"
                )));
            }
            if let Some(Value::String(t)) = obj.get("from_node") {
                return eval_reduce_node(t, pg, stack, memo, in_progress);
            }
        }
        Err(ExecError::InvalidGraph(format!(
            "reduce_dimension.reducer: node `{id}` arg `{key}` has unsupported shape"
        )))
    };

    let scalar = |rv: RedVal, who: &str| -> Result<f32, ExecError> {
        match rv {
            RedVal::Scalar(s) => Ok(s),
            RedVal::Array(_) => Err(ExecError::PerPixelComputation(format!(
                "reduce_dimension.reducer: `{who}` expected scalar, got array"
            ))),
        }
    };

    let out: RedVal = if let Some(reducer) = Reducer::from_process_id(pid) {
        // Array reducer: collapse the `data` array → scalar.
        let mut arr = match resolve("data", memo, in_progress)? {
            RedVal::Array(v) => v,
            RedVal::Scalar(s) => vec![s], // single value reduces to itself
        };
        if arr.is_empty() {
            RedVal::Scalar(SENTINEL_NDVI_NA)
        } else {
            RedVal::Scalar(apply_reducer(&mut arr, reducer))
        }
    } else {
        // Scalar op.
        match pid {
            "add" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")? + scalar(resolve("y", memo, in_progress)?, "y")?),
            "subtract" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")? - scalar(resolve("y", memo, in_progress)?, "y")?),
            "multiply" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")? * scalar(resolve("y", memo, in_progress)?, "y")?),
            "divide" => {
                let y = scalar(resolve("y", memo, in_progress)?, "y")?;
                if y == 0.0 {
                    return Err(ExecError::PerPixelComputation("reduce_dimension.reducer: divide by zero".into()));
                }
                RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")? / y)
            }
            "power" => RedVal::Scalar(scalar(resolve("base", memo, in_progress)?, "base")?.powf(scalar(resolve("p", memo, in_progress)?, "p")?)),
            "absolute" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")?.abs()),
            "sqrt" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")?.sqrt()),
            "exp" => RedVal::Scalar(scalar(resolve("p", memo, in_progress)?, "p")?.exp()),
            "ln" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")?.ln()),
            "min" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")?.min(scalar(resolve("y", memo, in_progress)?, "y")?)),
            "max" => RedVal::Scalar(scalar(resolve("x", memo, in_progress)?, "x")?.max(scalar(resolve("y", memo, in_progress)?, "y")?)),
            "clip" => {
                let x = scalar(resolve("x", memo, in_progress)?, "x")?;
                let lo = scalar(resolve("min", memo, in_progress)?, "min")?;
                let hi = scalar(resolve("max", memo, in_progress)?, "max")?;
                RedVal::Scalar(x.clamp(lo, hi))
            }
            other => return Err(ExecError::InvalidGraph(format!(
                "reduce_dimension.reducer: unsupported process `{other}`"
            ))),
        }
    };
    in_progress.pop();
    memo.insert(id.to_string(), out.clone());
    Ok(out)
}

impl GeoExecutor {
    /// openEO `reduce_dimension(data, reducer, dimension, context)`.
    ///
    /// Opens `data.__cube.ndvi_paths` (T scenes, single layer each) as a
    /// (T, 1, R, C) `RasterDataset<f32>`, parses the `reducer.process_graph`
    /// sub-callback to a [`Reducer`] enum, then calls
    /// `apply_reduction::<f32,_>(worker, Dimension::Time, n_threads, &out, NA)`
    /// where `worker` dispatches per-pixel through the chosen reducer.
    ///
    /// Currently only `dimension="t"` / `"time"` is supported (other axes
    /// surface `InvalidGraph`). Supported reducers: `mean`, `min`, `max`,
    /// `median`, `sum`, `count`, `first`, `last`, `sd`, `variance` — the
    /// canonical openEO 1.3.0 statistical reducers that consume the
    /// `data` parameter.
    pub(super) async fn eval_reduce_dimension(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let dim = args
            .get("dimension")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ExecError::InvalidGraph(
                "reduce_dimension: missing `dimension` (must be a string)".into(),
            ))?
            .to_string();
        // BUG-004 fix (2026-05-24): bands-axis reduction now supported.
        // Dispatch separately from the temporal-axis path because the
        // dataset shape and worker dimension differ (Time vs Layer).
        if matches!(dim.as_str(), "bands" | "band") {
            return self.eval_reduce_dimension_bands(args).await;
        }
        if !matches!(dim.as_str(), "t" | "time" | "temporal") {
            return Err(ExecError::InvalidGraph(format!(
                "reduce_dimension: dimension `{dim}` not supported (only `t`/`time`/`temporal` and `bands`/`band`)"
            )));
        }
        let reducer_val = args
            .get("reducer")
            .ok_or_else(|| ExecError::InvalidGraph(
                "reduce_dimension: missing `reducer` sub-process callback".into(),
            ))?;
        let reducer = parse_reducer_subgraph(reducer_val)?;

        let data = args
            .remove("data")
            .ok_or_else(|| ExecError::InvalidGraph("reduce_dimension: missing `data`".into()))?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "reduce_dimension: input is not a downloaded cube: {e}"
            ))
        })?;
        // Band-flexible (BUG-002 fix, 2026-05-24): pick whichever band the
        // caller intends to reduce. Preference order:
        //   1. A canonical f32 index band (ndvi/ndmi/...) if present —
        //      preserves the legacy NDVI-mean-time fast-path.
        //   2. ANY non-SCL band (was: only when cube had exactly 1 band).
        //      Allows custom `target_band` names from prior `ndvi` calls
        //      and cubes with multiple non-SCL bands.
        //   3. Error if no usable band — e.g. cube is empty or only SCL.
        const F32_INDEX_BANDS: &[&str] = &["ndvi", "ndmi", "ndwi", "evi", "savi", "msavi"];
        let index_key: String = F32_INDEX_BANDS
            .iter()
            .find(|k| cube.bands.contains_key(**k))
            .map(|s| s.to_string())
            .or_else(|| {
                cube.bands
                    .keys()
                    .find(|k| k.as_str() != "SCL")
                    .cloned()
            })
            .ok_or_else(|| ExecError::InvalidGraph(
                "reduce_dimension: __cube.bands has no usable band \
                 (cube is empty or only contains SCL)".into(),
            ))?;
        let paths: Vec<PathBuf> = cube.take_band(&index_key).map_err(|_| {
            ExecError::InvalidGraph(format!(
                "reduce_dimension: band `{index_key}` vanished from cube"
            ))
        })?;
        if paths.is_empty() {
            return Err(ExecError::Backend("reduce_dimension: empty input cube".into()));
        }

        // Build a (T, 1, R, C) f32 dataset from the per-scene NDVI files.
        let mut rds: RasterDataset<f32> = RasterDatasetBuilder::<f32>::from_files(&paths)
            .map_err(|e| ExecError::Backend(format!("reduce_dimension: builder: {e}")))?
            .resolution(ImageResolution { x: 10.0, y: -10.0 })
            .block_size(BlockSize {
                rows: self.crop_size as usize,
                cols: self.crop_size as usize,
            })
            .build()
            .map_err(|e| ExecError::Backend(format!("reduce_dimension: build: {e}")))?;
        rds.metadata.shape.times = paths.len();
        rds.metadata.shape.layers = 1;
        rds.layer_mappings = paths
            .iter()
            .enumerate()
            .map(|(t, p)| LayerMapping { source: p.clone(), time_pos: t, layer_pos: 0, band: 1 })
            .collect();

        let out_path = self.scratch_dir.join(format!(
            "reduce_{}.tif",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Dispatched worker — Builtin enum uses the tight numeric loop;
        // SubGraph evaluates the compound reducer expression per pixel
        // (#2, 2026-05-25). The SubGraph map is cloned into the closure
        // (Send + cheap structural clone).
        let red = reducer.clone();
        let reducer_label = reducer_kind_label(&reducer);
        let worker = move |rdb: &RasterDataBlock<f32>, _dim: Dimension| -> Array3<f32> {
            let t_dim = rdb.times();
            let r = rdb.rows();
            let c = rdb.cols();
            let mut out = Array3::<f32>::from_elem((1, r, c), SENTINEL_NDVI_NA);
            // Stack scratch — reused across pixels.
            let mut stack: Vec<f32> = Vec::with_capacity(t_dim);
            for row in 0..r {
                for col in 0..c {
                    stack.clear();
                    for t in 0..t_dim {
                        let v = rdb.data[[t, 0, row, col]];
                        if v.is_finite() && v != SENTINEL_NDVI_NA {
                            stack.push(v);
                        }
                    }
                    if !stack.is_empty() {
                        out[[0, row, col]] = match &red {
                            ReducerKind::Builtin(r) => apply_reducer(&mut stack, *r),
                            ReducerKind::SubGraph(pg) => {
                                eval_reduce_subgraph(pg, &stack).unwrap_or(SENTINEL_NDVI_NA)
                            }
                        };
                    }
                }
            }
            out
        };

        rds.apply_reduction::<f32, _>(worker, Dimension::Time, n_threads, &out_path, SENTINEL_NDVI_NA)
            .map_err(|e| ExecError::Backend(format!("reduce_dimension: apply_reduction: {e}")))?;

        // **2026-05-24 (merge_cubes band-axis follow-on)**: return __cube
        // preserving the input band name as the single-band entry — was
        // previously __raster (which lost the band identity), so two
        // independent reductions of cubes with different bands (ndvi vs
        // gndvi) couldn't be merged along the bands axis by merge_cubes.
        // The band-name preservation is what makes merge_cubes Case 1
        // (band-axis join, disjoint bands) trigger correctly downstream.
        // save_result, apply, and merge_cubes all already accept __cube.
        Ok(json!({
            "__cube": {
                "bands": { index_key: [ out_path ] },
                "produced_by": format!("reduce_dimension({reducer_label})"),
                "scene_count": 1
            }
        }))
    }

    /// **BUG-004 fix (2026-05-24)**: reduce_dimension over the `bands`
    /// axis — collapses a multi-band cube to a single-band cube by
    /// applying the reducer per-pixel across all non-SCL bands.
    ///
    /// Required cube shape: every non-SCL band must have the same scene
    /// count and (post-validation) the same spatial grid. SCL is
    /// forwarded unchanged (categorical mask data isn't reducible
    /// numerically).
    ///
    /// Output: `__cube{result: [scene_0_reduced, scene_1_reduced, ...]}`.
    /// One file per scene, single band named `result`.
    pub(super) async fn eval_reduce_dimension_bands(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let reducer_val = args.get("reducer").ok_or_else(|| {
            ExecError::InvalidGraph(
                "reduce_dimension(bands): missing `reducer` sub-process callback".into(),
            )
        })?;
        let reducer = parse_reducer_subgraph(reducer_val)?;
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("reduce_dimension(bands): missing `data`".into())
        })?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "reduce_dimension(bands): input is not a downloaded cube: {e}"
            ))
        })?;

        // Collect non-SCL band keys + their scene paths.
        let band_keys: Vec<String> = cube
            .bands
            .keys()
            .filter(|k| k.as_str() != "SCL")
            .cloned()
            .collect();
        if band_keys.is_empty() {
            return Err(ExecError::InvalidGraph(
                "reduce_dimension(bands): cube has no non-SCL band to reduce".into(),
            ));
        }
        if band_keys.len() == 1 {
            // Degenerate case: single band → just rename to "result".
            let only_key = band_keys[0].clone();
            let paths = cube.take_band(&only_key).map_err(|_| {
                ExecError::Backend(format!("reduce(bands): band `{only_key}` vanished"))
            })?;
            return Ok(json!({
                "__cube": {
                    "bands": { "result": paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>() },
                    "produced_by": format!("reduce_dimension(bands, {}, single-band-passthrough)", reducer_kind_label(&reducer)),
                    "scene_count": paths.len()
                }
            }));
        }

        // Validate equal scene counts across all bands.
        let band_paths_vec: Vec<(String, Vec<PathBuf>)> = band_keys
            .iter()
            .map(|k| {
                let v = cube.take_band(k).map_err(|_| {
                    ExecError::Backend(format!("reduce(bands): band `{k}` vanished"))
                })?;
                Ok::<_, ExecError>((k.clone(), v))
            })
            .collect::<Result<_, _>>()?;
        let scene_count = band_paths_vec[0].1.len();
        for (k, v) in &band_paths_vec {
            if v.len() != scene_count {
                return Err(ExecError::InvalidGraph(format!(
                    "reduce_dimension(bands): band `{k}` has {} scenes, expected {scene_count}",
                    v.len()
                )));
            }
        }

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Per scene: stack all bands as a (T=1, layers=N_bands, R, C) f32
        // dataset, run apply_reduction over Dimension::Layer with the
        // chosen reducer, output a single-layer file per scene.
        let mut out_paths: Vec<PathBuf> = Vec::with_capacity(scene_count);
        for t in 0..scene_count {
            let per_band_scene_paths: Vec<PathBuf> = band_paths_vec
                .iter()
                .map(|(_, v)| v[t].clone())
                .collect();
            let mut rds: RasterDataset<f32> = RasterDatasetBuilder::<f32>::from_files(&per_band_scene_paths)
                .map_err(|e| ExecError::Backend(format!("reduce(bands): builder t={t}: {e}")))?
                .resolution(ImageResolution { x: 10.0, y: -10.0 })
                .block_size(BlockSize {
                    rows: self.crop_size as usize,
                    cols: self.crop_size as usize,
                })
                .build()
                .map_err(|e| ExecError::Backend(format!("reduce(bands): build t={t}: {e}")))?;
            rds.metadata.shape.times = 1;
            rds.metadata.shape.layers = band_paths_vec.len();
            rds.layer_mappings = band_paths_vec
                .iter()
                .enumerate()
                .map(|(layer_idx, (_, v))| LayerMapping {
                    source: v[t].clone(),
                    time_pos: 0,
                    layer_pos: layer_idx,
                    band: 1,
                })
                .collect();

            let out_path = self.scratch_dir.join(format!(
                "reduce_bands_t{t}_{}.tif",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            // Per-pixel reducer over the layers axis — mirrors the time
            // worker (above) but with layers as the stacked dimension.
            // Builtin enum or compound SubGraph (#2, 2026-05-25).
            let red = reducer.clone();
            let worker = move |rdb: &RasterDataBlock<f32>, _dim: Dimension| -> Array3<f32> {
                let n_layers = rdb.layers();
                let r = rdb.rows();
                let c = rdb.cols();
                let mut out = Array3::<f32>::from_elem((1, r, c), SENTINEL_NDVI_NA);
                let mut stack: Vec<f32> = Vec::with_capacity(n_layers);
                for row in 0..r {
                    for col in 0..c {
                        stack.clear();
                        for l in 0..n_layers {
                            let v = rdb.data[[0, l, row, col]];
                            if v.is_finite() && v != SENTINEL_NDVI_NA {
                                stack.push(v);
                            }
                        }
                        if !stack.is_empty() {
                            out[[0, row, col]] = match &red {
                                ReducerKind::Builtin(r) => apply_reducer(&mut stack, *r),
                                ReducerKind::SubGraph(pg) => {
                                    eval_reduce_subgraph(pg, &stack).unwrap_or(SENTINEL_NDVI_NA)
                                }
                            };
                        }
                    }
                }
                out
            };
            rds.apply_reduction::<f32, _>(
                worker,
                Dimension::Layer,
                n_threads,
                &out_path,
                SENTINEL_NDVI_NA,
            )
            .map_err(|e| ExecError::Backend(format!("reduce(bands): apply_reduction t={t}: {e}")))?;
            out_paths.push(out_path);
        }

        Ok(json!({
            "__cube": {
                "bands": { "result": out_paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>() },
                "produced_by": format!("reduce_dimension(bands, {})", reducer_kind_label(&reducer)),
                "scene_count": scene_count
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_from_process_id_recognises_canonical_openeo_reducers() {
        assert_eq!(Reducer::from_process_id("mean"),     Some(Reducer::Mean));
        assert_eq!(Reducer::from_process_id("min"),      Some(Reducer::Min));
        assert_eq!(Reducer::from_process_id("max"),      Some(Reducer::Max));
        assert_eq!(Reducer::from_process_id("sum"),      Some(Reducer::Sum));
        assert_eq!(Reducer::from_process_id("median"),   Some(Reducer::Median));
        assert_eq!(Reducer::from_process_id("count"),    Some(Reducer::Count));
        assert_eq!(Reducer::from_process_id("first"),    Some(Reducer::First));
        assert_eq!(Reducer::from_process_id("last"),     Some(Reducer::Last));
        assert_eq!(Reducer::from_process_id("sd"),       Some(Reducer::Sd));
        assert_eq!(Reducer::from_process_id("variance"), Some(Reducer::Variance));
        assert_eq!(Reducer::from_process_id("nonsense"), None);
    }

    #[test]
    fn apply_reducer_mean_of_three_is_arithmetic_mean() {
        let mut s = [1.0_f32, 2.0, 3.0];
        assert!((apply_reducer(&mut s, Reducer::Mean) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn apply_reducer_median_handles_even_and_odd_lengths() {
        let mut odd = [3.0_f32, 1.0, 2.0];
        assert!((apply_reducer(&mut odd, Reducer::Median) - 2.0).abs() < 1e-6);
        let mut even = [1.0_f32, 2.0, 3.0, 4.0];
        assert!((apply_reducer(&mut even, Reducer::Median) - 2.5).abs() < 1e-6);
    }

    #[test]
    fn apply_reducer_min_max_sum_count_first_last() {
        let s = [4.0_f32, 1.0, 3.0, 2.0];
        assert!((apply_reducer(&mut s.clone(), Reducer::Min) - 1.0).abs() < 1e-6);
        assert!((apply_reducer(&mut s.clone(), Reducer::Max) - 4.0).abs() < 1e-6);
        assert!((apply_reducer(&mut s.clone(), Reducer::Sum) - 10.0).abs() < 1e-6);
        assert!((apply_reducer(&mut s.clone(), Reducer::Count) - 4.0).abs() < 1e-6);
        assert!((apply_reducer(&mut s.clone(), Reducer::First) - 4.0).abs() < 1e-6);
        assert!((apply_reducer(&mut s.clone(), Reducer::Last) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn apply_reducer_sample_variance_and_sd() {
        let mut s = [2.0_f32, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        // n=8, mean=5, sample variance = 32/7 ≈ 4.571
        let var = apply_reducer(&mut s.clone(), Reducer::Variance);
        assert!((var - 32.0_f32 / 7.0).abs() < 1e-4, "got {var}");
        let sd = apply_reducer(&mut s, Reducer::Sd);
        assert!((sd - (32.0_f32 / 7.0).sqrt()).abs() < 1e-4, "got {sd}");
    }

    #[test]
    fn parse_reducer_subgraph_mean_returns_mean() {
        let red = serde_json::json!({
            "process_graph": {
                "m": {
                    "process_id": "mean",
                    "arguments": { "data": {"from_parameter": "data"} },
                    "result": true
                }
            }
        });
        assert!(matches!(parse_reducer_subgraph(&red).unwrap(), ReducerKind::Builtin(Reducer::Mean)));
    }

    #[test]
    fn parse_reducer_subgraph_compound_returns_subgraph() {
        // range = subtract(max(data), min(data)) — a compound reducer the
        // enum can't express. Must parse as ReducerKind::SubGraph.
        let red = serde_json::json!({
            "process_graph": {
                "mx": {"process_id": "max", "arguments": {"data": {"from_parameter": "data"}}},
                "mn": {"process_id": "min", "arguments": {"data": {"from_parameter": "data"}}},
                "rng": {"process_id": "subtract", "arguments": {"x": {"from_node": "mx"}, "y": {"from_node": "mn"}}, "result": true}
            }
        });
        assert!(matches!(parse_reducer_subgraph(&red).unwrap(), ReducerKind::SubGraph(_)));
    }

    #[test]
    fn eval_reduce_subgraph_computes_range() {
        // subtract(max(data), min(data)) over [1,5,3] = 5 - 1 = 4.
        let red = serde_json::json!({
            "process_graph": {
                "mx": {"process_id": "max", "arguments": {"data": {"from_parameter": "data"}}},
                "mn": {"process_id": "min", "arguments": {"data": {"from_parameter": "data"}}},
                "rng": {"process_id": "subtract", "arguments": {"x": {"from_node": "mx"}, "y": {"from_node": "mn"}}, "result": true}
            }
        });
        let pg = super::super::sub_graph::require_subgraph(&red, "test").unwrap();
        let out = eval_reduce_subgraph(pg, &[1.0, 5.0, 3.0]).unwrap();
        assert_eq!(out, 4.0);
    }

    #[test]
    fn parse_reducer_subgraph_rejects_missing_result_node() {
        let red = serde_json::json!({
            "process_graph": {
                "m": {
                    "process_id": "mean",
                    "arguments": { "data": {"from_parameter": "data"} }
                    // no result: true
                }
            }
        });
        assert!(matches!(parse_reducer_subgraph(&red), Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn parse_reducer_subgraph_rejects_unsupported_process() {
        // #2 (2026-05-25): scalar ops like `ln` are now VALID inside a
        // compound reducer (e.g. ln(max(data))), so the rejection example
        // must be a genuinely-unknown process. `frobnicate` is neither an
        // array reducer nor a recognized scalar op → parse-time error.
        let red = serde_json::json!({
            "process_graph": {
                "m": {
                    "process_id": "frobnicate",
                    "arguments": { "data": {"from_parameter": "data"} },
                    "result": true
                }
            }
        });
        assert!(matches!(parse_reducer_subgraph(&red), Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn parse_reducer_subgraph_rejects_missing_process_graph_key() {
        let red = serde_json::json!({ "other": "stuff" });
        assert!(matches!(parse_reducer_subgraph(&red), Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reduce_dimension_rejects_unsupported_dimension() {
        // BUG-004 fix (2026-05-24): "bands" is now supported. Test now
        // asserts an UNSUPPORTED dimension (e.g. "x" / "y" / "z" /
        // garbage) still errors with InvalidGraph.
        let exe = GeoExecutor::new();
        let mut args = std::collections::BTreeMap::new();
        args.insert("dimension".into(), Value::String("x".into()));
        args.insert("data".into(), serde_json::json!({"__cube": {"bands": {"ndvi": []}}}));
        args.insert("reducer".into(), serde_json::json!({
            "process_graph": {"m": {"process_id": "mean", "arguments": {"data": {"from_parameter": "data"}}, "result": true}}
        }));
        let r = exe.eval_reduce_dimension(args).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
                "dimension `x` (spatial) is not supported");
    }
}
