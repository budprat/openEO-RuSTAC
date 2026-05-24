//! **Declarative imagery query DSL** (Tier 2).
//!
//! `Collection`, `Intersects`, `Cmp`, `ImageQueryBuilder` — provider-portable
//! STAC search construction. Mirrors the upstream raster crate's `ImageQueryBuilder` API shape
//! while building atop the rustac stack.

pub mod collection;
pub use collection::Collection;
pub mod intersects;
pub use intersects::Intersects;
pub mod filter;
pub use filter::{Cmp, cloudcover_filter};
pub mod builder;
pub use builder::{ImageQuery, ImageQueryBuilder};
pub mod registry;
pub use registry::canonical_bands;
pub mod exec;
