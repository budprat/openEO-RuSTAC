// ndarray's `s![]` macro expands to a `#[allow(unsafe_code)]` block; this
// module needs to accept that so the worker can slice into the 4-D
// RasterDataBlock without going through extra copies.
#![allow(unsafe_code)]

//! GeoExecutor — the block-parallel raster reduction pattern wired into the
//! openEO process-graph executor trait.
//!
//! Full pipeline:
//!
//! ```text
//! load_collection  →  STAC /search via reqwest
//!                  →  gdal_translate -srcwin crop download
//!                  →  orbit_geo::cache::FileCache
//!
//!     ndvi         →  RasterDatasetBuilder::from_files(cached_paths)
//!                  →  apply_reduction with block-parallel NDVI worker
//!                  →  GeoTIFF on disk
//!
//!     save_result  →  read GeoTIFF bytes, return as image/tiff
//! ```
//!
//! The cube state between nodes is a JSON envelope carrying either:
//! - `__cube`: list of cached scene paths + bbox + band assignments
//! - `__raster`: a single produced GeoTIFF path
//! - scalar / DataCube sentinel (for arithmetic graphs)
//!
//! Two traits are injected for testability:
//! - [`StacSearcher`] — abstracts the HTTP search call
//! - [`Downloader`]   — abstracts the COG-window download

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ndarray::{Array3, Axis};
use orbit_geo::providers::CropWindow;
use orbit_geo::types::Dimension;
use orbit_geo::RasterDataBlock;
use ndarray::s;
use serde_json::Value;

use crate::catalog::CollectionCatalog;
use crate::executor::{ExecError, ProcessGraphExecutor, SyncResult};

pub(crate) const SENTINEL_NDVI_NA: f32 = -9999.0;

/// Convert a slice of `PathBuf` into a `serde_json::Value::Array` of strings
/// without going through fallible `serde_json::to_value`. Used everywhere
/// we materialise cube `bands[<name>] = [<path>, ...]` so the deny-list on
/// `clippy::unwrap_used` stays clean without forcing each call site to
/// `?` an infallible serialisation.
#[inline]
pub(super) fn paths_to_value(paths: &[PathBuf]) -> Value {
    Value::Array(
        paths
            .iter()
            .map(|p| Value::String(p.to_string_lossy().into_owned()))
            .collect(),
    )
}

mod download;
mod eval_apply;
mod eval_load;
mod eval_mask;
mod eval_mask_from_values;
mod eval_misc;
mod eval_ndvi;
mod eval_reduce;
mod identifier;
mod registry;
mod stac;
mod sub_graph;

// Re-export the public API so external paths like `crate::geo_executor::GeoExecutor`
// keep working after the split.
pub use download::{AssetSigner, BearerSigner, Downloader, GdalTranslateDownloader,
    InProcessGdalDownloader, NoopAssetSigner, PlanetaryComputerSigner};

#[cfg(feature = "async-tiff-downloader")]
pub use download::AsyncTiffDownloader;
pub use eval_load::parse_eo_cloud_cover_lt;
pub use eval_reduce::{apply_reducer, parse_reducer_subgraph, Reducer};
pub use registry::{register_defaults, ProcessHandler, ProcessRegistry};
pub use stac::{HttpStacSearcher, StacScene, StacSearcher};

// ---------------------------------------------------------------------
// GeoExecutor
// ---------------------------------------------------------------------

/// Executor that walks an openEO process graph and produces real raster
/// bytes through the block-parallel raster reduction pattern.
pub struct GeoExecutor {
    pub(super) catalog: Option<Arc<dyn CollectionCatalog>>,
    pub(super) searcher: Option<Arc<dyn StacSearcher>>,
    pub(super) downloader: Arc<dyn Downloader>,
    /// Signs asset URLs before they reach the downloader. Defaults to a
    /// no-op signer so public-bucket backends like Element84 just work.
    pub(super) signer: Arc<dyn AssetSigner>,
    pub(super) scratch_dir: PathBuf,
    /// Content-addressed cache between downloader and dataset builder.
    /// When set, repeat-queries on identical asset URLs hit the cache and
    /// skip the network entirely — the explicit block-parallel speedup vehicle.
    pub(super) cache: Option<Arc<orbit_geo::cache::FileCache>>,
    /// Pixel side of the cropped window pulled from each remote COG.
    pub(super) crop_size: u32,
    /// Pixel offset of the crop window (top-left).
    pub(super) crop_offset: u32,
    /// **P0-5 / P1-9**: bounded concurrency on `gdal_translate` spawns.
    /// Default 8; configurable via `with_download_concurrency`.
    pub(super) download_sem: std::sync::Arc<tokio::sync::Semaphore>,
    /// **P1-7**: SSRF policy applied to STAC + asset URLs before any
    /// network call. Default denies http, RFC1918, IMDS, loopback,
    /// link-local. Use `with_url_policy(UrlPolicy::relaxed_dev())` for
    /// loopback testing.
    pub(super) url_policy: crate::url_policy::UrlPolicy,
    /// **A2**: process-name → handler table, populated once in
    /// [`GeoExecutor::new`] via [`register_defaults`]. Replaces the
    /// monolithic match-arm dispatcher inside [`evaluate`].
    pub(super) registry: ProcessRegistry,
}

impl GeoExecutor {
    /// New executor with no catalog / no searcher / a tempdir scratch.
    /// Falls back to the legacy 1×1 stamp behaviour for `save_result`.
    #[must_use]
    pub fn new() -> Self {
        let scratch = std::env::temp_dir().join(format!(
            "orbit-geoexec-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&scratch);
        let mut registry = ProcessRegistry::new();
        register_defaults(&mut registry);
        Self {
            catalog: None,
            searcher: None,
            downloader: Arc::new(GdalTranslateDownloader),
            signer: Arc::new(NoopAssetSigner),
            scratch_dir: scratch,
            cache: None,
            crop_size: 256,
            crop_offset: 0,
            download_sem: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
            url_policy: crate::url_policy::UrlPolicy::default(),
            registry,
        }
    }

    /// Inject a content-addressed file cache. With the cache wired,
    /// asset URLs that have been downloaded once on this host don't go
    /// over the network again — a key step in the block-parallel raster reduction pattern.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<orbit_geo::cache::FileCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Inject an asset-URL signer for STAC backends that require auth
    /// (Planetary Computer SAS, CDSE/EODC OIDC Bearer).
    #[must_use]
    pub fn with_signer(mut self, signer: Arc<dyn AssetSigner>) -> Self {
        self.signer = signer;
        self
    }

    /// New executor wired with a catalog (for `load_collection` validation).
    #[must_use]
    pub fn with_catalog(catalog: Arc<dyn CollectionCatalog>) -> Self {
        let mut s = Self::new();
        s.catalog = Some(catalog);
        s
    }

    /// Inject a STAC searcher (defaults to no-search → legacy behaviour).
    #[must_use]
    pub fn with_searcher(mut self, searcher: Arc<dyn StacSearcher>) -> Self {
        self.searcher = Some(searcher);
        self
    }

    /// Inject a custom downloader (defaults to `GdalTranslateDownloader`).
    #[must_use]
    pub fn with_downloader(mut self, downloader: Arc<dyn Downloader>) -> Self {
        self.downloader = downloader;
        self
    }

    /// Switch the executor to the no-subprocess in-process downloader.
    /// Opt-in -- the default remains `GdalTranslateDownloader` per the
    /// Phase A microbench finding that subprocess fork-exec overhead is
    /// dwarfed by S3 IO + libtiff decode (see `docs/perf/IN_PROCESS_DOWNLOAD_DESIGN.md`).
    #[must_use]
    pub fn with_inprocess_downloader(mut self) -> Self {
        self.downloader = Arc::new(download::InProcessGdalDownloader);
        self
    }

    /// **P2** -- switch to the pure-Rust async `Downloader` backed by
    /// `async-tiff` + `object_store::aws::AmazonS3`. Reads remote COGs
    /// over HTTP/2 with pipelined range requests instead of through
    /// libgdal `/vsicurl/`. Opt-in; falls back to the in-process libgdal
    /// path for cross-CRS crops, non-tiled sources, or unsupported URL
    /// schemes (see `crate::async_download::download_via_async_tiff_with_crs`).
    #[cfg(feature = "async-tiff-downloader")]
    #[must_use]
    pub fn with_async_tiff_downloader(mut self) -> Self {
        self.downloader = Arc::new(download::AsyncTiffDownloader);
        self
    }

    /// Override the scratch directory (cache root + intermediate GeoTIFFs).
    #[must_use]
    pub fn with_scratch_dir(mut self, dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        self.scratch_dir = dir;
        self
    }

    /// Override the crop window pulled from each remote COG.
    #[must_use]
    pub fn with_crop(mut self, offset: u32, size: u32) -> Self {
        self.crop_offset = offset;
        self.crop_size = size;
        self
    }

    fn parse_graph(&self, body: &Value) -> Result<eo_process::ProcessGraph, ExecError> {
        let pg_val = body
            .get("process")
            .and_then(|p| p.get("process_graph"))
            .or_else(|| body.get("process_graph"))
            .ok_or_else(|| ExecError::InvalidGraph("missing process.process_graph".into()))?;
        let nodes: std::collections::BTreeMap<String, eo_process::Process> =
            serde_json::from_value(pg_val.clone())
                .map_err(|e| ExecError::InvalidGraph(e.to_string()))?;
        if nodes.is_empty() {
            return Err(ExecError::InvalidGraph("process_graph is empty".into()));
        }
        Ok(eo_process::ProcessGraph { nodes })
    }

    async fn evaluate(&self, graph: &eo_process::ProcessGraph) -> Result<(String, Value), ExecError> {
        let analysis = crate::process_graph::ProcessGraphAnalysis::build(graph)
            .map_err(|e| ExecError::InvalidGraph(e.to_string()))?;
        let order = analysis.evaluation_order();
        let mut memo: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        for node_id in &order {
            let node = graph
                .nodes
                .get(node_id)
                .ok_or_else(|| ExecError::InvalidGraph(format!("unknown node `{node_id}`")))?;
            let mut resolved_args = std::collections::BTreeMap::new();
            for (k, v) in &node.arguments {
                let r = resolve_value(v, &memo)?;
                resolved_args.insert(k.clone(), r);
            }
            // **A2**: dispatch through ProcessRegistry. Behaviour is
            // identical to the old match arm (P0-4 apply guard preserved
            // inside `ApplyHandler`, save_result envelope inside
            // `SaveResultHandler`).
            let process_name = node.process_id.0.as_str();
            let handler = self
                .registry
                .get(process_name)
                .ok_or_else(|| ExecError::UnknownProcess(process_name.into()))?;
            let out = handler.handle(self, resolved_args).await?;
            memo.insert(node_id.clone(), out);
        }
        let result = memo
            .remove(&analysis.result_id)
            .ok_or_else(|| ExecError::InvalidGraph("result node was not visited during topo walk".into()))?;
        let result_proc = graph
            .nodes
            .get(&analysis.result_id)
            .map(|p| p.process_id.0.as_str())
            .unwrap_or("");
        Ok((result_proc.to_string(), result))
    }

    /// Download a remote asset, consulting [`FileCache`] first. On a
    /// cache hit, the network is skipped and the existing local path is
    /// returned. On a miss, the URL is signed via the configured
    /// [`AssetSigner`] (NoopAssetSigner by default), the downloader runs,
    /// then the freshly-written file is inserted into the cache.
    ///
    /// The cache key is the **unsigned** href so a token rotation does
    /// not invalidate cached crops.
    #[allow(dead_code)]
    fn fetch_with_cache(
        &self,
        href: &str,
        dst: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
    ) -> Result<PathBuf, ExecError> {
        if let Some(cache) = &self.cache {
            if let Some(cached) = cache.get(href) {
                tracing::debug!(href = %href, cached = %cached.display(), "FileCache hit");
                return Ok(cached);
            }
        }
        let signed = self.signer.sign(href)?;
        self.downloader.download(&signed, dst, crop, crop_crs)?;
        if let Some(cache) = &self.cache {
            return cache
                .insert(href, dst)
                .map_err(|e| ExecError::Backend(format!("FileCache insert {href}: {e}")));
        }
        Ok(dst.to_path_buf())
    }

    /// **P0-5 / P1-9**: async wrapper around [`Self::fetch_with_cache`]
    /// that routes the blocking `gdal_translate` subprocess through
    /// `tokio::task::spawn_blocking` and respects the per-executor
    /// download semaphore. Limits parallelism to `download_sem`'s
    /// permits (default 8) so a many-scene job can't fork-bomb the
    /// host.
    pub(super) async fn fetch_with_cache_async(
        &self,
        href: String,
        dst: PathBuf,
        crop: CropWindow,
        crop_crs: Option<String>,
    ) -> Result<PathBuf, ExecError> {
        // **P1-7**: SSRF check before any DNS / network work.
        self.url_policy
            .check(&href)
            .map_err(|e| ExecError::Backend(format!("url_policy: {e}")))?;
        // Cache check on the async side — no need to grab a permit if
        // we'll hit the cache.
        if let Some(cache) = &self.cache {
            if let Some(cached) = cache.get(&href) {
                return Ok(cached);
            }
        }
        let permit = self
            .download_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| ExecError::Backend(format!("semaphore: {e}")))?;
        let signer = self.signer.clone();
        let downloader = self.downloader.clone();
        let cache = self.cache.clone();
        let h_for_blocking = href.clone();
        let dst_for_blocking = dst.clone();
        let blocking_result = tokio::task::spawn_blocking(move || -> Result<PathBuf, ExecError> {
            let _permit = permit; // released when this closure returns
            let signed = signer.sign(&h_for_blocking)?;
            downloader.download(
                &signed,
                &dst_for_blocking,
                crop,
                crop_crs.as_deref(),
            )?;
            if let Some(cache) = cache {
                return cache
                    .insert(&h_for_blocking, &dst_for_blocking)
                    .map_err(|e| {
                        ExecError::Backend(format!("FileCache insert {h_for_blocking}: {e}"))
                    });
            }
            Ok(dst_for_blocking)
        })
        .await
        .map_err(|e| ExecError::Backend(format!("spawn_blocking join: {e}")))?;
        blocking_result
    }

    /// Configure the maximum number of concurrent download (gdal_translate)
    /// subprocesses. Default 8.
    #[must_use]
    pub fn with_download_concurrency(mut self, permits: usize) -> Self {
        self.download_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(permits.max(1)));
        self
    }

    /// **P1-7**: install a non-default URL policy. Use
    /// `UrlPolicy::relaxed_dev()` to allow loopback / private STAC for
    /// local development.
    #[must_use]
    pub fn with_url_policy(mut self, policy: crate::url_policy::UrlPolicy) -> Self {
        self.url_policy = policy;
        self
    }
}

impl Default for GeoExecutor {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl ProcessGraphExecutor for GeoExecutor {
    async fn run_sync(&self, body: &Value) -> Result<SyncResult, ExecError> {
        let graph = self.parse_graph(body)?;
        let (result_proc, value) = self.evaluate(&graph).await?;
        if result_proc == "save_result" {
            return finalise_save_result(value).await;
        }
        Ok(SyncResult::json(&value))
    }

    async fn enqueue(&self, body: &Value) -> Result<String, ExecError> {
        let _ = self.parse_graph(body)?;
        Ok(format!("job-{:08x}", deterministic_hash(body)))
    }
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

/// Pass an upstream cube/value through, recording the filter that was
/// applied. The returned value is `data` (or, if absent, the first arg
/// we can find) with a `_orbit_meta` annotation listing every filter
/// that has been applied so far — useful for debugging graphs without
/// dropping the cube state.
pub(super) fn filter_passthrough(
    args: &std::collections::BTreeMap<String, Value>,
    arg_names: &[&str],
    tag: &str,
) -> Result<Value, ExecError> {
    let data = args
        .get("data")
        .cloned()
        .ok_or_else(|| ExecError::InvalidGraph(format!("{tag}: missing `data`")))?;
    // If data is an object, attach the tag list under `_orbit_meta`.
    let mut out = data.clone();
    if let Value::Object(m) = &mut out {
        let meta = m
            .entry("_orbit_meta".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(meta_obj) = meta {
            let applied = meta_obj
                .entry("applied".to_string())
                .or_insert_with(|| Value::Array(vec![]));
            if let Value::Array(arr) = applied {
                let mut entry = serde_json::Map::new();
                entry.insert("step".into(), Value::String(tag.into()));
                for arg in arg_names {
                    if let Some(v) = args.get(*arg) {
                        entry.insert((*arg).into(), v.clone());
                    }
                }
                arr.push(Value::Object(entry));
            }
        }
    }
    Ok(out)
}

/// Cheap unit-interval RNG for retry jitter. We don't take a `rand`
/// dep — the resilience crate's `jittered_delay` expects a closure.
/// Uses nanosecond clock as entropy + xorshift.
pub(super) fn fastrand_unit_f64() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let mut x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    // Map u64 → [0, 1).
    (x as f64) / (u64::MAX as f64)
}

/// Decode a JSON 2-D number array into `ndarray::Array2<f64>`.
pub(super) fn json_to_array2(v: Option<&Value>, name: &str) -> Result<ndarray::Array2<f64>, ExecError> {
    let arr = v
        .and_then(|x| x.as_array())
        .ok_or_else(|| ExecError::InvalidGraph(format!("{name} must be a 2-D number array")))?;
    if arr.is_empty() {
        return Err(ExecError::InvalidGraph(format!("{name}: empty array")));
    }
    let rows = arr.len();
    let cols = arr[0]
        .as_array()
        .ok_or_else(|| ExecError::InvalidGraph(format!("{name} row 0 not an array")))?
        .len();
    let mut flat = Vec::with_capacity(rows * cols);
    for (r_idx, row) in arr.iter().enumerate() {
        let row = row.as_array().ok_or_else(|| {
            ExecError::InvalidGraph(format!("{name} row {r_idx} not an array"))
        })?;
        if row.len() != cols {
            return Err(ExecError::InvalidGraph(format!(
                "{name} row {r_idx} length {} != expected {cols}",
                row.len()
            )));
        }
        for c in row {
            flat.push(c.as_f64().ok_or_else(|| {
                ExecError::InvalidGraph(format!("{name}: non-numeric element"))
            })?);
        }
    }
    ndarray::Array2::from_shape_vec((rows, cols), flat)
        .map_err(|e| ExecError::InvalidGraph(format!("{name}: {e}")))
}

/// Decode a JSON 1-D array of small non-negative ints into `Array1<u8>`.
pub(super) fn json_to_array1_u8(v: Option<&Value>, name: &str) -> Result<ndarray::Array1<u8>, ExecError> {
    let arr = v
        .and_then(|x| x.as_array())
        .ok_or_else(|| ExecError::InvalidGraph(format!("{name} must be an array")))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let n = v.as_u64().ok_or_else(|| {
            ExecError::InvalidGraph(format!("{name}: non-integer element"))
        })?;
        if n > 255 {
            return Err(ExecError::InvalidGraph(format!(
                "{name}: value {n} out of u8 range"
            )));
        }
        out.push(n as u8);
    }
    Ok(ndarray::Array1::from(out))
}

/// Pull a `PathBuf` out of a `__raster` handle (or a bare string path
/// for convenience). Used by orbit-extension processes that consume
/// produced rasters.
pub(super) fn extract_raster_path(v: Option<&Value>, name: &str) -> Result<PathBuf, ExecError> {
    let v = v.ok_or_else(|| ExecError::InvalidGraph(format!("missing `{name}`")))?;
    if let Some(raster) = v.get("__raster") {
        return serde_json::from_value(raster["path"].clone())
            .map_err(|e| ExecError::InvalidGraph(format!("{name}: bad __raster.path: {e}")));
    }
    if let Some(s) = v.as_str() {
        return Ok(PathBuf::from(s));
    }
    Err(ExecError::InvalidGraph(format!(
        "{name} must be a __raster handle or a string path"
    )))
}

fn deterministic_hash(v: &Value) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let s = v.to_string();
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn resolve_value(v: &Value, memo: &std::collections::HashMap<String, Value>) -> Result<Value, ExecError> {
    if let Some(obj) = v.as_object() {
        if obj.len() == 1 {
            if let Some(Value::String(target)) = obj.get("from_node") {
                return memo
                    .get(target)
                    .cloned()
                    .ok_or_else(|| ExecError::InvalidGraph(format!(
                        "upstream node `{target}` not yet evaluated"
                    )));
            }
        }
    }
    Ok(v.clone())
}

/// Canonical openEO output format (`format` arg of `save_result`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    /// GeoTIFF (default for raster).
    GTiff,
    /// Cloud-Optimised GeoTIFF — same content-type as GTiff today.
    Cog,
    /// JSON (numbers, cube metadata, anything serialisable).
    Json,
    /// PNG (Float32 raster path also accepts this; we emit an 8-bit gray
    /// PNG approximation via the `image` crate when bytes are available).
    Png,
    /// NetCDF (deferred — requires GDAL netCDF driver; we map to TIFF
    /// for now to avoid lying about the bytes).
    NetCdf,
}

impl OutputFormat {
    /// Parse the openEO `format` string (case-insensitive). Falls back to
    /// `GTiff` (raster default) or `Json` (numeric default).
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "GTIFF" | "TIFF" | "GEOTIFF" => Self::GTiff,
            "COG" | "CLOUD_OPTIMIZED_GEOTIFF" => Self::Cog,
            "JSON" => Self::Json,
            "PNG" => Self::Png,
            "NETCDF" | "NC" => Self::NetCdf,
            _ => Self::GTiff,
        }
    }

    /// IANA media-type for the produced bytes.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::GTiff | Self::Cog | Self::NetCdf => "image/tiff",
            Self::Json => "application/json",
            Self::Png => "image/png",
        }
    }
}

/// Convert the value reaching the `save_result` node into a concrete
/// [`SyncResult`] using the optional `format` hint.
async fn finalise_save_result(value: Value) -> Result<SyncResult, ExecError> {
    let (data, format) = if let Some(env) = value.get("__save_result") {
        let fmt = env.get("format").and_then(|v| v.as_str()).unwrap_or("GTiff");
        (env.get("data").cloned().unwrap_or(Value::Null), OutputFormat::parse(fmt))
    } else {
        // No explicit format — pick raster default if input is a raster
        // handle, JSON otherwise.
        let inferred = if value.get("__raster").is_some() {
            OutputFormat::GTiff
        } else if value.is_number() {
            OutputFormat::GTiff
        } else {
            OutputFormat::Json
        };
        (value, inferred)
    };

    match format {
        OutputFormat::Json => Ok(SyncResult::json(&data)),
        OutputFormat::GTiff | OutputFormat::Cog | OutputFormat::NetCdf => {
            if let Some(raster) = data.get("__raster") {
                let path: PathBuf = serde_json::from_value(raster["path"].clone())
                    .map_err(|e| ExecError::Backend(format!("save_result: bad path: {e}")))?;
                let bytes = std::fs::read(&path).map_err(|e| {
                    ExecError::Backend(format!("save_result: read {}: {e}", path.display()))
                })?;
                return Ok(SyncResult { content_type: format.media_type().to_string(), body: bytes });
            }
            let pixel = data.as_f64().unwrap_or(0.0);
            let bytes = write_float32_tiff_1x1(pixel)?;
            Ok(SyncResult { content_type: format.media_type().to_string(), body: bytes })
        }
        OutputFormat::Png => {
            if let Some(raster) = data.get("__raster") {
                let path: PathBuf = serde_json::from_value(raster["path"].clone())
                    .map_err(|e| ExecError::Backend(format!("save_result: bad path: {e}")))?;
                // GDAL is blocking — route through spawn_blocking per
                // CLAUDE.md §4 P0-5. Reads the TIFF, dynamic-range stretches
                // to 8-bit grayscale, encodes via GDAL's PNG driver.
                let bytes = tokio::task::spawn_blocking(move || raster_to_png_bytes(&path))
                    .await
                    .map_err(|e| ExecError::Backend(format!("save_result PNG join: {e}")))??;
                return Ok(SyncResult { content_type: "image/png".into(), body: bytes });
            }
            // Scalar fallback — keep the 1×1 grey-pixel path for non-raster inputs.
            let pixel = data.as_f64().unwrap_or(0.0);
            let bytes = write_png_1x1(pixel)?;
            Ok(SyncResult { content_type: "image/png".into(), body: bytes })
        }
    }
}

/// Read a single-band GeoTIFF, dynamic-range stretch to 8-bit grayscale,
/// encode via GDAL's PNG driver, and return the encoded bytes. Honors the
/// `SENTINEL_NDVI_NA` (-9999) sentinel and i16::MIN: such pixels are
/// treated as no-data and rendered as 0 (and excluded from the stretch).
fn raster_to_png_bytes(tiff_path: &Path) -> Result<Vec<u8>, ExecError> {
    use gdal::raster::Buffer;
    use gdal::{Dataset, DriverManager};
    let src = Dataset::open(tiff_path)
        .map_err(|e| ExecError::Backend(format!("png: open {}: {e}", tiff_path.display())))?;
    let band = src
        .rasterband(1)
        .map_err(|e| ExecError::Backend(format!("png: band 1: {e}")))?;
    let (cols, rows) = band.size();
    // Read as f32 — covers NDVI (-1..1 with -9999 sentinel) and i16 sources.
    let buf: Buffer<f32> = band
        .read_as((0, 0), (cols, rows), (cols, rows), None)
        .map_err(|e| ExecError::Backend(format!("png: read: {e}")))?;
    let pixels: Vec<f32> = buf.data().to_vec();
    // No-data detection: SENTINEL_NDVI_NA (-9999), i16::MIN cast to f32,
    // and non-finite values are all rendered as 0.
    let is_nodata = |p: f32| {
        !p.is_finite()
            || (p - SENTINEL_NDVI_NA).abs() < 0.5
            || (p - (i16::MIN as f32)).abs() < 0.5
    };
    // Compute min/max ignoring no-data sentinels.
    let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
    for &p in &pixels {
        if is_nodata(p) {
            continue;
        }
        if p < mn {
            mn = p;
        }
        if p > mx {
            mx = p;
        }
    }
    if mn == f32::INFINITY {
        // All no-data raster — emit a valid PNG of pure-black at the
        // source dimensions (no panic, callers see something they can decode).
        mn = 0.0;
        mx = 1.0;
    }
    let span = (mx - mn).max(f32::EPSILON);
    let u8_pixels: Vec<u8> = pixels
        .iter()
        .map(|&p| {
            if is_nodata(p) {
                0u8
            } else {
                ((p - mn) / span * 255.0).round().clamp(0.0, 255.0) as u8
            }
        })
        .collect();
    // MEM driver → in-memory u8 dataset.
    let mem_drv = DriverManager::get_driver_by_name("MEM")
        .map_err(|e| ExecError::Backend(format!("png: MEM driver: {e}")))?;
    let mut mem_ds = mem_drv
        .create_with_band_type::<u8, _>("", cols, rows, 1)
        .map_err(|e| ExecError::Backend(format!("png: mem create: {e}")))?;
    {
        let mut b = mem_ds
            .rasterband(1)
            .map_err(|e| ExecError::Backend(format!("png: mem band: {e}")))?;
        let mut buffer = Buffer::new((cols, rows), u8_pixels);
        b.write::<u8>((0, 0), (cols, rows), &mut buffer)
            .map_err(|e| ExecError::Backend(format!("png: mem write: {e}")))?;
    }
    // CreateCopy via the PNG driver into a /vsimem/ file, then read it back.
    let png_drv = DriverManager::get_driver_by_name("PNG")
        .map_err(|e| ExecError::Backend(format!("png: PNG driver: {e}")))?;
    let vsi_path = format!(
        "/vsimem/orbit_save_result_{}_{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let copy = mem_ds
        .create_copy(&png_drv, &vsi_path, &Default::default())
        .map_err(|e| ExecError::Backend(format!("png: create_copy: {e}")))?;
    // Drop the copy handle to flush bytes into the /vsimem/ file.
    drop(copy);
    let bytes = gdal::vsi::get_vsi_mem_file_bytes_owned(&vsi_path)
        .map_err(|e| ExecError::Backend(format!("png: read vsimem: {e}")))?;
    // `get_vsi_mem_file_bytes_owned` unlinks the file (`unlink=true`).
    Ok(bytes)
}

/// Minimal PNG encoder for the 1×1 gray fallback. Hand-rolled to avoid
/// pulling the full `image` crate just for one pixel.
fn write_png_1x1(pixel: f64) -> Result<Vec<u8>, ExecError> {
    let v = pixel.clamp(0.0, 255.0) as u8;
    // PNG signature + IHDR + IDAT + IEND, computed by hand. Width=1,
    // height=1, bit_depth=8, color_type=0 (grayscale), interlace=0.
    let mut out: Vec<u8> = Vec::with_capacity(80);
    out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    // IHDR chunk (13 bytes data).
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1u32.to_be_bytes()); // width
    ihdr.extend_from_slice(&1u32.to_be_bytes()); // height
    ihdr.push(8);  // bit depth
    ihdr.push(0);  // color type = grayscale
    ihdr.push(0);  // compression
    ihdr.push(0);  // filter
    ihdr.push(0);  // interlace
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    // IDAT: deflate stream containing [filter=0, pixel].
    // We use the zero-compression "stored" deflate block format so we
    // don't need a deflate library: header + len/nlen + literal bytes.
    let raw = [0u8, v];
    let mut idat: Vec<u8> = Vec::with_capacity(16);
    idat.extend_from_slice(&[0x78, 0x01]); // zlib header (deflate, default)
    idat.push(0x01);                       // BFINAL=1, BTYPE=00 (stored)
    idat.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    idat.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    idat.extend_from_slice(&raw);
    // Adler-32 of the raw uncompressed data.
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    idat.extend_from_slice(&((b << 16) | a).to_be_bytes());
    write_png_chunk(&mut out, b"IDAT", &idat);
    write_png_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

fn write_png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_buf = Vec::with_capacity(tag.len() + data.len());
    crc_buf.extend_from_slice(tag);
    crc_buf.extend_from_slice(data);
    out.extend_from_slice(&png_crc32(&crc_buf).to_be_bytes());
}

fn png_crc32(buf: &[u8]) -> u32 {
    // Tiny PNG CRC-32 (poly 0xEDB88320). Standalone to avoid a crc32 dep.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in buf {
        let mut c = (crc ^ b as u32) & 0xFF;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        crc = (crc >> 8) ^ c;
    }
    crc ^ 0xFFFF_FFFF
}

fn write_float32_tiff_1x1(pixel: f64) -> Result<Vec<u8>, ExecError> {
    use std::io::Cursor;
    use tiff::encoder::{colortype::Gray32Float, TiffEncoder};
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut encoder = TiffEncoder::new(cursor)
            .map_err(|e| ExecError::Backend(format!("tiff: {e}")))?;
        encoder
            .write_image::<Gray32Float>(1, 1, &[pixel as f32])
            .map_err(|e| ExecError::Backend(format!("tiff write: {e}")))?;
    }
    Ok(buf)
}

// suppress unused-import warning when `Axis` is not used in non-default
// configurations
#[allow(dead_code)]
fn _axis_anchor() {
    let _ = Axis(0);
}

/// Sentinel-2 SCL → clear-sky bit. Uses `eo_mask::ScenSceneClass` typed
/// enum + the unified `MaskValue` taxonomy — clear-sky means the pixel
/// projects to `MaskValue::Clear` or `MaskValue::Water` (geophysics
/// processes often include water surfaces in the analysis domain).
fn scl_is_clear_sky(scl: u8) -> bool {
    use eo_mask::{MaskValue, ScenSceneClass};
    let mv = ScenSceneClass::from_u8(scl).to_mask_value();
    matches!(mv, MaskValue::Clear | MaskValue::Water | MaskValue::Snow)
}

/// Landsat Collection 2 QA_PIXEL → clear-sky bit. Uses `eo_mask::QaPixel`
/// typed bitfield with `MaskValue::Clear` semantics.
#[allow(dead_code)]
fn landsat_qa_is_clear_sky(qa: u16) -> bool {
    use eo_mask::{MaskValue, QaPixel};
    matches!(QaPixel::from_u16(qa).to_mask_value(), MaskValue::Clear)
}

/// Masked NDVI worker. Input layout matches `ndvi_worker` but the
/// `mask` block carries the SCL pixel values for the same (T, R, C)
/// grid. Pixels classified as cloud / shadow / cirrus are skipped.
#[allow(dead_code)]
fn ndvi_masked_worker(
    rdb: &RasterDataBlock<i16>,
    mask: &RasterDataBlock<u8>,
    _dim: Dimension,
) -> Array3<f32> {
    let t_dim = rdb.times();
    let r = rdb.rows();
    let c = rdb.cols();
    let mut out = Array3::<f32>::from_elem((1, r, c), 0.0_f32);
    let mut counts = Array3::<u32>::zeros((1, r, c));
    for t in 0..t_dim {
        let red = rdb.data.slice(s![t, 0, .., ..]);
        let nir = rdb.data.slice(s![t, 1, .., ..]);
        let scl = mask.data.slice(s![t, 0, .., ..]);
        for ((row, col), &rv) in red.indexed_iter() {
            let s_val = scl[[row, col]];
            if !scl_is_clear_sky(s_val) {
                continue;
            }
            let nv = nir[[row, col]];
            let r_f = rv as f32;
            let n_f = nv as f32;
            let denom = r_f + n_f;
            if denom.abs() > 1.0 && rv > 0 && nv > 0 {
                let ndvi = (n_f - r_f) / denom;
                out[[0, row, col]] += ndvi;
                counts[[0, row, col]] += 1;
            }
        }
    }
    for ((_, val), cnt) in out.indexed_iter_mut().zip(counts.iter()) {
        if *cnt > 0 {
            *val /= *cnt as f32;
        } else {
            *val = SENTINEL_NDVI_NA;
        }
    }
    out
}

// ---------------------------------------------------------------------
// tests — shared helpers + dispatcher-level tests
// ---------------------------------------------------------------------

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::catalog::{Collection, InMemoryCatalog};
    use serde_json::json;


    fn graph(args: serde_json::Value) -> serde_json::Value {
        json!({ "process": { "process_graph": args } })
    }

    // ---------- baseline (D2a-d) — kept green ----------

    #[tokio::test(flavor = "current_thread")]
    async fn save_result_returns_image_tiff_content_type() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 5.0, "y": 2.0 } },
            "s": { "process_id": "save_result", "arguments": { "data": { "from_node": "a" } }, "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "image/tiff");
        assert!(r.body.starts_with(b"II*\0") || r.body.starts_with(b"MM\0*"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tiff_payload_roundtrips_pixel_value() {
        use tiff::decoder::{Decoder, DecodingResult};
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 40.5, "y": 1.5 } },
            "s": { "process_id": "save_result", "arguments": { "data": { "from_node": "a" } }, "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let cursor = std::io::Cursor::new(&r.body);
        let mut dec = Decoder::new(cursor).expect("decode");
        let (w, h) = dec.dimensions().expect("dims");
        assert_eq!((w, h), (1, 1));
        match dec.read_image().expect("read") {
            DecodingResult::F32(v) => assert!((v[0] - 42.0).abs() < 1e-6, "got {}", v[0]),
            other => panic!("expected F32, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_save_result_node_emits_json() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 1, "y": 2 }, "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v.as_f64().unwrap(), 3.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cycle_detection_propagates() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": { "from_node": "b" }, "y": 1 } },
            "b": { "process_id": "add", "arguments": { "x": { "from_node": "a" }, "y": 1 }, "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_process_returns_error() {
        let body = graph(json!({
            "x": { "process_id": "definitely_not_a_real_process", "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::UnknownProcess(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_catalog_validates_load_collection() {
        let cat = Arc::new(InMemoryCatalog::with_collections(vec![Collection::new("s2")]))
            as Arc<dyn CollectionCatalog>;
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "s2" } },
            "s": { "process_id": "save_result", "arguments": { "data": 7.0 }, "result": true }
        }));
        let r = GeoExecutor::with_catalog(cat).run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "image/tiff");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_catalog_unknown_collection_fails() {
        let cat = Arc::new(InMemoryCatalog::empty()) as Arc<dyn CollectionCatalog>;
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "missing" } },
            "s": { "process_id": "save_result", "arguments": { "data": { "from_node": "l" } }, "result": true }
        }));
        let r = GeoExecutor::with_catalog(cat).run_sync(&body).await;
        match r {
            Err(ExecError::Backend(m)) => assert!(m.contains("CollectionNotFound"), "got {m}"),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_validates_graph() {
        let r = GeoExecutor::new().enqueue(&json!({})).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn write_float32_tiff_1x1_produces_minimum_viable_tiff() {
        let bytes = write_float32_tiff_1x1(123.5).expect("encode");
        assert!(bytes.len() > 32);
        assert!(bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"));
    }

    // ---------- D2c — load_collection wires search → download ----------

    // ---------- A3 — pure ndvi produces per-scene __cube.ndvi_paths ----------

    // ---------- D2e — full pipeline: load → ndvi → save_result ----------

    // ---------- P1a — FileCache integration ----------

    // ---------- P1b — cloud mask via apply_reduction_with_mask ----------

    #[test]
    fn scl_classifier_uses_eo_mask_typed_taxonomy() {
        // Per `eo_mask::ScenSceneClass::to_mask_value`:
        //   Clear: 2(DarkArea), 4(Vegetation), 5(BareSoil), 7(Unclassified)
        //   Water:  6  | Snow: 11 — both still surface us (analysis domain)
        //   Cloud:  8,9,10 | Shadow: 3 | Saturated: 1 | NoData: 0
        for c in [2, 4, 5, 6, 7, 11] {
            assert!(scl_is_clear_sky(c), "class {c} should be clear-sky");
        }
        for c in [0, 1, 3, 8, 9, 10] {
            assert!(!scl_is_clear_sky(c), "class {c} must be masked");
        }
    }

    #[test]
    fn landsat_qa_classifier_uses_eo_mask_qa_pixel() {
        // `eo_mask::QaPixel::to_mask_value` precedence:
        // fill → NoData; cloud OR dilated_cloud → Cloud;
        // cloud_shadow → Shadow; cirrus → Cloud; snow → Snow;
        // water → Water; clear bit only → Clear.
        // ONLY MaskValue::Clear here counts as clear-sky.
        assert!(landsat_qa_is_clear_sky(0b0100_0000));         // bit-6 clear, nothing else
        assert!(!landsat_qa_is_clear_sky(0b0100_0010));        // dilated_cloud now MASKED (was clear in old impl)
        assert!(!landsat_qa_is_clear_sky(0b0100_1000));        // cloud bit
        assert!(!landsat_qa_is_clear_sky(0b0101_0000));        // cloud-shadow bit
        // qa=0 — `eo_mask::QaPixel::to_mask_value` documents this as
        // benign-default Clear (no problematic bits set).
        assert!(landsat_qa_is_clear_sky(0));
    }

    /// Verifies the downloader receives the *signed* URL (not the raw one)
    /// while the cache is still keyed on the unsigned href.
    // ---------- P4c — multi-format save_result ----------

    #[tokio::test(flavor = "current_thread")]
    async fn save_result_json_format_emits_json() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 1.5, "y": 2.5 } },
            "s": { "process_id": "save_result",
                   "arguments": { "data": { "from_node": "a" }, "format": "JSON" },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v.as_f64().unwrap(), 4.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn save_result_gtiff_format_emits_tiff() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 7, "y": 35 } },
            "s": { "process_id": "save_result",
                   "arguments": { "data": { "from_node": "a" }, "format": "GTiff" },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "image/tiff");
        assert!(r.body.starts_with(b"II*\0") || r.body.starts_with(b"MM\0*"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn save_result_cog_alias_resolves_to_image_tiff() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 1, "y": 1 } },
            "s": { "process_id": "save_result",
                   "arguments": { "data": { "from_node": "a" }, "format": "COG" },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "image/tiff");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn save_result_png_emits_valid_png_bytes() {
        let body = graph(json!({
            "a": { "process_id": "add", "arguments": { "x": 100, "y": 28 } },
            "s": { "process_id": "save_result",
                   "arguments": { "data": { "from_node": "a" }, "format": "PNG" },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        assert_eq!(r.content_type, "image/png");
        // PNG signature.
        assert_eq!(&r.body[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn output_format_parses_known_aliases() {
        assert_eq!(OutputFormat::parse("GTiff"), OutputFormat::GTiff);
        assert_eq!(OutputFormat::parse("gtiff"), OutputFormat::GTiff);
        assert_eq!(OutputFormat::parse("TIFF"), OutputFormat::GTiff);
        assert_eq!(OutputFormat::parse("COG"), OutputFormat::Cog);
        assert_eq!(OutputFormat::parse("Json"), OutputFormat::Json);
        assert_eq!(OutputFormat::parse("png"), OutputFormat::Png);
        assert_eq!(OutputFormat::parse("NetCDF"), OutputFormat::NetCdf);
        assert_eq!(OutputFormat::parse("nc"), OutputFormat::NetCdf);
        // Unknown → default to GTiff (raster).
        assert_eq!(OutputFormat::parse("xyzzy"), OutputFormat::GTiff);
    }

    #[test]
    fn output_format_media_types() {
        assert_eq!(OutputFormat::GTiff.media_type(), "image/tiff");
        assert_eq!(OutputFormat::Cog.media_type(), "image/tiff");
        assert_eq!(OutputFormat::Json.media_type(), "application/json");
        assert_eq!(OutputFormat::Png.media_type(), "image/png");
        assert_eq!(OutputFormat::NetCdf.media_type(), "image/tiff");
    }

    #[test]
    fn png_crc32_known_vector() {
        // RFC 2083 sample (the IEND chunk type produces this CRC).
        assert_eq!(png_crc32(b"IEND"), 0xAE42_6082);
    }

    // ---------- P2a — additional openEO processes ----------

    #[tokio::test(flavor = "current_thread")]
    async fn multiply_and_divide_arithmetic() {
        let body = graph(json!({
            "m": { "process_id": "multiply", "arguments": { "x": 6, "y": 7 } },
            "d": { "process_id": "divide", "arguments": { "x": { "from_node": "m" }, "y": 2 }, "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v.as_f64().unwrap(), 21.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn divide_by_zero_is_invalid_graph() {
        let body = graph(json!({
            "d": { "process_id": "divide", "arguments": { "x": 1, "y": 0 }, "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filter_temporal_passes_data_and_records_metadata() {
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "f": { "process_id": "filter_temporal",
                   "arguments": { "data": { "from_node": "l" }, "extent": ["2024-01-01", "2024-06-01"] },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        // Original cube fields preserved.
        assert_eq!(v["type"], "DataCube");
        assert_eq!(v["collection"], "c");
        // Filter metadata captured.
        let applied = &v["_orbit_meta"]["applied"];
        assert_eq!(applied[0]["step"], "applied_filter_temporal");
        assert!(applied[0]["extent"].is_array());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filter_spatial_records_metadata() {
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "f": { "process_id": "filter_spatial",
                   "arguments": { "data": { "from_node": "l" }, "geometries": {"type":"Polygon"} },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let applied = &v["_orbit_meta"]["applied"];
        assert_eq!(applied[0]["step"], "applied_filter_spatial");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filter_bbox_records_extent() {
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "f": { "process_id": "filter_bbox",
                   "arguments": { "data": { "from_node": "l" },
                                  "extent": {"west": 0, "south": 0, "east": 1, "north": 1} },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["_orbit_meta"]["applied"][0]["step"], "applied_filter_bbox");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_with_empty_process_object_is_rejected() {
        // Post-P0-4-replacement: `apply` now evaluates real sub-graphs.
        // An empty `process: {}` (no `process_graph` key) must still
        // be rejected — the dispatcher demands a real callback.
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "a": { "process_id": "apply",
                   "arguments": { "data": { "from_node": "l" }, "process": {} },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
            "empty process object must surface InvalidGraph, got {r:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_with_malformed_subgraph_is_rejected_up_front() {
        // Sub-graph references an unknown process_id — must fail at
        // the apply-time graph-shape validation, NOT silently pass.
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "a": { "process_id": "apply",
                   "arguments": {
                       "data": { "from_node": "l" },
                       "process": { "process_graph": {
                           "u": { "process_id": "magic",
                                  "arguments": {},
                                  "result": true }
                       }}
                   },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
            "unsupported sub-process must surface InvalidGraph, got {r:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mask_rejects_when_input_is_not_a_band_cube_post_a8() {
        // Post-A8: standard `mask` is no longer a metadata pass-through —
        // it requires real `data.__cube` + `mask.__cube`. A raw
        // load_collection result with `id: "c"` is a lightweight sentinel
        // (`{"type": "DataCube"…}`), not a cube — so mask must reject.
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "m": { "process_id": "mask",
                   "arguments": { "data": { "from_node": "l" }, "mask": {} },
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
            "expected InvalidGraph from real mask process, got {r:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reduce_dimension_rejects_when_input_is_not_an_ndvi_cube_post_a4() {
        // Post-A4: reduce_dimension is no longer a metadata-tagging
        // pass-through — it requires a real `__cube.ndvi_paths` upstream.
        // Feeding it raw `load_collection` output (no NDVI computed) must
        // surface a typed InvalidGraph error per openEO spec.
        let body = graph(json!({
            "l": { "process_id": "load_collection", "arguments": { "id": "c" } },
            "r": { "process_id": "reduce_dimension",
                   "arguments": { "data": { "from_node": "l" }, "dimension": "time",
                                  "reducer": { "process_graph": { "m": {
                                      "process_id": "mean",
                                      "arguments": { "data": {"from_parameter": "data"} },
                                      "result": true
                                  }}}},
                   "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))),
            "expected InvalidGraph when reduce_dimension input lacks ndvi_paths, got {r:?}");
    }

    #[test]
    fn json_to_array2_decodes_2d_numbers() {
        let v = json!([[1, 2, 3], [4, 5, 6]]);
        let a = json_to_array2(Some(&v), "x").unwrap();
        assert_eq!(a.dim(), (2, 3));
        assert!((a[[1, 2]] - 6.0).abs() < 1e-9);
    }

    #[test]
    fn json_to_array2_rejects_ragged_rows() {
        let v = json!([[1, 2], [3, 4, 5]]);
        assert!(json_to_array2(Some(&v), "x").is_err());
    }

    #[test]
    fn json_to_array1_u8_rejects_out_of_range() {
        assert!(json_to_array1_u8(Some(&json!([300])), "y").is_err());
    }

    #[test]
    fn extract_raster_path_accepts_handle_and_bare_string() {
        let p = extract_raster_path(
            Some(&json!({"__raster": {"path": "/a/b.tif"}})),
            "x",
        ).unwrap();
        assert_eq!(p, PathBuf::from("/a/b.tif"));
        let p = extract_raster_path(Some(&json!("/c/d.tif")), "x").unwrap();
        assert_eq!(p, PathBuf::from("/c/d.tif"));
        assert!(extract_raster_path(Some(&json!(42)), "x").is_err());
        assert!(extract_raster_path(None, "x").is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chained_filters_accumulate_meta() {
        let body = graph(json!({
            "l":  { "process_id": "load_collection", "arguments": { "id": "c" } },
            "ft": { "process_id": "filter_temporal",
                    "arguments": { "data": { "from_node": "l" }, "extent": [] } },
            "fs": { "process_id": "filter_spatial",
                    "arguments": { "data": { "from_node": "ft" }, "geometries": {} },
                    "result": true }
        }));
        let r = GeoExecutor::new().run_sync(&body).await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let applied = v["_orbit_meta"]["applied"].as_array().unwrap();
        assert_eq!(applied.len(), 2, "both filters must accumulate");
        assert_eq!(applied[0]["step"], "applied_filter_temporal");
        assert_eq!(applied[1]["step"], "applied_filter_spatial");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ndvi_without_cube_input_returns_invalid_graph() {
        let exe = GeoExecutor::new();
        let body = graph(json!({
            "n": { "process_id": "ndvi",
                   "arguments": { "data": 42 },
                   "result": true }
        }));
        let r = exe.run_sync(&body).await;
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }
}
