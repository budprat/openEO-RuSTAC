//! In-memory store for user-defined process graphs (UDPs), backing the
//! openEO `/process_graphs` endpoints.
//!
//! UDPs are stored by client-chosen id via `PUT /process_graphs/{id}` and are
//! process-scoped metadata, not heavy assets — an in-memory map is adequate
//! for this single-tenant reference backend. Not persisted across restarts
//! (documented limitation); swap in a DB-backed store if durability is needed.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};

/// Thread-safe in-memory UDP store: `id -> stored UDP object`.
#[derive(Debug, Default)]
pub struct UdpStore {
    inner: Mutex<BTreeMap<String, Value>>,
}

impl UdpStore {
    /// New empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store (or replace) the UDP `body` under `id`. The stored object always
    /// carries its own `id` (set from the path, overriding any in the body)
    /// per the openEO process-object shape.
    pub fn put(&self, id: &str, mut body: Value) {
        if let Value::Object(map) = &mut body {
            map.insert("id".into(), Value::String(id.to_string()));
        }
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), body);
    }

    /// Fetch the stored UDP object for `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Value> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    /// Remove `id`. Returns true if it existed.
    pub fn delete(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id)
            .is_some()
    }

    /// All stored UDPs as a list of summary objects (`id` + carried metadata),
    /// for `GET /process_graphs`. The full `process_graph` is omitted from the
    /// listing per openEO (it's returned by `GET /process_graphs/{id}`).
    #[must_use]
    pub fn list_summaries(&self) -> Vec<Value> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, body)| {
                let mut summary = json!({ "id": id });
                if let (Value::Object(s), Value::Object(b)) = (&mut summary, body) {
                    for k in ["summary", "description", "categories", "parameters", "returns"] {
                        if let Some(v) = b.get(k) {
                            s.insert(k.into(), v.clone());
                        }
                    }
                }
                summary
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete_roundtrip() {
        let s = UdpStore::new();
        s.put("ndvi_udp", json!({ "process_graph": { "n": {} }, "summary": "my ndvi" }));
        let got = s.get("ndvi_udp").expect("stored");
        assert_eq!(got["id"], "ndvi_udp", "id must be set from the key");
        assert_eq!(got["summary"], "my ndvi");
        assert!(got["process_graph"].is_object());
        assert!(s.delete("ndvi_udp"));
        assert!(s.get("ndvi_udp").is_none());
        assert!(!s.delete("ndvi_udp"), "second delete is false");
    }

    #[test]
    fn list_summaries_omits_process_graph() {
        let s = UdpStore::new();
        s.put("a", json!({ "process_graph": { "x": {} }, "summary": "A" }));
        s.put("b", json!({ "process_graph": { "y": {} } }));
        let list = s.list_summaries();
        assert_eq!(list.len(), 2);
        for item in &list {
            assert!(item.get("id").is_some());
            assert!(item.get("process_graph").is_none(), "listing omits the graph");
        }
    }
}
