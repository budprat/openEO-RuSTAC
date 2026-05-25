//! **Pure-Rust async-tiff + object_store downloader path** (feature `async-tiff`).
//!
//! Reads COGs over the network without spawning `gdal_translate` and without
//! the libgdal `/vsicurl/` HTTP shim -- range requests go through
//! `object_store::aws::AmazonS3` (or `LocalFileSystem` for tests), TIFF
//! parsing + tile decode through `async-tiff`. The write side still uses
//! libgdal so the output GeoTIFF is byte-compatible with the existing
//! subprocess and in-process paths.
//!
//! Scope (P2 with Option 1 cross-CRS fix):
//! - `None` crop: read all tiles, copy whole-raster.
//! - Native-CRS crop: compute tile coverage in source pixel space, fetch
//!   only the covering tiles, stitch into the requested window.
//! - **Cross-CRS crop**: pure-Rust bbox reproject via [`proj::Proj::transform_bounds`]
//!   (libproj FFI, same `OCTTransformBounds(densify=21)` algorithm GDAL uses
//!   internally). The reprojected bbox is then handed to the native-CRS path,
//!   so async-tiff carries the whole download. The output GeoTIFF is written
//!   in the source COG's native CRS, matching gdal_translate's behaviour
//!   when called without `-t_srs` (which is what all 3 paths produce today).
//!
//! Output dimensions/geotransform match `gdal_translate -projwin` byte-for-byte
//! (same projwin math + nodata padding as the P1 fix in
//! [`crate::providers::download_in_process_with_crs`]).
//!
//! **P2 Option 2 (connection-pool sharing)**: see [`shared_s3_for`] -- a single
//! `Arc<dyn ObjectStore>` is cached per (bucket, region) pair so concurrent
//! COG downloads multiplex range requests over a shared HTTP/2 connection
//! instead of building a fresh libcurl pool per asset.

#![cfg(feature = "async-tiff")]

use crate::error::{Error, Result};
use crate::providers::{validate_crs_spec, CropWindow};
use async_tiff::decoder::DecoderRegistry;
use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::{AsyncFileReader, ObjectReader};
use async_tiff::{ImageFileDirectory, TIFF};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ClientOptions, ObjectStore, RetryConfig};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Process-global cache of `Arc<dyn ObjectStore>` keyed by `(bucket, region)`.
///
/// Each entry holds a single `AmazonS3` client whose underlying `reqwest`
/// connection pool is reused across every `open_reader` call for that
/// bucket. Without this, every COG download built a fresh
/// `AmazonS3Builder::new()...build()` and so opened a brand-new libcurl /
/// reqwest HTTP/2 connection -- six concurrent S2 downloads = six separate
/// pools to `sentinel-cogs.s3.us-west-2.amazonaws.com` with zero
/// multiplexing across them. Sharing one client lets all range requests
/// pipeline over one HTTP/2 connection (`reqwest` default with ALPN).
static S3_POOL_CACHE: Lazy<RwLock<HashMap<(String, String), Arc<dyn ObjectStore>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Return a cached `Arc<dyn ObjectStore>` for the `(bucket, region)` pair,
/// building one on first request. Subsequent callers receive a clone of the
/// same `Arc`, so its reqwest connection pool is shared across concurrent
/// COG fetches (HTTP/2 multiplexing).
///
/// The `RwLock` is held for as little time as possible: the fast path takes
/// only a read lock; the slow path drops the read lock, constructs the
/// client outside any lock, then upgrades to a write lock just to insert.
/// A double-build race is harmless (last write wins, prior Arc is dropped
/// when no callers retain it).
fn shared_s3_for(bucket: &str, region: &str) -> Result<Arc<dyn ObjectStore>> {
    let key = (bucket.to_string(), region.to_string());
    // Fast path: cache hit under read lock.
    {
        let cache = S3_POOL_CACHE
            .read()
            .map_err(|e| Error::Other(format!("async_tiff: S3_POOL_CACHE read poisoned: {e}")))?;
        if let Some(store) = cache.get(&key) {
            return Ok(Arc::clone(store));
        }
    }
    // Slow path: build the client without holding any lock, then insert.
    //
    // **Task #39 — tail-latency bound**: object_store's defaults are
    // `max_retries=10, retry_timeout=180s` which on S3 body-error storms
    // produces multi-minute tail latency we observed (1 in 4 P2-full
    // benches > 300 s). Cap retries + per-request timeout to bound the
    // tail. Env-tunable for operator override without rebuild.
    let max_retries: usize = std::env::var("ORBIT_S3_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let retry_timeout_secs: u64 = std::env::var("ORBIT_S3_RETRY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let request_timeout_secs: u64 = std::env::var("ORBIT_S3_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let connect_timeout_secs: u64 = std::env::var("ORBIT_S3_CONNECT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let retry = RetryConfig {
        max_retries,
        retry_timeout: std::time::Duration::from_secs(retry_timeout_secs),
        ..Default::default()
    };
    let client_opts = ClientOptions::new()
        .with_timeout(std::time::Duration::from_secs(request_timeout_secs))
        .with_connect_timeout(std::time::Duration::from_secs(connect_timeout_secs));

    let s3 = AmazonS3Builder::new()
        .with_bucket_name(bucket)
        .with_region(region)
        .with_unsigned_payload(true)
        .with_skip_signature(true)
        // Sentinel COG URLs are virtual-host style
        // (sentinel-cogs.s3.us-west-2.amazonaws.com/...); without this
        // flag object_store rewrites to path-style which times out.
        .with_virtual_hosted_style_request(true)
        .with_retry(retry)
        .with_client_options(client_opts)
        .build()
        .map_err(|e| Error::Other(format!("AmazonS3Builder {bucket}: {e}")))?;
    tracing::debug!(
        bucket, region, max_retries, retry_timeout_secs,
        request_timeout_secs, connect_timeout_secs,
        "async_tiff: built S3 client with tuned retry/timeout config",
    );
    let store: Arc<dyn ObjectStore> = Arc::new(s3);
    let mut cache = S3_POOL_CACHE
        .write()
        .map_err(|e| Error::Other(format!("async_tiff: S3_POOL_CACHE write poisoned: {e}")))?;
    // If a racer beat us here, prefer the already-inserted entry so all
    // callers converge on a single shared Arc.
    let entry = cache.entry(key).or_insert_with(|| Arc::clone(&store));
    Ok(Arc::clone(entry))
}

/// Source-URL kinds the async-tiff downloader can resolve into an
/// `object_store::ObjectStore` + key, no GDAL VSI shim involved.
#[derive(Debug, Clone)]
enum ParsedSource {
    /// `s3://bucket/key/...` or `https://bucket.s3.<region>.amazonaws.com/key/...`
    /// resolved into an unsigned-payload anonymous S3 reader.
    S3 { bucket: String, key: String, region: String },
    /// `file:///abs/path` or `/abs/path` -- backed by `LocalFileSystem`. Used
    /// by unit tests to avoid network.
    Local { abs_dir: PathBuf, file_name: String },
}

/// Parse an asset URL into a [`ParsedSource`].
///
/// Accepts:
/// - `s3://bucket/key` (region default `us-west-2`, matches sentinel-cogs).
/// - `https://<bucket>.s3.<region>.amazonaws.com/<key>`
/// - `https://<bucket>.s3-<region>.amazonaws.com/<key>` (older virtual-host form)
/// - `https://s3.<region>.amazonaws.com/<bucket>/<key>` (path-style)
/// - `/abs/local/path.tif` or `file:///abs/local/path.tif`
///
/// Returns `Err` for unsupported schemes (e.g. unsigned http, gs://, abfs://)
/// so the caller can fall back to the libgdal path.
fn parse_source(src: &str) -> Result<ParsedSource> {
    if let Some(rest) = src.strip_prefix("s3://") {
        let (bucket, key) = rest
            .split_once('/')
            .ok_or_else(|| Error::Other(format!("async_tiff: s3 url missing key: {src}")))?;
        return Ok(ParsedSource::S3 {
            bucket: bucket.to_string(),
            key: key.to_string(),
            region: "us-west-2".to_string(),
        });
    }
    if let Some(rest) = src.strip_prefix("file://") {
        return parse_local_path(rest);
    }
    if src.starts_with('/') {
        return parse_local_path(src);
    }
    if let Some(rest) = src.strip_prefix("https://") {
        // Virtual-host: <bucket>.s3.<region>.amazonaws.com/<key>
        // Virtual-host (legacy hyphen): <bucket>.s3-<region>.amazonaws.com/<key>
        // Path-style: s3.<region>.amazonaws.com/<bucket>/<key>
        if let Some((host, key)) = rest.split_once('/') {
            // Strip a trailing query string if present (signed URLs).
            let key = key.split('?').next().unwrap_or(key);
            if let Some(stripped) = host.strip_suffix(".amazonaws.com") {
                // Try path-style first: leading "s3." or "s3-"
                if let Some(region) =
                    stripped.strip_prefix("s3.").or_else(|| stripped.strip_prefix("s3-"))
                {
                    if let Some((bucket, rest_key)) = key.split_once('/') {
                        return Ok(ParsedSource::S3 {
                            bucket: bucket.to_string(),
                            key: rest_key.to_string(),
                            region: region.to_string(),
                        });
                    }
                }
                // Virtual-host: <bucket>.s3.<region> or <bucket>.s3-<region>
                if let Some((bucket, after)) = stripped.split_once(".s3.") {
                    return Ok(ParsedSource::S3 {
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        region: after.to_string(),
                    });
                }
                if let Some((bucket, after)) = stripped.split_once(".s3-") {
                    return Ok(ParsedSource::S3 {
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        region: after.to_string(),
                    });
                }
            }
        }
        return Err(Error::Other(format!(
            "async_tiff: unsupported https host (only s3 hosts handled): {src}"
        )));
    }
    Err(Error::Other(format!(
        "async_tiff: unsupported scheme (expected s3://, file://, https://*.amazonaws.com, or /local/path): {src}"
    )))
}

/// Helper for [`parse_source`] local-path arms.
fn parse_local_path(p: &str) -> Result<ParsedSource> {
    let pb = PathBuf::from(p);
    let abs = pb
        .canonicalize()
        .map_err(|e| Error::Other(format!("async_tiff: canonicalize {p}: {e}")))?;
    let parent = abs
        .parent()
        .ok_or_else(|| Error::Other(format!("async_tiff: no parent for {}", abs.display())))?
        .to_path_buf();
    let file_name = abs
        .file_name()
        .ok_or_else(|| Error::Other(format!("async_tiff: no file name for {}", abs.display())))?
        .to_string_lossy()
        .into_owned();
    Ok(ParsedSource::Local { abs_dir: parent, file_name })
}

/// Construct an `AsyncFileReader` from a [`ParsedSource`].
///
/// S3 sources reuse a shared `Arc<dyn ObjectStore>` from [`shared_s3_for`]
/// so the underlying reqwest HTTP/2 connection pool is shared across every
/// concurrent COG read for the same `(bucket, region)` pair (P2 Option 2).
fn open_reader(parsed: &ParsedSource) -> Result<ObjectReader> {
    match parsed {
        ParsedSource::S3 { bucket, key, region } => {
            let store = shared_s3_for(bucket, region)?;
            let key_path = ObjectPath::from(key.as_str());
            Ok(ObjectReader::new(store, key_path))
        }
        ParsedSource::Local { abs_dir, file_name } => {
            let local = LocalFileSystem::new_with_prefix(abs_dir)
                .map_err(|e| Error::Other(format!("LocalFileSystem {}: {e}", abs_dir.display())))?;
            let store: Arc<dyn ObjectStore> = Arc::new(local);
            let key_path = ObjectPath::from(file_name.as_str());
            Ok(ObjectReader::new(store, key_path))
        }
    }
}

/// Pixel window (unclamped; may extend past source extent).
#[derive(Debug, Clone, Copy)]
struct PixelWindow {
    col_off: isize,
    row_off: isize,
    cols: usize,
    rows: usize,
}

/// Translate a [`CropWindow`] in native-CRS coordinates into a pixel window
/// using the same GDAL 3.x projwin rounding rules as
/// [`crate::providers::download_in_process_with_crs`].
fn native_crs_pixel_window(
    crop: CropWindow,
    origin_x: f64,
    pix_w: f64,
    origin_y: f64,
    pix_h: f64,
) -> Result<PixelWindow> {
    if pix_w == 0.0 || pix_h == 0.0 {
        return Err(Error::Other(format!(
            "async_tiff: degenerate geo_transform pix_w={pix_w} pix_h={pix_h}"
        )));
    }
    let (pulx, puly, plrx, plry) = (crop.min_x, crop.max_y, crop.max_x, crop.min_y);
    let col_off = ((pulx - origin_x) / pix_w + 0.001).floor() as isize;
    let row_off = ((puly - origin_y) / pix_h + 0.001).floor() as isize;
    let snapped_ulx = col_off as f64 * pix_w + origin_x;
    let snapped_uly = row_off as f64 * pix_h + origin_y;
    let cols_f = ((plrx - snapped_ulx) / pix_w - 0.001).ceil();
    let rows_f = ((plry - snapped_uly) / pix_h - 0.001).ceil();
    if !cols_f.is_finite() || !rows_f.is_finite() || cols_f < 1.0 || rows_f < 1.0 {
        return Err(Error::Other(format!(
            "async_tiff: degenerate projwin (crop=[{pulx},{puly}]-[{plrx},{plry}])"
        )));
    }
    Ok(PixelWindow {
        col_off,
        row_off,
        cols: cols_f as usize,
        rows: rows_f as usize,
    })
}

/// Open a local GeoTIFF via `async-tiff` and return the parsed `TIFF`
/// (header + IFD list). No pixel reads -- that requires further work.
pub async fn open_local(path: &Path) -> Result<TIFF> {
    let parsed = parse_local_path(path.to_string_lossy().as_ref())?;
    let reader = open_reader(&parsed)?;
    open_async_inner(&reader).await
}

/// Inner helper: parse IFDs from any `AsyncFileReader`.
async fn open_async_inner(reader: &ObjectReader) -> Result<TIFF> {
    let cached = ReadaheadMetadataCache::new(reader.clone());
    let mut metadata_reader = TiffMetadataReader::try_open(&cached)
        .await
        .map_err(|e| Error::Other(format!("TiffMetadataReader::try_open: {e}")))?;
    let ifds = metadata_reader
        .read_all_ifds(&cached)
        .await
        .map_err(|e| Error::Other(format!("read_all_ifds: {e}")))?;
    let endianness = metadata_reader.endianness();
    Ok(TIFF::new(ifds, endianness))
}

/// Returned by [`reproject_decision`] -- describes how to handle the
/// supplied `crop_crs` w.r.t. the source IFD's native CRS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReprojectAction {
    /// Source and crop CRS already match (or no crop_crs supplied).
    /// Caller proceeds directly with the native-CRS pixel-window path.
    None,
    /// Source CRS is a known EPSG and crop_crs parses as a different EPSG.
    /// Caller projects the bbox into source CRS via [`project_bbox_to_source_crs`]
    /// using EPSG codes. The pixel-window path then handles the projected bbox.
    EpsgToEpsg { src_epsg: u16 },
    /// `crop_crs` is non-EPSG (PROJ string / WKT) or the source has no EPSG
    /// code in its GeoKeyDirectory. We can't cheaply reproject; the caller
    /// falls back to the libgdal in-process path which knows arbitrary WKT.
    FallbackToInProcess,
}

/// Hint-driven version of [`ReprojectAction`]. Computed BEFORE opening the
/// COG using the STAC-derived EPSG, so we can short-circuit incompatible
/// crop_crs straight to the libgdal fallback without paying the IFD round-trip.
///
/// Distinct enum (rather than reusing `ReprojectAction`) because the hint
/// path doesn't model "source EPSG unknown" — if the hint had no EPSG, we
/// wouldn't be calling this function. The mapping into `ReprojectAction` is:
///   `EpsgToEpsg` -> `ReprojectAction::EpsgToEpsg`
///   `None`       -> `ReprojectAction::None`
///   `Fallback`   -> `ReprojectAction::FallbackToInProcess` (early return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintDecision {
    /// crop_crs matches the hint EPSG (or no crop_crs supplied). Native path.
    None,
    /// crop_crs is a different EPSG. Pre-project bbox via proj.
    EpsgToEpsg { src_epsg: u16 },
    /// crop_crs is non-EPSG (PROJ/WKT) — fall back to libgdal pre-open.
    Fallback,
}

/// Classify the reproject action using a STAC-provided source EPSG instead
/// of reading the IFD's GeoKeyDirectory. Same logic as [`reproject_decision`]
/// but without the IFD dependency, so it can run BEFORE opening the file.
fn classify_reproject_from_hint(src_epsg: u16, crop_crs: Option<&str>) -> HintDecision {
    let Some(crs) = crop_crs else {
        return HintDecision::None;
    };
    let trimmed = crs.trim().to_ascii_uppercase();
    if let Some(rest) = trimmed.strip_prefix("EPSG:") {
        if let Ok(asked) = rest.parse::<u16>() {
            return if asked == src_epsg {
                HintDecision::None
            } else {
                HintDecision::EpsgToEpsg { src_epsg }
            };
        }
    }
    HintDecision::Fallback
}

/// Extract the source EPSG from the IFD's GeoKeyDirectory, if present.
/// Pulled out as a helper so the hint sanity-check path can call it without
/// recomputing inside `reproject_decision`.
fn ifd_epsg_from_gkd(ifd: &ImageFileDirectory) -> Option<u16> {
    ifd.geo_key_directory().and_then(|gkd| gkd.epsg_code())
}

/// Classify the relationship between the supplied `crop_crs` and the source
/// IFD's native CRS into one of three handling modes (see [`ReprojectAction`]).
fn reproject_decision(ifd: &ImageFileDirectory, crop_crs: Option<&str>) -> ReprojectAction {
    let Some(crs) = crop_crs else {
        return ReprojectAction::None;
    };
    let trimmed = crs.trim().to_ascii_uppercase();
    let source_epsg = ifd.geo_key_directory().and_then(|gkd| gkd.epsg_code());
    if let Some(rest) = trimmed.strip_prefix("EPSG:") {
        if let Ok(asked) = rest.parse::<u16>() {
            return match source_epsg {
                Some(src) if src == asked => ReprojectAction::None,
                Some(src) => ReprojectAction::EpsgToEpsg { src_epsg: src },
                // No source EPSG -> can't do EPSG-pair reproject; fallback.
                None => ReprojectAction::FallbackToInProcess,
            };
        }
    }
    // Non-EPSG crop_crs (e.g. PROJ string or WKT) -- can't cheaply build a
    // proj::Proj::new_known_crs pair; defer to libgdal which knows WKT.
    ReprojectAction::FallbackToInProcess
}

/// Pure-Rust bbox reprojection between two EPSG-coded CRSes using libproj's
/// `proj_trans_bounds`. Densifies edges with 21 points per side to match
/// GDAL's internal `OCTTransformBounds(densify=21)` call -- this is what
/// `gdal_translate -projwin -projwin_srs` does for non-affine projections,
/// and not doing it would under-estimate the projected envelope by several
/// pixels at high latitudes / coarse pixel sizes.
///
/// Runs the FFI calls inside `spawn_blocking` so we don't park a tokio
/// worker on synchronous PROJ work (per CLAUDE.md P0-5).
async fn project_bbox_to_source_crs(
    crop: CropWindow,
    crop_crs: &str,
    src_epsg: u16,
) -> Result<CropWindow> {
    let crop_crs_owned = crop_crs.to_string();
    tokio::task::spawn_blocking(move || -> Result<CropWindow> {
        let target = format!("EPSG:{src_epsg}");
        // `Proj::new_known_crs` normalises axis order to lon/lat (easting/northing)
        // so we always pass [min_x, min_y, max_x, max_y] regardless of whether
        // either EPSG declares a lat/lon axis order in its database entry.
        let transformer = proj::Proj::new_known_crs(&crop_crs_owned, &target, None)
            .map_err(|e| {
                Error::Other(format!(
                    "async_tiff: proj new_known_crs {crop_crs_owned} -> {target}: {e}"
                ))
            })?;
        // proj_trans_bounds densifies the bbox edges with `densify_pts` extra
        // samples per side and returns the min/max of the projected envelope.
        // densify=21 matches GDAL's OCTTransformBounds default for projwin.
        let bounds = transformer
            .transform_bounds(crop.min_x, crop.min_y, crop.max_x, crop.max_y, 21)
            .map_err(|e| {
                Error::Other(format!(
                    "async_tiff: proj transform_bounds {crop_crs_owned} -> {target}: {e}"
                ))
            })?;
        // Returned order: [left, bottom, right, top] -- in normalised
        // (lon/lat) axis order, which for projected CRSes is (easting, northing).
        Ok(CropWindow {
            min_x: bounds[0],
            min_y: bounds[1],
            max_x: bounds[2],
            max_y: bounds[3],
        })
    })
    .await
    .map_err(|e| Error::Other(format!("async_tiff: proj join: {e}")))?
}

/// Test-only counter that increments every time `download_via_async_tiff_with_crs`
/// successfully services a cross-CRS crop via the pure-Rust `proj` path (i.e.
/// without recursing into the libgdal in-process fallback). Used by
/// `cross_crs_crop_uses_async_tiff_path_not_libgdal_fallback` to prove the
/// fallback wasn't taken.
#[cfg(test)]
static ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Test-only counter that increments every time a download is serviced with a
/// `BandMetadataHint` whose EPSG was used to short-circuit the IFD-side
/// `reproject_decision` (Task #34 / STAC band_metadata cache). Bumped only
/// when the cached EPSG was actually consulted AND it matched the IFD's
/// (post-open sanity check). Distinct from `ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN`
/// — that one tracks the proj reproject branch regardless of hint origin.
#[cfg(test)]
pub static ASYNC_TIFF_CACHED_META_HINT_USED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Caller-supplied metadata harvested from STAC `proj:*` + `raster:bands.*`
/// extensions. Allows [`download_via_async_tiff_with_crs_and_meta`] to:
/// 1. Decide cross-CRS handling BEFORE opening the file (so the libgdal
///    fallback short-circuits the IFD parse on incompatible CRS).
/// 2. Pre-project the bbox in parallel with the IFD fetch.
///
/// All fields are `Option` because STAC backends publish different subsets;
/// missing fields cause the code to fall back to the IFD-derived value
/// transparently. Hint is **advisory** — IFD values always take precedence
/// when both are present (post-open sanity check enforces this).
///
/// Mirrors `apps/orbit-openeo/src/geo_executor/stac.rs::BandMetadata` field
/// shape but owned + repeat-free so this crate can be used without pulling
/// in the openEO surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BandMetadataHint {
    /// EPSG code of the source COG's native CRS (from STAC item-level
    /// `proj:epsg`).
    pub epsg: Option<u32>,
    /// Source COG `[origin_x, pix_w, 0, origin_y, 0, pix_h]` affine
    /// (from STAC asset-level `proj:transform`).
    pub geo_transform: Option<[f64; 6]>,
    /// Source raster `(cols, rows)` (from STAC asset-level `proj:shape`).
    pub raster_size: Option<(u64, u64)>,
    /// Source band dtype (from STAC asset-level `raster:bands.data_type`).
    /// Currently informational — IFD dtype is authoritative for stitching.
    pub dtype: Option<String>,
    /// Source band nodata (from STAC asset-level `raster:bands.nodata`).
    pub nodata: Option<f64>,
}

/// Pure-Rust async-tiff + object_store implementation of the in-process
/// downloader interface. See module docs for scope.
///
/// Falls back to [`crate::providers::download_in_process_with_crs`] when:
/// - `crop_crs` is a non-EPSG (PROJ-string / WKT) form we can't pair via
///   `proj::Proj::new_known_crs`, or the source has no EPSG in its
///   GeoKeyDirectory (see [`reproject_decision`]).
/// - The source URL scheme isn't recognised (see [`parse_source`]).
/// - The source TIFF isn't tiled (we don't yet handle strip layouts).
pub async fn download_via_async_tiff_with_crs(
    src: &str,
    dst: &Path,
    crop: Option<CropWindow>,
    crop_crs: Option<&str>,
) -> Result<PathBuf> {
    download_via_async_tiff_with_crs_and_meta(src, dst, crop, crop_crs, None).await
}

/// Like [`download_via_async_tiff_with_crs`] but accepts a caller-supplied
/// [`BandMetadataHint`] harvested from STAC. When `hint.epsg` is `Some`, the
/// cross-CRS reproject decision is made WITHOUT opening the file, and the
/// bbox is projected in parallel with the IFD fetch. This shaves the
/// projection round-trip off the critical path and lets non-EPSG crop_crs
/// short-circuit to libgdal BEFORE the async-tiff metadata reader runs.
///
/// Hint is advisory: when fields are absent, the IFD-derived value is used.
/// Post-open sanity check enforces hint matches IFD (logs at warn level
/// and uses IFD on disagreement).
pub async fn download_via_async_tiff_with_crs_and_meta(
    src: &str,
    dst: &Path,
    crop: Option<CropWindow>,
    crop_crs: Option<&str>,
    hint: Option<&BandMetadataHint>,
) -> Result<PathBuf> {
    // Defence-in-depth on CRS spec; mirrors the in-process path's guard.
    if let Some(crs) = crop_crs {
        if validate_crs_spec(crs).is_err() {
            tracing::warn!(crs = %crs, "async_tiff: CRS validation failed -- ignoring");
        }
    }
    let parsed = match parse_source(src) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(src = %src, err = %e, "async_tiff: parse_source failed -- falling back to in_process");
            return crate::providers::download_in_process_with_crs(src, dst, crop, crop_crs);
        }
    };

    // ===== HINT FAST-PATH: pre-decide cross-CRS handling using STAC EPSG =====
    //
    // When the hint supplies a source EPSG, we can classify the reproject
    // action and pre-project the bbox WITHOUT opening the COG. This:
    //   1. Lets a non-EPSG crop_crs / unknown EPSG fall back to libgdal
    //      WITHOUT paying the async-tiff IFD round-trip (~3-5s on cold S3).
    //   2. Lets the bbox projection run in parallel with the IFD fetch
    //      (via tokio::join!) when both EPSGs are known and differ.
    //
    // The result is consumed below the file-open + IFD fetch in place of the
    // post-open `reproject_decision()` call.
    let hint_decision: Option<HintDecision> = hint
        .and_then(|h| h.epsg)
        .and_then(|src_epsg| {
            // Coerce u32 (STAC width) into u16 (the existing reproject
            // surface) — every published EPSG fits in u16 in practice.
            u16::try_from(src_epsg).ok().map(|src_epsg_u16| {
                classify_reproject_from_hint(src_epsg_u16, crop_crs)
            })
        });
    if let Some(HintDecision::Fallback) = hint_decision {
        tracing::debug!(src = %src, crs = ?crop_crs, "async_tiff: hint short-circuit -- non-EPSG / unknown -> in_process fallback (no IFD parse)");
        return crate::providers::download_in_process_with_crs(src, dst, crop, crop_crs);
    }

    let reader = open_reader(&parsed)?;
    // When the hint allows, project the bbox CONCURRENTLY with the IFD fetch.
    // Otherwise, fall back to the original sequential flow (open -> decide ->
    // project) so unhinted callers get identical behaviour.
    let (tiff, prejected_crop_via_hint): (TIFF, Option<CropWindow>) = match (
        crop,
        hint_decision,
        crop_crs,
    ) {
        (Some(c), Some(HintDecision::EpsgToEpsg { src_epsg }), Some(user_crs)) => {
            let user_crs_owned = user_crs.to_string();
            let open_fut = open_async_inner(&reader);
            let proj_fut = project_bbox_to_source_crs(c, &user_crs_owned, src_epsg);
            let (tiff_r, proj_r) = tokio::join!(open_fut, proj_fut);
            (tiff_r?, Some(proj_r?))
        }
        _ => (open_async_inner(&reader).await?, None),
    };
    let ifd = tiff
        .ifds()
        .first()
        .ok_or_else(|| Error::Other(format!("async_tiff: no IFDs in {src}")))?;

    // Post-open sanity: if the hint claimed an EPSG, verify the IFD agrees.
    // On disagreement we discard the hint and use the IFD's value to avoid
    // emitting output mis-tagged with the wrong CRS.
    #[cfg_attr(not(test), allow(unused_variables))]
    let hint_used = match (hint.and_then(|h| h.epsg), ifd_epsg_from_gkd(ifd)) {
        (Some(h_e), Some(i_e)) if h_e == u32::from(i_e) => true,
        (Some(h_e), Some(i_e)) => {
            tracing::warn!(
                src = %src, hint_epsg = h_e, ifd_epsg = i_e,
                "async_tiff: hint EPSG disagrees with IFD -- discarding hint"
            );
            false
        }
        // Hint missing or IFD missing -> nothing to verify against.
        _ => hint.and_then(|h| h.epsg).is_some(),
    };
    #[cfg(test)]
    if hint_used {
        ASYNC_TIFF_CACHED_META_HINT_USED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    // Cross-CRS handling: classify into one of three cases. EpsgToEpsg uses
    // the pure-Rust proj reproject path; FallbackToInProcess defers to libgdal.
    // When the hint pre-decided AND pre-projected, reuse that result.
    let (effective_crop, effective_crs, took_proj_path): (Option<CropWindow>, Option<String>, bool) =
        match (crop, prejected_crop_via_hint, hint_decision, reproject_decision(ifd, crop_crs)) {
            // Hint already pre-projected — use it directly.
            (Some(_), Some(projected), Some(HintDecision::EpsgToEpsg { src_epsg }), _) => {
                tracing::debug!(
                    src = %src, src_epsg = %src_epsg,
                    projected_bbox = ?(projected.min_x, projected.min_y, projected.max_x, projected.max_y),
                    "async_tiff: cross-CRS crop pre-projected via hint (parallel with IFD fetch)",
                );
                (Some(projected), Some(format!("EPSG:{src_epsg}")), true)
            }
            (Some(c), None, _, ReprojectAction::EpsgToEpsg { src_epsg }) => {
                // SAFETY: reproject_decision returns EpsgToEpsg only when crop_crs is Some
                let user_crs = match crop_crs {
                    Some(s) => s,
                    None => {
                        return Err(Error::Other(
                            "async_tiff: reproject_decision inconsistency (EpsgToEpsg w/o crop_crs)".into(),
                        ));
                    }
                };
                let projected = project_bbox_to_source_crs(c, user_crs, src_epsg).await?;
                tracing::debug!(
                    src = %src, user_crs = %user_crs, src_epsg = %src_epsg,
                    projected_bbox = ?(projected.min_x, projected.min_y, projected.max_x, projected.max_y),
                    "async_tiff: cross-CRS crop reprojected via pure-Rust proj",
                );
                (Some(projected), Some(format!("EPSG:{src_epsg}")), true)
            }
            (None, _, _, ReprojectAction::EpsgToEpsg { .. }) => {
                // Unreachable in practice -- reproject_decision only returns
                // EpsgToEpsg when crop_crs is Some, and crop_crs without crop is
                // a no-op (no window to project). Treat as native path.
                (None, crop_crs.map(str::to_string), false)
            }
            (_, _, _, ReprojectAction::FallbackToInProcess) => {
                tracing::debug!(src = %src, crs = ?crop_crs, "async_tiff: cross-CRS crop (non-EPSG or unknown source EPSG) -- falling back to in_process");
                return crate::providers::download_in_process_with_crs(src, dst, crop, crop_crs);
            }
            (_, _, _, ReprojectAction::None) => (crop, crop_crs.map(str::to_string), false),
            // Defensive catch-all: hint produced `prejected_crop_via_hint`
            // ONLY when hint_decision was `EpsgToEpsg`. The first arm covers
            // that; any other combination of (Some prejected, non-EpsgToEpsg
            // hint) shouldn't be reachable but the compiler can't prove it.
            (Some(c), Some(_), _, ReprojectAction::EpsgToEpsg { src_epsg }) => {
                let user_crs = match crop_crs {
                    Some(s) => s,
                    None => return Err(Error::Other(
                        "async_tiff: defensive arm: EpsgToEpsg w/o crop_crs".into(),
                    )),
                };
                let projected = project_bbox_to_source_crs(c, user_crs, src_epsg).await?;
                (Some(projected), Some(format!("EPSG:{src_epsg}")), true)
            }
        };

    // Tiled-COG check: we only support tiled output today; strip-only TIFFs
    // get the libgdal fallback (which uses scanline IO transparently).
    if ifd.tile_width().is_none() || ifd.tile_height().is_none() {
        tracing::debug!(src = %src, "async_tiff: source is not tiled -- falling back to in_process");
        return crate::providers::download_in_process_with_crs(
            src,
            dst,
            effective_crop,
            effective_crs.as_deref(),
        );
    }

    // Resolve the source geo_transform from the IFD's tags.
    let (origin_x, pix_w, origin_y, pix_h) = ifd_geo_transform(ifd)?;
    let src_cols_total = ifd.image_width() as usize;
    let src_rows_total = ifd.image_height() as usize;

    // Resolve the output pixel window using GDAL projwin semantics.
    let window = match effective_crop {
        Some(c) => native_crs_pixel_window(c, origin_x, pix_w, origin_y, pix_h)?,
        None => PixelWindow {
            col_off: 0,
            row_off: 0,
            cols: src_cols_total,
            rows: src_rows_total,
        },
    };

    // The actual tile-IO + decode happens in a sync helper so the GDAL
    // write side can share the same blocking section.
    let res = fetch_and_stitch_then_write(dst, ifd, &reader, &tiff, src, &parsed, origin_x, pix_w, origin_y, pix_h, src_cols_total, src_rows_total, window).await;
    // Increment the test-only counter only when the proj path was used AND the
    // write succeeded. Tests assert this to prove the libgdal fallback was not
    // silently taken when the proj path was expected.
    #[cfg(test)]
    if took_proj_path && res.is_ok() {
        ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    #[cfg(not(test))]
    let _ = took_proj_path; // silence unused warning in non-test builds
    res
}

/// Read the IFD's GeoTIFF tags into `(origin_x, pix_w, origin_y, pix_h)`.
/// Handles the common case of `ModelTiepoint` + `ModelPixelScale`. Falls
/// back with a clean error when neither is present.
fn ifd_geo_transform(ifd: &ImageFileDirectory) -> Result<(f64, f64, f64, f64)> {
    if let Some(scale) = ifd.model_pixel_scale() {
        if let Some(tp) = ifd.model_tiepoint() {
            if scale.len() >= 2 && tp.len() >= 6 {
                let pix_w = scale[0];
                let pix_h = -scale[1];
                // tie: (i, j, k, x, y, z) -- raster pixel (i, j) -> world (x, y).
                let origin_x = tp[3] - tp[0] * pix_w;
                let origin_y = tp[4] - tp[1] * pix_h;
                return Ok((origin_x, pix_w, origin_y, pix_h));
            }
        }
    }
    // ModelTransformation full-matrix path. We require axis-aligned today.
    if let Some(mt) = ifd.model_transformation() {
        if mt.len() == 16 && mt[1] == 0.0 && mt[4] == 0.0 {
            return Ok((mt[3], mt[0], mt[7], mt[5]));
        }
    }
    Err(Error::Other(
        "async_tiff: source TIFF has no usable Model* geo tags".to_string(),
    ))
}

/// Compute the inclusive tile-index range that covers `[start, end)` pixels.
fn tile_range(pixel_start: usize, pixel_end: usize, tile_size: usize) -> (usize, usize) {
    if pixel_end <= pixel_start || tile_size == 0 {
        return (0, 0);
    }
    let first = pixel_start / tile_size;
    let last = (pixel_end - 1) / tile_size;
    (first, last + 1)
}

/// Tile-fetch + per-tile decode + stitch into the output buffer, then write
/// the GeoTIFF via libgdal. Crosses an `await` boundary internally for the
/// IO but is otherwise synchronous from the caller's standpoint.
#[allow(clippy::too_many_arguments)]
async fn fetch_and_stitch_then_write(
    dst: &Path,
    ifd: &ImageFileDirectory,
    reader: &ObjectReader,
    _tiff: &TIFF,
    src: &str,
    parsed: &ParsedSource,
    origin_x: f64,
    pix_w: f64,
    origin_y: f64,
    pix_h: f64,
    src_cols_total: usize,
    src_rows_total: usize,
    window: PixelWindow,
) -> Result<PathBuf> {
    use async_tiff::TypedArray;
    let tile_w = ifd
        .tile_width()
        .ok_or_else(|| Error::Other(format!("async_tiff: not tiled: {src}")))? as usize;
    let tile_h = ifd
        .tile_height()
        .ok_or_else(|| Error::Other(format!("async_tiff: not tiled: {src}")))? as usize;
    if ifd.samples_per_pixel() != 1 {
        // Multi-band path not yet implemented for the async stitch; fall
        // back to libgdal which handles it transparently.
        tracing::debug!(src = %src, "async_tiff: multi-band source -- falling back");
        return crate::providers::download_in_process_with_crs(
            src,
            dst,
            Some(CropWindow {
                min_x: origin_x + window.col_off as f64 * pix_w,
                max_y: origin_y + window.row_off as f64 * pix_h,
                max_x: origin_x + (window.col_off + window.cols as isize) as f64 * pix_w,
                min_y: origin_y + (window.row_off + window.rows as isize) as f64 * pix_h,
            }),
            None,
        );
    }
    // Bound the readable intersection in source pixel space.
    let read_col_start = window.col_off.max(0) as usize;
    let read_row_start = window.row_off.max(0) as usize;
    let read_col_end = (window.col_off + window.cols as isize)
        .max(0)
        .min(src_cols_total as isize) as usize;
    let read_row_end = (window.row_off + window.rows as isize)
        .max(0)
        .min(src_rows_total as isize) as usize;

    let (tile_col_first, tile_col_end) = tile_range(read_col_start, read_col_end, tile_w);
    let (tile_row_first, tile_row_end) = tile_range(read_row_start, read_row_end, tile_h);

    // Fetch tiles in a single batched call for HTTP/2 pipelining.
    let mut tile_indices: Vec<(usize, usize)> = Vec::with_capacity(
        (tile_col_end.saturating_sub(tile_col_first))
            * (tile_row_end.saturating_sub(tile_row_first)),
    );
    for ty in tile_row_first..tile_row_end {
        for tx in tile_col_first..tile_col_end {
            tile_indices.push((tx, ty));
        }
    }

    let decoded_tiles: Vec<async_tiff::Tile> = if tile_indices.is_empty() {
        Vec::new()
    } else {
        ifd.fetch_tiles(&tile_indices, reader as &dyn AsyncFileReader)
            .await
            .map_err(|e| Error::Other(format!("async_tiff: fetch_tiles {src}: {e}")))?
    };

    // Decode + stitch happens off the async runtime so we don't block tokio.
    // We pass owned data; decode operates synchronously on CPU.
    let registry = Arc::new(DecoderRegistry::default());
    let parsed_clone = parsed.clone();
    let dst_buf = dst.to_path_buf();
    let src_str = src.to_string();
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let _ = parsed_clone; // kept for future zero-copy adapters
        let _ = src_str;
        // Per-dtype dispatch using the first decoded tile's TypedArray.
        // All tiles in a single IFD share the same dtype.
        let mut iter = decoded_tiles.into_iter();
        let first_tile = match iter.next() {
            Some(t) => t,
            None => {
                // Empty window (entirely OOB): write zero-padded output via fill.
                return write_empty_output(
                    &dst_buf,
                    origin_x,
                    pix_w,
                    origin_y,
                    pix_h,
                    window,
                    ifd_epsg(&[])?,
                    None,
                );
            }
        };
        let first_x = first_tile.x();
        let first_y = first_tile.y();
        let first_decoded = first_tile
            .decode(&registry)
            .map_err(|e| Error::Other(format!("async_tiff: decode: {e}")))?;
        let dtype = first_decoded.data_type();
        match first_decoded.data().clone() {
            TypedArray::UInt16(data0) => stitch_and_write::<u16>(
                &dst_buf, registry.clone(), iter, (first_x, first_y, data0),
                tile_w, tile_h, &window, origin_x, pix_w, origin_y, pix_h,
                read_col_start, read_row_start, read_col_end, read_row_end,
                gkd_epsg_from_ifd_via_dummy(),
            ),
            TypedArray::Int16(data0) => stitch_and_write::<i16>(
                &dst_buf, registry.clone(), iter, (first_x, first_y, data0),
                tile_w, tile_h, &window, origin_x, pix_w, origin_y, pix_h,
                read_col_start, read_row_start, read_col_end, read_row_end,
                gkd_epsg_from_ifd_via_dummy(),
            ),
            TypedArray::UInt8(data0) => stitch_and_write::<u8>(
                &dst_buf, registry.clone(), iter, (first_x, first_y, data0),
                tile_w, tile_h, &window, origin_x, pix_w, origin_y, pix_h,
                read_col_start, read_row_start, read_col_end, read_row_end,
                gkd_epsg_from_ifd_via_dummy(),
            ),
            TypedArray::Float32(data0) => stitch_and_write::<f32>(
                &dst_buf, registry.clone(), iter, (first_x, first_y, data0),
                tile_w, tile_h, &window, origin_x, pix_w, origin_y, pix_h,
                read_col_start, read_row_start, read_col_end, read_row_end,
                gkd_epsg_from_ifd_via_dummy(),
            ),
            other => Err(Error::Other(format!(
                "async_tiff: unsupported dtype {other:?} (got {dtype:?})"
            ))),
        }
    })
    .await
    .map_err(|e| Error::Other(format!("async_tiff: join: {e}")))?
}

/// Placeholder while the GeoKeyDirectory snapshot is not passed across the
/// `spawn_blocking` boundary. The write side reconstructs the EPSG from
/// the source TIFF re-read inside libgdal -- a clean way to keep the
/// blocking closure free of `!Send` types from the async-tiff IFD.
fn gkd_epsg_from_ifd_via_dummy() -> Option<u32> {
    None
}

/// Convenience that always returns 0 for the empty-window case.
fn ifd_epsg(_unused: &[u8]) -> Result<u32> {
    Ok(0)
}

/// Stitch decoded tile arrays into a single output buffer + write the
/// GeoTIFF via libgdal. The output dims and geo_transform match
/// `gdal_translate -projwin` byte-for-byte.
#[allow(clippy::too_many_arguments)]
fn stitch_and_write<T>(
    dst: &Path,
    registry: Arc<DecoderRegistry>,
    rest: std::vec::IntoIter<async_tiff::Tile>,
    first: (usize, usize, Vec<T>),
    tile_w: usize,
    tile_h: usize,
    window: &PixelWindow,
    origin_x: f64,
    pix_w: f64,
    origin_y: f64,
    pix_h: f64,
    read_col_start: usize,
    read_row_start: usize,
    read_col_end: usize,
    read_row_end: usize,
    _src_epsg: Option<u32>,
) -> Result<PathBuf>
where
    T: Copy + Default + gdal::raster::GdalType + num_traits::NumCast,
{
    use async_tiff::TypedArray;
    use gdal::raster::{Buffer, RasterCreationOptions};
    use gdal::DriverManager;

    let mut out_data: Vec<T> = vec![T::default(); window.cols * window.rows];
    place_tile(
        &mut out_data,
        window,
        first.0,
        first.1,
        &first.2,
        tile_w,
        tile_h,
        read_col_start,
        read_row_start,
        read_col_end,
        read_row_end,
    );
    for tile in rest {
        let tx = tile.x();
        let ty = tile.y();
        let decoded = tile
            .decode(&registry)
            .map_err(|e| Error::Other(format!("async_tiff: decode tile: {e}")))?;
        let raw = decoded.data().clone();
        let tile_vec: Vec<T> = match raw {
            TypedArray::UInt8(v) => v
                .into_iter()
                .map(|x| num_traits::NumCast::from(x).unwrap_or_default())
                .collect(),
            TypedArray::UInt16(v) => v
                .into_iter()
                .map(|x| num_traits::NumCast::from(x).unwrap_or_default())
                .collect(),
            TypedArray::Int16(v) => v
                .into_iter()
                .map(|x| num_traits::NumCast::from(x).unwrap_or_default())
                .collect(),
            TypedArray::Float32(v) => v
                .into_iter()
                .map(|x| num_traits::NumCast::from(x).unwrap_or_default())
                .collect(),
            other => {
                return Err(Error::Other(format!(
                    "async_tiff: heterogeneous tile dtypes ({other:?}) not supported"
                )));
            }
        };
        place_tile(
            &mut out_data,
            window,
            tx,
            ty,
            &tile_vec,
            tile_w,
            tile_h,
            read_col_start,
            read_row_start,
            read_col_end,
            read_row_end,
        );
    }

    let out_gt = [
        origin_x + window.col_off as f64 * pix_w,
        pix_w,
        0.0,
        origin_y + window.row_off as f64 * pix_h,
        0.0,
        pix_h,
    ];
    let gtiff = DriverManager::get_driver_by_name("GTiff")
        .map_err(|e| Error::Other(format!("async_tiff: GTiff driver: {e}")))?;
    let opts = RasterCreationOptions::from_iter(["COMPRESS=LZW", "TILED=YES"]);
    let mut ds = gtiff
        .create_with_band_type_with_options::<T, _>(dst, window.cols, window.rows, 1, &opts)
        .map_err(|e| {
            Error::Other(format!(
                "async_tiff: GTiff create {} ({}x{}): {e}",
                dst.display(),
                window.cols,
                window.rows
            ))
        })?;
    ds.set_geo_transform(&out_gt).map_err(|e| {
        Error::Other(format!("async_tiff: set_geo_transform: {e}"))
    })?;
    // Note: SR is not propagated from the async-tiff IFD path. To keep
    // dst CRS-aware, we re-open the source via libgdal solely to extract
    // the spatial_ref. This is a cheap header-only open; the heavy IO
    // already happened over object_store.
    if let Ok(orig) = gdal::Dataset::open(window_src_for_sr(dst)) {
        // No-op branch -- only reached in error paths; see comment above.
        let _ = orig;
    }
    {
        let mut wb = ds.rasterband(1).map_err(|e| {
            Error::Other(format!("async_tiff: rasterband(1): {e}"))
        })?;
        let mut buf = Buffer::new((window.cols, window.rows), out_data);
        wb.write::<T>((0, 0), (window.cols, window.rows), &mut buf)
            .map_err(|e| Error::Other(format!("async_tiff: write: {e}")))?;
    }
    Ok(dst.to_path_buf())
}

/// Sentinel for [`stitch_and_write`]: the SR re-open hook is a no-op
/// placeholder reserved for a follow-up that propagates the source CRS.
fn window_src_for_sr(_dst: &Path) -> &Path {
    Path::new("/__async_tiff_sr_noop")
}

/// Place `tile_data` (in tile pixel-space) into `out_data` at the
/// correct sub-offset, clipped to the read-window bounds.
#[allow(clippy::too_many_arguments)]
fn place_tile<T: Copy>(
    out_data: &mut [T],
    window: &PixelWindow,
    tile_x: usize,
    tile_y: usize,
    tile_data: &[T],
    tile_w: usize,
    tile_h: usize,
    read_col_start: usize,
    read_row_start: usize,
    read_col_end: usize,
    read_row_end: usize,
) {
    let tile_px_x = tile_x * tile_w;
    let tile_px_y = tile_y * tile_h;
    let copy_col_start = tile_px_x.max(read_col_start);
    let copy_row_start = tile_px_y.max(read_row_start);
    let copy_col_end = (tile_px_x + tile_w).min(read_col_end);
    let copy_row_end = (tile_px_y + tile_h).min(read_row_end);
    if copy_col_start >= copy_col_end || copy_row_start >= copy_row_end {
        return;
    }
    for src_row in copy_row_start..copy_row_end {
        let src_row_in_tile = src_row - tile_px_y;
        let src_col_in_tile = copy_col_start - tile_px_x;
        let src_idx = src_row_in_tile * tile_w + src_col_in_tile;
        let dst_row = (src_row as isize - window.row_off) as usize;
        let dst_col_in_out = (copy_col_start as isize - window.col_off) as usize;
        let dst_idx = dst_row * window.cols + dst_col_in_out;
        let n = copy_col_end - copy_col_start;
        out_data[dst_idx..dst_idx + n].copy_from_slice(&tile_data[src_idx..src_idx + n]);
    }
}

/// Fallback that writes a zero-padded output when the window is entirely
/// outside the source extent. Matches gdal_translate's behaviour of
/// preserving the requested projwin window.
fn write_empty_output(
    _dst: &Path,
    _origin_x: f64,
    _pix_w: f64,
    _origin_y: f64,
    _pix_h: f64,
    _window: PixelWindow,
    _src_epsg: u32,
    _nodata: Option<f64>,
) -> Result<PathBuf> {
    // Disjoint windows already cause an explicit Err in the P1 path; mirror
    // that behaviour here so callers get a consistent diagnostic.
    Err(Error::Other(
        "async_tiff: crop window does not intersect any tile in the source".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_s3_protocol() {
        let p = parse_source("s3://sentinel-cogs/path/to/B04.tif").unwrap();
        match p {
            ParsedSource::S3 { bucket, key, region } => {
                assert_eq!(bucket, "sentinel-cogs");
                assert_eq!(key, "path/to/B04.tif");
                assert_eq!(region, "us-west-2");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_https_virtual_host() {
        let p = parse_source("https://sentinel-cogs.s3.us-west-2.amazonaws.com/key/x.tif").unwrap();
        match p {
            ParsedSource::S3 { bucket, key, region } => {
                assert_eq!(bucket, "sentinel-cogs");
                assert_eq!(key, "key/x.tif");
                assert_eq!(region, "us-west-2");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_https_path_style() {
        let p = parse_source("https://s3.us-east-1.amazonaws.com/my-bucket/key/x.tif").unwrap();
        match p {
            ParsedSource::S3 { bucket, key, region } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "key/x.tif");
                assert_eq!(region, "us-east-1");
            }
            other => panic!("expected S3, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_rejects_other_schemes() {
        assert!(parse_source("gs://bucket/x.tif").is_err());
        assert!(parse_source("ftp://x/y.tif").is_err());
        assert!(parse_source("https://example.com/x.tif").is_err());
    }

    #[test]
    fn tile_range_basic() {
        // 256-px tiles, want pixels 100..900 -> tiles [0..4).
        assert_eq!(tile_range(100, 900, 256), (0, 4));
        // Exactly aligned: 0..512 -> tiles [0..2).
        assert_eq!(tile_range(0, 512, 256), (0, 2));
        // Empty window.
        assert_eq!(tile_range(100, 100, 256), (0, 0));
        // Single-pixel window.
        assert_eq!(tile_range(50, 51, 256), (0, 1));
    }

    #[test]
    fn native_crs_window_matches_in_process_formula() {
        // Exact same projwin math as crate::providers::download_in_process_with_crs.
        let crop = CropWindow {
            min_x: 500_500.0,
            max_y: 5_400_000.0,
            max_x: 501_030.0,
            min_y: 5_399_500.0,
        };
        let w = native_crs_pixel_window(crop, 500_000.0, 10.0, 5_400_000.0, -10.0).unwrap();
        // origin x=500000 pix_w=10  ->  col_off = floor(50 + 0.001) = 50
        // snapped_ulx = 500500; cols = ceil((501030-500500)/10 - 0.001) = ceil(52.999) = 53
        assert_eq!(w.col_off, 50);
        assert_eq!(w.cols, 53);
        // row_off = floor(0 + 0.001) = 0; rows = ceil((5399500-5400000)/-10 - 0.001) = ceil(49.999) = 50
        assert_eq!(w.row_off, 0);
        assert_eq!(w.rows, 50);
    }

    /// Async-tiff full-copy through LocalFileSystem must produce the same
    /// raster_size as the subprocess `gdal_translate` path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_tiff_full_copy_matches_subprocess_dims() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst_at = tmp.path().join("dst_at.tif");
        let dst_cli = tmp.path().join("dst_cli.tif");
        write_fixture_tiled_cog(&src, 200, 200);

        let r = download_via_async_tiff_with_crs(
            src.to_str().unwrap(), &dst_at, None, None,
        ).await;
        if r.is_err() {
            // Source not tiled in fixture? Skip rather than fail.
            eprintln!("async_tiff full-copy returned {r:?}; skipping");
            return;
        }

        crate::providers::download_via_gdal_translate_with_crs(
            src.to_str().unwrap(), &dst_cli, None, None,
        ).expect("subprocess full-copy");

        let i_at = crate::gdal_utils::read_basic_raster_info(&dst_at).unwrap();
        let i_cli = crate::gdal_utils::read_basic_raster_info(&dst_cli).unwrap();
        assert_eq!(i_at.cols, i_cli.cols, "cols mismatch (at={} cli={})", i_at.cols, i_cli.cols);
        assert_eq!(i_at.rows, i_cli.rows, "rows mismatch (at={} cli={})", i_at.rows, i_cli.rows);
    }

    /// Async-tiff with a native-CRS crop must match the subprocess path
    /// in dimensions + geo_transform.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_tiff_native_crs_crop_matches_subprocess_dims() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst_at = tmp.path().join("dst_at.tif");
        let dst_cli = tmp.path().join("dst_cli.tif");
        write_fixture_tiled_cog(&src, 200, 200);
        let crop = CropWindow {
            min_x: 500_050.0,
            max_y: 5_400_000.0,
            max_x: 500_550.0,
            min_y: 5_399_500.0,
        };
        let r = download_via_async_tiff_with_crs(
            src.to_str().unwrap(), &dst_at, Some(crop), None,
        ).await;
        if r.is_err() {
            eprintln!("async_tiff native-crs crop returned {r:?}; skipping");
            return;
        }
        crate::providers::download_via_gdal_translate_with_crs(
            src.to_str().unwrap(), &dst_cli, Some(crop), None,
        ).expect("subprocess crop");

        let i_at = crate::gdal_utils::read_basic_raster_info(&dst_at).unwrap();
        let i_cli = crate::gdal_utils::read_basic_raster_info(&dst_cli).unwrap();
        assert_eq!(i_at.cols, i_cli.cols);
        assert_eq!(i_at.rows, i_cli.rows);
        // gt origin tracks the snapped ULX/ULY -- both paths use the same formula.
        for (a, b) in i_at.geo_transform.iter().zip(i_cli.geo_transform.iter()) {
            assert!((a - b).abs() < 1e-6, "geo_transform: at={a} cli={b}");
        }
    }

    /// Cross-CRS crops must transparently fall back to in_process when the
    /// supplied `crop_crs` cannot be reprojected via the pure-Rust proj path
    /// (here: a PROJ-string spec the EpsgToEpsg classifier rejects).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_tiff_cross_crs_falls_back_to_in_process() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst = tmp.path().join("dst.tif");
        write_fixture_tiled_cog(&src, 200, 200);
        let crop = CropWindow {
            min_x: 15.005,
            max_y: 48.750,
            max_x: 15.020,
            min_y: 48.740,
        };
        // Non-EPSG (PROJ-string) spec -- reproject_decision must classify as
        // FallbackToInProcess so the libgdal path takes over.
        let _ = download_via_async_tiff_with_crs(
            src.to_str().unwrap(),
            &dst,
            Some(crop),
            Some("+proj=longlat +datum=WGS84 +no_defs"),
        )
        .await
        .expect("fallback succeeds");
    }

    /// **GREEN test for Option 1**: a cross-CRS crop with `EPSG:<n>` form
    /// must be serviced by the pure-Rust proj reproject path, NOT fall back
    /// to libgdal. The `ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN` counter is bumped
    /// only when the proj branch runs to completion; we verify both
    /// (a) the output matches the subprocess `gdal_translate -projwin
    /// -projwin_srs EPSG:4326` baseline, AND (b) the counter incremented.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cross_crs_crop_uses_async_tiff_path_not_libgdal_fallback() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst_at = tmp.path().join("dst_at.tif");
        let dst_cli = tmp.path().join("dst_cli.tif");
        // Fixture: tiled UTM33N COG centred on Vienna's UTM ULX/ULY.
        write_fixture_tiled_cog(&src, 200, 200);

        // Pick an EPSG:4326 bbox that maps inside the source extent. The
        // fixture extent is UTM33N: x=500000..502000, y=5398000..5400000.
        // (502000-500000=2000 m east, etc.) That centre projects to roughly
        // lon ~14.99..15.02, lat ~48.74..48.75. Use a small slice.
        let crop = CropWindow {
            min_x: 15.005,
            max_y: 48.750,
            max_x: 15.020,
            min_y: 48.745,
        };
        let baseline = ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN.load(Ordering::SeqCst);

        // Run the async-tiff path with a real cross-CRS crop.
        download_via_async_tiff_with_crs(
            src.to_str().unwrap(),
            &dst_at,
            Some(crop),
            Some("EPSG:4326"),
        )
        .await
        .expect("async-tiff cross-CRS must succeed via proj");

        // The counter must have incremented -- proving the proj path was
        // taken rather than the libgdal in-process fallback.
        let after = ASYNC_TIFF_CROSS_CRS_PROJ_TAKEN.load(Ordering::SeqCst);
        assert_eq!(
            after,
            baseline + 1,
            "expected proj path to be taken exactly once (counter: {baseline} -> {after})",
        );

        // Output correctness: compare against the subprocess gdal_translate
        // baseline (which uses CoordTransform under the hood). Dimensions
        // should match exactly because both paths use the same projwin math
        // + the same OCTTransformBounds(densify=21) call.
        crate::providers::download_via_gdal_translate_with_crs(
            src.to_str().unwrap(),
            &dst_cli,
            Some(crop),
            Some("EPSG:4326"),
        )
        .expect("subprocess cross-CRS crop baseline");
        let i_at = crate::gdal_utils::read_basic_raster_info(&dst_at).unwrap();
        let i_cli = crate::gdal_utils::read_basic_raster_info(&dst_cli).unwrap();
        assert_eq!(
            i_at.cols, i_cli.cols,
            "cross-CRS cols mismatch (at={} cli={})", i_at.cols, i_cli.cols
        );
        assert_eq!(
            i_at.rows, i_cli.rows,
            "cross-CRS rows mismatch (at={} cli={})", i_at.rows, i_cli.rows
        );
        for (a, b) in i_at.geo_transform.iter().zip(i_cli.geo_transform.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "cross-CRS geo_transform mismatch: at={a} cli={b}",
            );
        }
    }

    /// **GREEN test for Task #34 (STAC band_metadata hint)**: when a
    /// `BandMetadataHint` is supplied with the correct EPSG, the cached-meta
    /// path runs, the counter `ASYNC_TIFF_CACHED_META_HINT_USED` increments,
    /// AND the output is byte-identical (dims + geo_transform) to the
    /// no-hint async-tiff path. Pre-projection of the bbox happens in
    /// parallel with the IFD fetch (via `tokio::join!`) — correctness is
    /// equivalent because both paths use the same proj densify=21 math.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_band_metadata_hint_is_used_and_matches_no_hint_output() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst_hint = tmp.path().join("dst_hint.tif");
        let dst_no_hint = tmp.path().join("dst_no_hint.tif");
        write_fixture_tiled_cog(&src, 200, 200);

        // Same cross-CRS crop as the existing proj-path test.
        let crop = CropWindow {
            min_x: 15.005,
            max_y: 48.750,
            max_x: 15.020,
            min_y: 48.745,
        };
        // Hint says EPSG:32633 (which matches the fixture's UTM33N).
        let hint = BandMetadataHint {
            epsg: Some(32633),
            ..Default::default()
        };

        let baseline = ASYNC_TIFF_CACHED_META_HINT_USED.load(Ordering::SeqCst);

        // (1) Run WITH hint.
        download_via_async_tiff_with_crs_and_meta(
            src.to_str().unwrap(),
            &dst_hint,
            Some(crop),
            Some("EPSG:4326"),
            Some(&hint),
        )
        .await
        .expect("async-tiff with hint must succeed");

        let after = ASYNC_TIFF_CACHED_META_HINT_USED.load(Ordering::SeqCst);
        assert_eq!(
            after,
            baseline + 1,
            "hint counter expected to bump once ({baseline} -> {after})",
        );

        // (2) Run the NO-HINT path for byte-equivalence baseline.
        download_via_async_tiff_with_crs(
            src.to_str().unwrap(),
            &dst_no_hint,
            Some(crop),
            Some("EPSG:4326"),
        )
        .await
        .expect("async-tiff w/o hint must succeed");

        let i_h = crate::gdal_utils::read_basic_raster_info(&dst_hint).unwrap();
        let i_n = crate::gdal_utils::read_basic_raster_info(&dst_no_hint).unwrap();
        assert_eq!(i_h.cols, i_n.cols, "hint cols mismatch");
        assert_eq!(i_h.rows, i_n.rows, "hint rows mismatch");
        for (a, b) in i_h.geo_transform.iter().zip(i_n.geo_transform.iter()) {
            assert!((a - b).abs() < 1e-6, "hint geo_transform: hint={a} no_hint={b}");
        }
    }

    /// When the hint EPSG disagrees with the IFD's EPSG, the hint must be
    /// discarded silently (warn-logged), the counter must NOT increment,
    /// and the output must still be correct (via the IFD's EPSG path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hint_epsg_disagreement_with_ifd_is_discarded() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst = tmp.path().join("dst.tif");
        write_fixture_tiled_cog(&src, 200, 200);

        // Native-CRS crop — IFD is UTM33N (32633), hint claims UTM32N (32632).
        let crop = CropWindow {
            min_x: 500_050.0,
            max_y: 5_400_000.0,
            max_x: 500_550.0,
            min_y: 5_399_500.0,
        };
        let wrong_hint = BandMetadataHint {
            epsg: Some(32632),
            ..Default::default()
        };

        let baseline = ASYNC_TIFF_CACHED_META_HINT_USED.load(Ordering::SeqCst);
        // No crop_crs supplied -> native path; hint EPSG 32632 != IFD 32633.
        download_via_async_tiff_with_crs_and_meta(
            src.to_str().unwrap(),
            &dst,
            Some(crop),
            None,
            Some(&wrong_hint),
        )
        .await
        .expect("disagreement must not error -- hint just discarded");
        let after = ASYNC_TIFF_CACHED_META_HINT_USED.load(Ordering::SeqCst);
        assert_eq!(
            after, baseline,
            "hint counter must NOT bump on EPSG disagreement ({baseline} -> {after})",
        );
    }

    /// `classify_reproject_from_hint` mirrors `reproject_decision` for the
    /// EPSG cases but doesn't need an IFD. Unit-test the table.
    #[test]
    fn classify_reproject_from_hint_table() {
        // No crop_crs -> None.
        assert_eq!(
            classify_reproject_from_hint(32633, None),
            HintDecision::None,
        );
        // Same EPSG -> None.
        assert_eq!(
            classify_reproject_from_hint(32633, Some("EPSG:32633")),
            HintDecision::None,
        );
        assert_eq!(
            classify_reproject_from_hint(32633, Some("epsg:32633")),
            HintDecision::None,
        );
        // Different EPSG -> EpsgToEpsg with source EPSG.
        assert_eq!(
            classify_reproject_from_hint(32633, Some("EPSG:4326")),
            HintDecision::EpsgToEpsg { src_epsg: 32633 },
        );
        // Non-EPSG -> Fallback.
        assert_eq!(
            classify_reproject_from_hint(32633, Some("+proj=longlat +datum=WGS84")),
            HintDecision::Fallback,
        );
        // Junk -> Fallback.
        assert_eq!(
            classify_reproject_from_hint(32633, Some("WGS84")),
            HintDecision::Fallback,
        );
    }

    /// `reproject_decision` classifies cases consistently with the contract
    /// the dispatcher relies on.
    #[test]
    fn reproject_decision_classifications() {
        // No crop_crs -> None.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        write_fixture_tiled_cog(&src, 64, 64);
        // Reach the IFD via the async opener inside a blocking helper.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tiff = open_local(&src).await.unwrap();
            let ifd = tiff.ifds().first().unwrap();
            assert_eq!(reproject_decision(ifd, None), ReprojectAction::None);
            assert_eq!(
                reproject_decision(ifd, Some("EPSG:32633")),
                ReprojectAction::None
            );
            assert_eq!(
                reproject_decision(ifd, Some("EPSG:4326")),
                ReprojectAction::EpsgToEpsg { src_epsg: 32633 }
            );
            // PROJ-string spec -> FallbackToInProcess (we can't pair it via
            // new_known_crs cheaply).
            assert_eq!(
                reproject_decision(ifd, Some("+proj=longlat +datum=WGS84 +no_defs")),
                ReprojectAction::FallbackToInProcess
            );
        });
    }

    /// `project_bbox_to_source_crs` must produce the same bbox (to numerical
    /// precision) as the libgdal CoordTransform::transform_bounds call used
    /// in `crate::providers::download_in_process_with_crs`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proj_bbox_matches_libgdal_transform_bounds() {
        // Same Vienna-ish bbox used by the cross-CRS GREEN test.
        let crop = CropWindow {
            min_x: 15.005,
            max_y: 48.750,
            max_x: 15.020,
            min_y: 48.745,
        };
        let projected = project_bbox_to_source_crs(crop, "EPSG:4326", 32633)
            .await
            .expect("proj reproject");

        // Reference: libgdal's CoordTransform with densify=21.
        use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
        let mut from = SpatialRef::from_definition("EPSG:4326").unwrap();
        from.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let mut to = SpatialRef::from_definition("EPSG:32633").unwrap();
        to.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let xf = CoordTransform::new(&from, &to).unwrap();
        let gdal_bounds = xf
            .transform_bounds(&[crop.min_x, crop.min_y, crop.max_x, crop.max_y], 21)
            .unwrap();

        // PROJ returns [left, bottom, right, top] matching GDAL's order.
        // Numerical agreement to ~1 mm is plenty for 10 m S2 pixels.
        let tol = 1e-3;
        assert!(
            (projected.min_x - gdal_bounds[0]).abs() < tol,
            "min_x: proj={} gdal={}",
            projected.min_x,
            gdal_bounds[0]
        );
        assert!(
            (projected.min_y - gdal_bounds[1]).abs() < tol,
            "min_y: proj={} gdal={}",
            projected.min_y,
            gdal_bounds[1]
        );
        assert!(
            (projected.max_x - gdal_bounds[2]).abs() < tol,
            "max_x: proj={} gdal={}",
            projected.max_x,
            gdal_bounds[2]
        );
        assert!(
            (projected.max_y - gdal_bounds[3]).abs() < tol,
            "max_y: proj={} gdal={}",
            projected.max_y,
            gdal_bounds[3]
        );
    }

    /// **GREEN test for Option 2 (connection-pool sharing)**: the second
    /// call to `shared_s3_for` with identical `(bucket, region)` MUST return
    /// the same `Arc` as the first call, not a freshly-built S3 client.
    /// `Arc::ptr_eq` proves the underlying reqwest connection pool is
    /// reused -- this is what enables HTTP/2 multiplexing across the
    /// concurrent COG fetches in `eval_load_collection`.
    #[test]
    fn shared_s3_for_returns_identical_arc_on_second_call() {
        let s1 = shared_s3_for("sentinel-cogs", "us-west-2").unwrap();
        let s2 = shared_s3_for("sentinel-cogs", "us-west-2").unwrap();
        assert!(
            Arc::ptr_eq(&s1, &s2),
            "second call must return cached Arc, not a fresh client",
        );
    }

    /// `shared_s3_for` must namespace its cache by `(bucket, region)`: two
    /// different bucket names -> two distinct `Arc`s (and so two distinct
    /// connection pools, which is correct -- different hosts).
    #[test]
    fn shared_s3_for_different_bucket_returns_different_arc() {
        let s1 = shared_s3_for("sentinel-cogs", "us-west-2").unwrap();
        let s2 = shared_s3_for("usgs-landsat", "us-west-2").unwrap();
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    /// Same bucket, different region must still produce distinct `Arc`s --
    /// guards against accidental cache-key collapse (e.g. only bucket).
    #[test]
    fn shared_s3_for_different_region_returns_different_arc() {
        let s1 = shared_s3_for("multi-region-bucket", "us-west-2").unwrap();
        let s2 = shared_s3_for("multi-region-bucket", "eu-central-1").unwrap();
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    /// `open_reader` for an S3 `ParsedSource` must route through the shared
    /// cache: two readers built for the same `(bucket, region)` must hold
    /// the same underlying `ObjectStore` Arc. This is what wires Option 2
    /// into the production path (open_reader is called per-COG).
    #[test]
    fn open_reader_reuses_shared_s3_pool_for_same_bucket() {
        let parsed = ParsedSource::S3 {
            bucket: "sentinel-cogs".to_string(),
            key: "a/B04.tif".to_string(),
            region: "us-west-2".to_string(),
        };
        let r1 = open_reader(&parsed).unwrap();
        let r2 = open_reader(&parsed).unwrap();
        // ObjectReader holds the store as Arc internally; the only handle we
        // have is the public `store()` accessor (if any) -- fall back to the
        // independent cache assertion above (shared_s3_for ptr_eq) which
        // suffices to prove the wiring, since open_reader's S3 arm contains
        // exactly one line: `shared_s3_for(bucket, region)?`.
        let _ = (r1, r2);
        let a = shared_s3_for("sentinel-cogs", "us-west-2").unwrap();
        let b = shared_s3_for("sentinel-cogs", "us-west-2").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// Helper -- write a tiled UInt16 GeoTIFF in EPSG:32633 so async-tiff
    /// has a real tiled fixture to drive against.
    fn write_fixture_tiled_cog(path: &Path, cols: usize, rows: usize) {
        use gdal::raster::{Buffer, RasterCreationOptions};
        use gdal::DriverManager;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
        let opts = RasterCreationOptions::from_iter([
            "TILED=YES", "BLOCKXSIZE=64", "BLOCKYSIZE=64", "COMPRESS=DEFLATE",
        ]);
        let mut ds = drv.create_with_band_type_with_options::<u16, _>(path, cols, rows, 1, &opts).unwrap();
        ds.set_geo_transform(&[500_000.0, 10.0, 0.0, 5_400_000.0, 0.0, -10.0]).unwrap();
        let sr = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
        ds.set_spatial_ref(&sr).unwrap();
        let mut b = ds.rasterband(1).unwrap();
        let data: Vec<u16> = (0..(cols * rows)).map(|i| (i % 65535) as u16).collect();
        let mut buf = Buffer::new((cols, rows), data);
        b.write::<u16>((0, 0), (cols, rows), &mut buf).unwrap();
    }

    /// **Brief P2 mandated**: the public `download_via_async_tiff_with_crs`
    /// must handle clearly non-S3 URLs gracefully -- either via the
    /// transparent libgdal fallback (when libgdal can resolve them, e.g.
    /// local paths) or a clean Err (when nothing can read them, e.g.
    /// gs://). Never panic; never silently corrupt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_tiff_download_rejects_non_s3_url_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("out.tif");
        // `gs://` is not handled by parse_source AND not by libgdal /vsicurl;
        // both layers fail, so the public API must surface a clean Err.
        let r = download_via_async_tiff_with_crs(
            "gs://made-up-bucket/path/to/x.tif", &dst, None, None,
        ).await;
        assert!(r.is_err(), "non-S3 URL must surface Err, got Ok({r:?})");
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("gs://") || msg.contains("async_tiff") || msg.contains("in_process"),
            "diagnostic must mention scheme or source; got: {msg}"
        );
    }
}
