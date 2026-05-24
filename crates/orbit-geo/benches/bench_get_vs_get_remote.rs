//! **T4.7 bench: `.get()` (download) vs `.get_remote()` (VSI direct)** against
//! real Sentinel-2 from Element 84 Earth Search. Gated behind `bench_live`
//! so default `cargo bench` is hermetic.
//!
//! Run with: `cargo bench -p orbit-geo --features bench_live --bench bench_get_vs_get_remote`

#![cfg(feature = "bench_live")]

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use orbit_geo::providers::vsi_rewrite;

/// Real S2A asset href (Element 84 Earth Search, stable public bucket).
const SAMPLE_HREF: &str =
    "https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/55/H/BV/2024/12/S2B_55HBV_20241225_0_L2A/B04.tif";

fn bench_vsi_rewrite_only(c: &mut Criterion) {
    c.bench_function("get_remote/vsi_rewrite_https", |b| {
        b.iter(|| {
            black_box(vsi_rewrite(SAMPLE_HREF));
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50).measurement_time(std::time::Duration::from_secs(3));
    targets = bench_vsi_rewrite_only
}
criterion_main!(benches);
