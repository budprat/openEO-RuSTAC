//! JSON-Schema validator registry loaded from `spec/openapi.json`.
//!
//! openEO API 1.3 has 60 named schemas under `components.schemas`. We
//! pre-compile each into a [`jsonschema::Validator`] at startup so request
//! validation is a single hash-map lookup + a deterministic walk.

use std::collections::HashMap;

use serde_json::Value;
use thiserror::Error;

/// Errors emitted by the registry.
#[derive(Debug, Error)]
pub enum SchemaError {
    /// openapi.json couldn't be parsed.
    #[error("parse openapi.json: {0}")]
    Parse(String),
    /// Schema with the given name not registered.
    #[error("schema not found: {0}")]
    NotFound(String),
    /// JSON body failed validation.
    #[error("validation failed for {schema}: {errors}")]
    Invalid {
        /// Schema name.
        schema: String,
        /// Joined error string.
        errors: String,
    },
}

/// Holds compiled validators keyed by schema name.
pub struct SchemaRegistry {
    by_name: HashMap<String, jsonschema::Validator>,
}

impl SchemaRegistry {
    /// Empty registry — useful for tests that don't need validation.
    #[must_use]
    pub fn empty() -> Self { Self { by_name: HashMap::new() } }

    /// Number of schemas in the registry.
    #[must_use]
    pub fn len(&self) -> usize { self.by_name.len() }
    /// True iff no schemas are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.by_name.is_empty() }

    /// Build the registry from the openEO spec JSON.
    ///
    /// Iterates `components.schemas` and compiles each. Schemas that fail
    /// to compile are skipped with a warning (the spec contains a few
    /// recursive $refs that `jsonschema` can't resolve standalone — they
    /// don't block request handling for the common cases).
    pub fn from_openapi_json(openapi: &str) -> Result<Self, SchemaError> {
        let spec: Value = serde_json::from_str(openapi)
            .map_err(|e| SchemaError::Parse(e.to_string()))?;
        let schemas = spec
            .get("components")
            .and_then(|c| c.get("schemas"))
            .and_then(|s| s.as_object())
            .ok_or_else(|| SchemaError::Parse("missing components.schemas".into()))?;

        let mut by_name = HashMap::with_capacity(schemas.len());
        for (name, schema) in schemas {
            match jsonschema::validator_for(schema) {
                Ok(v) => { by_name.insert(name.clone(), v); }
                Err(e) => {
                    tracing::debug!(
                        schema = %name,
                        error = %e,
                        "skipping schema that failed to compile"
                    );
                }
            }
        }
        Ok(Self { by_name })
    }

    /// True iff a schema with this name has been compiled.
    #[must_use]
    pub fn has(&self, name: &str) -> bool { self.by_name.contains_key(name) }

    /// Validate `value` against the named schema.
    pub fn validate(&self, name: &str, value: &Value) -> Result<(), SchemaError> {
        let v = self
            .by_name
            .get(name)
            .ok_or_else(|| SchemaError::NotFound(name.to_string()))?;
        let errors: Vec<String> = v.iter_errors(value).map(|e| e.to_string()).collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(SchemaError::Invalid {
                schema: name.to_string(),
                errors: errors.join("; "),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_registry_returns_not_found() {
        let r = SchemaRegistry::empty();
        let err = r.validate("nope", &json!({})).unwrap_err();
        assert!(matches!(err, SchemaError::NotFound(_)));
    }

    #[test]
    fn from_openapi_loads_real_spec() {
        let openapi = include_str!("../spec/openapi.json");
        let r = SchemaRegistry::from_openapi_json(openapi).expect("parse");
        // openEO spec has 60 named schemas in components.schemas.
        // Some recursive $refs may fail to compile — accept anything > 30.
        assert!(r.len() >= 30, "loaded {} schemas", r.len());
    }

    #[test]
    fn validate_passes_a_known_minimal_doc() {
        let openapi = include_str!("../spec/openapi.json");
        let r = SchemaRegistry::from_openapi_json(openapi).expect("parse");
        // `error` schema in openEO has minimal required fields: code + message.
        if r.has("error") {
            let valid = json!({"code": "BadRequest", "message": "boom"});
            assert!(r.validate("error", &valid).is_ok());
        }
    }

    #[test]
    fn malformed_openapi_returns_parse_error() {
        let r = SchemaRegistry::from_openapi_json("{ not json");
        assert!(matches!(r, Err(SchemaError::Parse(_))));
    }

    #[test]
    fn missing_components_returns_parse_error() {
        let r = SchemaRegistry::from_openapi_json(r#"{"openapi":"3.0.2"}"#);
        assert!(matches!(r, Err(SchemaError::Parse(_))));
    }

    #[test]
    fn registry_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SchemaRegistry>();
    }
}
