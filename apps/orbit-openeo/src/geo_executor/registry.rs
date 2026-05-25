//! Process dispatch registry — replaces the giant match arm in
//! [`GeoExecutor::evaluate`] with a `HashMap<&'static str, Arc<dyn ProcessHandler>>`
//! built once at executor construction.
//!
//! Design: handlers are **stateless** zero-sized structs. They receive the
//! borrowing `GeoExecutor` (which carries scratch_dir, crop_size, catalog,
//! searcher, downloader, signer, cache, semaphore, url_policy) as the
//! first argument so no Arc cycle exists between GeoExecutor → registry →
//! handler → GeoExecutor. Adding a new openEO process means: write an
//! `eval_X` method on GeoExecutor, write a unit-struct handler that calls
//! it, and append one `register` line in [`register_defaults`].
//!
//! Per the project's bench-first house style (CLAUDE.md §8): dispatching
//! through `Arc<dyn ProcessHandler>` adds one virtual call per node
//! evaluation — negligible against the GDAL I/O, NDVI compute, and
//! reduce_dimension passes that dominate every realistic graph.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::executor::ExecError;

use super::GeoExecutor;

/// Handler for one openEO process (e.g. `"ndvi"`, `"mask"`). Implementations
/// receive the resolved `args` (each value already pre-walked by the
/// topological evaluator) plus a borrow of the [`GeoExecutor`] so they can
/// reach scratch_dir / crop_size / cache / downloader without owning them.
#[async_trait]
pub trait ProcessHandler: Send + Sync {
    async fn handle(
        &self,
        exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError>;
}

/// Registry mapping openEO process names to handlers. Populated once at
/// [`GeoExecutor::new`] time via [`register_defaults`] — replaces the
/// monolithic match-arm dispatcher.
#[derive(Default, Clone)]
pub struct ProcessRegistry {
    handlers: HashMap<&'static str, Arc<dyn ProcessHandler>>,
}

impl ProcessRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler under `name`. If `name` was previously registered
    /// the prior handler is replaced (matches `HashMap::insert` semantics).
    pub fn register(&mut self, name: &'static str, handler: Arc<dyn ProcessHandler>) {
        self.handlers.insert(name, handler);
    }

    /// Look up the handler for `name`, returning a clone of the Arc so the
    /// caller can `await` it without borrowing the registry.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ProcessHandler>> {
        self.handlers.get(name).cloned()
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    /// Sorted list of registered process names (deterministic for tests
    /// and for surfacing the implementation set to clients).
    #[must_use]
    pub fn registered_processes(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.handlers.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// **did-you-mean (2026-05-25)**: up to `max` registered process names
    /// closest to `name` by Levenshtein distance, nearest first. Only
    /// returns candidates within a distance threshold of
    /// `max(2, name.len() / 3)` so unrelated names aren't suggested.
    /// Mirrors JonaAI's `difflib.get_close_matches` UX (executor.py:667).
    #[must_use]
    pub fn suggest(&self, name: &str, max: usize) -> Vec<&'static str> {
        let threshold = (name.len() / 3).max(2);
        let mut scored: Vec<(usize, &'static str)> = self
            .handlers
            .keys()
            .copied()
            .map(|p| (levenshtein(name, p), p))
            .filter(|(d, _)| *d <= threshold)
            .collect();
        // Sort by distance, then alphabetically for determinism.
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().take(max).map(|(_, p)| p).collect()
    }
}

/// Iterative Levenshtein edit distance (two-row DP). No external dep —
/// orbit keeps its dependency surface minimal (CLAUDE.md §8).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1)
                .min(curr[j] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

// ---------------------------------------------------------------------
// Handler unit structs — one per openEO process. Each is a thin
// adapter onto the existing `GeoExecutor::eval_X` method, which is
// the single source of behaviour. Touching the eval methods'
// internals is OUT OF SCOPE for atom A2 — A3 owns sub-graph parsing
// inside eval_apply / eval_mask / eval_reduce.
// ---------------------------------------------------------------------

macro_rules! async_handler {
    ($name:ident, $method:ident) => {
        pub struct $name;
        #[async_trait]
        impl ProcessHandler for $name {
            async fn handle(
                &self,
                exe: &GeoExecutor,
                args: BTreeMap<String, Value>,
            ) -> Result<Value, ExecError> {
                exe.$method(args).await
            }
        }
    };
}

macro_rules! sync_handler {
    ($name:ident, $method:ident) => {
        pub struct $name;
        #[async_trait]
        impl ProcessHandler for $name {
            async fn handle(
                &self,
                exe: &GeoExecutor,
                args: BTreeMap<String, Value>,
            ) -> Result<Value, ExecError> {
                exe.$method(args)
            }
        }
    };
}

// Async, cube-producing processes (real GDAL work).
async_handler!(LoadCollectionHandler, eval_load_collection);
async_handler!(NdviHandler, eval_ndvi);
async_handler!(MaskHandler, eval_mask);
async_handler!(MaskFromValuesHandler, eval_mask_from_values);
async_handler!(MaskSclDilationHandler, eval_mask_scl_dilation);
async_handler!(ReduceDimensionHandler, eval_reduce_dimension);

// Sync orbit-extension processes (pure GDAL / ndarray, no .await).
sync_handler!(ResampleSpatialHandler, eval_resample_spatial);
sync_handler!(ZonalHistogramHandler, eval_zonal_histogram);
sync_handler!(AggregateSpatialPolygonHandler, eval_aggregate_spatial_polygon);
sync_handler!(AggregateSpatialPointHandler, eval_aggregate_spatial_point);
sync_handler!(MergeCubesHandler, eval_merge_cubes);
sync_handler!(FitClassifierHandler, eval_fit_classifier);
sync_handler!(PredictClassifierHandler, eval_predict_classifier);

// Cube-metadata ops (sync, no GDAL I/O) — openEO 1.3.0 spec.
sync_handler!(FilterBandsHandler, eval_filter_bands);
sync_handler!(RenameLabelsHandler, eval_rename_labels);
sync_handler!(AddDimensionHandler, eval_add_dimension);
sync_handler!(DropDimensionHandler, eval_drop_dimension);

// `apply` is special — per P0-4 it must reject when the `process` callback
// is absent OR when sub-graph validation fails. The legacy match arm had a
// soft-fallback path (forward `data` unchanged when `process` is omitted)
// that we preserve here to avoid breaking the `apply(data)` only-call shape.
pub struct ApplyHandler;
#[async_trait]
impl ProcessHandler for ApplyHandler {
    async fn handle(
        &self,
        exe: &GeoExecutor,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        if args.get("process").is_some() {
            exe.eval_apply(args).await
        } else {
            Ok(args.remove("data").unwrap_or(Value::Null))
        }
    }
}

// `save_result` doesn't touch GeoExecutor state — it just rewraps args.
// The post-evaluate `finalise_save_result` pass in mod.rs does the actual
// byte materialisation.
pub struct SaveResultHandler;
#[async_trait]
impl ProcessHandler for SaveResultHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let format = args
            .get("format")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default();
        let data = args.remove("data").unwrap_or(Value::Null);
        if format.is_empty() {
            Ok(data)
        } else {
            Ok(serde_json::json!({ "__save_result": { "data": data, "format": format } }))
        }
    }
}

// Arithmetic processes — stateless, pure functions of args.
fn arg_number(args: &BTreeMap<String, Value>, key: &str) -> Result<f64, ExecError> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ExecError::InvalidGraph(format!("expected numeric `{key}` argument")))
}

fn arg_bool(args: &BTreeMap<String, Value>, key: &str) -> Result<bool, ExecError> {
    args.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| ExecError::InvalidGraph(format!("expected boolean `{key}` argument")))
}

// ---------------------------------------------------------------------
// **Standalone scalar math / comparison / logical processes**
// (openEO 1.3.0 spec — 2026-05-25). These complement the apply-callback
// versions: a process graph can now use e.g. `power` / `sqrt` / `eq`
// as TOP-LEVEL nodes (computing scalar parameters), not only inside an
// `apply` sub-graph. Each is a pure function of its args; no GeoExecutor
// state, no GDAL I/O.
//
// Param names follow the spec exactly (e.g. power(base, p), log(x, base),
// mod(x, y), normalized_difference(x, y), clip(x, min, max)).
// ---------------------------------------------------------------------

/// Unary numeric process: one arg named `x` → number.
macro_rules! unary_num_handler {
    ($name:ident, $argkey:literal, $f:expr) => {
        pub struct $name;
        #[async_trait]
        impl ProcessHandler for $name {
            async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
                -> Result<Value, ExecError> {
                let x = arg_number(&args, $argkey)?;
                let r: f64 = ($f)(x);
                Ok(Value::from(r))
            }
        }
    };
}

/// Binary numeric process: args `$a` and `$b` → number.
macro_rules! binary_num_handler {
    ($name:ident, $a:literal, $b:literal, $f:expr) => {
        pub struct $name;
        #[async_trait]
        impl ProcessHandler for $name {
            async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
                -> Result<Value, ExecError> {
                let a = arg_number(&args, $a)?;
                let b = arg_number(&args, $b)?;
                let r: f64 = ($f)(a, b);
                Ok(Value::from(r))
            }
        }
    };
}

/// Binary comparison process: args `x`,`y` → boolean.
macro_rules! cmp_handler {
    ($name:ident, $f:expr) => {
        pub struct $name;
        #[async_trait]
        impl ProcessHandler for $name {
            async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
                -> Result<Value, ExecError> {
                let x = arg_number(&args, "x")?;
                let y = arg_number(&args, "y")?;
                let r: bool = ($f)(x, y);
                Ok(Value::Bool(r))
            }
        }
    };
}

// Unary math (spec param `x`, except exp/ln use `p`/`x`).
unary_num_handler!(AbsoluteHandler, "x", |x: f64| x.abs());
unary_num_handler!(SqrtHandler, "x", |x: f64| x.sqrt());
unary_num_handler!(ExpHandler, "p", |x: f64| x.exp());
unary_num_handler!(LnHandler, "x", |x: f64| x.ln());
unary_num_handler!(SgnHandler, "x", |x: f64| x.signum());
unary_num_handler!(FloorHandler, "x", |x: f64| x.floor());
unary_num_handler!(CeilHandler, "x", |x: f64| x.ceil());
unary_num_handler!(IntHandler, "x", |x: f64| x.trunc());
unary_num_handler!(CosHandler, "x", |x: f64| x.cos());
unary_num_handler!(SinHandler, "x", |x: f64| x.sin());
unary_num_handler!(TanHandler, "x", |x: f64| x.tan());
unary_num_handler!(ArccosHandler, "x", |x: f64| x.acos());
unary_num_handler!(ArcsinHandler, "x", |x: f64| x.asin());
unary_num_handler!(ArctanHandler, "x", |x: f64| x.atan());

// Binary math.
binary_num_handler!(PowerHandler, "base", "p", |b: f64, p: f64| b.powf(p));
// openEO `mod(x, y)`: result has the **sign of the divisor** y (like
// Python's %), NOT Rust's `%` (sign of dividend) or `rem_euclid` (always
// non-negative). Spec-correct formula: x - y * floor(x / y).
binary_num_handler!(ModHandler, "x", "y", |x: f64, y: f64| x - y * (x / y).floor());
binary_num_handler!(Arctan2Handler, "y", "x", |y: f64, x: f64| y.atan2(x));

// Comparison (gt/gte/lt/lte → boolean). eq/neq are handled separately
// below because the spec gives them an optional `delta` tolerance param.
cmp_handler!(GtHandler, |x: f64, y: f64| x > y);
cmp_handler!(GteHandler, |x: f64, y: f64| x >= y);
cmp_handler!(LtHandler, |x: f64, y: f64| x < y);
cmp_handler!(LteHandler, |x: f64, y: f64| x <= y);

/// openEO `eq(x, y, delta?, case_sensitive?)` — equality with optional
/// numeric tolerance. Default (`delta` omitted) is EXACT comparison per
/// spec. When `delta` is supplied, |x - y| <= delta. (We handle the
/// numeric case; string/case_sensitive comparison is out of scope for
/// the raster value path.)
pub struct EqHandler;
#[async_trait]
impl ProcessHandler for EqHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let y = arg_number(&args, "y")?;
        let r = match args.get("delta").and_then(|v| v.as_f64()) {
            Some(d) => (x - y).abs() <= d,
            None => x == y, // exact per spec
        };
        Ok(Value::Bool(r))
    }
}

/// openEO `neq(x, y, delta?, case_sensitive?)` — logical negation of `eq`.
pub struct NeqHandler;
#[async_trait]
impl ProcessHandler for NeqHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let y = arg_number(&args, "y")?;
        let eq = match args.get("delta").and_then(|v| v.as_f64()) {
            Some(d) => (x - y).abs() <= d,
            None => x == y,
        };
        Ok(Value::Bool(!eq))
    }
}

/// `log(x, base)` — logarithm to arbitrary base. Spec param names `x`,`base`.
pub struct LogHandler;
#[async_trait]
impl ProcessHandler for LogHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let base = arg_number(&args, "base")?;
        if base <= 0.0 || (base - 1.0).abs() < f64::EPSILON {
            return Err(ExecError::InvalidGraph("log: base must be > 0 and != 1".into()));
        }
        Ok(Value::from(x.log(base)))
    }
}

/// `round(x, p?)` — round to `p` decimal places (default 0), banker's
/// rounding per the openEO spec (round-half-to-even).
pub struct RoundHandler;
#[async_trait]
impl ProcessHandler for RoundHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let p = args.get("p").and_then(|v| v.as_i64()).unwrap_or(0);
        let factor = 10f64.powi(p as i32);
        let scaled = x * factor;
        // Round half to even (banker's rounding) per spec.
        let r = scaled.round_ties_even() / factor;
        Ok(Value::from(r))
    }
}

/// `clip(x, min, max)` — clamp x to [min, max].
pub struct ClipHandler;
#[async_trait]
impl ProcessHandler for ClipHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let lo = arg_number(&args, "min")?;
        let hi = arg_number(&args, "max")?;
        Ok(Value::from(x.clamp(lo, hi)))
    }
}

/// `normalized_difference(x, y)` → (x - y) / (x + y). Generalizes NDVI.
pub struct NormalizedDifferenceHandler;
#[async_trait]
impl ProcessHandler for NormalizedDifferenceHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let y = arg_number(&args, "y")?;
        let denom = x + y;
        if denom == 0.0 {
            return Err(ExecError::InvalidGraph("normalized_difference: x + y == 0".into()));
        }
        Ok(Value::from((x - y) / denom))
    }
}

/// `between(x, min, max, exclude_max?)` → boolean.
pub struct BetweenHandler;
#[async_trait]
impl ProcessHandler for BetweenHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let lo = arg_number(&args, "min")?;
        let hi = arg_number(&args, "max")?;
        let exclude_max = args.get("exclude_max").and_then(|v| v.as_bool()).unwrap_or(false);
        let r = x >= lo && (if exclude_max { x < hi } else { x <= hi });
        Ok(Value::Bool(r))
    }
}

/// Logical `and(x, y)` / `or(x, y)` / `xor(x, y)` / `not(x)`.
pub struct AndHandler;
#[async_trait]
impl ProcessHandler for AndHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        Ok(Value::Bool(arg_bool(&args, "x")? && arg_bool(&args, "y")?))
    }
}
pub struct OrHandler;
#[async_trait]
impl ProcessHandler for OrHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        Ok(Value::Bool(arg_bool(&args, "x")? || arg_bool(&args, "y")?))
    }
}
pub struct XorHandler;
#[async_trait]
impl ProcessHandler for XorHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        Ok(Value::Bool(arg_bool(&args, "x")? ^ arg_bool(&args, "y")?))
    }
}
pub struct NotHandler;
#[async_trait]
impl ProcessHandler for NotHandler {
    async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
        -> Result<Value, ExecError> {
        Ok(Value::Bool(!arg_bool(&args, "x")?))
    }
}

// ---------------------------------------------------------------------
// **Array processes** (openEO 1.3.0 spec — 2026-05-25). Stateless free
// functions over JSON arrays in `super::eval_arrays`. This macro adapts
// a `fn(&BTreeMap) -> Result<Value>` into a ProcessHandler.
// ---------------------------------------------------------------------
macro_rules! array_fn_handler {
    ($name:ident, $f:path) => {
        pub struct $name;
        #[async_trait]
        impl ProcessHandler for $name {
            async fn handle(&self, _e: &GeoExecutor, args: BTreeMap<String, Value>)
                -> Result<Value, ExecError> {
                $f(&args)
            }
        }
    };
}
array_fn_handler!(ArrayElementHandler, super::eval_arrays::array_element);
array_fn_handler!(ArrayCreateHandler, super::eval_arrays::array_create);
array_fn_handler!(ArrayConcatHandler, super::eval_arrays::array_concat);
array_fn_handler!(ArrayAppendHandler, super::eval_arrays::array_append);
array_fn_handler!(ArrayContainsHandler, super::eval_arrays::array_contains);
array_fn_handler!(ArrayFindHandler, super::eval_arrays::array_find);
array_fn_handler!(CountHandler, super::eval_arrays::count);
array_fn_handler!(OrderHandler, super::eval_arrays::order);
array_fn_handler!(SortHandler, super::eval_arrays::sort);

pub struct AddHandler;
#[async_trait]
impl ProcessHandler for AddHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        Ok(Value::from(arg_number(&args, "x")? + arg_number(&args, "y")?))
    }
}

pub struct SubtractHandler;
#[async_trait]
impl ProcessHandler for SubtractHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        Ok(Value::from(arg_number(&args, "x")? - arg_number(&args, "y")?))
    }
}

pub struct MultiplyHandler;
#[async_trait]
impl ProcessHandler for MultiplyHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        Ok(Value::from(arg_number(&args, "x")? * arg_number(&args, "y")?))
    }
}

pub struct DivideHandler;
#[async_trait]
impl ProcessHandler for DivideHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let x = arg_number(&args, "x")?;
        let y = arg_number(&args, "y")?;
        if y == 0.0 {
            return Err(ExecError::InvalidGraph("divide: y must be non-zero".into()));
        }
        Ok(Value::from(x / y))
    }
}

// Filter-passthrough processes — annotate `_orbit_meta.applied` and forward
// `data`. Names map 1:1 to the old match arms.
pub struct FilterTemporalHandler;
#[async_trait]
impl ProcessHandler for FilterTemporalHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        super::filter_passthrough(&args, &["extent"], "applied_filter_temporal")
    }
}

pub struct FilterSpatialHandler;
#[async_trait]
impl ProcessHandler for FilterSpatialHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        super::filter_passthrough(&args, &["extent", "geometries"], "applied_filter_spatial")
    }
}

pub struct FilterBboxHandler;
#[async_trait]
impl ProcessHandler for FilterBboxHandler {
    async fn handle(
        &self,
        _exe: &GeoExecutor,
        args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        super::filter_passthrough(
            &args,
            &["extent", "west", "south", "east", "north"],
            "applied_filter_bbox",
        )
    }
}

/// Wire every default openEO process into `registry`. Mirrors the
/// pre-A2 match arm one-to-one — adding a process here is the ONLY
/// place that needs to change.
pub fn register_defaults(registry: &mut ProcessRegistry) {
    // Cube pipeline.
    registry.register("load_collection", Arc::new(LoadCollectionHandler));
    registry.register("ndvi", Arc::new(NdviHandler));
    registry.register("mask", Arc::new(MaskHandler));
    registry.register("mask_from_values", Arc::new(MaskFromValuesHandler));
    registry.register("mask_scl_dilation", Arc::new(MaskSclDilationHandler));
    registry.register("reduce_dimension", Arc::new(ReduceDimensionHandler));
    registry.register("apply", Arc::new(ApplyHandler));
    registry.register("save_result", Arc::new(SaveResultHandler));

    // Orbit extensions (synchronous).
    registry.register("resample_spatial", Arc::new(ResampleSpatialHandler));
    registry.register("zonal_histogram", Arc::new(ZonalHistogramHandler));
    registry.register(
        "aggregate_spatial_polygon",
        Arc::new(AggregateSpatialPolygonHandler),
    );
    registry.register(
        "aggregate_spatial_point",
        Arc::new(AggregateSpatialPointHandler),
    );
    registry.register("merge_cubes", Arc::new(MergeCubesHandler));
    registry.register("fit_classifier", Arc::new(FitClassifierHandler));
    registry.register("predict_classifier", Arc::new(PredictClassifierHandler));

    // Arithmetic.
    registry.register("add", Arc::new(AddHandler));
    registry.register("subtract", Arc::new(SubtractHandler));
    registry.register("multiply", Arc::new(MultiplyHandler));
    registry.register("divide", Arc::new(DivideHandler));

    // Filters (metadata-tagging passthroughs).
    registry.register("filter_temporal", Arc::new(FilterTemporalHandler));
    registry.register("filter_spatial", Arc::new(FilterSpatialHandler));
    registry.register("filter_bbox", Arc::new(FilterBboxHandler));

    // Cube-metadata ops (openEO 1.3.0 spec — 2026-05-25).
    registry.register("filter_bands", Arc::new(FilterBandsHandler));
    registry.register("rename_labels", Arc::new(RenameLabelsHandler));
    registry.register("add_dimension", Arc::new(AddDimensionHandler));
    registry.register("drop_dimension", Arc::new(DropDimensionHandler));

    // Standalone scalar math (openEO 1.3.0 spec — 2026-05-25).
    registry.register("absolute", Arc::new(AbsoluteHandler));
    registry.register("sqrt", Arc::new(SqrtHandler));
    registry.register("exp", Arc::new(ExpHandler));
    registry.register("ln", Arc::new(LnHandler));
    registry.register("log", Arc::new(LogHandler));
    registry.register("power", Arc::new(PowerHandler));
    registry.register("sgn", Arc::new(SgnHandler));
    registry.register("floor", Arc::new(FloorHandler));
    registry.register("ceil", Arc::new(CeilHandler));
    registry.register("int", Arc::new(IntHandler));
    registry.register("round", Arc::new(RoundHandler));
    registry.register("mod", Arc::new(ModHandler));
    registry.register("clip", Arc::new(ClipHandler));
    registry.register("normalized_difference", Arc::new(NormalizedDifferenceHandler));
    // Trigonometry.
    registry.register("cos", Arc::new(CosHandler));
    registry.register("sin", Arc::new(SinHandler));
    registry.register("tan", Arc::new(TanHandler));
    registry.register("arccos", Arc::new(ArccosHandler));
    registry.register("arcsin", Arc::new(ArcsinHandler));
    registry.register("arctan", Arc::new(ArctanHandler));
    registry.register("arctan2", Arc::new(Arctan2Handler));
    // Comparison (→ boolean).
    registry.register("eq", Arc::new(EqHandler));
    registry.register("neq", Arc::new(NeqHandler));
    registry.register("gt", Arc::new(GtHandler));
    registry.register("gte", Arc::new(GteHandler));
    registry.register("lt", Arc::new(LtHandler));
    registry.register("lte", Arc::new(LteHandler));
    registry.register("between", Arc::new(BetweenHandler));
    // Logical (→ boolean).
    registry.register("and", Arc::new(AndHandler));
    registry.register("or", Arc::new(OrHandler));
    registry.register("xor", Arc::new(XorHandler));
    registry.register("not", Arc::new(NotHandler));

    // Array processes (openEO 1.3.0 spec — 2026-05-25).
    registry.register("array_element", Arc::new(ArrayElementHandler));
    registry.register("array_create", Arc::new(ArrayCreateHandler));
    registry.register("array_concat", Arc::new(ArrayConcatHandler));
    registry.register("array_append", Arc::new(ArrayAppendHandler));
    registry.register("array_contains", Arc::new(ArrayContainsHandler));
    registry.register("array_find", Arc::new(ArrayFindHandler));
    registry.register("count", Arc::new(CountHandler));
    registry.register("order", Arc::new(OrderHandler));
    registry.register("sort", Arc::new(SortHandler));
}

// ---------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: invoke a registered scalar handler with the given args map.
    async fn call(proc: &str, args: serde_json::Value) -> Result<Value, ExecError> {
        let mut r = ProcessRegistry::new();
        register_defaults(&mut r);
        let h = r.get(proc).unwrap_or_else(|| panic!("process `{proc}` registered"));
        let exe = GeoExecutor::new();
        let m: BTreeMap<String, Value> = args.as_object().unwrap().clone().into_iter().collect();
        h.handle(&exe, m).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scalar_math_processes_compute_correctly() {
        use serde_json::json;
        assert_eq!(call("power", json!({"base": 2.0, "p": 10.0})).await.unwrap(), json!(1024.0));
        assert_eq!(call("sqrt", json!({"x": 16.0})).await.unwrap(), json!(4.0));
        assert_eq!(call("absolute", json!({"x": -3.5})).await.unwrap(), json!(3.5));
        assert_eq!(call("floor", json!({"x": 2.9})).await.unwrap(), json!(2.0));
        assert_eq!(call("ceil", json!({"x": 2.1})).await.unwrap(), json!(3.0));
        assert_eq!(call("clip", json!({"x": 15.0, "min": 0.0, "max": 10.0})).await.unwrap(), json!(10.0));
        assert_eq!(call("mod", json!({"x": 7.0, "y": 3.0})).await.unwrap(), json!(1.0));
        // openEO mod: result has SIGN OF DIVISOR. mod(7, -3) = -2 (not +1).
        assert_eq!(call("mod", json!({"x": 7.0, "y": -3.0})).await.unwrap(), json!(-2.0));
        // mod(-7, 3) = 2 (sign of +3).
        assert_eq!(call("mod", json!({"x": -7.0, "y": 3.0})).await.unwrap(), json!(2.0));
        // normalized_difference(8, 4) = (8-4)/(8+4) = 0.3333…
        let nd = call("normalized_difference", json!({"x": 8.0, "y": 4.0})).await.unwrap();
        assert!((nd.as_f64().unwrap() - (4.0/12.0)).abs() < 1e-9);
        // log(1000, 10) = 3
        let lg = call("log", json!({"x": 1000.0, "base": 10.0})).await.unwrap();
        assert!((lg.as_f64().unwrap() - 3.0).abs() < 1e-9);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn comparison_and_logical_processes_return_booleans() {
        use serde_json::json;
        assert_eq!(call("eq", json!({"x": 1.0, "y": 1.0})).await.unwrap(), json!(true));
        // eq exact (no delta): 0.1+0.2 != 0.3 in IEEE754.
        assert_eq!(call("eq", json!({"x": 0.3, "y": 0.30000000001})).await.unwrap(), json!(false));
        // eq with delta tolerance.
        assert_eq!(call("eq", json!({"x": 0.3, "y": 0.30000000001, "delta": 0.001})).await.unwrap(), json!(true));
        assert_eq!(call("gt", json!({"x": 2.0, "y": 1.0})).await.unwrap(), json!(true));
        assert_eq!(call("lte", json!({"x": 2.0, "y": 1.0})).await.unwrap(), json!(false));
        assert_eq!(call("between", json!({"x": 5.0, "min": 0.0, "max": 10.0})).await.unwrap(), json!(true));
        assert_eq!(call("and", json!({"x": true, "y": false})).await.unwrap(), json!(false));
        assert_eq!(call("or", json!({"x": true, "y": false})).await.unwrap(), json!(true));
        assert_eq!(call("xor", json!({"x": true, "y": true})).await.unwrap(), json!(false));
        assert_eq!(call("not", json!({"x": false})).await.unwrap(), json!(true));
    }

    #[test]
    fn suggest_returns_nearest_process_names() {
        let mut r = ProcessRegistry::new();
        register_defaults(&mut r);
        // Typo "ndiv" → "ndvi" (1 transposition-ish, distance 2).
        let s = r.suggest("ndiv", 3);
        assert!(s.contains(&"ndvi"), "expected ndvi suggestion, got {s:?}");
        // "reduce_dimensio" → "reduce_dimension".
        assert!(r.suggest("reduce_dimensio", 3).contains(&"reduce_dimension"));
        // Garbage far from everything → no suggestions.
        assert!(r.suggest("zzzzzzzzzzzz", 3).is_empty());
    }

    #[test]
    fn levenshtein_basic_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("ndiv", "ndvi"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn divide_and_normalized_difference_guard_zero() {
        use serde_json::json;
        assert!(call("normalized_difference", json!({"x": 5.0, "y": -5.0})).await.is_err());
        assert!(call("log", json!({"x": 10.0, "base": 1.0})).await.is_err());
    }

    // A trivial throw-away handler used only in registry tests — keeps the
    // tests independent of the real eval_X methods (which need GDAL).
    struct StubHandler {
        reply: Value,
    }
    #[async_trait]
    impl ProcessHandler for StubHandler {
        async fn handle(
            &self,
            _exe: &GeoExecutor,
            _args: BTreeMap<String, Value>,
        ) -> Result<Value, ExecError> {
            Ok(self.reply.clone())
        }
    }

    #[test]
    fn register_and_get_handler_roundtrip() {
        let mut r = ProcessRegistry::new();
        r.register(
            "stub",
            Arc::new(StubHandler {
                reply: Value::from(42),
            }),
        );
        assert!(r.contains("stub"));
        assert!(r.get("stub").is_some());
    }

    #[test]
    fn unregistered_process_returns_none() {
        let r = ProcessRegistry::new();
        assert!(r.get("never_registered").is_none());
        assert!(!r.contains("never_registered"));
    }

    #[test]
    fn registered_processes_returns_sorted_list() {
        let mut r = ProcessRegistry::new();
        r.register("zebra", Arc::new(StubHandler { reply: Value::Null }));
        r.register("alpha", Arc::new(StubHandler { reply: Value::Null }));
        r.register("mango", Arc::new(StubHandler { reply: Value::Null }));
        assert_eq!(r.registered_processes(), vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn register_defaults_populates_all_known_processes() {
        let mut r = ProcessRegistry::new();
        register_defaults(&mut r);
        // The full set the legacy match arm dispatched. If a process is
        // added or removed, update both the registry and this list — the
        // test exists precisely to make that coupling visible.
        // Process count grew substantially on 2026-05-25 (cube-metadata
        // ops + standalone scalar math/comparison/logical). Rather than
        // pin a brittle full list, assert (a) the count and (b) that every
        // representative process from each category is present. The full
        // sorted list is still available via registered_processes().
        let procs = r.registered_processes();
        // Spot-check one process per category.
        for must in [
            // pipeline
            "load_collection", "ndvi", "mask", "reduce_dimension", "apply", "save_result", "merge_cubes",
            // cube-metadata (2026-05-25)
            "filter_bands", "rename_labels", "add_dimension", "drop_dimension",
            // scalar math (2026-05-25)
            "absolute", "sqrt", "power", "log", "round", "clip", "normalized_difference",
            // trig
            "cos", "arctan2",
            // comparison + logical
            "eq", "gt", "between", "and", "or", "xor", "not",
        ] {
            assert!(procs.contains(&must), "missing process `{must}` in registry");
        }
        // Sorted + deduplicated invariant.
        let mut sorted = procs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(procs, sorted, "registered_processes must be sorted + unique");
        // Lower-bound on count (catches accidental mass-deletion).
        assert!(procs.len() >= 50, "expected >=50 processes, got {}", procs.len());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_can_be_invoked_through_trait_object() {
        let mut r = ProcessRegistry::new();
        r.register(
            "stub",
            Arc::new(StubHandler {
                reply: Value::from("hello"),
            }),
        );
        let exe = GeoExecutor::new();
        let handler = r.get("stub").expect("stub registered");
        let out = handler.handle(&exe, BTreeMap::new()).await.expect("ok");
        assert_eq!(out, Value::from("hello"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_handler_through_registry_matches_legacy_math() {
        let mut r = ProcessRegistry::new();
        register_defaults(&mut r);
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("x".into(), Value::from(5.0));
        args.insert("y".into(), Value::from(7.0));
        let out = r
            .get("add")
            .expect("add registered")
            .handle(&exe, args)
            .await
            .expect("add ok");
        assert_eq!(out.as_f64().unwrap(), 12.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn divide_by_zero_is_invalid_graph_through_registry() {
        let mut r = ProcessRegistry::new();
        register_defaults(&mut r);
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("x".into(), Value::from(1.0));
        args.insert("y".into(), Value::from(0.0));
        let r = r
            .get("divide")
            .expect("divide registered")
            .handle(&exe, args)
            .await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }
}
