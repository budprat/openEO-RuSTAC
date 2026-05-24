//! Per-route security parsed from `openapi.json`.
//!
//! At startup we walk `paths.<path>.<method>.security` and build a map
//! keyed by `(Method, normalised_path)`. A middleware looks up each
//! incoming request against this map; routes with required schemes are
//! gated by [`AuthPolicy`].
//!
//! OpenAPI path templates use `{name}` (RFC 6570 simple). Axum 0.8 uses
//! `{name}` too — the strings match without translation. The middleware
//! relies on `axum::extract::MatchedPath` to recover the template so it
//! works regardless of the captured variable values.

use std::collections::HashMap;

use serde_json::Value;
use thiserror::Error;

/// Required schemes for one route. Either-of semantics (matching OpenAPI's
/// `security:` array — each element is an alternative).
///
/// The strings are scheme names as declared in `components.securitySchemes`
/// (e.g. `"Bearer"`, `"Basic"`). Empty inner vec means "no scheme" which
/// in OpenAPI signals public access.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RouteSecurity {
    /// Each outer element is a fully-satisfying alternative; each inner
    /// scheme within an alternative is jointly required (rare in openEO).
    pub alternatives: Vec<Vec<String>>,
}

impl RouteSecurity {
    /// True iff the route is public (empty alternatives, or any
    /// alternative with zero schemes).
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.alternatives.is_empty()
            || self.alternatives.iter().any(|a| a.is_empty())
    }

    /// True iff any alternative requires the given scheme name.
    #[must_use]
    pub fn requires_scheme(&self, scheme: &str) -> bool {
        self.alternatives.iter().any(|a| a.iter().any(|s| s == scheme))
    }
}

/// Map `(method, path_template) → RouteSecurity`.
#[derive(Debug, Default)]
pub struct RouteSecurityMap {
    by_key: HashMap<(String, String), RouteSecurity>,
}

/// Errors while building the security map.
#[derive(Debug, Error)]
pub enum SecurityMapError {
    /// openapi.json parse failure.
    #[error("openapi parse: {0}")]
    Parse(String),
    /// `paths` section absent.
    #[error("openapi missing paths")]
    NoPaths,
}

impl RouteSecurityMap {
    /// Empty map — useful for tests.
    #[must_use]
    pub fn empty() -> Self { Self::default() }

    /// Number of (method, path) entries.
    #[must_use]
    pub fn len(&self) -> usize { self.by_key.len() }
    /// True iff empty.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.by_key.is_empty() }

    /// Build from openapi.json text.
    pub fn from_openapi_json(openapi: &str) -> Result<Self, SecurityMapError> {
        let spec: Value = serde_json::from_str(openapi).map_err(|e| SecurityMapError::Parse(e.to_string()))?;
        Self::from_spec(&spec)
    }

    /// Build from an already-parsed JSON value.
    pub fn from_spec(spec: &Value) -> Result<Self, SecurityMapError> {
        let paths = spec
            .get("paths")
            .and_then(|p| p.as_object())
            .ok_or(SecurityMapError::NoPaths)?;
        let mut map = Self::default();
        for (path, ops) in paths {
            let ops_obj = match ops.as_object() {
                Some(o) => o,
                None => continue,
            };
            for (method, op) in ops_obj {
                let m_upper = method.to_uppercase();
                if !matches!(
                    m_upper.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
                ) {
                    continue;
                }
                let sec = op
                    .get("security")
                    .and_then(|s| s.as_array())
                    .map(|a| parse_security_array(a.as_slice()))
                    .unwrap_or_default();
                map.by_key.insert((m_upper, path.clone()), RouteSecurity { alternatives: sec });
            }
        }
        Ok(map)
    }

    /// Look up the security policy for a route.
    #[must_use]
    pub fn get(&self, method: &str, path_template: &str) -> Option<&RouteSecurity> {
        self.by_key.get(&(method.to_uppercase(), path_template.to_string()))
    }
}

fn parse_security_array(arr: &[Value]) -> Vec<Vec<String>> {
    arr.iter()
        .filter_map(|alt| alt.as_object())
        .map(|alt| alt.keys().cloned().collect::<Vec<_>>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_spec() -> Value {
        json!({
            "openapi": "3.0.2",
            "paths": {
                "/": {
                    "get": {} // no security key → public
                },
                "/jobs": {
                    "get":  { "security": [{ "Bearer": [] }] },
                    "post": { "security": [{ "Bearer": [] }, { "Basic": [] }] } // either
                },
                "/credentials/basic": {
                    "get": { "security": [{ "Basic": [] }] }
                }
            }
        })
    }

    #[test]
    fn empty_returns_no_entries() {
        let m = RouteSecurityMap::empty();
        assert!(m.is_empty());
        assert!(m.get("GET", "/").is_none());
    }

    #[test]
    fn from_spec_indexes_methods() {
        let m = RouteSecurityMap::from_spec(&fixture_spec()).unwrap();
        assert!(m.len() >= 4);
        assert!(m.get("GET", "/").unwrap().is_public());
        assert!(!m.get("GET", "/jobs").unwrap().is_public());
        assert!(m.get("POST", "/jobs").unwrap().requires_scheme("Bearer"));
        assert!(m.get("POST", "/jobs").unwrap().requires_scheme("Basic"));
    }

    #[test]
    fn unknown_route_returns_none() {
        let m = RouteSecurityMap::from_spec(&fixture_spec()).unwrap();
        assert!(m.get("DELETE", "/unknown").is_none());
    }

    #[test]
    fn case_normalisation_on_method() {
        let m = RouteSecurityMap::from_spec(&fixture_spec()).unwrap();
        assert!(m.get("get", "/").is_some(), "method must be case-insensitive");
        assert!(m.get("Get", "/").is_some());
    }

    #[test]
    fn no_paths_section_errors() {
        let bad = serde_json::json!({"openapi": "3.0.2"});
        assert!(matches!(
            RouteSecurityMap::from_spec(&bad),
            Err(SecurityMapError::NoPaths)
        ));
    }

    #[test]
    fn malformed_json_errors() {
        assert!(matches!(
            RouteSecurityMap::from_openapi_json("{ not"),
            Err(SecurityMapError::Parse(_))
        ));
    }

    #[test]
    fn real_openapi_loads_with_path_count_match() {
        let openapi = include_str!("../spec/openapi.json");
        let m = RouteSecurityMap::from_openapi_json(openapi).expect("parse");
        // openapi.json has 27 paths; methods will be at least that many.
        assert!(m.len() >= 27, "loaded {} entries", m.len());
    }

    #[test]
    fn route_security_alternatives() {
        let s = RouteSecurity {
            alternatives: vec![vec!["Bearer".into()], vec!["Basic".into()]],
        };
        assert!(s.requires_scheme("Bearer"));
        assert!(s.requires_scheme("Basic"));
        assert!(!s.requires_scheme("OAuth2"));
        assert!(!s.is_public());
    }

    #[test]
    fn empty_alternative_marks_public() {
        let s = RouteSecurity { alternatives: vec![vec![]] };
        assert!(s.is_public());
    }
}
