//! Filter predicates for STAC queries.

use serde_json::{json, Value as JsonValue};

/// Comparison operators for STAC numeric filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cmp {
    /// `<` (strictly less than).
    Less,
    /// `>` (strictly greater than).
    Greater,
    /// `<=` (less than or equal).
    LessEq,
    /// `>=` (greater than or equal).
    GreaterEq,
    /// `==` (equal).
    Equal,
    /// `!=` (not equal).
    NotEqual,
}

impl Cmp {
    /// Build the STAC query JSON fragment for `{op: value}`.
    pub fn to_json(self, value: f64) -> JsonValue {
        match self {
            Cmp::Less => json!({"lt": value}),
            Cmp::Greater => json!({"gt": value}),
            Cmp::LessEq => json!({"lte": value}),
            Cmp::GreaterEq => json!({"gte": value}),
            Cmp::Equal => json!({"eq": value}),
            Cmp::NotEqual => json!({"neq": value}),
        }
    }
}

/// Build the (property_name, predicate_json) pair for a cloud-cover filter.
pub fn cloudcover_filter(op: Cmp, value: f64) -> (&'static str, JsonValue) {
    ("eo:cloud_cover", op.to_json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **RED T2.3/A1**: `Cmp::Less.to_json(20.0) == {"lt": 20.0}`.
    #[test]
    fn cmp_less_serializes_to_lt() {
        assert_eq!(Cmp::Less.to_json(20.0), json!({"lt": 20.0}));
    }

    /// **RED T2.3/A2**: `Cmp::Greater.to_json(5.0) == {"gt": 5.0}`.
    #[test]
    fn cmp_greater_serializes_to_gt() {
        assert_eq!(Cmp::Greater.to_json(5.0), json!({"gt": 5.0}));
    }

    /// **RED T2.3/A3**: `Cmp::Equal.to_json(0.0) == {"eq": 0.0}`.
    #[test]
    fn cmp_equal_serializes_to_eq() {
        assert_eq!(Cmp::Equal.to_json(0.0), json!({"eq": 0.0}));
    }

    /// **RED T2.3/A4**: `cloudcover_filter(Cmp::Less, 20)` returns `("eo:cloud_cover", {"lt": 20})`.
    #[test]
    fn cloudcover_filter_uses_eo_cloud_cover_property() {
        let (key, pred) = cloudcover_filter(Cmp::Less, 20.0);
        assert_eq!(key, "eo:cloud_cover");
        assert_eq!(pred, json!({"lt": 20.0}));
    }
}
