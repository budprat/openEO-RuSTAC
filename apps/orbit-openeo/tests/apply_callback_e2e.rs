//! P0-4-replacement integration test — `apply(data, process)` with a
//! real sub-process callback.
//!
//! ```text
//! load_collection(bands=[B04,B08])
//!   → ndvi(nir=B08, red=B04)
//!   → apply(process: max(x, 0))            ← canonical "clip negative NDVI to 0"
//!   → reduce_dimension(t, mean)
//!   → save_result(GTiff)
//! ```
//!
//! These tests exercise the **API surface** through the HTTP façade
//! against the default `LocalExecutor`. Submission + topological
//! validation + GET round-trip + reducer-subgraph short-circuit
//! preservation must all work. Mathematical execution of the
//! sub-graph against synthetic fixtures is covered by the lib unit
//! tests in `apps/orbit-openeo/src/geo_executor/eval_apply.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orbit_openeo::{build_router, AppStateBuilder};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> axum::Router {
    build_router(AppStateBuilder::new().build())
}

async fn body_to_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn full_graph() -> Value {
    json!({
        "process": { "process_graph": {
            "load1": {
                "process_id": "load_collection",
                "arguments": {
                    "id": "sentinel-2-l2a",
                    "spatial_extent": {
                        "west": 16.347, "south": 48.191,
                        "east": 16.374, "north": 48.209
                    },
                    "temporal_extent": ["2024-06-01", "2024-09-30"],
                    "bands": ["B04", "B08"]
                }
            },
            "ndvi1": {
                "process_id": "ndvi",
                "arguments": {
                    "data": { "from_node": "load1" },
                    "nir":  "B08",
                    "red":  "B04"
                }
            },
            "clipped": {
                "process_id": "apply",
                "arguments": {
                    "data": { "from_node": "ndvi1" },
                    "process": { "process_graph": {
                        "max_zero": {
                            "process_id": "max",
                            "arguments": { "data": [{ "from_parameter": "x" }, 0] },
                            "result": true
                        }
                    }}
                }
            },
            "reduce1": {
                "process_id": "reduce_dimension",
                "arguments": {
                    "data":      { "from_node": "clipped" },
                    "dimension": "t",
                    "reducer": { "process_graph": { "mean1": {
                        "process_id": "mean",
                        "arguments": { "data": { "from_parameter": "data" } },
                        "result": true
                    }}}
                }
            },
            "save1": {
                "process_id": "save_result",
                "arguments": { "data": { "from_node": "reduce1" }, "format": "GTiff" },
                "result": true
            }
        }},
        "title": "apply callback e2e: load → ndvi → apply(max(x,0)) → reduce → save"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_callback_full_graph_accepted_by_facade() {
    let body = full_graph().to_string();
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST").uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(body)).unwrap(),
        )
        .await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED,
        "submission with apply callback must be 201; got {}", resp.status());
    let v = body_to_json(resp).await;
    assert_eq!(v["status"], "created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_callback_graph_round_trips_all_five_nodes() {
    let app = app();
    let body = full_graph().to_string();
    let created = app.clone()
        .oneshot(Request::builder().method("POST").uri("/jobs")
            .header("content-type", "application/json")
            .body(Body::from(body)).unwrap())
        .await.unwrap();
    let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();

    let resp = app.oneshot(Request::builder().uri(format!("/jobs/{id}"))
        .body(Body::empty()).unwrap()).await.unwrap();
    let v = body_to_json(resp).await;
    let nodes = v["process"]["process_graph"].as_object().expect("graph must round-trip");

    // All 5 nodes survive serde + storage with their process_ids and links.
    for k in ["load1", "ndvi1", "clipped", "reduce1", "save1"] {
        assert!(nodes.contains_key(k), "missing node `{k}` after round-trip");
    }
    assert_eq!(nodes["load1"]["process_id"],   "load_collection");
    assert_eq!(nodes["ndvi1"]["process_id"],   "ndvi");
    assert_eq!(nodes["clipped"]["process_id"], "apply");
    assert_eq!(nodes["reduce1"]["process_id"], "reduce_dimension");
    assert_eq!(nodes["save1"]["process_id"],   "save_result");

    // The apply node carries the full sub-graph in its `process` arg.
    assert_eq!(nodes["clipped"]["arguments"]["data"]["from_node"], "ndvi1");
    let sub_pg = &nodes["clipped"]["arguments"]["process"]["process_graph"];
    assert_eq!(sub_pg["max_zero"]["process_id"], "max",
        "apply sub-graph must round-trip with `max_zero` node intact");

    // P0-3 short-circuit invariant: the reducer's `from_parameter: "data"`
    // is NOT pulled into the outer topological walk.
    assert_eq!(
        nodes["reduce1"]["arguments"]["reducer"]["process_graph"]["mean1"]["process_id"],
        "mean"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_callback_passes_topological_validation_despite_sub_graph_from_parameter() {
    // The apply sub-graph references `from_parameter: "x"` which is
    // bound to per-pixel values, NOT an outer-graph node. The outer
    // topo walker (process_graph.rs::collect_in_value) must short-
    // circuit on `process_graph` keys and not treat `x` as a dangling
    // node reference. This guards P0-3.
    let body = full_graph().to_string();
    let resp = app()
        .oneshot(Request::builder().method("POST").uri("/jobs")
            .header("content-type", "application/json")
            .body(Body::from(body)).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED,
        "apply sub-graph with from_parameter('x') must not be flagged as a dangling outer-graph ref");
}
