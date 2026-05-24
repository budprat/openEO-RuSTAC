//! HTTP Bearer auth — token extraction + constant-time compare.

use thiserror::Error;

/// Parsed `Authorization: Bearer …` header.
#[derive(Debug, Clone)]
pub struct BearerToken {
    raw: String,
}

/// Parse error for the Bearer scheme.
#[derive(Debug, Error)]
pub enum BearerError {
    /// Header value didn't start with "Bearer ".
    #[error("not a Bearer Authorization header")]
    NotBearer,
}

impl BearerToken {
    /// Parse a header value of the form `Bearer <token>`.
    pub fn parse(header: &str) -> Result<Self, BearerError> {
        let token = header.strip_prefix("Bearer ").ok_or(BearerError::NotBearer)?;
        Ok(Self { raw: token.to_string() })
    }

    /// Constant-time equality with the expected token.
    #[must_use]
    pub fn matches(&self, expected: &str) -> bool {
        constant_time_eq(self.raw.as_bytes(), expected.as_bytes())
    }

    /// The raw token bytes.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.raw }
}

/// Constant-time byte equality. Length difference returns false
/// immediately (the length itself is not a secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_bearer_prefix() {
        let b = BearerToken::parse("Bearer abc123").unwrap();
        assert_eq!(b.as_str(), "abc123");
    }

    #[test]
    fn parse_rejects_basic_scheme() {
        assert!(matches!(
            BearerToken::parse("Basic abc"),
            Err(BearerError::NotBearer)
        ));
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert!(matches!(
            BearerToken::parse("abc123"),
            Err(BearerError::NotBearer)
        ));
    }

    #[test]
    fn matches_correct_token() {
        let b = BearerToken::parse("Bearer secret").unwrap();
        assert!(b.matches("secret"));
        assert!(!b.matches("wrong"));
        assert!(!b.matches("secretX"));
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
