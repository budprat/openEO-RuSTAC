//! Microbench: compare subprocess `gdal_translate` vs in-process `gdal::Dataset`
//! on a single Sentinel-2 COG. Records phase timings.
//!
//! Usage:
//!   cargo run --release -p orbit-geo --example bench_download_one_cog -- <s3-href> <out-dir>
//!
//! Example href:
//!   https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/33/U/WP/2024/6/S2A_33UWP_20240603_0_L2A/B04.tif
//!
//! Bbox is hard-coded to Wien (EPSG:4326): west=16.10 south=48.10 east=16.60 north=48.40.
//! Both paths target the SAME crop window so subprocess vs in-process numbers are comparable.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
use gdal::{Dataset, DriverManager};
use orbit_geo::providers::{configure_anonymous_s3, vsi_rewrite, CropWindow};

// Wien bbox in EPSG:4326 — identical to apps/orbit-openeo/examples/*.json.
const WEST: f64 = 16.10;
const SOUTH: f64 = 48.10;
const EAST: f64 = 16.60;
const NORTH: f64 = 48.40;
const CROP_CRS: &str = "EPSG:4326";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let href = args.next().ok_or("missing <s3-href> argument")?;
    let out_dir = args.next().ok_or("missing <out-dir> argument")?;
    std::fs::create_dir_all(&out_dir)?;

    configure_anonymous_s3();

    let crop = CropWindow {
        min_x: WEST,
        max_y: NORTH,
        max_x: EAST,
        min_y: SOUTH,
    };

    eprintln!("=== Path 1: subprocess gdal_translate ===");
    let dst1 = Path::new(&out_dir).join("subprocess.tif");
    let t0 = Instant::now();
    let argv = orbit_geo::providers::build_gdal_translate_argv(
        &vsi_rewrite(&href),
        &dst1,
        Some(crop),
        Some(CROP_CRS),
    );
    let status = Command::new(&argv[0]).args(&argv[1..]).status()?;
    let subprocess_total = t0.elapsed();
    if !status.success() {
        return Err(format!("gdal_translate exited {status}").into());
    }
    let dst1_size = std::fs::metadata(&dst1)?.len();
    eprintln!("  subprocess_total = {:>8.2} ms", subprocess_total.as_secs_f64() * 1000.0);
    eprintln!("  output_bytes     = {dst1_size}");

    eprintln!();
    eprintln!("=== Path 2: in-process gdal::Dataset ===");
    let dst2 = Path::new(&out_dir).join("inprocess.tif");
    let t = bench_in_process(&href, &dst2, crop, CROP_CRS)?;
    let dst2_size = std::fs::metadata(&dst2)?.len();

    eprintln!("  open_vsicurl     = {:>8.2} ms", t.open_ms);
    eprintln!("  s3_io_decode     = {:>8.2} ms", t.read_ms);
    eprintln!("  encode_gtiff     = {:>8.2} ms", t.encode_ms);
    eprintln!("  inprocess_total  = {:>8.2} ms", t.total_ms);
    eprintln!("  output_bytes     = {dst2_size}");

    eprintln!();
    eprintln!("=== Summary ===");
    let sub_ms = subprocess_total.as_secs_f64() * 1000.0;
    eprintln!("  subprocess  = {sub_ms:>8.2} ms");
    eprintln!("  in-process  = {:>8.2} ms", t.total_ms);
    let delta = sub_ms - t.total_ms;
    eprintln!("  delta       = {delta:>8.2} ms (positive = subprocess slower)");
    if sub_ms > 0.0 {
        let ratio = (delta / sub_ms) * 100.0;
        eprintln!("  overhead    = {ratio:>8.2} % of subprocess wall-clock");
    }
    Ok(())
}

struct PhaseTimings {
    open_ms: f64,
    read_ms: f64,
    encode_ms: f64,
    total_ms: f64,
}

/// In-process equivalent of `gdal_translate -projwin ... -projwin_srs EPSG:4326`.
///
/// Phase split:
///   T0 → T1: `Dataset::open(/vsicurl/<href>)`          (HTTP HEAD + COG header read)
///   T1 → T2: `band.read_as::<u16>(window, ...)`        (S3 range reads + libtiff decode)
///   T2 → T3: MEM dataset → `create_copy` to GeoTIFF    (encode)
fn bench_in_process(
    href: &str,
    dst: &Path,
    crop: CropWindow,
    crop_crs: &str,
) -> Result<PhaseTimings, Box<dyn std::error::Error>> {
    let total_t0 = Instant::now();

    // ---- Phase 1: open --------------------------------------------------
    let t0 = Instant::now();
    let vsi = vsi_rewrite(href);
    let src = Dataset::open(&vsi)?;
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Resolve source CRS + geo-transform once.
    let src_gt = src.geo_transform()?;
    let mut src_sr = src.spatial_ref()?;
    let mut crop_sr = SpatialRef::from_definition(crop_crs)?;
    // Force lon/lat (x,y) ordering on both ends so CoordTransform does NOT
    // silently reorder our coords. GDAL 3.x default for EPSG:4326 is lat/lon.
    src_sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    crop_sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);

    // Project the crop bbox into the source CRS. CoordTransform requires
    // a flat (xs, ys) layout. Use all four corners so the projected envelope
    // covers the full window even after non-affine reprojection.
    let xform = CoordTransform::new(&crop_sr, &src_sr)?;
    let mut xs = [crop.min_x, crop.max_x, crop.min_x, crop.max_x];
    let mut ys = [crop.max_y, crop.max_y, crop.min_y, crop.min_y];
    let mut zs = [0.0_f64; 4];
    xform.transform_coords(&mut xs, &mut ys, &mut zs)?;
    let src_min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let src_max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let src_min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let src_max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    // Map source-CRS bbox to pixel window via the inverse geo-transform.
    // gt = [origin_x, pixel_w, 0, origin_y, 0, pix_h]  (pix_h negative for S2)
    let (origin_x, pix_w, origin_y, pix_h) = (src_gt[0], src_gt[1], src_gt[3], src_gt[5]);
    let col_min = ((src_min_x - origin_x) / pix_w).floor() as isize;
    let col_max = ((src_max_x - origin_x) / pix_w).ceil() as isize;
    // For pix_h < 0: rows increase as Y decreases. (Y - origin_Y)/pix_h is positive.
    let row_a = ((src_max_y - origin_y) / pix_h).floor() as isize;
    let row_b = ((src_min_y - origin_y) / pix_h).ceil() as isize;
    let row_min = row_a.min(row_b);
    let row_max = row_a.max(row_b);
    let (src_cols_total, src_rows_total) = src.raster_size();
    let col_off = col_min.max(0).min(src_cols_total as isize) as usize;
    let row_off = row_min.max(0).min(src_rows_total as isize) as usize;
    let col_end = col_max.max(0).min(src_cols_total as isize) as usize;
    let row_end = row_max.max(0).min(src_rows_total as isize) as usize;
    let cols = col_end.saturating_sub(col_off);
    let rows = row_end.saturating_sub(row_off);
    eprintln!(
        "  src_size         = {src_cols_total} x {src_rows_total}; window col={col_off} row={row_off} w={cols} h={rows}"
    );
    if cols == 0 || rows == 0 {
        return Err(format!(
            "bench window degenerate: src_bbox=({src_min_x:.1}..{src_max_x:.1}, {src_min_y:.1}..{src_max_y:.1}) \
             origin=({origin_x},{origin_y}) pix=({pix_w},{pix_h})"
        ).into());
    }

    // ---- Phase 2: read window -----------------------------------------------
    let t1 = Instant::now();
    let band = src.rasterband(1)?;
    // gdal 0.19: read_as<T>((col_off, row_off), (window_cols, window_rows), (buf_cols, buf_rows), resample)
    let buf: Buffer<u16> = band.read_as::<u16>(
        (col_off as isize, row_off as isize),
        (cols, rows),
        (cols, rows),
        None,
    )?;
    let read_ms = t1.elapsed().as_secs_f64() * 1000.0;

    // ---- Phase 3: encode ----------------------------------------------------
    let t2 = Instant::now();
    let mem_drv = DriverManager::get_driver_by_name("MEM")?;
    let mut mem_ds = mem_drv.create_with_band_type::<u16, _>("", cols, rows, 1)?;
    // Output geo-transform: same pixel size, origin shifted by window offset.
    let out_gt = [
        origin_x + col_off as f64 * pix_w,
        pix_w,
        0.0,
        origin_y + row_off as f64 * pix_h,
        0.0,
        pix_h,
    ];
    mem_ds.set_geo_transform(&out_gt)?;
    mem_ds.set_spatial_ref(&src_sr)?;
    {
        let mut b = mem_ds.rasterband(1)?;
        let data_vec: Vec<u16> = buf.data().to_vec();
        let mut out_buf = Buffer::new((cols, rows), data_vec);
        b.write::<u16>((0, 0), (cols, rows), &mut out_buf)?;
    }
    let gtiff_drv = DriverManager::get_driver_by_name("GTiff")?;
    let opts = RasterCreationOptions::from_iter(["COMPRESS=LZW", "TILED=YES"]);
    let copy = mem_ds.create_copy(&gtiff_drv, dst, &opts)?;
    drop(copy);
    let encode_ms = t2.elapsed().as_secs_f64() * 1000.0;

    let total_ms = total_t0.elapsed().as_secs_f64() * 1000.0;
    Ok(PhaseTimings { open_ms, read_ms, encode_ms, total_ms })
}
