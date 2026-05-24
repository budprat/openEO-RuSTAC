//! User files routes — backed by [`FileStore`] from `AppState`.
//!
//! `GET /files/{user_id}` — list files for a user.
//! `GET /files/{user_id}/{path}` — download (full body for now;
//!    streaming variants will land once an `object_store`-backed store
//!    is wired and we have `Body::from_stream` exercised end-to-end).
//! `PUT /files/{user_id}/{path}` — upload from request body (full
//!    in-memory buffer today).
//! `DELETE /files/{user_id}/{path}` — delete.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::file_store::{FileError, FileKey};
use crate::AppState;

/// Mount file routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/files/{user_id}", get(list_files))
        .route(
            "/files/{user_id}/{path}",
            get(download_file).put(upload_file).delete(delete_file),
        )
}

async fn list_files(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Json<Value> {
    let entries = state.files.list(&user_id).await.unwrap_or_default();
    let files: Vec<Value> = entries
        .into_iter()
        .map(|e| json!({ "path": e.path, "size": e.size }))
        .collect();
    Json(json!({ "files": files, "links": [] }))
}

async fn download_file(
    State(state): State<AppState>,
    Path((user_id, path)): Path<(String, String)>,
) -> impl IntoResponse {
    let key = FileKey::new(user_id, path);
    match state.files.get(&key).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(FileError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "FilePathNotFound", "message": "file not found" })),
        )
            .into_response(),
        Err(FileError::Forbidden(p)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "FilePathInvalid", "message": p })),
        )
            .into_response(),
        Err(FileError::Io(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e })),
        )
            .into_response(),
    }
}

async fn upload_file(
    State(state): State<AppState>,
    Path((user_id, path)): Path<(String, String)>,
    body: Bytes,
) -> impl IntoResponse {
    let key = FileKey::new(user_id, path);
    match state.files.put(&key, body.to_vec()).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(FileError::Forbidden(p)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "FilePathInvalid", "message": p })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
        )
            .into_response(),
    }
}

async fn delete_file(
    State(state): State<AppState>,
    Path((user_id, path)): Path<(String, String)>,
) -> impl IntoResponse {
    let key = FileKey::new(user_id, path);
    match state.files.delete(&key).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(FileError::Forbidden(p)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "FilePathInvalid", "message": p })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
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
    async fn list_starts_empty() {
        let r = app()
            .oneshot(axum::http::Request::builder().uri("/files/alice").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["files"].as_array().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_then_get_roundtrips_payload() {
        let app = app();
        let r = app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/files/alice/scene.tif")
                    .body(Body::from(vec![1u8, 2, 3, 4]))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let r = app
            .oneshot(axum::http::Request::builder().uri("/files/alice/scene.tif").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], &[1u8, 2, 3, 4]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_then_list_includes_entry() {
        let app = app();
        let _ = app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/files/alice/a.bin")
                    .body(Body::from(vec![0u8; 5]))
                    .unwrap(),
            )
            .await.unwrap();
        let r = app
            .oneshot(axum::http::Request::builder().uri("/files/alice").body(Body::empty()).unwrap())
            .await.unwrap();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["size"], 5);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_removes_file() {
        let app = app();
        let _ = app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT").uri("/files/alice/x")
                    .body(Body::from(vec![9u8])).unwrap(),
            ).await.unwrap();
        let r = app.clone()
            .oneshot(axum::http::Request::builder()
                .method("DELETE").uri("/files/alice/x").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 204);
        let r = app
            .oneshot(axum::http::Request::builder().uri("/files/alice/x").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn download_unknown_404() {
        let r = app()
            .oneshot(axum::http::Request::builder().uri("/files/alice/missing.tif").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 404);
    }
}
