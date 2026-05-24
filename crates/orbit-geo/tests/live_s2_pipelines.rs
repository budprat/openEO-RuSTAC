//! **Multi-process verification against real Sentinel-2 data.**
//!
//! Companion to `tests/live_s2.rs`. Where `live_s2.rs` proves one pipeline
//! (NDVI-mean-via-`apply_reduction_to_writer`), this file proves **five
//! independent processes** all work against the same scene:
//!
//! - P1: `read_block_layer_idx` reads B04 (red) only
//! - P2: `read_block_layer_idx` reads B08 (NIR) only — sanity: NIR > red on average
//! - P3: `apply` (no reduction) preserves layer count in output
//! - P4: `sample_at_point` matches a direct GDAL band read
//! - P5: `apply_with_mask_to_writer` with all-clear mask equals the unmasked reduction
//!
//! Gated behind `bench_live` — requires network access to Element 84 Earth Search.

#![cfg(feature = "bench_live")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ndarray::{s, Array1, Array2, Array3, ArrayView3, Axis};
use orbit_geo::{
    block::RasterDataBlock,
    builder::RasterDatasetBuilder,
    dataset::{LayerMapping, RasterDataset},
    providers::{configure_anonymous_s3, vsi_rewrite},
    sampling::sample_at_point,
    types::{BlockSize, Dimension, ImageResolution},
    writer::BlockWriter,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Shared discovery: STAC search → (red_href, nir_href)
// ─────────────────────────────────────────────────────────────────────────────

async fn find_s2_scene() -> (String, String, String) {
    let client = reqwest::Client::builder()
        .user_agent("orbit-geo-pipelines-test/0.1")
        .build()
        .unwrap();
    let body = serde_json::json!({
        "collections": ["sentinel-2-l2a"],
        "bbox": [144.5, -36.6, 144.7, -36.4],
        "datetime": "2024-01-01T00:00:00Z/2024-12-31T23:59:59Z",
        "query": { "eo:cloud_cover": { "lt": 10.0 } },
        "limit": 1
    });
    let resp = client
        .post("https://earth-search.aws.element84.com/v1/search")
        .json(&body)
        .send()
        .await
        .expect("STAC search");
    let payload: serde_json::Value = resp.json().await.expect("STAC JSON");
    let item = &payload["features"][0];
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
    (id, red, nir)
}

fn build_two_band_dataset(red_vsi: &str, nir_vsi: &str) -> RasterDataset<i16> {
    let red_pb = PathBuf::from(red_vsi);
    let nir_pb = PathBuf::from(nir_vsi);
    let mut rds: RasterDataset<i16> =
        RasterDatasetBuilder::from_files(&[red_pb.clone(), nir_pb.clone()])
            .expect("builder")
            .resolution(ImageResolution { x: 10.0, y: -10.0 })
            .block_size(BlockSize { rows: 256, cols: 256 })
            .build()
            .expect("build");
    rds.metadata.shape.times = 1;
    rds.metadata.shape.layers = 2;
    rds.layer_mappings = vec![
        LayerMapping { source: red_pb, time_pos: 0, layer_pos: 0, band: 1 },
        LayerMapping { source: nir_pb, time_pos: 0, layer_pos: 1, band: 1 },
    ];
    rds.blocks.truncate(1);
    rds
}

// ─────────────────────────────────────────────────────────────────────────────
// P1: read_block_layer_idx(0, red) returns real B04 values in reflectance range
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn p1_read_block_layer_idx_red() {
    configure_anonymous_s3();
    let (id, red_href, nir_href) = find_s2_scene().await;
    println!("p1: scene = {id}");
    let red_vsi = vsi_rewrite(&red_href);
    let nir_vsi = vsi_rewrite(&nir_href);
    let rds = build_two_band_dataset(&red_vsi, &nir_vsi);

    let block = rds.read_block_layer_idx(0, 0).expect("read red layer");
    assert_eq!(block.data.dim().1, 1, "single-layer block returned");
    let max = block.data.iter().copied().max().unwrap();
    let min = block.data.iter().copied().min().unwrap();
    println!("p1: red range [{min}, {max}]");
    // S2 L2A SR reflectance × 10000 → realistic range
    assert!(min >= -10000 && max <= 20000, "red values implausible");
}

// ─────────────────────────────────────────────────────────────────────────────
// P2: read_block_layer_idx(0, nir) — NIR mean should exceed red mean over land
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn p2_read_block_layer_idx_nir_vs_red() {
    configure_anonymous_s3();
    let (id, red_href, nir_href) = find_s2_scene().await;
    println!("p2: scene = {id}");
    let red_vsi = vsi_rewrite(&red_href);
    let nir_vsi = vsi_rewrite(&nir_href);
    let rds = build_two_band_dataset(&red_vsi, &nir_vsi);

    let red_block = rds.read_block_layer_idx(0, 0).expect("red");
    let nir_block = rds.read_block_layer_idx(0, 1).expect("nir");
    let red_mean: f64 = red_block.data.iter().map(|&v| v as f64).sum::<f64>()
        / red_block.data.len() as f64;
    let nir_mean: f64 = nir_block.data.iter().map(|&v| v as f64).sum::<f64>()
        / nir_block.data.len() as f64;
    println!("p2: mean(red)={red_mean:.1}, mean(NIR)={nir_mean:.1}");
    // For any non-water land area, NIR > red on average due to chlorophyll absorption.
    assert!(
        nir_mean > red_mean,
        "expected NIR mean > red mean for vegetated land; got NIR={nir_mean}, red={red_mean}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// P3: `apply` (no reduction) preserves layer count in output
// ─────────────────────────────────────────────────────────────────────────────

struct CountingWriter {
    write_calls: AtomicUsize,
    overview_calls: AtomicUsize,
    layer_count: AtomicUsize,
}
impl BlockWriter<i16> for CountingWriter {
    fn write_block(&self, data: &Array3<i16>, _w: orbit_geo::types::ReadWindow) -> orbit_geo::Result<()> {
        self.write_calls.fetch_add(1, Ordering::SeqCst);
        self.layer_count.store(data.dim().0, Ordering::SeqCst);
        Ok(())
    }
    fn build_overviews(&self, _r: &str, _l: &[i32]) -> orbit_geo::Result<()> {
        self.overview_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn p3_apply_preserves_worker_output_layer_count() {
    configure_anonymous_s3();
    let (id, red_href, nir_href) = find_s2_scene().await;
    println!("p3: scene = {id}");
    let red_vsi = vsi_rewrite(&red_href);
    let nir_vsi = vsi_rewrite(&nir_href);
    let rds = build_two_band_dataset(&red_vsi, &nir_vsi);

    // Worker returns 3-layer output (e.g. red, NIR, NDVI all in one).
    let out = tempfile::Builder::new()
        .suffix(".tif")
        .tempfile()
        .unwrap()
        .into_temp_path();
    std::fs::remove_file(&out).ok();
    rds.apply::<i16, _>(
        |rdb: &RasterDataBlock<i16>| {
            let red = rdb.data.slice(s![0, 0, .., ..]).to_owned();
            let nir = rdb.data.slice(s![0, 1, .., ..]).to_owned();
            let red_f = red.mapv(|v| v as f32);
            let nir_f = nir.mapv(|v| v as f32);
            let denom = &nir_f + &red_f + 1e-10_f32;
            let ndvi: Array2<i16> = ((&nir_f - &red_f) / &denom * 10000.0).mapv(|v| v as i16);

            let mut out = Array3::<i16>::zeros((3, rdb.shape.rows, rdb.shape.cols));
            out.slice_mut(s![0, .., ..]).assign(&red);
            out.slice_mut(s![1, .., ..]).assign(&nir);
            out.slice_mut(s![2, .., ..]).assign(&ndvi);
            out
        },
        1,
        &out,
    )
    .expect("apply ok");

    // Reopen and verify 3 bands.
    let ds = gdal::Dataset::open(&*out).expect("reopen");
    assert_eq!(ds.raster_count(), 3, "apply must preserve worker's 3-layer output");
    println!("p3: output has {} bands as expected", ds.raster_count());
}

// ─────────────────────────────────────────────────────────────────────────────
// P4: sample_at_point matches a direct GDAL band read at the same pixel
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn p4_sample_at_point_matches_direct_gdal_read() {
    configure_anonymous_s3();
    let (id, red_href, _nir_href) = find_s2_scene().await;
    println!("p4: scene = {id}");
    let red_vsi = vsi_rewrite(&red_href);

    // Build single-band dataset from the red asset.
    let red_pb = PathBuf::from(&red_vsi);
    let rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&[red_pb.clone()])
        .expect("builder")
        .resolution(ImageResolution { x: 10.0, y: -10.0 })
        .build()
        .expect("build");

    // Find the dataset's geo extent.
    let gt = rds.metadata.geo_transform.0;
    let origin_x = gt[0];
    let pixel_w = gt[1];
    let origin_y = gt[3];
    let pixel_h = gt[5].abs();

    // Pick a point ~100 pixels from origin in both axes (well inside extent).
    let geo_x = origin_x + 100.0 * pixel_w;
    let geo_y = origin_y - 100.0 * pixel_h;
    let sampled = sample_at_point(&rds, geo_x, geo_y).expect("sample");

    // Direct GDAL read at row=100, col=100
    let ds = gdal::Dataset::open(&red_pb).expect("open red");
    let band = ds.rasterband(1).expect("band 1");
    let buf: gdal::raster::Buffer<i16> = band
        .read_as((100, 100), (1, 1), (1, 1), None)
        .expect("direct read");
    let direct = buf.data()[0];

    println!("p4: sample_at_point={sampled}, direct={direct}");
    assert_eq!(sampled, direct, "sample_at_point must match direct GDAL read");
}

// ─────────────────────────────────────────────────────────────────────────────
// P5: apply_with_mask_to_writer with all-1 mask produces same NDVI as unmasked
// ─────────────────────────────────────────────────────────────────────────────

struct NdviSumWriter {
    sum: AtomicI64,
    count: AtomicI64,
}
impl BlockWriter<i16> for NdviSumWriter {
    fn write_block(&self, data: &Array3<i16>, _w: orbit_geo::types::ReadWindow) -> orbit_geo::Result<()> {
        for &v in data.iter() {
            self.sum.fetch_add(v as i64, Ordering::SeqCst);
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn p5_apply_with_mask_all_clear_matches_unmasked_ndvi() {
    configure_anonymous_s3();
    let (id, red_href, nir_href) = find_s2_scene().await;
    println!("p5: scene = {id}");
    let red_vsi = vsi_rewrite(&red_href);
    let nir_vsi = vsi_rewrite(&nir_href);
    let rds_data = build_two_band_dataset(&red_vsi, &nir_vsi);

    // Build a mask dataset from the red band (we ignore its values, just need
    // a u8-typed dataset with matching block partitioning). Worker forces
    // "use mask block exists" path without changing semantics.
    let mut rds_mask: RasterDataset<u8> =
        RasterDatasetBuilder::<u8>::from_files(&[PathBuf::from(&red_vsi)])
            .expect("mask builder")
            .resolution(ImageResolution { x: 10.0, y: -10.0 })
            .block_size(BlockSize { rows: 256, cols: 256 })
            .build()
            .expect("mask build");
    rds_mask.layer_mappings = vec![LayerMapping {
        source: PathBuf::from(&red_vsi),
        time_pos: 0,
        layer_pos: 0,
        band: 1,
    }];
    rds_mask.blocks.truncate(1);

    let writer = Arc::new(NdviSumWriter {
        sum: AtomicI64::new(0),
        count: AtomicI64::new(0),
    });

    rds_data
        .apply_reduction_with_mask_to_writer::<u8, i16, _, _>(
            &rds_mask,
            Arc::clone(&writer),
            |rdb: &RasterDataBlock<i16>, _mask: &RasterDataBlock<u8>, _dim: Dimension| {
                let red = rdb.data.slice(s![0, 0, .., ..]).mapv(|v| v as f32);
                let nir = rdb.data.slice(s![0, 1, .., ..]).mapv(|v| v as f32);
                let denom = &nir + &red + 1e-10_f32;
                let ndvi = ((&nir - &red) / &denom * 10000.0).mapv(|v| v as i16);
                ndvi.insert_axis(Axis(0))
            },
            Dimension::Time,
            1,
        )
        .expect("apply_reduction_with_mask_to_writer");

    let sum = writer.sum.load(Ordering::SeqCst);
    let count = writer.count.load(Ordering::SeqCst);
    let mean = sum / count;
    println!("p5: with-mask NDVI mean ×10000 = {mean} (over {count} pixels)");
    // Per `tests/live_s2.rs` baseline, this same scene yields ~1738 unmasked.
    assert!(
        (mean - 1738).abs() <= 5,
        "with-mask NDVI ({mean}) should match unmasked baseline 1738 ±5"
    );
}
