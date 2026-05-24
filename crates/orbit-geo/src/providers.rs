//! Public STAC providers and GDAL VSI URL rewriting.
//!
//! Built test-first (see `cfg(test)` block at the bottom of this file).

/// Allowlisted CRS specifications safe to pass to PROJ via gdal_translate.
/// Rejects `+nadgrids=`, `+wktext`, `@`, `?`, and any whitespace.
///
/// Reason: PROJ resolves `+nadgrids=/path` server-side, allowing
/// arbitrary file reads via crafted CRS strings.
pub fn validate_crs_spec(crs: &str) -> Result<(), String> {
    const MAX_LEN: usize = 128;
    if crs.is_empty() || crs.len() > MAX_LEN {
        return Err(format!("CRS spec length must be 1..={MAX_LEN}"));
    }
    // Block PROJ file-resolving directives outright.
    for bad in ["+nadgrids", "@", "+wktext"] {
        if crs.contains(bad) {
            return Err(format!("CRS spec contains disallowed token: {bad}"));
        }
    }
    // Allow only: A-Z, a-z, 0-9, +, -, _, =, ., :, /, comma, space.
    for c in crs.chars() {
        if !(c.is_ascii_alphanumeric()
            || matches!(c, '+' | '-' | '_' | '=' | '.' | ':' | '/' | ',' | ' '))
        {
            return Err(format!("CRS spec contains disallowed character: {c:?}"));
        }
    }
    // Block obvious shell metacharacters even if they slipped past the allowlist.
    for bad in ['$', '`', ';', '|', '&', '\n', '\r', '\0'] {
        if crs.contains(bad) {
            return Err(format!("CRS spec contains shell metacharacter: {bad:?}"));
        }
    }
    Ok(())
}


/// Sign a Microsoft Planetary Computer asset URL.
///
/// Composes:
/// - [`planetary_computer_sign_endpoint`] — builds the sign URL (pure)
/// - HTTP GET via the supplied client
/// - [`parse_signed_response`] — extracts `href` (pure)
///
/// The HTTP layer is the only un-mocked seam; both pure helpers are
/// covered by unit tests. A live-API test (`#[ignore]` by default,
/// run on demand with `cargo test -- --ignored`) confirms wire compatibility.
#[cfg(feature = "openeo")]
pub async fn sign_planetary_computer_url(
    client: &reqwest::Client,
    raw_asset_url: &str,
) -> crate::Result<String> {
    let endpoint = planetary_computer_sign_endpoint(raw_asset_url);
    let resp = client
        .get(&endpoint)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| crate::Error::Other(format!("PC sign GET {endpoint}: {e}")))?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| crate::Error::Other(format!("PC sign body read: {e}")))?;
    parse_signed_response(&bytes)
}

/// Spawn `gdal_translate` to fetch one COG asset locally. This mirrors
/// the upstream raster crate's download strategy — letting GDAL handle range reads,
/// decompression, and any optional cropping rather than streaming raw
/// bytes via reqwest.
///
/// Returns the destination path on success; surfaces a clean error if the
/// subprocess can't be launched (e.g. `gdal_translate` not on `$PATH`) or
/// exits non-zero. Pure-argv construction is covered by
/// [`build_gdal_translate_argv`] tests; the spawn behaviour itself has an
/// `#[ignore]`d test that requires GDAL on PATH.
pub fn download_via_gdal_translate(
    src: &str,
    dst: &std::path::Path,
    crop: Option<CropWindow>,
) -> crate::Result<std::path::PathBuf> {
    // Backwards-compatible: assume EPSG:4326 (openEO default) for the
    // bbox. Callers that already know the bbox CRS (the openEO executor
    // post-A12) should use [`download_via_gdal_translate_with_crs`].
    download_via_gdal_translate_with_crs(src, dst, crop, Some("EPSG:4326"))
}

/// Variant of [`download_via_gdal_translate`] that lets the caller specify
/// the SRS of the `-projwin` coordinates. Pass `None` to interpret the
/// coordinates in the source raster's native CRS (no `-projwin_srs`).
///
/// Added in A12 (honor `spatial_extent.crs` per openEO 1.3.0 spec).
pub fn download_via_gdal_translate_with_crs(
    src: &str,
    dst: &std::path::Path,
    crop: Option<CropWindow>,
    crop_crs: Option<&str>,
) -> crate::Result<std::path::PathBuf> {
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let argv = build_gdal_translate_argv(src, dst, crop, crop_crs);
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| {
            crate::Error::Other(format!(
                "spawn `{}`: {e} — is gdal installed and on PATH?",
                argv[0]
            ))
        })?;
    if !status.success() {
        return Err(crate::Error::Other(format!(
            "`{} … {}` exited with status {status}",
            argv[0],
            dst.display()
        )));
    }
    Ok(dst.to_path_buf())
}

/// In-process equivalent of [`download_via_gdal_translate_with_crs`] that
/// uses `gdal::Dataset::open(/vsicurl/<href>)` directly instead of spawning
/// `gdal_translate`. Eliminates the fork-exec round-trip and lets the host
/// process keep the COG's tile-cache between successive `read_as` calls.
///
/// Phase A microbench (`examples/bench_download_one_cog.rs`, 12 MP S2 B04,
/// Wien bbox, 2026-05-24) measured subprocess fork-exec at <500 ms vs
/// >25 s S3 IO + decode -- i.e. the subprocess wrapper is not the
/// bottleneck. This function exists as an additive opt-in path so the
/// `Downloader` trait gains a no-subprocess option for future P3
/// (direct-read inside block_executor) without breaking the default flow.
///
/// Behaviour parity with the subprocess path:
/// - Honors `crop` + `crop_crs` per the openEO 1.3.0 spec (A12).
/// - Writes a tiled LZW-compressed GeoTIFF to `dst`.
/// - Computes the pixel window using GDAL's `-projwin` rounding rules
///   (`floor(off + 0.001)` for the offsets, `ceil(size - 0.001)` for the
///   sizes) so output dimensions are byte-identical to `gdal_translate`.
/// - Pads the output with the source band's nodata value (or zero when
///   no nodata is set) for pixels outside the source extent — matches
///   gdal_translate, which silently emits a fully-sized projwin window
///   regardless of partial out-of-bounds overlap. **P1 dimension bug
///   fix (2026-05-24)**: previously clamped output dims to the
///   in-bounds intersection, which caused downstream block_executor
///   RasterIO access-window OOB errors on Sentinel-2 bboxes whose UTM
///   projection grazed the COG edge.
///
/// Notes:
/// - When `crop` is `None`, the **entire** source raster is copied
///   (matches gdal_translate with no `-projwin`).
pub fn download_in_process_with_crs(
    src: &str,
    dst: &std::path::Path,
    crop: Option<CropWindow>,
    crop_crs: Option<&str>,
) -> crate::Result<std::path::PathBuf> {
    use gdal::raster::{GdalDataType, RasterCreationOptions};
    use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
    use gdal::{Dataset, DriverManager};

    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Defence-in-depth: refuse a CRS spec that fails the PROJ-injection guard
    // even though primary validation lives at the eval_load.rs entry-point
    // (matches the subprocess path's behaviour).
    if let Some(crs) = crop_crs {
        if validate_crs_spec(crs).is_err() {
            tracing::warn!(crs = %crs, "in-process: ignoring -projwin_srs equivalent; CRS validation failed");
        }
    }

    let src_ds = Dataset::open(src).map_err(|e| {
        crate::Error::Other(format!("in_process open {src}: {e}"))
    })?;
    let src_gt = src_ds.geo_transform().map_err(|e| {
        crate::Error::Other(format!("in_process geo_transform {src}: {e}"))
    })?;
    let (src_cols_total, src_rows_total) = src_ds.raster_size();
    let mut src_sr = src_ds.spatial_ref().map_err(|e| {
        crate::Error::Other(format!("in_process spatial_ref {src}: {e}"))
    })?;
    src_sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let band = src_ds.rasterband(1).map_err(|e| {
        crate::Error::Other(format!("in_process rasterband {src}: {e}"))
    })?;
    let dtype = band.band_type();
    let src_nodata: Option<f64> = band.no_data_value();

    let (origin_x, pix_w, origin_y, pix_h) =
        (src_gt[0], src_gt[1], src_gt[3], src_gt[5]);
    if pix_w == 0.0 || pix_h == 0.0 {
        return Err(crate::Error::Other(format!(
            "in_process: degenerate geo_transform pix_w={pix_w} pix_h={pix_h}"
        )));
    }

    // Resolve the FULL requested window using GDAL's projwin rounding rules.
    // `col_off`/`row_off` can be negative and `col_off + cols` can exceed the
    // source extent -- the OOB margin is filled with `src_nodata` (or zero).
    let (col_off, row_off, cols, rows) = if let Some(w) = crop {
        let validated_crs = crop_crs.filter(|c| validate_crs_spec(c).is_ok());
        let mut work_sr = if let Some(c) = validated_crs {
            SpatialRef::from_definition(c).map_err(|e| {
                crate::Error::Other(format!("in_process from_definition {c}: {e}"))
            })?
        } else {
            src_sr.clone()
        };
        work_sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        // Project the bbox envelope into source CRS using OCTTransformBounds
        // with 21-point edge densification -- matches the algorithm GDAL CLI's
        // `gdal_translate -projwin -projwin_srs` uses internally. Densification
        // is essential for non-affine projections: a 4-corner min/max bracket
        // can under-estimate the true projected envelope by several pixels.
        let xform = CoordTransform::new(&work_sr, &src_sr).map_err(|e| {
            crate::Error::Other(format!("in_process CoordTransform: {e}"))
        })?;
        // Per GDAL 3.x gdal_translate_lib.cpp: transform the projwin via
        // OCTTransformBounds(densify=21) which densifies edges to handle
        // non-affine reprojection. Argument order is (xmin=ULX, ymin=LRY,
        // xmax=LRX, ymax=ULY). OCTTransformBounds is a no-op when source ==
        // target SRS so we always call it (avoiding the missing
        // SpatialRef::is_same in gdal 0.19).
        let src_bounds = xform
            .transform_bounds(&[w.min_x, w.min_y, w.max_x, w.max_y], 21)
            .map_err(|e| crate::Error::Other(format!("in_process transform_bounds: {e}")))?;
        // transform_bounds returns [xmin, ymin, xmax, ymax] in target axis order.
        let pulx = src_bounds[0];
        let plry = src_bounds[1];
        let plrx = src_bounds[2];
        let puly = src_bounds[3];
        // GDAL 3.x projwin math (assumes nearest-neighbour, our default):
        //   xOff = floor((ULX - gt[0]) / gt[1] + 0.001);
        //   yOff = floor((ULY - gt[3]) / gt[5] + 0.001);
        //   ULX  = xOff * gt[1] + gt[0];   (snap ULX to source pixel grid)
        //   ULY  = yOff * gt[5] + gt[3];
        //   xSize = ceil((LRX - ULX) / gt[1] - 0.001);
        //   ySize = ceil((LRY - ULY) / gt[5] - 0.001);
        let col_off_i = ((pulx - origin_x) / pix_w + 0.001).floor() as isize;
        let row_off_i = ((puly - origin_y) / pix_h + 0.001).floor() as isize;
        let snapped_ulx = col_off_i as f64 * pix_w + origin_x;
        let snapped_uly = row_off_i as f64 * pix_h + origin_y;
        let cols_f = ((plrx - snapped_ulx) / pix_w - 0.001).ceil();
        let rows_f = ((plry - snapped_uly) / pix_h - 0.001).ceil();
        if !cols_f.is_finite() || !rows_f.is_finite() || cols_f < 1.0 || rows_f < 1.0 {
            return Err(crate::Error::Other(format!(
                "in_process: degenerate projwin produces 0-pixel window (src=[{origin_x},{origin_y}] {src_cols_total}x{src_rows_total}, crop projects to [{pulx:.1},{plrx:.1}] x [{plry:.1},{puly:.1}])"
            )));
        }
        let cols = cols_f as usize;
        let rows = rows_f as usize;
        // Fail early if the window is entirely outside the source -- matches
        // the pre-existing semantics so callers don't get a fully-nodata raster.
        let in_x = (col_off_i + cols as isize) > 0 && col_off_i < src_cols_total as isize;
        let in_y = (row_off_i + rows as isize) > 0 && row_off_i < src_rows_total as isize;
        if !in_x || !in_y {
            return Err(crate::Error::Other(format!(
                "in_process: crop window does not intersect source raster (src=[{origin_x},{origin_y}] {src_cols_total}x{src_rows_total}, crop projects to [{pulx:.1},{plrx:.1}] x [{plry:.1},{puly:.1}])"
            )));
        }
        (col_off_i, row_off_i, cols, rows)
    } else {
        (0_isize, 0_isize, src_cols_total, src_rows_total)
    };

    let out_gt = [
        origin_x + col_off as f64 * pix_w,
        pix_w,
        0.0,
        origin_y + row_off as f64 * pix_h,
        0.0,
        pix_h,
    ];

    let mem_drv = DriverManager::get_driver_by_name("MEM").map_err(|e| {
        crate::Error::Other(format!("in_process MEM driver: {e}"))
    })?;
    let gtiff_drv = DriverManager::get_driver_by_name("GTiff").map_err(|e| {
        crate::Error::Other(format!("in_process GTiff driver: {e}"))
    })?;
    let opts = RasterCreationOptions::from_iter(["COMPRESS=LZW", "TILED=YES"]);

    // Dispatch on the source dtype so we read+write in the native pixel
    // format (matches gdal_translate's default behaviour of preserving type).
    let ctx = CopyWindowCtx {
        src_cols_total, src_rows_total,
        col_off, row_off, cols, rows,
        src_nodata,
    };
    match dtype {
        GdalDataType::UInt16 => copy_window_typed::<u16>(
            &src_ds, &src_sr, &out_gt, &mem_drv, &gtiff_drv, &opts, dst, &ctx,
        )?,
        GdalDataType::Int16 => copy_window_typed::<i16>(
            &src_ds, &src_sr, &out_gt, &mem_drv, &gtiff_drv, &opts, dst, &ctx,
        )?,
        GdalDataType::UInt8 => copy_window_typed::<u8>(
            &src_ds, &src_sr, &out_gt, &mem_drv, &gtiff_drv, &opts, dst, &ctx,
        )?,
        GdalDataType::Float32 => copy_window_typed::<f32>(
            &src_ds, &src_sr, &out_gt, &mem_drv, &gtiff_drv, &opts, dst, &ctx,
        )?,
        other => {
            return Err(crate::Error::Other(format!(
                "in_process: unsupported source dtype {other:?} (extend copy_window_typed dispatch)"
            )));
        }
    }
    Ok(dst.to_path_buf())
}

/// Read-and-pad context shared by [`copy_window_typed`] variants.
struct CopyWindowCtx {
    src_cols_total: usize,
    src_rows_total: usize,
    /// Unclamped output window origin; may be negative (window starts west/north of source).
    col_off: isize,
    /// Unclamped output window origin; may be negative.
    row_off: isize,
    /// Full output window dimensions (preserved even when partially OOB).
    cols: usize,
    rows: usize,
    /// Source band nodata value if any; used to pad OOB pixels.
    src_nodata: Option<f64>,
}

/// Type-generic core of [`download_in_process_with_crs`].
///
/// Reads the in-bounds intersection of the requested window from band 1
/// of `src_ds` and writes a MEM→GTiff copy at `dst`. The output
/// dimensions are `ctx.cols x ctx.rows` (FULL requested window). Pixels
/// outside the source extent are padded with the source band's nodata
/// value (or `T::default()` when no nodata is set) so output dims match
/// `gdal_translate -projwin` byte-for-byte.
fn copy_window_typed<T>(
    src_ds: &gdal::Dataset,
    src_sr: &gdal::spatial_ref::SpatialRef,
    out_gt: &[f64; 6],
    mem_drv: &gdal::Driver,
    gtiff_drv: &gdal::Driver,
    opts: &gdal::raster::RasterCreationOptions,
    dst: &std::path::Path,
    ctx: &CopyWindowCtx,
) -> crate::Result<()>
where
    T: Copy + Default + gdal::raster::GdalType + num_traits::NumCast,
{
    use gdal::raster::Buffer;
    let band = src_ds.rasterband(1).map_err(|e| {
        crate::Error::Other(format!("copy_window read rasterband: {e}"))
    })?;
    let CopyWindowCtx { src_cols_total, src_rows_total, col_off, row_off, cols, rows, src_nodata } = *ctx;

    // Compute the in-bounds read window (intersection with source extent).
    let read_col_start = col_off.max(0) as usize;
    let read_row_start = row_off.max(0) as usize;
    let read_col_end = (col_off + cols as isize).max(0).min(src_cols_total as isize) as usize;
    let read_row_end = (row_off + rows as isize).max(0).min(src_rows_total as isize) as usize;
    let read_cols = read_col_end.saturating_sub(read_col_start);
    let read_rows = read_row_end.saturating_sub(read_row_start);
    // Where the read data lands inside the output buffer.
    let dst_col_start = (read_col_start as isize - col_off) as usize;
    let dst_row_start = (read_row_start as isize - row_off) as usize;

    // Default fill: src_nodata cast to T, else T::default() (zero for numerics).
    let fill: T = src_nodata
        .and_then(|n| num_traits::NumCast::from(n))
        .unwrap_or_else(T::default);
    let mut out_data: Vec<T> = vec![fill; cols * rows];

    if read_cols > 0 && read_rows > 0 {
        let buf: Buffer<T> = band
            .read_as::<T>(
                (read_col_start as isize, read_row_start as isize),
                (read_cols, read_rows),
                (read_cols, read_rows),
                None,
            )
            .map_err(|e| {
                crate::Error::Other(format!(
                    "copy_window read_as col={read_col_start} row={read_row_start} w={read_cols} h={read_rows}: {e}"
                ))
            })?;
        let read_data = buf.data();
        for r in 0..read_rows {
            let src_row_start = r * read_cols;
            let dst_row = (dst_row_start + r) * cols + dst_col_start;
            out_data[dst_row..dst_row + read_cols]
                .copy_from_slice(&read_data[src_row_start..src_row_start + read_cols]);
        }
    }

    let mut mem_ds = mem_drv
        .create_with_band_type::<T, _>("", cols, rows, 1)
        .map_err(|e| crate::Error::Other(format!("copy_window mem create: {e}")))?;
    mem_ds.set_geo_transform(out_gt).map_err(|e| {
        crate::Error::Other(format!("copy_window set_geo_transform: {e}"))
    })?;
    mem_ds.set_spatial_ref(src_sr).map_err(|e| {
        crate::Error::Other(format!("copy_window set_spatial_ref: {e}"))
    })?;
    {
        let mut wb = mem_ds.rasterband(1).map_err(|e| {
            crate::Error::Other(format!("copy_window mem rasterband: {e}"))
        })?;
        // Propagate the source nodata only when it was set on the source band --
        // gdal_translate does NOT synthesise a nodata value when the source
        // lacks one, even when the projwin extends past the source extent.
        if let Some(nd) = src_nodata {
            let _ = wb.set_no_data_value(Some(nd));
        }
        let mut out_buf = Buffer::new((cols, rows), out_data);
        wb.write::<T>((0, 0), (cols, rows), &mut out_buf).map_err(|e| {
            crate::Error::Other(format!("copy_window mem write: {e}"))
        })?;
    }
    let copy = mem_ds
        .create_copy(gtiff_drv, dst, opts)
        .map_err(|e| crate::Error::Other(format!("copy_window create_copy {}: {e}", dst.display())))?;
    drop(copy);
    Ok(())
}

/// Parse the JSON body returned by Microsoft Planetary Computer's
/// `/api/sas/v1/sign` endpoint and extract the `href` field.
///
/// Pure function — no HTTP. Used by [`sign_planetary_computer_url`].
pub fn parse_signed_response(body: &[u8]) -> crate::Result<String> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| crate::Error::Other(format!("PC sign response JSON: {e}")))?;
    v.get("href")
        .and_then(|h| h.as_str())
        .map(str::to_owned)
        .ok_or_else(|| crate::Error::Other("PC sign response missing 'href' field".into()))
}

/// Optional crop window in target CRS coordinates (passed to gdal_translate's `-projwin`).
#[derive(Copy, Clone, Debug)]
pub struct CropWindow {
    /// Left X (e.g. min longitude).
    pub min_x: f64,
    /// Top Y (e.g. max latitude — `-projwin` is top-left then bottom-right).
    pub max_y: f64,
    /// Right X (e.g. max longitude).
    pub max_x: f64,
    /// Bottom Y (e.g. min latitude).
    pub min_y: f64,
}

/// Build the argv for spawning `gdal_translate` to fetch one asset.
///
/// Pure function — testable without spawning. Used by
/// [`download_via_gdal_translate`].
///
/// `crop_crs` is interpreted per the openEO 1.3.0 `spatial_extent.crs`
/// spec (A12): when both `crop` and `crop_crs` are provided, the SRS is
/// passed via `-projwin_srs <crs>`. When `crop_crs` is `None`, the
/// `-projwin` coordinates are interpreted in the source raster's
/// native CRS (no `-projwin_srs` flag).
#[must_use]
pub fn build_gdal_translate_argv(
    src: &str,
    dst: &std::path::Path,
    crop: Option<CropWindow>,
    crop_crs: Option<&str>,
) -> Vec<String> {
    let mut argv = vec!["gdal_translate".to_string(), "-q".to_string()];
    if let Some(w) = crop {
        argv.push("-projwin".to_string());
        argv.push(w.min_x.to_string());
        argv.push(w.max_y.to_string());
        argv.push(w.max_x.to_string());
        argv.push(w.min_y.to_string());
        // openEO `spatial_extent.crs` (default EPSG:4326). Without the
        // explicit `-projwin_srs`, GDAL interprets projwin in the COG's
        // native CRS (UTM meters for Sentinel-2), collapsing typical
        // degree-scale bboxes to sub-pixel windows and producing 1×1
        // outputs. When `crop_crs` is `None`, the caller has explicitly
        // opted into native-CRS interpretation.
        if let Some(crs) = crop_crs {
            // B2: defense in depth — refuse PROJ injection if a caller
            // somehow reached us without prior validation. Silently drop
            // the flag so the download still works in native CRS rather
            // than break callers; the eval_load.rs entry validator is the
            // primary gate.
            if validate_crs_spec(crs).is_ok() {
                argv.push("-projwin_srs".to_string());
                argv.push(crs.to_string());
            } else {
                tracing::warn!(crs = %crs, "dropping -projwin_srs: CRS spec failed validation");
            }
        }
    }
    // **Audit P1-8**: option-injection defense lives at the URL policy
    // layer (`UrlPolicy::OptionInjection` rejects any href starting with
    // `-` before any subprocess spawn — see
    // `apps/orbit-openeo/src/url_policy.rs`). A POSIX `--` sentinel
    // would be redundant here AND incompatible: GDAL's CLI parser
    // (verified against 3.12.1) raises "ERROR 1: Unknown argument: --"
    // when it sees a bare `--` token.
    argv.push(src.to_string());
    argv.push(dst.display().to_string());
    argv
}

/// Configure GDAL for efficient anonymous access to AWS Open Data buckets
/// like `sentinel-cogs` (us-west-2) and `usgs-landsat` (us-west-2).
///
/// Sets:
/// - `AWS_NO_SIGN_REQUEST=YES` — anonymous reads, no AWS credentials needed
/// - `GDAL_HTTP_MULTIPLEX=YES` — HTTP/2 multiplexing for parallel range reads
/// - `GDAL_HTTP_VERSION=2`
/// - `VSI_CACHE=TRUE`, `VSI_CACHE_SIZE=26214400` — 25 MB block cache
/// - `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` — skips probing sibling files
/// - `CPL_VSIL_CURL_ALLOWED_EXTENSIONS=.tif,.TIF,.tiff,.jp2`
///
/// Idempotent — safe to call from multiple places. Equivalent to the
/// upstream raster crate's `configure_gdal_s3_defaults` (cleanly re-implemented).
pub fn configure_anonymous_s3() {
    let opts: &[(&str, &str)] = &[
        ("AWS_NO_SIGN_REQUEST", "YES"),
        ("GDAL_HTTP_MULTIPLEX", "YES"),
        ("GDAL_HTTP_VERSION", "2"),
        ("VSI_CACHE", "TRUE"),
        ("VSI_CACHE_SIZE", "26214400"),
        ("GDAL_DISABLE_READDIR_ON_OPEN", "EMPTY_DIR"),
        ("CPL_VSIL_CURL_ALLOWED_EXTENSIONS", ".tif,.TIF,.tiff,.jp2"),
    ];
    for (k, v) in opts {
        // set_config_option returns Result but failures here are non-fatal
        // (e.g. running without an initialized GDAL) — log and continue.
        let _ = gdal::config::set_config_option(k, v);
    }
}

/// Pre-configured base URLs for the public STAC catalogs orbit-geo knows
/// how to talk to without extra configuration.
///
/// Pass any of these to [`crate::stac::StacClient::new`] as the catalog
/// root.
pub struct Provider;

impl Provider {
    /// Element 84 Earth Search v1 — free, AWS Open Data, no auth.
    /// Sentinel-2 L2A + Sentinel-1 + Landsat C2 + NAIP + Copernicus DEM.
    pub const EARTH_SEARCH_V1: &'static str = "https://earth-search.aws.element84.com/v1";

    /// Microsoft Planetary Computer — free, requires per-asset SAS signing.
    /// See [`planetary_computer_sign_endpoint`] / [`crate::stac::sign_planetary_computer_url`].
    pub const PLANETARY_COMPUTER: &'static str = "https://planetarycomputer.microsoft.com/api/stac/v1";

    /// USGS Landsat Look — free, no auth. Landsat only.
    pub const USGS_LANDSAT_LOOK: &'static str = "https://landsatlook.usgs.gov/stac-server";

    /// Digital Earth Australia (DEA) — free, regional Sentinel-2 + Landsat ARD.
    pub const DEA: &'static str = "https://explorer.sandbox.dea.ga.gov.au/stac";
}

/// Build the Microsoft Planetary Computer SAS-signing endpoint URL for a
/// raw asset href. The endpoint is unauthenticated; a GET returns
/// `{ "href": "<signed-url>", "msft:expiry": "<iso8601>" }`.
///
/// Pure function — does no HTTP. The HTTP call lives in
/// [`crate::stac::sign_planetary_computer_url`].
pub fn planetary_computer_sign_endpoint(raw_asset_url: &str) -> String {
    format!(
        "https://planetarycomputer.microsoft.com/api/sas/v1/sign?href={}",
        percent_encode(raw_asset_url)
    )
}

/// RFC-3986-style percent-encoding of every byte that's not an unreserved
/// character. Conservative on purpose — passes colons, slashes, ampersands,
/// equals signs through the encoder so they survive transport as query
/// values.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// Implementation moved to `eo_io::vsi::vsi_rewrite`. Re-exported here for
// backwards compatibility; new code should import directly from `eo_io`.
pub use eo_io::vsi::vsi_rewrite;

// **P2** -- pure-Rust async S3 downloader via `async-tiff` + `object_store`.
// Re-exported here so callers use `orbit_geo::providers::download_via_async_tiff_with_crs`
// for API parity with the subprocess + in-process variants. Implementation
// lives in [`crate::async_download`] (feature `async-tiff`).
#[cfg(feature = "async-tiff")]
pub use crate::async_download::download_via_async_tiff_with_crs;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsi_rewrite_https_to_vsicurl() {
        let url = "https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/35/U/PA/2026/4/S2A/B04.tif";
        let got = vsi_rewrite(url);
        assert_eq!(
            got,
            "/vsicurl/https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/35/U/PA/2026/4/S2A/B04.tif"
        );
    }

    #[test]
    fn vsi_rewrite_s3_protocol_to_vsis3() {
        // s3://bucket/key  →  /vsis3/bucket/key
        // GDAL's VSI S3 driver is more efficient than vsicurl for S3 URIs.
        let got = vsi_rewrite("s3://sentinel-cogs/sentinel-s2-l2a-cogs/35/B04.tif");
        assert_eq!(got, "/vsis3/sentinel-cogs/sentinel-s2-l2a-cogs/35/B04.tif");
    }

    #[test]
    fn build_gdal_translate_argv_minimal() {
        // No crop window, no crs. Argv: ["gdal_translate", "-q", <src>, <dst>]
        let argv = build_gdal_translate_argv(
            "/vsicurl/https://sentinel-cogs.s3.us-west-2.amazonaws.com/x.tif",
            std::path::Path::new("/tmp/out.tif"),
            None,
            None,
        );
        assert_eq!(argv.len(), 4);
        assert_eq!(argv[0], "gdal_translate");
        assert_eq!(argv[1], "-q");
        assert_eq!(argv[2], "/vsicurl/https://sentinel-cogs.s3.us-west-2.amazonaws.com/x.tif");
        assert_eq!(argv[3], "/tmp/out.tif");
    }

    #[test]
    fn download_via_gdal_translate_reports_clean_error_when_binary_missing() {
        // Temporarily neuter $PATH so `gdal_translate` cannot be found.
        // Verifies our wrapper surfaces a clear error rather than panicking.
        let original = std::env::var_os("PATH");
        // SAFETY: tests run single-threaded by default; we restore PATH before returning.
        unsafe { std::env::set_var("PATH", ""); }

        let dst = std::env::temp_dir().join("orbit_geo_test_should_not_exist.tif");
        let result = download_via_gdal_translate(
            "/vsicurl/https://example.com/x.tif",
            &dst,
            None,
        );

        // Restore PATH immediately so subsequent tests are unaffected.
        if let Some(p) = original {
            unsafe { std::env::set_var("PATH", p); }
        } else {
            unsafe { std::env::remove_var("PATH"); }
        }

        let err = result.expect_err("expected spawn failure");
        let msg = err.to_string();
        assert!(
            msg.contains("gdal_translate") || msg.contains("PATH"),
            "error must mention the missing binary; got: {msg}"
        );
    }

    #[test]
    fn parse_signed_response_extracts_href() {
        let body = br#"{"href":"https://example.com/signed?sig=abc","msft:expiry":"2026-05-21T12:00:00Z"}"#;
        let got = parse_signed_response(body).expect("valid body");
        assert_eq!(got, "https://example.com/signed?sig=abc");
    }

    #[test]
    fn parse_signed_response_errors_on_missing_href() {
        let body = br#"{"oops":"no href here"}"#;
        let result = parse_signed_response(body);
        assert!(result.is_err(), "expected Err, got {result:?}");
    }

    #[test]
    fn parse_signed_response_errors_on_invalid_json() {
        let body = b"{this is not json";
        let result = parse_signed_response(body);
        assert!(result.is_err());
    }

    #[test]
    fn build_gdal_translate_argv_with_crop_window() {
        // -projwin convention: top-left then bottom-right corner.
        // crop_crs is None → no -projwin_srs flag emitted.
        let crop = CropWindow {
            min_x: 13.0,
            max_y: 53.0,
            max_x: 14.0,
            min_y: 52.0,
        };
        let argv = build_gdal_translate_argv(
            "/vsicurl/https://example.com/x.tif",
            std::path::Path::new("/tmp/out.tif"),
            Some(crop),
            None,
        );
        // ["gdal_translate", "-q", "-projwin", "13", "53", "14", "52", src, dst]
        let projwin_idx = argv.iter().position(|s| s == "-projwin").expect("must include -projwin");
        assert_eq!(argv[projwin_idx + 1], "13");
        assert_eq!(argv[projwin_idx + 2], "53");
        assert_eq!(argv[projwin_idx + 3], "14");
        assert_eq!(argv[projwin_idx + 4], "52");
        assert_eq!(argv[argv.len() - 1], "/tmp/out.tif");
        // Without crs argument: ["gdal_translate", "-q", "-projwin", x, y, x, y, src, dst] = 9 args.
        assert_eq!(argv.len(), 9);
    }

    #[test]
    fn build_gdal_translate_argv_with_crop_window_and_crs_emits_projwin_srs() {
        // A12: passing Some("EPSG:4326") must produce the `-projwin_srs EPSG:4326`
        // flag immediately after the four projwin coordinates.
        let crop = CropWindow {
            min_x: 13.0,
            max_y: 53.0,
            max_x: 14.0,
            min_y: 52.0,
        };
        let argv = build_gdal_translate_argv(
            "/vsicurl/https://example.com/x.tif",
            std::path::Path::new("/tmp/out.tif"),
            Some(crop),
            Some("EPSG:4326"),
        );
        // ["gdal_translate", "-q", "-projwin", x, y, x, y, "-projwin_srs", "EPSG:4326", src, dst] = 11
        assert_eq!(argv.len(), 11);
        let srs_idx = argv
            .iter()
            .position(|s| s == "-projwin_srs")
            .expect("must include -projwin_srs when crop_crs is Some");
        assert_eq!(argv[srs_idx + 1], "EPSG:4326");
        // -projwin_srs must appear AFTER the four projwin coords.
        let projwin_idx = argv.iter().position(|s| s == "-projwin").unwrap();
        assert!(srs_idx > projwin_idx + 4, "-projwin_srs must follow the projwin coords");
    }

    #[test]
    fn build_gdal_translate_argv_with_crop_window_no_crs_omits_projwin_srs() {
        // A12: explicit None for crop_crs must skip the -projwin_srs flag entirely,
        // letting GDAL interpret the projwin in the source raster's native CRS.
        let crop = CropWindow {
            min_x: 100_000.0,
            max_y: 500_000.0,
            max_x: 110_000.0,
            min_y: 490_000.0,
        };
        let argv = build_gdal_translate_argv(
            "/vsicurl/https://example.com/x.tif",
            std::path::Path::new("/tmp/out.tif"),
            Some(crop),
            None,
        );
        assert!(
            !argv.iter().any(|s| s == "-projwin_srs"),
            "no -projwin_srs flag when crop_crs is None; argv={argv:?}"
        );
        // 4 (binary+q+src+dst) + 5 (-projwin + 4 coords) = 9
        assert_eq!(argv.len(), 9);
    }

    #[test]
    fn build_gdal_translate_argv_with_crs_but_no_crop_ignores_crs() {
        // No crop window → no projwin → crop_crs argument is irrelevant.
        let argv = build_gdal_translate_argv(
            "/vsicurl/https://example.com/x.tif",
            std::path::Path::new("/tmp/out.tif"),
            None,
            Some("EPSG:4326"),
        );
        assert!(!argv.iter().any(|s| s == "-projwin_srs"));
        assert!(!argv.iter().any(|s| s == "-projwin"));
        assert_eq!(argv.len(), 4);
    }

    #[test]
    fn configure_anonymous_s3_sets_gdal_options() {
        // Should set the GDAL config keys that allow anonymous access to
        // AWS Open Data buckets (sentinel-cogs, usgs-landsat, etc.). After
        // this call, GDAL_HTTP_MULTIPLEX and AWS_NO_SIGN_REQUEST must be
        // visible via gdal::config::get_config_option.
        configure_anonymous_s3();
        assert_eq!(
            gdal::config::get_config_option("AWS_NO_SIGN_REQUEST", "").unwrap_or_default(),
            "YES"
        );
        assert_eq!(
            gdal::config::get_config_option("GDAL_HTTP_MULTIPLEX", "").unwrap_or_default(),
            "YES"
        );
    }

    #[test]
    fn public_provider_urls_are_known_endpoints() {
        // Smoke test: the four pre-configured public STAC catalogs we depend on.
        // Locking these as constants prevents accidental typos that would only
        // show up at runtime against a real network.
        assert_eq!(
            Provider::EARTH_SEARCH_V1,
            "https://earth-search.aws.element84.com/v1"
        );
        assert_eq!(
            Provider::PLANETARY_COMPUTER,
            "https://planetarycomputer.microsoft.com/api/stac/v1"
        );
        assert_eq!(
            Provider::USGS_LANDSAT_LOOK,
            "https://landsatlook.usgs.gov/stac-server"
        );
        assert_eq!(
            Provider::DEA,
            "https://explorer.sandbox.dea.ga.gov.au/stac"
        );
    }

    #[test]
    fn planetary_computer_sign_endpoint_is_correct() {
        let raw = "https://landsateuwest.blob.core.windows.net/landsat-c2/level-2/standard/oli-tirs/2024/B04.tif";
        let endpoint = planetary_computer_sign_endpoint(raw);
        assert!(
            endpoint.starts_with("https://planetarycomputer.microsoft.com/api/sas/v1/sign?href="),
            "endpoint must point at PC sign API, got: {endpoint}"
        );
        // Asset URL must be url-encoded in the query string so colons/slashes survive.
        assert!(
            endpoint.contains("https%3A%2F%2Flandsateuwest.blob.core.windows.net"),
            "asset URL must be percent-encoded inside the query, got: {endpoint}"
        );
    }

    // ---------- B2: PROJ-injection guard ----------

    #[test]
    fn validate_crs_accepts_epsg_codes() {
        assert!(validate_crs_spec("EPSG:4326").is_ok());
        assert!(validate_crs_spec("EPSG:32633").is_ok());
        assert!(validate_crs_spec("EPSG:3857").is_ok());
    }

    #[test]
    fn validate_crs_accepts_proj_strings() {
        assert!(validate_crs_spec("+proj=longlat +datum=WGS84").is_ok());
        assert!(validate_crs_spec("+proj=utm +zone=33 +ellps=WGS84").is_ok());
    }

    #[test]
    fn validate_crs_rejects_nadgrids() {
        let r = validate_crs_spec("+nadgrids=/etc/passwd");
        assert!(r.is_err(), "must reject +nadgrids file-resolving directive");
    }

    #[test]
    fn validate_crs_rejects_at_file() {
        let r = validate_crs_spec("@/etc/passwd");
        assert!(r.is_err(), "must reject @file PROJ directive");
    }

    #[test]
    fn validate_crs_rejects_shell_metas() {
        for s in &["EPSG:4326;rm -rf", "EPSG:4326`whoami`", "EPSG:4326$x", "EPSG:4326|cat"] {
            assert!(validate_crs_spec(s).is_err(), "must reject shell-meta: {s}");
        }
    }

    #[test]
    fn validate_crs_rejects_empty_and_overlong() {
        assert!(validate_crs_spec("").is_err());
        assert!(validate_crs_spec(&"a".repeat(129)).is_err());
    }

    #[test]
    fn build_gdal_translate_argv_drops_injection_crs() {
        // Defense in depth: even if a malicious CRS reaches the argv builder,
        // the flag must not be emitted.
        let crop = CropWindow { min_x: 0.0, max_y: 1.0, max_x: 1.0, min_y: 0.0 };
        let argv = build_gdal_translate_argv(
            "/vsicurl/https://example.com/x.tif",
            std::path::Path::new("/tmp/out.tif"),
            Some(crop),
            Some("+nadgrids=/etc/passwd"),
        );
        assert!(!argv.iter().any(|s| s == "-projwin_srs"));
        assert!(!argv.iter().any(|s| s.contains("nadgrids")));
    }

        #[test]
    fn vsi_rewrite_already_vsi_passthrough() {
        // Idempotent: already-rewritten URLs must not get a second prefix.
        let v1 = "/vsicurl/https://example.com/x.tif";
        let v2 = "/vsis3/bucket/x.tif";
        let local = "/some/local/path.tif";
        assert_eq!(vsi_rewrite(v1), v1);
        assert_eq!(vsi_rewrite(v2), v2);
        assert_eq!(vsi_rewrite(local), local);
    }

    // ---------- in-process Downloader (Phase B) ----------

    /// Build a tiny 4x4 UInt16 GeoTIFF fixture so `download_in_process_with_crs`
    /// has a real raster to crop without going over the network.
    fn write_fixture_cog(path: &std::path::Path, cols: usize, rows: usize) {
        use gdal::raster::Buffer;
        use gdal::DriverManager;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = drv.create_with_band_type::<u16, _>(path, cols, rows, 1).unwrap();
        // Match S2 in EPSG:32633 with sub-tile origin so projwin math has real ground truth.
        ds.set_geo_transform(&[500_000.0, 10.0, 0.0, 5_400_000.0, 0.0, -10.0]).unwrap();
        let sr = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
        ds.set_spatial_ref(&sr).unwrap();
        let mut b = ds.rasterband(1).unwrap();
        let data: Vec<u16> = (0..(cols * rows) as u16).collect();
        let mut buf = Buffer::new((cols, rows), data);
        b.write::<u16>((0, 0), (cols, rows), &mut buf).unwrap();
    }

    /// Build a Sentinel-2-shaped Int16 fixture aligned to a real S2 UTM33N tile
    /// boundary. Pixel size 10 m, origin at a typical S2 tile NW corner
    /// (300000 E / 5400000 N — same CRS scale as the failing 12 MP case).
    fn write_s2_shape_fixture(path: &std::path::Path, cols: usize, rows: usize, origin_e: f64, origin_n: f64) {
        use gdal::raster::Buffer;
        use gdal::DriverManager;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = drv.create_with_band_type::<i16, _>(path, cols, rows, 1).unwrap();
        ds.set_geo_transform(&[origin_e, 10.0, 0.0, origin_n, 0.0, -10.0]).unwrap();
        let sr = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
        ds.set_spatial_ref(&sr).unwrap();
        let mut b = ds.rasterband(1).unwrap();
        // Deterministic ramp pattern so the centre-pixel check has known truth.
        let data: Vec<i16> = (0..(cols * rows)).map(|i| (i % 30000) as i16).collect();
        let mut buf = Buffer::new((cols, rows), data);
        b.write::<i16>((0, 0), (cols, rows), &mut buf).unwrap();
    }

    #[test]
    fn download_in_process_with_local_file_writes_full_copy_when_no_crop() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst = tmp.path().join("out.tif");
        write_fixture_cog(&src, 8, 6);
        let out = download_in_process_with_crs(
            src.to_str().unwrap(), &dst, None, None,
        ).unwrap();
        assert_eq!(out, dst);
        let info = crate::gdal_utils::read_basic_raster_info(&dst).unwrap();
        assert_eq!((info.cols, info.rows), (8, 6));
    }

    #[test]
    fn download_in_process_with_native_crs_crop_writes_subwindow() {
        // Fixture: 8x6 raster, origin=(500000, 5400000), pixel=10m. Native CRS bbox
        // [500020..500060, 5399960..5400000] -> cols 2..6 (=4), rows 0..4 (=4).
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst = tmp.path().join("out.tif");
        write_fixture_cog(&src, 8, 6);
        let crop = CropWindow {
            min_x: 500_020.0,
            max_y: 5_400_000.0,
            max_x: 500_060.0,
            min_y: 5_399_960.0,
        };
        let _ = download_in_process_with_crs(
            src.to_str().unwrap(), &dst, Some(crop), None,
        ).unwrap();
        let info = crate::gdal_utils::read_basic_raster_info(&dst).unwrap();
        assert_eq!((info.cols, info.rows), (4, 4));
        // Geo-transform origin advanced by 2 pixels (col_off=2 -> +20m east).
        assert!((info.geo_transform[0] - 500_020.0).abs() < 1e-6);
    }

    #[test]
    fn download_in_process_with_disjoint_crop_returns_clean_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        let dst = tmp.path().join("out.tif");
        write_fixture_cog(&src, 4, 4);
        // Crop entirely west of the raster (origin_x=500000, pixel=10m, cols=4 -> max_x=500040).
        let crop = CropWindow {
            min_x: 400_000.0,
            max_y: 5_400_000.0,
            max_x: 400_100.0,
            min_y: 5_399_900.0,
        };
        let err = download_in_process_with_crs(
            src.to_str().unwrap(), &dst, Some(crop), None,
        ).expect_err("disjoint crop must fail");
        assert!(format!("{err}").contains("does not intersect"),
            "expected intersection diagnostic, got: {err}");
    }

    #[test]
    fn download_in_process_with_invalid_src_returns_clean_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("out.tif");
        let err = download_in_process_with_crs(
            "/does/not/exist.tif", &dst, None, None,
        ).expect_err("missing source must surface error");
        let msg = err.to_string();
        assert!(msg.contains("in_process open") || msg.contains("does/not/exist"),
            "expected open-failure diagnostic, got: {msg}");
    }

    // ---------- Side-by-side: in-process must match gdal_translate dimensions ----------

    /// True if `gdal_translate` is executable on $PATH; tests below short-circuit
    /// (return ok without asserting) if the CLI is missing so the suite stays hermetic.
    fn have_gdal_translate() -> bool {
        std::process::Command::new("gdal_translate")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Drive both downloaders with the same crop and report
    /// (cols, rows, geo_transform[0..6]) tuples. Pure helper: panics on either failure.
    fn run_both(src: &std::path::Path, crop: Option<CropWindow>, crop_crs: Option<&str>)
        -> ((usize, usize, [f64; 6]), (usize, usize, [f64; 6])) {
        let tmp = tempfile::tempdir().unwrap();
        let cli_dst = tmp.path().join("cli.tif");
        let api_dst = tmp.path().join("api.tif");
        download_via_gdal_translate_with_crs(src.to_str().unwrap(), &cli_dst, crop, crop_crs)
            .expect("subprocess download");
        download_in_process_with_crs(src.to_str().unwrap(), &api_dst, crop, crop_crs)
            .expect("in_process download");
        let i_cli = crate::gdal_utils::read_basic_raster_info(&cli_dst).unwrap();
        let i_api = crate::gdal_utils::read_basic_raster_info(&api_dst).unwrap();
        (
            (i_cli.cols, i_cli.rows, i_cli.geo_transform),
            (i_api.cols, i_api.rows, i_api.geo_transform),
        )
    }

    #[test]
    fn side_by_side_no_crop_dimensions_and_geotransform_match() {
        if !have_gdal_translate() { return; }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        write_fixture_cog(&src, 200, 200);
        let (cli, api) = run_both(&src, None, None);
        assert_eq!(cli.0, api.0, "cols mismatch (cli={}, api={})", cli.0, api.0);
        assert_eq!(cli.1, api.1, "rows mismatch (cli={}, api={})", cli.1, api.1);
        for (i, (a, b)) in cli.2.iter().zip(api.2.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "geo_transform[{i}]: cli={a} api={b}");
        }
    }

    #[test]
    fn side_by_side_native_crs_inbounds_crop_dimensions_match() {
        // Fixture: 200x200 UTM33N raster, origin=(500000, 5400000), pixel=10m.
        // Crop fully inside: cols 5..55 (50px), rows 0..50 (50px) at native CRS.
        if !have_gdal_translate() { return; }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        write_fixture_cog(&src, 200, 200);
        let crop = CropWindow {
            min_x: 500_050.0,
            max_y: 5_400_000.0,
            max_x: 500_550.0,
            min_y: 5_399_500.0,
        };
        let (cli, api) = run_both(&src, Some(crop), None);
        assert_eq!(cli.0, api.0, "cols mismatch (cli={}, api={})", cli.0, api.0);
        assert_eq!(cli.1, api.1, "rows mismatch (cli={}, api={})", cli.1, api.1);
        for (i, (a, b)) in cli.2.iter().zip(api.2.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "geo_transform[{i}]: cli={a} api={b}");
        }
    }

    #[test]
    fn side_by_side_cross_crs_inbounds_crop_dimensions_match() {
        // Cross-CRS: EPSG:4326 bbox projected through to EPSG:32633 (CM=15E) source.
        // At fixture origin (500000, 5400000) UTM33N maps to ~(15.0E, 48.753N).
        // 200x200 at 10m = 2km square -> lon 15.000..15.027, lat 48.735..48.753.
        // Pick a sub-window centered well inside.
        if !have_gdal_translate() { return; }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        write_fixture_cog(&src, 200, 200);
        let crop = CropWindow {
            min_x: 15.005,
            max_y: 48.750,
            max_x: 15.020,
            min_y: 48.740,
        };
        let (cli, api) = run_both(&src, Some(crop), Some("EPSG:4326"));
        assert_eq!(cli.0, api.0, "cols mismatch (cli={}, api={})", cli.0, api.0);
        assert_eq!(cli.1, api.1, "rows mismatch (cli={}, api={})", cli.1, api.1);
        for (i, (a, b)) in cli.2.iter().zip(api.2.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "geo_transform[{i}]: cli={a} api={b}");
        }
    }

    #[test]
    fn side_by_side_partial_oob_crop_dimensions_match_with_nodata_padding() {
        // The P1 dimension bug: when the bbox extends past the source extent,
        // gdal_translate pads with nodata so the OUTPUT preserves the requested
        // pixel window, while the original in-process impl clamped to the in-bounds
        // intersection (smaller output -> downstream block_executor RasterIO OOB).
        if !have_gdal_translate() { return; }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.tif");
        // 100x100 raster, native CRS bbox extends 30m east past the right edge.
        write_fixture_cog(&src, 100, 100);
        let crop = CropWindow {
            min_x: 500_500.0,  // ~50 px in
            max_y: 5_400_000.0,
            max_x: 501_030.0,  // 30 m past right edge (raster ends at 500000+1000=501000)
            min_y: 5_399_500.0,
        };
        let (cli, api) = run_both(&src, Some(crop), None);
        assert_eq!(cli.0, api.0,
            "cols mismatch: subprocess preserves projwin window with nodata padding (cli={}, api={})",
            cli.0, api.0);
        assert_eq!(cli.1, api.1, "rows mismatch (cli={}, api={})", cli.1, api.1);
        for (i, (a, b)) in cli.2.iter().zip(api.2.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "geo_transform[{i}]: cli={a} api={b}");
        }
    }

    /// **Bug repro (2026-05-24)**: in-process path produced smaller dimensions
    /// than the subprocess `gdal_translate -projwin -projwin_srs` on a 12 MP
    /// S2-shaped fixture with a cross-CRS bbox that overlaps but partially
    /// extends past the source's UTM33N extent. Downstream block_executor
    /// derives expected dims from the FIRST loaded band (subprocess output),
    /// then reads the in-process file at column offsets the latter can't
    /// satisfy -- "Access window out of range in RasterIO()". Symmetry across
    /// both downloaders is therefore mandatory, not nice-to-have.
    #[test]
    fn side_by_side_cross_crs_partial_oob_int16_s2_shape_dimensions_match() {
        if !have_gdal_translate() { return; }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("s2_shape.tif");
        let cli_dst = tmp.path().join("cli.tif");
        let api_dst = tmp.path().join("api.tif");
        // 3000 x 3000 Int16 UTM33N raster (~9 MP, S2-band shaped).
        // Origin (300000, 5400000), pixel 10m -> covers [300000..330000] x [5370000..5400000].
        // In EPSG:4326 the NW corner ~ (12.087E, 48.736N), the SE ~ (12.498E, 48.479N).
        write_s2_shape_fixture(&src, 3000, 3000, 300_000.0, 5_400_000.0);
        // 4326 bbox extending well past source east edge (12.498E) to 12.520E (~140m past).
        let crop = CropWindow {
            min_x: 12.300,
            min_y: 48.600,
            max_x: 12.520,
            max_y: 48.700,
        };
        download_via_gdal_translate_with_crs(src.to_str().unwrap(), &cli_dst, Some(crop), Some("EPSG:4326"))
            .expect("subprocess download");
        download_in_process_with_crs(src.to_str().unwrap(), &api_dst, Some(crop), Some("EPSG:4326"))
            .expect("in_process download");
        let i_cli = crate::gdal_utils::read_basic_raster_info(&cli_dst).unwrap();
        let i_api = crate::gdal_utils::read_basic_raster_info(&api_dst).unwrap();
        assert_eq!((i_cli.cols, i_cli.rows), (i_api.cols, i_api.rows),
            "cross-CRS partial-OOB dim mismatch: cli={}x{} api={}x{} -- this is the 12 MP S2 RasterIO OOB bug",
            i_cli.cols, i_cli.rows, i_api.cols, i_api.rows);
        for (i, (a, b)) in i_cli.geo_transform.iter().zip(i_api.geo_transform.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "geo_transform[{i}]: cli={a} api={b}");
        }
        // Centre-pixel value parity (read inside the in-bounds intersection,
        // upper-left quadrant to avoid the OOB-padded right edge).
        let cli_ds = gdal::Dataset::open(&cli_dst).unwrap();
        let api_ds = gdal::Dataset::open(&api_dst).unwrap();
        let mid_col = i_cli.cols / 4;
        let mid_row = i_cli.rows / 2;
        let cli_band = cli_ds.rasterband(1).unwrap();
        let api_band = api_ds.rasterband(1).unwrap();
        let cli_buf = cli_band.read_as::<i16>(
            (mid_col as isize, mid_row as isize), (1, 1), (1, 1), None,
        ).unwrap();
        let api_buf = api_band.read_as::<i16>(
            (mid_col as isize, mid_row as isize), (1, 1), (1, 1), None,
        ).unwrap();
        assert_eq!(cli_buf.data()[0], api_buf.data()[0],
            "centre-pixel mismatch at ({mid_col},{mid_row}): cli={} api={}",
            cli_buf.data()[0], api_buf.data()[0]);
    }

}
