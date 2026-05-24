//! STAC searcher (trait + HTTP impl) and supporting types.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::executor::ExecError;

/// One STAC item returned by the search call. Band-flexible: the
/// `bands` map carries every asset href the searcher could resolve
/// for the requested band list (keyed by the canonical band name as
/// it appears in the openEO `load_collection.bands` argument).
///
/// Examples: `{"B04": "...", "B08": "...", "SCL": "..."}` for the
/// Sentinel-2 backbone, or `{"B11": "...", "B12": "..."}` for SWIR.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StacScene {
    /// STAC item id.
    pub id: String,
    /// Per-band asset hrefs. Key = band name as it appears in the
    /// STAC asset map (e.g. "B04", "B08", "SCL"). Empty when the
    /// feature exposed no requested band.
    pub bands: BTreeMap<String, String>,
}

/// Owned collection of STAC scenes returned by a search. Implements
/// [`orbit_geo::BandPathResolver`] so callers can hand it to
/// [`orbit_geo::RasterDatasetBuilder::from_band_resolver`] without
/// teaching `orbit-geo` about STAC types.
///
/// `band_paths` interprets each scene's href as a local filesystem path
/// (`PathBuf::from`). For remote hrefs (HTTPS / s3:// COGs) the caller
/// is responsible for downloading + caching first and inserting the
/// local path back into the scene's `bands` map before resolving.
#[derive(Clone, Debug, Default)]
pub struct FeatureCollection {
    /// Scenes in search order. Order matters: it becomes the time axis
    /// of the resulting raster dataset.
    pub scenes: Vec<StacScene>,
}

impl FeatureCollection {
    /// Build from an owned `Vec<StacScene>`.
    #[must_use]
    pub fn new(scenes: Vec<StacScene>) -> Self {
        Self { scenes }
    }
}

impl From<Vec<StacScene>> for FeatureCollection {
    fn from(scenes: Vec<StacScene>) -> Self {
        Self { scenes }
    }
}

impl orbit_geo::BandPathResolver for FeatureCollection {
    /// Walk `scenes` in order, emitting the local path for every scene
    /// that exposes `band_name`. Scenes missing the band are silently
    /// skipped — mirrors `eval_load_collection`'s "all-or-none" filter
    /// done one layer up.
    fn band_paths(&self, band_name: &str) -> Vec<std::path::PathBuf> {
        self.scenes
            .iter()
            .filter_map(|s| s.bands.get(band_name).map(std::path::PathBuf::from))
            .collect()
    }
}

/// STAC search backend. Default impl POSTs to `{base}/search`.
///
/// Band selection lives at the executor layer (`load_collection.bands`)
/// — the searcher MUST return whatever assets each STAC feature exposes;
/// the executor filters down to the requested subset. Callers that want
/// to be polite to the STAC API can pass the band list along (`fields`
/// extension), but this is opportunistic and not required by the trait.
#[async_trait]
pub trait StacSearcher: Send + Sync {
    /// Search a collection for scenes intersecting the given filters.
    ///
    /// `max_cloud_cover` (percent, 0..=100) translates into a STAC
    /// `query.eo:cloud_cover.lt` predicate when `Some`. Honors the
    /// openEO `properties.eo:cloud_cover` filter conventional binding.
    async fn search(
        &self,
        collection_id: &str,
        bbox: [f64; 4],
        datetime: Option<&str>,
        limit: u32,
        max_cloud_cover: Option<f64>,
    ) -> Result<Vec<StacScene>, ExecError>;
}

/// Default `StacSearcher` that talks to a real STAC API.
///
/// Transient failures (5xx / network glitches) are retried using
/// `orbit_resilience::RetryPolicy` (exponential back-off, jittered).
pub struct HttpStacSearcher {
    /// Base URL (e.g. `https://earth-search.aws.element84.com/v1`).
    pub base_url: String,
    /// Reqwest client (shared, supports HTTP keep-alive + redirects).
    pub client: reqwest::Client,
    /// Retry policy used on transient failures.
    pub retry: orbit_resilience::RetryPolicy,
    /// **P1-7**: per-searcher SSRF policy. Default strict.
    pub url_policy: crate::url_policy::UrlPolicy,
}

impl HttpStacSearcher {
    /// New searcher pointing at `base_url`, default `RetryPolicy` (4 attempts).
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .user_agent(concat!("orbit-openeo/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            retry: orbit_resilience::RetryPolicy::default(),
            url_policy: crate::url_policy::UrlPolicy::default(),
        }
    }

    /// New searcher with a custom retry policy (e.g. for tests).
    #[must_use]
    pub fn with_retry(mut self, retry: orbit_resilience::RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// **P1-7**: override the SSRF policy. Tests against wiremock use
    /// `UrlPolicy::relaxed_dev()` so loopback http:// URLs are allowed.
    #[must_use]
    pub fn with_url_policy(mut self, policy: crate::url_policy::UrlPolicy) -> Self {
        self.url_policy = policy;
        self
    }
}

#[async_trait]
impl StacSearcher for HttpStacSearcher {
    async fn search(
        &self,
        collection_id: &str,
        bbox: [f64; 4],
        datetime: Option<&str>,
        limit: u32,
        max_cloud_cover: Option<f64>,
    ) -> Result<Vec<StacScene>, ExecError> {
        let mut body = serde_json::json!({
            "collections": [collection_id],
            "bbox": bbox,
            "limit": limit,
        });
        if let Some(dt) = datetime {
            body["datetime"] = Value::String(dt.into());
        }
        // STAC API ext: filter on `eo:cloud_cover` via `query.lt`.
        // Element84 + PC + DEA all honor this shape.
        if let Some(max_cc) = max_cloud_cover {
            body["query"] = serde_json::json!({
                "eo:cloud_cover": { "lt": max_cc }
            });
        }
        let url = format!("{}/search", self.base_url);
        // **P1-7**: SSRF policy check.
        if let Err(e) = self.url_policy.check(&url) {
            return Err(ExecError::Backend(format!("url_policy: {e}")));
        }
        // Retry loop: 5xx / network errors → wait jittered back-off → retry.
        // 4xx (client errors) are returned immediately — they will not
        // succeed on retry.
        let mut attempt: u32 = 0;
        let payload: Value = loop {
            let send_result = self.client.post(&url).json(&body).send().await;
            let bail_with = |msg: String, transient: bool| (msg, transient);
            let (err_msg, transient) = match send_result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.json::<Value>().await {
                            Ok(v) => break v,
                            Err(e) => bail_with(format!("STAC search decode: {e}"), true),
                        }
                    } else if status.is_server_error() {
                        bail_with(format!("STAC search {url}: HTTP {status}"), true)
                    } else {
                        // 4xx → no retry.
                        return Err(ExecError::Backend(format!(
                            "STAC search {url}: HTTP {status}"
                        )));
                    }
                }
                Err(e) => bail_with(format!("STAC search {url}: {e}"), true),
            };
            attempt += 1;
            if !transient || !self.retry.should_retry(attempt) {
                return Err(ExecError::Backend(err_msg));
            }
            // Sleep jittered back-off before next attempt.
            let dly = self
                .retry
                .jittered_delay(attempt, || super::fastrand_unit_f64());
            tracing::warn!(attempt, url = %url, "STAC search retry after {dly:?}");
            tokio::time::sleep(dly).await;
        };
        let features = payload
            .get("features")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();
        let mut scenes = Vec::with_capacity(features.len());
        for f in features {
            let id = f
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("<no-id>")
                .to_string();
            let assets = f.get("assets").cloned().unwrap_or(Value::Null);
            // Collect EVERY asset href from the feature. The executor
            // layer then filters down to the requested `bands` subset.
            // Aliases (`red`→`B04`, `nir`→`B08`) are normalised to the
            // canonical S2 band name so executors can ask for "B04" and
            // hit either keying.
            let bands = collect_band_hrefs(&assets);
            if bands.is_empty() {
                tracing::warn!(item = %id, "skipping scene with no asset hrefs");
                continue;
            }
            scenes.push(StacScene { id, bands });
        }
        Ok(scenes)
    }
}

/// Map every STAC asset key to its href, normalising the common
/// `red`/`nir`/`scl` aliases to the canonical S2 band names so the
/// rest of the pipeline can address them uniformly.
fn collect_band_hrefs(assets: &Value) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let map = match assets.as_object() {
        Some(m) => m,
        None => return out,
    };
    for (k, v) in map.iter() {
        let href = match v.get("href").and_then(|h| h.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let canonical = canonical_band_name(k);
        out.entry(canonical).or_insert(href);
    }
    out
}

/// Normalise legacy STAC asset aliases (`red`→`B04`, `nir`→`B08`,
/// `scl`→`SCL`) to canonical Sentinel-2 band names. Unknown keys pass
/// through unchanged so the searcher stays generic across collections.
pub(super) fn canonical_band_name(k: &str) -> String {
    match k {
        "red" | "Red" => "B04".to_string(),
        "nir" | "Nir" | "NIR" => "B08".to_string(),
        "scl" => "SCL".to_string(),
        "blue" => "B02".to_string(),
        "green" => "B03".to_string(),
        "swir16" | "SWIR16" => "B11".to_string(),
        "swir22" | "SWIR22" => "B12".to_string(),
        // Lower-case Sentinel-2 band codes → upper.
        "b02" | "b03" | "b04" | "b05" | "b06" | "b07" | "b08"
        | "b8a" | "b09" | "b11" | "b12" => k.to_uppercase(),
        other => other.to_string(),
    }
}

pub(super) fn pick_asset_href(assets: &Value, candidates: &[&str]) -> Option<String> {
    for k in candidates {
        if let Some(s) = assets.get(*k).and_then(|a| a.get("href")).and_then(|h| h.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_emits_eo_cloud_cover_query_when_set() {
        // Verify the wire-level STAC POST body includes the `query.eo:cloud_cover.lt`
        // predicate when max_cloud_cover is Some.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/search"))
            .and(body_partial_json(json!({
                "query": { "eo:cloud_cover": { "lt": 30.0 } }
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "type": "FeatureCollection",
                "features": []
            })))
            .expect(1)
            .mount(&server)
            .await;
        let searcher = HttpStacSearcher::new(server.uri())
            .with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev());
        let scenes = searcher
            .search("sentinel-2-l2a", [0.0, 0.0, 1.0, 1.0], None, 5, Some(30.0))
            .await
            .unwrap();
        assert!(scenes.is_empty());
        // Mock server's .expect(1) panics on Drop if the predicate didn't match.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_omits_query_when_max_cloud_cover_is_none() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/search"))
            // Must NOT contain a `query` object.
            .and(body_partial_json(json!({ "collections": ["c"] })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "type": "FeatureCollection", "features": []
            })))
            .mount(&server)
            .await;
        let searcher = HttpStacSearcher::new(server.uri())
            .with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev());
        let _ = searcher
            .search("c", [0.0, 0.0, 1.0, 1.0], None, 1, None)
            .await
            .unwrap();
    }

    #[test]
    fn pick_asset_href_finds_red_under_alias() {
        let assets = json!({
            "B04": { "href": "https://example.com/red.tif" },
            "B08": { "href": "https://example.com/nir.tif" },
        });
        assert_eq!(pick_asset_href(&assets, &["red", "B04"]),
                   Some("https://example.com/red.tif".to_string()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_parses_features() {
        let server = MockServer::start().await;
        let payload = json!({
            "type": "FeatureCollection",
            "features": [{
                "id": "S2A_AAA",
                "assets": {
                    "red": { "href": "https://example.com/red0.tif" },
                    "nir": { "href": "https://example.com/nir0.tif" }
                }
            },{
                "id": "S2B_BBB",
                "assets": {
                    "B04": { "href": "https://example.com/red1.tif" },
                    "B08": { "href": "https://example.com/nir1.tif" }
                }
            }]
        });
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server)
            .await;
        let searcher = HttpStacSearcher::new(server.uri()).with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev());
        let scenes = searcher
            .search("sentinel-2-l2a", [144.5, -36.6, 144.7, -36.4], None, 2, None)
            .await
            .unwrap();
        assert_eq!(scenes.len(), 2);
        // `red`/`nir` aliases normalise to canonical S2 band names.
        assert_eq!(scenes[0].bands.get("B04").map(String::as_str),
            Some("https://example.com/red0.tif"));
        assert_eq!(scenes[1].bands.get("B08").map(String::as_str),
            Some("https://example.com/nir1.tif"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_500_maps_to_backend_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        // Tight retry policy so the test finishes quickly even with retries.
        let policy = orbit_resilience::RetryPolicy {
            max_attempts: 2,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
        };
        let r = HttpStacSearcher::new(server.uri()).with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev())
            .with_retry(policy)
            .search("c", [0.0, 0.0, 1.0, 1.0], None, 1, None).await;
        assert!(matches!(r, Err(ExecError::Backend(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_4xx_does_not_retry() {
        // 400 is a client error — must bubble up immediately (no retries).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)  // exactly one request, no retries
            .mount(&server)
            .await;
        let policy = orbit_resilience::RetryPolicy {
            max_attempts: 5,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
        };
        let r = HttpStacSearcher::new(server.uri()).with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev())
            .with_retry(policy)
            .search("c", [0.0, 0.0, 1.0, 1.0], None, 1, None).await;
        assert!(matches!(r, Err(ExecError::Backend(_))));
        // Wiremock will panic on drop if `.expect(1)` was violated.
    }

    #[test]
    fn fastrand_unit_f64_is_in_range() {
        for _ in 0..100 {
            let v = super::super::fastrand_unit_f64();
            assert!((0.0..1.0).contains(&v), "out of range: {v}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_skips_items_missing_assets() {
        // Post-band-flex: items with ZERO usable hrefs are dropped, but
        // single-band items (e.g. just red) are kept — band selection
        // happens at the executor layer.
        let server = MockServer::start().await;
        let payload = json!({
            "features": [
                {"id": "drop-me", "assets": {}}, // no hrefs at all
                {"id": "keep-me", "assets": {
                    "red": {"href": "https://example.com/r.tif"},
                    "nir": {"href": "https://example.com/n.tif"}
                }}
            ]
        });
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server).await;
        let scenes = HttpStacSearcher::new(server.uri()).with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev())
            .search("c", [0.0, 0.0, 1.0, 1.0], None, 10, None).await.unwrap();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].id, "keep-me");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stac_searcher_extracts_scl_when_present() {
        let server = MockServer::start().await;
        let payload = json!({
            "features": [{
                "id": "S2_with_scl",
                "assets": {
                    "red": { "href": "https://example.com/r.tif" },
                    "nir": { "href": "https://example.com/n.tif" },
                    "scl": { "href": "https://example.com/s.tif" }
                }
            }]
        });
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .mount(&server)
            .await;
        let scenes = HttpStacSearcher::new(server.uri()).with_url_policy(crate::url_policy::UrlPolicy::relaxed_dev())
            .search("c", [0.0, 0.0, 1.0, 1.0], None, 1, None).await.unwrap();
        assert_eq!(scenes[0].bands.get("SCL").map(String::as_str),
            Some("https://example.com/s.tif"));
    }

    #[test]
    fn pick_asset_href_finds_scl_under_alias() {
        let a1 = json!({ "scl": { "href": "x.tif" } });
        assert_eq!(pick_asset_href(&a1, &["scl", "SCL"]), Some("x.tif".to_string()));
        let a2 = json!({ "SCL": { "href": "y.tif" } });
        assert_eq!(pick_asset_href(&a2, &["scl", "SCL"]), Some("y.tif".to_string()));
    }

    // ---------- A20 — FeatureCollection / BandPathResolver bridge ----------

    #[test]
    fn feature_collection_band_paths_collects_in_scene_order() {
        use orbit_geo::BandPathResolver;
        let mut s0 = StacScene { id: "s0".into(), bands: BTreeMap::new() };
        s0.bands.insert("B04".into(), "/tmp/s0_red.tif".into());
        s0.bands.insert("B08".into(), "/tmp/s0_nir.tif".into());
        let mut s1 = StacScene { id: "s1".into(), bands: BTreeMap::new() };
        s1.bands.insert("B04".into(), "/tmp/s1_red.tif".into());
        s1.bands.insert("B08".into(), "/tmp/s1_nir.tif".into());
        let fc: FeatureCollection = vec![s0, s1].into();
        let red_paths = fc.band_paths("B04");
        assert_eq!(red_paths, vec![
            std::path::PathBuf::from("/tmp/s0_red.tif"),
            std::path::PathBuf::from("/tmp/s1_red.tif"),
        ]);
    }

    #[test]
    fn feature_collection_band_paths_skips_scenes_missing_band() {
        use orbit_geo::BandPathResolver;
        let mut s0 = StacScene { id: "s0".into(), bands: BTreeMap::new() };
        s0.bands.insert("B04".into(), "/tmp/s0_red.tif".into());
        // s1 has no B04 — should be silently dropped.
        let s1 = StacScene { id: "s1".into(), bands: BTreeMap::new() };
        let mut s2 = StacScene { id: "s2".into(), bands: BTreeMap::new() };
        s2.bands.insert("B04".into(), "/tmp/s2_red.tif".into());
        let fc = FeatureCollection::new(vec![s0, s1, s2]);
        assert_eq!(fc.band_paths("B04"), vec![
            std::path::PathBuf::from("/tmp/s0_red.tif"),
            std::path::PathBuf::from("/tmp/s2_red.tif"),
        ]);
        // Unknown band yields empty.
        assert!(fc.band_paths("SCL").is_empty());
    }
}
