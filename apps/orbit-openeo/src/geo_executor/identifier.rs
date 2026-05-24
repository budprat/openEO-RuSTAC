//! Band-name / CRS-name validators — defense against path traversal.

use crate::executor::ExecError;

/// Allowlist for safe identifiers reaching the filesystem: alphanumeric
/// + underscore + dash + dot, 1-64 chars, no leading dash or dot.
///
/// Reason: bands/target_band/CRS names propagate into
/// `scratch_dir.join(name)`. Without this guard, `"../../../etc/passwd"`
/// or `"-rf"` reaches the filesystem.
pub(super) fn validate_identifier(name: &str, field: &str) -> Result<(), ExecError> {
    const MAX: usize = 64;
    if name.is_empty() || name.len() > MAX {
        return Err(ExecError::InvalidGraph(format!(
            "{field}: identifier length must be 1..={MAX} (got {})",
            name.len()
        )));
    }
    if matches!(name.chars().next(), Some('-' | '.')) {
        return Err(ExecError::InvalidGraph(format!(
            "{field}: identifier may not start with '-' or '.'"
        )));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return Err(ExecError::InvalidGraph(format!(
                "{field}: identifier may only contain [A-Za-z0-9_.-]; found {c:?}"
            )));
        }
    }
    // Block `..` traversal even when other rules pass.
    if name.contains("..") {
        return Err(ExecError::InvalidGraph(format!(
            "{field}: identifier may not contain '..'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_band_names() {
        for s in &["B04", "B08", "SCL", "ndvi_out", "user.123", "B11", "mask"] {
            assert!(validate_identifier(s, "band").is_ok(), "should accept: {s}");
        }
    }

    #[test]
    fn rejects_traversal() {
        for s in &["../../etc/passwd", "..", "a/../b", "x..y"] {
            assert!(
                matches!(validate_identifier(s, "band"), Err(ExecError::InvalidGraph(_))),
                "should reject traversal: {s}"
            );
        }
    }

    #[test]
    fn rejects_leading_dash() {
        assert!(matches!(
            validate_identifier("-rf", "band"),
            Err(ExecError::InvalidGraph(_))
        ));
        assert!(matches!(
            validate_identifier("--config", "band"),
            Err(ExecError::InvalidGraph(_))
        ));
    }

    #[test]
    fn rejects_leading_dot() {
        assert!(matches!(
            validate_identifier(".env", "band"),
            Err(ExecError::InvalidGraph(_))
        ));
        assert!(matches!(
            validate_identifier(".", "band"),
            Err(ExecError::InvalidGraph(_))
        ));
    }

    #[test]
    fn rejects_special_chars() {
        for s in &["B04;rm -rf", "B*", "a b", "a/b", "a\\b", "a:b", "a$b", "a\0b"] {
            assert!(
                matches!(validate_identifier(s, "band"), Err(ExecError::InvalidGraph(_))),
                "should reject special-char: {s:?}"
            );
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            validate_identifier("", "band"),
            Err(ExecError::InvalidGraph(_))
        ));
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(65);
        assert!(matches!(
            validate_identifier(&s, "band"),
            Err(ExecError::InvalidGraph(_))
        ));
    }

    #[test]
    fn accepts_exactly_64_chars() {
        let s = "a".repeat(64);
        assert!(validate_identifier(&s, "band").is_ok());
    }
}
