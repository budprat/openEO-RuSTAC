//! `load_collection` + extent / properties parsers.

use std::path::PathBuf;

use orbit_geo::providers::CropWindow;
use serde_json::{json, Value};

use crate::executor::ExecError;

use super::stac::StacScene;
use super::GeoExecutor;

/// Resolve `band` against a scene's `bands` map, falling back to the
/// well-known legacy alias (e.g. asking for `B04` matches `red` if the
/// searcher kept the original asset key). Returns `None` if neither
/// the requested name nor a known alias is present.
pub(super) fn resolve_band_href(scene: &StacScene, band: &str) -> Option<String> {
    if let Some(h) = scene.bands.get(band) {
        return Some(h.clone());
    }
    // Aliases — symmetric counterpart of `canonical_band_name` in stac.rs.
    let aliases: &[&str] = match band {
        "B04" => &["red", "Red"],
        "B08" => &["nir", "Nir", "NIR"],
        "B03" => &["green"],
        "B02" => &["blue"],
        "B11" => &["swir16", "SWIR16"],
        "B12" => &["swir22", "SWIR22"],
        "SCL" => &["scl"],
        _ => &[],
    };
    for alias in aliases {
        if let Some(h) = scene.bands.get(*alias) {
            return Some(h.clone());
        }
    }
    None
}

impl GeoExecutor {
    pub(super) async fn eval_load_collection(
        &self,
        args: std::collections::BTreeMap<String, Value>,
    ) -> Result<Value, ExecError> {
        let id_arg = args.get("id").cloned().unwrap_or(Value::Null);
        let id_str = id_arg
            .as_str()
            .ok_or_else(|| ExecError::InvalidGraph(
                "load_collection: `id` argument must be a string".into(),
            ))?
            .to_string();

        // Catalog round-trip (so unknown collections fail loudly).
        if let Some(cat) = &self.catalog {
            cat.get(&id_str).await.map_err(|e| match e {
                crate::catalog::CatalogError::NotFound(id) => {
                    ExecError::Backend(format!("CollectionNotFound: {id}"))
                }
                other => ExecError::Backend(format!("catalog: {other}")),
            })?;
        }

        // If we have a searcher AND an explicit spatial_extent → run the
        // real STAC search + cropped-COG download pipeline. Otherwise
        // fall back to the lightweight cube sentinel so arithmetic-only
        // graphs still work.
        let spatial = args.get("spatial_extent");
        if let (Some(searcher), Some(spatial)) = (&self.searcher, spatial) {
            let (bbox, crs) = parse_bbox(spatial)?;
            let datetime = args
                .get("temporal_extent")
                .and_then(|v| parse_temporal(v).ok())
                .as_deref()
                .map(str::to_string);
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as u32;
            let max_cloud_cover = args
                .get("properties")
                .and_then(parse_eo_cloud_cover_lt);
            let scenes = searcher
                .search(&id_str, bbox, datetime.as_deref(), limit, max_cloud_cover)
                .await?;
            if scenes.is_empty() {
                return Err(ExecError::Backend(format!(
                    "STAC search returned no scenes for collection `{id_str}` in bbox {bbox:?}"
                )));
            }

            // Parse `bands` argument (openEO `load_collection.bands`).
            // When omitted, default to the canonical S2 backbone so
            // legacy graphs that don't set `bands` continue to work.
            let requested_bands: Vec<String> = args
                .get("bands")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_else(|| {
                    vec!["B04".to_string(), "B08".to_string(), "SCL".to_string()]
                });
            if requested_bands.is_empty() {
                return Err(ExecError::InvalidGraph(
                    "load_collection: `bands` argument must contain at least one band".into(),
                ));
            }
            // B1: validate every band name before it propagates to scratch_dir.join.
            for band in &requested_bands {
                super::identifier::validate_identifier(band, "load_collection.bands")?;
            }
            // B2: validate the CRS spec before it reaches gdal_translate.
            orbit_geo::providers::validate_crs_spec(&crs)
                .map_err(|e| ExecError::InvalidGraph(format!("load_collection.spatial_extent.crs: {e}")))?;

            // Download cropped windows for every requested band of every
            // scene. Scenes lacking a requested band cause that band to
            // be dropped from the output cube (only bands present on ALL
            // scenes survive, mirroring the old scl_paths "all-or-none"
            // contract).
            let crop = CropWindow {
                min_x: bbox[0],
                min_y: bbox[1],
                max_x: bbox[2],
                max_y: bbox[3],
            };
            // **P0-5 / P1-9**: concurrent per-band downloads via
            // `fetch_with_cache_async`, bounded by `download_sem`.
            use futures::stream::{FuturesUnordered, StreamExt};
            let mut tasks = FuturesUnordered::new();
            for (i, s) in scenes.iter().enumerate() {
                for band in &requested_bands {
                    // Resolve band on this scene: prefer exact match,
                    // then check the legacy alias map. If absent, skip
                    // — handled below by the "all scenes must have it"
                    // check.
                    let href = resolve_band_href(s, band);
                    if let Some(href) = href {
                        let dst = self.scratch_dir.join(format!("{band}_{i}.tif"));
                        let band_owned = band.clone();
                        let crs_owned = crs.clone();
                        tasks.push(Box::pin(async move {
                            let p = self
                                .fetch_with_cache_async(href, dst, crop, Some(crs_owned))
                                .await?;
                            Ok::<_, ExecError>((i, band_owned, p))
                        })
                            as std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                            Output = Result<(usize, String, PathBuf), ExecError>,
                                        > + Send
                                        + '_,
                                >,
                            >);
                    }
                }
            }
            // band_name → vec<Option<PathBuf>> indexed by scene.
            let n = scenes.len();
            let mut per_band: std::collections::BTreeMap<String, Vec<Option<PathBuf>>> =
                std::collections::BTreeMap::new();
            for band in &requested_bands {
                per_band.insert(band.clone(), vec![None; n]);
            }
            while let Some(res) = tasks.next().await {
                let (i, band, p) = res?;
                if let Some(slot) = per_band.get_mut(&band) {
                    slot[i] = Some(p);
                }
            }
            // Keep only bands present on EVERY scene (all-or-none
            // contract — mixed coverage would mis-align the time axis).
            let mut bands_map = serde_json::Map::new();
            for (band, slots) in &per_band {
                if slots.iter().all(|o| o.is_some()) {
                    let paths: Vec<PathBuf> =
                        slots.iter().flat_map(|o| o.clone()).collect();
                    bands_map.insert(band.clone(), super::paths_to_value(&paths));
                } else {
                    tracing::warn!(
                        band = %band,
                        "dropping band — not present on every scene"
                    );
                }
            }
            if bands_map.is_empty() {
                return Err(ExecError::Backend(format!(
                    "load_collection: none of the requested bands {requested_bands:?} \
                     are present on every scene"
                )));
            }

            let mut cube = serde_json::Map::new();
            cube.insert("collection".into(), Value::String(id_str.clone()));
            cube.insert(
                "bbox".into(),
                Value::Array(
                    bbox.iter()
                        .map(|x| {
                            serde_json::Number::from_f64(*x)
                                .map(Value::Number)
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                ),
            );
            cube.insert("scene_count".into(), Value::from(scenes.len()));
            cube.insert("bands".into(), Value::Object(bands_map));
            return Ok(json!({ "__cube": Value::Object(cube) }));
        }

        // Lightweight fallback — no search wired (e.g. arithmetic-only graphs).
        Ok(serde_json::json!({ "type": "DataCube", "collection": id_str }))
    }
}

/// openEO 1.3.0 `spatial_extent` is a BoundingBox object:
/// `{"west":…, "south":…, "east":…, "north":…, "crs":…?}`.
///
/// `crs` is optional and defaults to EPSG:4326 (WGS84 lon/lat) per spec.
/// Accepted forms:
/// - integer EPSG code  → `4326`             → `"EPSG:4326"`
/// - bare numeric string → `"4326"`           → `"EPSG:4326"`
/// - qualified string    → `"EPSG:32633"`     → passed through
/// - PROJ string         → `"+proj=utm …"`    → passed through (contains `:`)
/// - absent / null       →                    → `"EPSG:4326"` (default)
pub(super) fn parse_bbox(v: &Value) -> Result<([f64; 4], String), ExecError> {
    let west = v.get("west").and_then(|x| x.as_f64());
    let south = v.get("south").and_then(|x| x.as_f64());
    let east = v.get("east").and_then(|x| x.as_f64());
    let north = v.get("north").and_then(|x| x.as_f64());
    let bbox = match (west, south, east, north) {
        (Some(w), Some(s), Some(e), Some(n)) => [w, s, e, n],
        _ => return Err(ExecError::InvalidGraph(
            "spatial_extent must have numeric west/south/east/north".into(),
        )),
    };
    // Reason: L3 — reject inverted/empty bboxes (west>=east or south>=north). Without
    // this, gdal_translate silently produces a 1×1 output (CLAUDE.md §7). Antimeridian
    // wrap (west>east when crossing 180°) is not currently supported anywhere in the
    // codebase, so unconditional rejection is the safe path.
    if !(bbox[0] < bbox[2]) {
        return Err(ExecError::InvalidGraph(format!(
            "spatial_extent: west ({}) must be < east ({})", bbox[0], bbox[2]
        )));
    }
    if !(bbox[1] < bbox[3]) {
        return Err(ExecError::InvalidGraph(format!(
            "spatial_extent: south ({}) must be < north ({})", bbox[1], bbox[3]
        )));
    }
    let crs = parse_bbox_crs(v.get("crs"))?;
    Ok((bbox, crs))
}

/// Normalise the `spatial_extent.crs` field per openEO 1.3.0.
pub(super) fn parse_bbox_crs(v: Option<&Value>) -> Result<String, ExecError> {
    match v {
        None | Some(Value::Null) => Ok("EPSG:4326".to_string()),
        Some(Value::Number(n)) => {
            // openEO accepts integer EPSG codes.
            if let Some(u) = n.as_u64() {
                Ok(format!("EPSG:{u}"))
            } else if let Some(i) = n.as_i64() {
                if i < 0 {
                    Err(ExecError::InvalidGraph(format!(
                        "spatial_extent.crs invalid: negative EPSG code {i}"
                    )))
                } else {
                    Ok(format!("EPSG:{i}"))
                }
            } else {
                Err(ExecError::InvalidGraph(format!(
                    "spatial_extent.crs invalid: non-integer numeric {n}"
                )))
            }
        }
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(ExecError::InvalidGraph(
                    "spatial_extent.crs invalid: empty string".into(),
                ));
            }
            if s.contains(':') {
                // Already qualified (`EPSG:4326`) or a PROJ string (`+proj=...`).
                Ok(s.to_string())
            } else if s.chars().all(|c| c.is_ascii_digit()) {
                // Bare numeric string — qualify it.
                Ok(format!("EPSG:{s}"))
            } else {
                Err(ExecError::InvalidGraph(format!(
                    "spatial_extent.crs invalid: unqualified non-numeric string `{s}`"
                )))
            }
        }
        Some(other) => Err(ExecError::InvalidGraph(format!(
            "spatial_extent.crs invalid: expected integer or string, got {other}"
        ))),
    }
}

/// openEO `temporal_extent` is `["start", "end"]`. We collapse to the
/// STAC search `start/end` string `"start/end"`.
///
/// Element84 (and most STAC servers) require **full RFC 3339 timestamps**
/// (`YYYY-MM-DDTHH:MM:SSZ`); a bare date like `2024-06-01` silently
/// matches zero items. We normalise date-only inputs by appending
/// `T00:00:00Z` for the start and `T23:59:59Z` for the end.
pub(super) fn parse_temporal(v: &Value) -> Result<String, ExecError> {
    let arr = v
        .as_array()
        .ok_or_else(|| ExecError::InvalidGraph("temporal_extent must be an array".into()))?;
    if arr.len() != 2 {
        return Err(ExecError::InvalidGraph("temporal_extent must have two entries".into()));
    }
    let start = normalise_iso(arr[0].as_str().unwrap_or(".."), true);
    let end = normalise_iso(arr[1].as_str().unwrap_or(".."), false);
    Ok(format!("{start}/{end}"))
}

/// Extract a `max_cloud_cover` threshold from openEO `load_collection.properties`.
///
/// Expected canonical shape (what `openeo-python-client.max_cloud_cover()` emits):
///
/// ```json
/// {
///   "eo:cloud_cover": {
///     "process_graph": {
///       "cc": {
///         "process_id": "lt" | "lte",
///         "arguments": { "x": {"from_parameter": "value"}, "y": 30 },
///         "result": true
///       }
///     }
///   }
/// }
/// ```
///
/// Returns `Some(threshold)` for `lt`/`lte` over `value`, else `None`.
/// Anything more exotic falls through (server-side filter on `eo:cloud_cover`
/// is best-effort; the client can still get all scenes if the predicate
/// shape isn't one we recognise).
pub fn parse_eo_cloud_cover_lt(props: &Value) -> Option<f64> {
    let pg = props
        .get("eo:cloud_cover")?
        .get("process_graph")?
        .as_object()?;
    for node in pg.values() {
        let pid = node.get("process_id")?.as_str()?;
        if pid != "lt" && pid != "lte" {
            continue;
        }
        let args = node.get("arguments")?.as_object()?;
        // x must be `from_parameter: "value"`, y must be a number.
        let x_ok = args
            .get("x")
            .and_then(|x| x.get("from_parameter"))
            .and_then(|s| s.as_str())
            .map(|s| s == "value")
            .unwrap_or(false);
        let y = args.get("y").and_then(|y| y.as_f64())?;
        if x_ok {
            return Some(y);
        }
    }
    None
}

/// Promote a bare `YYYY-MM-DD` to a full RFC 3339 timestamp. Leaves
/// already-timestamped strings (anything containing `T` or `:` or `..`)
/// alone.
pub(super) fn normalise_iso(s: &str, is_start: bool) -> String {
    if s == ".." || s.contains('T') || s.contains(':') {
        return s.to_string();
    }
    // Heuristic: looks like a bare date (10 chars, two '-'s).
    let has_two_dashes = s.matches('-').count() == 2;
    if s.len() == 10 && has_two_dashes {
        return if is_start {
            format!("{s}T00:00:00Z")
        } else {
            format!("{s}T23:59:59Z")
        };
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_bbox_round_trips_openeo_spatial_extent() {
        let v = json!({"west": 1.0, "south": 2.0, "east": 3.0, "north": 4.0});
        let (bbox, crs) = parse_bbox(&v).unwrap();
        assert_eq!(bbox, [1.0, 2.0, 3.0, 4.0]);
        // No `crs` field → openEO 1.3.0 default of EPSG:4326.
        assert_eq!(crs, "EPSG:4326");
    }

    #[test]
    fn parse_bbox_missing_field_errors() {
        let v = json!({"west": 1.0, "south": 2.0});
        assert!(parse_bbox(&v).is_err());
    }

    #[test]
    fn parse_bbox_rejects_west_gt_east() {
        let v = json!({"west": 10.0, "south": 45.0, "east": 5.0, "north": 50.0});
        let err = parse_bbox(&v).unwrap_err();
        assert!(matches!(err, ExecError::InvalidGraph(m) if m.contains("west") && m.contains("east")));
    }

    #[test]
    fn parse_bbox_rejects_west_eq_east() {
        let v = json!({"west": 10.0, "south": 45.0, "east": 10.0, "north": 50.0});
        assert!(parse_bbox(&v).is_err());
    }

    #[test]
    fn parse_bbox_rejects_south_ge_north() {
        let v = json!({"west": 10.0, "south": 50.0, "east": 15.0, "north": 45.0});
        let err = parse_bbox(&v).unwrap_err();
        assert!(matches!(err, ExecError::InvalidGraph(m) if m.contains("south") && m.contains("north")));
    }

    // ---------- A12 — spatial_extent.crs per openEO 1.3.0 ----------

    #[test]
    fn parse_bbox_default_crs_is_wgs84() {
        // No crs field → EPSG:4326 (openEO 1.3.0 BoundingBox default).
        let v = json!({"west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0});
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:4326");
        // Explicit null also yields the default.
        let v = json!({"west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0, "crs": null});
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:4326");
    }

    #[test]
    fn parse_bbox_integer_crs_is_qualified_to_epsg_prefix() {
        // Integer EPSG code → must be qualified with "EPSG:" before
        // reaching GDAL.
        let v = json!({
            "west": 100000.0, "south": 5000000.0,
            "east":  110000.0, "north": 5010000.0,
            "crs": 32633
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:32633");
    }

    #[test]
    fn parse_bbox_string_crs_passthrough_when_qualified() {
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "EPSG:3857"
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:3857");
    }

    #[test]
    fn parse_bbox_bare_numeric_string_crs_is_qualified() {
        // The spec also accepts a bare numeric string like "32633"
        // (no EPSG: prefix). We normalise it for GDAL.
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "32633"
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert_eq!(crs, "EPSG:32633");
    }

    #[test]
    fn parse_bbox_proj_string_passthrough() {
        // PROJ strings contain `:` (via `+units=m` patterns we may see)
        // — keep the canonical-form check: anything containing `:` is
        // already a complete CRS spec.
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "+proj=utm +zone=33 +ellps=WGS84:meters"
        });
        let (_, crs) = parse_bbox(&v).unwrap();
        assert!(crs.starts_with("+proj=utm"));
    }

    #[test]
    fn parse_bbox_rejects_garbage_crs() {
        // Arrays, objects, and unqualified non-numeric strings are
        // rejected with InvalidGraph (spec-incompatible input).
        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": [1, 2, 3]
        });
        let r = parse_bbox(&v);
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))));

        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": { "wrong": "shape" }
        });
        assert!(matches!(parse_bbox(&v), Err(ExecError::InvalidGraph(_))));

        let v = json!({
            "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0,
            "crs": "not_a_crs"
        });
        assert!(matches!(parse_bbox(&v), Err(ExecError::InvalidGraph(_))));
    }

    #[test]
    fn parse_temporal_bare_dates_get_normalised_to_rfc3339() {
        // Element84 returns 0 features for `YYYY-MM-DD` — we widen to
        // full timestamps so the day boundaries match what users expect.
        let v = json!(["2024-01-01", "2024-12-31"]);
        assert_eq!(
            parse_temporal(&v).unwrap(),
            "2024-01-01T00:00:00Z/2024-12-31T23:59:59Z"
        );
    }

    #[test]
    fn parse_temporal_preserves_full_timestamps() {
        let v = json!(["2024-01-01T06:00:00Z", "2024-12-31T18:00:00Z"]);
        assert_eq!(
            parse_temporal(&v).unwrap(),
            "2024-01-01T06:00:00Z/2024-12-31T18:00:00Z"
        );
    }

    #[test]
    fn parse_temporal_wrong_arity_errors() {
        let v = json!(["only-one"]);
        assert!(parse_temporal(&v).is_err());
    }

    #[test]
    fn parse_eo_cloud_cover_lt_canonical_python_client_shape() {
        // openeo-python-client `dc.max_cloud_cover(30)` emits this.
        let props = json!({
            "eo:cloud_cover": {
                "process_graph": {
                    "cc": {
                        "process_id": "lt",
                        "arguments": { "x": {"from_parameter": "value"}, "y": 30 },
                        "result": true
                    }
                }
            }
        });
        assert_eq!(parse_eo_cloud_cover_lt(&props), Some(30.0));
    }

    #[test]
    fn parse_eo_cloud_cover_lte_is_also_recognised() {
        let props = json!({
            "eo:cloud_cover": {
                "process_graph": {
                    "cc": {
                        "process_id": "lte",
                        "arguments": { "x": {"from_parameter": "value"}, "y": 50.5 },
                        "result": true
                    }
                }
            }
        });
        assert_eq!(parse_eo_cloud_cover_lt(&props), Some(50.5));
    }

    #[test]
    fn parse_eo_cloud_cover_returns_none_when_missing() {
        let props = json!({ "other_band": { "process_graph": {} } });
        assert_eq!(parse_eo_cloud_cover_lt(&props), None);
    }

    #[test]
    fn parse_eo_cloud_cover_returns_none_for_unsupported_predicate() {
        // gt/gte aren't useful for "max cloud cover" — fall through.
        let props = json!({
            "eo:cloud_cover": {
                "process_graph": {
                    "cc": {
                        "process_id": "gt",
                        "arguments": { "x": {"from_parameter": "value"}, "y": 30 },
                        "result": true
                    }
                }
            }
        });
        assert_eq!(parse_eo_cloud_cover_lt(&props), None);
    }
}
