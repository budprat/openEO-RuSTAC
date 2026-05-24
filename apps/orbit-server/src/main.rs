//! orbit-server — gRPC server exposing the ETL engine.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use clap::Parser;
use orbit_etl::Engine;
use std::net::SocketAddr;
use tracing_subscriber::{prelude::*, EnvFilter};

mod service;

#[derive(Parser, Debug)]
#[command(name = "orbit-server", version, about = "orbit ETL gRPC server")]
struct Args {
    /// gRPC listen address.
    #[arg(long, env = "ORBIT_BIND", default_value = "127.0.0.1:9876")]
    bind: SocketAddr,

    /// SQLite database URL.
    #[arg(long, env = "ORBIT_DB", default_value = "sqlite://./data/orbit.db?mode=rwc")]
    db: String,

    /// Restrict every pipeline source path to canonicalise under this
    /// directory. Strongly recommended whenever `--bind` is non-loopback or
    /// the gRPC server is exposed to untrusted clients. Defaults to unset
    /// (developer mode; any locally-readable file may be ingested).
    #[arg(long, env = "ORBIT_DATA_ROOT")]
    data_root: Option<std::path::PathBuf>,

    /// Maximum decoded gRPC request body (bytes). Defaults to 4 MiB.
    #[arg(long, env = "ORBIT_MAX_MSG_BYTES", default_value_t = 4 * 1024 * 1024)]
    max_msg_bytes: usize,

    /// Per-connection concurrent request limit. Defaults to 32.
    #[arg(long, env = "ORBIT_CONCURRENCY", default_value_t = 32)]
    concurrency: usize,

    /// Per-request server timeout in seconds. Defaults to 600 (10 min).
    #[arg(long, env = "ORBIT_REQUEST_TIMEOUT_SECS", default_value_t = 600)]
    request_timeout_secs: u64,

    /// Optional bearer token required from clients. When non-loopback bind
    /// is configured this is enforced: refuses to start without a token.
    #[arg(long, env = "ORBIT_AUTH_TOKEN")]
    auth_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    // Ensure the data directory exists for default file-based SQLite.
    if let Some(path) = args.db.strip_prefix("sqlite://") {
        if let Some(file) = path.split('?').next() {
            if let Some(parent) = std::path::Path::new(file).parent() {
                if !parent.as_os_str().is_empty() { std::fs::create_dir_all(parent).ok(); }
            }
        }
    }

    let engine = Engine::open(&args.db).await?;
    let engine = if let Some(root) = &args.data_root {
        // Canonicalise once at startup so later canonicalize() calls compare apples-to-apples.
        let canon = root.canonicalize().map_err(|e| {
            anyhow::anyhow!("ORBIT_DATA_ROOT {root:?} does not exist or is not readable: {e}")
        })?;
        tracing::info!(data_root = %canon.display(), "source paths restricted to data_root");
        engine.with_data_root(canon)
    } else {
        if !args.bind.ip().is_loopback() {
            tracing::warn!(
                addr = %args.bind,
                "ORBIT_DATA_ROOT not set AND bind is non-loopback — unauthenticated callers can request any locally-readable file. Set ORBIT_DATA_ROOT to restrict source paths."
            );
        } else {
            tracing::warn!("ORBIT_DATA_ROOT not set — any locally-readable file may be ingested (developer mode)");
        }
        engine
    };
    let svc = service::EtlServer::new(engine);

    // Refuse to start with non-loopback bind and no auth token configured.
    if !args.bind.ip().is_loopback() && args.auth_token.is_none() {
        anyhow::bail!(
            "non-loopback bind {bind} requires ORBIT_AUTH_TOKEN to be set. \
             Refusing to expose an unauthenticated gRPC service.",
            bind = args.bind
        );
    }

    tracing::info!(
        addr = %args.bind,
        max_msg_bytes = args.max_msg_bytes,
        concurrency = args.concurrency,
        request_timeout_secs = args.request_timeout_secs,
        auth = args.auth_token.is_some(),
        "orbit-server listening",
    );

    use orbit_proto::etl::v1::etl_service_server::EtlServiceServer;

    // Bound the decoded payload before we look at it.
    let inner_svc = EtlServiceServer::new(svc).max_decoding_message_size(args.max_msg_bytes);

    // Wrap with an interceptor that enforces Bearer-token auth when configured.
    let auth_token = args.auth_token.clone();
    let intercepted = tonic::service::interceptor::InterceptedService::new(
        inner_svc,
        move |req: tonic::Request<()>| authenticate_request(req, auth_token.as_deref()),
    );

    let server = tonic::transport::Server::builder()
        .concurrency_limit_per_connection(args.concurrency)
        .timeout(std::time::Duration::from_secs(args.request_timeout_secs))
        .add_service(intercepted)
        .serve_with_shutdown(args.bind, shutdown_signal());

    server.await?;
    Ok(())
}

/// Validate the request's Authorization header against the configured token.
///
/// - If no token is configured (`expected_token == None`), all requests are
///   accepted. The startup check guarantees this only happens on loopback.
/// - If a token is configured, the request must carry a matching
///   `Authorization: Bearer <token>` header.
fn authenticate_request(
    req: tonic::Request<()>,
    expected_token: Option<&str>,
) -> std::result::Result<tonic::Request<()>, tonic::Status> {
    let Some(expected) = expected_token else {
        return Ok(req);
    };
    let header = req
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| tonic::Status::unauthenticated("missing authorization header"))?;
    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| tonic::Status::unauthenticated("authorization must be Bearer scheme"))?;
    // Constant-time compare to avoid timing oracles.
    if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        return Err(tonic::Status::unauthenticated("invalid bearer token"));
    }
    Ok(req)
}

/// Constant-time byte-slice equality. Returns false immediately on length
/// mismatch (the length itself is not a secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn req_with_auth(value: &str) -> tonic::Request<()> {
        let mut r = tonic::Request::new(());
        r.metadata_mut().insert("authorization", value.parse().unwrap());
        r
    }

    #[test]
    fn accepts_any_request_when_no_token_configured() {
        let r = tonic::Request::new(());
        assert!(authenticate_request(r, None).is_ok());
    }

    #[test]
    fn rejects_missing_header_when_token_required() {
        let r = tonic::Request::new(());
        let err = authenticate_request(r, Some("secret")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_wrong_scheme() {
        let r = req_with_auth("Basic abc");
        let err = authenticate_request(r, Some("secret")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_wrong_token() {
        let r = req_with_auth("Bearer wrong");
        let err = authenticate_request(r, Some("secret")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn accepts_correct_bearer() {
        let r = req_with_auth("Bearer s3cr3t");
        assert!(authenticate_request(r, Some("s3cr3t")).is_ok());
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) { s.recv().await; }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received");
}
