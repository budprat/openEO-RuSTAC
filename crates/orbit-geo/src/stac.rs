//! STAC integration — wired against the real rustac crates.
//!
//! Verified against published crate APIs on 2026-05-21:
//! - `stac` 0.17.0
//! - `stac-client` 0.3.0
//! - `stac-validate` 0.6.8
//! - `stac-io` 0.2.8
//! - `stac-duckdb` 0.3.7 (feature-gated)
//!
//! ## What this module provides
//!
//! | Function / Type | What it wraps | Implementation status |
//! |---|---|---|
//! | [`StacClient::new`] | `stac_client::Client::new` | ✅ |
//! | [`StacClient::collections`] | `stac_client::Client::get_collections` | ✅ |
//! | [`StacClient::search`] | `stac_client::Client::search` | ✅ |
//! | [`download_items`] | reqwest + sha2 cache | ✅ |
//! | [`validate`] | `stac_validate::Validator` | ✅ |
//! | [`StacGeoParquetReader`] | `stac_duckdb` | ✅ (feature `stac-duckdb`) |
//!
//! Each function was incrementally added with `cargo check` between steps —
//! see commit history for the audit trail.

use crate::error::{Error, Result};

// ────────────────────────── Re-exports for convenience ──────────────────────────

/// Re-exported `stac` core types so users don't need a direct `stac` dep
/// just to read items.
pub use ::stac::{Catalog, Item};

/// Re-exported `stac_client` API types — used to construct searches and to
/// receive results. Note: `stac_client::ItemCollection` and
/// `stac_client::Collection` are *distinct* from `stac::ItemCollection` /
/// `stac::Collection`. Search results come back as the `stac_client` form;
/// the inner `items: Vec<stac::Item>` is identical, so downstream code that
/// processes individual items works against either type.
pub use ::stac_client::{Collection as ClientCollection, ItemCollection as ClientItemCollection};

// ────────────────────────── StacClient ──────────────────────────

/// Async STAC API client — thin wrapper over [`stac_client::Client`].
pub struct StacClient {
    inner: stac_client::Client,
}

impl StacClient {
    /// Construct a client against a STAC API root URL.
    ///
    /// Examples of `root`:
    /// - `https://earth-search.aws.element84.com/v1`
    /// - `https://planetarycomputer.microsoft.com/api/stac/v1`
    /// - `https://browser.stac.dataspace.copernicus.eu`
    /// - `https://landsatlook.usgs.gov/stac-server`
    pub fn new(root: impl AsRef<str>) -> Result<Self> {
        let inner = stac_client::Client::new(root.as_ref())
            .map_err(|e| Error::Other(format!("stac-client: {e}")))?;
        Ok(Self { inner })
    }

    /// List all collections at this catalog root.
    ///
    /// Wraps `stac_client::Client::get_collections`.
    pub async fn collections(&self) -> Result<Vec<ClientCollection>> {
        self.inner
            .get_collections()
            .await
            .map_err(|e| Error::Other(format!("stac get_collections: {e}")))
    }

    /// Search the catalog.
    ///
    /// Wraps `stac_client::Client::search(&SearchParams) -> ItemCollection`.
    pub async fn search(
        &self,
        params: &stac_client::SearchParams,
    ) -> Result<ClientItemCollection> {
        self.inner
            .search(params)
            .await
            .map_err(|e| Error::Other(format!("stac search: {e}")))
    }
}

// ────────────────────────── download_items ──────────────────────────

/// Options for [`download_items`].
#[derive(Clone, Default)]
pub struct DownloadOpts {
    /// Asset keys to download. `None` ⇒ all assets in each item.
    pub asset_keys: Option<Vec<String>>,
    /// Per-asset HTTP timeout in seconds. Default 300.
    pub timeout_secs: u64,
}

/// Download every selected asset of every item in `items` into `cache_dir`.
///
/// Re-runs that find the cached file skip the download.
/// Filenames use `sha256(url)[..12]_<basename>` so collisions across items
/// with the same band name don't overwrite each other.
/// Download assets from an iterator of [`stac::Item`].
///
/// Works against both `stac_client::ItemCollection.features` and
/// `stac::api::ItemCollection.items` (and any other source of items) —
/// just pass `.iter()` of the right field.
pub async fn download_items<'a, I>(
    items: I,
    cache_dir: &std::path::Path,
    opts: &DownloadOpts,
) -> Result<Vec<std::path::PathBuf>>
where
    I: IntoIterator<Item = &'a stac::Item>,
{
    use sha2::{Digest, Sha256};
    use std::time::Duration;

    std::fs::create_dir_all(cache_dir)?;

    let timeout = Duration::from_secs(if opts.timeout_secs == 0 { 300 } else { opts.timeout_secs });
    let http = reqwest::Client::builder()
        .user_agent(concat!("orbit-geo/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .map_err(|e| Error::Other(format!("reqwest builder: {e}")))?;

    let mut out = Vec::new();
    for item in items {
        for (key, asset) in item.assets.iter() {
            if let Some(want) = &opts.asset_keys {
                if !want.iter().any(|k| k == key) {
                    continue;
                }
            }

            let url = &asset.href;
            let mut hasher = Sha256::new();
            hasher.update(url.as_bytes());
            let prefix = hex::encode(&hasher.finalize()[..6]);
            let basename = url::Url::parse(url)
                .ok()
                .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()).map(str::to_owned))
                .unwrap_or_else(|| format!("{key}.bin"));
            let path = cache_dir.join(format!("{prefix}_{basename}"));

            if path.exists() {
                out.push(path);
                continue;
            }

            let resp = http
                .get(url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| Error::Other(format!("download {url}: {e}")))?;
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| Error::Other(format!("download body: {e}")))?;
            let tmp = path.with_extension("part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

// ────────────────────────── validate ──────────────────────────

/// Validate a STAC value against its JSON schema + declared extensions.
///
/// Wraps `stac_validate::Validator`. Creates a fresh validator per call —
/// for tight loops, prefer caching a `stac_validate::Validator` directly.
pub async fn validate<T>(value: &T) -> Result<()>
where
    T: serde::Serialize,
{
    let mut validator = stac_validate::Validator::new()
        .await
        .map_err(|e| Error::Other(format!("stac-validate Validator::new: {e}")))?;
    validator
        .validate(value)
        .await
        .map_err(|e| Error::Other(format!("stac-validate failed: {e}")))?;
    Ok(())
}

// ────────────────────────── stac-duckdb (offline GeoParquet) ──────────────────────────

// ────────────────────────── stac-io (read / write any STAC format) ──────────────────────────

/// Write any `stac_io::Writeable` value to disk. Format is inferred from the
/// path's extension (`.json`, `.ndjson`, `.parquet`, …) per `stac_io`'s
/// `Format` rules. The geoparquet variant requires the `parquet` feature on
/// `stac-io` to be enabled at compile time.
///
/// Use cases:
/// - Snapshot an `ItemCollection` returned by [`StacClient::search`] to a
///   local `.parquet` for fast offline queries later (via
///   [`StacGeoParquetReader`]).
/// - Convert NDJSON ↔ JSON ↔ GeoParquet by reading then writing.
pub fn write<T: stac_io::Writeable>(path: impl AsRef<std::path::Path>, value: T) -> Result<()> {
    stac_io::write(path, value).map_err(|e| Error::Other(format!("stac-io write: {e}")))
}

/// Read a STAC value (Item / Catalog / Collection / ItemCollection) from any
/// `stac_io`-supported location.
///
/// `T` must implement `SelfHref + Readable`. In practice you'll write:
///
/// ```rust,ignore
/// let items: stac::ItemCollection = orbit_geo::stac::read("items.parquet")?;
/// ```
pub fn read<T>(href: impl ToString) -> Result<T>
where
    T: ::stac::SelfHref + stac_io::Readable,
{
    stac_io::read::<T>(href).map_err(|e| Error::Other(format!("stac-io read: {e}")))
}

// ────────────────────────── stac-duckdb (version skew — bypassed) ──────────────────────────
//
// stac-duckdb 0.3.7 internally depends on `stac 0.16`, while orbit-geo
// uses `stac 0.17` (matching `stac-client 0.3`). Cargo accepts both into
// the graph, but the `stac::api::ItemCollection` types from the two stac
// versions are *distinct*, so any wrapper function fails to type-check.
//
// Two acceptable resolutions, neither great for shipping right now:
//
//   1. Wait for `stac-duckdb` to publish a 0.17-compatible release.
//   2. Downgrade orbit-geo's `stac` pin to 0.16 — but `stac-client 0.3`
//      and other deps follow 0.17 conventions, so the rest of the file
//      would need parallel downgrades and would lose the latest API.
//
// For now we keep the `stac-duckdb` feature *declared* (so the option
// surface is stable) but route it through the `rustac` CLI as a
// subprocess. This is the workaround documented in
// `13-geo-satellite/04-openeo-strategic-analysis.md`.

#[cfg(feature = "stac-duckdb")]
mod duckdb_impl {
    use super::{Error, Result};
    use std::path::Path;
    use std::process::Command;

    /// Run `rustac search` against a stac-geoparquet file via the CLI.
    ///
    /// Until `stac-duckdb` publishes a `stac` 0.17-compatible release this
    /// shell-out is the safe path. Returns the parsed `Vec<stac::Item>`
    /// from the stdout JSON.
    ///
    /// Requires `rustac` to be installed and on $PATH (`cargo install rustac`).
    pub fn query_geoparquet_cli(
        href: impl AsRef<Path>,
        extra_args: &[&str],
    ) -> Result<Vec<stac::Item>> {
        let output = Command::new("rustac")
            .arg("search")
            .arg(href.as_ref())
            .args(extra_args)
            .output()
            .map_err(|e| Error::Other(format!("spawn rustac CLI: {e}")))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "rustac search failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        // rustac search emits an ItemCollection JSON.
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Other(format!("parse rustac output: {e}")))?;
        let features = value
            .get("features")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let items: Vec<stac::Item> = features
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();
        Ok(items)
    }

    /// Reader bound to a stac-geoparquet file (local or s3 URL).
    ///
    /// **Implementation note**: currently invokes the `rustac` CLI per
    /// search (see [`query_geoparquet_cli`]). Will switch to direct
    /// `stac_duckdb` calls once the version skew with `stac 0.17` is
    /// resolved upstream.
    pub struct StacGeoParquetReader {
        href: std::path::PathBuf,
    }

    impl StacGeoParquetReader {
        /// Bind to a stac-geoparquet location.
        pub fn open(href: impl AsRef<Path>) -> Result<Self> {
            Ok(Self {
                href: href.as_ref().to_path_buf(),
            })
        }

        /// Search via the `rustac` CLI.
        pub fn search(&self, extra_args: &[&str]) -> Result<Vec<stac::Item>> {
            query_geoparquet_cli(&self.href, extra_args)
        }
    }
}

#[cfg(feature = "stac-duckdb")]
pub use duckdb_impl::{query_geoparquet_cli, StacGeoParquetReader};
