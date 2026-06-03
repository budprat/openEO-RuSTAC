//! Catalog routes: collections, processes, output_formats, service_types,
//! udf_runtimes.
//!
//! These are read-mostly JSON endpoints. Concrete catalogs are wired in
//! later by injecting `eo-catalog::StacClient` into the state.

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::catalog::CatalogError;
use crate::AppState;

/// Mount catalog routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/collections", get(list_collections))
        .route("/collections/{collection_id}", get(get_collection))
        .route("/processes", get(list_processes))
        .route("/output_formats", get(list_output_formats))
        .route("/service_types", get(list_service_types))
        .route("/udf_runtimes", get(list_udf_runtimes))
}

async fn list_collections(State(state): State<AppState>) -> Json<Value> {
    let collections = state.catalog.list().await.unwrap_or_default();
    Json(json!({ "collections": collections, "links": [] }))
}

async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (axum::http::StatusCode, Json<Value>) {
    match state.catalog.get(&id).await {
        Ok(c) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&c).unwrap_or_else(|_| json!({"id": id}))),
        ),
        Err(CatalogError::NotFound(_)) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "code": "CollectionNotFound", "message": format!("collection '{id}' not found") })),
        ),
        Err(CatalogError::Backend(msg)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": msg })),
        ),
    }
}

async fn list_processes() -> Json<Value> {
    // H1 (process audit): advertise the implemented process set. The openEO
    // API requires `/processes` to list every supported process with its
    // description; the previous `[]` stub made clients believe the backend
    // supported nothing. Source of truth is `process_catalog` (kept in
    // lock-step with the runtime registry by a geo-kernel test).
    Json(json!({
        "processes": crate::process_catalog::process_descriptions(),
        "links": [],
    }))
}

async fn list_output_formats() -> Json<Value> {
    Json(json!({
        "output": {
            "GTiff": { "title": "GeoTIFF", "gis_data_types": ["raster"], "parameters": {} },
            "COG":   { "title": "Cloud-Optimized GeoTIFF", "gis_data_types": ["raster"], "parameters": {} }
        }
    }))
}

async fn list_service_types() -> Json<Value> { Json(json!({})) }

async fn list_udf_runtimes() -> Json<Value> { Json(json!({})) }

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
    async fn list_collections_empty() {
        let resp = app()
            .oneshot(axum::http::Request::builder().uri("/collections").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_collection_returns_404() {
        let resp = app()
            .oneshot(axum::http::Request::builder().uri("/collections/landsat-5").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 404);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "CollectionNotFound");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_processes_advertises_implemented_set() {
        // H1 regression: `/processes` must NOT be empty and must describe
        // real processes (id + parameters + returns) so openEO clients can
        // discover capabilities.
        let resp = app()
            .oneshot(axum::http::Request::builder().uri("/processes").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let procs = v["processes"].as_array().expect("processes array");
        assert!(procs.len() >= 60, "expected the full implemented set, got {}", procs.len());
        let ids: Vec<&str> = procs.iter().filter_map(|p| p["id"].as_str()).collect();
        assert!(ids.contains(&"ndvi"));
        assert!(ids.contains(&"reduce_dimension"));
        assert!(ids.contains(&"load_collection"));
        // Each entry is spec-shaped.
        assert!(procs[0].get("parameters").and_then(|p| p.as_array()).is_some());
        assert!(procs[0].get("returns").is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_formats_includes_cog_and_gtiff() {
        let resp = app()
            .oneshot(axum::http::Request::builder().uri("/output_formats").body(Body::empty()).unwrap())
            .await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["output"]["COG"].is_object());
        assert!(v["output"]["GTiff"].is_object());
    }

    // ------------------------------------------------------------------
    // CollectionCatalog wiring.
    // ------------------------------------------------------------------

    use crate::catalog::{Collection, InMemoryCatalog};
    use std::sync::Arc;

    fn app_with_catalog(items: Vec<Collection>) -> axum::Router {
        let cat = Arc::new(InMemoryCatalog::with_collections(items));
        let state = AppStateBuilder::new().with_catalog(cat).build();
        Router::new().merge(router()).with_state(state)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_collections_returns_catalog_contents() {
        let app = app_with_catalog(vec![
            Collection::new("sentinel-2-l2a"),
            Collection::new("landsat-c2-l2"),
        ]);
        let resp = app
            .oneshot(axum::http::Request::builder().uri("/collections").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let cols = v["collections"].as_array().unwrap();
        assert_eq!(cols.len(), 2);
        let ids: Vec<&str> = cols.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"sentinel-2-l2a"));
        assert!(ids.contains(&"landsat-c2-l2"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_collection_returns_payload_for_known_id() {
        let app = app_with_catalog(vec![Collection::new("s2")]);
        let resp = app
            .oneshot(axum::http::Request::builder().uri("/collections/s2").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "s2");
        assert_eq!(v["stac_version"], "1.0.0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_collection_404_for_unknown_id_with_real_catalog() {
        let app = app_with_catalog(vec![Collection::new("only-this")]);
        let resp = app
            .oneshot(axum::http::Request::builder().uri("/collections/missing").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 404);
    }
}
