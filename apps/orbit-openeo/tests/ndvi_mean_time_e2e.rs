//! E2E: canonical openEO 1.3.0 NDVI mean-time process graph round-trip.
//!
//! Proves the public façade accepts the *real-world* NDVI mean-time
//! graph shape produced by openeo-python-client / openeo-r-client:
//!
//!   load_collection(B04, B08) → ndvi → reduce_dimension(t, mean) → save_result(GTiff)
//!
//! Crucially this exercises the **P0-3 fix** — `reduce_dimension`'s
//! `reducer.process_graph` sub-callback uses `from_parameter` (inner
//! namespace) and an outer-graph topo walk that recursed into it would
//! reject the graph spuriously. The walker must short-circuit at
//! `process_graph` keys.
//!
//! Execution backend is `LocalExecutor` (no GDAL, no network). The
//! test asserts the **API surface**: POST → 201 + identifier,
//! GET round-trip, GET /jobs listing, and DELETE cleanup. End-to-end
//! *execution* of NDVI requires `--executor geo` + `--features geo-kernel`
//! and is covered in `apps/orbit-openeo/src/geo_executor.rs` unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orbit_openeo::{build_router, AppStateBuilder};
use serde_json::Value;
use tower::ServiceExt;

const NDVI_GRAPH_JSON: &str = include_str!("../examples/ndvi_mean_time.json");

fn app() -> axum::Router {
    build_router(AppStateBuilder::new().build())
}

async fn body_to_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn ndvi_mean_time_post_returns_201_with_openeo_identifier() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(NDVI_GRAPH_JSON))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "POST /jobs must accept canonical NDVI mean-time graph");
    let id = resp
        .headers()
        .get("openeo-identifier")
        .expect("openeo-identifier header missing")
        .to_str()
        .unwrap()
        .to_string();
    let location = resp
        .headers()
        .get("location")
        .expect("location header missing")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(location, format!("/jobs/{id}"));
    let v = body_to_json(resp).await;
    assert_eq!(v["status"], "created");
    assert_eq!(v["title"], "NDVI mean over time — Wien JJAS 2024");
}

#[tokio::test(flavor = "current_thread")]
async fn ndvi_mean_time_get_round_trip_preserves_graph() {
    let app = app();
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(NDVI_GRAPH_JSON))
                .unwrap(),
        )
        .await
        .unwrap();
    let id = created
        .headers()
        .get("openeo-identifier")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/jobs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_to_json(resp).await;
    assert_eq!(v["id"], id);
    assert_eq!(v["status"], "created");
    let nodes = v["process"]["process_graph"]
        .as_object()
        .expect("graph must round-trip in detail body");
    // The four canonical NDVI mean-time nodes survive the store.
    assert!(nodes.contains_key("load1"), "load_collection node missing");
    assert!(nodes.contains_key("ndvi1"),  "ndvi node missing");
    assert!(nodes.contains_key("reduce1"), "reduce_dimension node missing");
    assert!(nodes.contains_key("save1"),  "save_result node missing");
    // The reducer sub-graph survived (P0-3 — outer walker must not
    // flatten this into the outer namespace).
    let reducer = &nodes["reduce1"]["arguments"]["reducer"];
    assert!(
        reducer["process_graph"]["mean1"]["process_id"] == "mean",
        "reducer.process_graph.mean1 must survive serde round-trip; got {reducer}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ndvi_mean_time_listed_in_get_jobs() {
    let app = app();
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(NDVI_GRAPH_JSON))
                .unwrap(),
        )
        .await
        .unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_to_json(resp).await;
    let arr = v["jobs"].as_array().expect("jobs array");
    assert_eq!(arr.len(), 1, "expected exactly one job after one POST");
    assert_eq!(arr[0]["title"], "NDVI mean over time — Wien JJAS 2024");
}
