//! Job runner — drives a persisted job through its lifecycle.
//!
//! Today the runner is a thin wrapper around
//! `state.executor.run_sync(graph)` plus status writes + event publishes.
//! Atom-C of the integration plan extracts it from `routes::jobs` so the
//! cross-cutting concerns (status transitions, event emission, error
//! mapping) live in one place and can be unit-tested without an HTTP
//! request.
//!
//! Future versions of this module add:
//! - Granular progress ticks (parse → download → compute → save)
//! - Result asset persistence via `state.files`
//! - Cancellation through an `AtomicBool` checked between phases
//! - Retry / back-off for transient compute failures

use std::sync::Arc;

use serde_json::Value;

use crate::event_bus::{EventBus, JobEvent, JobEventKind};
use crate::executor::{ExecError, ProcessGraphExecutor};
use crate::file_store::{FileKey, FileStore};
use crate::job_store::{JobAsset, JobError, JobStatus, JobStore};

/// Lifecycle outcome of a single job run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Job completed successfully.
    Finished,
    /// Executor returned an error (invalid graph / unknown process / backend fault).
    Failed(String),
    /// Backing store rejected one of the lifecycle writes.
    StoreError(String),
}

/// Run one job to completion.
///
/// On entry the job is expected to be in any state other than `Finished`.
/// On exit it will be `Finished` or `Error`. Each transition publishes a
/// matching [`JobEvent`] onto the bus.
pub async fn run_job(
    job_id: &str,
    user_id: &str,
    body: Value,
    store: Arc<dyn JobStore>,
    executor: Arc<dyn ProcessGraphExecutor>,
    bus: Arc<dyn EventBus>,
    files: Arc<dyn FileStore>,
    metrics: Arc<dyn orbit_observability::Recorder>,
) -> RunOutcome {
    let t0 = std::time::Instant::now();
    let record_terminal = |outcome: &str| {
        let labels = orbit_observability::labels(&[("outcome", outcome)]);
        metrics.counter_inc(orbit_observability::MetricName::EtlJobsTotal, &labels, 1);
        let elapsed = t0.elapsed().as_secs_f64();
        metrics.histogram_observe(
            orbit_observability::MetricName::EtlJobDurationSeconds,
            &labels,
            elapsed,
        );
    };
    // queued
    if let Err(e) = store.set_status(job_id, JobStatus::Queued).await {
        return RunOutcome::StoreError(e.to_string());
    }
    bus.publish(JobEvent {
        user_id: user_id.to_string(),
        job_id: job_id.to_string(),
        kind: JobEventKind::Started,
    });

    // running — phase 1: enter compute
    if let Err(e) = store.set_status(job_id, JobStatus::Running).await {
        return RunOutcome::StoreError(e.to_string());
    }
    let _ = store.set_progress(job_id, 10.0).await;
    bus.publish(JobEvent {
        user_id: user_id.to_string(),
        job_id: job_id.to_string(),
        kind: JobEventKind::Progress,
    });

    // compute (download + transform + write)
    //
    // **Job timeout (2026-05-25, ported from JonaAI jobs.py poll-loop)**:
    // bound the executor wall-clock so a hung download / pathological graph
    // can't pin a worker forever. Default 600 s; override via
    // `ORBIT_JOB_TIMEOUT_SECS`. On timeout the job transitions to `error`
    // (NOT a panic) and the timed-out future is dropped. Note: a
    // spawn_blocking GDAL call already in flight finishes in the
    // background (tokio can't preempt it) but its result is discarded —
    // the job is already marked failed, matching JonaAI's behaviour.
    let timeout_secs: u64 = std::env::var("ORBIT_JOB_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(600);
    let result = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        executor.run_sync(&body),
    )
    .await
    {
        Ok(r) => r,
        Err(_elapsed) => {
            tracing::error!(
                job_id = %job_id, timeout_secs,
                "job exceeded ORBIT_JOB_TIMEOUT_SECS — marking error"
            );
            let _ = store.set_status(job_id, JobStatus::Error).await;
            bus.publish(JobEvent {
                user_id: user_id.to_string(),
                job_id: job_id.to_string(),
                kind: JobEventKind::Failed,
            });
            record_terminal("timeout");
            return RunOutcome::Failed(format!("job timed out after {timeout_secs}s"));
        }
    };

    // phase 2: compute done — start persisting
    let _ = store.set_progress(job_id, 80.0).await;
    bus.publish(JobEvent {
        user_id: user_id.to_string(),
        job_id: job_id.to_string(),
        kind: JobEventKind::Progress,
    });

    match result {
        Ok(sync_result) => {
            let asset_name = default_asset_name(&sync_result.content_type);
            let key = FileKey::new(job_id, &asset_name);
            let size = sync_result.body.len() as u64;
            if let Err(e) = files.put(&key, sync_result.body).await {
                let _ = store.set_status(job_id, JobStatus::Error).await;
                bus.publish(JobEvent {
                    user_id: user_id.to_string(),
                    job_id: job_id.to_string(),
                    kind: JobEventKind::Failed,
                });
                return RunOutcome::Failed(format!("file_store: {e}"));
            }
            let _ = store
                .add_asset(
                    job_id,
                    JobAsset {
                        name: asset_name,
                        media_type: sync_result.content_type,
                        size,
                    },
                )
                .await;
            // phase 3: persistence done — finalise.
            let _ = store.set_progress(job_id, 95.0).await;
            bus.publish(JobEvent {
                user_id: user_id.to_string(),
                job_id: job_id.to_string(),
                kind: JobEventKind::Progress,
            });
            if let Err(e) = store.set_status(job_id, JobStatus::Finished).await {
                return RunOutcome::StoreError(e.to_string());
            }
            let _ = store.set_progress(job_id, 100.0).await;
            bus.publish(JobEvent {
                user_id: user_id.to_string(),
                job_id: job_id.to_string(),
                kind: JobEventKind::Completed,
            });
            record_terminal("finished");
            RunOutcome::Finished
        }
        Err(e) => {
            // Map the execution error into a status + event.
            let detail = format_exec_error(&e);
            tracing::error!(job_id = %job_id, error = %detail, "executor failed");
            let _ = store.set_status(job_id, JobStatus::Error).await;
            bus.publish(JobEvent {
                user_id: user_id.to_string(),
                job_id: job_id.to_string(),
                kind: JobEventKind::Failed,
            });
            record_terminal("error");
            RunOutcome::Failed(detail)
        }
    }
}

/// Pick a sensible default asset name from a media type. `application/json`
/// → `result.json`, `image/tiff` → `result.tif`, anything else →
/// `result.bin`.
fn default_asset_name(content_type: &str) -> String {
    let ext = match content_type.split(';').next().unwrap_or("").trim() {
        "application/json" => "json",
        "image/tiff" | "image/geotiff" => "tif",
        "image/png" => "png",
        "application/x-netcdf" => "nc",
        _ => "bin",
    };
    format!("result.{ext}")
}

fn format_exec_error(e: &ExecError) -> String {
    match e {
        ExecError::InvalidGraph(m) => format!("invalid_graph: {m}"),
        ExecError::UnknownProcess(p) => format!("unknown_process: {p}"),
        ExecError::Backend(m) => format!("backend: {m}"),
        ExecError::PerPixelComputation(m) => format!("per_pixel_computation: {m}"),
    }
}

/// Helper trait so the runner can be invoked from a `tokio::spawn` block.
///
/// This trait is **convenience-only**: callers can also just `tokio::spawn(run_job(...))`
/// directly. The trait exists so tests that want to drop-in a fake
/// executor can express intent without re-implementing `JobStore`.
#[allow(dead_code)]
pub trait JobRunner: Send + Sync {
    /// Spawn the job runner. Returns the join handle for awaitable tests.
    fn spawn(
        &self,
        job_id: String,
        user_id: String,
        body: Value,
    ) -> tokio::task::JoinHandle<RunOutcome>;
}

/// Default runner that pulls dependencies off an [`AppState`-style] tuple.
pub struct DefaultRunner {
    /// Job store.
    pub store: Arc<dyn JobStore>,
    /// Executor.
    pub executor: Arc<dyn ProcessGraphExecutor>,
    /// Event bus.
    pub bus: Arc<dyn EventBus>,
    /// File store where output bytes are persisted.
    pub files: Arc<dyn FileStore>,
    /// Metrics recorder.
    pub metrics: Arc<dyn orbit_observability::Recorder>,
}

impl JobRunner for DefaultRunner {
    fn spawn(
        &self,
        job_id: String,
        user_id: String,
        body: Value,
    ) -> tokio::task::JoinHandle<RunOutcome> {
        let store = self.store.clone();
        let executor = self.executor.clone();
        let bus = self.bus.clone();
        let files = self.files.clone();
        let metrics = self.metrics.clone();
        tokio::spawn(async move {
            run_job(&job_id, &user_id, body, store, executor, bus, files, metrics).await
        })
    }
}

/// Convenience: map a `JobStore::get` `NotFound` into the runner's "no
/// such job" outcome. Routers can use this to skip the spawn when the
/// caller asks `POST /jobs/{id}/results` for a non-existent job.
pub fn map_not_found(err: JobError) -> RunOutcome {
    RunOutcome::StoreError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::InMemoryEventBus;
    use crate::executor::{EchoExecutor, LocalExecutor};
    use crate::file_store::InMemoryFileStore;
    use crate::job_store::InMemoryJobStore;

    async fn fixture(body: Value) -> (
        Arc<dyn JobStore>,
        Arc<dyn ProcessGraphExecutor>,
        Arc<dyn EventBus>,
        Arc<dyn FileStore>,
        Arc<dyn orbit_observability::Recorder>,
        String,
        String,
    ) {
        let store: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let executor: Arc<dyn ProcessGraphExecutor> = Arc::new(LocalExecutor::new());
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new(32));
        let files: Arc<dyn FileStore> = Arc::new(InMemoryFileStore::new());
        let metrics: Arc<dyn orbit_observability::Recorder> =
            Arc::new(orbit_observability::InMemoryRecorder::new());
        let rec = store
            .create("alice", None, None, body)
            .await
            .unwrap();
        (store, executor, bus, files, metrics, rec.id, "alice".into())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_job_finishes_a_valid_graph() {
        let body = serde_json::json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 4, "y": 5 }, "result": true }
            }}
        });
        let (store, exe, bus, files, metrics, id, user) = fixture(body.clone()).await;
        let mut sub = bus.subscribe();
        let outcome = run_job(&id, &user, body, store.clone(), exe, bus, files, metrics).await;
        assert_eq!(outcome, RunOutcome::Finished);
        let rec = store.get(&id).await.unwrap();
        assert_eq!(rec.status, JobStatus::Finished);
        assert_eq!(rec.progress, Some(100.0));
        // Lifecycle: Started → Progress → Progress → Progress → Completed.
        let mut kinds = Vec::new();
        for _ in 0..5 {
            kinds.push(sub.recv().await.unwrap().kind);
        }
        assert_eq!(kinds.first().copied(), Some(JobEventKind::Started));
        assert_eq!(kinds.last().copied(), Some(JobEventKind::Completed));
        let progress_count = kinds.iter().filter(|k| **k == JobEventKind::Progress).count();
        assert!(progress_count >= 2, "expected ≥2 Progress events, got {progress_count}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_job_writes_result_bytes_into_files_and_attaches_asset() {
        let body = serde_json::json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 40, "y": 2 }, "result": true }
            }}
        });
        let (store, exe, bus, files, metrics, id, user) = fixture(body.clone()).await;
        let outcome = run_job(&id, &user, body, store.clone(), exe, bus, files.clone(), metrics).await;
        assert_eq!(outcome, RunOutcome::Finished);

        let rec = store.get(&id).await.unwrap();
        assert_eq!(rec.assets.len(), 1);
        assert_eq!(rec.assets[0].name, "result.json");
        assert_eq!(rec.assets[0].media_type, "application/json");

        let key = FileKey::new(&id, "result.json");
        let bytes = files.get(&key).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.as_f64().unwrap(), 42.0);
        assert_eq!(rec.assets[0].size, bytes.len() as u64);
    }

    /// Executor that sleeps longer than the configured timeout, to drive
    /// the timeout path. Returns a trivial result if it ever completes.
    struct SlowExecutor {
        sleep: std::time::Duration,
    }
    #[async_trait::async_trait]
    impl ProcessGraphExecutor for SlowExecutor {
        async fn run_sync(&self, _body: &Value) -> Result<crate::executor::SyncResult, crate::executor::ExecError> {
            tokio::time::sleep(self.sleep).await;
            Ok(crate::executor::SyncResult::json(&serde_json::json!(1)))
        }
        async fn enqueue(&self, _body: &Value) -> Result<String, crate::executor::ExecError> {
            Ok("job-slow".into())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_job_times_out_and_marks_error() {
        // SAFETY: env mutation in a test. The only reader is run_job; the
        // value (1s) is harmless for the millisecond-fast sibling tests.
        unsafe { std::env::set_var("ORBIT_JOB_TIMEOUT_SECS", "1"); }
        let body = serde_json::json!({"process": {"process_graph": {
            "a": {"process_id": "add", "arguments": {"x": 1, "y": 1}, "result": true}
        }}});
        let (store, _exe, bus, files, metrics, id, user) = fixture(body.clone()).await;
        // Swap in a SlowExecutor that sleeps 5s vs the 1s timeout.
        let slow: Arc<dyn ProcessGraphExecutor> = Arc::new(SlowExecutor {
            sleep: std::time::Duration::from_secs(5),
        });
        let outcome = run_job(&id, &user, body, store.clone(), slow, bus, files, metrics).await;
        unsafe { std::env::remove_var("ORBIT_JOB_TIMEOUT_SECS"); }
        assert!(matches!(outcome, RunOutcome::Failed(ref m) if m.contains("timed out")),
                "expected timeout Failed, got {outcome:?}");
        assert_eq!(store.get(&id).await.unwrap().status, JobStatus::Error);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_job_marks_failed_on_unknown_process() {
        let body = serde_json::json!({
            "process": { "process_graph": {
                "x": { "process_id": "totally_made_up", "result": true }
            }}
        });
        let (store, exe, bus, files, metrics, id, user) = fixture(body.clone()).await;
        let mut sub = bus.subscribe();
        let outcome = run_job(&id, &user, body, store.clone(), exe, bus, files.clone(), metrics).await;
        assert!(matches!(outcome, RunOutcome::Failed(_)));
        let rec = store.get(&id).await.unwrap();
        assert_eq!(rec.status, JobStatus::Error);
        // No bytes should have been written for an erroring graph.
        let key = FileKey::new(&id, "result.json");
        assert!(matches!(files.get(&key).await, Err(crate::file_store::FileError::NotFound)));
        // Error lifecycle: Started → Progress (entered compute) → Progress
        // (compute returned, started persisting) → Failed.
        let mut kinds = Vec::new();
        for _ in 0..4 {
            kinds.push(sub.recv().await.unwrap().kind);
        }
        assert_eq!(kinds.first().copied(), Some(JobEventKind::Started));
        assert_eq!(kinds.last().copied(), Some(JobEventKind::Failed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_runner_spawn_joins() {
        let body = serde_json::json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 1, "y": 2 }, "result": true }
            }}
        });
        let (store, exe, bus, files, metrics, id, user) = fixture(body.clone()).await;
        let runner = DefaultRunner {
            store: store.clone(),
            executor: exe,
            bus,
            files,
            metrics,
        };
        let h = runner.spawn(id.clone(), user, body);
        let outcome = h.await.unwrap();
        assert_eq!(outcome, RunOutcome::Finished);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn echo_executor_also_finishes_via_runner() {
        let body = serde_json::json!({
            "process": { "process_graph": {
                "x": { "process_id": "anything", "result": true }
            }}
        });
        let store: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let exe: Arc<dyn ProcessGraphExecutor> = Arc::new(EchoExecutor);
        let bus: Arc<dyn EventBus> = Arc::new(InMemoryEventBus::new(8));
        let files: Arc<dyn FileStore> = Arc::new(InMemoryFileStore::new());
        let metrics: Arc<dyn orbit_observability::Recorder> =
            Arc::new(orbit_observability::InMemoryRecorder::new());
        let rec = store.create("u", None, None, body.clone()).await.unwrap();
        let outcome = run_job(&rec.id, "u", body, store.clone(), exe, bus, files, metrics).await;
        assert_eq!(outcome, RunOutcome::Finished);
        assert_eq!(store.get(&rec.id).await.unwrap().status, JobStatus::Finished);
    }

    // ---------- P3b — Progress events at phase granularity ----------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn happy_path_emits_progress_events_at_three_phases() {
        let body = serde_json::json!({
            "process": { "process_graph": {
                "a": { "process_id": "add", "arguments": { "x": 1, "y": 2 }, "result": true }
            }}
        });
        let (store, exe, bus, files, metrics, id, user) = fixture(body.clone()).await;
        let mut sub = bus.subscribe();
        let outcome = run_job(&id, &user, body, store.clone(), exe, bus, files, metrics).await;
        assert_eq!(outcome, RunOutcome::Finished);

        // Drain all events; expect:
        // 1. Started
        // 2. Progress (entered running, 10%)
        // 3. Progress (compute done, 80%)
        // 4. Progress (persisted, 95%)
        // 5. Completed (100%)
        let mut events = Vec::new();
        for _ in 0..5 {
            events.push(sub.recv().await.unwrap());
        }
        assert_eq!(events[0].kind, JobEventKind::Started);
        for i in 1..=3 {
            assert_eq!(events[i].kind, JobEventKind::Progress, "event[{i}] must be Progress");
        }
        assert_eq!(events[4].kind, JobEventKind::Completed);
        // Job record's progress should be 100 at the end.
        assert_eq!(store.get(&id).await.unwrap().progress, Some(100.0));
    }

    #[test]
    fn default_asset_name_picks_extension_by_content_type() {
        assert_eq!(default_asset_name("application/json"), "result.json");
        assert_eq!(default_asset_name("image/tiff"), "result.tif");
        assert_eq!(default_asset_name("image/png"), "result.png");
        assert_eq!(default_asset_name("application/x-netcdf"), "result.nc");
        assert_eq!(default_asset_name("application/octet-stream"), "result.bin");
        // content-type parameters (`; charset=utf-8`) should not bleed in.
        assert_eq!(default_asset_name("application/json; charset=utf-8"), "result.json");
    }
}
