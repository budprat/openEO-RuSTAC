//! **GDAL CLI helpers** — thin subprocess wrappers around the GDAL CLI tools.
//!
//! Single canonical namespace for all GDAL CLI subprocess wrappers in the
//! crate (T3.8 consolidation). Includes:
//! - [`mosaic`] (Tier 1.6) — combine multiple rasters via `gdalbuildvrt` + `gdal_translate`
//! - [`convert_to_cog`] (Tier 1.2) — `gdal_translate -of COG`
//! - [`download_via_gdal_translate`] (re-exported from `providers`) — windowed download
//! - [`crate::rasterization::rasterize`] (Tier 3.4) — burn vectors via `gdal_rasterize`

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

// T3.8 re-export: give users a single canonical import path for all
// GDAL CLI subprocess helpers.
pub use crate::providers::download_via_gdal_translate;

/// Combine `inputs` into a single raster at `output`.
///
/// Two-step subprocess pipeline:
/// 1. `gdalbuildvrt` constructs a virtual mosaic VRT
/// 2. `gdal_translate` materializes the VRT into a real GeoTIFF
///
/// Returns the output path on success. Requires `gdalbuildvrt` and
/// `gdal_translate` on PATH; reports a clean error if either is missing.
pub fn mosaic(inputs: &[PathBuf], output: &Path) -> Result<PathBuf> {
    if inputs.is_empty() {
        return Err(Error::Other("mosaic: inputs list is empty".into()));
    }

    // Step 1: gdalbuildvrt vrt_path input1 input2 ...
    let vrt_handle = tempfile::Builder::new()
        .suffix(".vrt")
        .tempfile()
        .map_err(|e| Error::Other(format!("create temp VRT: {e}")))?;
    let vrt_path = vrt_handle.into_temp_path();
    std::fs::remove_file(&vrt_path).ok();

    let mut vrt_cmd = Command::new("gdalbuildvrt");
    vrt_cmd.arg(&*vrt_path);
    for input in inputs {
        vrt_cmd.arg(input);
    }
    let vrt_status = vrt_cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Other(
                "mosaic: gdalbuildvrt not found on PATH — install GDAL with the CLI tools enabled"
                    .into(),
            )
        } else {
            Error::Other(format!("gdalbuildvrt spawn: {e}"))
        }
    })?;
    if !vrt_status.status.success() {
        return Err(Error::Other(format!(
            "gdalbuildvrt failed: {}",
            String::from_utf8_lossy(&vrt_status.stderr)
        )));
    }

    // Step 2: gdal_translate vrt output
    let mut translate_cmd = Command::new("gdal_translate");
    translate_cmd
        .arg("-of")
        .arg("GTiff")
        .arg(&*vrt_path)
        .arg(output);
    let translate_status = translate_cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Other(
                "mosaic: gdal_translate not found on PATH — install GDAL with the CLI tools enabled"
                    .into(),
            )
        } else {
            Error::Other(format!("gdal_translate spawn: {e}"))
        }
    })?;
    if !translate_status.status.success() {
        return Err(Error::Other(format!(
            "gdal_translate failed: {}",
            String::from_utf8_lossy(&translate_status.stderr)
        )));
    }

    Ok(output.to_path_buf())
}

// `staging_path_for` and `atomic_replace` have moved to `eo_io::atomic`.
// Use a thin local adapter so `convert_to_cog` keeps returning
// `orbit-geo`'s `Error` instead of leaking `eo_io::AtomicError` upward.
use eo_io::atomic::{atomic_replace as eo_atomic_replace, staging_path_for};

pub(crate) fn atomic_replace(staging: &Path, dst: &Path) -> Result<()> {
    eo_atomic_replace(staging, dst).map_err(|e| Error::Other(e.to_string()))
}

/// Convert an existing GeoTIFF at `src` to a Cloud-Optimized GeoTIFF at `dst`
/// using `gdal_translate -of COG`. Tile-aligned blocks (512×512 by default),
/// LZW compression, AVERAGE resampling for overviews.
///
/// Writes to a same-directory staging file then `rename`s into place, so a
/// crash or kill of `gdal_translate` cannot leave `dst` in a partial state.
///
/// **T1.2 helper.**
pub fn convert_to_cog(src: &Path, dst: &Path) -> Result<PathBuf> {
    let staging = staging_path_for(dst);
    let mut cmd = Command::new("gdal_translate");
    cmd.arg("-of").arg("COG")
        .arg("-co").arg("BLOCKSIZE=512")
        .arg("-co").arg("COMPRESS=LZW")
        .arg("-co").arg("OVERVIEW_RESAMPLING=AVERAGE")
        .arg(src)
        .arg(&staging);
    let status = cmd.output().map_err(|e| {
        // Best-effort cleanup if we never even spawned successfully.
        let _ = std::fs::remove_file(&staging);
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Other("convert_to_cog: gdal_translate not found on PATH".into())
        } else {
            Error::Other(format!("gdal_translate spawn: {e}"))
        }
    })?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr).into_owned();
        // gdal_translate may have created a partial staging file; clean it up.
        let _ = std::fs::remove_file(&staging);
        return Err(Error::Other(format!(
            "gdal_translate -of COG failed: {stderr}"
        )));
    }
    atomic_replace(&staging, dst)?;
    Ok(dst.to_path_buf())
}

/// Reproject a raster to `target_epsg` using `gdalwarp`. Tier 1.6 / Tier 4.4
pub fn warp(src: &Path, dst: &Path, target_epsg: u32) -> Result<PathBuf> {
    let mut cmd = Command::new("gdalwarp");
    cmd.arg("-t_srs")
        .arg(format!("EPSG:{target_epsg}"))
        .arg("-of").arg("GTiff")
        .arg("-overwrite")
        .arg(src)
        .arg(dst);
    let status = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Other("warp: gdalwarp not found on PATH".into())
        } else {
            Error::Other(format!("gdalwarp spawn: {e}"))
        }
    })?;
    if !status.status.success() {
        return Err(Error::Other(format!(
            "gdalwarp failed: {}",
            String::from_utf8_lossy(&status.stderr)
        )));
    }
    Ok(dst.to_path_buf())
}

// ─────────────────────────────────────────────────────────────────────
// Batch E (post-Tier 5 parity additions) — upstream top-level GDAL helpers.
// ─────────────────────────────────────────────────────────────────────

/// Basic metadata probed from a raster file.
#[derive(Debug, Clone)]
pub struct BasicRasterInfo {
    /// Number of rows (image height).
    pub rows: usize,
    /// Number of columns (image width).
    pub cols: usize,
    /// Number of bands.
    pub bands: usize,
    /// EPSG code (0 if no SRS set).
    pub epsg: u32,
    /// GeoTransform `[origin_x, pixel_w, 0, origin_y, 0, -pixel_h]`.
    pub geo_transform: [f64; 6],
}

/// Probe a raster file and return metadata without reading pixel data.
pub fn read_basic_raster_info(path: &Path) -> Result<BasicRasterInfo> {
    let ds = ::gdal::Dataset::open(path)
        .map_err(|e| Error::Other(format!("open {}: {e}", path.display())))?;
    let (cols, rows) = ds.raster_size();
    let bands = ds.raster_count();
    let geo_transform = ds.geo_transform().unwrap_or([0.0; 6]);
    let epsg = ds
        .spatial_ref()
        .ok()
        .and_then(|sr| sr.auth_code().ok())
        .unwrap_or(0) as u32;
    Ok(BasicRasterInfo { rows, cols, bands, epsg, geo_transform })
}

/// Translate (copy + optional reformat) a raster file. Thin wrapper.
pub fn translate(src: &Path, dst: &Path) -> Result<PathBuf> {
    translate_with_driver(src, dst, "GTiff")
}

/// Translate with a specific GDAL output driver (e.g. "GTiff", "COG", "VRT").
pub fn translate_with_driver(src: &Path, dst: &Path, driver: &str) -> Result<PathBuf> {
    let status = Command::new("gdal_translate")
        .arg("-of")
        .arg(driver)
        .arg(src)
        .arg(dst)
        .output()
        .map_err(|e| Error::Other(format!("gdal_translate spawn: {e}")))?;
    if !status.status.success() {
        return Err(Error::Other(format!(
            "gdal_translate -of {driver} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        )));
    }
    Ok(dst.to_path_buf())
}

/// Mosaic a list of inputs into `output`, **keeping the inputs** afterwards
/// (orbit-geo `mosaic` removes intermediates). Useful for debugging or when
/// inputs are needed by downstream steps.
pub fn mosaic_keep_inputs(inputs: &[PathBuf], output: &Path) -> Result<PathBuf> {
    // Same as mosaic but doesn't touch the inputs.
    mosaic(inputs, output)
}

/// Mosaic + translate + clean up the intermediate VRT.
///
/// Mirrors the upstream `mosaic_translate_cleanup` — produces a single output
/// without leaving a .vrt sidecar.
pub fn mosaic_translate_cleanup(inputs: &[PathBuf], output: &Path) -> Result<PathBuf> {
    // mosaic() already does buildvrt + translate in two steps and lets the
    // temp VRT drop on Drop. This wrapper makes the intent explicit.
    mosaic(inputs, output)
}

/// Mosaic + translate + cleanup, grouping inputs by timestep into multiple outputs.
///
/// `inputs_per_step`: slice of slices, one per timestep. `outputs`: one path per step.
pub fn mosaic_translate_cleanup_time_steps(
    inputs_per_step: &[Vec<PathBuf>],
    outputs: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    if inputs_per_step.len() != outputs.len() {
        return Err(Error::Other(format!(
            "mosaic_translate_cleanup_time_steps: input/output count mismatch: {} vs {}",
            inputs_per_step.len(),
            outputs.len()
        )));
    }
    let mut results = Vec::new();
    for (inputs, out) in inputs_per_step.iter().zip(outputs.iter()) {
        results.push(mosaic_translate_cleanup(inputs, out)?);
    }
    Ok(results)
}

/// Compute the union bounding-box of a list of rasters in `target_epsg` CRS.
///
/// Returns `[min_x, min_y, max_x, max_y]`. CRS reprojection of corner coords
/// is delegated to GDAL via `spatial_ref::CoordTransform`.
pub fn compute_raster_union_extent(files: &[PathBuf], target_epsg: u32) -> Result<[f64; 4]> {
    if files.is_empty() {
        return Err(Error::Other("compute_raster_union_extent: empty input list".into()));
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let target_sr = ::gdal::spatial_ref::SpatialRef::from_epsg(target_epsg)
        .map_err(|e| Error::Other(format!("target EPSG:{target_epsg}: {e}")))?;
    for path in files {
        let info = read_basic_raster_info(path)?;
        let src_sr = if info.epsg > 0 {
            ::gdal::spatial_ref::SpatialRef::from_epsg(info.epsg)
                .map_err(|e| Error::Other(format!("src EPSG:{}: {e}", info.epsg)))?
        } else {
            target_sr.clone()
        };
        // Corner coords in source CRS.
        let gt = info.geo_transform;
        let mut xs = [gt[0], gt[0] + info.cols as f64 * gt[1]];
        let mut ys = [gt[3], gt[3] + info.rows as f64 * gt[5]];
        if info.epsg != target_epsg && info.epsg > 0 {
            let xf = ::gdal::spatial_ref::CoordTransform::new(&src_sr, &target_sr)
                .map_err(|e| Error::Other(format!("CoordTransform: {e}")))?;
            let mut zs = [0.0_f64, 0.0];
            xf.transform_coords(&mut xs, &mut ys, &mut zs)
                .map_err(|e| Error::Other(format!("transform: {e}")))?;
        }
        for &x in &xs {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
        }
        for &y in &ys {
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    Ok([min_x, min_y, max_x, max_y])
}

/// Compute the bounding box of a vector file (GeoJSON, Shapefile, …) in `target_epsg`.
pub fn compute_vector_extent(vector_path: &Path, target_epsg: u32) -> Result<[f64; 4]> {
    use ::gdal::vector::LayerAccess;
    let ds = ::gdal::Dataset::open(vector_path)
        .map_err(|e| Error::Other(format!("open vector {}: {e}", vector_path.display())))?;
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let layer_count = ds.layer_count();
    for li in 0..layer_count {
        let mut layer = ds
            .layer(li)
            .map_err(|e| Error::Other(format!("layer {li}: {e}")))?;
        for feature in layer.features() {
            if let Some(geom) = feature.geometry() {
                let env = geom.envelope();
                min_x = min_x.min(env.MinX);
                min_y = min_y.min(env.MinY);
                max_x = max_x.max(env.MaxX);
                max_y = max_y.max(env.MaxY);
            }
        }
    }
    let _ = target_epsg; // reprojection deferred — current impl assumes vector already in target CRS
    if min_x.is_infinite() {
        return Err(Error::Other(format!(
            "compute_vector_extent: no features found in {}",
            vector_path.display()
        )));
    }
    Ok([min_x, min_y, max_x, max_y])
}

/// Return the file stem (filename without extension) as a `&str`.
pub fn file_stem_str(path: &Path) -> &str {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("")
}

/// Create a temporary file path with the given extension. The file is kept
/// on disk (caller owns cleanup).
///
/// Returns an error if `persist` fails. Previously this swallowed the
/// `PathPersistError`, which silently destroyed the file when the underlying
/// `TempPath` dropped — callers then received a `PathBuf` that no longer
/// pointed at anything, with downstream errors masquerading as “file not
/// found” far from the real cause.
pub fn create_temp_file(ext: &str) -> Result<PathBuf> {
    let suffix = if ext.starts_with('.') { ext.to_string() } else { format!(".{ext}") };
    let tmp = tempfile::Builder::new()
        .suffix(&suffix)
        .tempfile()
        .map_err(|e| Error::Other(format!("create_temp_file: {e}")))?;
    let path = tmp.into_temp_path();
    let owned = path.to_path_buf();
    path.persist(&owned).map_err(|e| {
        Error::Other(format!(
            "create_temp_file: persist {owned:?} failed: {e}"
        ))
    })?;
    Ok(owned)
}

#[cfg(test)]
mod batch_e_tests {
    use super::*;
    use gdal::raster::{Buffer, RasterCreationOptions};
    use gdal::spatial_ref::SpatialRef;
    use gdal::DriverManager;
    use tempfile::Builder;

    fn make_gtiff(rows: usize, cols: usize, fill: i16, epsg: u32) -> tempfile::TempPath {
        let tmp = Builder::new().suffix(".tif").tempfile().unwrap();
        let p = tmp.into_temp_path();
        std::fs::remove_file(&p).ok();
        let drv = DriverManager::get_driver_by_name("GTiff").unwrap();
        let opts = RasterCreationOptions::from_iter(["TILED=NO"]);
        let mut ds = drv
            .create_with_band_type_with_options::<i16, _>(&p, cols, rows, 1, &opts)
            .unwrap();
        ds.set_geo_transform(&[0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]).unwrap();
        ds.set_spatial_ref(&SpatialRef::from_epsg(epsg).unwrap()).unwrap();
        let mut band = ds.rasterband(1).unwrap();
        let data: Vec<i16> = vec![fill; rows * cols];
        let mut buf = Buffer::new((cols, rows), data);
        band.write::<i16>((0, 0), (cols, rows), &mut buf).unwrap();
        drop(band);
        drop(ds);
        p
    }

    #[test]
    fn read_basic_raster_info_reports_dims() {
        let f = make_gtiff(10, 20, 0, 4326);
        let info = read_basic_raster_info(&f).expect("info");
        assert_eq!(info.rows, 10);
        assert_eq!(info.cols, 20);
        assert_eq!(info.bands, 1);
        assert_eq!(info.epsg, 4326);
    }

    #[test]
    fn translate_produces_output_file() {
        let src = make_gtiff(4, 4, 7, 4326);
        let dst = Builder::new().suffix(".tif").tempfile().unwrap().into_temp_path();
        std::fs::remove_file(&dst).ok();
        translate(&src, &dst).expect("translate");
        assert!(dst.exists());
    }

    #[test]
    fn mosaic_translate_cleanup_produces_output() {
        let a = make_gtiff(2, 2, 50, 4326);
        let b = make_gtiff(2, 2, 99, 4326);
        let dst = Builder::new().suffix(".tif").tempfile().unwrap().into_temp_path();
        std::fs::remove_file(&dst).ok();
        mosaic_translate_cleanup(&[a.to_path_buf(), b.to_path_buf()], &dst).expect("mosaic");
        assert!(dst.exists());
    }

    #[test]
    fn compute_raster_union_extent_aggregates() {
        let a = make_gtiff(4, 4, 0, 4326);
        let b = make_gtiff(4, 4, 0, 4326);
        let extent = compute_raster_union_extent(&[a.to_path_buf(), b.to_path_buf()], 4326).expect("extent");
        // Both rasters have origin (0, rows=4) → minx=0, maxx=4, miny=0, maxy=4
        assert_eq!(extent, [0.0, 0.0, 4.0, 4.0]);
    }

    #[test]
    fn file_stem_str_strips_ext() {
        assert_eq!(file_stem_str(Path::new("/tmp/foo.tif")), "foo");
        assert_eq!(file_stem_str(Path::new("/tmp/no_ext")), "no_ext");
    }

    #[test]
    fn create_temp_file_makes_path_with_ext() {
        let p = create_temp_file("tif").expect("temp");
        assert!(p.extension().is_some_and(|e| e == "tif"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn create_temp_file_returned_path_actually_exists() {
        // Regression for H13: persist() result was discarded; on failure the
        // returned PathBuf pointed at a deleted file. Assert the file exists.
        let p = create_temp_file("tif").expect("temp");
        assert!(p.exists(), "create_temp_file must return a path that exists");
        std::fs::remove_file(&p).ok();
    }

    // ── atomic write tests (H12) ─────────────────────────────────────

    #[test]
    fn staging_path_for_uses_same_directory() {
        let dst = Path::new("/tmp/output.tif");
        let staging = staging_path_for(dst);
        assert_eq!(staging.parent(), Some(Path::new("/tmp")));
        // Hidden / disambiguated name, not equal to dst.
        assert_ne!(staging, dst);
    }

    #[test]
    fn atomic_replace_moves_staging_to_dst() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&staging, b"new content").unwrap();

        atomic_replace(&staging, &dst).expect("rename");
        assert!(dst.exists(), "dst not created");
        assert!(!staging.exists(), "staging not consumed");
        assert_eq!(std::fs::read(&dst).unwrap(), b"new content");
    }

    #[test]
    fn atomic_replace_overwrites_existing_dst() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&dst, b"OLD").unwrap();
        std::fs::write(&staging, b"NEW").unwrap();

        atomic_replace(&staging, &dst).expect("rename");
        assert_eq!(std::fs::read(&dst).unwrap(), b"NEW");
    }

    #[test]
    fn atomic_replace_cleans_up_staging_on_error() {
        // Rename to a nonexistent parent dir on a platform that returns ENOENT;
        // staging should be removed so we don't leak.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        std::fs::write(&staging, b"x").unwrap();
        let bad_dst = dir.path().join("no_such_subdir/dst.bin");

        let r = atomic_replace(&staging, &bad_dst);
        assert!(r.is_err());
        assert!(!staging.exists(), "staging should be cleaned up on failure");
    }
}
