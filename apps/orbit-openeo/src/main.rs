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

    // P3: tune libgdal/libcurl for /vsicurl/ range-read streaming. These
    // are no-ops when ORBIT_VSICURL_STREAM is unset (the legacy download
    // path doesn't issue range requests from worker threads), but they
    // matter the moment that flag flips on. Set early — before any
    // gdal::Dataset::open call in the process — so the first remote
    // probe in load_collection sees them.
    #[cfg(feature = "geo-kernel")]
    configure_gdal_for_vsicurl();

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
            // Downloader selection (2026-05-24, post-Task #34):
            //
            // **Default when `async-tiff-downloader` feature is built**: P2
            // (async-tiff + object_store) with Opt 1 (proj cross-CRS) +
            // Opt 2 (shared Arc<AmazonS3> pool) + STAC band_metadata hint
            // (Task #34). Median wall on 12 MP Wien S2 = 76 s (vs P1
            // 78 s); best case 53 s (~25 % upside). KNOWN TAIL RISK: 1/4
            // sample runs hit S3 body-error retry storms exceeding 300 s
            // — flagged in docs/perf/P2_P3_OPTIMIZATION_PROGRESS.md.
            //
            // Set ORBIT_INPROCESS_DOWNLOADER=1 to fall back to P1
            // (in-process libgdal eager download) — preferred when S3
            // transport instability is suspected.
            //
            // Set ORBIT_SUBPROCESS_DOWNLOADER=1 for the legacy
            // subprocess gdal_translate path (diagnostic A/B comparisons).
            //
            // Set ORBIT_VSICURL_STREAM=1 for P3: skip the eager download
            // phase entirely; eval_load.rs writes /vsicurl/<href> straight
            // into the cube and block_executor issues HTTP range requests
            // per-block on demand. No temp .tif files. See eval_load.rs.
            #[cfg(feature = "async-tiff-downloader")]
            {
                // Backward-compat: prior to the 2026-05-24 default flip,
                // `ORBIT_ASYNC_TIFF_DOWNLOADER=1` was the opt-in for P2. It
                // is now a no-op (P2 is the default), but accept + warn so
                // existing scripts and CI don't break.
                if std::env::var("ORBIT_ASYNC_TIFF_DOWNLOADER").as_deref() == Ok("1") {
                    tracing::warn!(
                        "ORBIT_ASYNC_TIFF_DOWNLOADER=1 is deprecated -- P2-full is now the \
                         default. Unset it and use ORBIT_INPROCESS_DOWNLOADER=1 to opt out to P1."
                    );
                }
                if std::env::var("ORBIT_INPROCESS_DOWNLOADER").as_deref() == Ok("1") {
                    tracing::info!("downloader: in-process gdal::Dataset (P1, opt-out from P2 default)");
                    geo = geo.with_inprocess_downloader();
                } else if std::env::var("ORBIT_SUBPROCESS_DOWNLOADER").as_deref() == Ok("1") {
                    tracing::info!("downloader: subprocess gdal_translate (legacy, opt-in)");
                    // No-op — GdalTranslateDownloader is the constructor default.
                } else {
                    tracing::info!(
                        "downloader: async-tiff + object_store + STAC hint (P2-full, default; \
                         set ORBIT_INPROCESS_DOWNLOADER=1 to opt-out to P1)"
                    );
                    geo = geo.with_async_tiff_downloader();
                }
            }
            #[cfg(not(feature = "async-tiff-downloader"))]
            if std::env::var("ORBIT_SUBPROCESS_DOWNLOADER").as_deref() == Ok("1") {
                tracing::info!("downloader: subprocess gdal_translate (legacy, opt-in)");
            } else {
                tracing::info!("downloader: in-process gdal::Dataset (P1, default — async-tiff feature not built)");
                geo = geo.with_inprocess_downloader();
            }
            // **Task #43 / semaphore sweep**: download concurrency is the
            // ceiling on simultaneous COG fetches. Default 8; lower values
            // reduce S3 connection-pool pressure (may cut body-error storms);
            // higher values only help when many COGs are in flight.
            if let Ok(v) = std::env::var("ORBIT_DOWNLOAD_CONCURRENCY") {
                if let Ok(n) = v.parse::<usize>() {
                    if n >= 1 {
                        tracing::info!(permits = n, "download concurrency override");
                        geo = geo.with_download_concurrency(n);
                    }
                }
            }
            if std::env::var("ORBIT_VSICURL_STREAM").as_deref() == Ok("1") {
                tracing::info!(
                    "P3: ORBIT_VSICURL_STREAM=1 — load_collection will skip downloads \
                     and emit /vsicurl/ paths into the cube (block-level range reads)"
                );
            }
            // Observability lever (Task #38 method, env-exposed): pin the
            // scratch dir to a user-owned path. Clears the auto-cleanup flag
            // so intermediate GeoTIFFs survive job completion — useful for
            // value-verifying a pipeline (gdalinfo on each stage).
            if let Ok(dir) = std::env::var("ORBIT_SCRATCH_DIR") {
                if !dir.is_empty() {
                    tracing::info!(scratch_dir = %dir, "scratch dir pinned (preserved on shutdown)");
                    geo = geo.with_scratch_dir(std::path::PathBuf::from(dir));
                }
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
        // **Orphan recovery (2026-05-25)**: a persistent store can hold
        // jobs left `running`/`queued` by a previous process that crashed
        // or was killed mid-execution. They can never complete, so flip
        // them to `error` at startup instead of leaving them "in progress"
        // forever. (No-op for the in-memory store, which starts empty.)
        match store.recover_orphans().await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(recovered = n, "recovered orphaned jobs (queued/running → error) from prior run"),
            Err(e) => tracing::error!(error = %e, "orphan recovery failed at startup"),
        }
        tracing::info!(url = %args.db_url, "wired SqliteJobStore (jobs survive restarts)");
        builder = builder.with_jobs(Arc::new(store) as Arc<dyn JobStore>);
    } else {
        tracing::warn!("--db-url empty; using in-memory job store (lost on restart)");
    }

    let state = builder.build();

    // audit-fix (2026-06-03): keep a handle to the in-flight job registry so we
    // can DRAIN spawned job tasks on shutdown. axum's graceful shutdown only
    // drains in-flight HTTP requests; batch jobs run in detached tasks and
    // would otherwise be dropped the instant the listener stops.
    let job_registry = state.job_registry.clone();

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %args.bind, "orbit-openeo listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;

    // HTTP server has stopped accepting; now wait for in-flight jobs to finish
    // (bounded). Override the budget via ORBIT_SHUTDOWN_DRAIN_SECS.
    let drain_secs: u64 = std::env::var("ORBIT_SHUTDOWN_DRAIN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    job_registry
        .drain(std::time::Duration::from_secs(drain_secs))
        .await;
    Ok(())
}

/// Tune libgdal/libcurl for `/vsicurl/` block-level range reads (P3).
///
/// All values are conservative defaults from the GDAL `/vsicurl/`
/// documentation. Each `set_config_option` call is best-effort — failures
/// are logged but non-fatal (GDAL falls back to its built-in defaults).
///
/// These affect *all* `Dataset::open("/vsicurl/...")` calls in the
/// process, regardless of whether ORBIT_VSICURL_STREAM is set; the
/// values were chosen to be safe no-ops for the existing
/// download-then-read path too.
#[cfg(feature = "geo-kernel")]
fn configure_gdal_for_vsicurl() {
    // 16 KB chunks balance round-trip count vs over-fetch. 512 MB
    // in-process cache absorbs hot blocks. HEAD-on-every-open is wasteful
    // — vsicurl can probe with the first GET instead.
    let opts: &[(&str, &str)] = &[
        ("VSI_CACHE", "TRUE"),
        ("VSI_CACHE_SIZE", "536870912"),
        ("CPL_VSIL_CURL_CHUNK_SIZE", "16384"),
        ("GDAL_HTTP_MAX_RETRY", "5"),
        ("GDAL_HTTP_RETRY_DELAY", "1"),
        ("CPL_VSIL_CURL_USE_HEAD", "NO"),
    ];
    for (k, v) in opts {
        if let Err(e) = gdal::config::set_config_option(k, v) {
            tracing::warn!(key = k, value = v, err = %e,
                "P3: failed to set GDAL config (non-fatal)");
        }
    }
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
