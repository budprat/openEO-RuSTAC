//! `ImageQueryBuilder` — fluent construction of a provider-portable STAC query.

use crate::dsl::{Collection, Cmp, Intersects};
use crate::error::{Error, Result};

/// Fluent builder for a remote imagery query.
///
/// ```ignore
/// use orbit_geo::dsl::{ImageQueryBuilder, Collection, Intersects, Cmp};
/// use orbit_geo::providers::Provider;
///
/// let query = ImageQueryBuilder::new()
///     .provider(Provider::EARTH_SEARCH_V1)
///     .collection(Collection::Sentinel2)
///     .intersects(Intersects::Bbox([148.0, -29.0, 149.0, -28.0]))
///     .datetime("2024-01-01T00:00:00Z/2024-12-31T23:59:59Z")
///     .cloudcover(Cmp::Less, 20.0)
///     .bands(vec!["red".into(), "nir".into()])
///     .limit(10)
///     .build()?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct ImageQueryBuilder {
    provider: Option<&'static str>,
    collection: Option<Collection>,
    intersects: Option<Intersects>,
    datetime: Option<String>,
    bands: Option<Vec<String>>,
    cloudcover: Option<(Cmp, f64)>,
    limit: Option<u32>,
}

impl ImageQueryBuilder {
    /// Start a new empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Required: which STAC endpoint to query.
    pub fn provider(mut self, p: &'static str) -> Self {
        self.provider = Some(p);
        self
    }

    /// Required: which satellite collection.
    pub fn collection(mut self, c: Collection) -> Self {
        self.collection = Some(c);
        self
    }

    /// Required: spatial / id filter.
    pub fn intersects(mut self, i: Intersects) -> Self {
        self.intersects = Some(i);
        self
    }

    /// Optional: ISO-8601 datetime or interval.
    pub fn datetime(mut self, dt: impl Into<String>) -> Self {
        self.datetime = Some(dt.into());
        self
    }

    /// Optional: which asset bands to select.
    pub fn bands(mut self, b: Vec<String>) -> Self {
        self.bands = Some(b);
        self
    }

    /// Optional: cloud-cover filter.
    pub fn cloudcover(mut self, op: Cmp, value: f64) -> Self {
        self.cloudcover = Some((op, value));
        self
    }

    /// Optional: result limit.
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }

    /// Materialize the built query. Errors if required fields are missing.
    pub fn build(self) -> Result<ImageQuery> {
        let provider = self
            .provider
            .ok_or_else(|| Error::invalid_builder("ImageQueryBuilder: provider not set"))?;
        let collection = self
            .collection
            .ok_or_else(|| Error::invalid_builder("ImageQueryBuilder: collection not set"))?;
        let intersects = self
            .intersects
            .ok_or_else(|| Error::invalid_builder("ImageQueryBuilder: intersects not set"))?;
        Ok(ImageQuery {
            provider,
            collection,
            intersects,
            datetime: self.datetime,
            bands: self.bands,
            cloudcover: self.cloudcover,
            limit: self.limit,
        })
    }
}

/// A materialized query — output of `ImageQueryBuilder::build()`.
#[derive(Debug, Clone)]
pub struct ImageQuery {
    /// STAC endpoint URL (one of the `Provider::*` constants).
    pub provider: &'static str,
    /// Canonical satellite collection identifier.
    pub collection: Collection,
    /// Spatial / item-id filter.
    pub intersects: Intersects,
    /// Optional ISO-8601 datetime or interval (e.g. `"2024-01-01/2024-12-31"`).
    pub datetime: Option<String>,
    /// Optional list of asset bands to select (canonical names or provider-specific).
    pub bands: Option<Vec<String>>,
    /// Optional cloud-cover predicate, e.g. `(Cmp::Less, 20.0)`.
    pub cloudcover: Option<(Cmp, f64)>,
    /// Optional result limit.
    pub limit: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;

    /// **RED T2.4/A1**: full chain builds successfully.
    #[test]
    fn full_chain_builds_successfully() {
        let q = ImageQueryBuilder::new()
            .provider(Provider::EARTH_SEARCH_V1)
            .collection(Collection::Sentinel2)
            .intersects(Intersects::Bbox([148.0, -29.0, 149.0, -28.0]))
            .datetime("2024-01-01T00:00:00Z/2024-12-31T23:59:59Z")
            .cloudcover(Cmp::Less, 20.0)
            .build()
            .expect("build ok");
        assert_eq!(q.provider, Provider::EARTH_SEARCH_V1);
        assert_eq!(q.collection, Collection::Sentinel2);
        assert!(q.cloudcover.is_some());
    }

    /// **RED T2.4/A2**: build errors when provider missing.
    #[test]
    fn build_errors_when_provider_missing() {
        let r = ImageQueryBuilder::new()
            .collection(Collection::Sentinel2)
            .intersects(Intersects::Bbox([0.0, 0.0, 1.0, 1.0]))
            .build();
        assert!(r.is_err(), "missing provider must error");
    }

    /// **RED T2.4/A2b**: build errors when collection missing.
    #[test]
    fn build_errors_when_collection_missing() {
        let r = ImageQueryBuilder::new()
            .provider(Provider::EARTH_SEARCH_V1)
            .intersects(Intersects::Bbox([0.0, 0.0, 1.0, 1.0]))
            .build();
        assert!(r.is_err(), "missing collection must error");
    }

    /// **RED T2.4/A2c**: build errors when intersects missing.
    #[test]
    fn build_errors_when_intersects_missing() {
        let r = ImageQueryBuilder::new()
            .provider(Provider::EARTH_SEARCH_V1)
            .collection(Collection::Sentinel2)
            .build();
        assert!(r.is_err(), "missing intersects must error");
    }

    /// **RED T2.4/A3**: minimal valid build (only required fields).
    #[test]
    fn minimal_valid_build_preserves_fields() {
        let q = ImageQueryBuilder::new()
            .provider(Provider::PLANETARY_COMPUTER)
            .collection(Collection::Landsat8)
            .intersects(Intersects::Scene(vec!["foo".to_string()]))
            .build()
            .expect("minimal build ok");
        assert_eq!(q.provider, Provider::PLANETARY_COMPUTER);
        assert_eq!(q.collection, Collection::Landsat8);
        assert!(q.intersects.as_scene().is_some());
        assert!(q.datetime.is_none());
    }
}
