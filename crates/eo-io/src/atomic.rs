//! Atomic-write helpers — staging path + same-fs atomic rename.
//!
//! Used to write output COGs and other large files without leaving partial
//! state behind if the writer crashes mid-way.
//!
//! Same-filesystem `rename(2)` is atomic on POSIX. We compute a staging
//! path *adjacent to the destination* so the rename never crosses
//! filesystems.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors from atomic-write helpers.
#[derive(Debug, Error)]
pub enum AtomicError {
    /// `rename(staging, dst)` failed.
    #[error("atomic_replace {staging:?} -> {dst:?}: {source}")]
    Rename {
        /// The staging file we were trying to rename from.
        staging: PathBuf,
        /// The destination path we were trying to rename to.
        dst: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Result type for this module.
pub type Result<T> = std::result::Result<T, AtomicError>;

/// Compute a sibling staging path for `dst` in the same directory.
///
/// Same-directory placement guarantees `std::fs::rename` is an atomic
/// same-fs rename (POSIX). Process id + nanoseconds disambiguate concurrent
/// writers.
#[must_use]
pub fn staging_path_for(dst: &Path) -> PathBuf {
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let base = dst
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".{base}.staging.{pid}.{nanos}"))
}

/// Atomically replace `dst` with `staging` via same-fs `rename`.
///
/// On POSIX `rename(2)` is atomic when both paths are on the same filesystem,
/// so concurrent readers of `dst` either see the old file or the new one,
/// never a partial write. On failure the staging file is removed.
pub fn atomic_replace(staging: &Path, dst: &Path) -> Result<()> {
    match std::fs::rename(staging, dst) {
        Ok(()) => Ok(()),
        Err(source) => {
            // Best-effort cleanup of the staging file so we don't leak.
            let _ = std::fs::remove_file(staging);
            Err(AtomicError::Rename {
                staging: staging.to_path_buf(),
                dst: dst.to_path_buf(),
                source,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_path_for_uses_same_directory() {
        let dst = Path::new("/tmp/output.tif");
        let staging = staging_path_for(dst);
        assert_eq!(staging.parent(), Some(Path::new("/tmp")));
        assert_ne!(staging, dst);
    }

    #[test]
    fn staging_path_for_handles_no_parent() {
        let dst = Path::new("output.tif");
        let staging = staging_path_for(dst);
        // Either current dir or empty — non-empty parent component check.
        assert!(staging.is_relative() || staging.parent().is_some());
        assert!(staging.to_string_lossy().contains(".staging."));
    }

    #[test]
    fn atomic_replace_moves_staging_to_dst() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&staging, b"new content").unwrap();

        atomic_replace(&staging, &dst).expect("rename");
        assert!(dst.exists(), "dst not created");
        assert!(!staging.exists(), "staging not consumed");
        assert_eq!(std::fs::read(&dst).unwrap(), b"new content");
    }

    #[test]
    fn atomic_replace_overwrites_existing_dst() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&dst, b"OLD").unwrap();
        std::fs::write(&staging, b"NEW").unwrap();

        atomic_replace(&staging, &dst).expect("rename");
        assert_eq!(std::fs::read(&dst).unwrap(), b"NEW");
    }

    #[test]
    fn atomic_replace_cleans_up_staging_on_error() {
        // Rename to a nonexistent parent dir; staging should be removed.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        std::fs::write(&staging, b"x").unwrap();
        let bad_dst = dir.path().join("no_such_subdir/dst.bin");

        let r = atomic_replace(&staging, &bad_dst);
        assert!(r.is_err());
        assert!(
            !staging.exists(),
            "staging should be cleaned up on failure"
        );
    }

    #[test]
    fn atomic_replace_error_carries_paths() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        std::fs::write(&staging, b"x").unwrap();
        let bad_dst = dir.path().join("no_such_subdir/dst.bin");

        match atomic_replace(&staging, &bad_dst) {
            Err(AtomicError::Rename { staging: s, dst: d, .. }) => {
                assert!(s.ends_with("staging.bin"));
                assert!(d.ends_with("dst.bin"));
            }
            other => panic!("expected Rename error, got {other:?}"),
        }
    }
}
