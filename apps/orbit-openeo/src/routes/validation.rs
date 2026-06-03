//! `POST /validation` — validate a process graph against the openEO spec.
//!
//! Uses the [`SchemaRegistry`] to validate the request body against the
//! `process_graph` schema, returning a list of errors per the openEO 1.3
//! validation response envelope.

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};

use crate::{schema::SchemaError, AppState};

/// Mount the `/validation` route.
pub fn router() -> Router<AppState> {
    Router::new().route("/validation", post(validate))
}

async fn validate(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut errors: Vec<Value> = Vec::new();

    // 1. JSON-Schema validation against the pinned openapi spec, if loaded.
    // Try several schema names since openEO didn't standardise one: we attempt
    // `process_graph` and `process` (the wrapping shape used by `/result` /
    // `/jobs`).
    for candidate in ["process_graph", "process"] {
        if state.schemas.has(candidate) {
            match state.schemas.validate(candidate, &body) {
                Ok(()) => {}
                Err(SchemaError::Invalid { errors: e, .. }) => {
                    errors.extend(e.split("; ").map(|m| {
                        json!({ "code": "ValidationError", "message": m })
                    }));
                }
                Err(other) => errors.push(
                    json!({ "code": "ValidationError", "message": other.to_string() }),
                ),
            }
            break;
        }
    }

    // 2. **M4 (process audit)**: schema validation alone never catches a
    // graph that references an UNIMPLEMENTED process — it would validate
    // clean and only fail at run time. Cross-check every TOP-LEVEL process
    // id against the implemented set so `openeo.validate()` surfaces the
    // problem before submission. (Callback sub-graphs are validated at run
    // time by their owning process and may use callback-only processes — see
    // `collect_process_ids` P0-3 short-circuit.)
    let known = crate::process_catalog::process_ids();
    for bad in crate::process_graph::unsupported_process_ids(&body, &known) {
        errors.push(json!({
            "code": "ProcessUnsupported",
            "message": format!("process `{bad}` is not supported by this backend"),
        }));
    }

    Json(json!({ "errors": errors }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        Router::new()
            .merge(router())
            .with_state(AppStateBuilder::new().build())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_schema_registry_returns_no_errors() {
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/validation")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["errors"].as_array().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validation_flags_unsupported_process() {
        // M4: a graph referencing an unimplemented process must produce a
        // ProcessUnsupported error even with no JSON schema registered.
        let body = r#"{"process":{"process_graph":{
            "a":{"process_id":"definitely_not_real","arguments":{},"result":true}
        }}}"#;
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/validation")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let errs = v["errors"].as_array().unwrap();
        assert!(errs.iter().any(|e| e["code"] == "ProcessUnsupported"
            && e["message"].as_str().unwrap_or("").contains("definitely_not_real")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validation_passes_known_processes() {
        let body = r#"{"process":{"process_graph":{
            "a":{"process_id":"add","arguments":{"x":1,"y":2}},
            "s":{"process_id":"save_result","arguments":{"data":{"from_node":"a"},"format":"JSON"},"result":true}
        }}}"#;
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/validation")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await.unwrap();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["errors"].as_array().unwrap().is_empty(), "known graph must validate clean: {v}");
    }
}
