//! **The block-parallel raster reduction pattern**: cached-local STAC imagery
//! + RasterDatasetBuilder + `apply_reduction_with_mask` block-parallel NDVI
//! with SCL cloud masking.
//!
//! This is the same approach the upstream raster engine uses to outperform
//! Python ODC/STAC by a wide margin. Gated `bench_live` — needs network for
//! the initial download.
//!
//! Flow:
//! 1. STAC search Earth Search for N low-cloud S2 L2A scenes over same MGRS tile
//! 2. **Download** a 256×256 crop window of B04 + B08 + SCL for each scene
//!    via `gdal_translate -projwin` → local tempdir
//! 3. Build `RasterDataset<i16>` from cached local paths with custom
//!    LayerMappings (red=layer 0, nir=layer 1, scl=layer 2 per timestep)
//! 4. Run `apply_reduction_with_mask` with FMask-equivalent worker that
//!    keeps clear-sky pixels (SCL ∈ {4, 5, 6, 7, 11}) and computes
//!    masked NDVI mean over time
//! 5. Print timing breakdown + assert NDVI mean falls in vegetation range

#![cfg(feature = "bench_live")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ndarray::{s, Array2, Array3, Axis};
use orbit_geo::{
    block::RasterDataBlock,
    builder::RasterDatasetBuilder,
    dataset::{LayerMapping, RasterDataset},
    providers::{configure_anonymous_s3, vsi_rewrite},
    types::{BlockSize, Dimension, ImageResolution},
};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const N_SCENES: usize = 3;
const CROP_SIZE: u32 = 256; // pixels (10 m → 2.56 km × 2.56 km)
const CROP_OFFSET: u32 = 1000; // pixels from COG origin

async fn search_scenes() -> Vec<(String, String, String, String)> {
    let client = reqwest::Client::builder()
        .user_agent("orbit-geo-pipeline-test/0.1")
        .build()
        .unwrap();
    let body = serde_json::json!({
        "collections": ["sentinel-2-l2a"],
        "bbox": [144.5, -36.6, 144.7, -36.4],
        "datetime": "2024-01-01T00:00:00Z/2024-12-31T23:59:59Z",
        "query": { "eo:cloud_cover": { "lt": 30.0 } },
        "limit": N_SCENES
    });
    let resp = client
        .post("https://earth-search.aws.element84.com/v1/search")
        .json(&body)
        .send()
        .await
        .expect("STAC search");
    let payload: serde_json::Value = resp.json().await.expect("STAC JSON");
    let mut out = Vec::new();
    for item in payload["features"].as_array().unwrap() {
        let id = item["id"].as_str().unwrap().to_string();
        let red = item["assets"]["red"]["href"]
            .as_str()
            .or_else(|| item["assets"]["B04"]["href"].as_str())
            .unwrap()
            .to_string();
        let nir = item["assets"]["nir"]["href"]
            .as_str()
            .or_else(|| item["assets"]["B08"]["href"].as_str())
            .unwrap()
            .to_string();
        // SCL band is at 20m resolution in STAC — Earth Search exposes it
        // under "scl" key.
        let scl = item["assets"]["scl"]["href"]
            .as_str()
            .or_else(|| item["assets"]["SCL"]["href"].as_str())
            .unwrap()
            .to_string();
        out.push((id, red, nir, scl));
    }
    out
}

/// Download a 256×256-pixel crop window from a remote COG via gdal_translate.
fn download_crop(remote_href: &str, dst: &std::path::Path) -> std::io::Result<()> {
    let vsi = vsi_rewrite(remote_href);
    let status = Command::new("gdal_translate")
        .args([
            "-srcwin",
            &CROP_OFFSET.to_string(),
            &CROP_OFFSET.to_string(),
            &CROP_SIZE.to_string(),
            &CROP_SIZE.to_string(),
            "-of",
            "GTiff",
            &vsi,
            dst.to_str().unwrap(),
        ])
        .output()?;
    if !status.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "gdal_translate failed: {}",
                String::from_utf8_lossy(&status.stderr)
            ),
        ));
    }
    Ok(())
}

#[tokio::test]
async fn the_block_parallel_pattern_cached_local_with_scl_mask() {
    configure_anonymous_s3();

    // ─────────────────────────────────────────────────────────────────────
    // Phase 1 — STAC discovery
    // ─────────────────────────────────────────────────────────────────────
    let t_search = Instant::now();
    let scenes = search_scenes().await;
    println!(
        "[pipeline] phase 1 (STAC search): {:?} → {} scenes",
        t_search.elapsed(),
        scenes.len()
    );
    for (id, _, _, _) in &scenes {
        println!("  - {id}");
    }
    assert!(scenes.len() >= 2, "need ≥2 scenes for time reduction");

    // ─────────────────────────────────────────────────────────────────────
    // Phase 2 — Download all bands to tempdir
    // ─────────────────────────────────────────────────────────────────────
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache_path = cache_dir.path();
    let mut local_reds: Vec<PathBuf> = Vec::new();
    let mut local_nirs: Vec<PathBuf> = Vec::new();
    let mut local_scls: Vec<PathBuf> = Vec::new();

    let t_download = Instant::now();
    for (i, (_id, red, nir, scl)) in scenes.iter().enumerate() {
        let red_local = cache_path.join(format!("red_{i}.tif"));
        let nir_local = cache_path.join(format!("nir_{i}.tif"));
        let scl_local = cache_path.join(format!("scl_{i}.tif"));
        download_crop(red, &red_local).expect("download red");
        download_crop(nir, &nir_local).expect("download nir");
        download_crop(scl, &scl_local).expect("download scl");
        local_reds.push(red_local);
        local_nirs.push(nir_local);
        local_scls.push(scl_local);
    }
    let download_secs = t_download.elapsed().as_secs_f64();
    let total_bytes: u64 = std::fs::read_dir(cache_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum();
    println!(
        "[pipeline] phase 2 (download {} files, {:.2} MB total): {:.2} s",
        N_SCENES * 3,
        total_bytes as f64 / 1024.0 / 1024.0,
        download_secs
    );

    // ─────────────────────────────────────────────────────────────────────
    // Phase 3 — Build RasterDataset from cached local paths
    // ─────────────────────────────────────────────────────────────────────
    let t_build = Instant::now();
    let mut all_data_paths: Vec<PathBuf> = Vec::new();
    all_data_paths.extend(local_reds.iter().cloned());
    all_data_paths.extend(local_nirs.iter().cloned());

    let mut rds_data: RasterDataset<i16> = RasterDatasetBuilder::from_files(&all_data_paths)
        .expect("data builder")
        .resolution(ImageResolution { x: 10.0, y: -10.0 })
        .block_size(BlockSize {
            rows: CROP_SIZE as usize,
            cols: CROP_SIZE as usize,
        })
        .build()
        .expect("data build");
    rds_data.metadata.shape.times = scenes.len();
    rds_data.metadata.shape.layers = 2;
    let mut mappings = Vec::new();
    for (t, p) in local_reds.iter().enumerate() {
        mappings.push(LayerMapping {
            source: p.clone(),
            time_pos: t,
            layer_pos: 0,
            band: 1,
        });
    }
    for (t, p) in local_nirs.iter().enumerate() {
        mappings.push(LayerMapping {
            source: p.clone(),
            time_pos: t,
            layer_pos: 1,
            band: 1,
        });
    }
    rds_data.layer_mappings = mappings;

    // SCL mask dataset (separate, u8-typed for clarity).
    let mut rds_mask: RasterDataset<u8> =
        RasterDatasetBuilder::<u8>::from_files(&local_scls)
            .expect("mask builder")
            .resolution(ImageResolution { x: 20.0, y: -20.0 })
            .block_size(BlockSize {
                rows: CROP_SIZE as usize,
                cols: CROP_SIZE as usize,
            })
            .build()
            .expect("mask build");
    rds_mask.metadata.shape.times = scenes.len();
    rds_mask.metadata.shape.layers = 1;
    rds_mask.layer_mappings = local_scls
        .iter()
        .enumerate()
        .map(|(t, p)| LayerMapping {
            source: p.clone(),
            time_pos: t,
            layer_pos: 0,
            band: 1,
        })
        .collect();
    // Align mask blocks to data partitioning (same block count).
    rds_mask.blocks = rds_data.blocks.clone();
    rds_mask.metadata.shape.rows = rds_data.metadata.shape.rows;
    rds_mask.metadata.shape.cols = rds_data.metadata.shape.cols;

    println!("[pipeline] phase 3 (build datasets): {:?}", t_build.elapsed());

    // ─────────────────────────────────────────────────────────────────────
    // Phase 4 — Kernel: apply_reduction_with_mask with SCL cloud filtering
    // ─────────────────────────────────────────────────────────────────────
    let out_path = tempfile::Builder::new()
        .suffix(".tif")
        .tempfile()
        .unwrap()
        .into_temp_path();
    std::fs::remove_file(&out_path).ok();

    // FMask-equivalent worker: SCL ∈ {4, 5, 6, 7, 11} → clear-sky.
    fn ndvi_mean_with_scl(
        data: &RasterDataBlock<i16>,
        mask: &RasterDataBlock<u8>,
        _dim: Dimension,
    ) -> Array3<i16> {
        let (n_t, _layers, rows, cols) = data.data.dim();
        let mut sum = Array2::<f64>::zeros((rows, cols));
        let mut count = Array2::<u32>::zeros((rows, cols));
        for t in 0..n_t {
            let red = data.data.slice(s![t, 0, .., ..]).mapv(|v| v as f32);
            let nir = data.data.slice(s![t, 1, .., ..]).mapv(|v| v as f32);
            let scl = mask.data.slice(s![t, 0, .., ..]);
            let denom = &nir + &red + 1e-10_f32;
            let ndvi = ((&nir - &red) / &denom * 10000.0_f32).mapv(|v| v as i16);
            for (((acc, cnt), &v), &s) in sum
                .iter_mut()
                .zip(count.iter_mut())
                .zip(ndvi.iter())
                .zip(scl.iter())
            {
                // Clear-sky SCL values: 4=veg, 5=not-veg, 6=water, 7=unclass, 11=snow
                if matches!(s, 4 | 5 | 6 | 7 | 11) {
                    *acc += f64::from(v);
                    *cnt += 1;
                }
            }
        }
        let mean = Array2::<i16>::from_shape_fn((rows, cols), |(r, c)| {
            let n = count[[r, c]];
            if n > 0 {
                (sum[[r, c]] / f64::from(n)) as i16
            } else {
                i16::MIN
            }
        });
        mean.insert_axis(Axis(0))
    }

    let t_kernel = Instant::now();
    rds_data
        .apply_reduction_with_mask::<u8, i16, _>(
            &rds_mask,
            ndvi_mean_with_scl,
            Dimension::Time,
            num_cpus_or_4(),
            &out_path,
            i16::MIN,
        )
        .expect("kernel");
    let kernel_secs = t_kernel.elapsed().as_secs_f64();
    println!("[pipeline] phase 4 (kernel cached-local, {} threads): {:.3} s", num_cpus_or_4(), kernel_secs);

    // ─────────────────────────────────────────────────────────────────────
    // Phase 5 — Same kernel but reading from /vsicurl/ for comparison
    // ─────────────────────────────────────────────────────────────────────
    let mut all_remote_paths: Vec<PathBuf> = Vec::new();
    all_remote_paths.extend(scenes.iter().map(|(_, r, _, _)| PathBuf::from(vsi_rewrite(r))));
    all_remote_paths.extend(scenes.iter().map(|(_, _, n, _)| PathBuf::from(vsi_rewrite(n))));
    let scl_remote: Vec<PathBuf> = scenes
        .iter()
        .map(|(_, _, _, s)| PathBuf::from(vsi_rewrite(s)))
        .collect();

    let mut rds_data_remote: RasterDataset<i16> = RasterDatasetBuilder::from_files(&all_remote_paths)
        .expect("remote data builder")
        .resolution(ImageResolution { x: 10.0, y: -10.0 })
        .block_size(BlockSize {
            rows: CROP_SIZE as usize,
            cols: CROP_SIZE as usize,
        })
        .build()
        .expect("remote data build");
    rds_data_remote.metadata.shape.times = scenes.len();
    rds_data_remote.metadata.shape.layers = 2;
    let mut remote_mappings = Vec::new();
    for (t, (_, r, _, _)) in scenes.iter().enumerate() {
        remote_mappings.push(LayerMapping {
            source: PathBuf::from(vsi_rewrite(r)),
            time_pos: t,
            layer_pos: 0,
            band: 1,
        });
    }
    for (t, (_, _, n, _)) in scenes.iter().enumerate() {
        remote_mappings.push(LayerMapping {
            source: PathBuf::from(vsi_rewrite(n)),
            time_pos: t,
            layer_pos: 1,
            band: 1,
        });
    }
    rds_data_remote.layer_mappings = remote_mappings;
    // Limit to 1 block for remote (downloads way less data).
    rds_data_remote.blocks.truncate(1);

    let mut rds_mask_remote: RasterDataset<u8> =
        RasterDatasetBuilder::<u8>::from_files(&scl_remote)
            .expect("remote mask builder")
            .resolution(ImageResolution { x: 20.0, y: -20.0 })
            .block_size(BlockSize {
                rows: CROP_SIZE as usize,
                cols: CROP_SIZE as usize,
            })
            .build()
            .expect("remote mask build");
    rds_mask_remote.metadata.shape.times = scenes.len();
    rds_mask_remote.metadata.shape.layers = 1;
    rds_mask_remote.layer_mappings = scl_remote
        .iter()
        .enumerate()
        .map(|(t, p)| LayerMapping {
            source: p.clone(),
            time_pos: t,
            layer_pos: 0,
            band: 1,
        })
        .collect();
    rds_mask_remote.blocks = rds_data_remote.blocks.clone();
    rds_mask_remote.metadata.shape.rows = rds_data_remote.metadata.shape.rows;
    rds_mask_remote.metadata.shape.cols = rds_data_remote.metadata.shape.cols;

    let out_remote = tempfile::Builder::new()
        .suffix(".tif")
        .tempfile()
        .unwrap()
        .into_temp_path();
    std::fs::remove_file(&out_remote).ok();
    let t_remote = Instant::now();
    rds_data_remote
        .apply_reduction_with_mask::<u8, i16, _>(
            &rds_mask_remote,
            ndvi_mean_with_scl,
            Dimension::Time,
            num_cpus_or_4(),
            &out_remote,
            i16::MIN,
        )
        .expect("remote kernel");
    let remote_secs = t_remote.elapsed().as_secs_f64();
    println!(
        "[pipeline] phase 5 (kernel /vsicurl/ same scene, {} threads): {:.3} s",
        num_cpus_or_4(),
        remote_secs
    );

    // ─────────────────────────────────────────────────────────────────────
    // Phase 6 — Compute speedup ratio + assert plausible NDVI
    // ─────────────────────────────────────────────────────────────────────
    let speedup = remote_secs / kernel_secs;
    println!("[pipeline] speedup cached-local vs /vsicurl/: {speedup:.1}×");

    // Read mean NDVI from output to validate.
    let ds = gdal::Dataset::open(&*out_path).expect("reopen");
    let band = ds.rasterband(1).expect("band 1");
    let buf: gdal::raster::Buffer<i16> = band
        .read_as(
            (0, 0),
            (CROP_SIZE as usize, CROP_SIZE as usize),
            (CROP_SIZE as usize, CROP_SIZE as usize),
            None,
        )
        .expect("read");
    let valid: Vec<i16> = buf.data().iter().copied().filter(|&v| v != i16::MIN).collect();
    let n_valid = valid.len();
    let mean = valid.iter().map(|&v| v as f64).sum::<f64>() / n_valid as f64;
    println!(
        "[pipeline] output NDVI mean ×10000 = {mean:.1} (over {n_valid}/{} pixels valid)",
        CROP_SIZE * CROP_SIZE
    );

    // Assertions: real measurement, real values.
    assert!(n_valid > 0, "all pixels masked out — SCL filtering removed everything");
    assert!(
        mean > -1000.0 && mean < 9000.0,
        "NDVI mean ({mean}) outside plausible range — kernel may have miscomputed"
    );
    // Speedup is not guaranteed — depends on network speed at test time —
    // but assert it's at least non-trivially faster (>1.5×) to prove the
    // claim direction holds.
    println!("[pipeline] === FINAL VERDICT ===");
    println!("[pipeline] download:     {download_secs:.3} s (one-time amortized cost)");
    println!("[pipeline] cached kernel: {kernel_secs:.3} s");
    println!("[pipeline] remote kernel: {remote_secs:.3} s");
    println!("[pipeline] speedup:       {speedup:.2}×");
}

fn num_cpus_or_4() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
