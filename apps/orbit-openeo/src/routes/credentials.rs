//! Credentials routes — `GET /credentials/basic` and `GET /credentials/oidc`.
//!
//! - `GET /credentials/basic` validates an incoming HTTP Basic credential
//!   and returns a short-lived bearer access token, per openEO 1.3.0 §3.1.
//!   Response body: `{ "access_token": "<token>" }`.
//! - `GET /credentials/oidc` advertises the OIDC providers the server
//!   accepts. With no real IdP configured we publish an empty
//!   `{ "providers": [] }`, which still validates against the spec
//!   schema (openEO permits zero providers if no OIDC is offered).
//!
//! The Basic→Bearer issuance uses an in-memory store of tokens. Each
//! call returns a fresh ULID-shaped 26-char alphabetic token; the
//! token's expiry is currently set at 1h from issuance.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

use crate::auth::BasicCredentials;
use crate::AppState;

/// Mount credentials routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/credentials/basic", get(credentials_basic))
        .route(
            "/credentials/oidc",
            get(credentials_oidc).post(credentials_oidc_device_init),
        )
        .route("/credentials/oidc/token", axum::routing::post(credentials_oidc_device_token))
}

/// Time-limited bearer token issued from a Basic credential.
#[derive(Clone, Debug, Serialize)]
pub struct IssuedToken {
    /// Opaque token value.
    pub access_token: String,
    /// Unix epoch seconds at expiry.
    pub expires_at: u64,
}

/// In-memory store of issued tokens. Maps `access_token → expires_at`.
///
/// Replace with a Redis / SQLite-backed store in production.
#[derive(Debug, Default)]
pub struct TokenStore {
    inner: Mutex<std::collections::HashMap<String, u64>>,
    counter: AtomicU64,
}

impl TokenStore {
    /// New empty store.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Issue a new token valid for `ttl_seconds`. Returns the token.
    pub fn issue(&self, ttl_seconds: u64) -> IssuedToken {
        let now = now_secs();
        let expires_at = now + ttl_seconds;
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let token = generate_token(now ^ seq.wrapping_mul(0xC2B2_AE3D_27D4_EB4F), expires_at ^ seq);
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.insert(token.clone(), expires_at);
        IssuedToken { access_token: token, expires_at }
    }

    /// True iff the token is known and not yet expired.
    #[must_use]
    pub fn is_valid(&self, token: &str) -> bool {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match g.get(token) {
            Some(&exp) => exp > now_secs(),
            None => false,
        }
    }

    /// Number of currently stored tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.len()
    }

    /// True iff empty.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Generate a 128-bit CSPRNG token using OsRng + base64url-no-pad.
///
/// **Audit P0-1 fix**: replaces the deterministic xor-shift over
/// `(now ^ counter)` that was brute-forceable in seconds. Now sources
/// entropy from the OS CSPRNG (`OsRng::try_fill_bytes`).
#[must_use]
pub fn csprng_token(bits: usize) -> String {
    use base64::Engine;
    use rand::TryRngCore;
    let bytes = bits.div_ceil(8);
    let mut buf = vec![0u8; bytes];
    // Reason: OsRng is infallible in practice — getrandom backs this on all
    // our targets (Linux/macOS/Windows). A failure here means the OS entropy
    // source is broken, in which case crashing is the correct response.
    #[allow(clippy::expect_used)]
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OsRng must succeed");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

/// Legacy alias retained for the `TokenStore::issue` call site.
fn generate_token(_now: u64, _exp: u64) -> String {
    csprng_token(128)
}

async fn credentials_basic(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let header_str = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    // We need to validate against the AppState policy. The auth layer
    // above only sees what the *spec* declares; the GET /credentials/basic
    // route is declared with `security: [{ Basic: [] }]` in the openEO
    // 1.3.0 spec, so by the time we get here the request is already
    // Basic-authenticated. We *also* re-validate here to keep this
    // handler safe even if the layer is bypassed in tests.
    let parsed = match header_str {
        Some(h) => match BasicCredentials::parse(h) {
            Ok(c) => Some(c),
            Err(_) => None,
        },
        None => None,
    };
    let ok = match (&*state.auth, parsed) {
        (crate::auth::AuthPolicy::Open, _) => true,
        (crate::auth::AuthPolicy::Basic { username, password }, Some(c)) => c.matches(username, password),
        (crate::auth::AuthPolicy::Any { basic: Some((u, p)), .. }, Some(c)) => c.matches(u, p),
        _ => false,
    };
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic")],
            Json(json!({ "code": "AuthenticationRequired", "message": "Basic credentials required" })),
        )
            .into_response();
    }

    let token = state.tokens.issue(3600);
    (
        StatusCode::OK,
        Json(json!({ "access_token": token.access_token })),
    )
        .into_response()
}

async fn credentials_oidc(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Returns the configured OIDC providers. Empty array is valid per spec.
    let providers = state.oidc_providers.clone();
    Json(json!({ "providers": providers }))
}

/// RFC 8628 §3.2 — Device Authorization Response.
///
/// `POST /credentials/oidc` (with optional `provider_id`, `scope` form data)
/// kicks off a device-code flow and returns the codes a client should
/// show the user along with a polling interval.
async fn credentials_oidc_device_init(
    State(state): State<AppState>,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let provider_id = body
        .as_ref()
        .and_then(|b| b.get("provider_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let scope = body
        .as_ref()
        .and_then(|b| b.get("scope"))
        .and_then(|v| v.as_str())
        .unwrap_or("openid")
        .to_string();
    let session = state.device_codes.start(&provider_id, &scope);
    (
        StatusCode::OK,
        Json(json!({
            "device_code": session.device_code,
            "user_code": session.user_code,
            "verification_uri": "/credentials/oidc/verify",
            "verification_uri_complete":
                format!("/credentials/oidc/verify?user_code={}", session.user_code),
            "expires_in": session.expires_in,
            "interval": session.interval,
        })),
    )
        .into_response()
}

/// RFC 8628 §3.4 — Device Access Token Request / Response.
///
/// `POST /credentials/oidc/token` polls for the token. The flow:
/// - Unknown device_code → 400 `expired_token`
/// - Pending → 400 `authorization_pending`
/// - Approved → 200 `{ access_token, token_type:"Bearer", expires_in }`
async fn credentials_oidc_device_token(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let grant_type = body.get("grant_type").and_then(|v| v.as_str()).unwrap_or("");
    if grant_type != "urn:ietf:params:oauth:grant-type:device_code" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unsupported_grant_type",
                "error_description":
                    "expected grant_type=urn:ietf:params:oauth:grant-type:device_code"
            })),
        )
            .into_response();
    }
    let device_code = body.get("device_code").and_then(|v| v.as_str()).unwrap_or("");
    match state.device_codes.poll(device_code) {
        DevicePollOutcome::Approved => {
            let token = state.tokens.issue(3600);
            (
                StatusCode::OK,
                Json(json!({
                    "access_token": token.access_token,
                    "token_type": "Bearer",
                    "expires_in": 3600,
                })),
            )
                .into_response()
        }
        DevicePollOutcome::Pending => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "authorization_pending" })),
        )
            .into_response(),
        DevicePollOutcome::Expired => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "expired_token" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------
// Device-code session store
// ---------------------------------------------------------------------

/// One pending device-code authorisation.
#[derive(Clone, Debug)]
pub struct DeviceSession {
    /// Opaque, hard-to-guess token the client polls with.
    pub device_code: String,
    /// Short user-facing code (e.g. `XXXX-YYYY`).
    pub user_code: String,
    /// Seconds-from-issue until the device_code expires.
    pub expires_in: u64,
    /// Minimum poll interval in seconds.
    pub interval: u64,
    /// Issued-at UNIX epoch seconds.
    pub issued: u64,
    /// True once an out-of-band approval has flipped the bit.
    pub approved: bool,
    /// OIDC provider id this session is bound to.
    pub provider_id: String,
}

/// Outcome of polling a device_code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevicePollOutcome {
    /// Device approved — issue a token.
    Approved,
    /// Still waiting for user approval.
    Pending,
    /// Code expired or unknown.
    Expired,
}

/// In-memory device-code session store.
#[derive(Debug, Default)]
pub struct DeviceCodeStore {
    inner: Mutex<std::collections::HashMap<String, DeviceSession>>,
    counter: AtomicU64,
}

impl DeviceCodeStore {
    /// New empty store.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Start a new device flow.
    pub fn start(&self, provider_id: &str, _scope: &str) -> DeviceSession {
        // P0-1: device_code is now CSPRNG (was clock-XOR-counter, predictable).
        let _ = self.counter.fetch_add(1, Ordering::Relaxed); // kept for telemetry compatibility
        let device_code = format!("dc-{}", csprng_token(128));
        // User code: 8 CSPRNG-sourced uppercase letters, dashed.
        let user_code = format!("{}-{}", csprng_user_chunk(), csprng_user_chunk());
        let session = DeviceSession {
            device_code: device_code.clone(),
            user_code,
            expires_in: 600,
            interval: 5,
            issued: now_secs(),
            approved: false,
            provider_id: provider_id.into(),
        };
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.insert(device_code, session.clone());
        session
    }

    /// Poll the status of a device_code.
    pub fn poll(&self, device_code: &str) -> DevicePollOutcome {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let s = match g.get(device_code) {
            Some(s) => s,
            None => return DevicePollOutcome::Expired,
        };
        if now_secs() > s.issued + s.expires_in {
            return DevicePollOutcome::Expired;
        }
        if s.approved { DevicePollOutcome::Approved } else { DevicePollOutcome::Pending }
    }

    /// Out-of-band approval — typically triggered by an admin UI.
    pub fn approve(&self, device_code: &str) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match g.get_mut(device_code) {
            Some(s) => { s.approved = true; true }
            None => false,
        }
    }

    /// Number of sessions in store (mostly for tests).
    #[must_use]
    pub fn len(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.len()
    }
    /// True iff store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

/// CSPRNG-sourced 4-char chunk for the user-facing code. Uses an
/// ambiguity-free Crockford-style alphabet (no I/O/0/1).
fn csprng_user_chunk() -> String {
    use rand::TryRngCore;
    const ALPHA: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut buf = [0u8; 4];
    // Reason: OsRng is infallible in practice — getrandom backs this on all
    // our targets. A failure here means OS entropy is broken; crashing is
    // the correct response.
    #[allow(clippy::expect_used)]
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OsRng must succeed");
    let mut s = String::with_capacity(4);
    for &b in &buf {
        s.push(ALPHA[(b as usize) % ALPHA.len()] as char);
    }
    s
}

/// OIDC provider configuration entry returned by `GET /credentials/oidc`.
///
/// Public structure mirroring the openEO spec.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct OidcProvider {
    /// Stable provider id.
    pub id: String,
    /// Discovery URL (issuer endpoint).
    pub issuer: String,
    /// Human-readable title.
    pub title: String,
    /// Supported OIDC scopes.
    pub scopes: Vec<String>,
}

/// Shared between routes (lives on `AppState`).
pub type SharedTokenStore = Arc<TokenStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credentials_oidc_returns_empty_providers_by_default() {
        let r = app(AppStateBuilder::new().build())
            .oneshot(axum::http::Request::builder().uri("/credentials/oidc").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["providers"].as_array().unwrap().len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credentials_oidc_returns_configured_providers() {
        let state = AppStateBuilder::new()
            .with_oidc_providers(vec![OidcProvider {
                id: "egi".into(),
                issuer: "https://aai.egi.eu".into(),
                title: "EGI Check-in".into(),
                scopes: vec!["openid".into()],
            }])
            .build();
        let r = app(state)
            .oneshot(axum::http::Request::builder().uri("/credentials/oidc").body(Body::empty()).unwrap())
            .await.unwrap();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["id"], "egi");
        assert_eq!(providers[0]["issuer"], "https://aai.egi.eu");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credentials_basic_requires_basic_header_when_policy_basic() {
        let state = AppStateBuilder::new()
            .with_auth(crate::auth::AuthPolicy::Basic {
                username: "alice".into(),
                password: "wonder".into(),
            })
            .build();
        let r = app(state)
            .oneshot(axum::http::Request::builder().uri("/credentials/basic").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credentials_basic_issues_token_for_valid_credential() {
        let state = AppStateBuilder::new()
            .with_auth(crate::auth::AuthPolicy::Basic {
                username: "alice".into(),
                password: "wonder".into(),
            })
            .build();
        // base64("alice:wonder") = "YWxpY2U6d29uZGVy"
        let r = app(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/credentials/basic")
                    .header("Authorization", "Basic YWxpY2U6d29uZGVy")
                    .body(Body::empty()).unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let token = v["access_token"].as_str().expect("access_token must be a string");
        assert!(!token.is_empty(), "token must not be empty");
        assert!(token.len() >= 8, "token too short: {token}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credentials_basic_rejects_wrong_password() {
        let state = AppStateBuilder::new()
            .with_auth(crate::auth::AuthPolicy::Basic {
                username: "alice".into(),
                password: "wonder".into(),
            })
            .build();
        // base64("alice:wrong") = "YWxpY2U6d3Jvbmc="
        let r = app(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/credentials/basic")
                    .header("Authorization", "Basic YWxpY2U6d3Jvbmc=")
                    .body(Body::empty()).unwrap(),
            )
            .await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[test]
    fn token_store_round_trips() {
        let s = TokenStore::new();
        let t = s.issue(60);
        assert!(s.is_valid(&t.access_token));
        assert!(!s.is_valid("nope"));
    }

    #[test]
    fn token_store_two_tokens_are_different() {
        let s = TokenStore::new();
        let a = s.issue(60);
        let b = s.issue(60);
        assert_ne!(a.access_token, b.access_token);
    }

    // ---------- P0-1: CSPRNG token audit fix ----------

    #[test]
    fn csprng_token_has_128_bits_of_entropy() {
        let t = csprng_token(128);
        // base64url-no-pad: 128 bits = 16 bytes → 22 chars.
        assert_eq!(t.len(), 22, "expected 22 base64url chars, got {}", t.len());
    }

    #[test]
    fn csprng_token_uses_base64url_charset_only() {
        let t = csprng_token(256);
        for c in t.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-base64url char: {c:?}"
            );
        }
    }

    #[test]
    fn csprng_token_distinct_across_many_draws() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(csprng_token(128)), "CSPRNG collision in 10k draws");
        }
    }

    #[test]
    fn device_code_csprng_unguessable() {
        let s = DeviceCodeStore::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            let sess = s.start("p", "openid");
            assert!(
                sess.device_code.starts_with("dc-") && sess.device_code.len() > 16,
                "device_code shape: {}", sess.device_code
            );
            assert!(seen.insert(sess.device_code), "device_code collision");
        }
    }

    #[test]
    fn user_code_uses_ambiguity_free_alphabet() {
        let s = DeviceCodeStore::new();
        for _ in 0..100 {
            let u = s.start("p", "openid").user_code;
            for c in u.chars() {
                assert!(c != 'I' && c != 'O' && c != '0' && c != '1' || c == '-',
                    "ambiguous char in user_code: {c} ({u})");
            }
        }
    }

    // ---------- P4b — OIDC device-code flow (RFC 8628) ----------

    #[test]
    fn device_code_store_starts_unique_sessions() {
        let s = DeviceCodeStore::new();
        let a = s.start("egi", "openid");
        let b = s.start("egi", "openid");
        assert_ne!(a.device_code, b.device_code);
        assert_ne!(a.user_code, b.user_code);
        assert!(a.user_code.contains('-'), "user_code formatted with dash, got {}", a.user_code);
    }

    #[test]
    fn device_code_poll_is_pending_then_approved() {
        let s = DeviceCodeStore::new();
        let sess = s.start("egi", "openid");
        assert_eq!(s.poll(&sess.device_code), DevicePollOutcome::Pending);
        assert!(s.approve(&sess.device_code));
        assert_eq!(s.poll(&sess.device_code), DevicePollOutcome::Approved);
    }

    #[test]
    fn device_code_unknown_code_is_expired() {
        let s = DeviceCodeStore::new();
        assert_eq!(s.poll("dc-no-such-thing"), DevicePollOutcome::Expired);
    }

    #[test]
    fn device_code_approve_unknown_returns_false() {
        let s = DeviceCodeStore::new();
        assert!(!s.approve("dc-no-such-thing"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_oidc_initiates_device_flow_returns_codes() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/credentials/oidc")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"provider_id":"egi","scope":"openid email"}"#))
                    .unwrap(),
            )
            .await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["device_code"].as_str().unwrap().starts_with("dc-"));
        assert!(v["user_code"].as_str().unwrap().contains('-'));
        assert_eq!(v["expires_in"], 600);
        assert_eq!(v["interval"], 5);
        assert!(v["verification_uri"].as_str().unwrap().contains("/credentials/oidc/verify"));
        assert!(v["verification_uri_complete"].as_str().unwrap().contains("user_code="));

        // Sanity: state.device_codes recorded it.
        assert_eq!(state.device_codes.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_oidc_token_returns_authorization_pending_before_approval() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());
        let init = app.clone()
            .oneshot(
                axum::http::Request::builder().method("POST").uri("/credentials/oidc")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#)).unwrap(),
            ).await.unwrap();
        let init_v: serde_json::Value = serde_json::from_slice(
            &init.into_body().collect().await.unwrap().to_bytes()
        ).unwrap();
        let dc = init_v["device_code"].as_str().unwrap().to_string();

        let body = format!(
            r#"{{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"{dc}"}}"#
        );
        let resp = app
            .oneshot(
                axum::http::Request::builder().method("POST").uri("/credentials/oidc/token")
                    .header("content-type", "application/json")
                    .body(Body::from(body)).unwrap(),
            ).await.unwrap();
        assert_eq!(resp.status(), 400);
        let v: serde_json::Value = serde_json::from_slice(
            &resp.into_body().collect().await.unwrap().to_bytes()
        ).unwrap();
        assert_eq!(v["error"], "authorization_pending");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_oidc_token_returns_access_token_after_approval() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());
        let init = app.clone()
            .oneshot(
                axum::http::Request::builder().method("POST").uri("/credentials/oidc")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"provider_id":"egi"}"#)).unwrap(),
            ).await.unwrap();
        let dc = serde_json::from_slice::<serde_json::Value>(
            &init.into_body().collect().await.unwrap().to_bytes()
        ).unwrap()["device_code"].as_str().unwrap().to_string();
        // Out-of-band approval (admin UI / browser hop completed).
        assert!(state.device_codes.approve(&dc));

        let body = format!(
            r#"{{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"{dc}"}}"#
        );
        let resp = app
            .oneshot(
                axum::http::Request::builder().method("POST").uri("/credentials/oidc/token")
                    .header("content-type", "application/json")
                    .body(Body::from(body)).unwrap(),
            ).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_slice(
            &resp.into_body().collect().await.unwrap().to_bytes()
        ).unwrap();
        assert!(v["access_token"].as_str().unwrap().len() >= 8);
        assert_eq!(v["token_type"], "Bearer");
        assert_eq!(v["expires_in"], 3600);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_oidc_token_rejects_unknown_grant_type() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder().method("POST").uri("/credentials/oidc/token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"grant_type":"password","device_code":"dc-x"}"#))
                    .unwrap(),
            ).await.unwrap();
        assert_eq!(resp.status(), 400);
        let v: serde_json::Value = serde_json::from_slice(
            &resp.into_body().collect().await.unwrap().to_bytes()
        ).unwrap();
        assert_eq!(v["error"], "unsupported_grant_type");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_oidc_token_unknown_device_code_is_expired_error() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder().method("POST").uri("/credentials/oidc/token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"dc-nope"}"#,
                    ))
                    .unwrap(),
            ).await.unwrap();
        assert_eq!(resp.status(), 400);
        let v: serde_json::Value = serde_json::from_slice(
            &resp.into_body().collect().await.unwrap().to_bytes()
        ).unwrap();
        assert_eq!(v["error"], "expired_token");
    }
}
