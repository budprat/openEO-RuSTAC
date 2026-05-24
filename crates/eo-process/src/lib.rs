//! **eo-process** — OpenEO process-graph AST + executor surface.
//!
//! Reference: OpenEO API 1.2 Processes spec. This crate models the *graph*;
//! the *executor* trait is implemented later by local kernels (delegating
//! to `eo-kernel`) and by remote-backend clients.
//!
//! Implementation discipline: the graph schema is canonical openEO JSON.
//! Implementations are independent re-creations from the spec, not from the
//! upstream raster engine workspace. See `docs/clean-room-protocol.md`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod graph;

pub use graph::{Process, ProcessGraph, ProcessId};
