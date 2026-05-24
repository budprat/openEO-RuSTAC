//! Pub/sub event bus for the `/subscription` WebSocket.
//!
//! Subscribers receive a `tokio::sync::broadcast::Receiver<JobEvent>`.
//! Publishers send via [`EventBus::publish`]. Slow subscribers that miss
//! events get a `RecvError::Lagged(n)` rather than back-pressuring the
//! publisher — the WebSocket route surfaces this as an explicit
//! `{"orbit.lagged": n}` notice.
//!
//! Future work: bridge to `orbit_etl::engine::Event` once the engine
//! itself exposes an event stream we can multiplex into per-user
//! subscribers.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// One event delivered over the openEO subscription channel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobEvent {
    /// Owning user id (subscribers filter on this).
    pub user_id: String,
    /// openEO job id.
    pub job_id: String,
    /// Event kind in the openEO sense.
    pub kind: JobEventKind,
}

/// openEO job event kinds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEventKind {
    /// Job submitted / queued.
    Started,
    /// Periodic progress tick.
    Progress,
    /// Job finished successfully.
    Completed,
    /// Job failed.
    Failed,
    /// Job cancelled.
    Cancelled,
}

/// Async event-bus surface. Default impl uses `tokio::sync::broadcast`.
pub trait EventBus: Send + Sync {
    /// Push an event to all current subscribers.
    fn publish(&self, ev: JobEvent);
    /// Subscribe — returns a receiver. New subscribers do not receive
    /// pre-existing events.
    fn subscribe(&self) -> broadcast::Receiver<JobEvent>;
}

/// `tokio::sync::broadcast`-backed implementation.
#[derive(Debug, Clone)]
pub struct InMemoryEventBus {
    tx: Arc<broadcast::Sender<JobEvent>>,
}

impl InMemoryEventBus {
    /// New bus with `capacity` slots in the broadcast buffer (slow
    /// subscribers lag past this size).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx: Arc::new(tx) }
    }

    /// How many subscribers are currently attached.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for InMemoryEventBus {
    fn default() -> Self { Self::new(1024) }
}

impl EventBus for InMemoryEventBus {
    fn publish(&self, ev: JobEvent) {
        // send() returns Err iff no receivers are attached; that's fine.
        let _ = self.tx.send(ev);
    }
    fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(user: &str, job: &str, kind: JobEventKind) -> JobEvent {
        JobEvent { user_id: user.into(), job_id: job.into(), kind }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publish_with_no_subscribers_does_not_panic() {
        let bus = InMemoryEventBus::new(8);
        bus.publish(ev("alice", "j1", JobEventKind::Started));
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_then_publish_delivers() {
        let bus = InMemoryEventBus::new(8);
        let mut rx = bus.subscribe();
        bus.publish(ev("alice", "j1", JobEventKind::Started));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.user_id, "alice");
        assert_eq!(got.job_id, "j1");
        assert_eq!(got.kind, JobEventKind::Started);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fanout_to_multiple_subscribers() {
        let bus = InMemoryEventBus::new(8);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.publish(ev("alice", "j1", JobEventKind::Completed));
        assert_eq!(rx1.recv().await.unwrap().job_id, "j1");
        assert_eq!(rx2.recv().await.unwrap().job_id, "j1");
        assert_eq!(bus.receiver_count(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_subscriber_lags() {
        let bus = InMemoryEventBus::new(2);
        let mut rx = bus.subscribe();
        for i in 0..5 {
            bus.publish(ev("alice", &format!("j{i}"), JobEventKind::Progress));
        }
        let err = rx.recv().await.unwrap_err();
        match err {
            broadcast::error::RecvError::Lagged(n) => assert!(n >= 3),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[test]
    fn job_event_serde_roundtrip() {
        let e = ev("alice", "j1", JobEventKind::Completed);
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""kind":"completed""#));
        let back: JobEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}

