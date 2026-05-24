//! Common error type for orbit. Library crates return `orbit_core::Result<T>`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid pipeline spec: {0}")]
    InvalidSpec(String),

    #[error("source file not found: {0}")]
    SourceNotFound(String),

    #[error("source format unsupported")]
    UnsupportedFormat,

    #[error("polars error: {0}")]
    Polars(#[from] polars::error::PolarsError),

    #[error("sql error: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("job not found: {0}")]
    JobNotFound(String),

    #[error("job already terminated")]
    JobTerminated,

    #[error("internal: {0}")]
    Internal(String),

    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("job cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

// `serde_json` and `serde_yaml` errors lose type identity; flatten to Serde variant.
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self { Self::Serde(e.to_string()) }
}

impl From<serde_yaml::Error> for Error {
    fn from(e: serde_yaml::Error) -> Self { Self::Serde(e.to_string()) }
}
