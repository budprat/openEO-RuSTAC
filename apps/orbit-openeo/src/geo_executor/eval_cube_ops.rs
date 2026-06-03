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
use std::path::PathBuf;

use serde_json::Value;

use orbit_geo::providers::CropWindow;

use crate::data_cube::DataCube;
use crate::executor::ExecError;

use super::eval_load::{normalise_iso, parse_bbox};
use super::GeoExecutor;

impl GeoExecutor {
    /// openEO `filter_temporal(data, extent, dimension?)` — keep only the
    /// scenes whose acquisition timestamp falls inside `extent`
    /// (`[start, end]`; either bound may be `null` for open-ended).
    ///
    /// **H2 (process audit)**: this was previously a silent no-op metadata
    /// tag, so a graph relying on it got UNFILTERED data back. It now prunes
    /// the per-scene path vectors (every band) and the `datetimes` vector by
    /// the surviving indices. Requires `__cube.datetimes` (populated by
    /// `load_collection`); errors if absent rather than guessing.
    ///
    /// Bounds are normalised to RFC3339 (date-only `start` → `T00:00:00Z`,
    /// date-only `end` → `T23:59:59Z`) and compared lexicographically — valid
    /// for same-format UTC timestamps. The interval is inclusive on both ends
    /// (matching this backend's `load_collection` temporal behaviour).
    pub(super) fn eval_filter_temporal(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("filter_temporal: missing `data`".into())
        })?;
        let extent = args.get("extent").and_then(|v| v.as_array()).ok_or_else(|| {
            ExecError::InvalidGraph("filter_temporal: `extent` must be a 2-element array".into())
        })?;
        if extent.len() != 2 {
            return Err(ExecError::InvalidGraph(
                "filter_temporal: `extent` must have exactly two elements".into(),
            ));
        }
        let start = extent[0].as_str().map(|s| normalise_iso(s, true));
        let end = extent[1].as_str().map(|s| normalise_iso(s, false));

        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!("filter_temporal: input is not a cube: {e}"))
        })?;
        let datetimes = cube.datetimes.clone().ok_or_else(|| {
            ExecError::InvalidGraph(
                "filter_temporal: cube has no per-scene datetimes (load_collection did not \
                 supply STAC `properties.datetime`); cannot filter by time".into(),
            )
        })?;

        // Surviving scene indices (inclusive bounds, lexical RFC3339 compare).
        let keep: Vec<usize> = datetimes
            .iter()
            .enumerate()
            .filter(|(_, dt)| {
                let after_start = start.as_deref().map(|s| dt.as_str() >= s).unwrap_or(true);
                let before_end = end.as_deref().map(|e| dt.as_str() <= e).unwrap_or(true);
                after_start && before_end
            })
            .map(|(i, _)| i)
            .collect();
        if keep.is_empty() {
            return Err(ExecError::Backend(
                "filter_temporal: no scenes fall within the requested extent".into(),
            ));
        }

        // Prune every band's path vector + the datetimes vector by `keep`.
        for paths in cube.bands.values_mut() {
            if paths.len() == datetimes.len() {
                *paths = keep.iter().map(|&i| paths[i].clone()).collect();
            }
            // Bands whose length doesn't match the time axis (already reduced)
            // are left untouched.
        }
        cube.datetimes = Some(keep.iter().map(|&i| datetimes[i].clone()).collect());
        cube.scene_count = Some(keep.len() as u64);
        Ok(cube.to_envelope())
    }

    /// openEO `filter_bbox(data, extent)` — spatially crop the cube to a
    /// tighter bounding box. **H2 (process audit)**: was a silent no-op.
    ///
    /// Re-crops every (band, scene) raster to the intersection of its current
    /// extent and `extent` using the in-process GDAL crop primitive
    /// (`download_in_process_with_crs`), honoring the bbox CRS. Blocking GDAL
    /// is routed through `spawn_blocking` + the download semaphore (P0-5).
    pub(super) async fn eval_filter_bbox(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("filter_bbox: missing `data`".into())
        })?;
        let extent = args.get("extent").ok_or_else(|| {
            ExecError::InvalidGraph("filter_bbox: missing `extent` bounding box".into())
        })?;
        let (bbox, crs) = parse_bbox(extent)?;
        orbit_geo::providers::validate_crs_spec(&crs)
            .map_err(|e| ExecError::InvalidGraph(format!("filter_bbox.extent.crs: {e}")))?;
        self.recrop_cube_to_bbox(data, bbox, crs, "filter_bbox").await
    }

    /// openEO `filter_spatial(data, geometries)` — crop the cube to the
    /// bounding envelope of the supplied geometries. **H2 (process audit)**:
    /// was a silent no-op. Derives a bbox from the geometry coordinates and
    /// delegates to the same re-crop path as `filter_bbox`.
    pub(super) async fn eval_filter_spatial(
        &self,
        mut args: BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let data = args.remove("data").ok_or_else(|| {
            ExecError::InvalidGraph("filter_spatial: missing `data`".into())
        })?;
        let geometries = args.get("geometries").ok_or_else(|| {
            ExecError::InvalidGraph("filter_spatial: missing `geometries`".into())
        })?;
        let bbox = geojson_bbox(geometries).ok_or_else(|| {
            ExecError::InvalidGraph(
                "filter_spatial: could not derive a bbox from `geometries` \
                 (expected GeoJSON with numeric coordinates)".into(),
            )
        })?;
        // Geometries default to EPSG:4326 per GeoJSON/openEO convention.
        self.recrop_cube_to_bbox(data, bbox, "EPSG:4326".to_string(), "filter_spatial").await
    }

    /// Shared re-crop worker for `filter_bbox` / `filter_spatial`: opens each
    /// (band, scene) raster and writes a new GeoTIFF cropped to `bbox` (in
    /// `crs`). Bands keep their identity; the cube `bbox` is updated.
    async fn recrop_cube_to_bbox(
        &self,
        data: Value,
        bbox: [f64; 4],
        crs: String,
        who: &str,
    ) -> Result<Value, ExecError> {
        let mut cube = DataCube::from_envelope_owned(data).map_err(|e| {
            ExecError::InvalidGraph(format!("{who}: input is not a cube: {e}"))
        })?;
        if cube.bands.is_empty() {
            return Err(ExecError::InvalidGraph(format!("{who}: cube has no bands")));
        }
        let crop = CropWindow {
            min_x: bbox[0],
            min_y: bbox[1],
            max_x: bbox[2],
            max_y: bbox[3],
        };
        let band_keys: Vec<String> = cube.bands.keys().cloned().collect();
        for band_key in &band_keys {
            super::identifier::validate_identifier(band_key, &format!("{who}.band_key"))?;
            let paths = cube.take_band(band_key).map_err(|_| {
                ExecError::Backend(format!("{who}: band `{band_key}` vanished"))
            })?;
            let mut out_paths: Vec<PathBuf> = Vec::with_capacity(paths.len());
            for (t, in_path) in paths.iter().enumerate() {
                let dst = self.scratch_dir.join(format!(
                    "{who}_{band_key}_t{t}_{}.tif",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
                let src = in_path.to_string_lossy().into_owned();
                let crs_owned = crs.clone();
                let permit = self
                    .download_sem
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| ExecError::Backend(format!("{who}: semaphore: {e}")))?;
                let out = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    orbit_geo::providers::download_in_process_with_crs(
                        &src,
                        &dst,
                        Some(crop),
                        Some(crs_owned.as_str()),
                    )
                })
                .await
                .map_err(|e| ExecError::Backend(format!("{who}: spawn_blocking join: {e}")))?
                .map_err(|e| ExecError::Backend(format!("{who}: crop {band_key} t={t}: {e}")))?;
                out_paths.push(out);
            }
            cube.bands.insert(band_key.clone(), out_paths);
        }
        cube.bbox = Some(vec![bbox[0], bbox[1], bbox[2], bbox[3]]);
        Ok(cube.to_envelope())
    }
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

/// Derive a `[west, south, east, north]` bbox (EPSG:4326) from a GeoJSON-ish
/// value by walking every numeric `[x, y, ...]` coordinate pair it contains.
/// Accepts a Feature, FeatureCollection, Geometry, GeometryCollection, a bare
/// coordinates array, or an openEO `{west,south,east,north}` object. Returns
/// `None` when no coordinate pair can be found.
fn geojson_bbox(v: &Value) -> Option<[f64; 4]> {
    // openEO BoundingBox object shortcut.
    if let (Some(w), Some(s), Some(e), Some(n)) = (
        v.get("west").and_then(Value::as_f64),
        v.get("south").and_then(Value::as_f64),
        v.get("east").and_then(Value::as_f64),
        v.get("north").and_then(Value::as_f64),
    ) {
        return Some([w, s, e, n]);
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut found = false;
    // Recursively collect numeric [x, y] leaf pairs.
    fn walk(v: &Value, min_x: &mut f64, min_y: &mut f64, max_x: &mut f64, max_y: &mut f64, found: &mut bool) {
        match v {
            Value::Array(arr) => {
                // A coordinate pair = array whose first two elements are numbers.
                if arr.len() >= 2 && arr[0].is_number() && arr[1].is_number() {
                    if let (Some(x), Some(y)) = (arr[0].as_f64(), arr[1].as_f64()) {
                        *min_x = min_x.min(x);
                        *min_y = min_y.min(y);
                        *max_x = max_x.max(x);
                        *max_y = max_y.max(y);
                        *found = true;
                        return;
                    }
                }
                for inner in arr {
                    walk(inner, min_x, min_y, max_x, max_y, found);
                }
            }
            Value::Object(map) => {
                for inner in map.values() {
                    walk(inner, min_x, min_y, max_x, max_y, found);
                }
            }
            _ => {}
        }
    }
    walk(v, &mut min_x, &mut min_y, &mut max_x, &mut max_y, &mut found);
    if found && min_x < max_x && min_y < max_y {
        Some([min_x, min_y, max_x, max_y])
    } else {
        None
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

    // ---------- H2: filter_temporal real pruning (no GDAL) ----------

    fn cube_with_datetimes() -> Value {
        json!({"__cube": {
            "bands": {
                "B04": ["/tmp/r0.tif", "/tmp/r1.tif", "/tmp/r2.tif"],
                "B08": ["/tmp/n0.tif", "/tmp/n1.tif", "/tmp/n2.tif"]
            },
            "datetimes": ["2024-06-05T10:00:00Z", "2024-06-15T10:00:00Z", "2024-06-25T10:00:00Z"],
            "scene_count": 3
        }})
    }

    #[test]
    fn filter_temporal_prunes_scenes_outside_extent() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_with_datetimes());
        // Keep only the middle scene (2024-06-10 .. 2024-06-20).
        args.insert("extent".into(), json!(["2024-06-10", "2024-06-20"]));
        let out = exe.eval_filter_temporal(args).unwrap();
        let cube = &out["__cube"];
        assert_eq!(cube["bands"]["B04"].as_array().unwrap().len(), 1);
        assert_eq!(cube["bands"]["B08"].as_array().unwrap().len(), 1);
        assert_eq!(cube["bands"]["B04"][0], "/tmp/r1.tif");
        assert_eq!(cube["datetimes"].as_array().unwrap().len(), 1);
        assert_eq!(cube["scene_count"], 1);
    }

    #[test]
    fn filter_temporal_open_ended_start_keeps_through_end() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_with_datetimes());
        // null start, end = 2024-06-20 → keep scenes 0 and 1.
        args.insert("extent".into(), json!([null, "2024-06-20"]));
        let out = exe.eval_filter_temporal(args).unwrap();
        assert_eq!(out["__cube"]["bands"]["B04"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn filter_temporal_errors_without_datetimes() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_env()); // no datetimes
        args.insert("extent".into(), json!(["2024-06-01", "2024-06-30"]));
        assert!(matches!(exe.eval_filter_temporal(args), Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn filter_temporal_empty_window_errors() {
        let exe = GeoExecutor::new();
        let mut args = BTreeMap::new();
        args.insert("data".into(), cube_with_datetimes());
        args.insert("extent".into(), json!(["2030-01-01", "2030-12-31"]));
        assert!(matches!(exe.eval_filter_temporal(args), Err(ExecError::Backend(_))));
    }

    #[test]
    fn geojson_bbox_from_polygon_and_bbox_object() {
        // Polygon ring → envelope.
        let poly = json!({
            "type": "Polygon",
            "coordinates": [[[16.30, 48.18], [16.40, 48.18], [16.40, 48.24], [16.30, 48.24], [16.30, 48.18]]]
        });
        assert_eq!(geojson_bbox(&poly), Some([16.30, 48.18, 16.40, 48.24]));
        // openEO BoundingBox object.
        let bb = json!({"west": 1.0, "south": 2.0, "east": 3.0, "north": 4.0});
        assert_eq!(geojson_bbox(&bb), Some([1.0, 2.0, 3.0, 4.0]));
        // Degenerate / empty → None.
        assert_eq!(geojson_bbox(&json!({"type": "Point", "coordinates": [1.0, 1.0]})), None);
    }
}
