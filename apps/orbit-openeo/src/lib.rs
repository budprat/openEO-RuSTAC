//! **orbit-openeo** — openEO API 1.3.0 façade for orbit-rs.
//!
//! Library entrypoint so tests can drive the router via
//! [`tower::ServiceExt::oneshot`] without binding a real TCP socket.
//!
//! Architectural pattern:
//! - `lib.rs` builds an `axum::Router<AppState>`
//! - `main.rs` constructs the `AppState` + binds + serves
//! - All route modules live under `routes::`, all auth interceptors
//!   under `auth::`
//! - `schema.rs` validates JSON bodies against the shipped openapi.json
//!   at request time

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod auth;
pub mod auth_layer;
pub mod catalog;
pub mod engine_bridge;
pub mod event_bus;
pub mod executor;
pub mod alignment;
pub mod block_executor;
pub mod chunk_plan;
pub mod datacube;
pub mod data_cube;
pub mod file_store;
#[cfg(feature = "geo-kernel")]
pub mod geo_executor;
pub mod process_graph_ir;
pub mod provenance;
pub mod storage_contracts;
pub mod typed_errors;
pub mod url_policy;
#[cfg(feature = "geo-kernel")]
pub mod typed_stac;
pub mod job_store;
pub mod process_graph;
pub mod routes;
pub mod runner;
pub mod schema;
pub mod security;
pub mod state;

pub use state::{AppState, AppStateBuilder};

/// Build the full Axum router for the openEO façade.
///
/// Routes are namespaced under the openEO `/v1.3` prefix per the spec's
/// `servers[0].url = https://localhost/api/{version}` convention. Discovery
/// routes (`/`, `/.well-known/openeo`) are mounted at the root.
pub fn build_router(state: AppState) -> axum::Router {
    use axum::Router;

    let auth_state = auth_layer::AuthLayerState::new(
        state.security.clone(),
        state.auth.clone(),
    );
    let api = Router::new()
        .merge(routes::discovery::router())
        .merge(routes::catalogs::router())
        .merge(routes::credentials::router())
        .merge(routes::files::router())
        .merge(routes::jobs::router())
        .merge(routes::result::router())
        .merge(routes::process_graphs::router())
        .merge(routes::services::router())
        .merge(routes::validation::router())
        .merge(routes::subscription::router())
        .merge(routes::spec::router())
        .merge(routes::products::router());

    Router::new()
        .merge(api)
        .layer(axum::middleware::from_fn_with_state(auth_state, auth_layer::enforce))
        // P0-2: cap request body to prevent unbounded memory growth.
        // 128 MiB suits openEO file uploads; clients needing larger
        // payloads should configure a higher cap via env / CLI.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(128 * 1024 * 1024))
        .with_state(state)
}
