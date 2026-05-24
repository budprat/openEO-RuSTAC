//! Auth interceptors for openEO routes.
//!
//! openEO API 1.3.0 advertises two HTTP auth schemes (`securitySchemes`):
//! - `Bearer` (HTTP Bearer)
//! - `Basic` (HTTP Basic)
//!
//! At the API level — per-route security is declared via the `security:`
//! key on each operation in the spec. For the initial implementation we
//! collapse them into a single policy enum; per-route enforcement is a
//! future refinement.

pub mod basic;
pub mod bearer;

use serde::{Deserialize, Serialize};

pub use basic::BasicCredentials;
pub use bearer::BearerToken;

/// Active auth policy for the server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuthPolicy {
    /// No auth — every request is accepted. Useful for dev / loopback.
    Open,
    /// Require an HTTP Bearer token matching `token`.
    Bearer {
        /// Expected token value (compared in constant time).
        token: String,
    },
    /// Require an HTTP Basic credential matching `username`/`password`.
    Basic {
        /// Expected username.
        username: String,
        /// Expected password.
        password: String,
    },
    /// Accept either a valid Bearer token *or* a valid Basic credential.
    Any {
        /// Bearer expected value (None = bearer disabled).
        bearer: Option<String>,
        /// Basic expected value (None = basic disabled).
        basic: Option<(String, String)>,
    },
}

/// Result of checking the `Authorization` header against [`AuthPolicy`].
#[derive(Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// No auth was required.
    Open,
    /// Required auth matched.
    Authenticated,
    /// Header missing.
    Missing,
    /// Header present but scheme not recognised.
    BadScheme,
    /// Credentials present but did not match.
    BadCredentials,
}

impl AuthPolicy {
    /// Inspect an HTTP `Authorization` header value and decide if the
    /// request should proceed.
    pub fn check(&self, header: Option<&str>) -> AuthOutcome {
        match self {
            Self::Open => AuthOutcome::Open,
            Self::Bearer { token } => match header {
                None => AuthOutcome::Missing,
                Some(h) => match BearerToken::parse(h) {
                    Ok(b) if b.matches(token) => AuthOutcome::Authenticated,
                    Ok(_) => AuthOutcome::BadCredentials,
                    Err(_) => AuthOutcome::BadScheme,
                },
            },
            Self::Basic { username, password } => match header {
                None => AuthOutcome::Missing,
                Some(h) => match BasicCredentials::parse(h) {
                    Ok(b) if b.matches(username, password) => AuthOutcome::Authenticated,
                    Ok(_) => AuthOutcome::BadCredentials,
                    Err(_) => AuthOutcome::BadScheme,
                },
            },
            Self::Any { bearer, basic } => match header {
                None => AuthOutcome::Missing,
                Some(h) => {
                    if let Some(tok) = bearer {
                        if let Ok(b) = BearerToken::parse(h) {
                            if b.matches(tok) { return AuthOutcome::Authenticated; }
                            return AuthOutcome::BadCredentials;
                        }
                    }
                    if let Some((u, p)) = basic {
                        if let Ok(c) = BasicCredentials::parse(h) {
                            if c.matches(u, p) { return AuthOutcome::Authenticated; }
                            return AuthOutcome::BadCredentials;
                        }
                    }
                    AuthOutcome::BadScheme
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_policy_accepts_anything() {
        let p = AuthPolicy::Open;
        assert_eq!(p.check(None), AuthOutcome::Open);
        assert_eq!(p.check(Some("anything")), AuthOutcome::Open);
    }

    #[test]
    fn bearer_policy_requires_header() {
        let p = AuthPolicy::Bearer { token: "secret".into() };
        assert_eq!(p.check(None), AuthOutcome::Missing);
    }

    #[test]
    fn bearer_policy_matches_correct_token() {
        let p = AuthPolicy::Bearer { token: "secret".into() };
        assert_eq!(p.check(Some("Bearer secret")), AuthOutcome::Authenticated);
    }

    #[test]
    fn bearer_policy_rejects_wrong_token() {
        let p = AuthPolicy::Bearer { token: "secret".into() };
        assert_eq!(p.check(Some("Bearer wrong")), AuthOutcome::BadCredentials);
    }

    #[test]
    fn bearer_policy_rejects_basic_scheme() {
        let p = AuthPolicy::Bearer { token: "secret".into() };
        assert_eq!(p.check(Some("Basic dXNlcjpwYXNz")), AuthOutcome::BadScheme);
    }

    #[test]
    fn basic_policy_matches_correct_credential() {
        let p = AuthPolicy::Basic {
            username: "alice".into(),
            password: "wonder".into(),
        };
        // base64("alice:wonder") = "YWxpY2U6d29uZGVy"
        assert_eq!(p.check(Some("Basic YWxpY2U6d29uZGVy")), AuthOutcome::Authenticated);
    }

    #[test]
    fn any_policy_accepts_either_scheme() {
        let p = AuthPolicy::Any {
            bearer: Some("tok".into()),
            basic: Some(("u".into(), "p".into())),
        };
        assert_eq!(p.check(Some("Bearer tok")), AuthOutcome::Authenticated);
        // base64("u:p") = "dTpw"
        assert_eq!(p.check(Some("Basic dTpw")), AuthOutcome::Authenticated);
    }
}
