//! Phase-C #6 — typed error taxonomy.
//!
//! Refines the broad `ExecError::{InvalidGraph, UnknownProcess, Backend}`
//! triad into the per-domain variants the audit recommended:
//! `MissingBand`, `CrsMismatch`, `ResolutionMismatch`, `InvalidChunkPlan`,
//! `UnsupportedProcess`, `Io`, `Gdal`, `Stac`. Stops error-flattening
//! at the executor boundary.
//!
//! `From` impls bridge to legacy `ExecError` so the migration is
//! incremental.

use thiserror::Error;

use crate::executor::ExecError;

/// Per-domain typed error for the orbit-openeo runtime.
///
/// Each variant carries enough context to render an openEO-compatible
/// `{"code","message"}` envelope without re-parsing string blobs.
#[derive(Debug, Error, PartialEq)]
pub enum OrbitError {
    /// Logical band absent from the cube.
    #[error("missing band: {0}")]
    MissingBand(String),
    /// Two cubes differ in CRS.
    #[error("CRS mismatch: lhs={lhs}, rhs={rhs}")]
    CrsMismatch { lhs: String, rhs: String },
    /// Resolutions differ; resample required.
    #[error("resolution mismatch: lhs=({lhs_x},{lhs_y}), rhs=({rhs_x},{rhs_y})")]
    ResolutionMismatch { lhs_x: f64, lhs_y: f64, rhs_x: f64, rhs_y: f64 },
    /// Chunk planner rejected the inputs.
    #[error("invalid chunk plan: {0}")]
    InvalidChunkPlan(String),
    /// process_id not in the typed catalogue.
    #[error("unsupported process: {0}")]
    UnsupportedProcess(String),
    /// I/O failure (file system, network).
    #[error("io: {0}")]
    Io(String),
    /// GDAL backend failure.
    #[error("gdal: {0}")]
    Gdal(String),
    /// STAC catalog / search failure.
    #[error("stac: {0}")]
    Stac(String),
    /// Process graph itself is malformed (cycle, missing arg, etc.).
    #[error("invalid process graph: {0}")]
    InvalidGraph(String),
    /// Auth/credentials problem.
    #[error("auth: {0}")]
    Auth(String),
    /// Anything else — last resort, should drain over time.
    #[error("internal: {0}")]
    Other(String),
}

impl OrbitError {
    /// Stable error code string for the openEO HTTP envelope.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingBand(_) => "MissingBand",
            Self::CrsMismatch { .. } => "CrsMismatch",
            Self::ResolutionMismatch { .. } => "ResolutionMismatch",
            Self::InvalidChunkPlan(_) => "InvalidChunkPlan",
            Self::UnsupportedProcess(_) => "ProcessUnsupported",
            Self::Io(_) => "Io",
            Self::Gdal(_) => "Gdal",
            Self::Stac(_) => "Stac",
            Self::InvalidGraph(_) => "InvalidProcessGraph",
            Self::Auth(_) => "AuthenticationRequired",
            Self::Other(_) => "Internal",
        }
    }

    /// HTTP status code per openEO conventions.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::MissingBand(_)
            | Self::CrsMismatch { .. }
            | Self::ResolutionMismatch { .. }
            | Self::InvalidChunkPlan(_)
            | Self::UnsupportedProcess(_)
            | Self::InvalidGraph(_) => 400,
            Self::Auth(_) => 401,
            Self::Stac(_) => 502,
            Self::Io(_) | Self::Gdal(_) | Self::Other(_) => 500,
        }
    }

    /// Render as the openEO error envelope `{"code","message"}`.
    #[must_use]
    pub fn to_openeo_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code(),
            "message": self.to_string(),
        })
    }
}

// Bridge — legacy ExecError → OrbitError, with light parsing so
// "CollectionNotFound: …", "tiff write: …", "gdalwarp: …" etc. map to
// the right typed variant.
impl From<ExecError> for OrbitError {
    fn from(e: ExecError) -> Self {
        match e {
            ExecError::InvalidGraph(m) => Self::InvalidGraph(m),
            ExecError::UnknownProcess(p) => Self::UnsupportedProcess(p),
            ExecError::Backend(m) => classify_backend(&m),
            // B4: per-pixel arithmetic errors at the boundary look like
            // backend failures from the API perspective.
            ExecError::PerPixelComputation(m) => Self::InvalidGraph(m),
        }
    }
}

fn classify_backend(m: &str) -> OrbitError {
    let lower = m.to_ascii_lowercase();
    if lower.contains("crsmismatch") || lower.contains("crs mismatch") {
        OrbitError::CrsMismatch { lhs: String::new(), rhs: m.to_string() }
    } else if lower.contains("missingband") || lower.contains("missing band") {
        OrbitError::MissingBand(m.to_string())
    } else if lower.contains("resolution mismatch") {
        OrbitError::ResolutionMismatch {
            lhs_x: 0.0, lhs_y: 0.0, rhs_x: 0.0, rhs_y: 0.0,
        }
    } else if lower.starts_with("gdal:") || lower.starts_with("gdal_translate")
        || lower.starts_with("tiff") || lower.contains("gdalwarp")
    {
        OrbitError::Gdal(m.to_string())
    } else if lower.contains("stac") || lower.contains("collectionnotfound") {
        OrbitError::Stac(m.to_string())
    } else if lower.starts_with("io")
        || lower.contains("read ")
        || lower.contains("write ")
        || lower.contains("filecache")
    {
        OrbitError::Io(m.to_string())
    } else {
        OrbitError::Other(m.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_strings_are_distinct_per_variant() {
        let cases = [
            OrbitError::MissingBand("red".into()),
            OrbitError::CrsMismatch { lhs: "EPSG:4326".into(), rhs: "EPSG:3857".into() },
            OrbitError::ResolutionMismatch { lhs_x: 10.0, lhs_y: 10.0, rhs_x: 20.0, rhs_y: 20.0 },
            OrbitError::InvalidChunkPlan("budget too small".into()),
            OrbitError::UnsupportedProcess("zzz".into()),
            OrbitError::Io("eof".into()),
            OrbitError::Gdal("create".into()),
            OrbitError::Stac("404".into()),
            OrbitError::InvalidGraph("cycle".into()),
            OrbitError::Auth("missing".into()),
            OrbitError::Other("?".into()),
        ];
        let codes: Vec<&str> = cases.iter().map(|e| e.code()).collect();
        let unique: std::collections::HashSet<&&str> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len(), "every variant must have a unique code");
    }

    #[test]
    fn http_status_classifies_client_vs_server_errors() {
        assert_eq!(OrbitError::MissingBand("r".into()).http_status(), 400);
        assert_eq!(OrbitError::UnsupportedProcess("z".into()).http_status(), 400);
        assert_eq!(OrbitError::Auth("x".into()).http_status(), 401);
        assert_eq!(OrbitError::Stac("502".into()).http_status(), 502);
        assert_eq!(OrbitError::Io("?".into()).http_status(), 500);
    }

    #[test]
    fn to_openeo_json_emits_code_and_message() {
        let e = OrbitError::MissingBand("nir".into());
        let j = e.to_openeo_json();
        assert_eq!(j["code"], "MissingBand");
        assert!(j["message"].as_str().unwrap().contains("nir"));
    }

    #[test]
    fn from_legacy_invalid_graph_maps_to_invalid_graph() {
        let o: OrbitError = ExecError::InvalidGraph("missing process_graph".into()).into();
        assert!(matches!(o, OrbitError::InvalidGraph(_)));
    }

    #[test]
    fn from_legacy_unknown_process_maps_to_unsupported() {
        let o: OrbitError = ExecError::UnknownProcess("xyz".into()).into();
        match o {
            OrbitError::UnsupportedProcess(p) => assert_eq!(p, "xyz"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn classify_backend_routes_collectionnotfound_to_stac() {
        let o: OrbitError = ExecError::Backend("CollectionNotFound: sentinel-2-l2a".into()).into();
        assert!(matches!(o, OrbitError::Stac(_)));
    }

    #[test]
    fn classify_backend_routes_gdal_translate_to_gdal() {
        let o: OrbitError = ExecError::Backend("gdal_translate /vsicurl/...: exited 1".into()).into();
        assert!(matches!(o, OrbitError::Gdal(_)));
    }

    #[test]
    fn classify_backend_routes_filecache_to_io() {
        let o: OrbitError = ExecError::Backend("FileCache insert https://x: disk full".into()).into();
        assert!(matches!(o, OrbitError::Io(_)));
    }

    #[test]
    fn classify_backend_unknown_falls_through_to_other() {
        let o: OrbitError = ExecError::Backend("something weird".into()).into();
        assert!(matches!(o, OrbitError::Other(_)));
    }
}
