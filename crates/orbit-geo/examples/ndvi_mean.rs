//! End-to-end NDVI-mean-over-time example.
//!
//! Demonstrates the **block-parallel raster reduction pattern** documented in the `13-geo-satellite/` library docs:
//! 9 timesteps × (red, NIR, FMask) → block-parallel NDVI per timestep →
//! FMask-filtered mean over time → single output GeoTIFF.
//!
//! ## Usage
//!
//! Pass paths to your scene files as three groups of 9 (red, NIR, FMask).
//! For a quick smoke test, set environment variable `ORBIT_GEO_NDVI_INPUT_DIR`
//! pointing to a directory containing files named `*_red.tif`, `*_nir.tif`,
//! `*_fmask.tif` — the example will glob them in.
//!
//! ```bash
//! ORBIT_GEO_NDVI_INPUT_DIR=/data/s2_56jns_2024 \
//!     cargo run --release --example ndvi_mean \
//!     -p orbit-geo -- --output /tmp/ndvi_mean.tif --threads 8
//! ```
//!
//! ## What this verifies
//!
//! - `RasterDatasetBuilder::from_files` partitions correctly
//! - Custom `LayerMapping` lets us declare `(time, layer) ← (file, band)`
//!   explicitly — so timestep N has red at `layers[0]`, NIR at `[1]`,
//!   FMask at `[2]`
//! - `apply_reduction_with_mask` reads blocks in parallel, runs the
//!   worker, writes results directly to the output (no temp files)
//! - GDAL VSI handles any path (local or `/vsis3/…`)

use ndarray::{s, Array2, Array3, Axis};
use orbit_geo::{
    dataset::{DatasetMetadata, LayerMapping, RasterDataset},
    types::{BlockSize, Dimension, ImageResolution, RasterShape},
    RasterDataBlock, RasterDatasetBuilder,
};
use std::marker::PhantomData;
use std::path::PathBuf;

/// NDVI-mean worker: for each block (times × 3 layers × rows × cols),
/// compute NDVI per timestep, keep only clear-sky pixels (FMask ∈ {0, 1}),
/// then average across time. Output: (1, rows, cols) scaled int16.
///
/// Layer order: 0 = red, 1 = nir, 2 = fmask (set by the LayerMappings below).
fn ndvi_mean_masked(rdb: &RasterDataBlock<i16>, _dim: Dimension) -> Array3<i16> {
    let (_t, layers, rows, cols) = rdb.data.dim();
    assert!(layers >= 3, "expected 3 layers: red, nir, fmask");

    let mut sum: Array2<f64> = Array2::zeros((rows, cols));
    let mut count: Array2<u32> = Array2::zeros((rows, cols));

    for time_slice in rdb.data.axis_iter(Axis(0)) {
        let red = time_slice.slice(s![0, .., ..]).mapv(|e| e as f32);
        let nir = time_slice.slice(s![1, .., ..]).mapv(|e| e as f32);
        let fmask = time_slice.slice(s![2, .., ..]);

        let denom = &nir + &red + 1e-10_f32;
        let ndvi_t: Array2<i16> = ((&nir - &red) / &denom).mapv(|e| (e * 10_000.0) as i16);

        // fmask convention: 0=nodata, 1=clear, 2=cloud, 3=shadow, 4=snow, 5=water
        // Keep clear-sky pixels only.
        for (((acc, cnt), &ndvi_v), &mask_v) in sum
            .iter_mut()
            .zip(count.iter_mut())
            .zip(ndvi_t.iter())
            .zip(fmask.iter())
        {
            if mask_v == 0 || mask_v == 1 {
                *acc += f64::from(ndvi_v);
                *cnt += 1;
            }
        }
    }

    let mean: Array2<i16> = Array2::from_shape_fn((rows, cols), |(r, c)| {
        let n = count[[r, c]];
        if n > 0 {
            (sum[[r, c]] / f64::from(n)) as i16
        } else {
            i16::MIN
        }
    });

    mean.insert_axis(Axis(0))
}

/// Trivial 1-band mask dataset that mirrors the data dataset's block
/// partitioning. For real use, you'd build this from your cloud-mask files;
/// here we just clone the data partitioning so the API surface gets
/// exercised end-to-end.
fn make_dummy_mask(data: &RasterDataset<i16>) -> RasterDataset<u8> {
    let metadata = DatasetMetadata {
        shape: RasterShape {
            times: 1,
            layers: 1,
            rows: data.metadata.shape.rows,
            cols: data.metadata.shape.cols,
        },
        epsg_code: data.metadata.epsg_code,
        geo_transform: data.metadata.geo_transform,
    };
    // Reuse the same block partitioning so par_iter.zip stays aligned.
    let blocks = data.blocks.clone();
    let layer_mappings: Vec<LayerMapping> = data
        .source_files()
        .iter()
        .take(1)
        .map(|p| LayerMapping {
            source: p.clone(),
            time_pos: 0,
            layer_pos: 0,
            band: 1,
        })
        .collect();
    RasterDataset {
        metadata,
        blocks,
        layer_mappings,
        source_files: data.source_files().to_vec(),
        _t: PhantomData,
    }
}

fn collect_inputs() -> anyhow::Result<(Vec<PathBuf>, Vec<LayerMapping>)> {
    let dir = std::env::var("ORBIT_GEO_NDVI_INPUT_DIR")
        .map_err(|_| anyhow::anyhow!("set ORBIT_GEO_NDVI_INPUT_DIR to a folder of *_red.tif, *_nir.tif, *_fmask.tif"))?;
    let dir = PathBuf::from(dir);

    let glob = |suffix: &str| -> anyhow::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(suffix))
            })
            .collect();
        v.sort();
        Ok(v)
    };

    let reds = glob("_red.tif")?;
    let nirs = glob("_nir.tif")?;
    let masks = glob("_fmask.tif")?;
    anyhow::ensure!(
        !reds.is_empty() && reds.len() == nirs.len() && reds.len() == masks.len(),
        "expected matching counts of *_red, *_nir, *_fmask in {}",
        dir.display()
    );

    // All files used by the dataset; default builder will derive a partitioning.
    let mut all_files = Vec::new();
    all_files.extend(reds.iter().cloned());
    all_files.extend(nirs.iter().cloned());
    all_files.extend(masks.iter().cloned());

    // Explicit layer mapping: per timestep, 3 layers (red=0, nir=1, fmask=2).
    let mut mappings = Vec::new();
    for (t, ((r, n), m)) in reds.iter().zip(nirs.iter()).zip(masks.iter()).enumerate() {
        mappings.push(LayerMapping { source: r.clone(), time_pos: t, layer_pos: 0, band: 1 });
        mappings.push(LayerMapping { source: n.clone(), time_pos: t, layer_pos: 1, band: 1 });
        mappings.push(LayerMapping { source: m.clone(), time_pos: t, layer_pos: 2, band: 1 });
    }
    Ok((all_files, mappings))
}

fn main() -> anyhow::Result<()> {
    let output = std::env::args()
        .skip_while(|a| a != "--output")
        .nth(1)
        .unwrap_or_else(|| "/tmp/ndvi_mean.tif".into());
    let threads: usize = std::env::args()
        .skip_while(|a| a != "--threads")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let block_size: usize = std::env::args()
        .skip_while(|a| a != "--block-size")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2048);

    println!("orbit-geo NDVI-mean example");
    println!("  output:     {output}");
    println!("  threads:    {threads}");
    println!("  block size: {block_size}");

    let (all_files, mappings) = collect_inputs()?;
    let n_times = mappings.iter().map(|m| m.time_pos).max().unwrap_or(0) + 1;
    println!("  timesteps:  {n_times}");
    println!("  files:      {}", all_files.len());

    // Build with the default mapping (one file per slot), then override.
    let mut rds: RasterDataset<i16> = RasterDatasetBuilder::from_files(&all_files)?
        .resolution(ImageResolution { x: 10.0, y: -10.0 })
        .block_size(BlockSize { rows: block_size, cols: block_size })
        .build()?;

    // Override the dataset's layout: 3 layers × n_times timesteps.
    rds.metadata.shape.times = n_times;
    rds.metadata.shape.layers = 3;
    rds.layer_mappings = mappings;

    // Mask dataset reuses the data dataset's first scene as a placeholder.
    let mask = make_dummy_mask(&rds);

    let t0 = std::time::Instant::now();
    rds.apply_reduction_with_mask::<u8, i16, _>(
        &mask,
        |rdb, _mask_block, dim| ndvi_mean_masked(rdb, dim),
        Dimension::Layer,
        threads,
        std::path::Path::new(&output),
        i16::MIN,
    )?;
    println!("  apply:      {:?}", t0.elapsed());
    println!("  done:       {output}");
    Ok(())
}
