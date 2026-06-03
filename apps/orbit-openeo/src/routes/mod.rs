//! HTTP route modules.

pub mod catalogs;
pub mod credentials;
pub mod discovery;
pub mod files;
// `/products` re-exports `orbit_geo::products`, which only links under the
// `geo-kernel` feature. Gate the module so `--no-default-features` (slim,
// no-libgdal) builds compile. (audit B1, 2026-06-03)
#[cfg(feature = "geo-kernel")]
pub mod products;
pub mod jobs;
pub mod process_graphs;
pub mod result;
pub mod services;
pub mod spec;
pub mod subscription;
pub mod validation;
