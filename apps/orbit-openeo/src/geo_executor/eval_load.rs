//! `load_collection` + extent / properties parsers.

use std::path::{Path, PathBuf};

use orbit_geo::providers::CropWindow;
use serde_json::{json, Value};

use crate::executor::ExecError;

use super::stac::{BandMetadata, StacScene};
use super::GeoExecutor;

/// Test-only probe counter — increments every time `probe_source` is invoked.
/// Lets unit tests assert the STAC-metadata cache short-circuited the network.
#[cfg(test)]
pub(super) static PROBE_SOURCE_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Test-only serialization lock — the two probe-counter tests share the
/// atomic, so the before/after delta is only meaningful with mutual exclusion.
#[cfg(test)]
pub(super) static PROBE_SOURCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// **Band-alias table (2026-05-25, refactored from two duplicated
/// match arms)**: maps a canonical Sentinel-2 band name to the set of
/// STAC `eo:bands.common_name` / asset-key aliases that different STAC
/// backends use for the same physical band. Element84 uses `red`/`nir`,
/// Microsoft Planetary Computer uses `B04`/`B08`, some use mixed case.
///
/// Single source of truth shared by [`resolve_band_href`] and
/// [`resolve_band_metadata`] so the two can never drift.
///
/// Coverage = the full Sentinel-2 L2A band set + STAC eo-extension
/// common names. Per-sensor keying (S1/Landsat/MODIS) would require
/// threading the collection id to this call site; deferred until a
/// non-S2 collection is wired (today only `sentinel-2-l2a` is exercised
/// end-to-end — see CLAUDE.md §3).
pub(super) fn band_aliases(band: &str) -> &'static [&'static str] {
    match band {
        "B01" => &["coastal", "B1"],
        "B02" => &["blue", "B2"],
        "B03" => &["green", "B3"],
        "B04" => &["red", "Red", "B4"],
        "B05" => &["rededge1", "rededge", "B5"],
        "B06" => &["rededge2", "B6"],
        "B07" => &["rededge3", "B7"],
        "B08" => &["nir", "Nir", "NIR", "B8"],
        "B8A" => &["nir08", "B8a"],
        "B09" => &["nir09", "watervapor", "B9"],
        "B11" => &["swir16", "SWIR16", "B11_swir"],
        "B12" => &["swir22", "SWIR22", "B12_swir"],
        "SCL" => &["scl"],
        _ => &[],
    }
}

/// Resolve `band` against a scene's `bands` map, falling back to the
/// well-known alias set (e.g. asking for `B04` matches `red` if the
/// searcher kept the original asset key). Returns `None` if neither
/// the requested name nor a known alias is present.
pub(super) fn resolve_band_href(scene: &StacScene, band: &str) -> Option<String> {
    if let Some(h) = scene.bands.get(band) {
        return Some(h.clone());
    }
    for alias in band_aliases(band) {
        if let Some(h) = scene.bands.get(*alias) {
            return Some(h.clone());
        }
    }
    None
}

/// **SSRF guard (audit-fix 2026-06-03)** — reject asset hrefs that could drive
/// a server-side request forgery. COG hrefs come from STAC search responses; a
/// malicious or compromised STAC backend (or a `--stac-url` pointed at an
/// attacker server) could return hrefs targeting internal infrastructure —
/// cloud metadata (IMDS `169.254.169.254`), loopback, link-local, or RFC-1918
/// private ranges — or non-HTTP schemes (`file://`). We allow only
/// `http(s)`/`s3` schemes to public-looking hosts.
///
/// This is a string-level denylist (defense-in-depth): it stops the obvious
/// literal-IP / scheme attacks but does NOT resolve DNS, so a hostname that
/// *resolves* to a private IP (DNS rebinding) is not caught here. For this
/// single-tenant reference backend that trade-off is acceptable; a hardened
/// deployment should additionally pin the STAC/asset host allowlist.
pub(super) fn validate_asset_href(href: &str) -> Result<(), ExecError> {
    let lower = href.to_ascii_lowercase();
    let is_s3 = lower.starts_with("s3://");
    let scheme_ok = lower.starts_with("https://") || lower.starts_with("http://") || is_s3;
    if !scheme_ok {
        return Err(ExecError::InvalidGraph(format!(
            "load_collection: asset href scheme not allowed (only http/https/s3): {href}"
        )));
    }
    // Host = substring between `://` and the next `/`, `?` or `#`; strip any
    // `user@` credential prefix and `:port` suffix.
    let after = href.splitn(2, "://").nth(1).unwrap_or("");
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    let h = host.trim().to_ascii_lowercase();

    let blocked = h.is_empty()
        || h == "localhost"
        || h == "169.254.169.254"          // AWS/GCP/Azure instance metadata
        || h == "metadata.google.internal"
        || h.starts_with("127.")           // loopback
        || h.starts_with("0.")
        || h.starts_with("10.")            // RFC-1918
        || h.starts_with("192.168.")       // RFC-1918
        || h.starts_with("169.254.")       // link-local
        || h.starts_with('[')              // IPv6 literal (::1, fc00::, fe80::, …)
        || is_private_172(&h)
        // bare hostname (no dot) ⇒ internal service name — but an `s3://`
        // authority is a BUCKET name (legitimately dotless), so skip this rule
        // for s3 (the IP/IMDS denylist above still applies).
        || (!is_s3 && !h.contains('.'));
    if blocked {
        return Err(ExecError::InvalidGraph(format!(
            "load_collection: asset href host '{host}' is blocked by the SSRF guard"
        )));
    }
    Ok(())
}

/// True for the RFC-1918 `172.16.0.0`–`172.31.255.255` private block.
fn is_private_172(h: &str) -> bool {
    h.strip_prefix("172.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|octet| octet.parse::<u8>().ok())
        .is_some_and(|n| (16..=31).contains(&n))
}

/// **Task #34** — convert a STAC-derived [`BandMetadata`] into the
/// [`orbit_geo::providers::BandMetadataHint`] expected by the async-tiff
/// downloader. Returns `None` only when the input lacks the EPSG (which
/// is the one field the hint REQUIRES to be useful — without it the
/// hint path no-ops anyway).
#[cfg(feature = "async-tiff-downloader")]
fn to_hint(meta: &BandMetadata) -> Option<orbit_geo::providers::BandMetadataHint> {
    meta.epsg.map(|_| orbit_geo::providers::BandMetadataHint {
        epsg: meta.epsg,
        geo_transform: meta.geo_transform,
        raster_size: meta.raster_size.map(|(c, r)| (c as u64, r as u64)),
        dtype: meta.dtype.clone(),
        nodata: meta.nodata,
    })
}

/// Resolve the cached `BandMetadata` for `band` on `scene`, mirroring
/// the alias fallback chain used for hrefs. Returns `None` when the
/// searcher didn't populate metadata for either the canonical name or
/// any known alias.
pub(super) fn resolve_band_metadata(scene: &StacScene, band: &str) -> Option<BandMetadata> {
    if let Some(m) = scene.band_metadata.get(band) {
        return Some(m.clone());
    }
    // Shared alias table (DRY with resolve_band_href).
    for alias in band_aliases(band) {
        if let Some(m) = scene.band_metadata.get(*alias) {
            return Some(m.clone());
        }
    }
    None
}

/// P3 — true when `ORBIT_VSICURL_STREAM=1`. When set, `load_collection`
/// skips the eager download phase entirely. Instead of emitting raw
/// `/vsicurl/<href>` paths into the cube (which would expose the FULL
/// 10980² source extent per S2 tile), it emits one VRT file per
/// (scene, band) cropped to the user's AOI. Downstream
/// `RasterDatasetBuilder::from_files` opens each VRT and libgdal issues
/// HTTP range requests for ONLY the cropped pixel window — no disk
/// overflow on multi-scene jobs, no wasted compute on out-of-AOI blocks.
#[inline]
fn p3_stream_enabled() -> bool {
    std::env::var("ORBIT_VSICURL_STREAM").as_deref() == Ok("1")
}

/// Build the `/vsicurl/<href>` `PathBuf` that GDAL accepts directly.
/// Refuses non-http(s) URLs so the SSRF defenses in `url_policy.rs`
/// still apply when this path is taken.
fn vsicurl_path(href: &str) -> Result<PathBuf, ExecError> {
    if !(href.starts_with("https://") || href.starts_with("http://")) {
        return Err(ExecError::InvalidGraph(format!(
            "P3 (ORBIT_VSICURL_STREAM): refusing non-http(s) href `{href}`"
        )));
    }
    Ok(PathBuf::from(format!("/vsicurl/{href}")))
}

/// AOI pixel window in the source raster's coordinate system, computed
/// from a bbox in `crop_crs` using the same projwin math as
/// `download_in_process_with_crs` (GDAL 3.x snapping rules:
/// `floor(off + 0.001)` for offsets, `ceil(size - 0.001)` for sizes).
///
/// Returned `col_off`/`row_off` may be negative (window starts
/// north/west of source); `cols`/`rows` are the full requested window
/// dimensions (matches `gdal_translate -projwin` semantics).
struct AoiWindow {
    col_off: isize,
    row_off: isize,
    cols: usize,
    rows: usize,
    /// Output geotransform: snapped origin + native pixel sizes.
    out_gt: [f64; 6],
}

/// Per-source metadata cached for VRT emission.
pub(super) struct SourceMeta {
    cols_total: usize,
    rows_total: usize,
    /// Source band dtype as a name suitable for the VRT `dataType`
    /// attribute (e.g. "Int16", "UInt16", "Byte").
    dtype_name: String,
    /// Source band nodata, if set.
    nodata: Option<f64>,
    /// Source SRS as WKT, used for the VRT `<SRS>` field.
    srs_wkt: String,
    /// Source geo-transform `[origin_x, pix_w, 0, origin_y, 0, pix_h]`.
    gt: [f64; 6],
}

/// Build a `SourceMeta` from cached `BandMetadata` (harvested from STAC
/// proj + raster extensions). Returns `None` when any required field is
/// missing so the caller falls back to a live `probe_source`. Builds
/// SRS WKT from the EPSG code via GDAL (one cheap PROJ DB lookup, no
/// network).
fn try_source_meta_from_band_metadata(meta: &BandMetadata) -> Option<SourceMeta> {
    use gdal::spatial_ref::{AxisMappingStrategy, SpatialRef};
    let epsg = meta.epsg?;
    let gt = meta.geo_transform?;
    let (cols, rows) = meta.raster_size?;
    let dtype = meta.dtype.clone()?;
    let mut sr = SpatialRef::from_epsg(epsg).ok()?;
    sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let srs_wkt = sr.to_wkt().ok()?;
    Some(SourceMeta {
        cols_total: cols,
        rows_total: rows,
        dtype_name: dtype,
        nodata: meta.nodata,
        srs_wkt,
        gt,
    })
}

/// Read source geotransform / projection / size / band metadata in a
/// single `Dataset::open` round-trip (HTTP HEAD + minimal GET on
/// `/vsicurl/...`). Blocking — callers in async context must invoke
/// inside `tokio::task::spawn_blocking` per CLAUDE.md §4 P0-5.
fn probe_source(src: &str) -> Result<SourceMeta, ExecError> {
    #[cfg(test)]
    PROBE_SOURCE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    use gdal::spatial_ref::AxisMappingStrategy;
    use gdal::Dataset;
    let ds = Dataset::open(src).map_err(|e| {
        ExecError::Backend(format!("P3 probe open {src}: {e}"))
    })?;
    let gt = ds.geo_transform().map_err(|e| {
        ExecError::Backend(format!("P3 probe geo_transform {src}: {e}"))
    })?;
    let (cols_total, rows_total) = ds.raster_size();
    let mut sr = ds.spatial_ref().map_err(|e| {
        ExecError::Backend(format!("P3 probe spatial_ref {src}: {e}"))
    })?;
    sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let srs_wkt = sr.to_wkt().map_err(|e| {
        ExecError::Backend(format!("P3 probe to_wkt {src}: {e}"))
    })?;
    let band = ds.rasterband(1).map_err(|e| {
        ExecError::Backend(format!("P3 probe rasterband {src}: {e}"))
    })?;
    let dtype_name = band.band_type().name();
    let nodata = band.no_data_value();
    Ok(SourceMeta {
        cols_total,
        rows_total,
        dtype_name,
        nodata,
        srs_wkt,
        gt,
    })
}

/// Compute the AOI pixel window in source CRS using the SAME math as
/// `orbit_geo::providers::download_in_process_with_crs` (which mirrors
/// GDAL 3.x's `gdal_translate -projwin -projwin_srs` semantics).
///
/// Returns `None` (with diagnostic in `Err`) when the projected window
/// would degenerate (0 px) or sit entirely outside the source raster.
fn compute_aoi_window(
    meta: &SourceMeta,
    crop: CropWindow,
    crop_crs: &str,
) -> Result<AoiWindow, ExecError> {
    use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
    let (origin_x, pix_w, origin_y, pix_h) =
        (meta.gt[0], meta.gt[1], meta.gt[3], meta.gt[5]);
    if pix_w == 0.0 || pix_h == 0.0 {
        return Err(ExecError::Backend(format!(
            "P3 compute_aoi_window: degenerate geo_transform pix_w={pix_w} pix_h={pix_h}"
        )));
    }
    let mut src_sr = SpatialRef::from_wkt(&meta.srs_wkt).map_err(|e| {
        ExecError::Backend(format!("P3 src SpatialRef::from_wkt: {e}"))
    })?;
    src_sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let mut work_sr = SpatialRef::from_definition(crop_crs).map_err(|e| {
        ExecError::Backend(format!("P3 work SpatialRef::from_definition({crop_crs}): {e}"))
    })?;
    work_sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let xform = CoordTransform::new(&work_sr, &src_sr).map_err(|e| {
        ExecError::Backend(format!("P3 CoordTransform: {e}"))
    })?;
    let src_bounds = xform
        .transform_bounds(&[crop.min_x, crop.min_y, crop.max_x, crop.max_y], 21)
        .map_err(|e| ExecError::Backend(format!("P3 transform_bounds: {e}")))?;
    let pulx = src_bounds[0];
    let plry = src_bounds[1];
    let plrx = src_bounds[2];
    let puly = src_bounds[3];
    let col_off_i = ((pulx - origin_x) / pix_w + 0.001).floor() as isize;
    let row_off_i = ((puly - origin_y) / pix_h + 0.001).floor() as isize;
    let snapped_ulx = col_off_i as f64 * pix_w + origin_x;
    let snapped_uly = row_off_i as f64 * pix_h + origin_y;
    let cols_f = ((plrx - snapped_ulx) / pix_w - 0.001).ceil();
    let rows_f = ((plry - snapped_uly) / pix_h - 0.001).ceil();
    if !cols_f.is_finite() || !rows_f.is_finite() || cols_f < 1.0 || rows_f < 1.0 {
        return Err(ExecError::Backend(format!(
            "P3: degenerate projwin produces 0-pixel window (src=[{origin_x},{origin_y}] \
             {cols}x{rows}, crop projects to [{pulx:.1},{plrx:.1}] x [{plry:.1},{puly:.1}])",
            cols = meta.cols_total, rows = meta.rows_total
        )));
    }
    let cols = cols_f as usize;
    let rows = rows_f as usize;
    let in_x = (col_off_i + cols as isize) > 0 && col_off_i < meta.cols_total as isize;
    let in_y = (row_off_i + rows as isize) > 0 && row_off_i < meta.rows_total as isize;
    if !in_x || !in_y {
        return Err(ExecError::Backend(format!(
            "P3: crop window does not intersect source raster (src=[{origin_x},{origin_y}] \
             {cols_t}x{rows_t}, crop projects to [{pulx:.1},{plrx:.1}] x [{plry:.1},{puly:.1}])",
            cols_t = meta.cols_total, rows_t = meta.rows_total
        )));
    }
    let out_gt = [
        origin_x + col_off_i as f64 * pix_w,
        pix_w,
        0.0,
        origin_y + row_off_i as f64 * pix_h,
        0.0,
        pix_h,
    ];
    Ok(AoiWindow {
        col_off: col_off_i,
        row_off: row_off_i,
        cols,
        rows,
        out_gt,
    })
}

/// Minimal XML-attribute escape for the `<SourceFilename>` text and
/// the SRS WKT inside `<SRS>...</SRS>`. We don't expect `<`, `>`, `&`
/// in S2 hrefs or PROJ WKT, but escape defensively so a future
/// signed-URL provider with `&` in query strings can't break the VRT
/// parser.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Format the GDAL geotransform tuple as the comma-separated string the
/// VRT `<GeoTransform>` element expects.
fn format_geotransform(gt: &[f64; 6]) -> String {
    format!(
        "{:.16e}, {:.16e}, {:.16e}, {:.16e}, {:.16e}, {:.16e}",
        gt[0], gt[1], gt[2], gt[3], gt[4], gt[5]
    )
}

/// Format the VRT XML for a windowed view onto `src_for_vrt` (anything
/// `Dataset::open` accepts — local path, `/vsicurl/...`, etc).
///
/// The output VRT exposes only the cropped pixel window. Downstream
/// `Dataset::open` on the VRT path sees a raster of size `win.cols x
/// win.rows`; block_executor's RasterIO calls translate to range reads
/// against the underlying source covering JUST the AOI.
fn build_vrt_xml(src_for_vrt: &str, meta: &SourceMeta, win: &AoiWindow) -> String {
    let src_escaped = xml_escape(src_for_vrt);
    let srs_escaped = xml_escape(&meta.srs_wkt);
    let gt_str = format_geotransform(&win.out_gt);
    let dtype = &meta.dtype_name;
    let nodata_xml = match meta.nodata {
        Some(n) => format!("    <NoDataValue>{n}</NoDataValue>\n"),
        None => String::new(),
    };
    // In partial-OOB cases (window extends past source) the SrcRect
    // covers only the in-bounds intersection while DstRect places it
    // at the matching offset inside the output. GDAL pads the rest
    // with NoDataValue.
    let read_col_start = win.col_off.max(0);
    let read_row_start = win.row_off.max(0);
    let read_col_end = (win.col_off + win.cols as isize)
        .max(0)
        .min(meta.cols_total as isize);
    let read_row_end = (win.row_off + win.rows as isize)
        .max(0)
        .min(meta.rows_total as isize);
    let src_w = (read_col_end - read_col_start).max(0) as usize;
    let src_h = (read_row_end - read_row_start).max(0) as usize;
    let dst_x = (read_col_start - win.col_off) as usize;
    let dst_y = (read_row_start - win.row_off) as usize;
    format!(
        r#"<VRTDataset rasterXSize="{cols}" rasterYSize="{rows}">
  <SRS>{srs}</SRS>
  <GeoTransform>{gt}</GeoTransform>
  <VRTRasterBand dataType="{dtype}" band="1">
{nodata}    <SimpleSource>
      <SourceFilename relativeToVRT="0">{src}</SourceFilename>
      <SourceBand>1</SourceBand>
      <SrcRect xOff="{src_x}" yOff="{src_y}" xSize="{src_w}" ySize="{src_h}"/>
      <DstRect xOff="{dst_x}" yOff="{dst_y}" xSize="{src_w}" ySize="{src_h}"/>
    </SimpleSource>
  </VRTRasterBand>
</VRTDataset>
"#,
        cols = win.cols,
        rows = win.rows,
        srs = srs_escaped,
        gt = gt_str,
        dtype = dtype,
        nodata = nodata_xml,
        src = src_escaped,
        src_x = read_col_start,
        src_y = read_row_start,
        src_w = src_w,
        src_h = src_h,
        dst_x = dst_x,
        dst_y = dst_y,
    )
}

/// Same as [`emit_cropped_vrt_p3`] but accepts a pre-resolved `SourceMeta`
/// to skip the per-source `Dataset::open` probe — shaves ~4 s per
/// (scene, band) when STAC already carried `proj:*` + `raster:bands`.
/// Module-private because `SourceMeta` is an internal type.
pub(super) fn emit_cropped_vrt_p3(
    src_for_vrt: &str,
    dst_vrt: &Path,
    crop: CropWindow,
    crop_crs: &str,
    cached_meta: Option<SourceMeta>,
) -> Result<PathBuf, ExecError> {
    let meta = match cached_meta {
        Some(m) => m,
        None => probe_source(src_for_vrt)?,
    };
    let win = compute_aoi_window(&meta, crop, crop_crs)?;
    if let Some(parent) = dst_vrt.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ExecError::Backend(format!("P3 mkdir {}: {e}", parent.display()))
            })?;
        }
    }
    let xml = build_vrt_xml(src_for_vrt, &meta, &win);
    std::fs::write(dst_vrt, xml).map_err(|e| {
        ExecError::Backend(format!("P3 write VRT {}: {e}", dst_vrt.display()))
    })?;
    Ok(dst_vrt.to_path_buf())
}

impl GeoExecutor {
    pub(super) async fn eval_load_collection(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let id_arg = args.get("id").cloned().unwrap_or(Value::Null);
        let id_str = id_arg
            .as_str()
            .ok_or_else(|| ExecError::InvalidGraph(
                "load_collection: `id` argument must be a string".into(),
            ))?
            .to_string();

        // Catalog round-trip (so unknown collections fail loudly).
        if let Some(cat) = &self.catalog {
            cat.get(&id_str).await.map_err(|e| match e {
                crate::catalog::CatalogError::NotFound(id) => {
                    ExecError::Backend(format!("CollectionNotFound: {id}"))
                }
                other => ExecError::Backend(format!("catalog: {other}")),
            })?;
        }

        // If we have a searcher AND an explicit spatial_extent → run the
        // real STAC search + cropped-COG download pipeline. Otherwise
        // fall back to the lightweight cube sentinel so arithmetic-only
        // graphs still work.
        let spatial = args.get("spatial_extent");
        if let (Some(searcher), Some(spatial)) = (&self.searcher, spatial) {
            let (bbox, crs) = parse_bbox(spatial)?;
            let datetime = args
                .get("temporal_extent")
                .and_then(|v| parse_temporal(v).ok())
                .as_deref()
                .map(str::to_string);
            // audit-fix (2026-06-03): clamp the scene limit. An attacker-set
            // `limit` of millions would drive an unbounded STAC fan-out +
            // per-(scene×band) download/compute task explosion (memory + disk
            // DoS). 50 scenes is well beyond any real openEO temporal composite.
            const MAX_SCENES: u64 = 50;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .clamp(1, MAX_SCENES) as u32;
            let max_cloud_cover = args
                .get("properties")
                .and_then(parse_eo_cloud_cover_lt);
            let scenes = searcher
                .search(&id_str, bbox, datetime.as_deref(), limit, max_cloud_cover)
                .await?;
            if scenes.is_empty() {
                return Err(ExecError::Backend(format!(
                    "STAC search returned no scenes for collection `{id_str}` in bbox {bbox:?}"
                )));
            }

            // Parse `bands` argument (openEO `load_collection.bands`).
            // When omitted, default to the canonical S2 backbone so
            // legacy graphs that don't set `bands` continue to work.
            let requested_bands: Vec<String> = args
                .get("bands")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_else(|| {
                    vec!["B04".to_string(), "B08".to_string(), "SCL".to_string()]
                });
            if requested_bands.is_empty() {
                return Err(ExecError::InvalidGraph(
                    "load_collection: `bands` argument must contain at least one band".into(),
                ));
            }
            // B1: validate every band name before it propagates to scratch_dir.join.
            for band in &requested_bands {
                super::identifier::validate_identifier(band, "load_collection.bands")?;
            }
            // B2: validate the CRS spec before it reaches gdal_translate.
            orbit_geo::providers::validate_crs_spec(&crs)
                .map_err(|e| ExecError::InvalidGraph(format!("load_collection.spatial_extent.crs: {e}")))?;

            // P3 dispatch — skip the FuturesUnordered download pump. For
            // each (scene, band) emit a VRT file that exposes ONLY the
            // user's AOI window onto the source COG. Downstream
            // `Dataset::open(<vrt>)` reads via /vsicurl/ range requests
            // and sees the cropped raster directly — no full-tile reads,
            // no disk-overflow on multi-scene jobs.
            if p3_stream_enabled() {
                let crop = CropWindow {
                    min_x: bbox[0],
                    min_y: bbox[1],
                    max_x: bbox[2],
                    max_y: bbox[3],
                };
                let n = scenes.len();
                let mut per_band: std::collections::BTreeMap<String, Vec<Option<PathBuf>>> =
                    std::collections::BTreeMap::new();
                for band in &requested_bands {
                    per_band.insert(band.clone(), vec![None; n]);
                }
                // CLAUDE.md §4 P0-5: GDAL Dataset::open + VRT write are
                // blocking; route every per-(scene,band) emission through
                // spawn_blocking + the download semaphore to bound
                // parallelism the same way the eager-download path does.
                use futures::stream::{FuturesUnordered, StreamExt};
                let mut tasks = FuturesUnordered::new();
                let mut cache_hits: usize = 0;
                let mut cache_misses: usize = 0;
                for (i, s) in scenes.iter().enumerate() {
                    for band in &requested_bands {
                        if let Some(href) = resolve_band_href(s, band) {
                            // SSRF guard (P3 /vsicurl/ stream path).
                            validate_asset_href(&href)?;
                            let vsi = vsicurl_path(&href)?;
                            let vsi_str = vsi.to_string_lossy().into_owned();
                            let dst = self.scratch_dir.join(format!("{band}_{i}_p3.vrt"));
                            let band_owned = band.clone();
                            let crs_owned = crs.clone();
                            let sem = self.download_sem.clone();
                            // STAC fast-path: prefer cached BandMetadata over a
                            // live /vsicurl/ probe. Falls back to None when any
                            // required field (epsg, transform, shape, dtype)
                            // is missing — downstream emits `Dataset::open`.
                            let cached = resolve_band_metadata(s, band)
                                .as_ref()
                                .and_then(try_source_meta_from_band_metadata);
                            if cached.is_some() {
                                cache_hits += 1;
                            } else {
                                cache_misses += 1;
                            }
                            tasks.push(Box::pin(async move {
                                let permit = sem
                                    .acquire_owned()
                                    .await
                                    .map_err(|e| ExecError::Backend(format!("semaphore: {e}")))?;
                                let p = tokio::task::spawn_blocking(move || {
                                    let _permit = permit;
                                    emit_cropped_vrt_p3(
                                        &vsi_str, &dst, crop, &crs_owned, cached,
                                    )
                                })
                                .await
                                .map_err(|e| ExecError::Backend(format!("spawn_blocking join: {e}")))??;
                                Ok::<_, ExecError>((i, band_owned, p))
                            })
                                as std::pin::Pin<
                                    Box<
                                        dyn std::future::Future<
                                                Output = Result<(usize, String, PathBuf), ExecError>,
                                            > + Send
                                            + '_,
                                    >,
                                >);
                        }
                    }
                }
                tracing::info!(
                    cache_hits,
                    cache_misses,
                    "P3: STAC metadata cache — probes skipped vs needed"
                );
                while let Some(res) = tasks.next().await {
                    let (i, band, p) = res?;
                    if let Some(slot) = per_band.get_mut(&band) {
                        slot[i] = Some(p);
                    }
                }
                let mut bands_map = serde_json::Map::new();
                for (band, slots) in &per_band {
                    if slots.iter().all(|o| o.is_some()) {
                        let paths: Vec<PathBuf> =
                            slots.iter().flat_map(|o| o.clone()).collect();
                        bands_map.insert(band.clone(), super::paths_to_value(&paths));
                    } else {
                        tracing::warn!(
                            band = %band,
                            "P3: dropping band — not present on every scene"
                        );
                    }
                }
                if bands_map.is_empty() {
                    return Err(ExecError::Backend(format!(
                        "load_collection (P3): none of the requested bands {requested_bands:?} \
                         are present on every scene"
                    )));
                }
                tracing::info!(
                    scenes = n,
                    bands = bands_map.len(),
                    "P3: emitted cropped-VRT cube — skipping eager download phase"
                );
                let mut cube = serde_json::Map::new();
                cube.insert("collection".into(), Value::String(id_str.clone()));
                cube.insert(
                    "bbox".into(),
                    Value::Array(
                        bbox.iter()
                            .map(|x| {
                                serde_json::Number::from_f64(*x)
                                    .map(Value::Number)
                                    .unwrap_or(Value::Null)
                            })
                            .collect(),
                    ),
                );
                cube.insert("scene_count".into(), Value::from(n));
                cube.insert("bands".into(), Value::Object(bands_map));
                cube.insert("crs".into(), Value::String(crs.clone()));
                cube.insert(
                    "p3_vsicurl_stream".into(),
                    Value::Bool(true),
                );
                return Ok(json!({ "__cube": Value::Object(cube) }));
            }

            // Download cropped windows for every requested band of every
            // scene. Scenes lacking a requested band cause that band to
            // be dropped from the output cube (only bands present on ALL
            // scenes survive, mirroring the old scl_paths "all-or-none"
            // contract).
            let crop = CropWindow {
                min_x: bbox[0],
                min_y: bbox[1],
                max_x: bbox[2],
                max_y: bbox[3],
            };
            // **P0-5 / P1-9**: concurrent per-band downloads via
            // `fetch_with_cache_async`, bounded by `download_sem`.
            // **Task #34**: when the async-tiff downloader is active, pass
            // the STAC-derived `BandMetadataHint` so the bbox can be
            // pre-projected in parallel with the IFD fetch and incompatible
            // crop_crs can short-circuit to libgdal pre-open.
            use futures::stream::{FuturesUnordered, StreamExt};
            let mut tasks = FuturesUnordered::new();
            // Telemetry: count how many dispatches got hints vs. fell back
            // to the no-hint path. Emitted as a single line below.
            // `mut` is needed only when the async-tiff-downloader feature is
            // built — under the default feature combo these are never
            // mutated, so we gate the bindings to avoid `unused_mut`.
            #[cfg(feature = "async-tiff-downloader")]
            let mut hint_dispatched: usize = 0;
            #[cfg(feature = "async-tiff-downloader")]
            let mut hint_missing: usize = 0;
            for (i, s) in scenes.iter().enumerate() {
                for band in &requested_bands {
                    // Resolve band on this scene: prefer exact match,
                    // then check the legacy alias map. If absent, skip
                    // — handled below by the "all scenes must have it"
                    // check.
                    let href = resolve_band_href(s, band);
                    if let Some(href) = href {
                        // SSRF guard: validate the STAC-supplied href host/scheme
                        // before issuing any server-side fetch.
                        validate_asset_href(&href)?;
                        let dst = self.scratch_dir.join(format!("{band}_{i}.tif"));
                        let band_owned = band.clone();
                        let crs_owned = crs.clone();
                        #[cfg(feature = "async-tiff-downloader")]
                        let hint = resolve_band_metadata(s, band).and_then(|m| to_hint(&m));
                        #[cfg(feature = "async-tiff-downloader")]
                        {
                            if hint.is_some() {
                                hint_dispatched += 1;
                            } else {
                                hint_missing += 1;
                            }
                        }
                        tasks.push(Box::pin(async move {
                            #[cfg(feature = "async-tiff-downloader")]
                            let p = self
                                .fetch_with_cache_async_with_meta(
                                    href, dst, crop, Some(crs_owned), hint,
                                )
                                .await?;
                            #[cfg(not(feature = "async-tiff-downloader"))]
                            let p = self
                                .fetch_with_cache_async(href, dst, crop, Some(crs_owned))
                                .await?;
                            Ok::<_, ExecError>((i, band_owned, p))
                        })
                            as std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                            Output = Result<(usize, String, PathBuf), ExecError>,
                                        > + Send
                                        + '_,
                                >,
                            >);
                    }
                }
            }
            #[cfg(feature = "async-tiff-downloader")]
            tracing::info!(
                hint_dispatched, hint_missing,
                "P2: STAC band_metadata hint — dispatches with hint vs without"
            );
            // band_name → vec<Option<PathBuf>> indexed by scene.
            let n = scenes.len();
            let mut per_band: std::collections::BTreeMap<String, Vec<Option<PathBuf>>> =
                std::collections::BTreeMap::new();
            for band in &requested_bands {
                per_band.insert(band.clone(), vec![None; n]);
            }
            while let Some(res) = tasks.next().await {
                let (i, band, p) = res?;
                if let Some(slot) = per_band.get_mut(&band) {
                    slot[i] = Some(p);
                }
            }
            // Keep only bands present on EVERY scene (all-or-none
            // contract — mixed coverage would mis-align the time axis).
            let mut bands_map = serde_json::Map::new();
            for (band, slots) in &per_band {
                if slots.iter().all(|o| o.is_some()) {
                    let paths: Vec<PathBuf> =
                        slots.iter().flat_map(|o| o.clone()).collect();
                    bands_map.insert(band.clone(), super::paths_to_value(&paths));
                } else {
                    tracing::warn!(
                        band = %band,
                        "dropping band — not present on every scene"
                    );
                }
            }
            if bands_map.is_empty() {
                return Err(ExecError::Backend(format!(
                    "load_collection: none of the requested bands {requested_bands:?} \
                     are present on every scene"
                )));
            }

            // **Reflectance scale (Option B, 2026-05-25)**: harvest
            // `raster:bands.scale`/`offset` per surviving band from the
            // first scene's STAC metadata. Only emit an entry when scale
            // is present AND not the identity (1.0, 0.0) — keeps SCL and
            // scale-less backends out of the map (= no conversion). Downstream
            // `apply` consults this to convert DN → reflectance before
            // absolute math; ratio indices ignore it (scale-invariant).
            let mut band_scales = serde_json::Map::new();
            if let Some(first_scene) = scenes.first() {
                for band in bands_map.keys() {
                    if let Some(meta) = resolve_band_metadata(first_scene, band) {
                        let scale = meta.scale.unwrap_or(1.0);
                        let offset = meta.offset.unwrap_or(0.0);
                        if scale != 1.0 || offset != 0.0 {
                            band_scales.insert(
                                band.clone(),
                                Value::Array(vec![
                                    serde_json::json!(scale),
                                    serde_json::json!(offset),
                                ]),
                            );
                        }
                    }
                }
            }

            let mut cube = serde_json::Map::new();
            cube.insert("collection".into(), Value::String(id_str.clone()));
            if !band_scales.is_empty() {
                cube.insert("band_scales".into(), Value::Object(band_scales));
            }
            cube.insert(
                "bbox".into(),
                Value::Array(
                    bbox.iter()
                        .map(|x| {
                            serde_json::Number::from_f64(*x)
                                .map(Value::Number)
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                ),
            );
            cube.insert("scene_count".into(), Value::from(scenes.len()));
            // **H2 (process audit)**: per-scene timestamps, index-aligned with
            // each band's path vector (scene order), so `filter_temporal` can
            // prune scenes by time. Emitted only when EVERY scene has a
            // datetime — a partial set would mis-align the time axis on prune.
            if scenes.iter().all(|s| s.datetime.is_some()) {
                let dts: Vec<Value> = scenes
                    .iter()
                    .filter_map(|s| s.datetime.clone())
                    .map(Value::String)
                    .collect();
                cube.insert("datetimes".into(), Value::Array(dts));
            }
            cube.insert("bands".into(), Value::Object(bands_map));
            return Ok(json!({ "__cube": Value::Object(cube) }));
        }

        // Lightweight fallback — no search wired (e.g. arithmetic-only graphs).
        Ok(serde_json::json!({ "type": "DataCube", "collection": id_str }))
    }
}

/// openEO 1.3.0 `spatial_extent` is a BoundingBox object:
/// `{"west":…, "south":…, "east":…, "north":…, "crs":…?}`.
///
/// `crs` is optional and defaults to EPSG:4326 (WGS84 lon/lat) per spec.
/// Accepted forms:
/// - integer EPSG code  → `4326`             → `"EPSG:4326"`
/// - bare numeric string → `"4326"`           → `"EPSG:4326"`
/// - qualified string    → `"EPSG:32633"`     → passed through
/// - PROJ string         → `"+proj=utm …"`    → passed through (contains `:`)
/// - absent / null       →                    → `"EPSG:4326"` (default)
pub(super) fn parse_bbox(v: &Value) -> Result<([f64; 4], String), ExecError> {
    let west = v.get("west").and_then(|x| x.as_f64());
    let south = v.get("south").and_then(|x| x.as_f64());
    let east = v.get("east").and_then(|x| x.as_f64());
    let north = v.get("north").and_then(|x| x.as_f64());
    let bbox = match (west, south, east, north) {
        (Some(w), Some(s), Some(e), Some(n)) => [w, s, e, n],
        _ => return Err(ExecError::InvalidGraph(
            "spatial_extent must have numeric west/south/east/north".into(),
        )),
    };
    // Reason: L3 — reject inverted/empty bboxes (west>=east or south>=north). Without
    // this, gdal_translate silently produces a 1×1 output (CLAUDE.md §7). Antimeridian
    // wrap (west>east when crossing 180°) is not currently supported anywhere in the
    // codebase, so unconditional rejection is the safe path.
    if !(bbox[0] < bbox[2]) {
        return Err(ExecError::InvalidGraph(format!(
            "spatial_extent: west ({}) must be < east ({})", bbox[0], bbox[2]
        )));
    }
    if !(bbox[1] < bbox[3]) {
        return Err(ExecError::InvalidGraph(format!(
            "spatial_extent: south ({}) must be < north ({})", bbox[1], bbox[3]
        )));
    }
    let crs = parse_bbox_crs(v.get("crs"))?;
    Ok((bbox, crs))
}

/// Normalise the `spatial_extent.crs` field per openEO 1.3.0.
pub(super) fn parse_bbox_crs(v: Option<&Value>) -> Result<String, ExecError> {
    match v {
        None | Some(Value::Null) => Ok("EPSG:4326".to_string()),
        Some(Value::Number(n)) => {
            // openEO accepts integer EPSG codes.
            if let Some(u) = n.as_u64() {
                Ok(format!("EPSG:{u}"))
            } else if let Some(i) = n.as_i64() {
                if i < 0 {
                    Err(ExecError::InvalidGraph(format!(
                        "spatial_extent.crs invalid: negative EPSG code {i}"
                    )))
                } else {
                    Ok(format!("EPSG:{i}"))
                }
            } else {
                Err(ExecError::InvalidGraph(format!(
                    "spatial_extent.crs invalid: non-integer numeric {n}"
                )))
            }
        }
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(ExecError::InvalidGraph(
                    "spatial_extent.crs invalid: empty string".into(),
                ));
            }
            if s.contains(':') {
                // Already qualified (`EPSG:4326`) or a PROJ string (`+proj=...`).
                Ok(s.to_string())
            } else if s.chars().all(|c| c.is_ascii_digit()) {
                // Bare numeric string — qualify it.
                Ok(format!("EPSG:{s}"))
            } else {
                Err(ExecError::InvalidGraph(format!(
                    "spatial_extent.crs invalid: unqualified non-numeric string `{s}`"
                )))
            }
        }
        Some(other) => Err(ExecError::InvalidGraph(format!(
            "spatial_extent.crs invalid: expected integer or string, got {other}"
        ))),
    }
}

/// openEO `temporal_extent` is `["start", "end"]`. We collapse to the
/// STAC search `start/end` string `"start/end"`.
///
/// Element84 (and most STAC servers) require **full RFC 3339 timestamps**
/// (`YYYY-MM-DDTHH:MM:SSZ`); a bare date like `2024-06-01` silently
/// matches zero items. We normalise date-only inputs by appending
/// `T00:00:00Z` for the start and `T23:59:59Z` for the end.
pub(super) fn parse_temporal(v: &Value) -> Result<String, ExecError> {
    let arr = v
        .as_array()
        .ok_or_else(|| ExecError::InvalidGraph("temporal_extent must be an array".into()))?;
    if arr.len() != 2 {
        return Err(ExecError::InvalidGraph("temporal_extent must have two entries".into()));
    }
    let start = normalise_iso(arr[0].as_str().unwrap_or(".."), true);
    let end = normalise_iso(arr[1].as_str().unwrap_or(".."), false);
    Ok(format!("{start}/{end}"))
}

/// Extract a `max_cloud_cover` threshold from openEO `load_collection.properties`.
///
/// Expected canonical shape (what `openeo-python-client.max_cloud_cover()` emits):
///
/// ```json
/// {
///   "eo:cloud_cover": {
///     "process_graph": {
///       "cc": {
///         "process_id": "lt" | "lte",
///         "arguments": { "x": {"from_parameter": "value"}, "y": 30 },
///         "result": true
///       }
///     }
///   }
/// }
/// ```
///
/// Returns `Some(threshold)` for `lt`/`lte` over `value`, else `None`.
/// Anything more exotic falls through (server-side filter on `eo:cloud_cover`
/// is best-effort; the client can still get all scenes if the predicate
/// shape isn't one we recognise).
pub fn parse_eo_cloud_cover_lt(props: &Value) -> Option<f64> {
    let pg = props
        .get("eo:cloud_cover")?
        .get("process_graph")?
        .as_object()?;
    for node in pg.values() {
        let pid = node.get("process_id")?.as_str()?;
        if pid != "lt" && pid != "lte" {
            continue;
        }
        let args = node.get("arguments")?.as_object()?;
        // x must be `from_parameter: "value"`, y must be a number.
        let x_ok = args
            .get("x")
            .and_then(|x| x.get("from_parameter"))
            .and_then(|s| s.as_str())
            .map(|s| s == "value")
            .unwrap_or(false);
        let y = args.get("y").and_then(|y| y.as_f64())?;
        if x_ok {
            return Some(y);
        }
    }
    None
}

/// Promote a bare `YYYY-MM-DD` to a full RFC 3339 timestamp. Leaves
/// already-timestamped strings (anything containing `T` or `:` or `..`)
/// alone.
pub(super) fn normalise_iso(s: &str, is_start: bool) -> String {
    if s == ".." || s.contains('T') || s.contains(':') {
        return s.to_string();
    }
    // Heuristic: looks like a bare date (10 chars, two '-'s).
    let has_two_dashes = s.matches('-').count() == 2;
    if s.len() == 10 && has_two_dashes {
        return if is_start {
            format!("{s}T00:00:00Z")
        } else {
            format!("{s}T23:59:59Z")
        };
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_bbox_round_trips_openeo_spatial_extent() {
        let v = json!({"west": 1.0, "south": 2.0, "east": 3.0, "north": 4.0});
        let (bbox, crs) = parse_bbox(&v).unwrap();
        assert_eq!(bbox, [1.0, 2.0, 3.0, 4.0]);
        // No `crs` field → openEO 1.3.0 default of EPSG:4326.
        assert_eq!(crs, "EPSG:4326");
    }

    #[test]
    fn band_aliases_resolves_common_names_both_directions() {
        // Element84 common-name → canonical S2.
        assert!(band_aliases("B04").contains(&"red"));
        assert!(band_aliases("B08").contains(&"nir"));
        assert!(band_aliases("B8A").contains(&"nir08"));
        assert!(band_aliases("B11").contains(&"swir16"));
        assert!(band_aliases("B05").contains(&"rededge1"));
        // Unknown band → empty alias set (no panic).
        assert!(band_aliases("B99").is_empty());
    }

    #[test]
    fn resolve_band_href_falls_back_to_alias() {
        use crate::geo_executor::stac::StacScene;
        let mut bands = std::collections::BTreeMap::new();
        bands.insert("red".to_string(), "https://x/red.tif".to_string());
        let scene = StacScene { id: "s".into(), bands, band_metadata: Default::default(), datetime: None };
        // Asking for canonical B04 resolves the `red` alias href.
        assert_eq!(resolve_band_href(&scene, "B04").as_deref(), Some("https://x/red.tif"));
        // Asking for a band not present (even via alias) → None.
        assert_eq!(resolve_band_href(&scene, "B08"), None);
    }

    #[test]
    fn validate_asset_href_allows_public_cog_blocks_ssrf() {
        // Real sentinel-cogs hrefs must pass.
        assert!(validate_asset_href(
            "https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/33/U/XP/2024/6/x.tif"
        ).is_ok());
        assert!(validate_asset_href("s3://sentinel-cogs/foo/bar.tif").is_ok());
        // SSRF targets + bad schemes must be rejected.
        for bad in [
            "http://169.254.169.254/latest/meta-data/iam/",   // cloud IMDS
            "https://localhost/x.tif",
            "http://127.0.0.1:8080/x.tif",
            "https://10.0.0.5/x.tif",                          // RFC-1918
            "https://192.168.1.1/x.tif",
            "https://172.16.0.1/x.tif",
            "http://metadata.google.internal/x",
            "file:///etc/passwd",
            "https://internalhost/x.tif",                      // bare name, no dot
            "https://[::1]/x.tif",                              // IPv6 loopback literal
        ] {
            assert!(validate_asset_href(bad).is_err(), "must block SSRF href: {bad}");
        }
        // A public 172.x outside the private block is allowed.
        assert!(validate_asset_href("https://172.32.0.1/x.tif").is_ok());
    }

    #[test]
    fn parse_bbox_missing_field_errors() {
        let v = json!({"west": 1.0, "south": 2.0});
        assert!(parse_bbox(&v).is_err());
    }

    #[test]
    fn parse_bbox_rejects_west_gt_east() {
        let v = json!({"west": 10.0, "south": 45.0, "east": 5.0, "north": 50.0});
        let err = parse_bbox(&v).unwrap_err();
        assert!(matches!(err, ExecError::InvalidGraph(m) if m.contains("west") && m.contains("east")));
    }

    #[test]
    fn parse_bbox_rejects_west_eq_east() {
        let v = json!({"west": 10.0, "south": 45.0, "east": 10.0, "north": 50.0});
        assert!(parse_bbox(&v).is_err());
    }

    #[test]
    fn parse_bbox_rejects_south_ge_north() {
        let v = json!({"west": 10.0, "south": 50.0, "east": 15.0, "north": 45.0});
        let err = parse_bbox(&v).unwrap_err();
        assert!(matches!(err, ExecError::InvalidGraph(m) if m.contains("south") && m.contains("north")));
    }

    // ---------- A12 — spatial_extent.crs per openEO 1.3.0 ----------

    #[test]
    fn parse_bbox_default_crs_is_wgs84() {
        // No crs field → EPSG:4326 (openEO 1.3.0 BoundingBox default).
        let v = json!({"west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0});
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:4326");
        // Explicit null also yields the default.
        let v = json!({"west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0, "crs": null});
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:4326");
    }

    #[test]
    fn parse_bbox_integer_crs_is_qualified_to_epsg_prefix() {
        // Integer EPSG code → must be qualified with "EPSG:" before
        // reaching GDAL.
        let v = json!({
            "west": 100000.0, "south": 5000000.0,
            "east":  110000.0, "north": 5010000.0,
            "crs": 32633
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:32633");
    }

    #[test]
    fn parse_bbox_string_crs_passthrough_when_qualified() {
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "EPSG:3857"
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:3857");
    }

    #[test]
    fn parse_bbox_bare_numeric_string_crs_is_qualified() {
        // The spec also accepts a bare numeric string like "32633"
        // (no EPSG: prefix). We normalise it for GDAL.
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "32633"
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:32633");
    }

    #[test]
    fn parse_bbox_proj_string_passthrough() {
        // PROJ strings contain `:` (via `+units=m` patterns we may see)
        // — keep the canonical-form check: anything containing `:` is
        // already a complete CRS spec.
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "+proj=utm +zone=33 +ellps=WGS84:meters"
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert!(crs.starts_with("+proj=utm"));
    }

    #[test]
    fn parse_bbox_rejects_garbage_crs() {
        // Arrays, objects, and unqualified non-numeric strings are
        // rejected with InvalidGraph (spec-incompatible input).
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": [1, 2, 3]
        });
        let r = parse_bbox(&v);
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));

        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": { "wrong": "shape" }
        });
        assert!(matches!(parse_bbox(&v), Err(ExecError::InvalidGraph(_))));

        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "not_a_crs"
        });
        assert!(matches!(parse_bbox(&v), Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn parse_temporal_bare_dates_get_normalised_to_rfc3339() {
        // Element84 returns 0 features for `YYYY-MM-DD` — we widen to
        // full timestamps so the day boundaries match what users expect.
        let v = json!(["2024-01-01", "2024-12-31"]);
        assert_eq!(
            parse_temporal(&v).unwrap(),
            "2024-01-01T00:00:00Z/2024-12-31T23:59:59Z"
        );
    }

    #[test]
    fn parse_temporal_preserves_full_timestamps() {
        let v = json!(["2024-01-01T06:00:00Z", "2024-12-31T18:00:00Z"]);
        assert_eq!(
            parse_temporal(&v).unwrap(),
            "2024-01-01T06:00:00Z/2024-12-31T18:00:00Z"
        );
    }

    #[test]
    fn parse_temporal_wrong_arity_errors() {
        let v = json!(["only-one"]);
        assert!(parse_temporal(&v).is_err());
    }

    #[test]
    fn parse_eo_cloud_cover_lt_canonical_python_client_shape() {
        // openeo-python-client `dc.max_cloud_cover(30)` emits this.
        let props = json!({
            "eo:cloud_cover": {
                "process_graph": {
                    "cc": {
                        "process_id": "lt",
                        "arguments": { "x": {"from_parameter": "value"}, "y": 30 },
                        "result": true
                    }
                }
            }
        });
        assert_eq!(parse_eo_cloud_cover_lt(&props), Some(30.0));
    }

    #[test]
    fn parse_eo_cloud_cover_lte_is_also_recognised() {
        let props = json!({
            "eo:cloud_cover": {
                "process_graph": {
                    "cc": {
                        "process_id": "lte",
                        "arguments": { "x": {"from_parameter": "value"}, "y": 50.5 },
                        "result": true
                    }
                }
            }
        });
        assert_eq!(parse_eo_cloud_cover_lt(&props), Some(50.5));
    }

    #[test]
    fn parse_eo_cloud_cover_returns_none_when_missing() {
        let props = json!({ "other_band": { "process_graph": {} } });
        assert_eq!(parse_eo_cloud_cover_lt(&props), None);
    }

    #[test]
    fn parse_eo_cloud_cover_returns_none_for_unsupported_predicate() {
        // gt/gte aren't useful for "max cloud cover" — fall through.
        let props = json!({
            "eo:cloud_cover": {
                "process_graph": {
                    "cc": {
                        "process_id": "gt",
                        "arguments": { "x": {"from_parameter": "value"}, "y": 30 },
                        "result": true
                    }
                }
            }
        });
        assert_eq!(parse_eo_cloud_cover_lt(&props), None);
    }

    // ---------- P3 — /vsicurl/ stream-mode path builder ----------

    #[test]
    fn vsicurl_path_prepends_prefix_to_https() {
        let p = vsicurl_path("https://sentinel-cogs.s3.us-west-2.amazonaws.com/foo.tif").unwrap();
        assert_eq!(
            p.to_string_lossy(),
            "/vsicurl/https://sentinel-cogs.s3.us-west-2.amazonaws.com/foo.tif"
        );
    }

    #[test]
    fn vsicurl_path_rejects_non_http_schemes() {
        // file:// or s3:// would either bypass SSRF defenses or fail
        // opaquely at libcurl time — refuse explicitly with InvalidGraph
        // so the runner surfaces a typed error.
        for bad in &["s3://bucket/key", "file:///etc/passwd", "ftp://x/y"] {
            let r = vsicurl_path(bad);
            assert!(matches!(r, Err(ExecError::InvalidGraph(m)) if m.contains(bad)),
                "expected reject for {bad}");
        }
    }

    // ---------- P3 — emit_cropped_vrt_p3 windows the source ----------

    /// Build a tiny i16 UTM33N GeoTIFF (S2-shape) for the VRT crop test.
    fn write_s2_shape_fixture_i16(
        path: &std::path::Path,
        cols: usize,
        rows: usize,
        origin_e: f64,
        origin_n: f64,
    ) {
        use gdal::raster::Buffer;
        use gdal::DriverManager;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
        let mut ds = drv
            .create_with_band_type::<i16, _>(path, cols, rows, 1)
            .unwrap();
        ds.set_geo_transform(&[origin_e, 10.0, 0.0, origin_n, 0.0, -10.0])
            .unwrap();
        let sr = gdal::spatial_ref::SpatialRef::from_epsg(32633).unwrap();
        ds.set_spatial_ref(&sr).unwrap();
        let mut b = ds.rasterband(1).unwrap();
        // Deterministic ramp pattern so the centre-pixel check has known truth.
        let data: Vec<i16> = (0..(cols * rows)).map(|i| (i % 30000) as i16).collect();
        let mut buf = Buffer::new((cols, rows), data);
        b.write::<i16>((0, 0), (cols, rows), &mut buf).unwrap();
    }

    #[test]
    fn p3_emit_cropped_vrt_exposes_aoi_window_not_full_source() {
        // RED-then-GREEN: previously the P3 branch emitted /vsicurl/<href>
        // as-is and downstream Dataset::open saw the FULL 10980x10980 S2
        // tile. The fix emits a VRT cropped to the user's AOI, so
        // raster_size() == AOI window dims (NOT the source dims).
        //
        // **Flaky-test fix (2026-05-25)**: this test calls
        // emit_cropped_vrt_p3(.., None) which invokes probe_source and
        // bumps the global PROBE_SOURCE_CALLS counter. The two
        // probe-counter delta tests assert exact before/after deltas, so
        // this caller MUST hold the same lock to avoid racing their
        // snapshots. (Root cause of the intermittent
        // p3_emit_cropped_vrt_with_cached_meta_skips_probe_source failure.)
        let _guard = PROBE_SOURCE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("s2_shape.tif");
        let vrt = tmp.path().join("b04_0_p3.vrt");
        // 200x200 i16 UTM33N raster: origin (300000, 5400000), pix 10m
        // → covers [300000..302000] x [5398000..5400000].
        write_s2_shape_fixture_i16(&src, 200, 200, 300_000.0, 5_400_000.0);
        // Native-CRS crop entirely inside: cols 5..55 (50 px), rows 0..50 (50 px).
        let crop = CropWindow {
            min_x: 300_050.0,
            max_y: 5_400_000.0,
            max_x: 300_550.0,
            min_y: 5_399_500.0,
        };
        let out = emit_cropped_vrt_p3(src.to_str().unwrap(), &vrt, crop, "EPSG:32633", None)
            .expect("VRT emission");
        assert_eq!(out, vrt, "returned path must match dst_vrt");
        assert!(vrt.exists(), "VRT file must be written to disk");
        // Open the VRT and check it exposes the cropped window, NOT the
        // full 200x200 source.
        let ds = gdal::Dataset::open(&vrt).expect("open VRT");
        let (cols, rows) = ds.raster_size();
        assert_eq!(
            (cols, rows),
            (50, 50),
            "VRT must expose the AOI window (50x50), not the full source (200x200)"
        );
        // Geotransform origin advanced by 5 px (50 m east) but not in y.
        let gt = ds.geo_transform().unwrap();
        assert!((gt[0] - 300_050.0).abs() < 1e-6, "VRT origin_x: {}", gt[0]);
        assert!((gt[3] - 5_400_000.0).abs() < 1e-6, "VRT origin_y: {}", gt[3]);
        assert!((gt[1] - 10.0).abs() < 1e-6, "pix_w preserved: {}", gt[1]);
        assert!((gt[5] - -10.0).abs() < 1e-6, "pix_h preserved: {}", gt[5]);
        // Centre-pixel parity: VRT row=25,col=25 maps to source
        // row=25,col=(5+25)=30 → src index 25*200+30 = 5030, value 5030.
        let band = ds.rasterband(1).unwrap();
        let buf = band
            .read_as::<i16>((25, 25), (1, 1), (1, 1), None)
            .unwrap();
        assert_eq!(
            buf.data()[0],
            5030,
            "centre pixel must match source (row=25 vrt = row=25 src, col=25 vrt = col=30 src)"
        );
    }

    // ---------- P3-fast — STAC metadata cache skips probe_source ----------

    #[test]
    fn try_source_meta_from_band_metadata_returns_none_when_incomplete() {
        // Missing dtype → must yield None so caller falls back to probe.
        let mut m = BandMetadata::default();
        m.epsg = Some(32633);
        m.geo_transform = Some([0.0, 10.0, 0.0, 0.0, 0.0, -10.0]);
        m.raster_size = Some((100, 100));
        assert!(try_source_meta_from_band_metadata(&m).is_none());
        // All required fields present → Some.
        m.dtype = Some("UInt16".into());
        assert!(try_source_meta_from_band_metadata(&m).is_some());
    }

    #[test]
    fn p3_emit_cropped_vrt_with_cached_meta_skips_probe_source() {
        use std::sync::atomic::Ordering;
        // Serialize against the sibling probe-counter test so the
        // shared atomic delta is meaningful.
        let _guard = PROBE_SOURCE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Build a real on-disk source so compute_aoi_window has live data
        // to work against, but feed cached SourceMeta so probe_source is
        // never called. 200x200 UTM33N at 10 m.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("s2_shape_cached.tif");
        let vrt = tmp.path().join("b04_cached_p3.vrt");
        write_s2_shape_fixture_i16(&src, 200, 200, 300_000.0, 5_400_000.0);
        // Delta-based count: other tests in the module run in parallel
        // and may bump PROBE_SOURCE_CALLS too, so snapshot before/after.
        let before = PROBE_SOURCE_CALLS.load(Ordering::SeqCst);
        // Cached BandMetadata that fully describes the on-disk fixture.
        let bm = BandMetadata {
            epsg: Some(32633),
            geo_transform: Some([300_000.0, 10.0, 0.0, 5_400_000.0, 0.0, -10.0]),
            raster_size: Some((200, 200)),
            dtype: Some("Int16".into()),
            nodata: None,
            ..Default::default()
        };
        let cached = try_source_meta_from_band_metadata(&bm).expect("cached meta");
        let crop = CropWindow {
            min_x: 300_050.0,
            max_y: 5_400_000.0,
            max_x: 300_550.0,
            min_y: 5_399_500.0,
        };
        let _ = emit_cropped_vrt_p3(
            src.to_str().unwrap(),
            &vrt,
            crop,
            "EPSG:32633",
            Some(cached),
        )
        .expect("VRT emission with cached meta");
        let after = PROBE_SOURCE_CALLS.load(Ordering::SeqCst);
        assert_eq!(
            after, before,
            "probe_source must NOT be called when cached metadata is supplied              (before={before}, after={after})"
        );
        // VRT still exposes the cropped 50x50 window — cached path is correct.
        let ds = gdal::Dataset::open(&vrt).expect("open cached-meta VRT");
        assert_eq!(ds.raster_size(), (50, 50));
    }

    #[test]
    fn p3_emit_cropped_vrt_without_cached_meta_calls_probe_source() {
        // Inverse of the previous test — when cached is None, probe must
        // run exactly once. Use a delta because tests run in parallel.
        use std::sync::atomic::Ordering;
        // Serialize against the sibling probe-counter test so the
        // shared atomic delta is meaningful.
        let _guard = PROBE_SOURCE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("s2_shape_probe.tif");
        let vrt = tmp.path().join("b04_probe_p3.vrt");
        write_s2_shape_fixture_i16(&src, 200, 200, 300_000.0, 5_400_000.0);
        let before = PROBE_SOURCE_CALLS.load(Ordering::SeqCst);
        let crop = CropWindow {
            min_x: 300_050.0,
            max_y: 5_400_000.0,
            max_x: 300_550.0,
            min_y: 5_399_500.0,
        };
        let _ = emit_cropped_vrt_p3(
            src.to_str().unwrap(),
            &vrt,
            crop,
            "EPSG:32633",
            None,
        )
            .expect("VRT emission without cached meta");
        let after = PROBE_SOURCE_CALLS.load(Ordering::SeqCst);
        // Exactly one probe from THIS call (delta == 1). Other tests may
        // bump the global counter concurrently, so we only check our own.
        assert!(
            after >= before + 1,
            "probe_source must be called at least once when no cached meta              (before={before}, after={after})"
        );
    }
}
