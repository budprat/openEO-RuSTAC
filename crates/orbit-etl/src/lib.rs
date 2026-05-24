//! orbit-etl — file → Polars → SQLite pipeline engine.
//!
//! Public surface:
//! - [`PipelineSpec`] — declarative pipeline configuration
//! - [`Engine`] — runs pipelines, tracks job state, emits events
//! - [`Event`] — observable progress events

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod engine;
pub mod spec;

pub use engine::{Engine, Event};
pub use spec::{FileFormat, FileSource, PipelineSpec};
