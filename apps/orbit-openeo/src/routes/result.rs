//! `POST /result` — synchronous compute. Dispatches through the
//! [`ProcessGraphExecutor`] in [`AppState`].

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

use crate::executor::ExecError;
use crate::AppState;

/// Mount the `/result` route.
pub fn router() -> Router<AppState> {
    Router::new().route("/result", post(sync_compute))
}

async fn sync_compute(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    match state.executor.run_sync(&body).await {
        Ok(result) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, result.content_type.clone())],
            result.body,
        )
            .into_response(),
        Err(ExecError::InvalidGraph(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "InvalidProcessGraph", "message": msg })),
        )
            .into_response(),
        Err(ExecError::UnknownProcess(name)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "ProcessUnsupported", "message": name })),
        )
            .into_response(),
        Err(ExecError::Backend(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": msg })),
        )
            .into_response(),
        // B4: per-pixel arithmetic errors leaked here are structural bugs;
        // surface them at the boundary so they're not silently lost.
        Err(ExecError::PerPixelComputation(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": format!("per_pixel: {msg}") })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        Router::new().merge(router()).with_state(AppStateBuilder::new().build())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_result_evaluates_graph_on_default_local_executor() {
        // LocalExecutor evaluates `add` then `save_result` and returns 7.
        let body = r#"{"process":{"process_graph":{
            "a":{"process_id":"add","arguments":{"x":3,"y":4}},
            "s":{"process_id":"save_result","arguments":{"data":{"from_node":"a"}},"result":true}
        }}}"#;
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/result")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.as_f64().unwrap(), 7.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_result_400_on_unknown_process() {
        let body = r#"{"process":{"process_graph":{
            "a":{"process_id":"unknown_process","result":true}
        }}}"#;
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/result")
                    .header("content-type", "application/json")
                    .body(Body::from(body)).unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 400);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "ProcessUnsupported");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_result_400_on_missing_process_graph() {
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/result")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 400);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "InvalidProcessGraph");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_result_400_on_empty_process_graph() {
        let r = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/result")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"process":{"process_graph":{}}}"#))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 400);
    }
}
