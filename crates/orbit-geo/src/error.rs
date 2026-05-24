//! Typed errors for orbit-geo.
//!
//! Library code returns [`Result<T>`]; application code can convert with
//! `?` to `anyhow::Result` if it prefers boxed errors.

use std::path::PathBuf;
use thiserror::Error;

/// Crate-wide error type.
#[derive(Debug, Error)]
pub enum Error {
    /// A source file or path could not be opened.
    #[error("source not found: {0}")]
    SourceNotFound(PathBuf),

    /// The dataset metadata across input scenes did not agree on CRS / shape.
    #[error("inconsistent metadata: {0}")]
    InconsistentMetadata(String),

    /// A worker function panicked or returned an error array of the wrong shape.
    #[error("worker error: {0}")]
    Worker(String),

    /// I/O error from the OS or GDAL.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// GDAL-reported error (file read/write).
    #[error("gdal: {0}")]
    Gdal(#[from] gdal::errors::GdalError),

    /// Builder configuration was invalid.
    #[error("invalid builder: {0}")]
    InvalidBuilder(String),

    /// Catch-all for context-rich error chains.
    #[error("{0}")]
    Other(String),
}

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Construct a builder-validation error from any displayable value.
    pub fn invalid_builder(msg: impl Into<String>) -> Self {
        Self::InvalidBuilder(msg.into())
    }

    /// Construct a worker error from any displayable value.
    pub fn worker(msg: impl Into<String>) -> Self {
        Self::Worker(msg.into())
    }
}
