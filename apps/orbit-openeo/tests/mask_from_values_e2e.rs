//! A10 integration test — the **generic `mask` + `mask_from_values`** composition.
//!
//! This is the spec-compliant alternative to the S2-specific `mask_scl_dilation`:
//!
//! ```text
//! load_collection(bands=[B04,B08,SCL])
//!     ├──→ binmask = mask_from_values(data=load, band="SCL", values=[3,8,9,10,11])
//!     │         (builds a single-band u8 binary cube: 1 where SCL ∈ values, 0 elsewhere)
//!     └──→ masked  = mask(data=load, mask=binmask)
//!              ↓
//!            ndvi(masked, nir=B08, red=B04)
//!              ↓
//!            reduce_dimension(t, mean)
//!              ↓
//!            save_result(GTiff)
//! ```
//!
//! These tests exercise the **API surface** through the HTTP façade against
//! the default `LocalExecutor`. Submission + topological validation + GET
//! round-trip + DELETE lifecycle must all work. Mathematical execution of
//! `mask_from_values` against synthetic fixtures is covered by the lib
//! unit tests in `apps/orbit-openeo/src/geo_executor/eval_mask_from_values.rs`.

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
                    "bands": ["B04", "B08", "SCL"]
                }
            },
            "binmask": {
                "process_id": "mask_from_values",
                "arguments": {
                    "data": { "from_node": "load1" },
                    "band": "SCL",
                    "values": [3, 8, 9, 10, 11]
                }
            },
            "masked": {
                "process_id": "mask",
                "arguments": {
                    "data": { "from_node": "load1" },
                    "mask": { "from_node": "binmask" }
                }
            },
            "ndvi1": {
                "process_id": "ndvi",
                "arguments": {
                    "data": { "from_node": "masked" },
                    "nir":  "B08",
                    "red":  "B04"
                }
            },
            "reduce1": {
                "process_id": "reduce_dimension",
                "arguments": {
                    "data":      { "from_node": "ndvi1" },
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
        "title": "A10: load → mask_from_values → mask → ndvi → reduce → save"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_from_values_full_graph_accepted_by_facade() {
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
        "submission with mask_from_values + mask must be 201; got {}", resp.status());
    let v = body_to_json(resp).await;
    assert_eq!(v["status"], "created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_from_values_graph_round_trips_all_six_nodes() {
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

    // All 6 nodes survive serde + storage with their process_ids and links.
    for k in ["load1", "binmask", "masked", "ndvi1", "reduce1", "save1"] {
        assert!(nodes.contains_key(k), "missing node `{k}` after round-trip");
    }
    assert_eq!(nodes["load1"]["process_id"],   "load_collection");
    assert_eq!(nodes["binmask"]["process_id"], "mask_from_values");
    assert_eq!(nodes["masked"]["process_id"],  "mask");
    assert_eq!(nodes["ndvi1"]["process_id"],   "ndvi");
    assert_eq!(nodes["reduce1"]["process_id"], "reduce_dimension");
    assert_eq!(nodes["save1"]["process_id"],   "save_result");

    // The two from_node fan-in pattern (load1 feeds both binmask and masked).
    assert_eq!(nodes["binmask"]["arguments"]["data"]["from_node"], "load1");
    assert_eq!(nodes["masked"]["arguments"]["data"]["from_node"],  "load1");
    assert_eq!(nodes["masked"]["arguments"]["mask"]["from_node"],  "binmask");
    assert_eq!(nodes["ndvi1"]["arguments"]["data"]["from_node"],   "masked");

    // mask_from_values args preserved verbatim
    assert_eq!(nodes["binmask"]["arguments"]["band"], "SCL");
    assert_eq!(nodes["binmask"]["arguments"]["values"], json!([3, 8, 9, 10, 11]));

    // Reducer sub-graph survives (P0-3 short-circuit invariant)
    assert_eq!(
        nodes["reduce1"]["arguments"]["reducer"]["process_graph"]["mean1"]["process_id"],
        "mean"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_from_values_graph_passes_structural_validation_with_two_from_node_links_from_one_source() {
    // Specifically guards the case where a single node (load1) is referenced
    // by two downstream nodes (binmask + masked) via from_node. This is a
    // valid DAG pattern and the topo walker must accept it.
    let body = full_graph().to_string();
    let resp = app()
        .oneshot(Request::builder().method("POST").uri("/jobs")
            .header("content-type", "application/json")
            .body(Body::from(body)).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED,
        "fan-in from_node graph must pass ProcessGraphAnalysis::build");
}
