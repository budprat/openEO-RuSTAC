//! orbit-core — shared types and errors for orbit-rs.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod error;
pub mod model;

pub use error::{Error, Result};
pub use model::{JobId, JobState, JobStatus};
