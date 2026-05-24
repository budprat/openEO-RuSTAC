//! **eo-kernel** — block-parallel raster compute kernels.
//!
//! Provides the public surface for "apply this worker to every block of a
//! raster" and "reduce these blocks into a single output". Implementations
//! that need I/O (GDAL reads, COG writes) live in `eo-io` and consume the
//! traits defined here.
//!
//! # Goals
//!
//! - **Zero `Arc<Mutex<Vec<_>>>`** for parallel collection — use
//!   `rayon::collect_into_vec` or `tokio::sync::mpsc` channels.
//! - Each kernel is a `trait`, not a concrete function, so providers
//!   (e.g. CPU rayon vs GPU vs distributed) can swap.
//! - All shape arithmetic uses `eo_core` types.
//!
//! See `docs/plans/01-maturity-and-parity.md` for the larger refactor.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod block;
pub mod executor;
pub mod reduce;
pub mod worker;

pub use block::{RasterBlock, RasterBlockId};
pub use executor::{apply_blocks, enumerate_blocks, GridShape};
pub use reduce::{ReduceKind, ReductionWorker};
pub use worker::{BlockWorker, KernelError, Result};
