//! `apply(data, process, context?)` — openEO 1.3.0 per-pixel sub-callback process.
//!
//! The `process` argument is a Process object whose `process_graph`
//! evaluates per pixel. The sub-graph receives the pixel value via
//! `from_parameter("x")` and yields a new per-pixel value.
//!
//! The sub-graph is a small DAG of pure-numeric processes (arithmetic,
//! comparison, boolean, basic statistics). See `apply_pure_numeric_op` for
//! the supported set.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use ndarray::Array3;
use orbit_geo::types::{BlockSize, Dimension, ImageResolution};
use orbit_geo::{LayerMapping, RasterDataBlock, RasterDataset, RasterDatasetBuilder};
use serde_json::{Map, Value};

use crate::data_cube::DataCube;
use crate::executor::ExecError;

use super::sub_graph::find_unique_result_node;
use super::{GeoExecutor, SENTINEL_NDVI_NA};

/// Evaluate the `apply.process.process_graph` sub-callback against a
/// single pixel value `x`.
///
/// Returns the result-node's f64 value.
///
/// **Note**: `sub_pg` is the BARE inner-node map (already unwrapped from
/// the `{"process_graph": {…}}` envelope by the caller). Extraction and
/// result-node discovery delegate to `sub_graph::find_unique_result_node`
/// so apply + reduce_dimension share the same validation contract.
pub fn eval_apply_subgraph(sub_pg: &Value, x: f64) -> Result<f64, ExecError> {
    let pg = sub_pg
        .as_object()
        .ok_or_else(|| ExecError::InvalidGraph(
            "apply: process_graph must be an object".into(),
        ))?;
    if pg.is_empty() {
        return Err(ExecError::InvalidGraph(
            "apply: process_graph is empty".into(),
        ));
    }
    // Shared result-node discovery (also enforces uniqueness).
    let (result_id, _) = find_unique_result_node(pg, "apply")?;

    // Memo: each node's computed f64.
    let mut memo: HashMap<String, f64> = HashMap::new();
    let mut in_progress: Vec<String> = Vec::new();
    let result_owned = result_id.to_string();
    eval_node_inline(&result_owned, pg, &mut memo, &mut in_progress, x)
}

/// A resolved argument value — either a scalar f64 or an array of f64s
/// (for `data: [a, b]` shapes used by arithmetic/comparison processes).
#[derive(Clone, Debug)]
enum ArgVal {
    Num(f64),
    Arr(Vec<f64>),
}

impl ArgVal {
    fn as_num(&self, who: &str) -> Result<f64, ExecError> {
        match self {
            ArgVal::Num(n) => Ok(*n),
            ArgVal::Arr(_) => Err(ExecError::InvalidGraph(format!(
                "apply: `{who}` expected scalar, got array"
            ))),
        }
    }
    fn as_pair(&self, who: &str) -> Result<(f64, f64), ExecError> {
        match self {
            ArgVal::Arr(v) if v.len() == 2 => Ok((v[0], v[1])),
            _ => Err(ExecError::InvalidGraph(format!(
                "apply: `{who}` expected 2-element array"
            ))),
        }
    }
}

fn resolve_arg_value(
    v: &Value,
    nodes: &Map<String, Value>,
    memo: &mut HashMap<String, f64>,
    in_progress: &mut Vec<String>,
    x: f64,
) -> Result<ArgVal, ExecError> {
    if let Some(n) = v.as_f64() {
        return Ok(ArgVal::Num(n));
    }
    if let Some(obj) = v.as_object() {
        // `from_parameter: "x"` → the per-pixel value.
        if let Some(Value::String(p)) = obj.get("from_parameter") {
            if p == "x" {
                return Ok(ArgVal::Num(x));
            }
            return Err(ExecError::InvalidGraph(format!(
                "apply: unsupported sub-process parameter `{p}` (only `x` is bound)"
            )));
        }
        // `from_node: "<id>"` → recurse.
        if let Some(Value::String(target)) = obj.get("from_node") {
            let val = eval_node_inline(target, nodes, memo, in_progress, x)?;
            return Ok(ArgVal::Num(val));
        }
        return Err(ExecError::InvalidGraph(format!(
            "apply: unsupported argument shape: {v}"
        )));
    }
    if let Some(arr) = v.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for elem in arr {
            match resolve_arg_value(elem, nodes, memo, in_progress, x)? {
                ArgVal::Num(n) => out.push(n),
                ArgVal::Arr(_) => return Err(ExecError::InvalidGraph(
                    "apply: nested arrays not supported in sub-graph arguments".into(),
                )),
            }
        }
        return Ok(ArgVal::Arr(out));
    }
    if v.is_null() {
        return Err(ExecError::InvalidGraph(
            "apply: null argument value".into(),
        ));
    }
    Err(ExecError::InvalidGraph(format!(
        "apply: unsupported argument value: {v}"
    )))
}

// Inline recursion helper so eval_node can call across module boundary
// without re-entering the public entry point.
fn eval_node_inline(
    id: &str,
    nodes: &Map<String, Value>,
    memo: &mut HashMap<String, f64>,
    in_progress: &mut Vec<String>,
    x: f64,
) -> Result<f64, ExecError> {
    if let Some(v) = memo.get(id) {
        return Ok(*v);
    }
    if in_progress.iter().any(|n| n == id) {
        return Err(ExecError::InvalidGraph(format!(
            "apply: cycle detected at node `{id}`"
        )));
    }
    in_progress.push(id.to_string());
    let node = nodes.get(id).ok_or_else(|| ExecError::InvalidGraph(format!(
        "apply: unknown node reference `{id}`"
    )))?;
    let pid = node
        .get("process_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ExecError::InvalidGraph(format!(
            "apply: node `{id}` has no process_id"
        )))?;
    let args_obj = node
        .get("arguments")
        .and_then(|v| v.as_object())
        .ok_or_else(|| ExecError::InvalidGraph(format!(
            "apply: node `{id}` has no arguments object"
        )))?;
    let mut resolved: BTreeMap<String, ArgVal> = BTreeMap::new();
    for (k, v) in args_obj {
        let r = resolve_arg_value(v, nodes, memo, in_progress, x)?;
        resolved.insert(k.clone(), r);
    }
    let value = apply_pure_numeric_op(pid, &resolved)?;
    in_progress.pop();
    memo.insert(id.to_string(), value);
    Ok(value)
}

/// Pure-numeric per-pixel kernel dispatch. Both `(x, y)` named-arg and
/// `data: [a, b]` array-arg shapes are supported for binary processes.
fn apply_pure_numeric_op(
    process_id: &str,
    args: &BTreeMap<String, ArgVal>,
) -> Result<f64, ExecError> {
    // Helpers to pull the two-operand shape: either (x, y) named or
    // `data: [a, b]` array.
    let two = |who: &str| -> Result<(f64, f64), ExecError> {
        if let (Some(x), Some(y)) = (args.get("x"), args.get("y")) {
            return Ok((x.as_num("x")?, y.as_num("y")?));
        }
        if let Some(d) = args.get("data") {
            return d.as_pair(who);
        }
        Err(ExecError::InvalidGraph(format!(
            "apply: `{who}` needs `x`+`y` or `data: [a, b]`"
        )))
    };
    let one = |who: &str| -> Result<f64, ExecError> {
        if let Some(x) = args.get("x") {
            return x.as_num("x");
        }
        if let Some(d) = args.get("data") {
            return d.as_num("data");
        }
        Err(ExecError::InvalidGraph(format!(
            "apply: `{who}` needs `x` or `data`"
        )))
    };

    Ok(match process_id {
        // arithmetic
        "add" => { let (x, y) = two("add")?; x + y }
        "subtract" => { let (x, y) = two("subtract")?; x - y }
        "multiply" => { let (x, y) = two("multiply")?; x * y }
        "divide" => {
            let (x, y) = two("divide")?;
            if y == 0.0 {
                // B4: pixel-specific arithmetic error — swallowed → NA.
                return Err(ExecError::PerPixelComputation("apply: divide by zero".into()));
            }
            x / y
        }
        "power" => {
            // openEO `power(base, p)`: prefer named `base`+`p`, fall back to `x`+`y`.
            let base = args
                .get("base")
                .or_else(|| args.get("x"))
                .ok_or_else(|| ExecError::InvalidGraph("apply: `power` needs `base` or `x`".into()))?
                .as_num("base")?;
            let p = args
                .get("p")
                .or_else(|| args.get("y"))
                .ok_or_else(|| ExecError::InvalidGraph("apply: `power` needs `p` or `y`".into()))?
                .as_num("p")?;
            base.powf(p)
        }
        "absolute" | "abs" => one("absolute")?.abs(),
        "ln" => one("ln")?.ln(),
        "log" => {
            // openEO `log(x, base)`.
            let x = args
                .get("x")
                .or_else(|| args.get("data"))
                .ok_or_else(|| ExecError::InvalidGraph("apply: `log` needs `x` or `data`".into()))?
                .as_num("x")?;
            let base = args
                .get("base")
                .or_else(|| args.get("y"))
                .ok_or_else(|| ExecError::InvalidGraph("apply: `log` needs `base` or `y`".into()))?
                .as_num("base")?;
            x.log(base)
        }
        "exp" => one("exp")?.exp(),

        // comparison → 0.0 / 1.0
        "eq" => { let (x, y) = two("eq")?; if (x - y).abs() < 1e-9 { 1.0 } else { 0.0 } }
        "neq" => { let (x, y) = two("neq")?; if (x - y).abs() >= 1e-9 { 1.0 } else { 0.0 } }
        "lt" => { let (x, y) = two("lt")?; if x < y  { 1.0 } else { 0.0 } }
        "lte" => { let (x, y) = two("lte")?; if x <= y { 1.0 } else { 0.0 } }
        "gt" => { let (x, y) = two("gt")?; if x > y  { 1.0 } else { 0.0 } }
        "gte" => { let (x, y) = two("gte")?; if x >= y { 1.0 } else { 0.0 } }

        // boolean (treat non-zero as true)
        "and" => { let (x, y) = two("and")?; if x != 0.0 && y != 0.0 { 1.0 } else { 0.0 } }
        "or" => { let (x, y) = two("or")?; if x != 0.0 || y != 0.0 { 1.0 } else { 0.0 } }
        "not" => { let x = one("not")?; if x == 0.0 { 1.0 } else { 0.0 } }

        // statistics
        "max" => { let (x, y) = two("max")?; x.max(y) }
        "min" => { let (x, y) = two("min")?; x.min(y) }
        "clip" => {
            let x = args
                .get("x")
                .or_else(|| args.get("data"))
                .ok_or_else(|| ExecError::InvalidGraph("apply: `clip` needs `x` or `data`".into()))?
                .as_num("x")?;
            let lo = args
                .get("min")
                .ok_or_else(|| ExecError::InvalidGraph("apply: `clip` needs `min`".into()))?
                .as_num("min")?;
            let hi = args
                .get("max")
                .ok_or_else(|| ExecError::InvalidGraph("apply: `clip` needs `max`".into()))?
                .as_num("max")?;
            x.clamp(lo, hi)
        }
        "linear_scale_range" => {
            let x = args
                .get("x")
                .or_else(|| args.get("data"))
                .ok_or_else(|| ExecError::InvalidGraph("apply: `linear_scale_range` needs `x` or `data`".into()))?
                .as_num("x")?;
            let in_min = args.get("inputMin")
                .ok_or_else(|| ExecError::InvalidGraph("apply: `linear_scale_range` needs `inputMin`".into()))?
                .as_num("inputMin")?;
            let in_max = args.get("inputMax")
                .ok_or_else(|| ExecError::InvalidGraph("apply: `linear_scale_range` needs `inputMax`".into()))?
                .as_num("inputMax")?;
            let out_min = args.get("outputMin").map(|v| v.as_num("outputMin")).transpose()?.unwrap_or(0.0);
            let out_max = args.get("outputMax").map(|v| v.as_num("outputMax")).transpose()?.unwrap_or(1.0);
            if (in_max - in_min).abs() < 1e-12 {
                return Err(ExecError::InvalidGraph(
                    "apply: linear_scale_range: inputMin == inputMax".into(),
                ));
            }
            (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min
        }

        other => {
            return Err(ExecError::InvalidGraph(format!(
                "apply: unsupported sub-process `{other}`"
            )));
        }
    })
}

impl GeoExecutor {
    /// openEO `apply(data, process, context?)` — per-pixel sub-callback.
    ///
    /// For each scene of `data.__cube`'s primary index/band, opens the
    /// raster as `RasterDataset<f32>`, runs the worker per pixel where
    /// each pixel is bound to `from_parameter("x")` in the sub-graph,
    /// and writes a new GeoTIFF per scene.
    ///
    /// Returns a new `__cube` with the same band name but updated paths.
    pub(super) async fn eval_apply(
        &self,
        mut args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let process = args
            .remove("process")
            .ok_or_else(|| ExecError::InvalidGraph("apply: missing `process` sub-callback".into()))?;
        // Validate sub-graph shape up front (before any I/O) and capture
        // for the worker.
        let sub_pg_val = match process {
            Value::Object(mut m) => m.remove("process_graph").ok_or_else(|| {
                ExecError::InvalidGraph(
                    "apply: `process` must be a Process object with `process_graph`".into(),
                )
            })?,
            _ => {
                return Err(ExecError::InvalidGraph(
                    "apply: `process` must be a Process object with `process_graph`".into(),
                ));
            }
        };
        // Tiny smoke-test with x = 0.0 to surface graph-shape errors before
        // we touch the file system.
        let _ = eval_apply_subgraph(&sub_pg_val, 0.0)?;

        let data = args
            .remove("data")
            .ok_or_else(|| ExecError::InvalidGraph("apply: missing `data`".into()))?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!(
                "apply: input is not a downloaded cube (run load_collection + ndvi first): {e}"
            ))
        })?;
        // **BUG-005 fix (2026-05-24)**: iterate over EVERY non-SCL band
        // (was: pick one allow-listed band and silently drop the rest).
        // openEO `apply(data, process)` spec: "Applies a unary process
        // (anything in `data` is replaced with the result of the inner
        // process) **on every value** in the data cube" — i.e. every
        // band, every scene, every pixel. Previously the impl picked one
        // band and dropped the others; post-merge_cubes multi-band cubes
        // therefore lost data.
        //
        // SCL (categorical 0..=11) is excluded from the per-pixel math
        // because the inner process is numeric and would corrupt the
        // classification labels. SCL is forwarded unchanged.
        let band_keys: Vec<String> = cube
            .bands
            .keys()
            .filter(|k| k.as_str() != "SCL")
            .cloned()
            .collect();
        if band_keys.is_empty() {
            return Err(ExecError::InvalidGraph(
                "apply: __cube.bands has no non-SCL band to apply over".into(),
            ));
        }
        for k in &band_keys {
            super::identifier::validate_identifier(k, "apply.band_key")?;
        }

        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Per-band: take paths, apply math per scene, accumulate output paths.
        let mut applied_bands: std::collections::BTreeMap<String, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        let mut scene_count_max: u64 = 0;
        for band_key in &band_keys {
            // **Reflectance scale (Option B, 2026-05-25)**: if this band
            // carries a DN→physical scale (from STAC raster:bands.scale),
            // convert each pixel `v -> v*scale + offset` BEFORE the user's
            // sub-graph runs, so absolute-value math (thresholds, EVI, …)
            // sees true reflectance, not raw DN. Identity (1.0, 0.0) for
            // bands without scale metadata (no-op). The OUTPUT band is in
            // physical units, so it is NOT re-registered in band_scales.
            let (scale, offset): (f64, f64) =
                cube.band_scales.get(band_key).copied().unwrap_or((1.0, 0.0));
            let paths: Vec<PathBuf> = cube.take_band(band_key).map_err(|_| {
                ExecError::InvalidGraph(format!("apply: band `{band_key}` vanished from cube"))
            })?;
            if paths.is_empty() {
                return Err(ExecError::Backend(
                    format!("apply: empty band `{band_key}` in input cube"),
                ));
            }
            scene_count_max = scene_count_max.max(paths.len() as u64);

            let mut out_paths: Vec<PathBuf> = Vec::with_capacity(paths.len());
            for (t, in_path) in paths.iter().enumerate() {
            // Build a (T=1, layers=1, R, C) f32 dataset for this scene.
            let mut rds: RasterDataset<f32> = RasterDatasetBuilder::<f32>::from_files(
                std::slice::from_ref(in_path),
            )
            .map_err(|e| ExecError::Backend(format!("apply: builder t={t}: {e}")))?
            .resolution(ImageResolution { x: 10.0, y: -10.0 })
            .block_size(BlockSize {
                rows: self.crop_size as usize,
                cols: self.crop_size as usize,
            })
            .build()
            .map_err(|e| ExecError::Backend(format!("apply: build t={t}: {e}")))?;
            rds.metadata.shape.times = 1;
            rds.metadata.shape.layers = 1;
            rds.layer_mappings = vec![
                LayerMapping { source: in_path.clone(), time_pos: 0, layer_pos: 0, band: 1 },
            ];

            // Clone the sub_pg into the worker closure (Arc'd is overkill;
            // serde Value clones are cheap structurally).
            let sub_pg = sub_pg_val.clone();
            let (w_scale, w_offset) = (scale, offset);
            let worker = move |rdb: &RasterDataBlock<f32>, _dim: Dimension| -> Array3<f32> {
                let r = rdb.rows();
                let c = rdb.cols();
                let mut out = Array3::<f32>::from_elem((1, r, c), SENTINEL_NDVI_NA);
                for row in 0..r {
                    for col in 0..c {
                        let v = rdb.data[[0, 0, row, col]];
                        if !v.is_finite() || v == SENTINEL_NDVI_NA {
                            continue;
                        }
                        // Option B: DN → physical reflectance before the
                        // user's process. Identity when (1.0, 0.0).
                        let scaled = v as f64 * w_scale + w_offset;
                        match eval_apply_subgraph(&sub_pg, scaled) {
                            Ok(n) if n.is_finite() => out[[0, row, col]] = n as f32,
                            Ok(_) => {
                                // Non-finite (NaN/Inf) — treat as NA.
                            }
                            Err(ExecError::PerPixelComputation(_)) => {
                                // B4: only pixel-specific arithmetic errors
                                // are swallowed → NA. Structural graph errors
                                // are caught by the upfront smoke test below
                                // and propagated at the call site.
                            }
                            Err(_) => {
                                // Structural error reaching here is unexpected
                                // (upfront smoke test should have caught it);
                                // keep NA so we never panic from a worker.
                                // Logged for visibility.
                                tracing::warn!(
                                    "apply: unexpected structural error during per-pixel eval;                                      upfront smoke test should have caught this"
                                );
                            }
                        }
                    }
                }
                out
            };

            let out_path = self.scratch_dir.join(format!(
                "apply_{band_key}_t{t}_{}.tif",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            rds.apply_reduction::<f32, _>(
                worker,
                Dimension::Layer,
                n_threads,
                &out_path,
                SENTINEL_NDVI_NA,
            )
            .map_err(|e| ExecError::Backend(format!("apply: apply_reduction t={t}: {e}")))?;
            out_paths.push(out_path);
            } // end per-scene loop
            applied_bands.insert(band_key.clone(), out_paths);
        } // end per-band loop

        // Build the output cube: insert every applied band's new paths,
        // then forward any remaining bands (SCL) that we did not touch.
        let mut out_cube = DataCube::new();
        let layer_names: Vec<String> = applied_bands.keys().cloned().collect();
        for (k, v) in applied_bands {
            out_cube.bands.insert(k, v);
        }
        // Forward remaining bands (e.g. SCL) — moved by value.
        for (k, v) in std::mem::take(&mut cube.bands) {
            out_cube.bands.insert(k, v);
        }
        out_cube.bbox = cube.bbox.clone();
        out_cube.scene_count = Some(scene_count_max);
        out_cube.times = Some(scene_count_max);
        out_cube.layers = Some(layer_names.len() as u64);
        out_cube.layer_names = Some(layer_names);
        Ok(out_cube.to_envelope())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn eval_apply_subgraph_constant_returns_value() {
        // add(x, 0) → returns x
        let pg = json!({
            "c": {
                "process_id": "add",
                "arguments": { "x": { "from_parameter": "x" }, "y": 0 },
                "result": true
            }
        });
        let r = eval_apply_subgraph(&pg, 5.0).unwrap();
        assert!((r - 5.0).abs() < 1e-9);
    }

    #[test]
    fn eval_apply_subgraph_add_then_multiply_topo_order() {
        // add(x, 1) → multiply(result, 2) on x=3.0 should yield 8.0
        let pg = json!({
            "a": {
                "process_id": "add",
                "arguments": { "x": { "from_parameter": "x" }, "y": 1 }
            },
            "m": {
                "process_id": "multiply",
                "arguments": { "x": { "from_node": "a" }, "y": 2 },
                "result": true
            }
        });
        let r = eval_apply_subgraph(&pg, 3.0).unwrap();
        assert!((r - 8.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn eval_apply_subgraph_clip_negative_to_zero() {
        // max([x, 0]) is the canonical "clip negative NDVI to 0" recipe.
        let pg = json!({
            "max_zero": {
                "process_id": "max",
                "arguments": { "data": [{ "from_parameter": "x" }, 0] },
                "result": true
            }
        });
        assert!((eval_apply_subgraph(&pg, -0.5).unwrap() - 0.0).abs() < 1e-9);
        assert!((eval_apply_subgraph(&pg, 0.7).unwrap() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn eval_apply_subgraph_eq_threshold() {
        let pg = json!({
            "e": {
                "process_id": "eq",
                "arguments": { "data": [{ "from_parameter": "x" }, 5] },
                "result": true
            }
        });
        assert!((eval_apply_subgraph(&pg, 5.0).unwrap() - 1.0).abs() < 1e-9);
        assert!((eval_apply_subgraph(&pg, 4.0).unwrap() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn eval_apply_subgraph_rejects_unknown_process() {
        let pg = json!({
            "u": {
                "process_id": "magic",
                "arguments": { "x": { "from_parameter": "x" } },
                "result": true
            }
        });
        let r = eval_apply_subgraph(&pg, 1.0);
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "got {r:?}");
        if let Err(ExecError::InvalidGraph(m)) = r {
            assert!(m.contains("magic"), "expected mention of bad process_id, got `{m}`");
        }
    }

    #[test]
    fn eval_apply_subgraph_rejects_missing_result_node() {
        let pg = json!({
            "n": {
                "process_id": "add",
                "arguments": { "x": { "from_parameter": "x" }, "y": 1 }
                // no result: true
            }
        });
        let r = eval_apply_subgraph(&pg, 0.0);
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "got {r:?}");
    }

    #[test]
    fn eval_apply_subgraph_handles_data_array_arg() {
        // add(data=[3, 4]) → 7
        let pg = json!({
            "a": {
                "process_id": "add",
                "arguments": { "data": [3, 4] },
                "result": true
            }
        });
        let r = eval_apply_subgraph(&pg, 0.0).unwrap();
        assert!((r - 7.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn eval_apply_subgraph_divide_by_zero_is_per_pixel_error() {
        // B4: divide-by-zero is a per-pixel arithmetic error (PerPixelComputation),
        // NOT a structural InvalidGraph error. The per-pixel loop swallows the
        // former and writes NA; the latter propagates.
        let pg = json!({
            "d": {
                "process_id": "divide",
                "arguments": { "x": { "from_parameter": "x" }, "y": 0 },
                "result": true
            }
        });
        let r = eval_apply_subgraph(&pg, 5.0);
        assert!(matches!(r, Err(ExecError::PerPixelComputation(_))), "got {r:?}");
    }

    #[test]
    fn eval_apply_subgraph_boolean_not() {
        let pg = json!({
            "n": {
                "process_id": "not",
                "arguments": { "x": { "from_parameter": "x" } },
                "result": true
            }
        });
        assert!((eval_apply_subgraph(&pg, 0.0).unwrap() - 1.0).abs() < 1e-9);
        assert!((eval_apply_subgraph(&pg, 1.0).unwrap() - 0.0).abs() < 1e-9);
    }

    // ---------- B4: unknown sub-process must error, not silently NA ----------

    #[test]
    fn eval_apply_subgraph_divide_by_zero_is_per_pixel_error_not_structural() {
        // B4: divide-by-zero is a per-pixel arithmetic error — the worker
        // swallows it and writes NA. It MUST be the PerPixelComputation
        // variant so the worker can distinguish it from structural errors.
        let pg = json!({
            "d": {
                "process_id": "divide",
                "arguments": { "x": { "from_parameter": "x" }, "y": 0 },
                "result": true
            }
        });
        let r = eval_apply_subgraph(&pg, 5.0);
        match r {
            Err(ExecError::PerPixelComputation(m)) => {
                assert!(m.contains("divide"), "msg should mention divide, got: {m}");
            }
            other => panic!("expected PerPixelComputation, got {other:?}"),
        }
    }

    #[test]
    fn eval_apply_subgraph_clip_clamps_three_arg() {
        let pg = json!({
            "c": {
                "process_id": "clip",
                "arguments": {
                    "x": { "from_parameter": "x" },
                    "min": 0.0, "max": 1.0
                },
                "result": true
            }
        });
        assert!((eval_apply_subgraph(&pg, -0.3).unwrap() - 0.0).abs() < 1e-9);
        assert!((eval_apply_subgraph(&pg, 0.5).unwrap() - 0.5).abs() < 1e-9);
        assert!((eval_apply_subgraph(&pg, 1.5).unwrap() - 1.0).abs() < 1e-9);
    }
}
