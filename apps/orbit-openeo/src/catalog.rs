//! Collection catalog trait — pluggable backing for `/collections` and
//! `/collections/{id}`.
//!
//! `eo-catalog::StacClient` is search-shaped (items in / item ids out);
//! the openEO `/collections` endpoints want collection-shaped metadata.
//! Rather than torque the upstream trait, we declare a small openEO-side
//! interface here and ship two implementations:
//!
//! - [`InMemoryCatalog`] — fixed list, useful for tests and offline demos.
//! - [`StacCatalog`]     — backed by an HTTP STAC API (future session).
//!
//! Both expose the openEO 1.3.0 Collection JSON shape directly so the
//! HTTP layer is a thin pass-through.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// openEO 1.3.0 Collection summary (subset). Use `extra` to round-trip
/// any vendor-specific fields the spec permits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    /// Stable collection id.
    pub id: String,
    /// STAC version this collection conforms to.
    pub stac_version: String,
    /// Free-text description.
    #[serde(default)]
    pub description: String,
    /// License identifier (SPDX or `"proprietary"`).
    #[serde(default = "default_license")]
    pub license: String,
    /// Anything else from the source document.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

fn default_license() -> String { "proprietary".into() }

impl Collection {
    /// Convenience constructor for fixtures.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            stac_version: "1.0.0".into(),
            description: String::new(),
            license: default_license(),
            extra: serde_json::Map::new(),
        }
    }
}

/// Errors a catalog backend can surface.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// Requested collection id wasn't found.
    #[error("collection not found: {0}")]
    NotFound(String),
    /// Backend HTTP/IO failure.
    #[error("catalog backend error: {0}")]
    Backend(String),
}

/// Pluggable catalog backend.
#[async_trait]
pub trait CollectionCatalog: Send + Sync {
    /// List all known collections.
    async fn list(&self) -> Result<Vec<Collection>, CatalogError>;
    /// Fetch one collection by id.
    async fn get(&self, id: &str) -> Result<Collection, CatalogError>;
}

/// In-memory catalog driven by a fixed `Vec<Collection>`.
#[derive(Debug, Default)]
pub struct InMemoryCatalog {
    inner: Vec<Collection>,
}

impl InMemoryCatalog {
    /// New empty catalog.
    #[must_use]
    pub fn empty() -> Self { Self { inner: Vec::new() } }

    /// New catalog seeded with known collections.
    #[must_use]
    pub fn with_collections(items: Vec<Collection>) -> Self {
        Self { inner: items }
    }

    /// Replace the catalog contents.
    pub fn set(&mut self, items: Vec<Collection>) { self.inner = items; }
}

#[async_trait]
impl CollectionCatalog for InMemoryCatalog {
    async fn list(&self) -> Result<Vec<Collection>, CatalogError> {
        Ok(self.inner.clone())
    }

    async fn get(&self, id: &str) -> Result<Collection, CatalogError> {
        self.inner
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound(id.into()))
    }
}

// ---------------------------------------------------------------------
// HttpStacCatalog — backed by a remote STAC API (e.g. Element84 Earth Search).
// ---------------------------------------------------------------------

/// `CollectionCatalog` implementation that proxies through to a remote
/// STAC API. Tested against the Element84 Earth Search endpoint:
/// `https://earth-search.aws.element84.com/v1`.
///
/// Two endpoints are consumed:
/// - `GET {base}/collections`        → `{"collections":[…], "links":[…]}`
/// - `GET {base}/collections/{id}`   → single collection JSON (404 → NotFound)
///
/// The implementation does not paginate. Element84's `/collections` is a
/// single-page response. A `cursor`-aware variant lands when we hit a
/// backend that paginates aggressively.
pub struct HttpStacCatalog {
    base_url: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for HttpStacCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpStacCatalog")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl HttpStacCatalog {
    /// Build a catalog backed by `base_url`. The URL must NOT include a
    /// trailing slash; we append `/collections` and `/collections/{id}`
    /// directly.
    pub fn new(base_url: impl Into<String>) -> Self {
        let base = base_url.into();
        let base = base.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .user_agent(concat!("orbit-openeo/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { base_url: base, client }
    }

    /// Construct from an existing `reqwest::Client` (allows custom TLS,
    /// retry middleware, instrumentation, …).
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    /// Convenience: Element84 Earth Search v1.
    #[must_use]
    pub fn earth_search_v1() -> Self {
        Self::new("https://earth-search.aws.element84.com/v1")
    }
}

#[async_trait]
impl CollectionCatalog for HttpStacCatalog {
    async fn list(&self) -> Result<Vec<Collection>, CatalogError> {
        let url = format!("{}/collections", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| CatalogError::Backend(format!("{url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(CatalogError::Backend(format!("{url}: HTTP {status}")));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| CatalogError::Backend(format!("{url}: decode: {e}")))?;
        let raw_collections = body
            .get("collections")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(raw_collections.len());
        for c in raw_collections {
            match serde_json::from_value::<Collection>(c) {
                Ok(parsed) => out.push(parsed),
                Err(e) => tracing::warn!(error = %e, "skipping malformed collection"),
            }
        }
        Ok(out)
    }

    async fn get(&self, id: &str) -> Result<Collection, CatalogError> {
        let url = format!("{}/collections/{}", self.base_url, id);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| CatalogError::Backend(format!("{url}: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CatalogError::NotFound(id.into()));
        }
        if !status.is_success() {
            return Err(CatalogError::Backend(format!("{url}: HTTP {status}")));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| CatalogError::Backend(format!("{url}: decode: {e}")))?;
        serde_json::from_value::<Collection>(body)
            .map_err(|e| CatalogError::Backend(format!("{url}: parse: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn empty_catalog_lists_nothing() {
        let c = InMemoryCatalog::empty();
        assert!(c.list().await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn populated_catalog_lists_all() {
        let c = InMemoryCatalog::with_collections(vec![
            Collection::new("sentinel-2-l2a"),
            Collection::new("landsat-c2-l2"),
        ]);
        let listed = c.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "sentinel-2-l2a");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_returns_collection_by_id() {
        let c = InMemoryCatalog::with_collections(vec![Collection::new("s2")]);
        let got = c.get("s2").await.unwrap();
        assert_eq!(got.id, "s2");
        assert_eq!(got.stac_version, "1.0.0");
        assert_eq!(got.license, "proprietary");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_unknown_returns_not_found() {
        let c = InMemoryCatalog::empty();
        assert!(matches!(c.get("nope").await, Err(CatalogError::NotFound(_))));
    }

    #[test]
    fn collection_roundtrips_json_with_extras() {
        let mut extra = serde_json::Map::new();
        extra.insert("title".into(), Value::String("Sentinel-2 L2A".into()));
        let c = Collection {
            id: "s2".into(),
            stac_version: "1.0.0".into(),
            description: "test".into(),
            license: "CC-BY-4.0".into(),
            extra,
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["id"], "s2");
        assert_eq!(j["title"], "Sentinel-2 L2A");
        let back: Collection = serde_json::from_value(j).unwrap();
        assert_eq!(back, c);
    }

    // ------------------------------------------------------------------
    // HttpStacCatalog — wiremock-backed.
    // ------------------------------------------------------------------

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn collections_payload() -> serde_json::Value {
        serde_json::json!({
            "collections": [
                { "id": "sentinel-2-l2a", "stac_version": "1.0.0", "description": "S2 L2A",
                  "license": "proprietary", "title": "Sentinel-2 L2A" },
                { "id": "landsat-c2-l2", "stac_version": "1.0.0", "description": "Landsat C2 L2",
                  "license": "proprietary" }
            ],
            "links": []
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_list_parses_collections() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(collections_payload()))
            .mount(&server)
            .await;
        let cat = HttpStacCatalog::new(server.uri());
        let listed = cat.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "sentinel-2-l2a");
        assert_eq!(listed[1].id, "landsat-c2-l2");
        // Vendor extras survive via #[serde(flatten)] into Collection.extra.
        let title = listed[0].extra.get("title").and_then(|v| v.as_str());
        assert_eq!(title, Some("Sentinel-2 L2A"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_get_returns_collection() {
        let server = MockServer::start().await;
        let payload = serde_json::json!({
            "id": "sentinel-2-l2a",
            "stac_version": "1.0.0",
            "description": "Sentinel-2 L2A",
            "license": "proprietary",
            "title": "Sentinel-2 L2A"
        });
        Mock::given(method("GET"))
            .and(path("/collections/sentinel-2-l2a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload.clone()))
            .mount(&server)
            .await;
        let cat = HttpStacCatalog::new(server.uri());
        let got = cat.get("sentinel-2-l2a").await.unwrap();
        assert_eq!(got.id, "sentinel-2-l2a");
        assert_eq!(got.stac_version, "1.0.0");
        assert_eq!(got.extra.get("title").unwrap(), "Sentinel-2 L2A");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_get_404_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/collections/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let cat = HttpStacCatalog::new(server.uri());
        let r = cat.get("missing").await;
        assert!(matches!(r, Err(CatalogError::NotFound(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_500_maps_to_backend() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/collections"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let cat = HttpStacCatalog::new(server.uri());
        let r = cat.list().await;
        assert!(matches!(r, Err(CatalogError::Backend(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_trailing_slash_in_base_url_is_handled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(collections_payload()))
            .mount(&server)
            .await;
        // Trailing slash should not cause "/collections/" with no path-match.
        let cat = HttpStacCatalog::new(format!("{}/", server.uri()));
        let listed = cat.list().await.unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn earth_search_v1_constructor_uses_element84_url() {
        let cat = HttpStacCatalog::earth_search_v1();
        let s = format!("{cat:?}");
        assert!(s.contains("earth-search.aws.element84.com/v1"), "got {s}");
    }
}
