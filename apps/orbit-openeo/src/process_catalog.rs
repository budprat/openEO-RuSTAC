//! Canonical catalog of the openEO processes this backend implements.
//!
//! **Single source of truth, non-gated.** This module is deliberately NOT
//! behind the `geo-kernel` feature so two things work even in a
//! `--no-default-features` build:
//!   1. `GET /processes` can advertise the implemented set (openEO API
//!      requires the endpoint to list supported processes — H1 in the
//!      process audit). The previous handler returned `{"processes": []}`,
//!      so every openEO client concluded the backend supported nothing.
//!   2. `POST /validation` and `POST /jobs` can reject a graph that
//!      references an unimplemented process at submit time instead of only
//!      at run time (M4 in the audit).
//!
//! The id set here MUST stay in lock-step with
//! `geo_executor::registry::register_defaults`. A `#[cfg(feature =
//! "geo-kernel")]` test in `registry.rs` asserts the two agree so adding a
//! handler without documenting it (or vice-versa) fails CI.
//!
//! Descriptions are spec-shaped (openEO `Process` objects: `id`, `summary`,
//! `categories`, `parameters[]`, `returns`). Parameter `schema`s are left
//! permissive (`{}` = any) for the math family; the cube-level processes
//! carry their real parameter names so clients can introspect them.

use serde_json::{json, Value};

/// A compact, declarative description of one implemented process. Expanded
/// into a full openEO `Process` JSON object by [`to_process_json`].
struct ProcessSpec {
    /// openEO process id (matches the `process_id` clients send).
    id: &'static str,
    /// One-line human summary.
    summary: &'static str,
    /// openEO process categories (free-form tags used by clients for grouping).
    categories: &'static [&'static str],
    /// Parameter names in declaration order. `*name` (leading `*`) marks an
    /// OPTIONAL parameter; otherwise required. Kept terse on purpose — the
    /// schema is permissive, the names are the useful part for introspection.
    parameters: &'static [&'static str],
    /// Short description of the return value.
    returns: &'static str,
}

/// The full implemented-process table. Order is grouped by category for
/// readability; `process_ids()` sorts for determinism.
const SPECS: &[ProcessSpec] = &[
    // ---- cube pipeline ----
    ProcessSpec { id: "load_collection", summary: "Load a collection into a data cube, filtered by space/time/bands.",
        categories: &["cubes", "import"], parameters: &["id", "spatial_extent", "temporal_extent", "*bands", "*properties"], returns: "A data cube." },
    ProcessSpec { id: "save_result", summary: "Save processing results to the requested output format.",
        categories: &["cubes", "export"], parameters: &["data", "format", "*options"], returns: "Always true." },
    ProcessSpec { id: "reduce_dimension", summary: "Reduce a cube dimension to a single value with a reducer callback.",
        categories: &["cubes", "reducer"], parameters: &["data", "reducer", "dimension", "*context"], returns: "A data cube with the dimension removed." },
    ProcessSpec { id: "apply", summary: "Apply a unary per-pixel process to every value in the cube.",
        categories: &["cubes"], parameters: &["data", "process", "*context"], returns: "A data cube with the same dimensions." },
    ProcessSpec { id: "mask", summary: "Mask a cube with a truthy mask cube (replace masked values).",
        categories: &["cubes", "masks"], parameters: &["data", "mask", "*replacement"], returns: "The masked data cube." },
    ProcessSpec { id: "merge_cubes", summary: "Merge two data cubes (band-axis join, overlap resolver, or spatial mosaic).",
        categories: &["cubes"], parameters: &["cube1", "cube2", "*overlap_resolver", "*context"], returns: "The merged data cube." },
    ProcessSpec { id: "ndvi", summary: "Normalized Difference Vegetation Index from nir/red bands.",
        categories: &["cubes", "vegetation indices"], parameters: &["data", "*nir", "*red", "*target_band"], returns: "A data cube with an NDVI band." },
    ProcessSpec { id: "filter_temporal", summary: "Limit a cube to the given temporal interval.",
        categories: &["cubes", "filter"], parameters: &["data", "extent", "*dimension"], returns: "A temporally-filtered data cube." },
    ProcessSpec { id: "filter_spatial", summary: "Limit a cube to the given spatial geometries.",
        categories: &["cubes", "filter"], parameters: &["data", "geometries"], returns: "A spatially-filtered data cube." },
    ProcessSpec { id: "filter_bbox", summary: "Limit a cube to the given bounding box.",
        categories: &["cubes", "filter"], parameters: &["data", "extent"], returns: "A spatially-filtered data cube." },
    ProcessSpec { id: "filter_bands", summary: "Keep only the requested bands.",
        categories: &["cubes", "filter"], parameters: &["data", "bands"], returns: "A band-subset data cube." },
    ProcessSpec { id: "rename_labels", summary: "Rename labels (band names) along a dimension.",
        categories: &["cubes"], parameters: &["data", "dimension", "target", "*source"], returns: "A data cube with renamed labels." },
    ProcessSpec { id: "add_dimension", summary: "Add a new singleton dimension.",
        categories: &["cubes"], parameters: &["data", "name", "label", "*type"], returns: "A data cube with the added dimension." },
    ProcessSpec { id: "drop_dimension", summary: "Remove a singleton dimension.",
        categories: &["cubes"], parameters: &["data", "name"], returns: "A data cube without the dimension." },
    ProcessSpec { id: "resample_spatial", summary: "Reproject/resample a cube to a target CRS and resolution.",
        categories: &["cubes", "reproject"], parameters: &["data", "*projection", "*resolution", "*method"], returns: "A resampled data cube." },
    ProcessSpec { id: "aggregate_spatial", summary: "Aggregate cube values over geometries with a reducer.",
        categories: &["cubes", "aggregate"], parameters: &["data", "geometries", "reducer", "*target_dimension", "*context"], returns: "A vector cube of aggregated values." },
    // ---- masks (S2 conveniences) ----
    ProcessSpec { id: "mask_scl_dilation", summary: "Mask Sentinel-2 clouds/shadows using the SCL band (dilated).",
        categories: &["cubes", "masks"], parameters: &["data", "*scl_band_name", "*kernel_size"], returns: "A cloud-masked data cube." },
    ProcessSpec { id: "mask_from_values", summary: "Build a binary mask from a band by listing the mask-out class values.",
        categories: &["cubes", "masks"], parameters: &["data", "band", "values"], returns: "A masked data cube." },
    // ---- orbit raster extensions ----
    ProcessSpec { id: "aggregate_spatial_polygon", summary: "(extension) Per-polygon mean of raster values.",
        categories: &["cubes", "aggregate"], parameters: &["data", "geometries"], returns: "Per-polygon means." },
    ProcessSpec { id: "aggregate_spatial_point", summary: "(extension) Sample raster values at points.",
        categories: &["cubes", "aggregate"], parameters: &["data", "points"], returns: "Per-point samples." },
    ProcessSpec { id: "zonal_histogram", summary: "(extension) Per-zone pixel counts from a data + mask raster.",
        categories: &["cubes", "aggregate"], parameters: &["data", "mask"], returns: "Per-zone counts." },
    ProcessSpec { id: "fit_classifier", summary: "(extension) Train a binary logistic classifier.",
        categories: &["machine learning"], parameters: &["x", "y", "*iterations", "*lr"], returns: "A model object." },
    ProcessSpec { id: "predict_classifier", summary: "(extension) Apply a fitted classifier to features.",
        categories: &["machine learning"], parameters: &["model", "x"], returns: "Predictions." },
    // ---- arithmetic ----
    ProcessSpec { id: "add", summary: "x + y.", categories: &["math"], parameters: &["x", "y"], returns: "The sum." },
    ProcessSpec { id: "subtract", summary: "x - y.", categories: &["math"], parameters: &["x", "y"], returns: "The difference." },
    ProcessSpec { id: "multiply", summary: "x * y.", categories: &["math"], parameters: &["x", "y"], returns: "The product." },
    ProcessSpec { id: "divide", summary: "x / y.", categories: &["math"], parameters: &["x", "y"], returns: "The quotient." },
    // ---- unary / binary math ----
    ProcessSpec { id: "absolute", summary: "Absolute value.", categories: &["math"], parameters: &["x"], returns: "|x|." },
    ProcessSpec { id: "sqrt", summary: "Square root.", categories: &["math"], parameters: &["x"], returns: "sqrt(x)." },
    ProcessSpec { id: "power", summary: "base raised to p.", categories: &["math"], parameters: &["base", "p"], returns: "base^p." },
    ProcessSpec { id: "exp", summary: "e raised to p.", categories: &["math"], parameters: &["p"], returns: "e^p." },
    ProcessSpec { id: "ln", summary: "Natural logarithm.", categories: &["math"], parameters: &["x"], returns: "ln(x)." },
    ProcessSpec { id: "log", summary: "Logarithm to a base.", categories: &["math"], parameters: &["x", "base"], returns: "log_base(x)." },
    ProcessSpec { id: "sgn", summary: "Sign of x.", categories: &["math"], parameters: &["x"], returns: "-1, 0 or 1." },
    ProcessSpec { id: "floor", summary: "Round down.", categories: &["math", "rounding"], parameters: &["x"], returns: "floor(x)." },
    ProcessSpec { id: "ceil", summary: "Round up.", categories: &["math", "rounding"], parameters: &["x"], returns: "ceil(x)." },
    ProcessSpec { id: "int", summary: "Truncate to integer.", categories: &["math", "rounding"], parameters: &["x"], returns: "trunc(x)." },
    ProcessSpec { id: "round", summary: "Round half to even to p decimals.", categories: &["math", "rounding"], parameters: &["x", "*p"], returns: "The rounded value." },
    ProcessSpec { id: "mod", summary: "Modulo (sign of divisor).", categories: &["math"], parameters: &["x", "y"], returns: "x mod y." },
    ProcessSpec { id: "clip", summary: "Clamp x to [min, max].", categories: &["math"], parameters: &["x", "min", "max"], returns: "The clamped value." },
    ProcessSpec { id: "normalized_difference", summary: "(x - y) / (x + y).", categories: &["math"], parameters: &["x", "y"], returns: "The normalized difference." },
    // ---- trig ----
    ProcessSpec { id: "cos", summary: "Cosine.", categories: &["math", "trigonometric"], parameters: &["x"], returns: "cos(x)." },
    ProcessSpec { id: "sin", summary: "Sine.", categories: &["math", "trigonometric"], parameters: &["x"], returns: "sin(x)." },
    ProcessSpec { id: "tan", summary: "Tangent.", categories: &["math", "trigonometric"], parameters: &["x"], returns: "tan(x)." },
    ProcessSpec { id: "arccos", summary: "Inverse cosine.", categories: &["math", "trigonometric"], parameters: &["x"], returns: "acos(x)." },
    ProcessSpec { id: "arcsin", summary: "Inverse sine.", categories: &["math", "trigonometric"], parameters: &["x"], returns: "asin(x)." },
    ProcessSpec { id: "arctan", summary: "Inverse tangent.", categories: &["math", "trigonometric"], parameters: &["x"], returns: "atan(x)." },
    ProcessSpec { id: "arctan2", summary: "Two-argument inverse tangent.", categories: &["math", "trigonometric"], parameters: &["y", "x"], returns: "atan2(y, x)." },
    // ---- comparison ----
    ProcessSpec { id: "eq", summary: "Equality (optional delta).", categories: &["comparison"], parameters: &["x", "y", "*delta", "*case_sensitive"], returns: "Boolean." },
    ProcessSpec { id: "neq", summary: "Inequality (optional delta).", categories: &["comparison"], parameters: &["x", "y", "*delta", "*case_sensitive"], returns: "Boolean." },
    ProcessSpec { id: "gt", summary: "Greater than.", categories: &["comparison"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "gte", summary: "Greater than or equal.", categories: &["comparison"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "lt", summary: "Less than.", categories: &["comparison"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "lte", summary: "Less than or equal.", categories: &["comparison"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "between", summary: "Test min <= x <= max.", categories: &["comparison"], parameters: &["x", "min", "max", "*exclude_max"], returns: "Boolean." },
    // ---- logical ----
    ProcessSpec { id: "and", summary: "Logical AND.", categories: &["logic"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "or", summary: "Logical OR.", categories: &["logic"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "xor", summary: "Logical XOR.", categories: &["logic"], parameters: &["x", "y"], returns: "Boolean." },
    ProcessSpec { id: "not", summary: "Logical NOT.", categories: &["logic"], parameters: &["x"], returns: "Boolean." },
    // ---- arrays ----
    ProcessSpec { id: "array_element", summary: "Get an array element by index or label.", categories: &["arrays"], parameters: &["data", "*index", "*label", "*return_nodata"], returns: "The element." },
    ProcessSpec { id: "array_create", summary: "Create an array from values.", categories: &["arrays"], parameters: &["*data", "*repeat"], returns: "An array." },
    ProcessSpec { id: "array_concat", summary: "Concatenate two arrays.", categories: &["arrays"], parameters: &["array1", "array2"], returns: "The concatenated array." },
    ProcessSpec { id: "array_append", summary: "Append a value to an array.", categories: &["arrays"], parameters: &["data", "value"], returns: "The extended array." },
    ProcessSpec { id: "array_contains", summary: "Test array membership.", categories: &["arrays"], parameters: &["data", "value"], returns: "Boolean." },
    ProcessSpec { id: "array_find", summary: "Find the index of a value.", categories: &["arrays"], parameters: &["data", "value"], returns: "The index or null." },
    ProcessSpec { id: "count", summary: "Count elements (optionally by condition).", categories: &["arrays", "reducer"], parameters: &["data", "*condition"], returns: "The count." },
    ProcessSpec { id: "order", summary: "Permutation that would sort the array.", categories: &["arrays", "sorting"], parameters: &["data", "*asc", "*nodata"], returns: "The order indices." },
    ProcessSpec { id: "sort", summary: "Sort the array.", categories: &["arrays", "sorting"], parameters: &["data", "*asc", "*nodata"], returns: "The sorted array." },
];

/// Sorted, de-duplicated list of every implemented process id. Used by
/// `/validation` + `/jobs` to reject unknown processes, and asserted equal
/// to the runtime registry by a geo-kernel test.
#[must_use]
pub fn process_ids() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = SPECS.iter().map(|s| s.id).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// True if `id` is an implemented process.
#[must_use]
pub fn is_known_process(id: &str) -> bool {
    SPECS.iter().any(|s| s.id == id)
}

/// Expand one [`ProcessSpec`] into an openEO `Process` JSON object.
fn to_process_json(spec: &ProcessSpec) -> Value {
    let parameters: Vec<Value> = spec
        .parameters
        .iter()
        .map(|raw| {
            let optional = raw.starts_with('*');
            let name = raw.trim_start_matches('*');
            json!({
                "name": name,
                "description": name,
                "schema": {},
                "optional": optional,
            })
        })
        .collect();
    json!({
        "id": spec.id,
        "summary": spec.summary,
        "description": spec.summary,
        "categories": spec.categories,
        "parameters": parameters,
        "returns": { "description": spec.returns, "schema": {} },
    })
}

/// All implemented processes as openEO `Process` JSON objects, sorted by id
/// for deterministic output. This is the body of `GET /processes.processes`.
#[must_use]
pub fn process_descriptions() -> Vec<Value> {
    let mut specs: Vec<&ProcessSpec> = SPECS.iter().collect();
    specs.sort_by_key(|s| s.id);
    specs.into_iter().map(to_process_json).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_sorted_unique_and_nonempty() {
        let ids = process_ids();
        assert!(ids.len() >= 60, "expected the full implemented set, got {}", ids.len());
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids, sorted, "process ids must be sorted + unique (no duplicate specs)");
    }

    #[test]
    fn known_process_lookup_works() {
        assert!(is_known_process("ndvi"));
        assert!(is_known_process("reduce_dimension"));
        assert!(is_known_process("aggregate_spatial"));
        assert!(!is_known_process("definitely_not_a_process"));
    }

    #[test]
    fn descriptions_are_spec_shaped() {
        let descs = process_descriptions();
        assert_eq!(descs.len(), process_ids().len());
        for d in &descs {
            assert!(d.get("id").and_then(|v| v.as_str()).is_some(), "process needs an id");
            assert!(d.get("parameters").and_then(|v| v.as_array()).is_some(), "process needs parameters[]");
            assert!(d.get("returns").is_some(), "process needs returns");
        }
        // Sorted by id.
        let ids: Vec<&str> = descs.iter().filter_map(|d| d["id"].as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "descriptions must be id-sorted");
    }

    #[test]
    fn load_collection_advertises_its_real_parameters() {
        let descs = process_descriptions();
        let lc = descs.iter().find(|d| d["id"] == "load_collection").expect("load_collection present");
        let params: Vec<&str> = lc["parameters"].as_array().unwrap()
            .iter().filter_map(|p| p["name"].as_str()).collect();
        assert!(params.contains(&"id"));
        assert!(params.contains(&"spatial_extent"));
        assert!(params.contains(&"temporal_extent"));
        assert!(params.contains(&"bands"));
    }
}
