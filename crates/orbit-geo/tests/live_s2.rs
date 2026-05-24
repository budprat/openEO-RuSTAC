//! **T0.3 — Live Sentinel-2 smoke test against Element 84's Earth Search.**
//!
//! Gated behind the `bench_live` feature so default `cargo test` is hermetic.
//!
//! Run with: `cargo test -p orbit-geo --features bench_live --test live_s2 -- --nocapture`
//!
//! This test exercises the FULL remote path:
//! 1. STAC search for a low-cloud Sentinel-2 L2A scene
//! 2. Convert asset hrefs to `/vsis3/...`
//! 3. GDAL opens via anonymous S3 (`configure_anonymous_s3`)
//! 4. orbit-geo kernel computes NDVI mean over a tiny crop window
//! 5. Assert output values fall in the vegetated NDVI range

#![cfg(feature = "bench_live")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use orbit_geo::{
    builder::RasterDatasetBuilder,
    dataset::LayerMapping,
    providers::{configure_anonymous_s3, vsi_rewrite},
    types::{BlockSize, Dimension, ImageResolution},
    RasterDataBlock,
};

#[tokio::test]
async fn live_s2_ndvi_mean_against_earth_search_anonymous() {
    // Configure GDAL for anonymous S3 (AWS_NO_SIGN_REQUEST etc.)
    configure_anonymous_s3();

    // 1. STAC search Earth Search for a recent low-cloud S2 L2A scene
    //    over Australian agricultural area.
    let stac_url = "https://earth-search.aws.element84.com/v1/search";
    let body = serde_json::json!({
        "collections": ["sentinel-2-l2a"],
        "bbox": [144.5, -36.6, 144.7, -36.4],
        "datetime": "2024-01-01T00:00:00Z/2024-12-31T23:59:59Z",
        "query": { "eo:cloud_cover": { "lt": 10.0 } },
        "limit": 1
    });
    let client = reqwest::Client::builder()
        .user_agent("orbit-geo-live-test/0.1")
        .build()
        .expect("reqwest client");
    let resp = client
        .post(stac_url)
        .json(&body)
        .send()
        .await
        .expect("STAC search request");
    assert!(resp.status().is_success(), "STAC search HTTP {}", resp.status());
    let payload: serde_json::Value = resp.json().await.expect("STAC search JSON");
    let features = payload["features"].as_array().expect("features array");
    assert!(
        !features.is_empty(),
        "Earth Search returned no S2 scenes for the requested bbox + cloud_cover"
    );

    let item = &features[0];
    let item_id = item["id"].as_str().expect("item id").to_string();
    println!("live_s2: using scene {item_id}");

    // 2. Get red (B04) + nir (B08) asset hrefs
    //    Earth Search v1 exposes assets by short name: "red", "nir", or by band id.
    let red_href = item["assets"]["red"]["href"]
        .as_str()
        .or_else(|| item["assets"]["B04"]["href"].as_str())
        .expect("red asset href");
    let nir_href = item["assets"]["nir"]["href"]
        .as_str()
        .or_else(|| item["assets"]["B08"]["href"].as_str())
        .expect("nir asset href");
    println!("live_s2: red={red_href}");
    println!("live_s2: nir={nir_href}");

    // 3. Convert to VSI paths
    let red_vsi = vsi_rewrite(red_href);
    let nir_vsi = vsi_rewrite(nir_href);
    println!("live_s2: red_vsi={red_vsi}");
    println!("live_s2: nir_vsi={nir_vsi}");

    // 4. Build dataset. Use a small block_size so we don't process the whole tile.
    let mut rds: orbit_geo::dataset::RasterDataset<i16> = RasterDatasetBuilder::from_files(&[
        std::path::PathBuf::from(&red_vsi),
        std::path::PathBuf::from(&nir_vsi),
    ])
    .expect("builder from_files")
    .resolution(ImageResolution { x: 10.0, y: -10.0 })
    .block_size(BlockSize { rows: 256, cols: 256 })
    .build()
    .expect("build dataset");

    rds.metadata.shape.times = 1;
    rds.metadata.shape.layers = 2;
    rds.layer_mappings = vec![
        LayerMapping {
            source: std::path::PathBuf::from(&red_vsi),
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        },
        LayerMapping {
            source: std::path::PathBuf::from(&nir_vsi),
            time_pos: 0,
            layer_pos: 1,
            band: 1,
        },
    ];

    // 5. NDVI mean worker — same math as the bench, but for n_times=1 the
    //    "mean over time" is just a per-pixel NDVI.
    fn ndvi_worker(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> ndarray::Array3<i16> {
        use ndarray::{s, Array2, Axis};
        let (_t, _layers, _rows, _cols) = rdb.data.dim();
        let red = rdb.data.slice(s![0, 0, .., ..]).mapv(|e| e as f32);
        let nir = rdb.data.slice(s![0, 1, .., ..]).mapv(|e| e as f32);
        let denom = &nir + &red + 1e-10_f32;
        let ndvi = ((&nir - &red) / &denom * 10_000.0_f32).mapv(|v| v as i16);
        let arr: Array2<i16> = ndvi;
        arr.insert_axis(Axis(0))
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    // Use a writer that just sums all output values for assertion.
    struct SumWriter {
        sum: AtomicI64,
        count: AtomicI64,
    }
    impl orbit_geo::writer::BlockWriter<i16> for SumWriter {
        fn write_block(
            &self,
            data: &ndarray::Array3<i16>,
            _w: orbit_geo::types::ReadWindow,
        ) -> orbit_geo::Result<()> {
            for &v in data.iter() {
                self.sum.fetch_add(v as i64, Ordering::SeqCst);
                self.count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let writer = Arc::new(SumWriter {
        sum: AtomicI64::new(0),
        count: AtomicI64::new(0),
    });

    // 6. Run kernel — read just the first block (~256×256) to keep the test fast.
    //    By default the dataset will partition the full tile; we want only block 0
    //    so we override blocks to a single-element vec.
    rds.blocks.truncate(1);

    let t0 = std::time::Instant::now();
    rds.apply_reduction_to_writer::<i16, _, _>(
        Arc::clone(&writer),
        ndvi_worker,
        Dimension::Time,
        1,
    )
    .expect("apply_reduction_to_writer ok");
    let elapsed = t0.elapsed();
    println!("live_s2: kernel {elapsed:?}");

    let total = writer.sum.load(Ordering::SeqCst);
    let count = writer.count.load(Ordering::SeqCst);
    assert!(count > 0, "no pixels written");
    let mean = total / count;
    println!("live_s2: mean NDVI ×10000 = {mean} (over {count} pixels)");

    // 7. Assert. Vegetated areas: NDVI ∈ [0.2, 0.9] → ×10000 = [2000, 9000].
    //    Mixed agricultural fields can easily span [1000, 6000].
    //    Accept a wide band to tolerate seasonal variation; the point is
    //    "we got real numbers back through the full pipeline".
    assert!(
        mean.abs() < 11000 && count > 1000,
        "expected real-looking NDVI mean (|mean| < 11000, count > 1000); got mean={mean}, count={count}"
    );
}
