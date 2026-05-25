//! Typed cube envelope (Atom A1) — replaces ad-hoc `__cube` `serde_json::Value`
//! plumbing between `GeoExecutor` eval arms.
//!
//! The on-wire shape is unchanged: a `{ "__cube": { ... } }` envelope whose
//! inner object carries a band-keyed map of per-scene file paths plus
//! optional spatial/temporal metadata. Per CLAUDE.md A9, the band-name keys
//! are MANDATORY — no `red_paths`/`nir_paths`/`scl_paths` flat shortcuts.
//!
//! The `BTreeMap<String, Vec<PathBuf>>` storage preserves deterministic
//! lexical iteration order so JSON serialisation is reproducible across runs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Errors raised when decoding or querying a `DataCube`.
#[derive(Debug, Error, PartialEq)]
pub enum DataCubeError {
    /// A required band wasn't present in the cube.
    #[error("DataCube missing required band: {0}")]
    MissingBand(String),
    /// The JSON didn't carry a `__cube` envelope.
    #[error("DataCube envelope missing `__cube` key")]
    MissingEnvelope,
    /// serde failed to deserialize the inner object.
    #[error("DataCube decode failed: {0}")]
    Decode(String),
}

/// Inner cube payload that lives under the `__cube` envelope key.
///
/// Field-equivalent of the legacy ad-hoc `serde_json::Map<String, Value>`
/// emitted by `eval_load_collection` and consumed by every transform arm.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DataCube {
    /// band_name -> ordered list of per-scene file paths.
    #[serde(default)]
    pub bands: BTreeMap<String, Vec<PathBuf>>,

    /// **Reflectance scale (2026-05-25, Option B)**: band_name → (scale,
    /// offset) for converting stored DN to physical units
    /// (`physical = DN * scale + offset`). Populated by `load_collection`
    /// from STAC `raster:bands.scale`/`offset`. A band ABSENT from this
    /// map (or with scale 1.0) needs no conversion (already physical, e.g.
    /// SCL classes or a derived index). Compute kernels that read raw
    /// reflectance for absolute math (apply) consult this; ratio indices
    /// (ndvi) are scale-invariant and ignore it. Cleared on outputs that
    /// are already in physical units (index/reduced bands).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub band_scales: BTreeMap<String, (f64, f64)>,

    /// Source collection id (e.g. "sentinel-2-l2a"). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,

    /// `[west, south, east, north]`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Vec<f64>>,

    /// Total scenes the cube covers (== len of any band's path vec). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene_count: Option<u64>,

    /// Time-axis size after a reduce/transform (separate from `scene_count`
    /// because reducers collapse the t axis to 1). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u64>,

    /// Layer count along the band axis. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<u64>,

    /// Output band names produced by an index transform (ndvi/ndmi/...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_names: Option<Vec<String>>,

    /// Source band consumed when this cube is the output of `mask_from_values`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_band: Option<String>,

    /// Names of bands replaced with masked variants on this output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masked_bands: Option<Vec<String>>,

    /// Mask process producing this cube ("mask" or "mask_scl_dilation").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masked_by: Option<String>,

    /// Replacement value substituted by a `mask` call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<i64>,

    /// Catch-all for forward-compat metadata fields not modelled above —
    /// preserves round-trip equality so old cubes don't lose state.
    #[serde(flatten)]
    pub extras: BTreeMap<String, Value>,
}

impl DataCube {
    /// Empty cube — useful as a builder seed in transform arms.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Borrow the path list for `band` if present.
    #[must_use]
    pub fn band(&self, band: &str) -> Option<&[PathBuf]> {
        self.bands.get(band).map(Vec::as_slice)
    }

    /// Borrow the path list for `band`, returning `MissingBand` if absent.
    pub fn band_required(&self, band: &str) -> Result<&[PathBuf], DataCubeError> {
        self.band(band).ok_or_else(|| DataCubeError::MissingBand(band.into()))
    }

    /// Take ownership of `band`'s paths, removing them from the cube.
    /// Avoids cloning the inner `Vec<PathBuf>`.
    pub fn take_band(&mut self, band: &str) -> Result<Vec<PathBuf>, DataCubeError> {
        self.bands
            .remove(band)
            .ok_or_else(|| DataCubeError::MissingBand(band.into()))
    }

    /// Decode a `__cube` envelope from a `serde_json::Value`.
    ///
    /// Accepts either `{"__cube": {...}}` (canonical) or a bare inner-object
    /// `{...}` for convenience inside transform arms that may have already
    /// unwrapped the envelope.
    pub fn from_envelope(v: &Value) -> Result<Self, DataCubeError> {
        let inner = v.get("__cube").unwrap_or(v);
        if inner.is_null() {
            return Err(DataCubeError::MissingEnvelope);
        }
        serde_json::from_value(inner.clone()).map_err(|e| DataCubeError::Decode(e.to_string()))
    }

    /// Decode by-value, consuming the input `Value` to avoid the inner clone.
    ///
    /// **BUG-003 fix (2026-05-24)**: also accepts `__raster` envelopes from
    /// processes that produce a single mosaic file (e.g. `merge_cubes`,
    /// `reduce_dimension`). Auto-wraps them as a single-band, single-scene
    /// `DataCube` so downstream `apply`/`reduce` can operate on
    /// `merge_cubes` output without an explicit conversion step.
    /// The synthetic band name is `result` — chosen to be neutral (not in
    /// the canonical index-band allow-list, exercising the BUG-002 fix's
    /// "first non-SCL band" fallback path).
    pub fn from_envelope_owned(mut v: Value) -> Result<Self, DataCubeError> {
        // __raster fast-path: promote single-file mosaic to a 1-band cube.
        if let Some(raster) = v.as_object_mut().and_then(|m| m.remove("__raster")) {
            return Self::from_raster_envelope(raster);
        }
        let inner = match v.as_object_mut().and_then(|m| m.remove("__cube")) {
            Some(inner) => inner,
            None => v,
        };
        if inner.is_null() {
            return Err(DataCubeError::MissingEnvelope);
        }
        serde_json::from_value(inner).map_err(|e| DataCubeError::Decode(e.to_string()))
    }

    /// Wrap a `__raster` envelope (single-file mosaic) as a 1-band,
    /// 1-scene `DataCube`. The band key is `"result"`. Used by
    /// [`Self::from_envelope_owned`] to bridge processes that emit
    /// `__raster` (merge_cubes, reduce_dimension) into downstream
    /// processes that expect `__cube` (apply, mask, reduce).
    fn from_raster_envelope(raster: Value) -> Result<Self, DataCubeError> {
        let path = raster
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| DataCubeError::Decode(
                "__raster envelope: missing `path` (string)".into(),
            ))?
            .to_string();
        let mut cube = DataCube::new();
        cube.bands.insert("result".into(), vec![PathBuf::from(&path)]);
        cube.scene_count = Some(1);
        Ok(cube)
    }

    /// Serialise back into `{"__cube": {...}}` envelope shape.
    /// Returns `Value::Null` only if the inner serialiser itself fails,
    /// which is impossible for the well-formed struct fields above.
    #[must_use]
    pub fn to_envelope(&self) -> Value {
        let inner = serde_json::to_value(self).unwrap_or(Value::Null);
        serde_json::json!({ "__cube": inner })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_cube() -> DataCube {
        let mut bands = BTreeMap::new();
        bands.insert("B04".into(), vec![PathBuf::from("/tmp/r0.tif"), PathBuf::from("/tmp/r1.tif")]);
        bands.insert("B08".into(), vec![PathBuf::from("/tmp/n0.tif"), PathBuf::from("/tmp/n1.tif")]);
        bands.insert("SCL".into(), vec![PathBuf::from("/tmp/s0.tif"), PathBuf::from("/tmp/s1.tif")]);
        DataCube {
            bands,
            collection: Some("sentinel-2-l2a".into()),
            bbox: Some(vec![144.5, -36.6, 144.7, -36.4]),
            scene_count: Some(2),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_via_serde_json() {
        let cube = sample_cube();
        let env = cube.to_envelope();
        let back = DataCube::from_envelope(&env).expect("decode envelope");
        assert_eq!(cube, back, "envelope round-trip preserves all fields");
    }

    #[test]
    fn band_scales_round_trip_through_envelope() {
        // Option B (2026-05-25): per-band (scale, offset) must survive the
        // __cube envelope so it travels between processes (load → apply).
        let mut cube = sample_cube();
        cube.band_scales.insert("B04".into(), (0.0001, 0.0));
        cube.band_scales.insert("B08".into(), (0.0001, -0.1));
        let env = cube.to_envelope();
        // Serializes as a 2-element array per (scale, offset) tuple.
        assert_eq!(env["__cube"]["band_scales"]["B04"], json!([0.0001, 0.0]));
        let back = DataCube::from_envelope(&env).expect("decode");
        assert_eq!(back.band_scales.get("B04"), Some(&(0.0001, 0.0)));
        assert_eq!(back.band_scales.get("B08"), Some(&(0.0001, -0.1)));
    }

    #[test]
    fn empty_band_scales_is_not_serialized() {
        // skip_serializing_if keeps the envelope clean for scale-less cubes
        // (SCL-only / derived-index cubes) so existing graphs are unchanged.
        let cube = sample_cube();
        let env = cube.to_envelope();
        assert!(env["__cube"].get("band_scales").is_none(),
                "empty band_scales must not appear in the envelope");
    }

    #[test]
    fn missing_band_returns_error() {
        let cube = sample_cube();
        match cube.band_required("NDMI") {
            Err(DataCubeError::MissingBand(b)) => assert_eq!(b, "NDMI"),
            other => panic!("expected MissingBand, got {other:?}"),
        }
    }

    #[test]
    fn empty_cube_serializes_cleanly() {
        let cube = DataCube::new();
        let env = cube.to_envelope();
        let back = DataCube::from_envelope(&env).expect("decode empty");
        assert_eq!(cube, back);
        // No optional fields should leak as nulls.
        let inner = env.get("__cube").expect("__cube key");
        assert!(inner.get("bbox").is_none(), "absent optionals must not serialize");
        assert!(inner.get("collection").is_none());
        assert!(inner.get("scene_count").is_none());
    }

    #[test]
    fn preserves_btreemap_ordering() {
        // Insert in non-lexical order; expect lexical output.
        let mut bands = BTreeMap::new();
        bands.insert("SCL".into(), vec![PathBuf::from("/tmp/s.tif")]);
        bands.insert("B08".into(), vec![PathBuf::from("/tmp/n.tif")]);
        bands.insert("B04".into(), vec![PathBuf::from("/tmp/r.tif")]);
        let cube = DataCube { bands, ..Default::default() };
        let env = cube.to_envelope();
        // Walk the serialised JSON text to verify key order is lexical.
        let s = serde_json::to_string(&env).expect("encode envelope");
        let b04 = s.find("\"B04\"").expect("B04 present");
        let b08 = s.find("\"B08\"").expect("B08 present");
        let scl = s.find("\"SCL\"").expect("SCL present");
        assert!(b04 < b08 && b08 < scl, "bands must serialize in lexical order");
    }

    #[test]
    fn decodes_legacy_inner_object_without_envelope() {
        // Some test fixtures pass the inner object directly. Accept it.
        let inner = json!({
            "bands": { "B04": ["/tmp/r.tif"] }
        });
        let cube = DataCube::from_envelope(&inner).expect("accept bare inner");
        assert_eq!(cube.band_required("B04").unwrap().len(), 1);
    }

    #[test]
    fn from_envelope_owned_drains_envelope() {
        let env = sample_cube().to_envelope();
        let cube = DataCube::from_envelope_owned(env).expect("owned decode");
        assert_eq!(cube.bands.len(), 3);
    }

    #[test]
    fn extras_preserve_forward_compat_fields() {
        let raw = json!({
            "__cube": {
                "bands": { "B04": ["/tmp/r.tif"] },
                "future_field_we_dont_know_about": { "anything": 42 }
            }
        });
        let cube = DataCube::from_envelope(&raw).expect("decode");
        // Round-trip must keep the unknown field reachable.
        let env = cube.to_envelope();
        let inner = env.get("__cube").expect("__cube");
        assert!(inner.get("future_field_we_dont_know_about").is_some(),
            "unknown fields must survive round-trip");
    }

    #[test]
    fn take_band_removes_and_returns_paths() {
        let mut cube = sample_cube();
        let red = cube.take_band("B04").expect("B04 present");
        assert_eq!(red.len(), 2);
        assert!(cube.band("B04").is_none(), "B04 must be gone after take");
    }
}
