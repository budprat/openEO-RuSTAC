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

/// Parse an openEO `reducer` sub-process callback into a [`Reducer`].
///
/// Per spec, `reducer` is a Process object holding a `process_graph`.
/// The result-node's `process_id` names the reducer; its `data` argument
/// must bind `from_parameter("data")` (the per-pixel temporal vector).
///
/// Returns `InvalidGraph` if the sub-graph is missing, malformed, or
/// names an unsupported process_id.
///
/// **A4+A5 invariant**: extraction + result-node discovery delegates to
/// `sub_graph::{require_subgraph, find_unique_result_node, result_process_id}`
/// so the three eval sites share the same validation contract.
pub fn parse_reducer_subgraph(reducer: &Value) -> Result<Reducer, ExecError> {
    let pg = require_subgraph(reducer, "reduce_dimension.reducer")?;
    let (_, result_node) = find_unique_result_node(pg, "reduce_dimension.reducer")?;
    let pid = result_process_id(result_node, "reduce_dimension.reducer")?;
    Reducer::from_process_id(pid).ok_or_else(|| ExecError::InvalidGraph(format!(
        "reduce_dimension: unsupported reducer `{pid}` (supported: mean, min, max, sum, median, count, first, last, sd, variance)"
    )))
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
        if !matches!(dim.as_str(), "t" | "time" | "temporal") {
            return Err(ExecError::InvalidGraph(format!(
                "reduce_dimension: only temporal reduction is supported by GeoExecutor today (got `{dim}`)"
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
        // Band-flexible: reduce_dimension consumes whichever index band
        // is present (ndvi/ndmi/ndwi/etc.) — the f32 output of a prior
        // `ndvi` call.
        const F32_INDEX_BANDS: &[&str] = &["ndvi", "ndmi", "ndwi", "evi", "savi", "msavi"];
        let index_key: String = F32_INDEX_BANDS
            .iter()
            .find(|k| cube.bands.contains_key(**k))
            .map(|s| s.to_string())
            .or_else(|| {
                if cube.bands.len() == 1 {
                    cube.bands.keys().next().cloned()
                } else {
                    None
                }
            })
            .ok_or_else(|| ExecError::InvalidGraph(
                "reduce_dimension: __cube.bands has no recognised index band \
                 (expected ndvi/ndmi/ndwi/etc. — run `ndvi` first)".into(),
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

        // Dispatched worker — captures the `reducer` enum and runs the
        // chosen per-pixel kernel over the time vector.
        let red = reducer;
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
                        out[[0, row, col]] = apply_reducer(&mut stack, red);
                    }
                }
            }
            out
        };

        rds.apply_reduction::<f32, _>(worker, Dimension::Time, n_threads, &out_path, SENTINEL_NDVI_NA)
            .map_err(|e| ExecError::Backend(format!("reduce_dimension: apply_reduction: {e}")))?;

        Ok(json!({
            "__raster": {
                "path": out_path,
                "media_type": "image/tiff",
                "produced_by": format!("reduce_dimension({})", red.name())
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
        assert_eq!(parse_reducer_subgraph(&red).unwrap(), Reducer::Mean);
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
        let red = serde_json::json!({
            "process_graph": {
                "m": {
                    "process_id": "ln",  // not a supported reducer
                    "arguments": {},
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
    async fn reduce_dimension_rejects_non_temporal_dimension() {
        let exe = GeoExecutor::new();
        let mut args = std::collections::BTreeMap::new();
        args.insert("dimension".into(), Value::String("bands".into()));
        args.insert("data".into(), serde_json::json!({"__cube": {"bands": {"ndvi": []}}}));
        args.insert("reducer".into(), serde_json::json!({
            "process_graph": {"m": {"process_id": "mean", "arguments": {"data": {"from_parameter": "data"}}, "result": true}}
        }));
        let r = exe.eval_reduce_dimension(args).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }
}
