//! orbit-openeo — HTTP server binary.
//!
//! **Reference openEO 1.3.0 backend, intentionally not certified.**
//! Operational scope is bounded by [`BACKEND-SCOPE.md`](../../BACKEND-SCOPE.md)
//! (MAY / WILL NOT contract). Any change adding routes, process nodes, or
//! auth paths must satisfy §4 of that document or re-open Approach D in
//! `13-geo-satellite/04-openeo-strategic-analysis.md` §4.5.3 first.
//!
//! Loads the spec from `spec/openapi.json`, builds the app state, mounts
//! the router, and binds.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use orbit_openeo::{
    auth::AuthPolicy, build_router,
    catalog::{CollectionCatalog, HttpStacCatalog},
    executor::{LocalExecutor, ProcessGraphExecutor},
    file_store::{FileStore, ObjectStoreBackend},
    job_store::{JobStore, SqliteJobStore},
    schema::SchemaRegistry,
    security::RouteSecurityMap,
    AppStateBuilder,
};
#[cfg(feature = "geo-kernel")]
use orbit_openeo::geo_executor::{GeoExecutor, HttpStacSearcher, StacSearcher};
use tracing_subscriber::{prelude::*, EnvFilter};

const OPENAPI_JSON: &str = include_str!("../spec/openapi.json");

#[derive(Parser, Debug)]
#[command(name = "orbit-openeo", version, about = "orbit-rs openEO API façade")]
struct Args {
    /// HTTP bind address.
    #[arg(long, env = "ORBIT_OPENEO_BIND", default_value = "127.0.0.1:9080")]
    bind: SocketAddr,

    /// Optional bearer token. If set, all requests must carry
    /// `Authorization: Bearer <token>`. If unset and bind is non-loopback,
    /// the server refuses to start.
    #[arg(long, env = "ORBIT_OPENEO_AUTH_TOKEN")]
    auth_token: Option<String>,

    /// Backend id reported in the capabilities document.
    #[arg(long, env = "ORBIT_OPENEO_BACKEND_ID", default_value = "orbit-rs")]
    backend_id: String,

    /// Directory backing `/files` uploads/downloads via `object_store`.
    /// Defaults to in-memory storage (suitable for tests only).
    #[arg(long, env = "ORBIT_OPENEO_FILES_DIR")]
    files_dir: Option<std::path::PathBuf>,

    /// Remote STAC API base URL backing `/collections`. Defaults to the
    /// Element84 Earth Search v1 endpoint. Set to empty string to disable
    /// (returns an empty in-memory catalog).
    #[arg(
        long,
        env = "ORBIT_OPENEO_STAC_URL",
        default_value = "https://earth-search.aws.element84.com/v1"
    )]
    stac_url: String,

    /// Process-graph executor backend.
    ///   `geo`    — **default**. Real STAC search + cropped COG downloads + block-parallel
    ///              raster compute (NDVI, mask, reduce_dimension). Requires the
    ///              `geo-kernel` cargo feature (on by default) + GDAL on PATH.
    ///   `local`  — JSON-only outputs, suitable for CI and arithmetic-only graphs.
    ///              Build with `cargo run -p orbit-openeo --no-default-features --
    ///              --executor local` to skip the GDAL dependency entirely.
    #[arg(
        long,
        env = "ORBIT_OPENEO_EXECUTOR",
        default_value = "geo",
        value_parser = ["local", "geo"]
    )]
    executor: String,

    /// Persistent job store URL. Empty (default) uses the in-memory store,
    /// which loses jobs on restart. Example: `sqlite://./jobs.db?mode=rwc`.
    #[arg(long, env = "ORBIT_OPENEO_DB_URL", default_value = "")]
    db_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    if !args.bind.ip().is_loopback() && args.auth_token.is_none() {
        anyhow::bail!(
            "non-loopback bind {bind} requires ORBIT_OPENEO_AUTH_TOKEN. \
             Refusing to expose an unauthenticated openEO server.",
            bind = args.bind
        );
    }

    let schemas = Arc::new(SchemaRegistry::from_openapi_json(OPENAPI_JSON)?);
    tracing::info!(
        schema_count = schemas.len(),
        "loaded openEO schemas from spec/openapi.json"
    );

    let security = Arc::new(RouteSecurityMap::from_openapi_json(OPENAPI_JSON)?);
    tracing::info!(
        route_count = security.len(),
        "loaded openEO per-route security from spec/openapi.json"
    );

    let auth = match args.auth_token {
        Some(t) => AuthPolicy::Bearer { token: t },
        None => AuthPolicy::Open,
    };

    let mut builder = AppStateBuilder::new()
        .with_schemas(schemas)
        .with_security(security)
        .with_auth(auth)
        .with_backend_id(args.backend_id);

    if let Some(dir) = args.files_dir.as_ref() {
        std::fs::create_dir_all(dir).ok();
        let backend = ObjectStoreBackend::local_disk(dir)
            .map_err(|e| anyhow::anyhow!("failed to open file store at {}: {e}", dir.display()))?;
        tracing::info!(dir = %dir.display(), "using disk-backed file store");
        builder = builder.with_files(Arc::new(backend) as Arc<dyn FileStore>);
    } else {
        tracing::warn!("no --files-dir set; /files uses in-memory storage (lost on restart)");
    }

    // Resolve the catalog now so we can share it with whichever executor
    // we pick below. The builder also holds a clone for the routes.
    let catalog: Arc<dyn CollectionCatalog> = if !args.stac_url.is_empty() {
        tracing::info!(url = %args.stac_url, "wired remote STAC catalog");
        Arc::new(HttpStacCatalog::new(&args.stac_url))
    } else {
        tracing::warn!("--stac-url empty; /collections returns an empty list");
        Arc::new(orbit_openeo::catalog::InMemoryCatalog::empty())
    };
    builder = builder.with_catalog(catalog.clone());

    let executor: Arc<dyn ProcessGraphExecutor> = match args.executor.as_str() {
        #[cfg(feature = "geo-kernel")]
        "geo" => {
            tracing::info!(
                stac_url = %args.stac_url,
                "executor backend: geo — STAC search + cropped COG download + block-parallel NDVI"
            );
            let mut geo = GeoExecutor::with_catalog(catalog.clone());
            if !args.stac_url.is_empty() {
                let searcher: Arc<dyn StacSearcher> =
                    Arc::new(HttpStacSearcher::new(&args.stac_url));
                geo = geo.with_searcher(searcher);
            }
            // Downloader selection. P1 (in-process gdal::Dataset) is the
            // default — measured ~2.3× faster than subprocess gdal_translate
            // on 12 MP Wien S2 (85 s vs 194 s) by skipping per-call
            // fork-exec + GDAL re-init.
            //
            // Set ORBIT_SUBPROCESS_DOWNLOADER=1 to fall back to the legacy
            // subprocess path (for diagnostic A/B comparisons).
            //
            // Set ORBIT_ASYNC_TIFF_DOWNLOADER=1 to opt into the pure-Rust
            // async-tiff path (P2). Currently slower on cross-CRS S2 jobs
            // (falls back to libgdal for reprojection); kept opt-in until
            // tuned.
            #[cfg(feature = "async-tiff-downloader")]
            if std::env::var("ORBIT_ASYNC_TIFF_DOWNLOADER").as_deref() == Ok("1") {
                tracing::info!("downloader: async-tiff + object_store (P2, opt-in)");
                geo = geo.with_async_tiff_downloader();
            } else if std::env::var("ORBIT_SUBPROCESS_DOWNLOADER").as_deref() == Ok("1") {
                tracing::info!("downloader: subprocess gdal_translate (legacy, opt-in)");
                // No-op — GdalTranslateDownloader is the constructor default.
            } else {
                tracing::info!("downloader: in-process gdal::Dataset (P1, default)");
                geo = geo.with_inprocess_downloader();
            }
            #[cfg(not(feature = "async-tiff-downloader"))]
            if std::env::var("ORBIT_SUBPROCESS_DOWNLOADER").as_deref() == Ok("1") {
                tracing::info!("downloader: subprocess gdal_translate (legacy, opt-in)");
            } else {
                tracing::info!("downloader: in-process gdal::Dataset (P1, default)");
                geo = geo.with_inprocess_downloader();
            }
            Arc::new(geo)
        }
        #[cfg(not(feature = "geo-kernel"))]
        "geo" => {
            anyhow::bail!(
                "--executor geo requires the `geo-kernel` cargo feature; \
                 rebuild with `cargo build --features geo-kernel`"
            );
        }
        _ => {
            tracing::info!("executor backend: local (JSON outputs)");
            Arc::new(LocalExecutor::with_catalog(catalog.clone()))
        }
    };
    builder = builder.with_executor(executor);

    if !args.db_url.is_empty() {
        let store = SqliteJobStore::open(&args.db_url)
            .await
            .map_err(|e| anyhow::anyhow!("opening {}: {e}", args.db_url))?;
        tracing::info!(url = %args.db_url, "wired SqliteJobStore (jobs survive restarts)");
        builder = builder.with_jobs(Arc::new(store) as Arc<dyn JobStore>);
    } else {
        tracing::warn!("--db-url empty; using in-memory job store (lost on restart)");
    }

    let state = builder.build();

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %args.bind, "orbit-openeo listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
    tracing::info!("orbit-openeo shutdown signal received");
}
