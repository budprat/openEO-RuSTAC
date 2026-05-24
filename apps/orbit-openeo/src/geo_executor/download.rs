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
/// no subprocess fork-exec. Opt-in via
/// [`crate::geo_executor::GeoExecutor::with_inprocess_downloader`].
///
/// Phase A microbench (2026-05-24) measured subprocess overhead at <500 ms
/// vs >25 s S3 IO + decode -- i.e. the subprocess wrapper is not the
/// bottleneck. This impl exists so the trait gains an additive
/// no-subprocess option without changing the default behaviour, and to
/// pave the path for P3 (direct-read inside block_executor).
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
/// [`orbit_geo::providers::download_via_async_tiff_with_crs`] (async).
/// Uses `object_store::aws::AmazonS3` for HTTP/2 pipelined range reads
/// instead of libgdal `/vsicurl/`. Opt-in via
/// [`crate::geo_executor::GeoExecutor::with_async_tiff_downloader`].
///
/// Falls back transparently to the in-process libgdal path (P1) for
/// cross-CRS crops, non-tiled sources, multi-band sources, and any URL
/// scheme the parser does not recognise (`gs://`, plain `https://`, ...).
#[cfg(feature = "async-tiff-downloader")]
pub struct AsyncTiffDownloader;

#[cfg(feature = "async-tiff-downloader")]
impl Downloader for AsyncTiffDownloader {
    fn download(
        &self,
        src_url: &str,
        dst_path: &Path,
        crop: CropWindow,
        crop_crs: Option<&str>,
    ) -> Result<(), ExecError> {
        // Bridge sync -> async per CLAUDE.md §4 P0-5. Caller already
        // routes us through `tokio::task::spawn_blocking` via
        // `fetch_with_cache_async`, so we are on a blocking thread
        // here; `block_in_place` + `Handle::block_on` is safe.
        let src_owned = src_url.to_string();
        let dst_owned = dst_path.to_path_buf();
        let crop_crs_owned = crop_crs.map(str::to_string);
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                orbit_geo::providers::download_via_async_tiff_with_crs(
                    &src_owned,
                    &dst_owned,
                    Some(crop),
                    crop_crs_owned.as_deref(),
                ),
            )
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
}
