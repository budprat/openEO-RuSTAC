//! Phase-B #8 — provenance records for every output.
//!
//! Every saved asset carries: source STAC ids, process-graph hash,
//! band mapping, software version, CRS, transform, nodata, scale/offset,
//! processing timestamp. Serialises to JSON for inclusion in STAC
//! Items alongside the GeoTIFF.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::datacube::{Crs, GeoTransform};

/// Per-output provenance record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    /// Source STAC item ids that fed this output.
    pub stac_ids: Vec<String>,
    /// Hex SHA-256 of the canonical process-graph JSON.
    pub graph_hash: String,
    /// Logical band name → asset key mapping.
    pub band_mapping: BTreeMap<String, String>,
    /// Software version emitting the output.
    pub software_version: String,
    /// CRS of the output raster.
    pub crs: Crs,
    /// Geo-transform of the output raster.
    pub transform: GeoTransform,
    /// Sentinel value (per logical band).
    pub nodata: BTreeMap<String, f64>,
    /// Scale factor per logical band.
    pub scale: BTreeMap<String, f64>,
    /// Offset per logical band.
    pub offset: BTreeMap<String, f64>,
    /// Processing wall-clock timestamp (RFC 3339).
    pub processed_at: String,
}

impl ProvenanceRecord {
    /// Build a new record with required fields.
    #[must_use]
    pub fn new(graph_hash: impl Into<String>, software_version: impl Into<String>) -> Self {
        Self {
            stac_ids: vec![],
            graph_hash: graph_hash.into(),
            band_mapping: BTreeMap::new(),
            software_version: software_version.into(),
            crs: Crs::epsg(4326),
            transform: GeoTransform([0.0; 6]),
            nodata: BTreeMap::new(),
            scale: BTreeMap::new(),
            offset: BTreeMap::new(),
            processed_at: now_rfc3339(),
        }
    }

    /// Hash an arbitrary process-graph JSON value to a stable hex string.
    /// Uses a hand-rolled FNV-1a 64-bit hash (cryptographically weak,
    /// but stable + dependency-free — enough for provenance equality
    /// checks).
    #[must_use]
    pub fn graph_hash_of(graph_json: &serde_json::Value) -> String {
        let s = canonical_json(graph_json);
        let h = fnv1a_64(s.as_bytes());
        format!("{h:016x}")
    }
}

/// Stable JSON serialisation (BTreeMap-ordered keys) so two
/// process-graphs that only differ in key order hash identically.
fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut buf = String::from("{");
            // Sort keys.
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 { buf.push(','); }
                buf.push('"'); buf.push_str(k); buf.push('"'); buf.push(':');
                buf.push_str(&canonical_json(&m[*k]));
            }
            buf.push('}');
            buf
        }
        serde_json::Value::Array(a) => {
            let mut buf = String::from("[");
            for (i, x) in a.iter().enumerate() {
                if i > 0 { buf.push(','); }
                buf.push_str(&canonical_json(x));
            }
            buf.push(']');
            buf
        }
        other => other.to_string(),
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (now / 86_400) as i64;
    let secs_of_day = now % 86_400;
    let (y, m, d) = days_to_ymd(days + 719_468);
    let (h, mn, s) = (secs_of_day / 3600, (secs_of_day / 60) % 60, secs_of_day % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mn:02}:{s:02}Z")
}

fn days_to_ymd(z: i64) -> (i64, u32, u32) {
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_record_has_default_fields() {
        let p = ProvenanceRecord::new("abc", "orbit-openeo/0.1");
        assert_eq!(p.graph_hash, "abc");
        assert_eq!(p.software_version, "orbit-openeo/0.1");
        assert!(p.stac_ids.is_empty());
        assert!(!p.processed_at.is_empty());
    }

    #[test]
    fn graph_hash_is_stable_across_key_order() {
        let a = json!({"a": 1, "b": 2, "c": [1, 2, 3]});
        let b = json!({"c": [1, 2, 3], "b": 2, "a": 1});
        assert_eq!(
            ProvenanceRecord::graph_hash_of(&a),
            ProvenanceRecord::graph_hash_of(&b),
            "canonical JSON must normalise key order",
        );
    }

    #[test]
    fn graph_hash_changes_when_value_differs() {
        let a = json!({"a": 1});
        let b = json!({"a": 2});
        assert_ne!(
            ProvenanceRecord::graph_hash_of(&a),
            ProvenanceRecord::graph_hash_of(&b),
        );
    }

    #[test]
    fn record_round_trips_through_json() {
        let mut p = ProvenanceRecord::new("h", "v");
        p.stac_ids.push("S2A_X".into());
        p.band_mapping.insert("red".into(), "B04".into());
        p.nodata.insert("red".into(), 0.0);
        p.scale.insert("red".into(), 0.0001);
        p.offset.insert("red".into(), 0.0);
        let s = serde_json::to_string(&p).unwrap();
        let back: ProvenanceRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn rfc3339_format_is_ten_dash_t() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(s.chars().last(), Some('Z'));
    }

    #[test]
    fn canonical_json_handles_nested_objects() {
        let v = json!({"b": {"y": 1, "x": 2}, "a": [3, 2, 1]});
        let s = canonical_json(&v);
        // Keys sorted top-level and within nested map.
        assert!(s.starts_with("{\"a\":["));
        assert!(s.contains("\"b\":{\"x\":2,\"y\":1}"));
    }

    #[test]
    fn fnv1a_known_vectors() {
        assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
        assert_ne!(fnv1a_64(b"a"), fnv1a_64(b"b"));
    }
}
