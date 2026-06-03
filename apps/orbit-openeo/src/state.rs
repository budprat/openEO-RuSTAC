//! Shared application state.

use std::sync::Arc;

use crate::auth::AuthPolicy;
use crate::catalog::{CollectionCatalog, InMemoryCatalog};
use crate::event_bus::{EventBus, InMemoryEventBus};
use crate::executor::{LocalExecutor, ProcessGraphExecutor};
use crate::file_store::{FileStore, InMemoryFileStore};
use crate::job_store::{InMemoryJobStore, JobStore};
use crate::schema::SchemaRegistry;
use crate::routes::credentials::{DeviceCodeStore, OidcProvider, TokenStore};
use crate::security::RouteSecurityMap;

/// State passed to every Axum handler.
///
/// All fields are `Arc`-wrapped so cloning is cheap (Axum clones the state
/// per request).
#[derive(Clone)]
pub struct AppState {
    /// JSON-Schema registry loaded from openapi.json at startup.
    pub schemas: Arc<SchemaRegistry>,
    /// Active auth policy (None = open; Some = enforce).
    pub auth: Arc<AuthPolicy>,
    /// openEO `version` string returned by `GET /`.
    pub api_version: Arc<str>,
    /// Backend identifier returned in capabilities.
    pub backend_id: Arc<str>,
    /// Process-graph execution backend.
    pub executor: Arc<dyn ProcessGraphExecutor>,
    /// File storage backend.
    pub files: Arc<dyn FileStore>,
    /// Event bus driving the `/subscription` WebSocket.
    pub events: Arc<dyn EventBus>,
    /// Per-route required-scheme map (consulted by the auth layer).
    pub security: Arc<RouteSecurityMap>,
    /// In-memory bearer-token store for Basic→Bearer issuance.
    pub tokens: Arc<TokenStore>,
    /// OIDC device-code session store (RFC 8628).
    pub device_codes: Arc<DeviceCodeStore>,
    /// OIDC providers advertised by `GET /credentials/oidc`.
    pub oidc_providers: Vec<OidcProvider>,
    /// Collection catalog (`/collections` + `/collections/{id}`).
    pub catalog: Arc<dyn CollectionCatalog>,
    /// Job persistence for `/jobs` routes.
    pub jobs: Arc<dyn JobStore>,
    /// Observability recorder (counters / histograms). Defaults to an
    /// in-memory recorder; production deployments swap in a Prometheus
    /// exporter via the `Recorder` trait.
    pub metrics: Arc<dyn orbit_observability::Recorder>,
    /// **P0-5 / P1-9**: bounded concurrency for `POST /jobs/{id}/results`
    /// spawns. Default 16; configure via `with_job_concurrency`.
    pub job_sem: Arc<tokio::sync::Semaphore>,
    /// In-flight job task registry (cooperative cancellation + graceful
    /// drain on shutdown). audit-fix 2026-06-03.
    pub job_registry: Arc<crate::job_registry::JobRegistry>,
}

/// Builder for [`AppState`].
#[derive(Default)]
pub struct AppStateBuilder {
    schemas: Option<Arc<SchemaRegistry>>,
    auth: Option<Arc<AuthPolicy>>,
    api_version: Option<Arc<str>>,
    backend_id: Option<Arc<str>>,
    executor: Option<Arc<dyn ProcessGraphExecutor>>,
    files: Option<Arc<dyn FileStore>>,
    events: Option<Arc<dyn EventBus>>,
    security: Option<Arc<RouteSecurityMap>>,
    tokens: Option<Arc<TokenStore>>,
    device_codes: Option<Arc<DeviceCodeStore>>,
    oidc_providers: Option<Vec<OidcProvider>>,
    catalog: Option<Arc<dyn CollectionCatalog>>,
    jobs: Option<Arc<dyn JobStore>>,
    metrics: Option<Arc<dyn orbit_observability::Recorder>>,
    job_sem: Option<Arc<tokio::sync::Semaphore>>,
}

impl AppStateBuilder {
    /// New builder.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Provide the loaded schema registry.
    #[must_use]
    pub fn with_schemas(mut self, s: Arc<SchemaRegistry>) -> Self {
        self.schemas = Some(s);
        self
    }

    /// Provide an auth policy.
    #[must_use]
    pub fn with_auth(mut self, a: AuthPolicy) -> Self {
        self.auth = Some(Arc::new(a));
        self
    }

    /// Set the openEO API version reported to clients (e.g. "1.3.0").
    #[must_use]
    pub fn with_api_version(mut self, v: impl Into<String>) -> Self {
        self.api_version = Some(Arc::from(v.into().as_str()));
        self
    }

    /// Set the backend id reported in capabilities (e.g. "orbit-rs").
    #[must_use]
    pub fn with_backend_id(mut self, v: impl Into<String>) -> Self {
        self.backend_id = Some(Arc::from(v.into().as_str()));
        self
    }

    /// Inject a process-graph executor. Defaults to [`LocalExecutor`].
    #[must_use]
    pub fn with_executor(mut self, e: Arc<dyn ProcessGraphExecutor>) -> Self {
        self.executor = Some(e);
        self
    }

    /// Inject a file store. Defaults to [`InMemoryFileStore`].
    #[must_use]
    pub fn with_files(mut self, f: Arc<dyn FileStore>) -> Self {
        self.files = Some(f);
        self
    }

    /// Inject an event bus. Defaults to [`InMemoryEventBus`].
    #[must_use]
    pub fn with_events(mut self, e: Arc<dyn EventBus>) -> Self {
        self.events = Some(e);
        self
    }

    /// Inject a per-route security map (parsed from openapi.json).
    #[must_use]
    pub fn with_security(mut self, s: Arc<RouteSecurityMap>) -> Self {
        self.security = Some(s);
        self
    }

    /// Inject a bearer-token store for Basic→Bearer issuance.
    #[must_use]
    pub fn with_tokens(mut self, t: Arc<TokenStore>) -> Self {
        self.tokens = Some(t);
        self
    }

    /// Inject a device-code session store (RFC 8628).
    #[must_use]
    pub fn with_device_codes(mut self, d: Arc<DeviceCodeStore>) -> Self {
        self.device_codes = Some(d);
        self
    }

    /// Configure the OIDC providers advertised on `/credentials/oidc`.
    #[must_use]
    pub fn with_oidc_providers(mut self, providers: Vec<OidcProvider>) -> Self {
        self.oidc_providers = Some(providers);
        self
    }

    /// Inject a collection catalog. Defaults to [`InMemoryCatalog::empty`].
    #[must_use]
    pub fn with_catalog(mut self, c: Arc<dyn CollectionCatalog>) -> Self {
        self.catalog = Some(c);
        self
    }

    /// Inject a job store. Defaults to [`InMemoryJobStore`].
    #[must_use]
    pub fn with_jobs(mut self, j: Arc<dyn JobStore>) -> Self {
        self.jobs = Some(j);
        self
    }

    /// Inject a metrics recorder. Defaults to
    /// [`orbit_observability::InMemoryRecorder`].
    #[must_use]
    pub fn with_metrics(mut self, m: Arc<dyn orbit_observability::Recorder>) -> Self {
        self.metrics = Some(m);
        self
    }

    /// **P0-5 / P1-9**: cap on concurrent job runners. Default 16.
    #[must_use]
    pub fn with_job_concurrency(mut self, permits: usize) -> Self {
        self.job_sem = Some(Arc::new(tokio::sync::Semaphore::new(permits.max(1))));
        self
    }

    /// Finalise. Falls back to safe defaults for any unset field.
    #[must_use]
    pub fn build(self) -> AppState {
        // Resolve catalog first so the default executor can share it.
        let catalog: Arc<dyn CollectionCatalog> = self
            .catalog
            .unwrap_or_else(|| Arc::new(InMemoryCatalog::empty()) as Arc<dyn CollectionCatalog>);
        let executor: Arc<dyn ProcessGraphExecutor> = self.executor.unwrap_or_else(|| {
            Arc::new(LocalExecutor::with_catalog(catalog.clone())) as Arc<dyn ProcessGraphExecutor>
        });
        AppState {
            schemas: self
                .schemas
                .unwrap_or_else(|| Arc::new(SchemaRegistry::empty())),
            auth: self.auth.unwrap_or_else(|| Arc::new(AuthPolicy::Open)),
            api_version: self
                .api_version
                .unwrap_or_else(|| Arc::from("1.3.0")),
            backend_id: self
                .backend_id
                .unwrap_or_else(|| Arc::from("orbit-rs")),
            executor,
            files: self
                .files
                .unwrap_or_else(|| Arc::new(InMemoryFileStore::new()) as Arc<dyn FileStore>),
            events: self
                .events
                .unwrap_or_else(|| Arc::new(InMemoryEventBus::default()) as Arc<dyn EventBus>),
            security: self
                .security
                .unwrap_or_else(|| Arc::new(RouteSecurityMap::empty())),
            tokens: self.tokens.unwrap_or_else(|| Arc::new(TokenStore::new())),
            device_codes: self.device_codes.unwrap_or_else(|| Arc::new(DeviceCodeStore::new())),
            oidc_providers: self.oidc_providers.unwrap_or_default(),
            catalog,
            jobs: self
                .jobs
                .unwrap_or_else(|| Arc::new(InMemoryJobStore::new()) as Arc<dyn JobStore>),
            metrics: self.metrics.unwrap_or_else(|| {
                Arc::new(orbit_observability::InMemoryRecorder::new())
                    as Arc<dyn orbit_observability::Recorder>
            }),
            job_sem: self
                .job_sem
                .unwrap_or_else(|| Arc::new(tokio::sync::Semaphore::new(16))),
            job_registry: Arc::new(crate::job_registry::JobRegistry::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_builder_yields_safe_values() {
        let s = AppStateBuilder::new().build();
        assert_eq!(&*s.api_version, "1.3.0");
        assert_eq!(&*s.backend_id, "orbit-rs");
        assert!(matches!(*s.auth, AuthPolicy::Open));
    }

    #[test]
    fn builder_overrides_propagate() {
        let s = AppStateBuilder::new()
            .with_api_version("1.3.0-orbit")
            .with_backend_id("orbit-test")
            .build();
        assert_eq!(&*s.api_version, "1.3.0-orbit");
        assert_eq!(&*s.backend_id, "orbit-test");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_executor_is_local_and_evaluates() {
        let s = AppStateBuilder::new().build();
        let body = serde_json::json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 4, "y": 5 }, "result": true }
            }}
        });
        let r = s.executor.run_sync(&body).await.expect("local exec ok");
        assert_eq!(r.content_type, "application/json");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v.as_f64().unwrap(), 9.0);
    }
}
