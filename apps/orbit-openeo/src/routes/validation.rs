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
    // Try several schema names since openEO didn't standardise one.
    // We attempt `process_graph` and `process` (the latter is the wrapping
    // shape used by `POST /result` / `POST /jobs`).
    for candidate in ["process_graph", "process"] {
        if state.schemas.has(candidate) {
            return match state.schemas.validate(candidate, &body) {
                Ok(()) => Json(json!({ "errors": [] })),
                Err(SchemaError::Invalid { errors, .. }) => Json(json!({
                    "errors": errors
                        .split("; ")
                        .map(|m| json!({ "code": "ValidationError", "message": m }))
                        .collect::<Vec<_>>()
                })),
                Err(other) => Json(json!({
                    "errors": [{ "code": "ValidationError", "message": other.to_string() }]
                })),
            };
        }
    }
    // No schemas registered — open mode for tests.
    Json(json!({ "errors": [] }))
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
}
