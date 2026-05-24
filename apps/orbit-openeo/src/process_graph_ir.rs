//! **Typed openEO Process Graph IR.**
//!
//! Phase-A #3 of the orbit-rs upgrade plan. Replaces the
//! string-match-on-`process_id` dispatcher in `geo_executor` with a
//! typed `ProcessNode` enum derived from the raw `eo_process::Process`
//! shape.
//!
//! # Why
//!
//! - **Closes audit P0-3**: the old `collect_in_value` walker treated
//!   sub-process callbacks (`apply.process`, `reduce_dimension.reducer`,
//!   `mask.mask`) as data dependencies, creating false cycles or
//!   phantom edges. The typed IR carries callbacks as proper
//!   `Box<ProcessNode>` children, so the topo walker can ignore them.
//! - **Closes audit P0-4**: typed dispatch makes "unimplemented
//!   process" a compiler error rather than a silent pass-through.
//! - **Foundation for #1 Datacube model**: typed args (`BBox`,
//!   `TemporalExtent`, `BandSelector`) replace untyped JSON.
//!
//! This module is intentionally **standalone** — it doesn't yet plug
//! into `GeoExecutor` (that's a follow-up atom). The conversion
//! `Process → ProcessNode::parse` round-trips through `serde_json`
//! today, so the existing executor surface keeps working while we
//! migrate process arms one by one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Errors raised while lifting raw openEO process JSON into the typed IR.
#[derive(Debug, Error, PartialEq)]
pub enum IrError {
    /// The `process_id` is not in our typed catalogue.
    #[error("unsupported process: {0}")]
    UnsupportedProcess(String),
    /// A required argument was missing.
    #[error("process `{process}` missing required argument `{arg}`")]
    MissingArgument { process: String, arg: String },
    /// An argument had the wrong shape or type.
    #[error("process `{process}` argument `{arg}`: {reason}")]
    BadArgument { process: String, arg: String, reason: String },
    /// A `data: {from_node: ...}` link inside a node argument.
    #[error("unresolved from_node reference: `{0}` (resolve at evaluation time)")]
    UnresolvedFromNode(String),
}

/// Geographic bounding box.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    /// Min longitude / west.
    pub west: f64,
    /// Min latitude / south.
    pub south: f64,
    /// Max longitude / east.
    pub east: f64,
    /// Max latitude / north.
    pub north: f64,
}

impl BBox {
    /// Try to lift an openEO `spatial_extent` object.
    pub fn parse(v: &Value) -> Result<Self, IrError> {
        let west = num_field(v, "west")?;
        let south = num_field(v, "south")?;
        let east = num_field(v, "east")?;
        let north = num_field(v, "north")?;
        Ok(Self { west, south, east, north })
    }
}

fn num_field(v: &Value, k: &str) -> Result<f64, IrError> {
    v.get(k)
        .and_then(|x| x.as_f64())
        .ok_or_else(|| IrError::BadArgument {
            process: "bbox".into(),
            arg: k.into(),
            reason: "expected number".into(),
        })
}

/// ISO-8601 / RFC-3339 inclusive temporal extent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemporalExtent {
    /// Start instant (RFC-3339).
    pub start: String,
    /// End instant (RFC-3339).
    pub end: String,
}

impl TemporalExtent {
    /// Try to lift an openEO `temporal_extent` 2-array.
    pub fn parse(v: &Value) -> Result<Self, IrError> {
        let arr = v.as_array().ok_or_else(|| IrError::BadArgument {
            process: "temporal_extent".into(),
            arg: "value".into(),
            reason: "expected 2-array".into(),
        })?;
        if arr.len() != 2 {
            return Err(IrError::BadArgument {
                process: "temporal_extent".into(),
                arg: "value".into(),
                reason: format!("expected 2 entries, got {}", arr.len()),
            });
        }
        let start = arr[0].as_str().unwrap_or("..").to_string();
        let end = arr[1].as_str().unwrap_or("..").to_string();
        Ok(Self { start, end })
    }
}

/// Reference to another node in the graph (`{"from_node": "<id>"}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRef(pub String);

impl NodeRef {
    /// Try to lift a `{from_node: "x"}` object. Returns `None` for
    /// non-link values so callers can keep literal scalars.
    pub fn try_parse(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let target = obj.get("from_node")?.as_str()?;
        Some(Self(target.to_string()))
    }
}

/// Value that may be a literal or a `from_node` link to another node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    /// Link to another node's result.
    Ref(NodeRef),
    /// Literal JSON value (string / number / bool / array / object).
    Literal(Value),
}

impl ArgValue {
    /// Lift any JSON value, detecting `{from_node: ...}` shape.
    pub fn parse(v: &Value) -> Self {
        if let Some(nref) = NodeRef::try_parse(v) {
            Self::Ref(nref)
        } else {
            Self::Literal(v.clone())
        }
    }
}

/// Typed openEO process node. Phase-A #3 covers the most-used set;
/// remaining string-dispatched arms in `geo_executor` will migrate
/// to this enum in follow-up atoms.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessNode {
    /// `load_collection(id, spatial_extent, temporal_extent, bands?)`.
    LoadCollection {
        /// Collection id, e.g. `"sentinel-2-l2a"`.
        id: String,
        /// Optional spatial extent.
        spatial_extent: Option<BBox>,
        /// Optional temporal extent.
        temporal_extent: Option<TemporalExtent>,
        /// Optional band selection.
        bands: Option<Vec<String>>,
    },
    /// `filter_bbox(data, extent)`.
    FilterBbox { data: ArgValue, extent: BBox },
    /// `filter_temporal(data, extent)`.
    FilterTemporal { data: ArgValue, extent: TemporalExtent },
    /// `filter_spatial(data, geometries)`.
    FilterSpatial { data: ArgValue, geometries: Value },
    /// `ndvi(data)` (Sentinel-2 red/nir convenience).
    Ndvi { data: ArgValue },
    /// `mask(data, mask, replacement)`.
    Mask {
        data: ArgValue,
        mask: ArgValue,
        replacement: Option<ArgValue>,
    },
    /// `reduce_dimension(data, reducer, dimension)`.
    /// The reducer is a SUB-GRAPH carried as a boxed nested `ProcessGraphIr`.
    ReduceDimension {
        data: ArgValue,
        reducer: Box<ProcessGraphIr>,
        dimension: String,
    },
    /// `apply(data, process)`.
    Apply { data: ArgValue, process: Box<ProcessGraphIr> },
    /// `resample_spatial(data, projection, resolution?)`.
    ResampleSpatial {
        data: ArgValue,
        projection: u32,
        resolution: Option<f64>,
    },
    /// `merge_cubes(cube1, cube2)`.
    MergeCubes { cube1: ArgValue, cube2: ArgValue },
    /// Save result. Format is one of `GTiff` / `COG` / `Json` / `Png` / `NetCDF`.
    SaveResult { data: ArgValue, format: String },
    /// `add(x, y)`.
    Add { x: ArgValue, y: ArgValue },
    /// `subtract(x, y)`.
    Subtract { x: ArgValue, y: ArgValue },
    /// `multiply(x, y)`.
    Multiply { x: ArgValue, y: ArgValue },
    /// `divide(x, y)`.
    Divide { x: ArgValue, y: ArgValue },
}

impl ProcessNode {
    /// True if the node carries a sub-process-graph callback (whose
    /// `from_node` references live in an INNER namespace and MUST NOT
    /// be walked by the outer topological sort). Closes audit P0-3.
    #[must_use]
    pub fn has_subgraph(&self) -> bool {
        matches!(
            self,
            Self::ReduceDimension { .. } | Self::Apply { .. },
        )
    }

    /// The process name for diagnostics + telemetry labels.
    #[must_use]
    pub fn process_id(&self) -> &'static str {
        match self {
            Self::LoadCollection { .. } => "load_collection",
            Self::FilterBbox { .. } => "filter_bbox",
            Self::FilterTemporal { .. } => "filter_temporal",
            Self::FilterSpatial { .. } => "filter_spatial",
            Self::Ndvi { .. } => "ndvi",
            Self::Mask { .. } => "mask",
            Self::ReduceDimension { .. } => "reduce_dimension",
            Self::Apply { .. } => "apply",
            Self::ResampleSpatial { .. } => "resample_spatial",
            Self::MergeCubes { .. } => "merge_cubes",
            Self::SaveResult { .. } => "save_result",
            Self::Add { .. } => "add",
            Self::Subtract { .. } => "subtract",
            Self::Multiply { .. } => "multiply",
            Self::Divide { .. } => "divide",
        }
    }

    /// Lift one raw `eo_process::Process` into a typed `ProcessNode`.
    pub fn parse(p: &eo_process::Process) -> Result<Self, IrError> {
        let pid = p.process_id.0.as_str();
        let args = &p.arguments;
        match pid {
            "load_collection" => Ok(Self::LoadCollection {
                id: str_arg(pid, args, "id")?,
                spatial_extent: args.get("spatial_extent").map(BBox::parse).transpose()?,
                temporal_extent: args
                    .get("temporal_extent")
                    .map(TemporalExtent::parse)
                    .transpose()?,
                bands: args
                    .get("bands")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    }),
            }),
            "filter_bbox" => Ok(Self::FilterBbox {
                data: data_arg(args)?,
                extent: BBox::parse(req_arg(pid, args, "extent")?)?,
            }),
            "filter_temporal" => Ok(Self::FilterTemporal {
                data: data_arg(args)?,
                extent: TemporalExtent::parse(req_arg(pid, args, "extent")?)?,
            }),
            "filter_spatial" => Ok(Self::FilterSpatial {
                data: data_arg(args)?,
                geometries: req_arg(pid, args, "geometries")?.clone(),
            }),
            "ndvi" => Ok(Self::Ndvi {
                data: data_arg(args)?,
            }),
            "mask" => Ok(Self::Mask {
                data: data_arg(args)?,
                mask: arg(args, "mask")?,
                replacement: args.get("replacement").map(ArgValue::parse),
            }),
            "reduce_dimension" => Ok(Self::ReduceDimension {
                data: data_arg(args)?,
                reducer: Box::new(subgraph(pid, args, "reducer")?),
                dimension: str_arg(pid, args, "dimension")?,
            }),
            "apply" => Ok(Self::Apply {
                data: data_arg(args)?,
                process: Box::new(subgraph(pid, args, "process")?),
            }),
            "resample_spatial" => Ok(Self::ResampleSpatial {
                data: data_arg(args)?,
                projection: req_arg(pid, args, "projection")?
                    .as_u64()
                    .ok_or_else(|| IrError::BadArgument {
                        process: pid.into(),
                        arg: "projection".into(),
                        reason: "expected u64 (EPSG)".into(),
                    })? as u32,
                resolution: args.get("resolution").and_then(|v| v.as_f64()),
            }),
            "merge_cubes" => Ok(Self::MergeCubes {
                cube1: arg(args, "cube1").or_else(|_| arg(args, "data1"))?,
                cube2: arg(args, "cube2").or_else(|_| arg(args, "data2"))?,
            }),
            "save_result" => Ok(Self::SaveResult {
                data: data_arg(args)?,
                format: args
                    .get("format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GTiff")
                    .to_string(),
            }),
            "add" => Ok(Self::Add { x: arg(args, "x")?, y: arg(args, "y")? }),
            "subtract" => Ok(Self::Subtract { x: arg(args, "x")?, y: arg(args, "y")? }),
            "multiply" => Ok(Self::Multiply { x: arg(args, "x")?, y: arg(args, "y")? }),
            "divide" => Ok(Self::Divide { x: arg(args, "x")?, y: arg(args, "y")? }),
            other => Err(IrError::UnsupportedProcess(other.into())),
        }
    }
}

fn arg(args: &BTreeMap<String, Value>, key: &str) -> Result<ArgValue, IrError> {
    args.get(key).map(ArgValue::parse).ok_or_else(|| IrError::MissingArgument {
        process: "<unknown>".into(),
        arg: key.into(),
    })
}

fn data_arg(args: &BTreeMap<String, Value>) -> Result<ArgValue, IrError> {
    arg(args, "data")
}

fn req_arg<'a>(
    process: &str,
    args: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a Value, IrError> {
    args.get(key).ok_or_else(|| IrError::MissingArgument {
        process: process.into(),
        arg: key.into(),
    })
}

fn str_arg(process: &str, args: &BTreeMap<String, Value>, key: &str) -> Result<String, IrError> {
    req_arg(process, args, key)?
        .as_str()
        .map(String::from)
        .ok_or_else(|| IrError::BadArgument {
            process: process.into(),
            arg: key.into(),
            reason: "expected string".into(),
        })
}

fn subgraph(
    process: &str,
    args: &BTreeMap<String, Value>,
    key: &str,
) -> Result<ProcessGraphIr, IrError> {
    let v = req_arg(process, args, key)?;
    // openEO sub-process callbacks come in two shapes:
    //   1. `{"process_graph": {...}}` — the canonical form
    //   2. `{...nodes...}` — some clients shorthand straight to a graph
    let pg_value = v
        .get("process_graph")
        .cloned()
        .unwrap_or_else(|| v.clone());
    let nodes: BTreeMap<String, eo_process::Process> = serde_json::from_value(pg_value)
        .map_err(|e| IrError::BadArgument {
            process: process.into(),
            arg: key.into(),
            reason: format!("not a valid process_graph: {e}"),
        })?;
    ProcessGraphIr::from_nodes(nodes)
}

/// Typed process graph — a map of node-id → typed `ProcessNode`, plus
/// the id of the result-marked node. Replaces `eo_process::ProcessGraph`
/// at the dispatch layer.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessGraphIr {
    /// Map of `node_id -> typed node`.
    pub nodes: BTreeMap<String, ProcessNode>,
    /// Id of the unique `result: true` node, if any.
    pub result_id: Option<String>,
}

impl ProcessGraphIr {
    /// Lift a `BTreeMap<String, eo_process::Process>` into a typed
    /// `ProcessGraphIr`. Each `Process` is run through
    /// `ProcessNode::parse`; the first node carrying `result: true` is
    /// recorded as `result_id`.
    pub fn from_nodes(
        nodes: BTreeMap<String, eo_process::Process>,
    ) -> Result<Self, IrError> {
        let mut out = BTreeMap::new();
        let mut result_id: Option<String> = None;
        for (id, p) in nodes {
            if p.result == Some(true) && result_id.is_none() {
                result_id = Some(id.clone());
            }
            out.insert(id, ProcessNode::parse(&p)?);
        }
        Ok(Self { nodes: out, result_id })
    }

    /// Lift an `eo_process::ProcessGraph` directly.
    pub fn from_graph(g: eo_process::ProcessGraph) -> Result<Self, IrError> {
        Self::from_nodes(g.nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proc(id: &str, args: Value) -> eo_process::Process {
        let nodes: BTreeMap<String, Value> = args
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let typed_args: BTreeMap<String, Value> = nodes.into_iter().collect();
        eo_process::Process {
            process_id: eo_process::ProcessId(id.into()),
            arguments: typed_args,
            result: None,
        }
    }

    #[test]
    fn bbox_parses_canonical_shape() {
        let v = json!({"west":1.0,"south":2.0,"east":3.0,"north":4.0});
        let b = BBox::parse(&v).unwrap();
        assert_eq!(b, BBox { west: 1.0, south: 2.0, east: 3.0, north: 4.0 });
    }

    #[test]
    fn bbox_missing_field_is_bad_argument() {
        let v = json!({"west":1.0,"south":2.0});
        assert!(matches!(BBox::parse(&v), Err(IrError::BadArgument { .. })));
    }

    #[test]
    fn temporal_extent_round_trips_two_array() {
        let v = json!(["2024-06-01T00:00:00Z","2024-09-30T23:59:59Z"]);
        let t = TemporalExtent::parse(&v).unwrap();
        assert_eq!(t.start, "2024-06-01T00:00:00Z");
        assert_eq!(t.end, "2024-09-30T23:59:59Z");
    }

    #[test]
    fn temporal_extent_wrong_arity_errors() {
        let v = json!(["only-one"]);
        assert!(matches!(TemporalExtent::parse(&v), Err(IrError::BadArgument { .. })));
    }

    #[test]
    fn node_ref_detects_from_node_object() {
        assert_eq!(
            NodeRef::try_parse(&json!({"from_node": "a"})),
            Some(NodeRef("a".into()))
        );
        assert_eq!(NodeRef::try_parse(&json!({"from_node": 42})), None);
        assert_eq!(NodeRef::try_parse(&json!(42)), None);
    }

    #[test]
    fn arg_value_distinguishes_literal_vs_ref() {
        let r = ArgValue::parse(&json!({"from_node": "a"}));
        assert_eq!(r, ArgValue::Ref(NodeRef("a".into())));
        let l = ArgValue::parse(&json!(42));
        assert_eq!(l, ArgValue::Literal(json!(42)));
    }

    #[test]
    fn parse_load_collection_lifts_all_args() {
        let p = proc(
            "load_collection",
            json!({
                "id": "sentinel-2-l2a",
                "spatial_extent": {"west":1.0,"south":2.0,"east":3.0,"north":4.0},
                "temporal_extent": ["2024-06-01T00:00:00Z","2024-09-30T23:59:59Z"],
                "bands": ["B04", "B08"]
            }),
        );
        let n = ProcessNode::parse(&p).unwrap();
        match n {
            ProcessNode::LoadCollection { id, spatial_extent, temporal_extent, bands } => {
                assert_eq!(id, "sentinel-2-l2a");
                assert!(spatial_extent.is_some());
                assert!(temporal_extent.is_some());
                assert_eq!(bands.unwrap(), vec!["B04".to_string(), "B08".to_string()]);
            }
            other => panic!("expected LoadCollection, got {other:?}"),
        }
    }

    #[test]
    fn parse_unsupported_process_errors() {
        let p = proc("totally_made_up", json!({}));
        match ProcessNode::parse(&p) {
            Err(IrError::UnsupportedProcess(id)) => assert_eq!(id, "totally_made_up"),
            other => panic!("expected UnsupportedProcess, got {other:?}"),
        }
    }

    #[test]
    fn parse_add_with_from_node_arg() {
        let p = proc("add", json!({"x": {"from_node":"a"}, "y": 5}));
        match ProcessNode::parse(&p).unwrap() {
            ProcessNode::Add { x, y } => {
                assert_eq!(x, ArgValue::Ref(NodeRef("a".into())));
                assert_eq!(y, ArgValue::Literal(json!(5)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_reduce_dimension_carries_subgraph() {
        let p = proc(
            "reduce_dimension",
            json!({
                "data": {"from_node": "l"},
                "dimension": "time",
                "reducer": {
                    "process_graph": {
                        "n1": { "process_id": "ndvi", "arguments": { "data": {"from_node":"data"} }, "result": true }
                    }
                }
            }),
        );
        let n = ProcessNode::parse(&p).unwrap();
        assert!(n.has_subgraph(), "must surface as a sub-graph carrier");
        match n {
            ProcessNode::ReduceDimension { dimension, reducer, .. } => {
                assert_eq!(dimension, "time");
                // The sub-graph parser must see the inner ndvi node.
                assert!(reducer.nodes.contains_key("n1"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_apply_carries_subgraph() {
        let p = proc(
            "apply",
            json!({
                "data": {"from_node": "l"},
                "process": {
                    "process_graph": {
                        "n1": { "process_id": "add", "arguments": {"x":1,"y":2}, "result": true }
                    }
                }
            }),
        );
        let n = ProcessNode::parse(&p).unwrap();
        assert!(n.has_subgraph());
    }

    #[test]
    fn has_subgraph_false_for_leaf_nodes() {
        let p = proc("add", json!({"x":1, "y":2}));
        let n = ProcessNode::parse(&p).unwrap();
        assert!(!n.has_subgraph());
    }

    #[test]
    fn process_id_label_matches_each_variant() {
        let cases = [
            ("add", json!({"x":1,"y":2})),
            ("subtract", json!({"x":1,"y":2})),
            ("multiply", json!({"x":1,"y":2})),
            ("divide", json!({"x":1,"y":2})),
            ("ndvi", json!({"data":{"from_node":"l"}})),
            ("save_result", json!({"data":{"from_node":"l"},"format":"GTiff"})),
        ];
        for (id, args) in cases {
            let n = ProcessNode::parse(&proc(id, args)).unwrap();
            assert_eq!(n.process_id(), id, "label drift on {id}");
        }
    }

    #[test]
    fn process_graph_ir_records_result_id() {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("add", json!({"x":1,"y":2})));
        let mut p_save = proc("save_result", json!({"data":{"from_node":"a"},"format":"GTiff"}));
        p_save.result = Some(true);
        nodes.insert("s".into(), p_save);
        let ir = ProcessGraphIr::from_nodes(nodes).unwrap();
        assert_eq!(ir.result_id.as_deref(), Some("s"));
        assert_eq!(ir.nodes.len(), 2);
    }

    #[test]
    fn process_graph_ir_no_result_node_records_none() {
        let mut nodes = BTreeMap::new();
        nodes.insert("a".into(), proc("add", json!({"x":1,"y":2})));
        let ir = ProcessGraphIr::from_nodes(nodes).unwrap();
        assert_eq!(ir.result_id, None);
    }

    #[test]
    fn merge_cubes_accepts_cube1_or_data1_aliases() {
        let a = ProcessNode::parse(&proc(
            "merge_cubes",
            json!({"cube1":{"from_node":"a"},"cube2":{"from_node":"b"}}),
        )).unwrap();
        let b = ProcessNode::parse(&proc(
            "merge_cubes",
            json!({"data1":{"from_node":"a"},"data2":{"from_node":"b"}}),
        )).unwrap();
        assert!(matches!(a, ProcessNode::MergeCubes { .. }));
        assert!(matches!(b, ProcessNode::MergeCubes { .. }));
    }

    #[test]
    fn save_result_defaults_format_to_gtiff() {
        let n = ProcessNode::parse(&proc(
            "save_result",
            json!({"data":{"from_node":"a"}}),
        )).unwrap();
        match n {
            ProcessNode::SaveResult { format, .. } => assert_eq!(format, "GTiff"),
            _ => panic!(),
        }
    }

    #[test]
    fn mask_replacement_is_optional() {
        let without = ProcessNode::parse(&proc(
            "mask",
            json!({"data":{"from_node":"d"},"mask":{"from_node":"m"}}),
        )).unwrap();
        let with = ProcessNode::parse(&proc(
            "mask",
            json!({"data":{"from_node":"d"},"mask":{"from_node":"m"},"replacement":-9999}),
        )).unwrap();
        if let (ProcessNode::Mask { replacement: r0, .. }, ProcessNode::Mask { replacement: r1, .. }) =
            (without, with)
        {
            assert!(r0.is_none());
            assert!(r1.is_some());
        } else {
            panic!();
        }
    }

    #[test]
    fn resample_spatial_requires_projection() {
        let no_proj = ProcessNode::parse(&proc(
            "resample_spatial",
            json!({"data":{"from_node":"d"}}),
        ));
        assert!(matches!(no_proj, Err(IrError::MissingArgument { .. })));
        let ok = ProcessNode::parse(&proc(
            "resample_spatial",
            json!({"data":{"from_node":"d"},"projection":3857}),
        )).unwrap();
        match ok {
            ProcessNode::ResampleSpatial { projection, .. } => assert_eq!(projection, 3857),
            _ => panic!(),
        }
    }
}
