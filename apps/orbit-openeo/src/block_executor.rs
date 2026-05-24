//! Phase-B #7 — bounded-concurrency block executor.
//!
//! Wraps a stream of block-id work items with:
//! - **Bounded concurrency** via `tokio::sync::Semaphore`
//! - **Cancellation** via shared `AtomicBool`
//! - **Progress** via async callback `(done, total)`
//! - **Memory accounting** via an opt-in budget tracker
//!
//! Closes audit P0-5 (sync gdal blocking tokio worker) by routing
//! per-block work through `tokio::task::spawn_blocking` and the
//! semaphore.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use thiserror::Error;
use tokio::sync::Semaphore;

/// Errors the executor can surface.
#[derive(Debug, Error)]
pub enum BlockExecError {
    /// Worker returned an error.
    #[error("worker error: {0}")]
    Worker(String),
    /// Cancellation token tripped.
    #[error("cancelled by token")]
    Cancelled,
    /// Memory budget tracker rejected the block.
    #[error("memory budget exhausted: requested {requested} B, available {available} B")]
    BudgetExhausted { requested: usize, available: usize },
}

/// Shared cancellation handle.
#[derive(Clone, Debug, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
}

impl Cancel {
    /// New cancellation token (not yet tripped).
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Trip the token; future block submits will fail with `Cancelled`.
    pub fn cancel(&self) { self.flag.store(true, Ordering::SeqCst); }

    /// True iff the token has been tripped.
    #[must_use]
    pub fn is_cancelled(&self) -> bool { self.flag.load(Ordering::SeqCst) }
}

/// Memory budget guard. Atomically allocates / releases bytes around
/// each block. Submit→worker→release is wrapped via [`MemoryGuard`].
#[derive(Clone, Debug)]
pub struct MemoryBudget {
    used: Arc<AtomicUsize>,
    cap: usize,
}

impl MemoryBudget {
    /// New budget with `cap` bytes available.
    #[must_use]
    pub fn new(cap_bytes: usize) -> Self {
        Self { used: Arc::new(AtomicUsize::new(0)), cap: cap_bytes }
    }

    /// Currently-allocated bytes.
    #[must_use]
    pub fn used(&self) -> usize { self.used.load(Ordering::Acquire) }

    /// Bytes still available.
    #[must_use]
    pub fn available(&self) -> usize { self.cap.saturating_sub(self.used()) }

    /// Try to reserve `bytes`. On success returns a `MemoryGuard` that
    /// releases the bytes when dropped.
    pub fn try_reserve(&self, bytes: usize) -> Result<MemoryGuard, BlockExecError> {
        // Compare-and-swap loop on the atomic.
        let mut cur = self.used.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_add(bytes);
            if next > self.cap {
                return Err(BlockExecError::BudgetExhausted {
                    requested: bytes,
                    available: self.cap.saturating_sub(cur),
                });
            }
            match self.used.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(MemoryGuard { used: self.used.clone(), bytes }),
                Err(observed) => cur = observed,
            }
        }
    }
}

/// RAII guard returned by [`MemoryBudget::try_reserve`].
#[derive(Debug)]
pub struct MemoryGuard {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for MemoryGuard {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Progress reporter — passed to the executor; receives `(done, total)`.
pub type ProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// Run `total_blocks` tasks via `worker`, bounded by `n_threads`
/// concurrent runners. Each worker invocation is dispatched on
/// `tokio::task::spawn_blocking` so synchronous GDAL/CPU work doesn't
/// block the async runtime.
///
/// `worker(block_id) -> Result<O, String>` returns one outcome per block.
/// Returns the in-order vector of outcomes (or the first error /
/// cancellation).
pub async fn run_blocks<O, F>(
    total_blocks: usize,
    n_threads: usize,
    cancel: Cancel,
    progress: Option<ProgressFn>,
    worker: F,
) -> Result<Vec<O>, BlockExecError>
where
    O: Send + 'static,
    F: Fn(usize) -> Result<O, String> + Send + Sync + 'static,
{
    let sem = Arc::new(Semaphore::new(n_threads.max(1)));
    let worker = Arc::new(worker);
    let done = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(total_blocks);
    for id in 0..total_blocks {
        if cancel.is_cancelled() {
            return Err(BlockExecError::Cancelled);
        }
        let permit = sem.clone().acquire_owned().await.map_err(|e| {
            BlockExecError::Worker(format!("semaphore closed unexpectedly: {e}"))
        })?;
        let worker = worker.clone();
        let cancel = cancel.clone();
        let progress = progress.clone();
        let done = done.clone();
        let h = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }
            let r = worker(id);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(p) = progress.as_ref() { p(n, total_blocks); }
            r
        });
        handles.push(h);
    }
    let mut out: Vec<O> = Vec::with_capacity(total_blocks);
    for h in handles {
        match h.await.map_err(|e| BlockExecError::Worker(e.to_string()))? {
            Ok(v) => out.push(v),
            Err(e) if e == "cancelled" => return Err(BlockExecError::Cancelled),
            Err(e) => return Err(BlockExecError::Worker(e)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn runs_n_blocks_in_order() {
        let r = run_blocks::<usize, _>(
            10, 4, Cancel::new(), None,
            |id| Ok(id * 2),
        ).await.unwrap();
        assert_eq!(r, (0..10).map(|i| i * 2).collect::<Vec<_>>());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn surfaces_worker_error() {
        let r = run_blocks::<(), _>(
            5, 2, Cancel::new(), None,
            |id| if id == 2 { Err("boom".into()) } else { Ok(()) },
        ).await;
        assert!(matches!(r, Err(BlockExecError::Worker(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_short_circuits_remaining_blocks() {
        let cancel = Cancel::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c.cancel();
        });
        let r = run_blocks::<(), _>(
            1000, 2, cancel, None,
            |_| { std::thread::sleep(std::time::Duration::from_millis(5)); Ok(()) },
        ).await;
        assert!(matches!(r, Err(BlockExecError::Cancelled)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_callback_fires_per_block() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let progress: ProgressFn = Arc::new(move |_done, _total| {
            c.fetch_add(1, Ordering::Relaxed);
        });
        run_blocks::<(), _>(
            5, 2, Cancel::new(), Some(progress),
            |_| Ok(()),
        ).await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn memory_budget_tracks_reservation_and_release() {
        let b = MemoryBudget::new(1000);
        assert_eq!(b.used(), 0);
        assert_eq!(b.available(), 1000);
        {
            let _g = b.try_reserve(400).unwrap();
            assert_eq!(b.used(), 400);
            assert_eq!(b.available(), 600);
            let _g2 = b.try_reserve(500).unwrap();
            assert_eq!(b.used(), 900);
        }
        // Both guards dropped → fully released.
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn memory_budget_rejects_over_cap() {
        let b = MemoryBudget::new(1000);
        let _g = b.try_reserve(800).unwrap();
        match b.try_reserve(500) {
            Err(BlockExecError::BudgetExhausted { requested, available }) => {
                assert_eq!(requested, 500);
                assert_eq!(available, 200);
            }
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }
    }

    #[test]
    fn cancel_token_default_not_tripped() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
    }
}
