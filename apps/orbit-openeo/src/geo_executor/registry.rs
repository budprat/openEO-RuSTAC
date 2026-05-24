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
}

// ---------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let expected: Vec<&'static str> = vec![
            "add",
            "aggregate_spatial_point",
            "aggregate_spatial_polygon",
            "apply",
            "divide",
            "filter_bbox",
            "filter_spatial",
            "filter_temporal",
            "fit_classifier",
            "load_collection",
            "mask",
            "mask_from_values",
            "mask_scl_dilation",
            "merge_cubes",
            "multiply",
            "ndvi",
            "predict_classifier",
            "reduce_dimension",
            "resample_spatial",
            "save_result",
            "subtract",
            "zonal_histogram",
        ];
        assert_eq!(r.registered_processes(), expected);
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
