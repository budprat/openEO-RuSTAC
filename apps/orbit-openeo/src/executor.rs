//! Process-graph executor trait.
//!
//! The HTTP layer dispatches `POST /result` (sync compute) and
//! `POST /jobs/{id}/results` (kick off a batch) through this trait so the
//! router code doesn't bind to a specific backend.
//!
//! Today we ship:
//! - [`EchoExecutor`] — returns the input graph back as JSON. Useful for
//!   smoke tests and for clients that just want to round-trip a graph.
//!
//! A future session adds a `LocalExecutor` that walks the AST in
//! `eo_process` and dispatches to `eo-kernel` / `eo-catalog` /
//! `eo-mask` etc.

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

/// Errors a process-graph execution can surface.
#[derive(Debug, Error)]
pub enum ExecError {
    /// The body wasn't a valid openEO process-graph wrapper.
    #[error("invalid process graph: {0}")]
    InvalidGraph(String),
    /// The graph referenced a process the backend doesn't implement.
    #[error("unknown process: {0}")]
    UnknownProcess(String),
    /// Backend-specific failure during evaluation.
    #[error("backend error: {0}")]
    Backend(String),
    /// B4: per-pixel computation failure during `apply` sub-graph evaluation
    /// (e.g. divide-by-zero on a specific pixel value). Swallowed by the
    /// per-pixel loop and converted to NA; structural errors (unknown process,
    /// cycle, bad graph shape) use `InvalidGraph` and propagate instead.
    #[error("per-pixel computation error: {0}")]
    PerPixelComputation(String),
}

/// Synchronous-style result envelope. Real backends return binary blobs
/// (a GeoTIFF, a NetCDF). For the in-process executor we keep it as a
/// JSON `Value` so the HTTP layer can decide how to serialise.
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// Result media type (RFC 6838). Defaults to `application/json`.
    pub content_type: String,
    /// Result payload.
    pub body: Vec<u8>,
}

impl SyncResult {
    /// JSON helper.
    #[must_use]
    pub fn json(v: &Value) -> Self {
        Self {
            content_type: "application/json".into(),
            body: serde_json::to_vec(v).unwrap_or_else(|_| b"null".to_vec()),
        }
    }
}

/// Process-graph execution surface.
#[async_trait]
pub trait ProcessGraphExecutor: Send + Sync {
    /// Run the graph synchronously, returning a single result envelope.
    /// Maps onto `POST /result`.
    async fn run_sync(&self, body: &Value) -> Result<SyncResult, ExecError>;

    /// Enqueue the graph for asynchronous execution; returns a job id.
    /// Maps onto `POST /jobs` + `POST /jobs/{id}/results`.
    async fn enqueue(&self, body: &Value) -> Result<String, ExecError>;
}

/// Echo executor — returns the input graph back as the result.
///
/// Trivial, dependency-free, sufficient for the openEO client SDK's
/// connection-test pattern.
#[derive(Debug, Default, Clone, Copy)]
pub struct EchoExecutor;

#[async_trait]
impl ProcessGraphExecutor for EchoExecutor {
    async fn run_sync(&self, body: &Value) -> Result<SyncResult, ExecError> {
        validate_process_wrapper(body)?;
        Ok(SyncResult::json(body))
    }

    async fn enqueue(&self, body: &Value) -> Result<String, ExecError> {
        validate_process_wrapper(body)?;
        // Deterministic synthetic id for tests; in real use this is a UUID.
        Ok(format!("job-{:08x}", deterministic_hash(body)))
    }
}

/// Verify the JSON has the openEO `process.process_graph` shape.
fn validate_process_wrapper(body: &Value) -> Result<(), ExecError> {
    let pg = body
        .get("process")
        .and_then(|p| p.get("process_graph"))
        .or_else(|| body.get("process_graph"));
    match pg {
        Some(Value::Object(m)) if !m.is_empty() => Ok(()),
        Some(_) => Err(ExecError::InvalidGraph("process_graph is not a non-empty object".into())),
        None => Err(ExecError::InvalidGraph("missing process.process_graph".into())),
    }
}

fn deterministic_hash(v: &Value) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let s = v.to_string();
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// LocalExecutor — walks an `eo_process::ProcessGraph` AST in-process,
/// dispatching by `process_id`. Unknown processes return [`ExecError::UnknownProcess`].
///
/// Today's process catalogue is intentionally tiny — it covers the four
/// flows the openEO client SDK exercises against any backend's
/// connection-test:
///
/// - `load_collection` — returns a `{"type":"DataCube"}` shaped sentinel.
/// - `save_result`     — passes the upstream result through.
/// - `add`             — sums numeric `x` + `y` arguments.
/// - `subtract`        — `x - y`.
///
/// Real numerical kernels arrive when `eo-kernel` lands; this skeleton
/// proves the graph-walk is correct and lets the `/result` route return
/// computed values rather than echoing.
pub struct LocalExecutor {
    /// Optional catalog backend. When set, `load_collection` consults it
    /// to confirm the requested collection exists; unknown collections
    /// fail with `ExecError::Backend(CollectionNotFound)`. When `None`,
    /// `load_collection` returns the legacy sentinel without I/O.
    catalog: Option<std::sync::Arc<dyn crate::catalog::CollectionCatalog>>,
}

impl LocalExecutor {
    /// New executor with no catalog (legacy sentinel behaviour).
    #[must_use]
    pub fn new() -> Self { Self { catalog: None } }

    /// New executor that validates `load_collection` against the supplied
    /// catalog at dispatch time.
    #[must_use]
    pub fn with_catalog(catalog: std::sync::Arc<dyn crate::catalog::CollectionCatalog>) -> Self {
        Self { catalog: Some(catalog) }
    }

    fn parse_graph(&self, body: &Value) -> Result<eo_process::ProcessGraph, ExecError> {
        let pg_val = body
            .get("process")
            .and_then(|p| p.get("process_graph"))
            .or_else(|| body.get("process_graph"))
            .ok_or_else(|| ExecError::InvalidGraph("missing process.process_graph".into()))?;
        let nodes: std::collections::BTreeMap<String, eo_process::Process> =
            serde_json::from_value(pg_val.clone())
                .map_err(|e| ExecError::InvalidGraph(e.to_string()))?;
        if nodes.is_empty() {
            return Err(ExecError::InvalidGraph("process_graph is empty".into()));
        }
        Ok(eo_process::ProcessGraph { nodes })
    }

    /// Walk the graph and return the value at the result node.
    ///
    /// Uses `process_graph::ProcessGraphAnalysis` (petgraph-backed) for
    /// cycle detection + topological ordering. Then evaluates each node
    /// in dependency order, memoising results so diamond-shaped graphs
    /// don't re-execute shared upstreams.
    pub async fn evaluate(&self, graph: &eo_process::ProcessGraph) -> Result<Value, ExecError> {
        let analysis = crate::process_graph::ProcessGraphAnalysis::build(graph)
            .map_err(|e| ExecError::InvalidGraph(e.to_string()))?;
        let order = analysis.evaluation_order();
        let mut memo: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        for node_id in &order {
            let node = graph
                .nodes
                .get(node_id)
                .ok_or_else(|| ExecError::InvalidGraph(format!("unknown node `{node_id}`")))?;

            // Resolve any `from_node`-linked argument by reading the memo
            // for the upstream node (we processed it earlier in topo order).
            let mut resolved_args = std::collections::BTreeMap::new();
            for (k, v) in &node.arguments {
                let r = resolve_value(v, &memo)?;
                resolved_args.insert(k.clone(), r);
            }

            let out = match node.process_id.0.as_str() {
                "load_collection" => {
                    let id_arg = resolved_args
                        .get("id")
                        .cloned()
                        .unwrap_or(Value::Null);
                    // If a catalog is wired, confirm the collection exists
                    // and surface its metadata in the sentinel. Unknown
                    // collections fail loudly so client SDKs see a typed
                    // error rather than a meaningless DataCube stub.
                    if let Some(cat) = &self.catalog {
                        let id_str = id_arg
                            .as_str()
                            .ok_or_else(|| ExecError::InvalidGraph(
                                "load_collection: `id` argument must be a string".into(),
                            ))?
                            .to_string();
                        match cat.get(&id_str).await {
                            Ok(meta) => {
                                let mut sentinel = serde_json::Map::new();
                                sentinel.insert("type".into(), Value::String("DataCube".into()));
                                sentinel.insert("collection".into(), id_arg);
                                sentinel.insert(
                                    "stac_version".into(),
                                    Value::String(meta.stac_version),
                                );
                                if !meta.description.is_empty() {
                                    sentinel.insert("description".into(), Value::String(meta.description));
                                }
                                Value::Object(sentinel)
                            }
                            Err(crate::catalog::CatalogError::NotFound(id)) => {
                                return Err(ExecError::Backend(format!(
                                    "CollectionNotFound: {id}"
                                )));
                            }
                            Err(e) => {
                                return Err(ExecError::Backend(format!(
                                    "catalog: {e}"
                                )));
                            }
                        }
                    } else {
                        serde_json::json!({ "type": "DataCube", "collection": id_arg })
                    }
                }
                "save_result" => resolved_args
                    .get("data")
                    .cloned()
                    .unwrap_or(Value::Null),
                "add" => {
                    let x = number(resolved_args.get("x"))?;
                    let y = number(resolved_args.get("y"))?;
                    Value::from(x + y)
                }
                "subtract" => {
                    let x = number(resolved_args.get("x"))?;
                    let y = number(resolved_args.get("y"))?;
                    Value::from(x - y)
                }
                "multiply" => {
                    let x = number(resolved_args.get("x"))?;
                    let y = number(resolved_args.get("y"))?;
                    Value::from(x * y)
                }
                "divide" => {
                    let x = number(resolved_args.get("x"))?;
                    let y = number(resolved_args.get("y"))?;
                    if y == 0.0 {
                        return Err(ExecError::InvalidGraph(
                            "divide: y must be non-zero".into(),
                        ));
                    }
                    Value::from(x / y)
                }
                other => return Err(ExecError::UnknownProcess(other.into())),
            };
            memo.insert(node_id.clone(), out);
        }
        memo.remove(&analysis.result_id)
            .ok_or_else(|| ExecError::InvalidGraph(
                "result node was not visited during topo walk".into(),
            ))
    }
}

/// Resolve a single argument value. If it's a `{"from_node": "x"}`
/// link, look up the memo. Otherwise pass through unchanged.
fn resolve_value(
    v: &Value,
    memo: &std::collections::HashMap<String, Value>,
) -> Result<Value, ExecError> {
    if let Some(obj) = v.as_object() {
        if obj.len() == 1 {
            if let Some(Value::String(target)) = obj.get("from_node") {
                return memo
                    .get(target)
                    .cloned()
                    .ok_or_else(|| ExecError::InvalidGraph(format!(
                        "upstream node `{target}` not yet evaluated"
                    )));
            }
        }
    }
    Ok(v.clone())
}

fn number(opt: Option<&Value>) -> Result<f64, ExecError> {
    opt.and_then(|v| v.as_f64())
        .ok_or_else(|| ExecError::InvalidGraph("expected numeric argument".into()))
}

impl Default for LocalExecutor {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl ProcessGraphExecutor for LocalExecutor {
    async fn run_sync(&self, body: &Value) -> Result<SyncResult, ExecError> {
        let graph = self.parse_graph(body)?;
        let value = self.evaluate(&graph).await?;
        Ok(SyncResult::json(&value))
    }

    async fn enqueue(&self, body: &Value) -> Result<String, ExecError> {
        // Validate by parsing; the engine bridge in a later session
        // takes over dispatch.
        let _ = self.parse_graph(body)?;
        Ok(format!("job-{:08x}", deterministic_hash(body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn echo_sync_returns_input() {
        let e = EchoExecutor;
        let body = json!({
            "process": { "process_graph": { "load1": { "process_id": "load_collection" } } }
        });
        let r = e.run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "application/json");
        let back: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(back, body);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn echo_enqueue_returns_job_id() {
        let e = EchoExecutor;
        let body = json!({ "process": { "process_graph": { "x": {} } } });
        let id = e.enqueue(&body).await.unwrap();
        assert!(id.starts_with("job-"), "got {id}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn echo_rejects_missing_process_graph() {
        let e = EchoExecutor;
        let r = e.run_sync(&json!({})).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn echo_rejects_empty_process_graph() {
        let e = EchoExecutor;
        let r = e.run_sync(&json!({"process":{"process_graph":{}}})).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn echo_accepts_bare_process_graph() {
        let e = EchoExecutor;
        let r = e.run_sync(&json!({"process_graph":{"a":{}}})).await;
        assert!(r.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_is_deterministic() {
        let e = EchoExecutor;
        let body = json!({"process":{"process_graph":{"x":{}}}});
        let a = e.enqueue(&body).await.unwrap();
        let b = e.enqueue(&body).await.unwrap();
        assert_eq!(a, b);
    }

    // ------------------------------------------------------------------
    // LocalExecutor — graph-walking executor.
    // ------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn local_executes_add_through_save_result() {
        let body = json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 2, "y": 3 } },
                "b": {
                    "process_id": "save_result",
                    "arguments": { "data": { "from_node": "a" } },
                    "result": true
                }
            }}
        });
        let r = LocalExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v.as_f64().unwrap(), 5.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_subtract_works() {
        let body = json!({
            "process": { "process_graph": {
                "s": { "process_id": "subtract", "arguments": { "x": 10, "y": 7 }, "result": true }
            }}
        });
        let v: Value = serde_json::from_slice(&LocalExecutor::new().run_sync(&body).await.unwrap().body).unwrap();
        assert_eq!(v.as_f64().unwrap(), 3.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_load_collection_returns_datacube_sentinel() {
        let body = json!({
            "process": { "process_graph": {
                "load": {
                    "process_id": "load_collection",
                    "arguments": { "id": "SENTINEL2_L2A" },
                    "result": true
                }
            }}
        });
        let v: Value = serde_json::from_slice(&LocalExecutor::new().run_sync(&body).await.unwrap().body).unwrap();
        assert_eq!(v["type"], "DataCube");
        assert_eq!(v["collection"], "SENTINEL2_L2A");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_unknown_process_returns_error() {
        let body = json!({
            "process": { "process_graph": {
                "x": { "process_id": "unknown_thing", "result": true }
            }}
        });
        let r = LocalExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::UnknownProcess(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_rejects_graph_without_result_node() {
        let body = json!({
            "process": { "process_graph": {
                "x": { "process_id": "add", "arguments": { "x": 1, "y": 2 } }
            }}
        });
        let r = LocalExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_detects_cycles() {
        let body = json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": { "from_node": "b" }, "y": 1 } },
                "b": { "process_id": "add", "arguments": { "x": { "from_node": "a" }, "y": 1 }, "result": true }
            }}
        });
        let r = LocalExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_enqueue_validates_graph() {
        let r = LocalExecutor::new().enqueue(&json!({})).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    // ------------------------------------------------------------------
    // D1 — load_collection consults the catalog when wired.
    // ------------------------------------------------------------------

    use crate::catalog::{Collection, CollectionCatalog, InMemoryCatalog};
    use std::sync::Arc;

    fn graph_with_load(id_val: serde_json::Value) -> Value {
        json!({
            "process": { "process_graph": {
                "l": { "process_id": "load_collection",
                       "arguments": { "id": id_val },
                       "result": true }
            }}
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_without_catalog_returns_sentinel() {
        // Backwards-compat: no catalog → legacy {"type":"DataCube"} sentinel.
        let body = graph_with_load(json!("sentinel-2-l2a"));
        let r = LocalExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["type"], "DataCube");
        assert_eq!(v["collection"], "sentinel-2-l2a");
        assert!(v.get("stac_version").is_none(), "no catalog → no metadata");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_with_catalog_known_collection_includes_metadata() {
        let cat: Arc<dyn CollectionCatalog> = Arc::new(InMemoryCatalog::with_collections(vec![
            Collection {
                id: "sentinel-2-l2a".into(),
                stac_version: "1.0.0".into(),
                description: "S2 L2A".into(),
                license: "proprietary".into(),
                extra: Default::default(),
            },
        ]));
        let body = graph_with_load(json!("sentinel-2-l2a"));
        let r = LocalExecutor::with_catalog(cat).run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["type"], "DataCube");
        assert_eq!(v["collection"], "sentinel-2-l2a");
        assert_eq!(v["stac_version"], "1.0.0");
        assert_eq!(v["description"], "S2 L2A");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_with_catalog_unknown_collection_fails_backend() {
        let cat: Arc<dyn CollectionCatalog> = Arc::new(InMemoryCatalog::empty());
        let body = graph_with_load(json!("does-not-exist"));
        let r = LocalExecutor::with_catalog(cat).run_sync(&body).await;
        match r {
            Err(ExecError::Backend(m)) => {
                assert!(m.contains("CollectionNotFound"), "got: {m}");
                assert!(m.contains("does-not-exist"), "got: {m}");
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_with_catalog_non_string_id_fails_invalid_graph() {
        let cat: Arc<dyn CollectionCatalog> = Arc::new(InMemoryCatalog::empty());
        let body = graph_with_load(json!(42));
        let r = LocalExecutor::with_catalog(cat).run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }
}
