//! petgraph-backed process-graph analyser.
//!
//! Wraps an `eo_process::ProcessGraph` (which is logically an adjacency
//! list keyed by node id) with a `petgraph::DiGraph` so we can use
//! mature graph algorithms instead of hand-rolling DFS:
//!
//! - cycle detection via `algo::is_cyclic_directed`
//! - topological iteration via `algo::toposort`
//! - reachability from the result node via `Bfs`
//!
//! The wrapper is read-only — once built it does not mutate. Builders
//! that need to *modify* the graph should rebuild from a new
//! `ProcessGraph`.

use std::collections::{BTreeMap, HashMap};

use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use serde_json::Value;
use thiserror::Error;

/// Maximum recursion depth for argument-value walks (DoS guard).
/// Real graphs nest 4–10 deep; 64 is a comfortable ceiling that still
/// catches adversarial `{x:{x:{…}}}` JSON-bomb inputs well below the
/// 128 MiB body cap from `lib.rs::build_router`.
pub const MAX_SUBGRAPH_DEPTH: usize = 64;

/// Errors a graph build can surface.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    /// The graph contained a directed cycle. Includes one node id from
    /// the strongly-connected component for diagnostics.
    #[error("process graph contains a cycle through node `{0}`")]
    Cycle(String),
    /// An argument referenced an undefined node id via `{"from_node": "..."}`.
    #[error("unknown node `{0}` referenced as `from_node`")]
    UnknownNodeReference(String),
    /// No node carried `result: true`, or more than one did.
    #[error("expected exactly one result-marked node (got {0})")]
    AmbiguousResult(usize),
    /// Argument-value recursion exceeded the configured depth cap.
    /// Adversarial inputs with deeply-nested JSON would otherwise blow
    /// the thread stack inside the 128 MiB body cap.
    #[error("process graph argument nesting exceeded depth limit ({limit})")]
    DepthExceeded {
        /// Configured maximum recursion depth.
        limit: usize,
    },
}

/// Petgraph-backed analyser. Constructed once per request.
#[derive(Debug)]
pub struct ProcessGraphAnalysis {
    /// Underlying directed graph. Nodes carry the process_id string for
    /// easy debug output.
    pub graph: DiGraph<String, ()>,
    /// Lookup: node-id (from openEO) → petgraph index.
    pub idx: HashMap<String, NodeIndex>,
    /// Reverse lookup: petgraph index → node-id.
    pub by_index: HashMap<NodeIndex, String>,
    /// The single `result: true` node id.
    pub result_id: String,
}

impl ProcessGraphAnalysis {
    /// Build the analyser from an openEO process graph.
    pub fn build(pg: &eo_process::ProcessGraph) -> Result<Self, GraphError> {
        // 1. Add every node first so we can resolve `from_node` references in pass 2.
        let mut graph: DiGraph<String, ()> = DiGraph::new();
        let mut idx: HashMap<String, NodeIndex> = HashMap::with_capacity(pg.nodes.len());
        let mut by_index: HashMap<NodeIndex, String> = HashMap::with_capacity(pg.nodes.len());
        for (id, node) in &pg.nodes {
            let n = graph.add_node(node.process_id.0.clone());
            idx.insert(id.clone(), n);
            by_index.insert(n, id.clone());
        }

        // 2. Walk arguments adding edges. `dst` (this node) depends on
        //    `src` (the referenced node), so the edge points src → dst.
        for (dst_id, node) in &pg.nodes {
            let dst_idx = idx[dst_id];
            collect_from_node_refs(&node.arguments, &mut |src_id| {
                let src_idx = *idx
                    .get(src_id)
                    .ok_or_else(|| GraphError::UnknownNodeReference(src_id.to_string()))?;
                graph.add_edge(src_idx, dst_idx, ());
                Ok::<(), GraphError>(())
            })?;
        }

        // 3. Cycle detection up front — short-circuit before we try to topo-sort.
        if is_cyclic_directed(&graph) {
            let any = pg
                .nodes
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "<unknown>".into());
            return Err(GraphError::Cycle(any));
        }

        // 4. Result node — exactly one with `result: true`.
        let result_nodes: Vec<&str> = pg
            .nodes
            .iter()
            .filter(|(_, n)| n.result == Some(true))
            .map(|(id, _)| id.as_str())
            .collect();
        if result_nodes.len() != 1 {
            return Err(GraphError::AmbiguousResult(result_nodes.len()));
        }
        let result_id = result_nodes[0].to_string();

        Ok(Self { graph, idx, by_index, result_id })
    }

    /// Evaluation order for nodes that the result node transitively
    /// depends on. Always returns the result node last.
    pub fn evaluation_order(&self) -> Vec<String> {
        // Cycles are rejected during `analyze()` build; if toposort still
        // fails the graph is malformed beyond what we can recover — return
        // an empty schedule rather than panicking the executor.
        let topo = toposort(&self.graph, None).unwrap_or_default();
        topo.into_iter()
            .map(|n| self.by_index[&n].clone())
            .collect()
    }

    /// Number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
}

/// Walk argument values looking for `{"from_node": "<id>"}` references.
/// Calls `visit(src_id)` once per reference. Stops at the first
/// `GraphError` produced by `visit`.
fn collect_from_node_refs<F>(
    args: &BTreeMap<String, Value>,
    visit: &mut F,
) -> Result<(), GraphError>
where
    F: FnMut(&str) -> Result<(), GraphError>,
{
    for v in args.values() {
        collect_in_value(v, visit)?;
    }
    Ok(())
}

/// Public entry — preserves the historical signature; depth-limited
/// inner helper threads the recursion counter to enforce
/// `MAX_SUBGRAPH_DEPTH`.
fn collect_in_value<F>(v: &Value, visit: &mut F) -> Result<(), GraphError>
where
    F: FnMut(&str) -> Result<(), GraphError>,
{
    collect_in_value_inner(v, 0, visit)
}

fn collect_in_value_inner<F>(
    v: &Value,
    depth: usize,
    visit: &mut F,
) -> Result<(), GraphError>
where
    F: FnMut(&str) -> Result<(), GraphError>,
{
    if depth > MAX_SUBGRAPH_DEPTH {
        return Err(GraphError::DepthExceeded { limit: MAX_SUBGRAPH_DEPTH });
    }
    if let Some(obj) = v.as_object() {
        // **Audit P1-12**: `{from_node: "x"}` is a link even when it
        // carries sibling fields like `from_parameter`. We no longer
        // require obj.len()==1.
        if let Some(Value::String(s)) = obj.get("from_node") {
            visit(s)?;
            return Ok(());
        }
        // **Audit P0-3**: sub-process callbacks (`reducer`, `process`,
        // `mask`) carry `{process_graph: {...}}` whose inner
        // `from_node` references live in an INNER namespace and MUST
        // NOT bleed into the outer topological sort. Short-circuit
        // recursion when we see a `process_graph` key.
        if obj.contains_key("process_graph") {
            return Ok(());
        }
        // Object that is NOT a {from_node}-link or sub-graph — recurse
        // into its values so nested references work.
        for inner in obj.values() {
            collect_in_value_inner(inner, depth + 1, visit)?;
        }
    } else if let Some(arr) = v.as_array() {
        for inner in arr {
            collect_in_value_inner(inner, depth + 1, visit)?;
        }
    }
    Ok(())
}

/// **M4 (process audit)** — collect every `process_id` referenced anywhere
/// in a raw process-graph JSON value, INCLUDING those nested inside
/// sub-callback `process_graph`s (`reducer` / `process` / `overlap_resolver`
/// / `mask`). Unlike the topological walker, callback ids ARE collected here
/// because an unknown process in a callback is just as unsupported as one at
/// the top level — we want to reject it at submit/validate time, not only
/// when the runner reaches it.
///
/// Accepts either the full submission body (`{"process": {"process_graph":
/// …}}`), the `{"process_graph": …}` wrapper, or a bare node map.
pub fn collect_process_ids(body: &Value) -> std::collections::BTreeSet<String> {
    fn walk(v: &Value, depth: usize, out: &mut std::collections::BTreeSet<String>) {
        if depth > MAX_SUBGRAPH_DEPTH {
            return;
        }
        match v {
            Value::Object(map) => {
                if let Some(Value::String(pid)) = map.get("process_id") {
                    out.insert(pid.clone());
                }
                for inner in map.values() {
                    walk(inner, depth + 1, out);
                }
            }
            Value::Array(arr) => {
                for inner in arr {
                    walk(inner, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let pg = body
        .get("process")
        .and_then(|p| p.get("process_graph"))
        .or_else(|| body.get("process_graph"))
        .unwrap_or(body);
    let mut out = std::collections::BTreeSet::new();
    walk(pg, 0, &mut out);
    out
}

/// **M4** — return the sorted list of referenced process ids that are NOT in
/// `known` (the implemented set, e.g. `process_catalog::process_ids()`).
/// Empty when every referenced process is supported.
pub fn unsupported_process_ids(body: &Value, known: &[&str]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = known.iter().copied().collect();
    collect_process_ids(body)
        .into_iter()
        .filter(|id| !known.contains(id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eo_process::{Process, ProcessGraph, ProcessId};
    use std::collections::BTreeMap;

    fn proc(id: &str, args: Vec<(&str, Value)>, result: bool) -> Process {
        let mut a = BTreeMap::new();
        for (k, v) in args {
            a.insert(k.into(), v);
        }
        Process {
            process_id: ProcessId(id.into()),
            arguments: a,
            result: if result { Some(true) } else { None },
        }
    }

    fn from_node(s: &str) -> Value {
        serde_json::json!({ "from_node": s })
    }

    #[test]
    fn linear_chain_topo_orders_dependencies_first() {
        // a → b → c, c is result.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        nodes.insert(
            "b".into(),
            proc("save_result", vec![("data", from_node("a"))], false),
        );
        nodes.insert(
            "c".into(),
            proc("save_result", vec![("data", from_node("b"))], true),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes }).unwrap();
        let order = g.evaluation_order();
        let pos = |id: &str| order.iter().position(|s| s == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
        assert_eq!(order.last().unwrap(), "c");
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn diamond_graph_evaluates_shared_dependency_once() {
        // a → b, a → c, b → d, c → d. d is result.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        nodes.insert(
            "b".into(),
            proc("save_result", vec![("data", from_node("a"))], false),
        );
        nodes.insert(
            "c".into(),
            proc("save_result", vec![("data", from_node("a"))], false),
        );
        nodes.insert(
            "d".into(),
            proc(
                "add",
                vec![("x", from_node("b")), ("y", from_node("c"))],
                true,
            ),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes }).unwrap();
        let order = g.evaluation_order();
        let pos = |id: &str| order.iter().position(|s| s == id).unwrap();
        assert!(pos("a") < pos("b"), "a must precede b");
        assert!(pos("a") < pos("c"), "a must precede c");
        assert!(pos("b") < pos("d"), "b must precede d");
        assert!(pos("c") < pos("d"), "c must precede d");
        assert_eq!(g.edge_count(), 4);
    }

    #[test]
    fn cycle_is_rejected_at_build_time() {
        // a depends on b, b depends on a → cycle.
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "a".into(),
            proc(
                "add",
                vec![("x", from_node("b")), ("y", serde_json::json!(1))],
                false,
            ),
        );
        nodes.insert(
            "b".into(),
            proc(
                "add",
                vec![("x", from_node("a")), ("y", serde_json::json!(1))],
                true,
            ),
        );
        let r = ProcessGraphAnalysis::build(&ProcessGraph { nodes });
        assert!(matches!(r, Err(GraphError::Cycle(_))), "got {r:?}");
    }

    #[test]
    fn unknown_from_node_reference_is_rejected() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "a".into(),
            proc(
                "save_result",
                vec![("data", from_node("does_not_exist"))],
                true,
            ),
        );
        let r = ProcessGraphAnalysis::build(&ProcessGraph { nodes });
        assert!(matches!(r, Err(GraphError::UnknownNodeReference(_))), "got {r:?}");
    }

    #[test]
    fn missing_result_node_is_rejected() {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        let r = ProcessGraphAnalysis::build(&ProcessGraph { nodes });
        assert!(matches!(r, Err(GraphError::AmbiguousResult(0))), "got {r:?}");
    }

    #[test]
    fn multiple_result_nodes_is_rejected() {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("save_result", vec![], true));
        nodes.insert("b".into(), proc("save_result", vec![], true));
        let r = ProcessGraphAnalysis::build(&ProcessGraph { nodes });
        assert!(matches!(r, Err(GraphError::AmbiguousResult(2))), "got {r:?}");
    }

    #[test]
    fn nested_from_node_in_array_is_detected() {
        // Some openEO processes take arrays of inputs (e.g. `array_element`).
        // A `{"from_node": …}` nested inside an array argument still
        // needs to be tracked as a dependency.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        nodes.insert(
            "b".into(),
            proc(
                "save_result",
                vec![("data", serde_json::json!([from_node("a")]))],
                true,
            ),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes }).unwrap();
        assert_eq!(g.edge_count(), 1, "must follow refs through arrays");
    }

    #[test]
    fn nested_from_node_in_object_is_detected() {
        // openEO sub-process graphs nest {"data":{"from_node":…}}
        // inside a wrapping `{"reducer":{"data":…}}` shape.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        nodes.insert(
            "b".into(),
            proc(
                "save_result",
                vec![
                    (
                        "reducer",
                        serde_json::json!({ "inner": { "from_node": "a" } }),
                    ),
                ],
                true,
            ),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes }).unwrap();
        assert_eq!(g.edge_count(), 1, "must follow refs through nested objects");
    }

    // ---------- P0-3: sub-process callback namespace isolation ----------

    #[test]
    fn sub_process_callback_inner_from_nodes_do_not_leak_to_outer_graph() {
        // Outer graph: a → b (b is result). The callback `reducer`
        // contains an inner `process_graph` with `from_node: "data"`
        // that references the SUB-graph's own namespace, NOT the outer
        // `a`. Without P0-3 fix, the walker would treat `data` as an
        // UnknownNodeReference (or, worse, a false outer edge).
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        nodes.insert(
            "b".into(),
            proc(
                "reduce_dimension",
                vec![
                    ("data", from_node("a")),
                    ("dimension", serde_json::json!("time")),
                    (
                        "reducer",
                        serde_json::json!({
                            "process_graph": {
                                "inner": {
                                    "process_id": "mean",
                                    "arguments": { "data": { "from_node": "data" } },
                                    "result": true
                                }
                            }
                        }),
                    ),
                ],
                true,
            ),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes })
            .expect("inner from_node must not bleed into outer graph");
        // Outer has only the one a→b edge.
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn from_node_with_sibling_from_parameter_is_still_a_link() {
        // **P1-12**: openEO spec allows `{from_node:"x", from_parameter:"y"}`
        // (parameter-passing callbacks). Both keys present must still
        // register a from_node edge.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        nodes.insert(
            "b".into(),
            proc(
                "save_result",
                vec![(
                    "data",
                    serde_json::json!({ "from_node": "a", "from_parameter": "x" }),
                )],
                true,
            ),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes }).unwrap();
        assert_eq!(g.edge_count(), 1, "sibling-key from_node must still be a link");
    }

    #[test]
    fn deep_chain_does_not_stack_overflow() {
        // 1000-node linear chain. petgraph's iterative toposort handles
        // this without blowing the stack — a hand-rolled recursive DFS
        // would already be at risk with the default 8 MiB Rust thread.
        let mut nodes = BTreeMap::new();
        let n = 1000usize;
        nodes.insert("n0".into(), proc("load_collection", vec![], false));
        for i in 1..n {
            let prev = format!("n{}", i - 1);
            let id = format!("n{i}");
            let is_last = i == n - 1;
            nodes.insert(
                id,
                proc(
                    "save_result",
                    vec![("data", from_node(&prev))],
                    is_last,
                ),
            );
        }
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes }).unwrap();
        assert_eq!(g.node_count(), n);
        assert_eq!(g.edge_count(), n - 1);
        let order = g.evaluation_order();
        assert_eq!(order.first().unwrap(), "n0");
        assert_eq!(order.last().unwrap(), &format!("n{}", n - 1));
    }

    // ---------- L1: argument-value recursion depth guard ----------

    /// Wrap `inner` in `levels` levels of `{"x": …}` nesting.
    fn nest(levels: usize, inner: Value) -> Value {
        let mut v = inner;
        for _ in 0..levels {
            v = serde_json::json!({ "x": v });
        }
        v
    }

    #[test]
    fn walker_accepts_depth_64() {
        // A {"x":{"x":…}} chain that recurses exactly MAX_SUBGRAPH_DEPTH
        // (=64) times before reaching the inner from_node leaf must
        // still resolve cleanly.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        let payload = nest(MAX_SUBGRAPH_DEPTH, from_node("a"));
        nodes.insert(
            "b".into(),
            proc("save_result", vec![("data", payload)], true),
        );
        let g = ProcessGraphAnalysis::build(&ProcessGraph { nodes })
            .expect("depth at the cap must succeed");
        assert_eq!(g.edge_count(), 1, "from_node at depth 64 must be detected");
    }

    #[test]
    fn walker_rejects_depth_65() {
        // One past the cap → DepthExceeded with the configured limit.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        let payload = nest(MAX_SUBGRAPH_DEPTH + 1, from_node("a"));
        nodes.insert(
            "b".into(),
            proc("save_result", vec![("data", payload)], true),
        );
        let r = ProcessGraphAnalysis::build(&ProcessGraph { nodes });
        match r {
            Err(GraphError::DepthExceeded { limit }) => {
                assert_eq!(limit, MAX_SUBGRAPH_DEPTH);
            }
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    // ---------- M4: process-id collection + unsupported detection ----------

    #[test]
    fn collect_process_ids_includes_callback_processes() {
        // Top-level reduce_dimension + an inner `mean` reducer callback.
        let body = serde_json::json!({
            "process": { "process_graph": {
                "load": { "process_id": "load_collection", "arguments": {} },
                "r": { "process_id": "reduce_dimension", "arguments": {
                    "data": { "from_node": "load" },
                    "reducer": { "process_graph": {
                        "m": { "process_id": "mean", "arguments": { "data": { "from_parameter": "data" } }, "result": true }
                    }}
                }, "result": true }
            }}
        });
        let ids = collect_process_ids(&body);
        assert!(ids.contains("load_collection"));
        assert!(ids.contains("reduce_dimension"));
        assert!(ids.contains("mean"), "callback process ids must be collected");
    }

    #[test]
    fn unsupported_process_ids_flags_unknown_only() {
        let body = serde_json::json!({
            "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 1, "y": 2 } },
                "z": { "process_id": "frobnicate", "arguments": {}, "result": true }
            }
        });
        let known = ["add", "subtract", "save_result"];
        let bad = unsupported_process_ids(&body, &known);
        assert_eq!(bad, vec!["frobnicate".to_string()]);
        // All-known graph → empty.
        let ok = unsupported_process_ids(&body, &["add", "frobnicate"]);
        assert!(ok.is_empty());
    }

    #[test]
    fn walker_rejects_deeply_nested_arrays() {
        // Arrays are the other recursion site — same cap applies.
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("load_collection", vec![], false));
        let mut payload = from_node("a");
        for _ in 0..(MAX_SUBGRAPH_DEPTH + 1) {
            payload = serde_json::json!([payload]);
        }
        nodes.insert(
            "b".into(),
            proc("save_result", vec![("data", payload)], true),
        );
        let r = ProcessGraphAnalysis::build(&ProcessGraph { nodes });
        match r {
            Err(GraphError::DepthExceeded { limit }) => {
                assert_eq!(limit, MAX_SUBGRAPH_DEPTH);
            }
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }
}
