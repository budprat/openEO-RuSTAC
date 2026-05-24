//! Burn vector geometries into raster pixels.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Rasterize a GeoJSON file (or any OGR-readable vector source) into a
/// raster output at `output`. All pixels covered by any feature get
/// `burn_value`; pixels outside features get `no_data`.
///
/// Output dimensions are determined from `width × height` and the supplied
/// `(min_x, min_y, max_x, max_y)` bounding box (lon/lat for EPSG:4326).
///
/// Subprocess wrapper around `gdal_rasterize`.
///
pub fn rasterize(
    vector_src: &Path,
    output: &Path,
    width: usize,
    height: usize,
    bbox: (f64, f64, f64, f64),
    burn_value: f64,
    no_data: f64,
) -> Result<PathBuf> {
    let (min_x, min_y, max_x, max_y) = bbox;
    let mut cmd = Command::new("gdal_rasterize");
    cmd.arg("-burn").arg(format!("{burn_value}"))
        .arg("-a_nodata").arg(format!("{no_data}"))
        .arg("-ts").arg(format!("{width}")).arg(format!("{height}"))
        .arg("-te").arg(format!("{min_x}")).arg(format!("{min_y}"))
            .arg(format!("{max_x}")).arg(format!("{max_y}"))
        .arg("-of").arg("GTiff")
        .arg("-ot").arg("Float64")
        .arg("-init").arg(format!("{no_data}"))
        .arg(vector_src)
        .arg(output);
    let status = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Other("rasterize: gdal_rasterize not found on PATH".into())
        } else {
            Error::Other(format!("gdal_rasterize spawn: {e}"))
        }
    })?;
    if !status.status.success() {
        return Err(Error::Other(format!(
            "gdal_rasterize failed: {}",
            String::from_utf8_lossy(&status.stderr)
        )));
    }
    Ok(output.to_path_buf())
}

#[allow(dead_code)]
fn _command_used() -> Command {
    Command::new("dummy")
}
