//! Discovery routes: `GET /`, `/.well-known/openeo`, `/credentials/*`, `/me`.

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

use crate::AppState;

/// Mount discovery routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(capabilities))
        .route("/.well-known/openeo", get(well_known))
        .route("/me", get(me))
}

/// `GET /` — Capabilities document per openEO 1.3.
async fn capabilities(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<Value> {
    Json(json!({
        "api_version": &*state.api_version,
        "backend_version": "0.1.0",
        "stac_version": "1.0.0",
        "id": &*state.backend_id,
        "title": "orbit-rs openEO",
        "description": "orbit-rs openEO 1.3.0 façade",
        "production": false,
        "endpoints": endpoint_list(),
        "links": [
            { "rel": "self",          "href": "/", "type": "application/json" },
            { "rel": "version-history", "href": "/.well-known/openeo", "type": "application/json" },
            { "rel": "data",          "href": "/collections", "type": "application/json" },
            { "rel": "conformance",   "href": "/conformance", "type": "application/json" },
            { "rel": "about",         "href": "https://github.com/NU/orbit-rs/blob/main/mvp/orbit-etl/apps/orbit-openeo/BACKEND-SCOPE.md", "type": "text/markdown", "title": "Backend scope contract (MAY / WILL NOT) — reference backend, NOT certified" }
        ],
        "scope": {
            "certified": false,
            "tenancy": "single-tenant",
            "spec_pin": "openEO 1.3.0",
            "contract": "https://github.com/NU/orbit-rs/blob/main/mvp/orbit-etl/apps/orbit-openeo/BACKEND-SCOPE.md"
        }
    }))
}

/// `GET /.well-known/openeo` — published API versions for client discovery.
async fn well_known(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<Value> {
    Json(json!({
        "versions": [{
            "url": "/",
            "production": false,
            "api_version": &*state.api_version
        }],
        "scope": {
            "certified": false,
            "contract": "https://github.com/NU/orbit-rs/blob/main/mvp/orbit-etl/apps/orbit-openeo/BACKEND-SCOPE.md"
        }
    }))
}

/// `GET /me` — user info for the currently-authenticated principal.
async fn me() -> Json<Value> {
    Json(json!({
        "user_id": "anonymous",
        "name": "anonymous"
    }))
}

/// Static endpoint list reported in the capabilities doc. Mirrors the
/// routes we actually serve.
fn endpoint_list() -> Vec<Value> {
    [
        ("/", &["GET"][..]),
        ("/.well-known/openeo", &["GET"]),
        ("/credentials/basic", &["GET"]),
        ("/credentials/oidc", &["GET"]),
        ("/me", &["GET"]),
        ("/collections", &["GET"]),
        ("/collections/{collection_id}", &["GET"]),
        ("/processes", &["GET"]),
        ("/process_graphs", &["GET", "POST"]),
        ("/process_graphs/{process_graph_id}", &["GET", "PATCH", "DELETE"]),
        ("/jobs", &["GET", "POST"]),
        ("/jobs/{job_id}", &["GET", "PATCH", "DELETE"]),
        ("/jobs/{job_id}/results", &["GET", "POST", "DELETE"]),
        ("/jobs/{job_id}/estimate", &["GET"]),
        ("/result", &["POST"]),
        ("/files/{user_id}", &["GET"]),
        ("/files/{user_id}/{path}", &["GET", "PUT", "DELETE"]),
        ("/services", &["GET", "POST"]),
        ("/services/{service_id}", &["GET", "PATCH", "DELETE"]),
        ("/service_types", &["GET"]),
        ("/output_formats", &["GET"]),
        ("/udf_runtimes", &["GET"]),
        ("/validation", &["POST"]),
        ("/subscription", &["GET"]),
    ]
    .iter()
    .map(|(path, methods)| json!({
        "path": path,
        "methods": methods,
    }))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        let state = AppStateBuilder::new().build();
        Router::new().merge(router()).with_state(state)
    }

    async fn body_to_json(resp: axum::http::Response<Body>) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_root_returns_capabilities() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        assert_eq!(v["api_version"], "1.3.0");
        assert_eq!(v["id"], "orbit-rs");
        assert!(v["endpoints"].as_array().unwrap().len() >= 20);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_well_known_lists_versions() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/.well-known/openeo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        assert_eq!(v["versions"][0]["api_version"], "1.3.0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_me_returns_anonymous() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        assert_eq!(v["user_id"], "anonymous");
    }

}
