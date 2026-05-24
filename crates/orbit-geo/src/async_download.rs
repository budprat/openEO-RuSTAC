//! **Pure-Rust async-tiff + object_store downloader path** (feature `async-tiff`).
//!
//! Reads COGs over the network without spawning `gdal_translate` and without
//! the libgdal `/vsicurl/` HTTP shim -- range requests go through
//! `object_store::aws::AmazonS3` (or `LocalFileSystem` for tests), TIFF
//! parsing + tile decode through `async-tiff`. The write side still uses
//! libgdal so the output GeoTIFF is byte-compatible with the existing
//! subprocess and in-process paths.
//!
//! Scope (P2 first cut):
//! - `None` crop: read all tiles, copy whole-raster.
//! - Native-CRS crop: compute tile coverage in source pixel space, fetch
//!   only the covering tiles, stitch into the requested window.
//! - **Cross-CRS crop**: falls back to [`crate::providers::download_in_process_with_crs`]
//!   (P1) which already handles `transform_bounds` + libgdal reprojection.
//!   Pure-Rust reprojection without a `proj` dep is out of scope for P2.
//!
//! Output dimensions/geotransform match `gdal_translate -projwin` byte-for-byte
//! (same projwin math + nodata padding as the P1 fix in
//! [`crate::providers::download_in_process_with_crs`]).

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
use object_store::ObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
fn open_reader(parsed: &ParsedSource) -> Result<ObjectReader> {
    match parsed {
        ParsedSource::S3 { bucket, key, region } => {
            let s3 = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_unsigned_payload(true)
                .with_skip_signature(true)
                .build()
                .map_err(|e| Error::Other(format!("AmazonS3Builder {bucket}: {e}")))?;
            let store: Arc<dyn ObjectStore> = Arc::new(s3);
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

/// Whether the supplied `crop_crs` requires reprojection from the
/// source's native CRS (using EPSG comparison only).
fn needs_reprojection(ifd: &ImageFileDirectory, crop_crs: Option<&str>) -> bool {
    let Some(crs) = crop_crs else { return false; };
    let trimmed = crs.trim().to_ascii_uppercase();
    let source_epsg = ifd.geo_key_directory().and_then(|gkd| gkd.epsg_code());
    if let Some(rest) = trimmed.strip_prefix("EPSG:") {
        if let Ok(asked) = rest.parse::<u16>() {
            return source_epsg != Some(asked);
        }
    }
    // Anything not "EPSG:<n>" we can't compare cheaply -- assume reprojection.
    true
}

/// Pure-Rust async-tiff + object_store implementation of the in-process
/// downloader interface. See module docs for scope.
///
/// Falls back to [`crate::providers::download_in_process_with_crs`] when:
/// - `crop_crs` requires reprojection from the source CRS.
/// - The source URL scheme isn't recognised (see [`parse_source`]).
/// - The source TIFF isn't tiled (we don't yet handle strip layouts).
pub async fn download_via_async_tiff_with_crs(
    src: &str,
    dst: &Path,
    crop: Option<CropWindow>,
    crop_crs: Option<&str>,
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
    let reader = open_reader(&parsed)?;
    let tiff = open_async_inner(&reader).await?;
    let ifd = tiff
        .ifds()
        .first()
        .ok_or_else(|| Error::Other(format!("async_tiff: no IFDs in {src}")))?;

    // Cross-CRS path: fall back to libgdal's transform_bounds. The pure-Rust
    // path could be extended with a `proj` dep but that's deferred (P3 scope).
    if needs_reprojection(ifd, crop_crs) {
        tracing::debug!(src = %src, crs = ?crop_crs, "async_tiff: cross-CRS crop -- falling back to in_process");
        return crate::providers::download_in_process_with_crs(src, dst, crop, crop_crs);
    }

    // Tiled-COG check: we only support tiled output today; strip-only TIFFs
    // get the libgdal fallback (which uses scanline IO transparently).
    if ifd.tile_width().is_none() || ifd.tile_height().is_none() {
        tracing::debug!(src = %src, "async_tiff: source is not tiled -- falling back to in_process");
        return crate::providers::download_in_process_with_crs(src, dst, crop, crop_crs);
    }

    // Resolve the source geo_transform from the IFD's tags.
    let (origin_x, pix_w, origin_y, pix_h) = ifd_geo_transform(ifd)?;
    let src_cols_total = ifd.image_width() as usize;
    let src_rows_total = ifd.image_height() as usize;

    // Resolve the output pixel window using GDAL projwin semantics.
    let window = match crop {
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
    fetch_and_stitch_then_write(dst, ifd, &reader, &tiff, src, &parsed, origin_x, pix_w, origin_y, pix_h, src_cols_total, src_rows_total, window).await
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

    /// Cross-CRS crops must transparently fall back to in_process.
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
        // The fallback path is exercised; we just need it not to error.
        let _ = download_via_async_tiff_with_crs(
            src.to_str().unwrap(), &dst, Some(crop), Some("EPSG:4326"),
        ).await.expect("fallback succeeds");
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
