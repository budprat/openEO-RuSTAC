//! Tower middleware that enforces [`AuthPolicy`] per route, consulting
//! [`RouteSecurityMap`] derived from `openapi.json`.
//!
//! How it fits together:
//! 1. `RouteSecurityMap` is parsed at startup from the spec.
//! 2. This layer wraps the whole router.
//! 3. On every request we extract `axum::extract::MatchedPath` (the
//!    *templated* path like `/jobs/{job_id}`) and look it up.
//! 4. If the matched route declares any security alternatives, the
//!    request's `Authorization` header must satisfy the policy.
//! 5. If a route's `security: []` (or no `security` key, which OpenAPI
//!    treats as "public" when the doc has no global `security:` either)
//!    we let it through.
//!
//! The layer is intentionally a single function so it composes via
//! `axum::middleware::from_fn_with_state` rather than a hand-rolled
//! `tower::Service`.

use std::sync::Arc;

use axum::{
    extract::{MatchedPath, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::auth::{AuthOutcome, AuthPolicy};
use crate::security::RouteSecurityMap;

/// State the middleware needs.
#[derive(Clone)]
pub struct AuthLayerState {
    /// Required-scheme map parsed from openapi.json.
    pub routes: Arc<RouteSecurityMap>,
    /// Active policy.
    pub policy: Arc<AuthPolicy>,
}

impl AuthLayerState {
    /// New layer state.
    #[must_use]
    pub fn new(routes: Arc<RouteSecurityMap>, policy: Arc<AuthPolicy>) -> Self {
        Self { routes, policy }
    }
}

/// `axum::middleware::from_fn_with_state` handler.
pub async fn enforce(
    State(state): State<AuthLayerState>,
    req: Request,
    next: Next,
) -> Response {
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string());
    let method = req.method().as_str().to_string();

    let route_sec = matched
        .as_deref()
        .and_then(|p| state.routes.get(&method, p));

    // Public route (or route not declared in spec → pass through; the
    // router itself will 404 if it doesn't exist).
    if route_sec.map_or(true, |s| s.is_public()) {
        return next.run(req).await;
    }

    // Required schemes — pull the header and ask the policy.
    let header_val = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let outcome = state.policy.check(header_val.as_deref());

    match outcome {
        AuthOutcome::Authenticated | AuthOutcome::Open => next.run(req).await,
        AuthOutcome::Missing => unauthorized("missing Authorization header"),
        AuthOutcome::BadScheme => unauthorized("unsupported Authorization scheme"),
        AuthOutcome::BadCredentials => unauthorized("invalid credentials"),
    }
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer, Basic")],
        Json(json!({ "code": "AuthenticationRequired", "message": msg })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{RouteSecurity, RouteSecurityMap};
    use axum::{body::Body, routing::get, Router};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    fn map_with(method: &str, path: &str, sec: RouteSecurity) -> RouteSecurityMap {
        let security_arr: Vec<serde_json::Value> = sec.alternatives.iter()
            .map(|alt| {
                let mut m = serde_json::Map::new();
                for s in alt { m.insert(s.clone(), serde_json::json!([])); }
                serde_json::Value::Object(m)
            }).collect();
        let mut method_obj = serde_json::Map::new();
        method_obj.insert("security".into(), serde_json::Value::Array(security_arr));
        let mut path_obj = serde_json::Map::new();
        path_obj.insert(method.to_lowercase(), serde_json::Value::Object(method_obj));
        let mut paths_obj = serde_json::Map::new();
        paths_obj.insert(path.into(), serde_json::Value::Object(path_obj));
        let mut root = serde_json::Map::new();
        root.insert("paths".into(), serde_json::Value::Object(paths_obj));
        RouteSecurityMap::from_spec(&serde_json::Value::Object(root)).unwrap()
    }

    fn app(routes: RouteSecurityMap, policy: AuthPolicy) -> Router {
        let st = AuthLayerState::new(Arc::new(routes), Arc::new(policy));
        Router::new()
            .route("/public", get(|| async { "public" }))
            .route("/private", get(|| async { "private" }))
            .layer(axum::middleware::from_fn_with_state(st, enforce))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn route_not_in_map_passes_through() {
        // Empty map → /public has no security entry → public.
        let app = app(RouteSecurityMap::empty(), AuthPolicy::Bearer { token: "x".into() });
        let r = app
            .oneshot(axum::http::Request::builder().uri("/public").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn private_route_without_auth_returns_401() {
        let routes = map_with("get", "/private",
            RouteSecurity { alternatives: vec![vec!["Bearer".into()]] });
        let app = app(routes, AuthPolicy::Bearer { token: "secret".into() });
        let r = app
            .oneshot(axum::http::Request::builder().uri("/private").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 401);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "AuthenticationRequired");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn private_route_with_correct_bearer_returns_200() {
        let routes = map_with("get", "/private",
            RouteSecurity { alternatives: vec![vec!["Bearer".into()]] });
        let app = app(routes, AuthPolicy::Bearer { token: "secret".into() });
        let r = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty()).unwrap()
            )
            .await.unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn private_route_with_wrong_token_returns_401() {
        let routes = map_with("get", "/private",
            RouteSecurity { alternatives: vec![vec!["Bearer".into()]] });
        let app = app(routes, AuthPolicy::Bearer { token: "secret".into() });
        let r = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header("Authorization", "Bearer nope")
                    .body(Body::empty()).unwrap()
            )
            .await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_policy_short_circuits_private_route() {
        // Open policy → middleware passes everything through.
        let routes = map_with("get", "/private",
            RouteSecurity { alternatives: vec![vec!["Bearer".into()]] });
        let app = app(routes, AuthPolicy::Open);
        let r = app
            .oneshot(axum::http::Request::builder().uri("/private").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_public_route_allows_no_auth() {
        let routes = map_with("get", "/public",
            RouteSecurity { alternatives: vec![] }); // empty alternatives = public
        let app = app(routes, AuthPolicy::Bearer { token: "secret".into() });
        let r = app
            .oneshot(axum::http::Request::builder().uri("/public").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
    }
}
