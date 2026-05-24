//! **eo-core** — pure, I/O-free data types shared by every Earth-observation
//! crate in the `orbit-rs` workspace.
//!
//! # Design rules
//!
//! - No I/O. No GDAL. No async runtime. Nothing that touches the filesystem,
//!   the network, or a process boundary belongs here.
//! - Every public type is `Serialize + Deserialize` so callers can persist /
//!   transmit it without owning a domain crate.
//! - Every type is `Copy` where the field shape allows it — these values
//!   propagate freely between threads, channels, and parallel kernels.
//! - The crate is *concept-named*, not *file-named*: pick the module from the
//!   concept (window, shape, transform), not from where the file used to live.
//!
//! # Roadmap
//!
//! Initial extraction (Week 1 of the maturity-and-parity plan) moves the
//! pure data types out of `orbit-geo::types`. Subsequent weeks add CRS
//! identifiers, NoData specifications, and sensor / temporal value types.
//!
//! See `docs/plans/01-maturity-and-parity.md`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod block;
pub mod output;
pub mod shape;
pub mod transform;
pub mod window;

// Re-export at crate root for convenience.
pub use block::BlockSize;
pub use output::{OutputConfig, OutputFormat};
pub use shape::{Dimension, RasterShape};
pub use transform::{GeoTransform, ImageResolution};
pub use window::{Offset, Overlap, ReadWindow, Size};
