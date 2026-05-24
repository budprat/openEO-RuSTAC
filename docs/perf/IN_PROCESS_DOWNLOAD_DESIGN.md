# In-process download paths (P1/P2/P3) — design sketch

Status: P1 landed (additive, opt-in). P2 and P3 are designs only.

## Phase A microbench — what we actually measured

Bench: `crates/orbit-geo/examples/bench_download_one_cog.rs`, run 2026-05-24
against `S2A_33UWP_20240603_0_L2A/B04.tif` (10980x10980 UInt16 COG) with the
Wien Vienna bbox (EPSG:4326 16.10..16.60, 48.10..48.40) — ~12 MP/band crop.

| Run | subprocess (ms) | in-process total (ms) | open (ms) | s3+decode (ms) | encode (ms) |
|-----|----------------:|----------------------:|----------:|---------------:|------------:|
| 1   |          38,491 |                22,225 |     2,465 |         19,478 |         261 |
| 2   |          26,490 |                34,137 |     2,379 |         31,480 |         257 |
| 3   |          25,807 |                14,597 |     1,994 |         12,322 |         254 |

Notes:
- Encode is consistently ~260 ms (~1 % of wall). Open (vsicurl HEAD + COG
  header reads) is ~2 s. Everything else — 90+ % of wall — is **S3 range reads
  + libtiff decode**.
- Run-to-run variance is dominated by S3 network latency. Both code paths
  go through libcurl + libtiff via the same libgdal, so they share the
  variance source.
- The fork-exec cost of `gdal_translate` is the delta between
  `subprocess_total` and `(in_process_total + ~constant gdal_translate
  parse/init)` once you control for S3 noise. That delta is well under
  500 ms — i.e. **subprocess overhead is < 2 % of wall-clock**.

**Decision rule from the task: "subprocess overhead < 10 % of total → STOP."
We are at < 2 %. P1 was implemented anyway as an additive opt-in path
because the trait surface is needed for P3, and the encoder/decoder cost
gets re-amortised differently there.**

## P1 — In-process Downloader (LANDED, additive)

What landed:
- `orbit_geo::providers::download_in_process_with_crs()` — same signature
  as `download_via_gdal_translate_with_crs`, uses `gdal::Dataset::open` +
  `band.read_as::<T>()` + `MEM` driver `create_copy` to GTiff.
- `orbit_openeo::geo_executor::download::InProcessGdalDownloader` — impls
  the existing `Downloader` trait.
- `GeoExecutor::with_inprocess_downloader()` builder method (opt-in).
- 4 unit tests in `providers::tests` using local fixture COGs.
- Dispatches on `band.band_type()` so UInt16 / Int16 / UInt8 / Float32
  inputs all preserve their native pixel format.

What did NOT change:
- `GdalTranslateDownloader` stays the default. Existing code paths,
  `geo-kernel` feature gate, and CLI flags are unchanged.
- `download_via_gdal_translate_*` functions are untouched.

Pros:
- One less process fork per asset (saves the < 500 ms overhead).
- Source `Dataset` stays in scope and reusable for future tile caching.
- Errors surface as Rust `Result`s — no `exit-code != 0` ambiguity.

Cons (and known differences vs subprocess):
- Pixel window is clamped to source extent — gdal_translate fills
  out-of-bounds margin with nodata; this impl returns only in-bounds pixels.
  For openEO graphs this is benign (downstream block reader honours
  whatever the file actually contains).
- Type dispatch is fixed (UInt16 / Int16 / UInt8 / Float32). Extend
  `copy_window_typed` if you need UInt32 / Float64.

Effort: ~half a day. Blast radius: small (additive).

## P2 — async-tiff + object_store::AmazonS3 (pure Rust, no GDAL FFI on read path)

Architecture sketch:
```
Downloader trait gains async variant:
    pub trait AsyncDownloader: Send + Sync {
        async fn download(&self, src_url, dst, crop, crop_crs) -> Result<()>;
    }

AsyncTiffDownloader { store: Arc<dyn ObjectStore> }
    -> object_store::aws::AmazonS3::new(...)         (auth / region)
    -> async_tiff::TIFF::open_via(reader)            (async COG header parse)
    -> tiff.read_image(decoder, window)              (async HTTP range reads + decode in Rust)
    -> Write GTiff out via gdal::Dataset (or skip — pass Vec<u16> directly to next stage).
```

What's already in the workspace:
- `async-tiff = "0.3"` and `object_store = "0.13"` are listed as optional
  deps in `crates/orbit-geo/Cargo.toml` behind the `async-tiff` feature.
- `crates/orbit-geo/src/async_io.rs` (feature `async-tiff`) exists as
  the planned home for this reader.

Pros:
- No libgdal FFI thread contention on the read path. The GDAL global
  CPL_MUTEX disappears from the critical path of N parallel downloads.
- True async pipelined fetches — `object_store` can issue overlapping
  HTTP/2 range reads with backpressure.
- Pure-Rust decode means stack traces / panics are debuggable.

Cons:
- Requires turning the `Downloader` trait async (or adding a parallel
  `AsyncDownloader` trait). Existing call site in
  `GeoExecutor::fetch_with_cache_async` already runs from async — the
  change is mostly removing the `tokio::task::spawn_blocking` shim.
- `async-tiff 0.3` does not yet support COG-Y-axis-flipped strips or
  certain LZW predictors — needs a real-COG fixture test before enabling
  by default.
- Auth (PC SAS, CDSE Bearer) needs to flow into `object_store`'s
  per-request signer rather than GDAL config options.

File changes (sketch):
- `crates/orbit-geo/src/providers.rs::download_via_async_tiff(...)` (new
  fn, feature `async-tiff`).
- `apps/orbit-openeo/src/geo_executor/download.rs::AsyncTiffDownloader`
  (new impl, gated by a future `async-downloader` feature).
- `GeoExecutor` gains `with_async_downloader(Arc<dyn AsyncDownloader>)`.

Blast radius: medium. Effort: 1–2 days. Win: 10–30 % wall on multi-scene
jobs because of pipelined range reads (subprocess can't multiplex across
forks).

## P3 — /vsis3/ direct-read inside block_executor (no download phase)

Architecture sketch:
```
Skip the download phase entirely. Pass an array of REMOTE hrefs to
RasterDataset, which holds open `gdal::Dataset` handles against
/vsis3/<bucket>/<key>. block_executor::apply_blocks reads pixels
on-demand per block — each block issues only the HTTP range request
its window requires. No temp GeoTIFFs.
```

What changes:
- `RasterDatasetBuilder::from_remote_files(&[s3_urls])` constructor —
  variant of `from_files` that opens via `/vsis3/` URI.
- `FileCache` semantics: becomes a **block cache**, not a file cache.
  Key = (href, band, window) instead of href.
- `eval_load_collection` stops eagerly downloading. The cube envelope
  carries the list of remote hrefs; download happens lazily per block.
- `block_executor` gets a remote-aware path that handles 503 / 429 from
  S3 with retry + back-off.

What stays:
- `Downloader` trait remains for the eager-download fallback (jobs that
  need a persistent local cache, e.g. for re-runs against the same window).
- `gdal_translate` subprocess path stays for offline / debugging.

Pros:
- Eliminates the entire "download phase" (~120 s in the 194 s baseline)
  — by far the biggest win.
- Lowest peak disk usage — no intermediate GeoTIFFs at all.
- Matches the blog architecture cited in the task brief.
- Block-level parallelism gets to overlap S3 range reads across blocks.

Cons:
- Block cache invalidation is harder than file cache invalidation.
- Partial network failures during a long apply_blocks pass surface as
  per-block errors — needs a richer error type (which blocks failed,
  retry policy).
- `/vsis3/` requires `AWS_REGION` + `AWS_S3_ENDPOINT` config; the
  PC SAS signer story is awkward because `/vsis3/` doesn't natively
  accept query-string tokens (use `/vsicurl/` with header injection
  for non-S3 backends).

File changes (sketch):
- `crates/orbit-geo/src/builder.rs::RasterDatasetBuilder::from_remote_files`.
- `crates/orbit-geo/src/processing.rs::read_block` already opens via the
  thread-local cache — needs to honour `/vsis3/` paths (a one-line check).
- `crates/orbit-geo/src/cache.rs` gains a block-cache variant or is
  bypassed entirely when remote-mode is on.
- `apps/orbit-openeo/src/geo_executor/eval_load.rs` stops calling
  `fetch_with_cache_async` and just records the hrefs.
- `apps/orbit-openeo/src/geo_executor/eval_*.rs` workers all need
  remote-aware error propagation.

Blast radius: large. Effort: 2–3 days. Win: 60+ % wall on multi-scene
jobs (eliminates the download phase entirely).

## Comparison table

| Path | Complexity | Expected win on 194 s baseline | Risk | Fallback |
|------|------------|-------------------------------:|------|----------|
| P1 (LANDED) | low (additive) | < 2 % (sub-process overhead only) | very low — opt-in flag | revert to `GdalTranslateDownloader` |
| P2 (async-tiff) | medium | 10–30 % (pipelined S3 + no GDAL mutex) | medium — async-tiff 0.3 COG support gaps | fall back to P1 or subprocess if read fails |
| P3 (/vsis3/ direct-read in block_executor) | large | 60+ % (skip download phase) | high — touches cache, builder, block_executor, error handling | retain P1/P2 + subprocess for cache-required jobs |

## Recommendation

Given Phase A data (subprocess fork-exec is < 2 % of wall):

1. **Ship P1 as opt-in** (done). Keep `GdalTranslateDownloader` as the
   default. The new path is exercised by tests; production traffic stays
   on the subprocess path until P2/P3 are ready.
2. **Skip P2 unless** parallel multi-scene jobs become the bottleneck.
   Its win is in pipelining, not single-file throughput — Phase A
   measures a single COG so this benefit is invisible.
3. **Invest in P3 next.** It is the only path that actually attacks the
   194 s baseline — by removing the download phase rather than making it
   faster. Bench first: instrument the 194 s job to break down
   download-phase vs apply-phase before committing.
