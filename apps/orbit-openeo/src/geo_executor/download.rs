//! Asset signing + COG-window downloading.

use std::path::Path;

use orbit_geo::providers::CropWindow;

use crate::executor::ExecError;

/// COG-window downloader. Default impl shells out to `gdal_translate
/// -srcwin`. Tests inject a copy-from-fixture impl so they don't need
/// network or GDAL CLI in unit-test mode.
pub trait Downloader: Send + Sync {
    /// Download the crop window described by `crop` from `src_url` into
    /// `dst_path`. Implementations must create `dst_path`'s parent.
    ///
    /// `crop_crs` is the SRS of the crop coordinates per openEO 1.3.0
    /// `spatial_extent.crs` (A12). `None` means "interpret in native
    /// CRS"; `Some("EPSG:4326")` is the openEO default.
    fn download(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
    ) -> Result<(), ExecError>;

    /// **Task #34** — hint-aware variant. Callers that have STAC-derived
    /// metadata (epsg, geo_transform, raster_size, dtype, nodata) may pass
    /// it via `hint` so the downloader can short-circuit IFD round-trips
    /// or parallelise bbox projection with the IFD fetch.
    ///
    /// Default impl ignores the hint and forwards to [`Self::download`] —
    /// existing impls (gdal_translate / in-process / fixture) need no
    /// changes. Only `AsyncTiffDownloader` overrides this method today.
    #[cfg(feature = "async-tiff-downloader")]
    fn download_with_meta(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
        _hint: Option<&orbit_geo::providers::BandMetadataHint>,
    ) -> Result<(), ExecError> {
        self.download(src_url, dst_path, crop, crop_crs)
    }
}

/// Signs asset URLs before they reach the downloader.
///
/// STAC backends like Planetary Computer hand out plain `s3://…` hrefs
/// that need a short-lived SAS query string appended before they're
/// readable. CDSE / EODC issue OIDC Bearer tokens you must inject into
/// `/vsicurl?...` request headers. `AssetSigner` is the plug-point.
pub trait AssetSigner: Send + Sync {
    /// Transform `href` into a fetchable URL. Default impl returns the
    /// href unchanged (good for public buckets like Element84).
    fn sign(&self, href: &str) -> Result<String, ExecError>;
}

/// No-op signer — passes URLs through. Default for `GeoExecutor`.
pub struct NoopAssetSigner;

impl AssetSigner for NoopAssetSigner {
    fn sign(&self, href: &str) -> Result<String, ExecError> {
        Ok(href.to_string())
    }
}

/// Planetary Computer signer — appends a query-string SAS token.
///
/// In production the token comes from
/// `https://planetarycomputer.microsoft.com/api/sas/v1/token/<container>`;
/// this struct accepts the already-issued token + container so token
/// refresh is the caller's responsibility (mirrors orbit-geo's
/// `sign_planetary_computer_url`).
pub struct PlanetaryComputerSigner {
    /// SAS query string, **without** the leading `?`.
    pub sas_token: String,
}

impl PlanetaryComputerSigner {
    /// New PC signer with the given SAS token.
    #[must_use]
    pub fn new(sas_token: impl Into<String>) -> Self {
        Self { sas_token: sas_token.into() }
    }
}

impl AssetSigner for PlanetaryComputerSigner {
    fn sign(&self, href: &str) -> Result<String, ExecError> {
        if self.sas_token.is_empty() {
            return Ok(href.to_string());
        }
        if href.contains('?') {
            Ok(format!("{href}&{}", self.sas_token))
        } else {
            Ok(format!("{href}?{}", self.sas_token))
        }
    }
}

/// CDSE / OIDC Bearer signer — embeds the token in the gdal_translate
/// `/vsicurl?` request via the `GDAL_HTTP_HEADER_FILE` mechanism is the
/// production approach. For URL-level signing (the AssetSigner contract)
/// we rewrite to `/vsicurl?url=...&header.Authorization=Bearer%20<tok>`.
pub struct BearerSigner {
    /// Pre-acquired OIDC bearer token.
    pub token: String,
}

impl BearerSigner {
    /// New bearer signer.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self { Self { token: token.into() } }
}

impl AssetSigner for BearerSigner {
    fn sign(&self, href: &str) -> Result<String, ExecError> {
        if self.token.is_empty() {
            return Ok(href.to_string());
        }
        // URL-encode the bearer header value so it's safe inside the query string.
        let encoded = url_encode(&format!("Bearer {}", self.token));
        let separator = if href.contains('?') { '&' } else { '?' };
        Ok(format!("{href}{separator}header.Authorization={encoded}"))
    }
}

/// RFC 3986 percent-encoding for the small set of chars we actually emit.
/// Avoids a `urlencoding` dep.
pub(super) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Default `Downloader` that invokes `gdal_translate -srcwin` via the
/// orbit-geo helper.
pub struct GdalTranslateDownloader;

impl Downloader for GdalTranslateDownloader {
    fn download(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
    ) -> Result<(), ExecError> {
        let rewritten = orbit_geo::providers::vsi_rewrite(src_url);
        orbit_geo::providers::download_via_gdal_translate_with_crs(
            &rewritten,
            dst_path,
            Some(crop),
            crop_crs,
        )
        .map(|_| ())
        .map_err(|e| ExecError::Backend(format!("gdal_translate {src_url}: {e}")))
    }
}

/// In-process `Downloader` that calls `gdal::Dataset::open` directly --
/// no subprocess fork-exec. Installed via
/// [`crate::geo_executor::GeoExecutor::with_inprocess_downloader`].
///
/// **Status (post-Task #34, 2026-05-24)**: this is the **P2 opt-out
/// fallback** in `apps/orbit-openeo` when `async-tiff-downloader` is built.
/// Activated by `ORBIT_INPROCESS_DOWNLOADER=1`. Wall: 71 / 76 / 88 s
/// (78 avg) on 12 MP Wien S2 — tighter tail than P2 but slightly slower
/// hot-path median. Use when monitoring shows P2 S3 body-error retry
/// storms. See `docs/perf/FEATURE_FLAG_MATRIX.md`.
///
/// Phase A microbench (2026-05-24) measured subprocess overhead at <500 ms
/// vs >25 s S3 IO + decode -- i.e. the subprocess wrapper is not the
/// bottleneck. This impl exists so the trait gains an additive
/// no-subprocess option without changing the constructor-level default,
/// and to pave the path for P3 (direct-read inside block_executor).
pub struct InProcessGdalDownloader;

impl Downloader for InProcessGdalDownloader {
    fn download(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
    ) -> Result<(), ExecError> {
        let rewritten = orbit_geo::providers::vsi_rewrite(src_url);
        orbit_geo::providers::download_in_process_with_crs(
            &rewritten,
            dst_path,
            Some(crop),
            crop_crs,
        )
        .map(|_| ())
        .map_err(|e| ExecError::Backend(format!("in_process_download {src_url}: {e}")))
    }
}

/// **P2** -- pure-Rust async `Downloader` that bridges to
/// [`orbit_geo::providers::download_via_async_tiff_with_crs_and_meta`] (async).
/// Uses `object_store::aws::AmazonS3` for HTTP/2 pipelined range reads
/// instead of libgdal `/vsicurl/`. Installed via
/// [`crate::geo_executor::GeoExecutor::with_async_tiff_downloader`].
///
/// **Runtime default for `apps/orbit-openeo` (since 2026-05-24 / Task #34)**
/// when the `async-tiff-downloader` feature is built. Opt out with
/// `ORBIT_INPROCESS_DOWNLOADER=1` to fall back to the in-process libgdal
/// path (P1) — preferred under S3 transport instability.
/// See `docs/perf/FEATURE_FLAG_MATRIX.md`.
///
/// Falls back transparently to the in-process libgdal path (P1) for
/// cross-CRS crops via PROJ-string / WKT (`EPSG:` is handled in-band via
/// the `proj` crate), non-tiled sources, multi-band sources, and any URL
/// scheme the parser does not recognise (`gs://`, plain `https://`, ...).
#[cfg(feature = "async-tiff-downloader")]
pub struct AsyncTiffDownloader;

/// Dedicated multi-thread tokio runtime that drives the async-tiff +
/// object_store IO. SEPARATE from the main openEO runtime to avoid the
/// `block_in_place` + `Handle::current().block_on(...)` deadlock that
/// occurred when the sync `Downloader::download` bridge was invoked from
/// within `tokio::task::spawn_blocking` (the blocking thread is not a
/// worker, so `Handle::current()` returns the main runtime and re-entry
/// stalls under concurrency on the shared scheduler).
///
/// Lazily initialised once per process; reused across all downloads.
#[cfg(feature = "async-tiff-downloader")]
static ASYNC_TIFF_RT: once_cell::sync::Lazy<tokio::runtime::Runtime> =
    once_cell::sync::Lazy::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("orbit-async-tiff")
            .build()
            .expect("orbit-async-tiff runtime")
    });

#[cfg(feature = "async-tiff-downloader")]
impl Downloader for AsyncTiffDownloader {
    fn download(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
    ) -> Result<(), ExecError> {
        self.download_with_meta(src_url, dst_path, crop, crop_crs, None)
    }

    /// **Task #34**: hint-aware download. When the caller supplies a
    /// [`BandMetadataHint`] harvested from STAC (epsg + transform + shape),
    /// the underlying async-tiff path can:
    /// - pre-decide cross-CRS handling without opening the file
    /// - project the bbox in parallel with the IFD fetch
    fn download_with_meta(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
        hint: Option<&orbit_geo::providers::BandMetadataHint>,
    ) -> Result<(), ExecError> {
        // Bridge sync -> async via a DEDICATED runtime. The caller routes
        // us through `tokio::task::spawn_blocking` from the main runtime;
        // using `Handle::current().block_on(...)` here would re-enter the
        // main runtime from a blocking thread that isn't a worker, which
        // deadlocks under concurrency (verified: 600 s hang on 12 MP S2
        // NDVI). ASYNC_TIFF_RT is wholly separate, so block_on drives
        // the future on its own workers.
        let src_owned = src_url.to_string();
        let dst_owned = dst_path.to_path_buf();
        let crop_crs_owned = crop_crs.map(str::to_string);
        let hint_owned = hint.cloned();
        let result = ASYNC_TIFF_RT.block_on(async move {
            orbit_geo::providers::download_via_async_tiff_with_crs_and_meta(
                &src_owned,
                &dst_owned,
                Some(crop),
                crop_crs_owned.as_deref(),
                hint_owned.as_ref(),
            )
            .await
        });
        result
            .map(|_| ())
            .map_err(|e| ExecError::Backend(format!("async_tiff {src_url}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_signer_returns_href_unchanged() {
        let s = NoopAssetSigner;
        assert_eq!(s.sign("https://example.com/a.tif").unwrap(),
                   "https://example.com/a.tif");
    }

    #[test]
    fn planetary_computer_signer_appends_sas_token_with_question_mark() {
        let s = PlanetaryComputerSigner::new("st=2024&se=2025&sig=ABC");
        assert_eq!(
            s.sign("https://sentinel2l2a01.blob.core.windows.net/x/B04.tif").unwrap(),
            "https://sentinel2l2a01.blob.core.windows.net/x/B04.tif?st=2024&se=2025&sig=ABC"
        );
    }

    #[test]
    fn planetary_computer_signer_appends_with_ampersand_when_existing_query() {
        let s = PlanetaryComputerSigner::new("sig=XYZ");
        assert_eq!(
            s.sign("https://x.example/y?foo=bar").unwrap(),
            "https://x.example/y?foo=bar&sig=XYZ"
        );
    }

    #[test]
    fn planetary_computer_signer_with_empty_token_is_noop() {
        let s = PlanetaryComputerSigner::new("");
        assert_eq!(s.sign("https://x/y.tif").unwrap(), "https://x/y.tif");
    }

    #[test]
    fn bearer_signer_appends_url_encoded_authorization_header() {
        let s = BearerSigner::new("ey.JhbGciOiJIUzI1NiJ");
        let out = s.sign("https://cdse.example/B04.tif").unwrap();
        assert!(out.starts_with("https://cdse.example/B04.tif?header.Authorization="),
                "got {out}");
        // URL-encoded: "Bearer " → "Bearer%20"
        assert!(out.contains("Bearer%20ey.JhbGciOiJIUzI1NiJ"), "got {out}");
    }

    #[test]
    fn bearer_signer_uses_ampersand_when_existing_query() {
        let s = BearerSigner::new("tok");
        let out = s.sign("https://x/y?a=b").unwrap();
        assert!(out.contains("?a=b&header.Authorization="), "got {out}");
    }

    #[test]
    fn url_encode_handles_special_chars() {
        // Spaces become %20, slashes become %2F, common Bearer separators.
        assert_eq!(url_encode("Bearer tok"), "Bearer%20tok");
        assert_eq!(url_encode("a/b+c=d"), "a%2Fb%2Bc%3Dd");
        // Unreserved chars pass through.
        assert_eq!(url_encode("abcXYZ-._~"), "abcXYZ-._~");
    }

    /// Deadlock regression: 8 concurrent `AsyncTiffDownloader::download`
    /// invocations on the MAIN tokio runtime via `spawn_blocking` must
    /// all complete within 30 s. The pre-fix bridge used
    /// `block_in_place` + `Handle::current().block_on(...)` which stalls
    /// the shared scheduler under concurrency from blocking threads.
    /// The post-fix bridge uses a dedicated runtime (ASYNC_TIFF_RT) so
    /// there is no contention with the caller's runtime.
    #[cfg(feature = "async-tiff-downloader")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_tiff_downloader_parallel_no_deadlock() {
        use std::sync::Arc;
        use std::time::Duration;

        // Build a small tiled COG fixture and stitch via the LOCAL parser
        // path -- no network, no S3 -- so the test is hermetic. The local
        // FS path inside async_download still flows through tokio IO, so
        // the runtime-reentry bug reproduces here.
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src.tif");
        write_fixture_tiled_cog(&src, 256, 256);

        let dl: Arc<dyn Downloader> = Arc::new(AsyncTiffDownloader);
        let crop = CropWindow {
            min_x: 500_050.0,
            max_y: 5_400_000.0,
            max_x: 500_550.0,
            min_y: 5_399_500.0,
        };

        let mut handles = Vec::with_capacity(8);
        for i in 0..8u32 {
            let dl_i = dl.clone();
            let src_i = src.to_string_lossy().into_owned();
            let dst_i = tmp.path().join(format!("out_{i}.tif"));
            // Mirror production: bridge invoked inside spawn_blocking on
            // the caller's runtime.
            let h = tokio::task::spawn_blocking(move || {
                dl_i.download(&src_i, &dst_i, crop, None)
            });
            handles.push(h);
        }

        // Wall-clock guard -- the deadlock case hangs ~600 s in prod.
        let joined = tokio::time::timeout(Duration::from_secs(30), async move {
            let mut outs = Vec::with_capacity(handles.len());
            for h in handles {
                outs.push(h.await.expect("join"));
            }
            outs
        })
        .await
        .expect("all 8 concurrent downloads must finish within 30 s");

        for (i, r) in joined.into_iter().enumerate() {
            r.unwrap_or_else(|e| panic!("download {i} failed: {e}"));
        }
    }

    /// Helper duplicate of crates/orbit-geo/src/async_download.rs's test
    /// fixture writer. Local copy avoids a `pub` leak from the geo crate.
    #[cfg(feature = "async-tiff-downloader")]
    fn write_fixture_tiled_cog(path: &Path, cols: usize, rows: usize) {
        use gdal::raster::{Buffer, RasterCreationOptions};
        use gdal::DriverManager;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create_dir_all");
        }
        let drv = DriverManager::get_driver_by_name("GTiff").expect("GTiff");
        let opts = RasterCreationOptions::from_iter([
            "TILED=YES",
            "BLOCKXSIZE=64",
            "BLOCKYSIZE=64",
            "COMPRESS=DEFLATE",
        ]);
        let mut ds = drv
            .create_with_band_type_with_options::<u16, _>(path, cols, rows, 1, &opts)
            .expect("create");
        ds.set_geo_transform(&[500_000.0, 10.0, 0.0, 5_400_000.0, 0.0, -10.0])
            .expect("gt");
        let sr = gdal::spatial_ref::SpatialRef::from_epsg(32633).expect("sr");
        ds.set_spatial_ref(&sr).expect("set_sr");
        let mut b = ds.rasterband(1).expect("band");
        let data: Vec<u16> = (0..(cols * rows)).map(|i| (i % 65535) as u16).collect();
        let mut buf = Buffer::new((cols, rows), data);
        b.write::<u16>((0, 0), (cols, rows), &mut buf).expect("write");
    }
}
