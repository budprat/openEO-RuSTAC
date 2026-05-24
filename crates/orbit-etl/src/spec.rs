//! Declarative pipeline specification.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One pipeline run: source → transform → SQLite sink.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSpec {
    pub source: FileSource,
    pub destination_table: String,
    /// Optional Polars SQL transform applied to the input.
    /// Refer to the input table as `input`. Example:
    /// `"SELECT user_id, ts, amount FROM input WHERE country = 'US'"`
    #[serde(default)]
    pub sql_transform: Option<String>,
    /// If set, INSERT uses ON CONFLICT DO NOTHING keyed on this column.
    #[serde(default)]
    pub dedupe_column: Option<String>,
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
}

const fn default_batch_size() -> u32 { 1024 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSource {
    pub path: PathBuf,
    pub format: FileFormat,
    #[serde(default = "default_has_header")]
    pub has_header: bool,
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
}

const fn default_has_header() -> bool { true }
fn default_delimiter() -> String { ",".into() }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat { Csv, Parquet, Json }

impl PipelineSpec {
    /// Basic validation. Run before execute.
    pub fn validate(&self) -> orbit_core::Result<()> {
        if self.destination_table.is_empty() {
            return Err(orbit_core::Error::InvalidSpec("destination_table is empty".into()));
        }
        validate_sql_identifier(&self.destination_table, "destination_table")?;
        if !self.source.path.exists() {
            return Err(orbit_core::Error::SourceNotFound(
                self.source.path.display().to_string(),
            ));
        }
        Ok(())
    }
}

/// Validate that `path` resolves under `root` after canonicalisation.
///
/// Prevents path-traversal attacks where a gRPC client supplies
/// `../../etc/passwd` or an absolute path outside the configured data root.
///
/// Returns the canonicalised path on success.
pub fn validate_path_under_root(
    path: &std::path::Path,
    root: &std::path::Path,
) -> orbit_core::Result<std::path::PathBuf> {
    let canon_root = root.canonicalize().map_err(|e| {
        orbit_core::Error::InvalidSpec(format!("data_root {root:?} does not exist: {e}"))
    })?;
    let canon_path = path.canonicalize().map_err(|e| {
        orbit_core::Error::InvalidSpec(format!("source path {path:?} cannot be resolved: {e}"))
    })?;
    if !canon_path.starts_with(&canon_root) {
        return Err(orbit_core::Error::InvalidSpec(format!(
            "source path {canon_path:?} escapes data_root {canon_root:?}"
        )));
    }
    Ok(canon_path)
}

/// Validate a string is a safe SQL identifier (table or column name).
///
/// Returns `Ok(())` iff `name` matches `^[A-Za-z_][A-Za-z0-9_]*$`.
/// This prevents identifier-injection where attacker-controlled column headers
/// (e.g. from a malicious CSV) could break out of quoted DDL/DML.
pub fn validate_sql_identifier(name: &str, context: &str) -> orbit_core::Result<()> {
    if name.is_empty() {
        return Err(orbit_core::Error::InvalidSpec(
            format!("{context} is empty"),
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('\0');
    let head_ok = first.is_ascii_alphabetic() || first == '_';
    let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !(head_ok && tail_ok) {
        return Err(orbit_core::Error::InvalidSpec(
            format!("{context} must match [A-Za-z_][A-Za-z0-9_]*: got {name:?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_quote_terminator_injection() {
        // Classic PoC: CSV header that breaks out of quoted identifier.
        let r = validate_sql_identifier(r#"foo" ); DROP TABLE orbit_jobs; --"#, "col");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_semicolon() {
        assert!(validate_sql_identifier("a;b", "col").is_err());
    }

    #[test]
    fn rejects_whitespace() {
        assert!(validate_sql_identifier("foo bar", "col").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_sql_identifier("", "col").is_err());
    }

    #[test]
    fn rejects_leading_digit() {
        assert!(validate_sql_identifier("1col", "col").is_err());
    }

    #[test]
    fn rejects_leading_dash() {
        assert!(validate_sql_identifier("-col", "col").is_err());
    }

    #[test]
    fn rejects_unicode_homoglyph() {
        // Cyrillic 'а' looks like Latin 'a' but isn't ASCII alphabetic — must reject.
        assert!(validate_sql_identifier("аbc", "col").is_err());
    }

    #[test]
    fn accepts_snake_case() {
        assert!(validate_sql_identifier("user_id", "col").is_ok());
    }

    #[test]
    fn accepts_leading_underscore() {
        assert!(validate_sql_identifier("_private", "col").is_ok());
    }

    #[test]
    fn accepts_alphanumeric() {
        assert!(validate_sql_identifier("col42", "col").is_ok());
    }

    #[test]
    fn accepts_single_letter() {
        assert!(validate_sql_identifier("a", "col").is_ok());
    }

    // ── path traversal tests ─────────────────────────────────────────

    use std::fs;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "orbit-etl-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&p).expect("mkdir tmp");
        p
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let root = tmp();
        let outside = tmp();
        let evil_file = outside.join("evil.csv");
        fs::write(&evil_file, "x\n").expect("write evil");
        // Symlink-free attack: just point at a file outside root.
        let r = validate_path_under_root(&evil_file, &root);
        assert!(r.is_err(), "should reject path outside root");
    }

    #[test]
    fn rejects_absolute_outside_root() {
        let root = tmp();
        // /etc/hosts is essentially always readable + canonical-able on macOS/Linux.
        let r = validate_path_under_root(std::path::Path::new("/etc/hosts"), &root);
        assert!(r.is_err());
    }

    #[test]
    fn accepts_file_under_root() {
        let root = tmp();
        let good_file = root.join("data.csv");
        fs::write(&good_file, "a,b\n1,2\n").expect("write good");
        let r = validate_path_under_root(&good_file, &root);
        assert!(r.is_ok(), "should accept path under root: {r:?}");
    }

    #[test]
    fn rejects_nonexistent_path() {
        let root = tmp();
        let missing = root.join("nonexistent.csv");
        let r = validate_path_under_root(&missing, &root);
        assert!(r.is_err());
    }

    #[test]
    fn rejects_nonexistent_root() {
        let nope = std::env::temp_dir().join("does-not-exist-xyzzy-orbit");
        let _ = fs::remove_dir_all(&nope);
        let r = validate_path_under_root(std::path::Path::new("/etc/hosts"), &nope);
        assert!(r.is_err());
    }
}
