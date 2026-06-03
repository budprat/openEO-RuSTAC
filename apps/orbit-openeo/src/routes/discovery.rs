//! Discovery routes: `GET /`, `/.well-known/openeo`, `/credentials/*`, `/me`.

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

use crate::AppState;

/// Mount discovery routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(capabilities))
        .route("/.well-known/openeo", get(well_known))
        // openEO/STAC-API required: the capabilities `conformance` link points
        // here. Was advertised but unmounted (404) before this fix.
        .route("/conformance", get(conformance))
        .route("/me", get(me))
        // audit-fix (2026-06-03): unauthenticated liveness/readiness probe for
        // container orchestrators (k8s). Not part of the openEO spec, so it is
        // absent from the security map → public by default (auth_layer.rs).
        .route("/health", get(health))
        // audit-fix (2026-06-03): Prometheus scrape endpoint. Also unmapped →
        // public (scrapers are unauthenticated). Exposes the recorder series.
        .route("/metrics", get(metrics))
}

/// `GET /health` — liveness/readiness probe. Returns `200 {"status":"ok"}`.
/// Cheap and dependency-free: a `200` means the process is up and the router
/// is serving. Intended for k8s `livenessProbe`/`readinessProbe`.
async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// `GET /metrics` — Prometheus exposition of the recorder's series
/// (`text/plain; version=0.0.4`). Empty body when the recorder exports
/// nothing. Intended for a Prometheus scrape job.
async fn metrics(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render_prometheus(),
    )
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
        // openEO 1.1+/STAC-API: advertise the conformance classes inline in
        // the capabilities document (mirrors GET /conformance).
        "conformsTo": conformance_classes(),
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

/// `GET /conformance` — the conformance classes this backend claims, per
/// the openEO API + STAC API. Required: the capabilities `conformance` link
/// resolves here. We claim only the classes the implemented surface actually
/// satisfies (core + collections); we deliberately do NOT claim certification.
async fn conformance() -> Json<Value> {
    Json(json!({ "conformsTo": conformance_classes() }))
}

/// Conformance-class URIs shared by `GET /` and `GET /conformance`.
fn conformance_classes() -> Vec<&'static str> {
    vec![
        // General openEO API conformance class (per the bundled
        // spec/openapi.json, openEO 1.3.0). `conformsTo` in GET / and
        // GET /conformance MUST be equal — both call this fn.
        "https://api.openeo.org/1.3.0",
        // STAC API classes backing /collections.
        "https://api.stacspec.org/v1.0.0/core",
        "https://api.stacspec.org/v1.0.0/collections",
    ]
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
        // audit-fix (2026-06-03): the advertised set now mirrors the routes
        // actually mounted. Added /conformance, /file_formats, /jobs/{id}/logs;
        // corrected the OIDC token path; dropped the unmounted GET
        // /credentials/oidc discovery endpoint.
        ("/", &["GET"][..]),
        ("/.well-known/openeo", &["GET"]),
        ("/conformance", &["GET"]),
        ("/credentials/basic", &["GET"]),
        ("/credentials/oidc", &["GET", "POST"]),
        ("/credentials/oidc/token", &["POST"]),
        ("/me", &["GET"]),
        ("/collections", &["GET"]),
        ("/collections/{collection_id}", &["GET"]),
        ("/processes", &["GET"]),
        ("/process_graphs", &["GET"]),
        ("/process_graphs/{process_graph_id}", &["GET", "PUT", "DELETE"]),
        ("/jobs", &["GET", "POST"]),
        ("/jobs/{job_id}", &["GET", "PATCH", "DELETE"]),
        ("/jobs/{job_id}/results", &["GET", "POST", "DELETE"]),
        ("/jobs/{job_id}/estimate", &["GET"]),
        ("/jobs/{job_id}/logs", &["GET"]),
        ("/result", &["POST"]),
        ("/files/{user_id}", &["GET"]),
        ("/files/{user_id}/{path}", &["GET", "PUT", "DELETE"]),
        ("/services", &["GET", "POST"]),
        ("/services/{service_id}", &["GET", "PATCH", "DELETE"]),
        ("/service_types", &["GET"]),
        ("/file_formats", &["GET"]),
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
    async fn get_health_returns_ok() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        assert_eq!(v["status"], "ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_metrics_returns_prometheus_text() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("text/plain"), "Prometheus content-type, got {ct}");
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
        // Capabilities advertises conformance classes inline.
        let conforms = v["conformsTo"].as_array().expect("conformsTo array");
        assert!(conforms.iter().any(|c| c == "https://api.openeo.org/1.3.0"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_conformance_returns_conforms_to_and_matches_capabilities() {
        let resp = app()
            .oneshot(axum::http::Request::builder().uri("/conformance").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        let classes = v["conformsTo"].as_array().expect("conformsTo array");
        // Spec: GET / and GET /conformance MUST list equal classes.
        assert_eq!(classes, &super::conformance_classes().into_iter()
            .map(serde_json::Value::from).collect::<Vec<_>>());
        assert!(classes.iter().any(|c| c == "https://api.openeo.org/1.3.0"));
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
