# Benchmark Baseline — orbit-openeo canonical pipeline

Captured: TODO
Machine:  TODO (hostname / CPU)
Build:    `cargo bench -p orbit-openeo --features geo-kernel --bench canonical_pipeline`

> Sibling to `docs/perf/BENCHMARK_BASELINE.md` (orbit-geo `apply_reduction`
> kernel baseline). This file scopes the **openEO façade**'s canonical
> NDVI pipeline — load_collection → mask_scl_dilation → ndvi →
> reduce_dimension(t, mean) — through `GeoExecutor::run_sync`.

| Bench                      | Median | Notes |
|----------------------------|--------|-------|
| pure_ndvi                  | TODO   | 1024×1024 single-scene kernel, in-memory `RasterDataBlock` |
| spec_strict_pipeline       | TODO   | 3 scenes 256×256, full chain through `GeoExecutor` (fake searcher + fixture downloader, no network) |
| fused_pipeline (A18)       | n/a    | atom not yet shipped — bench is scaffolded but not registered |
| ratio (spec ÷ fused)       | n/a    | recompute once fused lands |

## How to re-run

```bash
./scripts/bench.sh
```

Pass any extra flags through to criterion, e.g.:

```bash
./scripts/bench.sh --measurement-time 10 --sample-size 50
```

## Regression detection

A regression > 10% on `spec_strict_pipeline` median time should block
merge until investigated. Tracking ratio of `spec ÷ fused` is the
intended way to demonstrate A18's speedup once it lands.

## Provenance

- Bench file: `apps/orbit-openeo/benches/canonical_pipeline.rs`
- Runner script: `scripts/bench.sh`
- Inline fakes mirror `apps/orbit-openeo/src/geo_executor/test_support.rs`
  (`FixtureDownloader`, `FakeSearcher`, `fake_scenes_with_scl`) — they're
  `#[cfg(test)]`-gated and not reachable from a `benches/` build unit, so
  the bench inlines minimal equivalents.
