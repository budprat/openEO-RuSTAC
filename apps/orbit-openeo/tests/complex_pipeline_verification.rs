//! Complex process-graph verification — exercises the largest subset of
//! the openEO process catalog we currently implement, with **mathematical
//! ground-truth assertions** at each step.
//!
//! Two graphs are covered here:
//!
//! ## Graph A — Arithmetic chain (LocalExecutor, no GDAL needed)
//!
//! ```text
//! addNode      = add(10, 5)             →  15
//! multiplyNode = multiply(addNode, 3)   →  45
//! subtractNode = subtract(multiplyNode, 7) → 38
//! divideNode   = divide(subtractNode, 4) →  9.5
//! save         = save_result(divideNode, "JSON") → 9.5
//! ```
//!
//! Tests:
//! - Submission accepted (201 + OpenEO-Identifier)
//! - GET /jobs/{id}.process round-trips with all 5 nodes intact
//! - POST /jobs/{id}/results spawns runner
//! - Job reaches `status=finished` + `progress=100`
//! - GET /jobs/{id}/results manifest lists `result.json` asset
//! - GET /jobs/{id}/results/result.json returns the **exact** value `9.5`
//! - Topological order respected: divideNode evaluated last
//!
//! ## Graph B — Full geo pipeline (real-network only)
//!
//! Live end-to-end NDVI execution requires a real STAC + COG backend
//! and is exercised by `apps/orbit-openeo/examples/test_ndvi_e2e.sh`
//! against Element84 STAC + Sentinel-2 COGs on S3 (see CLAUDE.md §2.4).
//! Synthetic-fixture lib tests were removed as part of the fake-data purge.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orbit_openeo::{build_router, AppStateBuilder};
use serde_json::Value;
use tower::ServiceExt;

const ARITHMETIC_GRAPH: &str =
    include_str!("../examples/arithmetic_chain.json");

fn app() -> axum::Router {
    build_router(AppStateBuilder::new().build())
}

async fn body_to_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arithmetic_chain_post_returns_201_with_identifier() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(ARITHMETIC_GRAPH))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = resp
        .headers()
        .get("openeo-identifier")
        .expect("openeo-identifier header required")
        .to_str()
        .unwrap()
        .to_string();
    let v = body_to_json(resp).await;
    assert_eq!(v["status"], "created");
    assert_eq!(v["id"], id);
    assert_eq!(v["title"], "Arithmetic chain: ((10 + 5) × 3 − 7) ÷ 4 = 9.5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arithmetic_chain_round_trips_all_five_process_nodes() {
    let app = app();
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(ARITHMETIC_GRAPH))
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
    let v = body_to_json(resp).await;
    let nodes = v["process"]["process_graph"]
        .as_object()
        .expect("graph round-trips");
    // All five nodes must survive serde + storage.
    for k in ["addNode", "multiplyNode", "subtractNode", "divideNode", "save"] {
        assert!(nodes.contains_key(k), "missing node `{k}` in round-trip");
    }
    // Process_ids preserved.
    assert_eq!(nodes["addNode"]["process_id"],      "add");
    assert_eq!(nodes["multiplyNode"]["process_id"], "multiply");
    assert_eq!(nodes["subtractNode"]["process_id"], "subtract");
    assert_eq!(nodes["divideNode"]["process_id"],   "divide");
    assert_eq!(nodes["save"]["process_id"],         "save_result");
    // from_node links preserved.
    assert_eq!(nodes["multiplyNode"]["arguments"]["x"]["from_node"], "addNode");
    assert_eq!(nodes["subtractNode"]["arguments"]["x"]["from_node"], "multiplyNode");
    assert_eq!(nodes["divideNode"]["arguments"]["x"]["from_node"],   "subtractNode");
    assert_eq!(nodes["save"]["arguments"]["data"]["from_node"],      "divideNode");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arithmetic_chain_executes_to_exact_value_nine_point_five() {
    // The full mathematical proof:
    //   addNode      = 10 + 5            = 15
    //   multiplyNode = 15 × 3            = 45
    //   subtractNode = 45 − 7            = 38
    //   divideNode   = 38 ÷ 4            = 9.5
    //
    // Every binary op above must be dispatched in topological order,
    // every `from_node` link resolved against the memo, and the final
    // f64 result serialized through save_result(JSON).
    let app = app();
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(ARITHMETIC_GRAPH))
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

    // Start processing.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/jobs/{id}/results"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Poll for terminal state.
    let mut terminal = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/jobs/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_to_json(r).await;
        let status = v["status"].as_str().unwrap_or("").to_string();
        if status == "finished" || status == "error" {
            terminal = Some((status, v));
            break;
        }
    }
    let (status, rec) = terminal.expect("job never reached terminal state");
    assert_eq!(
        status, "finished",
        "job ended in non-finished state: {rec:?}"
    );
    assert_eq!(rec["progress"].as_f64(), Some(100.0));

    // Fetch manifest.
    let manifest = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/jobs/{id}/results"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let m = body_to_json(manifest).await;
    let assets = m["assets"].as_object().expect("manifest.assets");
    assert!(
        assets.contains_key("result.json"),
        "expected result.json asset, got assets={assets:?}"
    );

    // Fetch the asset bytes and parse.
    let asset = app
        .oneshot(
            Request::builder()
                .uri(format!("/jobs/{id}/results/result.json"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    assert_eq!(
        asset.headers().get("content-type").unwrap(),
        "application/json"
    );
    let v = body_to_json(asset).await;
    let result = v.as_f64().expect("result must be a JSON number");

    // ★ The math ★
    assert!(
        (result - 9.5).abs() < 1e-9,
        "expected ((10+5)*3-7)/4 = 9.5, got {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn arithmetic_chain_rejects_introduced_cycle() {
    // Modify the graph to introduce a cycle: divideNode now depends on save,
    // making it impossible to topologically order. Must reject at submit time
    // via ProcessGraphAnalysis::build (stage 4 of the decode pipeline).
    let bad = serde_json::json!({
        "process": { "process_graph": {
            "a": { "process_id": "add", "arguments": { "x": 1, "y": { "from_node": "b" } } },
            "b": { "process_id": "add", "arguments": { "x": 1, "y": { "from_node": "a" } } },
            "s": { "process_id": "save_result", "arguments": { "data": { "from_node": "a" } }, "result": true }
        }}
    });
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("content-type", "application/json")
                .body(Body::from(bad.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_to_json(resp).await;
    assert_eq!(v["code"], "ProcessGraphInvalid");
}
