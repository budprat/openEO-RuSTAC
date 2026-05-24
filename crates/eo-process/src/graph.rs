//! OpenEO process-graph types.
//!
//! Schema follows openEO API 1.2 — a process graph is a JSON object
//! mapping a node id to a `Process { process_id, arguments, result }`.
//! Arguments are typed via `serde_json::Value` so the AST can carry the
//! full openEO type system (numbers, strings, datacubes, sub-graphs).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifier of a process within the openEO process registry.
///
/// Examples: `"load_collection"`, `"mask"`, `"reduce_dimension"`,
/// `"save_result"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessId(pub String);

impl<S: Into<String>> From<S> for ProcessId {
    fn from(s: S) -> Self { Self(s.into()) }
}

/// A single node in a process graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Process {
    /// The id of the process to invoke.
    #[serde(rename = "process_id")]
    pub process_id: ProcessId,
    /// Arguments as raw openEO JSON values (number, string, datacube ref…).
    #[serde(default)]
    pub arguments: BTreeMap<String, Value>,
    /// Mark this node as the final-result node of the graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<bool>,
}

/// A process graph: id → node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessGraph {
    /// Nodes keyed by their graph-local id.
    pub nodes: BTreeMap<String, Process>,
}

impl ProcessGraph {
    /// Find the result node — the single node with `result: true`.
    /// Returns `None` if zero or more than one result node exists.
    #[must_use]
    pub fn result_node(&self) -> Option<(&str, &Process)> {
        let mut found: Option<(&str, &Process)> = None;
        for (id, node) in &self.nodes {
            if node.result == Some(true) {
                if found.is_some() {
                    return None;
                }
                found = Some((id.as_str(), node));
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(k: &str, v: Value) -> (String, Value) { (k.into(), v) }

    #[test]
    fn graph_with_one_result_node_is_resolvable() {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "load".into(),
            Process {
                process_id: "load_collection".into(),
                arguments: [arg("id", Value::String("SENTINEL2_L2A".into()))].into_iter().collect(),
                result: None,
            },
        );
        nodes.insert(
            "save".into(),
            Process {
                process_id: "save_result".into(),
                arguments: BTreeMap::new(),
                result: Some(true),
            },
        );
        let g = ProcessGraph { nodes };
        let (id, node) = g.result_node().expect("exactly one result");
        assert_eq!(id, "save");
        assert_eq!(node.process_id.0, "save_result");
    }

    #[test]
    fn graph_with_zero_result_nodes_is_none() {
        let g = ProcessGraph::default();
        assert!(g.result_node().is_none());
    }

    #[test]
    fn graph_with_two_result_nodes_is_none() {
        let mut nodes = BTreeMap::new();
        for id in ["a", "b"] {
            nodes.insert(
                id.into(),
                Process {
                    process_id: "x".into(),
                    arguments: BTreeMap::new(),
                    result: Some(true),
                },
            );
        }
        let g = ProcessGraph { nodes };
        assert!(g.result_node().is_none(), "ambiguous result must return None");
    }

    #[test]
    fn process_id_from_str() {
        let p: ProcessId = "load_collection".into();
        assert_eq!(p.0, "load_collection");
    }

    #[test]
    fn graph_roundtrips_json() {
        let g = ProcessGraph {
            nodes: [(
                "save".into(),
                Process {
                    process_id: "save_result".into(),
                    arguments: BTreeMap::new(),
                    result: Some(true),
                },
            )]
            .into_iter()
            .collect(),
        };
        let s = serde_json::to_string(&g).unwrap();
        let back: ProcessGraph = serde_json::from_str(&s).unwrap();
        assert_eq!(back, g);
    }
}
