//! **Async TIFF reader path** (gated by `async-tiff` feature).
//!
//! Opens local or remote GeoTIFFs without libgdal — uses the
//! [`async-tiff`](https://crates.io/crates/async-tiff) crate +
//! [`object_store`](https://crates.io/crates/object_store).

#![cfg(feature = "async-tiff")]

use crate::error::{Error, Result};
use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::ObjectReader;
use async_tiff::TIFF;
use object_store::local::LocalFileSystem;
use std::path::Path;
use std::sync::Arc;

/// Open a local GeoTIFF via `async-tiff` and return the parsed `TIFF`
/// (header + IFD list). No pixel reads — that requires further work.
///
pub async fn open_async(path: &Path) -> Result<TIFF> {
    let abs = path
        .canonicalize()
        .map_err(|e| Error::Other(format!("async_io canonicalize {}: {e}", path.display())))?;
    let parent = abs
        .parent()
        .ok_or_else(|| Error::Other(format!("async_io: no parent for {}", abs.display())))?;
    let file_name = abs
        .file_name()
        .ok_or_else(|| Error::Other(format!("async_io: no file name for {}", abs.display())))?
        .to_string_lossy()
        .into_owned();

    let local = LocalFileSystem::new_with_prefix(parent)
        .map_err(|e| Error::Other(format!("LocalFileSystem: {e}")))?;
    let store: Arc<dyn object_store::ObjectStore> = Arc::new(local);
    let object_store_path = object_store::path::Path::from(file_name);
    let reader = ObjectReader::new(store, object_store_path);
    let cached = ReadaheadMetadataCache::new(reader);

    let mut metadata_reader = TiffMetadataReader::try_open(&cached)
        .await
        .map_err(|e| Error::Other(format!("TiffMetadataReader::try_open: {e}")))?;
    let ifds = metadata_reader
        .read_all_ifds(&cached)
        .await
        .map_err(|e| Error::Other(format!("read_all_ifds: {e}")))?;
    let endianness = metadata_reader.endianness();
    Ok(TIFF::new(ifds, endianness))
}
