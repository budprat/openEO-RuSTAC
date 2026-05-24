//! `StacClient` trait + `SearchRequest` parameter object + an
//! [`InMemoryStacClient`] reference implementation.
//!
//! The in-memory client is a test/dev harness. It implements the
//! `search` semantics from STAC API 1.0.0:
//!
//! - `collections` (any-of)
//! - `ids` (any-of) — short-circuits when set
//! - `bbox` (geographic-rectangle intersection with item bbox)
//! - `datetime` (RFC3339 single instant or `start/end` interval)
//! - `limit` (server-side cap; default 100)
//!
//! Reqwest-backed live clients land in subsequent sessions.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Parameters for a STAC `/search` POST.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchRequest {
    /// Bounding box `[west, south, east, north]` in EPSG:4326.
    pub bbox: Option<[f64; 4]>,
    /// Datetime interval (RFC 3339 single instant or `start/end`).
    pub datetime: Option<String>,
    /// Collection ids to scope the search to.
    pub collections: Vec<String>,
    /// Item ids to fetch directly.
    pub ids: Vec<String>,
    /// Free-form CQL2-text predicate (post-parsed by the client).
    pub filter: Option<String>,
    /// Page size; server may cap.
    pub limit: Option<u32>,
}

impl SearchRequest {
    /// Builder helper: set the bounding box.
    #[must_use]
    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.bbox = Some(bbox);
        self
    }
    /// Builder helper: append a collection id.
    #[must_use]
    pub fn with_collection(mut self, c: impl Into<String>) -> Self {
        self.collections.push(c.into());
        self
    }
    /// Builder helper: set the datetime expression.
    #[must_use]
    pub fn with_datetime(mut self, d: impl Into<String>) -> Self {
        self.datetime = Some(d.into());
        self
    }
    /// Builder helper: set the result limit.
    #[must_use]
    pub fn with_limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }
}

/// Asynchronous STAC API client surface.
#[async_trait]
pub trait StacClient: Send + Sync {
    /// Errors surfaced by the implementation.
    type Error: std::error::Error + Send + Sync + 'static;
    /// Hit `/search` and return matching item ids (paginated by the impl).
    async fn search(&self, req: &SearchRequest) -> std::result::Result<Vec<String>, Self::Error>;
}

// ─────────────────────────────────────────────────────────────────────
// InMemoryStacClient
// ─────────────────────────────────────────────────────────────────────

/// A simplified STAC item record for the in-memory client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InMemoryItem {
    /// STAC item id.
    pub id: String,
    /// Owning collection id.
    pub collection: String,
    /// Geographic bbox `[west, south, east, north]` in EPSG:4326.
    pub bbox: [f64; 4],
    /// Acquisition datetime in RFC 3339.
    pub datetime: String,
}

/// Errors from the in-memory client.
#[derive(Debug, Error)]
pub enum InMemoryError {
    /// Caller passed a malformed datetime expression.
    #[error("invalid datetime expression: {0}")]
    BadDatetime(String),
}

/// A purely in-memory `StacClient` for tests and dev workflows.
///
/// Construct with [`InMemoryStacClient::with_items`], then [`StacClient::search`]
/// applies the spec filters in-process. Cloning is cheap (Arc-wrapped).
#[derive(Clone, Debug, Default)]
pub struct InMemoryStacClient {
    items: Arc<Vec<InMemoryItem>>,
}

impl InMemoryStacClient {
    /// Construct from a fixed item list.
    #[must_use]
    pub fn with_items(items: Vec<InMemoryItem>) -> Self {
        Self { items: Arc::new(items) }
    }

    /// Number of items in the catalog.
    #[must_use]
    pub fn len(&self) -> usize { self.items.len() }
    /// True iff the catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool { self.items.is_empty() }
}

#[async_trait]
impl StacClient for InMemoryStacClient {
    type Error = InMemoryError;

    async fn search(&self, req: &SearchRequest) -> std::result::Result<Vec<String>, Self::Error> {
        // Parse the datetime once.
        let dt_filter = req
            .datetime
            .as_deref()
            .map(parse_datetime)
            .transpose()?;

        let mut out: Vec<String> = self
            .items
            .iter()
            .filter(|it| {
                // ids — short-circuit (no other predicates run if ids match).
                if !req.ids.is_empty() {
                    return req.ids.iter().any(|i| i == &it.id);
                }
                // collections any-of.
                if !req.collections.is_empty()
                    && !req.collections.iter().any(|c| c == &it.collection)
                {
                    return false;
                }
                // bbox intersection (axis-aligned in 4326).
                if let Some(b) = req.bbox {
                    if !bbox_intersects(b, it.bbox) {
                        return false;
                    }
                }
                // datetime predicate.
                if let Some((start, end)) = &dt_filter {
                    if !datetime_matches(start.as_deref(), end.as_deref(), &it.datetime) {
                        return false;
                    }
                }
                true
            })
            .map(|it| it.id.clone())
            .collect();

        // Limit.
        let limit = req.limit.unwrap_or(100) as usize;
        out.truncate(limit);
        Ok(out)
    }
}

/// Parse a STAC datetime expression into `(start, end)` strings.
///
/// Accepted forms:
/// - `"2024-06-01T00:00:00Z"` → exact (start == end)
/// - `"2024-06-01/2024-06-30"` → closed interval
/// - `"2024-06-01/.."` → open-ended (no upper bound)
/// - `"../2024-06-30"` → open-ended (no lower bound)
fn parse_datetime(s: &str) -> std::result::Result<(Option<String>, Option<String>), InMemoryError> {
    if s.is_empty() {
        return Err(InMemoryError::BadDatetime(s.into()));
    }
    if let Some((lhs, rhs)) = s.split_once('/') {
        let start = if lhs == ".." { None } else { Some(lhs.to_string()) };
        let end = if rhs == ".." { None } else { Some(rhs.to_string()) };
        if start.is_none() && end.is_none() {
            return Err(InMemoryError::BadDatetime(s.into()));
        }
        Ok((start, end))
    } else {
        Ok((Some(s.to_string()), Some(s.to_string())))
    }
}

/// Lexicographic RFC3339 comparison is correct because all UTC RFC3339
/// strings of the same width are ordered identically to their wall-clock
/// instants.
fn datetime_matches(start: Option<&str>, end: Option<&str>, item: &str) -> bool {
    if let Some(s) = start {
        if item < s { return false; }
    }
    if let Some(e) = end {
        if item > e { return false; }
    }
    true
}

/// Axis-aligned bbox intersection in EPSG:4326. Bboxes that touch at a
/// single coordinate are *not* considered intersecting (strict).
fn bbox_intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    let [a_w, a_s, a_e, a_n] = a;
    let [b_w, b_s, b_e, b_n] = b;
    !(a_e < b_w || b_e < a_w || a_n < b_s || b_n < a_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s2_item(id: &str, dt: &str, bbox: [f64; 4]) -> InMemoryItem {
        InMemoryItem {
            id: id.into(),
            collection: "sentinel-2-l2a".into(),
            bbox,
            datetime: dt.into(),
        }
    }

    fn ls_item(id: &str, dt: &str, bbox: [f64; 4]) -> InMemoryItem {
        InMemoryItem {
            id: id.into(),
            collection: "landsat-c2-l2".into(),
            bbox,
            datetime: dt.into(),
        }
    }

    fn fixture() -> InMemoryStacClient {
        InMemoryStacClient::with_items(vec![
            s2_item("S2_A", "2024-06-01T10:00:00Z", [10.0, 50.0, 11.0, 51.0]),
            s2_item("S2_B", "2024-06-15T10:00:00Z", [11.5, 50.0, 12.5, 51.0]),
            ls_item("LS_A", "2024-06-10T10:00:00Z", [10.0, 50.0, 11.0, 51.0]),
            ls_item("LS_B", "2024-08-01T10:00:00Z", [10.0, 50.0, 11.0, 51.0]),
        ])
    }

    // ── builder ────────────────────────────────────────────────────

    #[test]
    fn builder_chains() {
        let r = SearchRequest::default()
            .with_collection("sentinel-2-l2a")
            .with_bbox([0.0, 0.0, 10.0, 10.0])
            .with_datetime("2024-06-01/2024-06-30")
            .with_limit(5);
        assert_eq!(r.collections, vec!["sentinel-2-l2a"]);
        assert_eq!(r.bbox, Some([0.0, 0.0, 10.0, 10.0]));
        assert_eq!(r.datetime.as_deref(), Some("2024-06-01/2024-06-30"));
        assert_eq!(r.limit, Some(5));
    }

    // ── parse_datetime ────────────────────────────────────────────

    #[test]
    fn parse_datetime_exact() {
        let (s, e) = parse_datetime("2024-06-01T00:00:00Z").unwrap();
        assert_eq!(s.as_deref(), Some("2024-06-01T00:00:00Z"));
        assert_eq!(e.as_deref(), Some("2024-06-01T00:00:00Z"));
    }

    #[test]
    fn parse_datetime_closed_interval() {
        let (s, e) = parse_datetime("2024-06-01/2024-06-30").unwrap();
        assert_eq!(s.as_deref(), Some("2024-06-01"));
        assert_eq!(e.as_deref(), Some("2024-06-30"));
    }

    #[test]
    fn parse_datetime_open_upper() {
        let (s, e) = parse_datetime("2024-06-01/..").unwrap();
        assert_eq!(s.as_deref(), Some("2024-06-01"));
        assert!(e.is_none());
    }

    #[test]
    fn parse_datetime_open_lower() {
        let (s, e) = parse_datetime("../2024-06-30").unwrap();
        assert!(s.is_none());
        assert_eq!(e.as_deref(), Some("2024-06-30"));
    }

    #[test]
    fn parse_datetime_double_open_is_error() {
        assert!(parse_datetime("../..").is_err());
    }

    #[test]
    fn parse_datetime_empty_is_error() {
        assert!(parse_datetime("").is_err());
    }

    // ── bbox_intersects ───────────────────────────────────────────

    #[test]
    fn bbox_intersects_overlapping() {
        assert!(bbox_intersects([0.0, 0.0, 5.0, 5.0], [3.0, 3.0, 7.0, 7.0]));
    }

    #[test]
    fn bbox_intersects_contained() {
        assert!(bbox_intersects([0.0, 0.0, 10.0, 10.0], [3.0, 3.0, 4.0, 4.0]));
    }

    #[test]
    fn bbox_intersects_disjoint() {
        assert!(!bbox_intersects([0.0, 0.0, 1.0, 1.0], [5.0, 5.0, 6.0, 6.0]));
    }

    // ── in-memory search ──────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn search_empty_filter_returns_all() {
        let c = fixture();
        let r = c.search(&SearchRequest::default()).await.unwrap();
        assert_eq!(r.len(), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_by_collection_narrows() {
        let c = fixture();
        let req = SearchRequest::default().with_collection("sentinel-2-l2a");
        let r = c.search(&req).await.unwrap();
        assert_eq!(r, vec!["S2_A", "S2_B"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_by_multiple_collections() {
        let c = fixture();
        let req = SearchRequest::default()
            .with_collection("sentinel-2-l2a")
            .with_collection("landsat-c2-l2");
        let r = c.search(&req).await.unwrap();
        assert_eq!(r.len(), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_by_bbox_excludes_disjoint() {
        let c = fixture();
        // bbox over S2_A region only.
        let req = SearchRequest::default().with_bbox([10.0, 50.0, 11.0, 51.0]);
        let r = c.search(&req).await.unwrap();
        // S2_A and Landsat items (same bbox); S2_B is offset.
        assert!(r.contains(&"S2_A".to_string()));
        assert!(!r.contains(&"S2_B".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_by_datetime_interval() {
        let c = fixture();
        let req = SearchRequest::default()
            .with_datetime("2024-06-01T00:00:00Z/2024-06-30T23:59:59Z");
        let r = c.search(&req).await.unwrap();
        // All June items: S2_A, S2_B, LS_A.
        assert_eq!(r.len(), 3);
        assert!(!r.contains(&"LS_B".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_by_open_ended_datetime() {
        let c = fixture();
        let req = SearchRequest::default().with_datetime("2024-07-01/..");
        let r = c.search(&req).await.unwrap();
        assert_eq!(r, vec!["LS_B"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_ids_short_circuit_other_predicates() {
        let c = fixture();
        let req = SearchRequest {
            ids: vec!["S2_A".into()],
            // bbox that excludes S2_A — must be ignored once `ids` is set.
            bbox: Some([100.0, 100.0, 110.0, 110.0]),
            ..Default::default()
        };
        let r = c.search(&req).await.unwrap();
        assert_eq!(r, vec!["S2_A"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_respects_limit() {
        let c = fixture();
        let req = SearchRequest::default().with_limit(2);
        let r = c.search(&req).await.unwrap();
        assert_eq!(r.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_combines_collection_bbox_and_datetime() {
        let c = fixture();
        let req = SearchRequest::default()
            .with_collection("sentinel-2-l2a")
            .with_bbox([10.0, 50.0, 11.0, 51.0])
            .with_datetime("2024-06-01/2024-06-30");
        let r = c.search(&req).await.unwrap();
        assert_eq!(r, vec!["S2_A"], "exactly S2_A satisfies all three");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_invalid_datetime_errors() {
        let c = fixture();
        let req = SearchRequest::default().with_datetime("../..");
        let r = c.search(&req).await;
        assert!(matches!(r, Err(InMemoryError::BadDatetime(_))));
    }

    #[test]
    fn empty_catalog_has_zero_len() {
        let c = InMemoryStacClient::default();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }
}
