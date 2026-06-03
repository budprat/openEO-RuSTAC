//! Stored process-graphs CRUD.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::AppState;

/// Mount process_graphs (user-defined process graph) routes. openEO verbs:
/// `GET /process_graphs` (list), `GET/PUT/DELETE /process_graphs/{id}`.
/// UDPs are stored by client-chosen id via PUT (there is no POST in the
/// openEO UDP API).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/process_graphs", get(list))
        .route(
            "/process_graphs/{process_graph_id}",
            get(get_one).put(store).delete(remove),
        )
}

/// `GET /process_graphs` — list stored UDP summaries.
async fn list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "process_graphs": state.udp.list_summaries(), "links": [] }))
}

/// `GET /process_graphs/{id}` — fetch one stored UDP.
async fn get_one(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.udp.get(&id) {
        Some(udp) => (StatusCode::OK, Json(udp)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "ProcessGraphNotFound", "message": format!("process graph '{id}' not found") })),
        )
            .into_response(),
    }
}

/// `PUT /process_graphs/{id}` — store or replace a UDP. The body must be a
/// process object carrying a `process_graph`. Returns 200 on success.
async fn store(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if body.get("process_graph").and_then(|p| p.as_object()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "ProcessGraphInvalid", "message": "body must contain a `process_graph` object" })),
        )
            .into_response();
    }
    state.udp.put(&id, body);
    StatusCode::OK.into_response()
}

/// `DELETE /process_graphs/{id}` — remove a UDP. 204 on success, 404 if absent.
async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if state.udp.delete(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "ProcessGraphNotFound", "message": format!("process graph '{id}' not found") })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        Router::new().merge(router()).with_state(AppStateBuilder::new().build())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_empty() {
        let r = app()
            .oneshot(axum::http::Request::builder().uri("/process_graphs").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_unknown_404() {
        let r = app()
            .oneshot(axum::http::Request::builder().uri("/process_graphs/foo").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_put_get_list_delete_roundtrip() {
        // Share one state across requests so the store persists between calls.
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state);

        // PUT stores the UDP.
        let put = app.clone().oneshot(
            axum::http::Request::builder().method("PUT").uri("/process_graphs/my_udp")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"summary":"mine","process_graph":{"add1":{"process_id":"add","arguments":{"x":1,"y":2},"result":true}}}"#))
                .unwrap()).await.unwrap();
        assert_eq!(put.status(), 200);

        // GET returns it with the id set from the path.
        let got = app.clone().oneshot(
            axum::http::Request::builder().uri("/process_graphs/my_udp").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(got.status(), 200);
        let v: Value = serde_json::from_slice(&http_body_util::BodyExt::collect(got.into_body()).await.unwrap().to_bytes()).unwrap();
        assert_eq!(v["id"], "my_udp");
        assert!(v["process_graph"].is_object());

        // LIST shows the summary (without the graph).
        let list = app.clone().oneshot(
            axum::http::Request::builder().uri("/process_graphs").body(Body::empty()).unwrap()
        ).await.unwrap();
        let lv: Value = serde_json::from_slice(&http_body_util::BodyExt::collect(list.into_body()).await.unwrap().to_bytes()).unwrap();
        let arr = lv["process_graphs"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "my_udp");
        assert!(arr[0].get("process_graph").is_none());

        // DELETE removes it; second GET 404s.
        let del = app.clone().oneshot(
            axum::http::Request::builder().method("DELETE").uri("/process_graphs/my_udp").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(del.status(), 204);
        let gone = app.oneshot(
            axum::http::Request::builder().uri("/process_graphs/my_udp").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(gone.status(), 404);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_put_without_process_graph_is_400() {
        let app = app();
        let r = app.oneshot(
            axum::http::Request::builder().method("PUT").uri("/process_graphs/bad")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"summary":"no graph here"}"#)).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), 400);
    }
}
