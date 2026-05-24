//! `GET /products` — discovery endpoint backed by the workspace-shared
//! product registry in [`orbit_geo::products`]. The types here are pure
//! re-exports so CLI/gRPC consumers see the same catalog.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

use crate::AppState;

// Re-exports — single source of truth lives in orbit-geo.
pub use orbit_geo::products::{known_products, BandAliases, MaskKind, Product};

/// Mount `/products` routes.
pub fn router() -> Router<AppState> {
    Router::new().route("/products", get(list_products))
}

async fn list_products() -> Result<Json<Value>, Response> {
    let prods = known_products();
    // Reason: propagate serialization errors as 500 instead of panicking —
    // `Product` is trivially serializable today, but a future field addition
    // (e.g. a map with non-string keys) must not crash the handler.
    let arr: Vec<Value> = prods
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?;
    Ok(Json(json!({ "products": arr, "links": [] })))
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
    async fn get_products_returns_catalog_from_orbit_geo() {
        let resp = app()
            .oneshot(axum::http::Request::builder().uri("/products").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["products"].as_array().unwrap();
        assert!(arr.len() >= 4);
        assert!(arr.iter().any(|p| p["id"] == "sentinel-2-l2a"));
        let s2 = arr.iter().find(|p| p["id"] == "sentinel-2-l2a").unwrap();
        assert_eq!(s2["mask_kind"], "sentinel2_scl");
    }

    #[test]
    fn re_exported_types_match_orbit_geo_canon() {
        // Identity check: orbit-openeo::products::MaskKind === orbit_geo's.
        let m: MaskKind = orbit_geo::products::MaskKind::Sentinel2Scl;
        assert_eq!(m, MaskKind::Sentinel2Scl);
    }
}
