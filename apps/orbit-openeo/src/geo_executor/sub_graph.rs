//! Sub-graph evaluator trait + shared extraction helpers for openEO
//! process sub-callbacks (the inner `process_graph` blob nested inside
//! arguments like `reducer.process_graph`, `apply.process.process_graph`,
//! mask callbacks, etc).
//!
//! ## Why a shared module
//!
//! Three sites used to hand-roll the same three steps:
//!   1. Pull `process_graph` out of the wrapping `{"process_graph": {…}}`
//!      envelope.
//!   2. Walk its nodes looking for the single `result: true` node.
//!   3. Pull the result node's `process_id`.
//!
//! Each site enforced the **A4+A5** invariant ("real sub-callback, not a
//! metadata pass-through") with a slightly different error message and a
//! slightly different shape check. This module centralises the contract so
//! the invariants live in one place.
//!
//! The outer-walker **P0-3** short-circuit (sub-callback `process_graph`
//! keys must not leak into the outer topological sort) lives in
//! `process_graph.rs::collect_in_value` and is unchanged by this module —
//! we only deal with the *inner* world after that short-circuit has fired.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

use crate::executor::ExecError;
use crate::process_graph::MAX_SUBGRAPH_DEPTH;

/// Result of evaluating a sub-callback against its inner-parameter bindings.
// Intentional shared-infra scaffold: exercised by this module's tests but not
// yet wired into the non-test binary (the per-pixel evaluators in eval_apply /
// eval_reduce inline their own walk for performance). Retained for reuse.
#[allow(dead_code)]
pub type SubGraphResult = Result<Value, ExecError>;

/// Evaluator for openEO process sub-graphs.
///
/// `eval_apply` is the only call site today that needs a full per-node
/// interpreter (it runs the sub-graph per pixel). The reducer-name lookup
/// in `eval_reduce` does NOT need this trait — it only needs the shared
/// `require_subgraph` + `find_unique_result_node` helpers below.
///
/// Reason: keeping the trait surface minimal avoids forcing the reducer
/// path through a per-pixel `Value` round-trip just for the sake of
/// reuse — that would regress performance for zero invariant payoff.
#[allow(dead_code)]
pub trait SubGraphEvaluator {
    /// Evaluate the sub-graph rooted at `graph` against inner-parameter
    /// bindings `params`. The conventional inner-param key is `"data"`
    /// for reducers and `"x"` for per-pixel `apply`.
    fn evaluate_subgraph(
        &self,
        graph: &Value,
        params: BTreeMap<String, Value>,
    ) -> SubGraphResult;
}

/// Validate that `value` contains a real `process_graph` object (not just
/// a metadata pass-through). Returns the inner graph `Map`.
///
/// **A4+A5 invariant**: sub-callbacks like `reducer` MUST carry a real
/// `process_graph` — `reduce_dimension` without one is rejected here
/// (was rejected per-site before this refactor).
pub fn require_subgraph<'a>(
    value: &'a Value,
    field_name: &str,
) -> Result<&'a Map<String, Value>, ExecError> {
    let pg = value
        .get("process_graph")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            ExecError::InvalidGraph(format!(
                "{field_name}: must be a Process with `process_graph` sub-callback"
            ))
        })?;
    if pg.is_empty() {
        return Err(ExecError::InvalidGraph(format!(
            "{field_name}: process_graph is empty"
        )));
    }
    Ok(pg)
}

/// Find the single `result: true` node in a sub-graph's node map and
/// return `(id, node)`. Returns `InvalidGraph` if no result node exists
/// or if more than one is marked.
///
/// Centralises the "exactly one result" rule that was previously
/// open-coded in eval_apply + eval_reduce with subtly different error
/// strings.
pub fn find_unique_result_node<'a>(
    graph: &'a Map<String, Value>,
    field_name: &str,
) -> Result<(&'a str, &'a Value), ExecError> {
    let mut result: Option<(&str, &Value)> = None;
    for (id, node) in graph {
        if node
            .get("result")
            .and_then(|r| r.as_bool())
            .unwrap_or(false)
        {
            if result.is_some() {
                return Err(ExecError::InvalidGraph(format!(
                    "{field_name}: process_graph has multiple result nodes"
                )));
            }
            result = Some((id.as_str(), node));
        }
    }
    result.ok_or_else(|| {
        ExecError::InvalidGraph(format!(
            "{field_name}: process_graph has no `result: true` node"
        ))
    })
}

/// Pull the `process_id` string from a sub-graph result node.
///
/// Returns `InvalidGraph` if the field is missing or non-string.
pub fn result_process_id<'a>(
    node: &'a Value,
    field_name: &str,
) -> Result<&'a str, ExecError> {
    node.get("process_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ExecError::InvalidGraph(format!(
                "{field_name}: result node has no `process_id`"
            ))
        })
}

// ---------------------------------------------------------------------
// Default per-pixel evaluator
// ---------------------------------------------------------------------

/// Canonical `SubGraphEvaluator` impl: walks an openEO sub-graph node by
/// node, resolving `from_parameter` against an inner-namespace
/// `params` map and `from_node` references against memoised intermediate
/// values.
///
/// Used by `eval_apply` to thread per-pixel f64 values into the sub-graph
/// via `params = {"x": v}`. The smoke-test path in `eval_apply` (one
/// throwaway pixel evaluation to surface graph-shape errors before any
/// I/O) goes through this exact impl.
#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)]
pub struct DefaultSubGraphEvaluator;

#[allow(dead_code)]
impl SubGraphEvaluator for DefaultSubGraphEvaluator {
    fn evaluate_subgraph(
        &self,
        graph: &Value,
        params: BTreeMap<String, Value>,
    ) -> SubGraphResult {
        // Accept either the wrapped `{"process_graph": {…}}` envelope or
        // the bare inner node map — eval_apply calls us with the inner
        // map already extracted (legacy shape), the reducer parser calls
        // us with the wrapper.
        let nodes_map: &Map<String, Value> = if let Some(obj) = graph.as_object() {
            if obj.contains_key("process_graph") {
                require_subgraph(graph, "subgraph")?
            } else {
                obj
            }
        } else {
            return Err(ExecError::InvalidGraph(
                "subgraph: process_graph must be an object".into(),
            ));
        };

        let (result_id, _) = find_unique_result_node(nodes_map, "subgraph")?;

        let mut memo: std::collections::HashMap<String, Value> =
            std::collections::HashMap::new();
        let mut in_progress: Vec<String> = Vec::new();
        let result_owned = result_id.to_string();
        // Per-sub-graph depth budget — restarts at 0 on every entry so a
        // legitimately deep sub-graph nested inside an outer walk is not
        // penalised by the outer walker's separate budget.
        eval_node_value(&result_owned, nodes_map, &params, &mut memo, &mut in_progress, 0)
    }
}

#[allow(dead_code)]
fn eval_node_value(
    id: &str,
    nodes: &Map<String, Value>,
    params: &BTreeMap<String, Value>,
    memo: &mut std::collections::HashMap<String, Value>,
    in_progress: &mut Vec<String>,
    depth: usize,
) -> SubGraphResult {
    if depth > MAX_SUBGRAPH_DEPTH {
        return Err(ExecError::InvalidGraph(format!(
            "subgraph: argument/from_node recursion exceeded depth limit ({MAX_SUBGRAPH_DEPTH})"
        )));
    }
    if let Some(v) = memo.get(id) {
        return Ok(v.clone());
    }
    if in_progress.iter().any(|n| n == id) {
        return Err(ExecError::InvalidGraph(format!(
            "subgraph: cycle detected at node `{id}`"
        )));
    }
    in_progress.push(id.to_string());
    let node = nodes.get(id).ok_or_else(|| {
        ExecError::InvalidGraph(format!("subgraph: unknown node reference `{id}`"))
    })?;
    let pid = node
        .get("process_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ExecError::InvalidGraph(format!("subgraph: node `{id}` has no process_id"))
        })?
        .to_string();
    let args = node
        .get("arguments")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            ExecError::InvalidGraph(format!(
                "subgraph: node `{id}` has no arguments object"
            ))
        })?;

    // Resolve every argument to a concrete Value first.
    let mut resolved: BTreeMap<String, Value> = BTreeMap::new();
    for (k, v) in args {
        let r = resolve_arg(v, nodes, params, memo, in_progress, depth + 1)?;
        resolved.insert(k.clone(), r);
    }

    // Apply the (small) numeric kernel. For unknown processes, the caller
    // is expected to either pre-validate via eval_apply's per-pixel path
    // or extend this dispatch — keeping the kernel small avoids
    // ballooning duplicated logic with eval_apply.rs's own kernel.
    let value = apply_kernel(&pid, &resolved)?;
    in_progress.pop();
    memo.insert(id.to_string(), value.clone());
    Ok(value)
}

#[allow(dead_code)]
fn resolve_arg(
    v: &Value,
    nodes: &Map<String, Value>,
    params: &BTreeMap<String, Value>,
    memo: &mut std::collections::HashMap<String, Value>,
    in_progress: &mut Vec<String>,
    depth: usize,
) -> SubGraphResult {
    if depth > MAX_SUBGRAPH_DEPTH {
        return Err(ExecError::InvalidGraph(format!(
            "subgraph: argument/from_node recursion exceeded depth limit ({MAX_SUBGRAPH_DEPTH})"
        )));
    }
    if let Some(obj) = v.as_object() {
        // `from_parameter` → inner-namespace binding.
        if let Some(Value::String(p)) = obj.get("from_parameter") {
            return params.get(p).cloned().ok_or_else(|| {
                ExecError::InvalidGraph(format!(
                    "subgraph: unbound parameter `{p}` (bindings: {:?})",
                    params.keys().collect::<Vec<_>>()
                ))
            });
        }
        // `from_node` → recurse into the named node.
        if let Some(Value::String(target)) = obj.get("from_node") {
            return eval_node_value(target, nodes, params, memo, in_progress, depth + 1);
        }
    }
    // Literal scalar / array / object → pass through.
    Ok(v.clone())
}

/// Minimal numeric kernel for the default evaluator: just enough to
/// support the openEO statistical reducers when wrapped as a sub-graph
/// (mean of an f64 array, count, etc) so the trait has a working
/// canonical impl beyond unit tests. eval_apply.rs keeps its own
/// per-pixel kernel for the scalar arithmetic dispatch — overlap is
/// intentional and small (under a dozen processes).
#[allow(dead_code)]
fn apply_kernel(pid: &str, args: &BTreeMap<String, Value>) -> SubGraphResult {
    let data_arr = || -> Result<Vec<f64>, ExecError> {
        let d = args.get("data").ok_or_else(|| {
            ExecError::InvalidGraph(format!("subgraph: `{pid}` needs `data`"))
        })?;
        let arr = d.as_array().ok_or_else(|| {
            ExecError::InvalidGraph(format!(
                "subgraph: `{pid}.data` expected array, got {d}"
            ))
        })?;
        arr.iter()
            .map(|v| {
                v.as_f64().ok_or_else(|| {
                    ExecError::InvalidGraph(format!(
                        "subgraph: `{pid}.data` element not numeric: {v}"
                    ))
                })
            })
            .collect()
    };
    Ok(match pid {
        "mean" => {
            let xs = data_arr()?;
            if xs.is_empty() {
                return Err(ExecError::InvalidGraph("subgraph: mean of empty array".into()));
            }
            let s: f64 = xs.iter().sum();
            Value::from(s / xs.len() as f64)
        }
        "sum" => {
            let xs = data_arr()?;
            Value::from(xs.iter().sum::<f64>())
        }
        "min" => {
            let xs = data_arr()?;
            if xs.is_empty() {
                return Err(ExecError::InvalidGraph("subgraph: min of empty array".into()));
            }
            Value::from(xs.iter().copied().fold(f64::INFINITY, f64::min))
        }
        "max" => {
            let xs = data_arr()?;
            if xs.is_empty() {
                return Err(ExecError::InvalidGraph("subgraph: max of empty array".into()));
            }
            Value::from(xs.iter().copied().fold(f64::NEG_INFINITY, f64::max))
        }
        "count" => {
            let xs = data_arr()?;
            Value::from(xs.len() as f64)
        }
        other => {
            return Err(ExecError::InvalidGraph(format!(
                "subgraph: unsupported process `{other}` in DefaultSubGraphEvaluator \
                 (this kernel covers reducer-style processes; per-pixel arithmetic \
                 lives in eval_apply's own kernel)"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- require_subgraph ----------

    #[test]
    fn require_subgraph_returns_inner_graph_on_valid() {
        let v = json!({
            "process_graph": {
                "m": {
                    "process_id": "mean",
                    "arguments": {"data": {"from_parameter": "data"}},
                    "result": true
                }
            }
        });
        let pg = require_subgraph(&v, "reducer").expect("valid sub-graph");
        assert_eq!(pg.len(), 1);
        assert!(pg.contains_key("m"));
    }

    #[test]
    fn require_subgraph_errors_on_missing_process_graph() {
        let v = json!({"other": "stuff"});
        let r = require_subgraph(&v, "reducer");
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "got {r:?}");
    }

    #[test]
    fn require_subgraph_error_message_includes_field_name() {
        let v = json!({"other": "stuff"});
        let r = require_subgraph(&v, "myCallback");
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(m.contains("myCallback"), "expected field name in error, got `{m}`");
            }
            other => panic!("expected InvalidGraph, got {other:?}"),
        }
    }

    #[test]
    fn require_subgraph_errors_on_empty_process_graph() {
        let v = json!({"process_graph": {}});
        let r = require_subgraph(&v, "reducer");
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(m.contains("empty"), "expected `empty` in error, got `{m}`");
            }
            other => panic!("expected InvalidGraph, got {other:?}"),
        }
    }

    // ---------- find_unique_result_node ----------

    #[test]
    fn find_unique_result_node_returns_the_one_marked_node() {
        let v = json!({
            "process_graph": {
                "a": {"process_id": "load", "arguments": {}},
                "b": {"process_id": "mean", "arguments": {}, "result": true}
            }
        });
        let pg = require_subgraph(&v, "r").expect("valid");
        let (id, node) = find_unique_result_node(pg, "r").expect("unique result");
        assert_eq!(id, "b");
        assert_eq!(node.get("process_id").and_then(|v| v.as_str()), Some("mean"));
    }

    #[test]
    fn find_unique_result_node_rejects_zero_results() {
        let v = json!({
            "process_graph": {"a": {"process_id": "mean", "arguments": {}}}
        });
        let pg = require_subgraph(&v, "r").expect("valid");
        let r = find_unique_result_node(pg, "r");
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(m.contains("no `result: true`"), "got `{m}`");
            }
            other => panic!("expected InvalidGraph, got {other:?}"),
        }
    }

    #[test]
    fn find_unique_result_node_rejects_multiple_results() {
        let v = json!({
            "process_graph": {
                "a": {"process_id": "mean", "arguments": {}, "result": true},
                "b": {"process_id": "min", "arguments": {}, "result": true}
            }
        });
        let pg = require_subgraph(&v, "r").expect("valid");
        let r = find_unique_result_node(pg, "r");
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(m.contains("multiple"), "got `{m}`");
            }
            other => panic!("expected InvalidGraph, got {other:?}"),
        }
    }

    // ---------- result_process_id ----------

    #[test]
    fn result_process_id_pulls_the_string() {
        let n = json!({"process_id": "mean", "arguments": {}, "result": true});
        assert_eq!(result_process_id(&n, "r").expect("ok"), "mean");
    }

    #[test]
    fn result_process_id_rejects_missing_field() {
        let n = json!({"arguments": {}, "result": true});
        let r = result_process_id(&n, "myField");
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(m.contains("myField"), "expected field name, got `{m}`");
                assert!(m.contains("process_id"), "expected mention of process_id, got `{m}`");
            }
            other => panic!("expected InvalidGraph, got {other:?}"),
        }
    }

    // ---------- DefaultSubGraphEvaluator ----------

    #[test]
    fn default_evaluator_runs_mean_reducer() {
        // Wrapped envelope shape (what reducer parsers see).
        let pg = json!({
            "process_graph": {
                "m": {
                    "process_id": "mean",
                    "arguments": {"data": {"from_parameter": "data"}},
                    "result": true
                }
            }
        });
        let mut params = BTreeMap::new();
        params.insert("data".into(), json!([1.0, 2.0, 3.0]));
        let out = DefaultSubGraphEvaluator
            .evaluate_subgraph(&pg, params)
            .expect("eval");
        assert!((out.as_f64().expect("numeric") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn default_evaluator_accepts_bare_node_map() {
        // Bare inner-node map (legacy shape eval_apply uses internally).
        let pg = json!({
            "s": {
                "process_id": "sum",
                "arguments": {"data": {"from_parameter": "data"}},
                "result": true
            }
        });
        let mut params = BTreeMap::new();
        params.insert("data".into(), json!([10.0, 20.0]));
        let out = DefaultSubGraphEvaluator
            .evaluate_subgraph(&pg, params)
            .expect("eval");
        assert!((out.as_f64().expect("numeric") - 30.0).abs() < 1e-9);
    }

    #[test]
    fn default_evaluator_rejects_missing_subgraph() {
        let pg = json!({"not_a_graph": "x"});
        // Bare map without process_id field → eval will fail when it tries
        // to find a result node.
        let r = DefaultSubGraphEvaluator
            .evaluate_subgraph(&pg, BTreeMap::new());
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "got {r:?}");
    }

    #[test]
    fn default_evaluator_propagates_unbound_parameter() {
        let pg = json!({
            "process_graph": {
                "m": {
                    "process_id": "mean",
                    "arguments": {"data": {"from_parameter": "data"}},
                    "result": true
                }
            }
        });
        let r = DefaultSubGraphEvaluator.evaluate_subgraph(&pg, BTreeMap::new());
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(m.contains("unbound"), "expected `unbound`, got `{m}`");
            }
            other => panic!("expected InvalidGraph, got {other:?}"),
        }
    }

    // ---------- P0-3 invariant guard ----------
    //
    // Confirms this module operates only on the inner sub-graph and
    // never tries to evaluate the outer-graph world. The outer-walker
    // short-circuit at `process_graph` keys lives in
    // `process_graph.rs::collect_in_value` and is unchanged by this
    // refactor; here we only confirm the inner walker treats the
    // sub-graph's `from_parameter` namespace as isolated from any outer
    // `from_node` (i.e. resolving an inner `from_parameter` does not
    // consult any outer memo).
    #[test]
    fn evaluator_respects_inner_namespace_isolation() {
        // Inner graph references parameter `data` — bindings only contain
        // `data` (no outer `from_node` IDs). The evaluator must NOT try to
        // walk outside this scope.
        let pg = json!({
            "process_graph": {
                "m": {
                    "process_id": "sum",
                    "arguments": {"data": {"from_parameter": "data"}},
                    "result": true
                }
            }
        });
        let mut params = BTreeMap::new();
        params.insert("data".into(), json!([5.0, 5.0]));
        let out = DefaultSubGraphEvaluator
            .evaluate_subgraph(&pg, params)
            .expect("eval");
        assert!((out.as_f64().expect("numeric") - 10.0).abs() < 1e-9);
    }

    // ---------- L1: per-sub-graph depth guard ----------

    /// Build a chain of `n` `count` nodes where each consumes the next
    /// via `{from_node: …}`. `n0` is the result; the deepest reads a
    /// literal data array. Past N>=2 the `count`-of-`count` type
    /// mismatch will surface — these tests only care whether the
    /// **depth guard** is the failure mode (or isn't).
    fn build_from_node_chain(n: usize) -> Value {
        let mut graph = serde_json::Map::new();
        for i in 0..n {
            let id = format!("n{i}");
            let is_first = i == 0;
            let arg = if i + 1 < n {
                json!({ "from_node": format!("n{}", i + 1) })
            } else {
                json!([1.0, 2.0, 3.0])
            };
            let node = json!({
                "process_id": "count",
                "arguments": { "data": arg },
                "result": is_first,
            });
            graph.insert(id, node);
        }
        json!({ "process_graph": graph })
    }

    fn err_message(r: &SubGraphResult) -> Option<&str> {
        match r {
            Err(ExecError::InvalidGraph(m)) => Some(m.as_str()),
            _ => None,
        }
    }

    #[test]
    fn subgraph_evaluator_accepts_chain_within_depth_limit() {
        // A short from_node chain (well under MAX_SUBGRAPH_DEPTH) must
        // NOT trip the depth guard. The kernel itself may complain about
        // count-of-count type mismatch — that's fine; we only assert the
        // failure mode is *not* depth-exceeded.
        let pg = build_from_node_chain(8);
        let r = DefaultSubGraphEvaluator.evaluate_subgraph(&pg, BTreeMap::new());
        if let Some(m) = err_message(&r) {
            assert!(
                !m.contains("recursion exceeded depth limit"),
                "short chain must not trip depth guard, got `{m}`"
            );
        }
    }

    #[test]
    fn subgraph_evaluator_rejects_chain_past_depth_limit() {
        // Two recursion levels per chain link (eval_node_value →
        // resolve_arg → eval_node_value). A chain comfortably past the
        // cap guarantees the guard fires before any kernel mismatch.
        let pg = build_from_node_chain(MAX_SUBGRAPH_DEPTH * 2 + 4);
        let r = DefaultSubGraphEvaluator.evaluate_subgraph(&pg, BTreeMap::new());
        match r {
            Err(ExecError::InvalidGraph(m)) => {
                assert!(
                    m.contains("recursion exceeded depth limit"),
                    "expected depth-limit error, got `{m}`"
                );
                assert!(
                    m.contains(&MAX_SUBGRAPH_DEPTH.to_string()),
                    "error must mention the cap, got `{m}`"
                );
            }
            other => panic!("expected InvalidGraph depth-exceeded, got {other:?}"),
        }
    }

    #[test]
    fn subgraph_evaluator_depth_budget_resets_per_evaluate_entry() {
        // **Invariant**: every `evaluate_subgraph` entry restarts its
        // depth counter from 0 — outer-walker budget MUST NOT bleed in,
        // and consecutive sub-graph entries MUST NOT accumulate. Calling
        // the evaluator twice with the same short chain must not produce
        // a depth-exceeded error on either pass; if the counter were
        // sticky the second call would fail.
        let pg = build_from_node_chain(4);
        let r1 = DefaultSubGraphEvaluator.evaluate_subgraph(&pg, BTreeMap::new());
        let r2 = DefaultSubGraphEvaluator.evaluate_subgraph(&pg, BTreeMap::new());
        for (label, r) in [("first", &r1), ("second", &r2)] {
            if let Some(m) = err_message(r) {
                assert!(
                    !m.contains("recursion exceeded depth limit"),
                    "{label} eval must not trip depth guard, got `{m}`"
                );
            }
        }
    }
}
