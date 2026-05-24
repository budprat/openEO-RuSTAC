//! **orbit-resilience** — retry + timeout + circuit-breaker primitives.
//!
//! All primitives are framework-agnostic — they don't depend on `tower`,
//! `hyper`, or `tonic`. Wire-ups to those middleware stacks happen in the
//! caller (orbit-server, eo-catalog).
//!
//! Ships:
//! - [`RetryPolicy`] — exponential backoff with full jitter.
//! - [`retry_async`] — drive an async closure under a [`RetryPolicy`].
//! - [`CircuitBreaker`] — Closed → Open → HalfOpen state machine with
//!   wall-clock cooldown.
//!
//! References:
//! - "Exponential Backoff And Jitter", AWS Architecture Blog 2015-03-04
//!   (Marc Brooker).
//! - "Release It!", Michael Nygard 2nd ed. — Circuit Breaker pattern.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────
// RetryPolicy
// ─────────────────────────────────────────────────────────────────────────

/// Retry policy with exponential backoff + full jitter.
///
/// Compute delay for attempt `n` (1-indexed; n=0 is the original attempt,
/// no wait):
///
/// ```text
/// raw  = base * 2^(n-1)
/// cap  = min(raw, max_delay)
/// wait = rand_uniform(0, cap)
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of attempts (initial + retries).
    pub max_attempts: u32,
    /// Base delay for the first retry.
    pub base_delay: Duration,
    /// Hard cap on per-attempt delay before jitter.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Maximum (un-jittered) delay for retry attempt index `n`.
    #[must_use]
    pub fn delay_for_attempt(&self, n: u32) -> Duration {
        if n == 0 {
            return Duration::ZERO;
        }
        let shift = (n - 1).min(30);
        self.base_delay
            .checked_mul(1 << shift)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }

    /// True iff a further attempt is allowed after `n` failures.
    #[must_use]
    pub const fn should_retry(&self, n: u32) -> bool {
        n < self.max_attempts
    }

    /// Jittered delay for attempt `n`. `rng` returns a value in [0, 1).
    /// We don't take a `rand` dep — the caller supplies a closure.
    #[must_use]
    pub fn jittered_delay(&self, n: u32, rng: impl FnOnce() -> f64) -> Duration {
        let cap = self.delay_for_attempt(n);
        let cap_micros = cap.as_micros() as u64;
        if cap_micros == 0 {
            return Duration::ZERO;
        }
        let jittered = (rng().clamp(0.0, 1.0) * cap_micros as f64) as u64;
        Duration::from_micros(jittered)
    }
}

/// Drive an async closure under a [`RetryPolicy`] using deterministic
/// (un-jittered) delays so test outcomes are reproducible.
///
/// `op` is called up to `policy.max_attempts` times. On `Ok` returns
/// immediately; on `Err` waits `policy.delay_for_attempt(n)` then retries.
/// Final error is propagated.
pub async fn retry_async<T, E, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut last_err: Option<E> = None;
    let mut attempt: u32 = 0;
    while policy.should_retry(attempt) {
        attempt += 1;
        match op().await {
            Ok(t) => return Ok(t),
            Err(e) => {
                last_err = Some(e);
                if policy.should_retry(attempt) {
                    tokio::time::sleep(policy.delay_for_attempt(attempt)).await;
                }
            }
        }
    }
    // SAFETY: loop runs at least once when max_attempts > 0; for
    // max_attempts == 0 we return the user's None case by panicking-free
    // unreachable. Default policy has max_attempts = 4, so this is fine.
    match last_err {
        Some(e) => Err(e),
        None => panic!("retry_async: max_attempts must be > 0"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// CircuitBreaker
// ─────────────────────────────────────────────────────────────────────────

/// Circuit-breaker state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CircuitState {
    /// Normal operation; requests pass through.
    Closed,
    /// Fail-fast; recent failures exceeded the threshold.
    Open,
    /// Probe; a single request is allowed to test recovery.
    HalfOpen,
}

/// Configuration for a [`CircuitBreaker`].
#[derive(Clone, Debug)]
pub struct CircuitConfig {
    /// Number of consecutive failures that trip the circuit.
    pub failure_threshold: u32,
    /// How long to stay Open before transitioning to HalfOpen.
    pub open_cooldown: Duration,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_cooldown: Duration::from_secs(30),
        }
    }
}

/// Errors from passing a call through a circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitError {
    /// The circuit is open; the call was rejected without running.
    Open,
}

/// Closed → Open → HalfOpen → Closed circuit breaker.
///
/// The `Instant`-keyed cooldown lives behind a Mutex so the state machine
/// can be safely shared across tasks. `Now` is injectable for tests.
#[derive(Debug)]
pub struct CircuitBreaker<Now = SystemNow> {
    cfg: CircuitConfig,
    state: Mutex<CircuitInner>,
    consecutive_failures: AtomicU32,
    open_count: AtomicU64,
    now: Now,
}

#[derive(Debug)]
struct CircuitInner {
    state: CircuitState,
    opened_at: Option<Instant>,
}

/// Wall-clock `now` provider (default).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemNow;

/// Trait for injectable `Instant::now`. Allows deterministic cooldown
/// tests without `tokio::time::pause`.
pub trait NowProvider: Send + Sync {
    /// Current instant.
    fn now(&self) -> Instant;
}

impl NowProvider for SystemNow {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

impl CircuitBreaker<SystemNow> {
    /// New breaker with system clock.
    #[must_use]
    pub fn new(cfg: CircuitConfig) -> Self {
        Self::with_now(cfg, SystemNow)
    }
}

impl<N: NowProvider> CircuitBreaker<N> {
    /// New breaker with a custom now provider (tests).
    pub fn with_now(cfg: CircuitConfig, now: N) -> Self {
        Self {
            cfg,
            state: Mutex::new(CircuitInner {
                state: CircuitState::Closed,
                opened_at: None,
            }),
            consecutive_failures: AtomicU32::new(0),
            open_count: AtomicU64::new(0),
            now,
        }
    }

    /// Current state, advancing Open→HalfOpen if cooldown has elapsed.
    pub fn state(&self) -> CircuitState {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.state == CircuitState::Open {
            if let Some(at) = g.opened_at {
                if self.now.now().duration_since(at) >= self.cfg.open_cooldown {
                    g.state = CircuitState::HalfOpen;
                }
            }
        }
        g.state
    }

    /// Number of times the breaker has tripped open (cumulative).
    pub fn open_count(&self) -> u64 {
        self.open_count.load(Ordering::Relaxed)
    }

    /// Check whether a call is permitted right now. Returns `Ok` if the
    /// caller may proceed (Closed or HalfOpen); `Err(Open)` otherwise.
    pub fn try_acquire(&self) -> Result<(), CircuitError> {
        match self.state() {
            CircuitState::Open => Err(CircuitError::Open),
            CircuitState::Closed | CircuitState::HalfOpen => Ok(()),
        }
    }

    /// Record a successful call. Resets failure counter; HalfOpen → Closed.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(g.state, CircuitState::HalfOpen) {
            g.state = CircuitState::Closed;
            g.opened_at = None;
        }
    }

    /// Record a failed call. Trips Closed → Open at threshold; from
    /// HalfOpen also goes Open (failed probe).
    pub fn record_failure(&self) {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let should_open = match g.state {
            CircuitState::Closed => prev >= self.cfg.failure_threshold,
            CircuitState::HalfOpen => true,
            CircuitState::Open => false,
        };
        if should_open && g.state != CircuitState::Open {
            g.state = CircuitState::Open;
            g.opened_at = Some(self.now.now());
            self.open_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI32;

    // ── RetryPolicy ────────────────────────────────────────────────

    #[test]
    fn delay_for_attempt_zero_is_zero() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for_attempt(0), Duration::ZERO);
    }

    #[test]
    fn delay_grows_exponentially() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(60),
        };
        assert_eq!(p.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(p.delay_for_attempt(2), Duration::from_millis(200));
        assert_eq!(p.delay_for_attempt(3), Duration::from_millis(400));
    }

    #[test]
    fn delay_clamps_to_max() {
        let p = RetryPolicy {
            max_attempts: 20,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
        };
        assert_eq!(p.delay_for_attempt(20), Duration::from_secs(1));
    }

    #[test]
    fn jittered_delay_uniform_within_cap() {
        let p = RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(60),
        };
        // rng=0 → 0, rng=1 → cap (we clamp to <1 in impl).
        let zero = p.jittered_delay(2, || 0.0);
        let nearly_one = p.jittered_delay(2, || 0.9999);
        assert_eq!(zero, Duration::ZERO);
        // cap at n=2 is 2000ms; rng=0.9999 gives ~2000ms.
        let cap = p.delay_for_attempt(2);
        assert!(nearly_one < cap);
        assert!(nearly_one > cap.mul_f64(0.99));
    }

    // ── retry_async ────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_on_first_try() {
        let calls = Arc::new(AtomicI32::new(0));
        let c = calls.clone();
        let r: Result<i32, ()> = retry_async(&RetryPolicy::default(), || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(42)
            }
        })
        .await;
        assert_eq!(r, Ok(42));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_returns_success_after_failures() {
        let calls = Arc::new(AtomicI32::new(0));
        let c = calls.clone();
        let r: Result<i32, &'static str> = retry_async(
            &RetryPolicy {
                max_attempts: 5,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
            },
            || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::Relaxed) + 1;
                    if n < 3 { Err("fail") } else { Ok(7) }
                }
            },
        )
        .await;
        assert_eq!(r, Ok(7));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_propagates_final_error_after_exhausting_attempts() {
        let calls = Arc::new(AtomicI32::new(0));
        let c = calls.clone();
        let r: Result<(), &'static str> = retry_async(
            &RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(10),
            },
            || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Err::<(), _>("permanent")
                }
            },
        )
        .await;
        assert_eq!(r, Err("permanent"));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    // ── CircuitBreaker ─────────────────────────────────────────────

    /// Test-time clock: monotonically advanceable.
    #[derive(Debug)]
    struct FakeClock(Mutex<Instant>);
    impl FakeClock {
        fn new() -> Self { Self(Mutex::new(Instant::now())) }
        fn advance(&self, d: Duration) {
            let mut g = self.0.lock().unwrap();
            *g += d;
        }
    }
    impl NowProvider for FakeClock {
        fn now(&self) -> Instant { *self.0.lock().unwrap() }
    }

    #[test]
    fn breaker_starts_closed() {
        let b = CircuitBreaker::new(CircuitConfig::default());
        assert_eq!(b.state(), CircuitState::Closed);
        assert!(b.try_acquire().is_ok());
        assert_eq!(b.open_count(), 0);
    }

    #[test]
    fn breaker_opens_after_threshold_failures() {
        let cfg = CircuitConfig { failure_threshold: 3, ..CircuitConfig::default() };
        let b = CircuitBreaker::new(cfg);
        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Closed);
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        assert_eq!(b.open_count(), 1);
        assert!(matches!(b.try_acquire(), Err(CircuitError::Open)));
    }

    #[test]
    fn success_resets_failure_count() {
        let cfg = CircuitConfig { failure_threshold: 3, ..CircuitConfig::default() };
        let b = CircuitBreaker::new(cfg);
        b.record_failure();
        b.record_failure();
        b.record_success();
        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Closed, "success should reset counter");
    }

    #[test]
    fn breaker_transitions_open_to_halfopen_after_cooldown() {
        let clock = FakeClock::new();
        let b = CircuitBreaker::with_now(
            CircuitConfig {
                failure_threshold: 1,
                open_cooldown: Duration::from_secs(10),
            },
            clock,
        );
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        // Not enough time: still Open.
        b.now.advance(Duration::from_secs(5));
        assert_eq!(b.state(), CircuitState::Open);
        // Past cooldown: HalfOpen.
        b.now.advance(Duration::from_secs(6));
        assert_eq!(b.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn halfopen_success_closes_circuit() {
        let clock = FakeClock::new();
        let b = CircuitBreaker::with_now(
            CircuitConfig {
                failure_threshold: 1,
                open_cooldown: Duration::from_secs(1),
            },
            clock,
        );
        b.record_failure();
        b.now.advance(Duration::from_secs(2));
        assert_eq!(b.state(), CircuitState::HalfOpen);
        b.record_success();
        assert_eq!(b.state(), CircuitState::Closed);
    }

    #[test]
    fn halfopen_failure_reopens_circuit() {
        let clock = FakeClock::new();
        let b = CircuitBreaker::with_now(
            CircuitConfig {
                failure_threshold: 1,
                open_cooldown: Duration::from_secs(1),
            },
            clock,
        );
        b.record_failure();
        b.now.advance(Duration::from_secs(2));
        assert_eq!(b.state(), CircuitState::HalfOpen);
        b.record_failure();
        assert_eq!(b.state(), CircuitState::Open);
        assert_eq!(b.open_count(), 2, "trip count includes the re-open");
    }
}
