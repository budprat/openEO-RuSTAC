//! Stored process-graphs CRUD.

use axum::{
    extract::Path,
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::AppState;

/// Mount process_graphs routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/process_graphs", get(list).post(create))
        .route(
            "/process_graphs/{process_graph_id}",
            get(get_one).patch(update).delete(remove),
        )
}

async fn list() -> Json<Value> {
    // openEO `GET /process_graphs` returns user-defined process graphs under
    // the `process_graphs` key (NOT `processes` — that's `GET /processes`).
    // UDP storage is not implemented (BACKEND-SCOPE), so the list is empty.
    Json(json!({ "process_graphs": [], "links": [] }))
}

async fn create(Json(_pg): Json<Value>) -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn get_one(Path(_id): Path<String>) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "code": "ProcessGraphNotFound" })))
}

async fn update(Path(_id): Path<String>, Json(_pg): Json<Value>) -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn remove(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
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
}
