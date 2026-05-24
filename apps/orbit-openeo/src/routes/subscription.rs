//! `GET /subscription` — WebSocket endpoint that pushes job-status events
//! to the connected client.
//!
//! For the initial skeleton we accept the upgrade, send a single hello
//! frame, and close. The real impl subscribes to the engine event bus
//! and relays Started/Progress/Completed/Failed/Cancelled events.

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};

use crate::AppState;

/// Mount `/subscription`.
pub fn router() -> Router<AppState> {
    Router::new().route("/subscription", get(ws_handler))
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    use futures::SinkExt;
    // Send hello + close. Future revisions subscribe to the engine bus.
    let _ = socket
        .send(Message::Text(
            r#"{"openeo.message":"connected","note":"event stream not yet implemented"}"#.into(),
        ))
        .await;
    let _ = SinkExt::close(&mut socket).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test(flavor = "current_thread")]
    async fn get_subscription_without_upgrade_returns_400() {
        let app = Router::new().merge(router()).with_state(AppStateBuilder::new().build());
        let r = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/subscription")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await.unwrap();
        // axum's ws extractor rejects non-upgrade requests with 400.
        assert!(r.status().is_client_error(), "got {}", r.status());
    }
}
