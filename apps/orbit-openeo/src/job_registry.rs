// ABOUTME: In-flight job task registry for cooperative cancellation + drain.
// ABOUTME: Maps job id → (cancel signal, supervisor JoinHandle) for the lifetime of a running job.

//! Tracks the supervisor task of every in-flight job so the server can
//! (a) **cooperatively cancel** a job (`DELETE /jobs/{id}/results`) and
//! (b) **drain** in-flight jobs on graceful shutdown instead of dropping them.
//!
//! Cancellation is *cooperative / best-effort*: signalling a job aborts its
//! supervisor's await of `run_job`, which drops the executor future at its
//! next await point. A `spawn_blocking` GDAL call already in flight cannot be
//! preempted by tokio and finishes in the background — but its result is
//! discarded and the job is marked `Canceled`. This matches the documented
//! `ORBIT_JOB_TIMEOUT_SECS` behaviour.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

struct RegEntry {
    cancel: Arc<Notify>,
    handle: JoinHandle<()>,
}

/// Registry of in-flight job supervisor tasks. Cheap to clone via `Arc`.
#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<HashMap<String, RegEntry>>,
}

impl JobRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an in-flight job's cancel signal + supervisor handle. Must be
    /// called synchronously right after spawning the supervisor (before any
    /// await) so the supervisor — which is not polled until the spawner yields
    /// — cannot deregister before this insert runs.
    pub fn register(&self, id: String, cancel: Arc<Notify>, handle: JoinHandle<()>) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.insert(id, RegEntry { cancel, handle });
    }

    /// Signal cancellation for a job. Returns `true` if the job was in-flight.
    ///
    /// Uses `notify_one` (not `notify_waiters`) so the signal is **buffered**
    /// if the supervisor has not yet parked on `notified()` — this closes the
    /// race where `DELETE /results` arrives in the microsecond window between
    /// the supervisor spawning and reaching its `select!`.
    pub fn cancel(&self, id: &str) -> bool {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match g.get(id) {
            Some(e) => {
                e.cancel.notify_one();
                true
            }
            None => false,
        }
    }

    /// Remove a finished job's entry (called by the supervisor on completion).
    pub fn deregister(&self, id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(id);
    }

    /// Number of jobs currently in-flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Graceful drain: await every in-flight supervisor up to `timeout`. After
    /// the deadline, remaining handles are dropped (their tasks detach and
    /// finish in the background until the process exits). Called once after the
    /// HTTP server's graceful shutdown completes.
    pub async fn drain(&self, timeout: std::time::Duration) {
        let handles: Vec<JoinHandle<()>> = {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.drain().map(|(_, e)| e.handle).collect()
        };
        if handles.is_empty() {
            return;
        }
        let n = handles.len();
        tracing::info!(in_flight = n, ?timeout, "draining in-flight jobs before shutdown");
        match tokio::time::timeout(timeout, futures::future::join_all(handles)).await {
            Ok(_) => tracing::info!(drained = n, "all in-flight jobs completed"),
            Err(_) => tracing::warn!(
                in_flight = n,
                "drain deadline exceeded; remaining jobs detached"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_cancel_deregister_roundtrip() {
        let reg = JobRegistry::new();
        let cancel = Arc::new(Notify::new());
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = fired.clone();
        let c2 = cancel.clone();
        let handle = tokio::spawn(async move {
            c2.notified().await;
            f2.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        reg.register("job-1".into(), cancel, handle);
        assert_eq!(reg.in_flight(), 1);

        // Unknown job → false; known job → true + wakes the waiter.
        assert!(!reg.cancel("nope"));
        assert!(reg.cancel("job-1"));
        // Give the woken task a moment, then drain.
        reg.drain(std::time::Duration::from_secs(2)).await;
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst), "cancel must wake the task");
        assert_eq!(reg.in_flight(), 0, "drain empties the registry");
    }
}
