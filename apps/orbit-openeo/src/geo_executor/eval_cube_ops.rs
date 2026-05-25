//! Cube-metadata openEO processes — band/dimension management that operates
//! on the `__cube` envelope's `bands` map and metadata WITHOUT touching pixel
//! data. Implemented strictly per the openEO 1.3.0 process spec.
//!
//! Processes here:
//! - `filter_bands(data, bands)` — subset the band dimension
//! - `rename_labels(data, dimension, target, source?)` — relabel band keys
//! - `add_dimension(data, name, label, type?)` — add a (metadata) dimension
//! - `drop_dimension(data, name)` — remove a singleton dimension
//!
//! All four are SYNC (pure metadata transforms, no GDAL I/O).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::data_cube::DataCube;
use crate::executor::ExecError;

use super::GeoExecutor;

impl GeoExecutor {
    /// openEO `filter_bands(data, bands)` — keep only the listed bands,
    /// preserving their order in the output cube's metadata. Per spec, the
    /// `bands` parameter is an array of band names (or common names /
    /// wavelengths — we support exact band-name match today).
    ///
    /// Errors with `InvalidGraph` if `bands` is missing/not-an-array, or if a
    /// requested band is absent from the input cube (spec: "If a band is not
    /// available, … the process throws a BandNotAvailable exception").
    pub(super) fn eval_filter_bands(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("filter_bands: missing `data`".into())
        })?;
        let requested: Vec<String> = args
            .get("bands")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ExecError::InvalidGraph(
                "filter_bands: `bands` must be an array of band names".into(),
            ))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if requested.is_empty() {
            return Err(ExecError::InvalidGraph(
                "filter_bands: `bands` array is empty".into(),
            ));
        }
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!("filter_bands: input is not a cube: {e}"))
        })?;
        // Validate every requested band exists (spec: BandNotAvailable).
        for b in &requested {
            if !cube.bands.contains_key(b) {
                return Err(ExecError::InvalidGraph(format!(
                    "filter_bands: band `{b}` not available (have: {:?})",
                    cube.bands.keys().collect::<Vec<_>>()
                )));
            }
        }
        // Keep only requested bands. BTreeMap retains sorted order; openEO
        // doesn't mandate output band order matches the request, only that
        // the subset is correct.
        let mut kept: BTreeMap<String, Vec<std::path::PathBuf>> = BTreeMap::new();
        for b in &requested {
            if let Some(paths) = cube.bands.remove(b) {
                kept.insert(b.clone(), paths);
            }
        }
        cube.bands = kept;
        cube.layers = Some(requested.len() as u64);
        cube.layer_names = Some(requested);
        Ok(cube.to_envelope())
    }

    /// openEO `rename_labels(data, dimension, target, source?)` — rename
    /// labels along a dimension. We support `dimension = "bands"`: rename
    /// band keys. When `source` is provided it must be the same length as
    /// `target` and lists the old names; when omitted, ALL existing labels
    /// are renamed positionally to `target` (spec behavior).
    pub(super) fn eval_rename_labels(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("rename_labels: missing `data`".into())
        })?;
        let dimension = args
            .get("dimension")
            .and_then(|v| v.as_str())
            .unwrap_or("bands")
            .to_string();
        if !matches!(dimension.as_str(), "bands" | "band") {
            return Err(ExecError::InvalidGraph(format!(
                "rename_labels: only `bands` dimension supported (got `{dimension}`)"
            )));
        }
        let target: Vec<String> = args
            .get("target")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ExecError::InvalidGraph(
                "rename_labels: `target` must be an array".into(),
            ))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let source: Option<Vec<String>> = args.get("source").and_then(|v| v.as_array()).map(|a| {
            a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
        });

        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!("rename_labels: input is not a cube: {e}"))
        })?;

        let old_keys: Vec<String> = match &source {
            Some(src) => {
                if src.len() != target.len() {
                    return Err(ExecError::InvalidGraph(format!(
                        "rename_labels: source ({}) and target ({}) length mismatch",
                        src.len(),
                        target.len()
                    )));
                }
                src.clone()
            }
            None => {
                // Positional rename of all existing bands.
                let existing: Vec<String> = cube.bands.keys().cloned().collect();
                if existing.len() != target.len() {
                    return Err(ExecError::InvalidGraph(format!(
                        "rename_labels: cube has {} bands but target has {} labels (omit source only for full rename)",
                        existing.len(),
                        target.len()
                    )));
                }
                existing
            }
        };

        let mut renamed: BTreeMap<String, Vec<std::path::PathBuf>> = BTreeMap::new();
        // First move the renamed entries.
        for (old, new) in old_keys.iter().zip(target.iter()) {
            super::identifier::validate_identifier(new, "rename_labels.target")?;
            let paths = cube.bands.remove(old).ok_or_else(|| {
                ExecError::InvalidGraph(format!("rename_labels: source band `{old}` not found"))
            })?;
            renamed.insert(new.clone(), paths);
        }
        // Forward any bands not in the rename set (e.g. SCL when only
        // renaming the index band).
        for (k, v) in std::mem::take(&mut cube.bands) {
            renamed.entry(k).or_insert(v);
        }
        cube.bands = renamed;
        cube.layer_names = Some(cube.bands.keys().cloned().collect());
        Ok(cube.to_envelope())
    }

    /// openEO `add_dimension(data, name, label, type?)` — add a new
    /// singleton dimension. For our raster cubes this is a metadata-only
    /// operation: we record the added dimension in `extras` so a downstream
    /// `dimension_labels` / `drop_dimension` can see it. Pixel data is
    /// untouched (single label = single slice).
    pub(super) fn eval_add_dimension(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("add_dimension: missing `data`".into())
        })?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ExecError::InvalidGraph("add_dimension: missing `name`".into()))?
            .to_string();
        let label = args.get("label").cloned().unwrap_or(Value::Null);
        let dim_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("other")
            .to_string();
        super::identifier::validate_identifier(&name, "add_dimension.name")?;
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!("add_dimension: input is not a cube: {e}"))
        })?;
        // Record in extras under a dedicated key so we don't collide with
        // modelled fields. Spec: the new dimension has exactly one label.
        let added = cube
            .extras
            .entry("added_dimensions".into())
            .or_insert_with(|| Value::Array(vec![]));
        if let Some(arr) = added.as_array_mut() {
            arr.push(serde_json::json!({ "name": name, "label": label, "type": dim_type }));
        }
        Ok(cube.to_envelope())
    }

    /// openEO `drop_dimension(data, name)` — remove a dimension that has a
    /// single label. We support dropping a previously-`add_dimension`'d
    /// metadata dimension (removes it from `extras.added_dimensions`).
    /// Dropping `bands`/`t`/spatial dims is rejected (they're not singletons
    /// in our model, or are load-bearing).
    pub(super) fn eval_drop_dimension(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("drop_dimension: missing `data`".into())
        })?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ExecError::InvalidGraph("drop_dimension: missing `name`".into()))?
            .to_string();
        if matches!(name.as_str(), "bands" | "band" | "t" | "time" | "x" | "y" | "spatial") {
            return Err(ExecError::InvalidGraph(format!(
                "drop_dimension: cannot drop load-bearing dimension `{name}` \
                 (only metadata dimensions added via add_dimension may be dropped)"
            )));
        }
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!("drop_dimension: input is not a cube: {e}"))
        })?;
        let mut found = false;
        if let Some(Value::Array(arr)) = cube.extras.get_mut("added_dimensions") {
            let before = arr.len();
            arr.retain(|d| d.get("name").and_then(|n| n.as_str()) != Some(name.as_str()));
            found = arr.len() != before;
        }
        if !found {
            return Err(ExecError::InvalidGraph(format!(
                "drop_dimension: dimension `{name}` not found (only metadata dimensions added via add_dimension can be dropped)"
            )));
        }
        Ok(cube.to_envelope())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cube_env() -> Value {
        json!({"__cube": {"bands": {
            "B04": ["/tmp/r0.tif", "/tmp/r1.tif"],
            "B08": ["/tmp/n0.tif", "/tmp/n1.tif"],
            "SCL": ["/tmp/s0.tif", "/tmp/s1.tif"]
        }}})
    }

    #[test]
    fn filter_bands_keeps_only_requested() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_env());
        args.insert("bands".into(), json!(["B04", "B08"]));
        let out = exe.eval_filter_bands(args).unwrap();
        let bands = out["__cube"]["bands"].as_object().unwrap();
        assert_eq!(bands.len(), 2);
        assert!(bands.contains_key("B04"));
        assert!(bands.contains_key("B08"));
        assert!(!bands.contains_key("SCL"), "SCL must be filtered out");
    }

    #[test]
    fn filter_bands_rejects_missing_band() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_env());
        args.insert("bands".into(), json!(["B04", "B99"]));
        let r = exe.eval_filter_bands(args);
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn rename_labels_with_source_renames_specific_band() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_env());
        args.insert("dimension".into(), json!("bands"));
        args.insert("source".into(), json!(["B04"]));
        args.insert("target".into(), json!(["red"]));
        let out = exe.eval_rename_labels(args).unwrap();
        let bands = out["__cube"]["bands"].as_object().unwrap();
        assert!(bands.contains_key("red"), "B04 renamed to red");
        assert!(!bands.contains_key("B04"));
        assert!(bands.contains_key("B08"), "B08 unchanged");
        assert!(bands.contains_key("SCL"), "SCL forwarded");
    }

    #[test]
    fn rename_labels_source_target_length_mismatch_errors() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_env());
        args.insert("source".into(), json!(["B04", "B08"]));
        args.insert("target".into(), json!(["red"]));
        assert!(matches!(exe.eval_rename_labels(args), Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn add_then_drop_dimension_roundtrips() {
        let exe = GeoExecutor::new();
        // add
        let mut a = BTreeMap::new();
        a.insert("data".into(), cube_env());
        a.insert("name".into(), json!("scenario"));
        a.insert("label".into(), json!("baseline"));
        let added = exe.eval_add_dimension(a).unwrap();
        let dims = added["__cube"]["added_dimensions"].as_array().unwrap();
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0]["name"], "scenario");
        // drop
        let mut d = BTreeMap::new();
        d.insert("data".into(), added);
        d.insert("name".into(), json!("scenario"));
        let dropped = exe.eval_drop_dimension(d).unwrap();
        let dims2 = dropped["__cube"]["added_dimensions"].as_array().unwrap();
        assert!(dims2.is_empty(), "scenario dimension dropped");
    }

    #[test]
    fn drop_dimension_rejects_load_bearing() {
        let exe = GeoExecutor::new();
        let mut d = BTreeMap::new();
        d.insert("data".into(), cube_env());
        d.insert("name".into(), json!("bands"));
        assert!(matches!(exe.eval_drop_dimension(d), Err(ExecError::InvalidGraph(_))));
    }
}
