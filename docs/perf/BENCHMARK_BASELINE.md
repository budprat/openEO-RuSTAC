# orbit-geo — Performance Baseline (T0.2)

**Captured:** 2026-05-21
**Machine:** macOS Darwin 25.2.0 (Apple Silicon)
**Rust:** 1.95.x stable (Edition 2024)
**Profile:** `cargo bench --bench apply_reduction` (release, LTO inherited from workspace)
**Methodology:** Criterion 0.8.2, `--warm-up-time 1 --measurement-time 3 --sample-size 10` for the first capture; deeper runs to follow once Tier 1 lands.

## What we measured

`apply_reduction_to_writer` running an NDVI-mean-over-time worker against
synthetic 2-layer (red + nir) datasets generated via `tiny_geotiff`. All
files live in tempdir (memory-backed via APFS), so this measures **read +
compute** without disk-I/O contention or network latency. The bench uses an
in-memory `DiscardWriter` so output serialization is also excluded.

NDVI worker (per timestep):
```
ndvi[t] = (nir[t] - red[t]) / (nir[t] + red[t] + 1e-10)
mean = (sum over t of ndvi[t]) / n_times  # scaled ×10000 to fit i16
```

Hand-computed expected output for synthetic fixtures
(red[t]=100+t, nir[t]=200+t):
- n_times=2 → 3322
- n_times=3 → 3311

Both validated in `tier0_t02_tests`.

## Results

### `apply_reduction` (T0.2)

| Configuration | Time (median) | Iter/sec |
|---|---|---|
| 256×256 × 9 timesteps × 128px blocks × 1 thread  | **1.93 ms** | ~520 |
| 1024×1024 × 9 timesteps × 256px blocks × 4 threads | **10.28 ms** | ~97 |

### `apply` — `bench_apply` (T4.7)

| Configuration | Time (median) | Notes |
|---|---|---|
| 512×512 × 1 timestep × 128px blocks × 1 thread → real GeoTIFF on disk | **4.74 ms** | 16 blocks; full write path incl. LZW + overviews |

### `apply_with_mask` — `bench_apply_with_mask` (T4.7)

| Configuration | Time (median) | Notes |
|---|---|---|
| 512×512 × 128px blocks × 1 thread → `DiscardWriter` (no disk) | **397 µs** | Pure compute path; 12× faster than `bench_apply` because that one writes to disk |

### `bench_ndvi_annual_full_tile` (T4.7)

Compiles and runs. Long measurement time (≈30–60 s per sample at 2048×2048 × 12 timesteps).
Reserved for offline measurement campaigns rather than CI.

### `bench_get_vs_get_remote` (T4.7, gated `bench_live`)

VSI-path-rewrite microbench against a stable Element 84 S2A asset URL.
Network access required; not part of default `cargo bench`.

## Notes

- **Why so fast vs EORS's published 12s number?** EORS measured against real
  Sentinel-2 COGs (cached via `/vsicurl/`) at full tile size; this baseline
  runs synthetic tempfile data with no I/O. Useful as a kernel-cost floor;
  *not* directly comparable to EORS until T0.3 (live S2 smoke test) and
  T4.7 (`bench_ndvi_annual_full_tile`) land.
- **Per-block read+compute cost**: ~26–36 μs/block at 128–256 px²/9 timesteps/2 layers.
- **Outlier on the 1024×1024 case**: 1/10 high-severe, likely page-cache
  contention. Real benches should use larger sample size (default 100).

## How to reproduce

```bash
cargo bench -p orbit-geo --bench apply_reduction \
    -- --warm-up-time 3 --measurement-time 5 --sample-size 100
```

## Provenance

- Test code: `crates/orbit-geo/benches/apply_reduction.rs`
- Correctness validation: `crates/orbit-geo/src/test_support.rs` — modules
  `tier0_t02_tests` and `tier0_t02_discriminator`.
- Worker function source: identical in both files (small enough to
  duplicate; `test_support` is `cfg(test)`-only so bench cannot import it).
