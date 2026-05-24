//! Asset signing — each STAC provider has its own auth scheme.
//!
//! Provided signers:
//! - [`NoopSigner`] — pass-through. Useful for pre-signed catalogs and tests.
//! - [`BearerSigner`] — for OIDC / token-based catalogs that take Bearer
//!   tokens. Returns the unmodified URL; the auth value is meant to be
//!   added as an HTTP header by the caller via [`AssetSigner::header`].
//! - [`QueryStringSigner`] — appends a `?key=value&...` query string
//!   to the URL. Models Planetary Computer SAS-token append behaviour.

use std::collections::BTreeMap;

use async_trait::async_trait;
use thiserror::Error;

/// Errors surfaced by signers.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Token fetch / refresh from the upstream auth endpoint failed.
    #[error("auth request failed: {0}")]
    Request(String),
    /// Caller passed a URL the signer can't handle.
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),
    /// Credentials were missing or rejected.
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),
}

/// Sign / annotate an asset URL so it becomes fetchable.
#[async_trait]
pub trait AssetSigner: Send + Sync {
    /// Return a fetchable URL given a raw STAC asset href.
    async fn sign(&self, href: &str) -> Result<String, AuthError>;

    /// Optional HTTP headers to attach when fetching the signed URL.
    /// Default impl returns empty.
    async fn header(&self, _href: &str) -> Result<BTreeMap<String, String>, AuthError> {
        Ok(BTreeMap::new())
    }
}

/// No-op signer — useful for tests and pre-signed catalogs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSigner;

#[async_trait]
impl AssetSigner for NoopSigner {
    async fn sign(&self, href: &str) -> Result<String, AuthError> {
        Ok(href.to_string())
    }
}

/// Signer that emits an HTTP `Authorization: Bearer <token>` header.
///
/// URL is returned unchanged; the caller must apply [`AssetSigner::header`]
/// to the request.
#[derive(Debug, Clone)]
pub struct BearerSigner {
    token: String,
}

impl BearerSigner {
    /// Construct from a static token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }
}

#[async_trait]
impl AssetSigner for BearerSigner {
    async fn sign(&self, href: &str) -> Result<String, AuthError> {
        if self.token.is_empty() {
            return Err(AuthError::Unauthenticated("empty bearer token".into()));
        }
        Ok(href.to_string())
    }

    async fn header(&self, _href: &str) -> Result<BTreeMap<String, String>, AuthError> {
        let mut h = BTreeMap::new();
        h.insert("authorization".into(), format!("Bearer {}", self.token));
        Ok(h)
    }
}

/// Signer that appends a query string to the URL. Models the Planetary
/// Computer SAS-token shape: `?st=...&se=...&sig=...&sv=...`.
///
/// Append semantics:
/// - If `href` already has `?`, the params are joined with `&`.
/// - Otherwise `?` is inserted.
///
/// Param order is **deterministic** (BTreeMap sorts keys) — important for
/// cache-key stability and test reproducibility.
#[derive(Debug, Clone, Default)]
pub struct QueryStringSigner {
    params: BTreeMap<String, String>,
}

impl QueryStringSigner {
    /// Construct an empty signer.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Add a single `(key, value)` pair.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }
}

#[async_trait]
impl AssetSigner for QueryStringSigner {
    async fn sign(&self, href: &str) -> Result<String, AuthError> {
        // Only allow http/https/s3/gs/az. Caller controls upstream; this
        // is a sanity check so we don't accidentally append to "file://"
        // URLs etc.
        let lower = href.to_ascii_lowercase();
        if !lower.starts_with("http://")
            && !lower.starts_with("https://")
            && !lower.starts_with("s3://")
            && !lower.starts_with("gs://")
            && !lower.starts_with("az://")
        {
            return Err(AuthError::UnsupportedScheme(href.into()));
        }
        if self.params.is_empty() {
            return Ok(href.to_string());
        }
        let sep = if href.contains('?') { '&' } else { '?' };
        let qs: String = self
            .params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        Ok(format!("{href}{sep}{qs}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── NoopSigner ─────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn noop_signer_returns_input_unchanged() {
        let s = NoopSigner;
        let out = s.sign("https://example.com/a.tif").await.unwrap();
        assert_eq!(out, "https://example.com/a.tif");
        assert!(s.header("ignored").await.unwrap().is_empty());
    }

    // ── BearerSigner ───────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn bearer_signer_url_unchanged() {
        let s = BearerSigner::new("abc123");
        let out = s.sign("https://example.com/a.tif").await.unwrap();
        assert_eq!(out, "https://example.com/a.tif");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bearer_signer_emits_authorization_header() {
        let s = BearerSigner::new("abc123");
        let h = s.header("any").await.unwrap();
        assert_eq!(h.get("authorization").unwrap(), "Bearer abc123");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bearer_signer_empty_token_errors() {
        let s = BearerSigner::new("");
        assert!(matches!(s.sign("https://x").await, Err(AuthError::Unauthenticated(_))));
    }

    // ── QueryStringSigner ──────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_no_params_passes_through() {
        let s = QueryStringSigner::new();
        let out = s.sign("https://example.com/a.tif").await.unwrap();
        assert_eq!(out, "https://example.com/a.tif");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_appends_question_mark_when_no_query() {
        let s = QueryStringSigner::new().with("sv", "2024-06-01");
        let out = s.sign("https://example.com/a.tif").await.unwrap();
        assert_eq!(out, "https://example.com/a.tif?sv=2024-06-01");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_joins_with_ampersand_when_query_exists() {
        let s = QueryStringSigner::new().with("sig", "abc");
        let out = s.sign("https://example.com/a.tif?x=1").await.unwrap();
        assert_eq!(out, "https://example.com/a.tif?x=1&sig=abc");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_keys_sort_deterministically() {
        let s = QueryStringSigner::new()
            .with("z", "1")
            .with("a", "2")
            .with("m", "3");
        let out = s.sign("https://example.com/a.tif").await.unwrap();
        // BTreeMap sorts alphabetically.
        assert_eq!(out, "https://example.com/a.tif?a=2&m=3&z=1");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_planetary_computer_sas_shape() {
        // Real-ish SAS-token shape: st, se, sp, sig, sv.
        let s = QueryStringSigner::new()
            .with("st", "2024-06-01T00:00:00Z")
            .with("se", "2024-06-02T00:00:00Z")
            .with("sp", "rl")
            .with("sv", "2023-11-03")
            .with("sig", "deadbeef");
        let out = s.sign("https://pcsasprodweu.blob.core.windows.net/scene.tif").await.unwrap();
        assert!(out.contains("sig=deadbeef"));
        assert!(out.contains("sv=2023-11-03"));
        assert!(out.starts_with("https://pcsasprodweu.blob.core.windows.net/scene.tif?"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_rejects_unsupported_scheme() {
        let s = QueryStringSigner::new().with("k", "v");
        let r = s.sign("file:///etc/passwd").await;
        assert!(matches!(r, Err(AuthError::UnsupportedScheme(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn qs_signer_accepts_s3_scheme() {
        let s = QueryStringSigner::new().with("k", "v");
        let out = s.sign("s3://bucket/key").await.unwrap();
        assert_eq!(out, "s3://bucket/key?k=v");
    }

    #[test]
    fn auth_error_display_categories() {
        assert!(AuthError::Request("x".into()).to_string().contains("auth"));
        assert!(AuthError::UnsupportedScheme("ftp".into()).to_string().contains("ftp"));
        assert!(AuthError::Unauthenticated("token".into()).to_string().contains("unauthenticated"));
    }
}
