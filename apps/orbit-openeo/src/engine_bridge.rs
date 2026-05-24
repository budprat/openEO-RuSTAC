//! Bridge from an arbitrary engine event stream into the openEO
//! [`EventBus`].
//!
//! The orbit-etl engine emits `tokio_stream::Stream<Item = Event>`
//! (an mpsc receiver wrapped in `ReceiverStream`). The openEO `/subscription`
//! WebSocket consumes `tokio::sync::broadcast::Receiver<JobEvent>`. This
//! module is the glue.
//!
//! We avoid taking a hard dep on `orbit-etl` from this crate (which would
//! drag in SQLx, Polars, time, dashmap, …) by being generic over the
//! upstream event type and accepting a user-supplied mapping closure.
//!
//! Typical use from the binary:
//!
//! ```ignore
//! use orbit_openeo::engine_bridge::spawn_bridge;
//! use orbit_etl::Event as EtlEvent;
//!
//! let (job_id, stream) = engine.run(spec).await?;
//! let bus = state.events.clone();
//! spawn_bridge(stream, bus, "alice", move |ev| match ev {
//!     EtlEvent::Started { .. }   => JobEventKind::Started,
//!     EtlEvent::Progress { .. }  => JobEventKind::Progress,
//!     EtlEvent::Completed { .. } => JobEventKind::Completed,
//!     EtlEvent::Failed { .. }    => JobEventKind::Failed,
//!     EtlEvent::Cancelled { .. } => JobEventKind::Cancelled,
//! });
//! ```

use std::sync::Arc;

use futures::StreamExt;

use crate::event_bus::{EventBus, JobEvent, JobEventKind};

/// Map a single `orbit_etl::Event` to the openEO `JobEventKind`. This
/// closes the documented use case at the top of this module by giving
/// callers a canonical mapping without duplicating the match arms.
pub fn etl_event_kind(e: &orbit_etl::Event) -> JobEventKind {
    match e {
        orbit_etl::Event::Started { .. } => JobEventKind::Started,
        orbit_etl::Event::Progress { .. } => JobEventKind::Progress,
        orbit_etl::Event::Completed { .. } => JobEventKind::Completed,
        orbit_etl::Event::Failed { .. } => JobEventKind::Failed,
        orbit_etl::Event::Cancelled { .. } => JobEventKind::Cancelled,
    }
}

/// Spawn a tokio task that pulls items off `stream` and republishes them
/// onto `bus` as `JobEvent`s.
///
/// `map_kind` translates each upstream event into the openEO
/// `JobEventKind` we publish. The bridge attaches the supplied
/// `user_id` and `job_id` to every published event.
///
/// Returns the spawned `JoinHandle` so callers can await termination
/// (e.g. for tests).
pub fn spawn_bridge<S, E, F>(
    mut stream: S,
    bus: Arc<dyn EventBus>,
    user_id: impl Into<String>,
    job_id: impl Into<String>,
    mut map_kind: F,
) -> tokio::task::JoinHandle<()>
where
    S: futures::Stream<Item = E> + Send + Unpin + 'static,
    E: Send + 'static,
    F: FnMut(&E) -> JobEventKind + Send + 'static,
{
    let user = user_id.into();
    let job = job_id.into();
    tokio::spawn(async move {
        while let Some(ev) = stream.next().await {
            let kind = map_kind(&ev);
            bus.publish(JobEvent {
                user_id: user.clone(),
                job_id: job.clone(),
                kind,
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::InMemoryEventBus;

    /// Stand-in for an engine event variant.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeEvent {
        Started,
        Progress,
        Completed,
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_forwards_each_event_to_bus() {
        let bus = Arc::new(InMemoryEventBus::new(32)) as Arc<dyn EventBus>;
        let mut rx = bus.subscribe();

        let stream =
            tokio_stream::iter(vec![FakeEvent::Started, FakeEvent::Progress, FakeEvent::Completed]);
        let handle = spawn_bridge(stream, bus.clone(), "alice", "j1", |e| match e {
            FakeEvent::Started => JobEventKind::Started,
            FakeEvent::Progress => JobEventKind::Progress,
            FakeEvent::Completed => JobEventKind::Completed,
        });

        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        let c = rx.recv().await.unwrap();
        assert_eq!(a.kind, JobEventKind::Started);
        assert_eq!(b.kind, JobEventKind::Progress);
        assert_eq!(c.kind, JobEventKind::Completed);
        assert_eq!(a.user_id, "alice");
        assert_eq!(a.job_id, "j1");
        handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_terminates_when_stream_ends() {
        let bus = Arc::new(InMemoryEventBus::new(8)) as Arc<dyn EventBus>;
        let stream = tokio_stream::iter::<Vec<FakeEvent>>(vec![]);
        let handle = spawn_bridge(stream, bus, "u", "j", |_| JobEventKind::Started);
        handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_attaches_correct_user_and_job_ids() {
        let bus = Arc::new(InMemoryEventBus::new(8)) as Arc<dyn EventBus>;
        let mut rx = bus.subscribe();
        let stream = tokio_stream::iter(vec![FakeEvent::Started]);
        let handle = spawn_bridge(stream, bus.clone(), "bob", "job-42", |_| JobEventKind::Started);
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.user_id, "bob");
        assert_eq!(ev.job_id, "job-42");
        handle.await.unwrap();
    }

    // ---------- orbit-etl Event → JobEventKind conversion ----------

    use orbit_core::JobId;

    fn jid() -> JobId { JobId(uuid::Uuid::new_v4()) }

    fn etl_started() -> orbit_etl::Event {
        orbit_etl::Event::Started { job_id: jid() }
    }
    fn etl_progress() -> orbit_etl::Event {
        orbit_etl::Event::Progress { job_id: jid(), rows_read: 100, rows_written: 100 }
    }
    fn etl_completed() -> orbit_etl::Event {
        orbit_etl::Event::Completed { job_id: jid(), rows_read: 100, rows_written: 100 }
    }
    fn etl_failed() -> orbit_etl::Event {
        orbit_etl::Event::Failed { job_id: jid(), error: "boom".into() }
    }
    fn etl_cancelled() -> orbit_etl::Event {
        orbit_etl::Event::Cancelled { job_id: jid(), rows_read: 0, rows_written: 0 }
    }

    #[test]
    fn etl_event_kind_maps_every_variant() {
        assert_eq!(etl_event_kind(&etl_started()), JobEventKind::Started);
        assert_eq!(etl_event_kind(&etl_progress()), JobEventKind::Progress);
        assert_eq!(etl_event_kind(&etl_completed()), JobEventKind::Completed);
        assert_eq!(etl_event_kind(&etl_failed()), JobEventKind::Failed);
        assert_eq!(etl_event_kind(&etl_cancelled()), JobEventKind::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_relays_orbit_etl_events_via_etl_event_kind() {
        // Closes the doc-comment use case from the top of the module.
        let bus = Arc::new(InMemoryEventBus::new(8)) as Arc<dyn EventBus>;
        let mut rx = bus.subscribe();
        let stream = tokio_stream::iter(vec![
            etl_started(),
            etl_progress(),
            etl_completed(),
        ]);
        let handle = spawn_bridge(stream, bus.clone(), "alice", "j-1", |e| etl_event_kind(e));
        assert_eq!(rx.recv().await.unwrap().kind, JobEventKind::Started);
        assert_eq!(rx.recv().await.unwrap().kind, JobEventKind::Progress);
        assert_eq!(rx.recv().await.unwrap().kind, JobEventKind::Completed);
        handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_fan_outs_to_multiple_ws_subscribers() {
        let bus = Arc::new(InMemoryEventBus::new(8)) as Arc<dyn EventBus>;
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let stream = tokio_stream::iter(vec![FakeEvent::Completed]);
        let handle = spawn_bridge(stream, bus.clone(), "u", "j", |_| JobEventKind::Completed);
        assert_eq!(rx1.recv().await.unwrap().kind, JobEventKind::Completed);
        assert_eq!(rx2.recv().await.unwrap().kind, JobEventKind::Completed);
        handle.await.unwrap();
    }
}
