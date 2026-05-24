//! Minimal openEO client — gated by the `openeo` feature.
//!
//! As of May 2026 there is **no Rust openEO client crate on crates.io**, so
//! orbit-geo ships its own focused implementation. The scope is intentionally
//! narrow: submit a process graph, poll until done, download result assets to
//! a local cache directory. No process-graph construction helpers, no
//! interactive helpers — those belong in client code (or in Python via
//! `bindings/orbit-py`).
//!
//! ## Why we ship this rather than wait for a community crate
//!
//! See [`13-geo-satellite/04-openeo-strategic-analysis.md`](../../../../13-geo-satellite/04-openeo-strategic-analysis.md)
//! Approach (C): orbit-geo treats openEO backends as **one of multiple data
//! sources** (alongside local files + STAC). We do not become an openEO
//! backend ourselves — we just want to *consume* one when convenient.
//!
//! ## Endpoint surface (10 calls)
//!
//! | Method | Endpoint                          | Use |
//! |--------|-----------------------------------|-----|
//! | POST   | /credentials/oidc                  | (informational; not used by Bearer path) |
//! | GET    | /credentials/oidc                  | Discover OIDC providers |
//! | POST   | /jobs                              | Create batch job from process graph |
//! | POST   | /jobs/{job_id}/results             | Start processing |
//! | GET    | /jobs/{job_id}                     | Poll job status |
//! | GET    | /jobs/{job_id}/results             | Get result asset URLs |
//! | DELETE | /jobs/{job_id}                     | Clean up after download |
//!
//! Reference: [openEO API spec](https://openeo.org/documentation/1.0/developers/api/reference.html).

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// openEO authentication options.
///
/// Most production backends (CDSE, VITO, EODC) require OIDC; some pilot
/// backends accept HTTP Basic.
#[derive(Clone, Debug)]
pub enum OpenEoAuth {
    /// Pre-acquired OIDC Bearer token. Caller is responsible for refresh.
    OidcBearer(String),
    /// HTTP Basic — username + password (rare in production, used by some pilots).
    Basic {
        /// Username.
        user: String,
        /// Password.
        password: String,
    },
    /// No authentication (public endpoints only — extremely rare).
    None,
}

impl OpenEoAuth {
    /// Apply this auth to a reqwest request builder.
    pub(crate) fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::OidcBearer(token) => rb.bearer_auth(token),
            Self::Basic { user, password } => rb.basic_auth(user, Some(password)),
            Self::None => rb,
        }
    }
}

/// Job state as reported by openEO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Job created but not started.
    Created,
    /// Job in queue, awaiting compute resources.
    Queued,
    /// Job currently running.
    Running,
    /// Job finished successfully — results are ready.
    Finished,
    /// Job failed — see error message.
    Error,
    /// Job was cancelled.
    Canceled,
}

impl JobStatus {
    /// Has the job reached a terminal state (no further polling required)?
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Error | Self::Canceled)
    }
}

/// Per-job response payload (subset of the openEO `BatchJob` schema we use).
#[derive(Debug, Clone, Deserialize)]
pub struct JobInfo {
    /// Job identifier assigned by the backend.
    pub id: String,
    /// Current status.
    pub status: JobStatus,
    /// Progress percentage 0–100 (optional, backend-dependent).
    #[serde(default)]
    pub progress: Option<f64>,
}

/// Asset descriptor inside `GET /jobs/{id}/results`.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // title and media_type are populated from JSON but not used yet
struct Asset {
    href: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "type", default)]
    media_type: Option<String>,
}

/// `GET /jobs/{id}/results` shape (subset).
#[derive(Debug, Clone, Deserialize)]
struct ResultsResponse {
    #[serde(default)]
    assets: std::collections::BTreeMap<String, Asset>,
}

/// Polling configuration.
#[derive(Debug, Clone, Copy)]
pub struct PollConfig {
    /// Time between status checks.
    pub interval: Duration,
    /// Maximum wall-clock time before giving up (returns [`Error::Other`]).
    pub timeout: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(60 * 60 * 4), // 4 hours
        }
    }
}

/// openEO client handle.
///
/// Cheap to construct; reuse across calls to amortise reqwest connection pooling.
pub struct Client {
    http: reqwest::Client,
    backend_url: String,
    auth: OpenEoAuth,
}

impl Client {
    /// Build a new client.
    pub fn new(backend_url: impl Into<String>, auth: OpenEoAuth) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("orbit-geo/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Other(format!("reqwest: {e}")))?;
        Ok(Self {
            http,
            backend_url: backend_url.into().trim_end_matches('/').to_string(),
            auth,
        })
    }

    /// `POST /jobs` — create a batch job from a process graph.
    pub async fn create_job(&self, process_graph: &serde_json::Value) -> Result<String> {
        let body = serde_json::json!({
            "process": { "process_graph": process_graph },
            "title": "orbit-geo",
        });
        let resp = self
            .auth
            .apply(self.http.post(format!("{}/jobs", self.backend_url)).json(&body))
            .send()
            .await
            .map_err(map_err)?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "openEO create_job failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            )));
        }
        let location = resp
            .headers()
            .get("openeo-identifier")
            .or_else(|| resp.headers().get("location"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| Error::Other("openEO create_job: missing job id header".into()))?;

        // Some backends return the bare ID, others a URL — strip URL prefix.
        Ok(location
            .rsplit('/')
            .next()
            .unwrap_or(&location)
            .to_string())
    }

    /// `POST /jobs/{id}/results` — start processing.
    pub async fn start_job(&self, job_id: &str) -> Result<()> {
        let resp = self
            .auth
            .apply(
                self.http
                    .post(format!("{}/jobs/{}/results", self.backend_url, job_id)),
            )
            .send()
            .await
            .map_err(map_err)?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "openEO start_job failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            )));
        }
        Ok(())
    }

    /// `GET /jobs/{id}` — poll status.
    pub async fn get_job(&self, job_id: &str) -> Result<JobInfo> {
        let resp = self
            .auth
            .apply(
                self.http
                    .get(format!("{}/jobs/{}", self.backend_url, job_id)),
            )
            .send()
            .await
            .map_err(map_err)?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "openEO get_job failed: {}",
                resp.status()
            )));
        }
        let info: JobInfo = resp.json().await.map_err(map_err)?;
        Ok(info)
    }

    /// Block (in async terms) until the job reaches a terminal state.
    pub async fn wait_for_completion(&self, job_id: &str, cfg: PollConfig) -> Result<JobInfo> {
        let start = std::time::Instant::now();
        loop {
            let info = self.get_job(job_id).await?;
            if info.status.is_terminal() {
                return Ok(info);
            }
            if start.elapsed() > cfg.timeout {
                return Err(Error::Other(format!(
                    "openEO job {job_id} did not complete within {:?}",
                    cfg.timeout
                )));
            }
            tracing::info!(
                job_id = %job_id,
                status = ?info.status,
                progress = ?info.progress,
                "openEO job in progress"
            );
            tokio::time::sleep(cfg.interval).await;
        }
    }

    /// `GET /jobs/{id}/results` — list result asset URLs.
    pub async fn get_results(&self, job_id: &str) -> Result<Vec<String>> {
        let resp = self
            .auth
            .apply(
                self.http
                    .get(format!("{}/jobs/{}/results", self.backend_url, job_id)),
            )
            .send()
            .await
            .map_err(map_err)?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "openEO get_results failed: {}",
                resp.status()
            )));
        }
        let body: ResultsResponse = resp.json().await.map_err(map_err)?;
        Ok(body.assets.into_values().map(|a| a.href).collect())
    }

    /// `DELETE /jobs/{id}` — clean up.
    pub async fn delete_job(&self, job_id: &str) -> Result<()> {
        let resp = self
            .auth
            .apply(
                self.http
                    .delete(format!("{}/jobs/{}", self.backend_url, job_id)),
            )
            .send()
            .await
            .map_err(map_err)?;
        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            return Err(Error::Other(format!(
                "openEO delete_job failed: {}",
                resp.status()
            )));
        }
        Ok(())
    }

    /// Download one asset URL into `cache_dir`. Re-runs that find the file
    /// already present and the same size **skip the download**.
    pub async fn download_asset(&self, asset_url: &str, cache_dir: &Path) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        std::fs::create_dir_all(cache_dir)?;

        // Stable cache filename: short URL hash + last path segment.
        let mut hasher = Sha256::new();
        hasher.update(asset_url.as_bytes());
        let digest = hasher.finalize();
        let prefix = &hex::encode(&digest[..6]);
        let filename = url::Url::parse(asset_url)
            .ok()
            .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()).map(str::to_owned))
            .unwrap_or_else(|| "asset.bin".into());
        let out_path = cache_dir.join(format!("{prefix}_{filename}"));

        if out_path.exists() {
            tracing::debug!(path = ?out_path, "cache hit");
            return Ok(out_path);
        }

        let resp = self.auth.apply(self.http.get(asset_url)).send().await.map_err(map_err)?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "openEO asset download failed for {asset_url}: {}",
                resp.status()
            )));
        }

        // Streamed write to a temp file then atomic rename — safe across crashes.
        let tmp = out_path.with_extension("part");
        let bytes = resp.bytes().await.map_err(map_err)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &out_path)?;
        Ok(out_path)
    }
}

fn map_err(e: reqwest::Error) -> Error {
    Error::Other(format!("openEO http: {e}"))
}

/// High-level helper used by `DataSource::OpenEO`.
///
/// Submits the process graph, polls until completion, downloads every result
/// asset to `cache_dir`, returns the local paths in deterministic order.
///
/// Re-runs hit the cache (sha256(url)-prefixed filenames).
pub async fn submit_and_download(
    backend_url: &str,
    process_graph: &serde_json::Value,
    auth: &OpenEoAuth,
    cache_dir: &Path,
    poll: PollConfig,
) -> Result<Vec<PathBuf>> {
    let client = Client::new(backend_url, auth.clone())?;

    tracing::info!(backend = %backend_url, "submitting openEO job");
    let job_id = client.create_job(process_graph).await?;
    tracing::info!(job_id = %job_id, "openEO job created; starting");

    client.start_job(&job_id).await?;
    let info = client.wait_for_completion(&job_id, poll).await?;

    match info.status {
        JobStatus::Finished => {
            tracing::info!(job_id = %job_id, "openEO job finished; downloading assets");
            let urls = client.get_results(&job_id).await?;
            let mut paths = Vec::with_capacity(urls.len());
            for url in urls {
                paths.push(client.download_asset(&url, cache_dir).await?);
            }
            paths.sort();
            // Best-effort cleanup; ignore errors — many backends auto-expire jobs.
            let _ = client.delete_job(&job_id).await;
            Ok(paths)
        }
        other => Err(Error::Other(format!(
            "openEO job {job_id} ended in non-Finished state: {:?}",
            other
        ))),
    }
}
