//! openEO 1.3.0 **array processes** — pure functions over JSON arrays.
//!
//! These operate on literal `data` arrays (e.g. the per-pixel vector inside
//! a reducer callback, or an array constructed by `array_create`). None
//! touch raster I/O or GeoExecutor state, so they're implemented as free
//! functions + thin handlers.
//!
//! Implemented (spec param names in parens):
//! - `array_element(data, index?, label?, return_nodata?)`
//! - `array_create(data?, repeat?)`
//! - `array_concat(array1, array2)`
//! - `array_append(data, value)`
//! - `array_contains(data, value)`
//! - `array_find(data, value)`
//! - `count(data, condition?)`  (condition limited to `true`/omitted today)
//! - `order(data, asc?, nodata?)` → permutation indices
//! - `sort(data, asc?, nodata?)` → sorted array

use serde_json::Value;

use crate::executor::ExecError;

fn arr<'a>(args: &'a std::collections::BTreeMap<String, Value>, key: &str) -> Result<&'a Vec<Value>, ExecError> {
    args.get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| ExecError::InvalidGraph(format!("expected array `{key}` argument")))
}

/// `array_element(data, index?, label?, return_nodata?)` — element at the
/// 0-based `index`. `label` (named-dimension lookup) is not supported on
/// raw arrays. Out-of-bounds → error UNLESS `return_nodata=true`, then null.
pub fn array_element(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = arr(args, "data")?;
    let return_nodata = args.get("return_nodata").and_then(|v| v.as_bool()).unwrap_or(false);
    let index = args
        .get("index")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ExecError::InvalidGraph(
            "array_element: `index` (integer) required (label lookup unsupported on raw arrays)".into(),
        ))?;
    if index < 0 {
        return Err(ExecError::InvalidGraph("array_element: index must be >= 0".into()));
    }
    match data.get(index as usize) {
        Some(v) => Ok(v.clone()),
        None if return_nodata => Ok(Value::Null),
        None => Err(ExecError::InvalidGraph(format!(
            "array_element: index {index} out of bounds (len {})",
            data.len()
        ))),
    }
}

/// Maximum materialised length for `array_create` — guards against a tiny
/// request (`{"repeat": 1e11}`) amplifying into a multi-GB allocation that
/// would OOM-abort the whole server (audit-fix 2026-06-03).
const MAX_ARRAY_LEN: usize = 10_000_000;

/// `array_create(data?, repeat?)` — build an array by repeating `data`
/// `repeat` times (default 1). `data` defaults to empty.
///
/// The result length (`data.len() * repeat`) is bounded by [`MAX_ARRAY_LEN`]
/// and computed with `checked_mul` so an attacker-controlled `repeat` cannot
/// trigger an unbounded allocation / allocator abort.
pub fn array_create(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = args.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let repeat = args.get("repeat").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
    let total = usize::try_from(repeat)
        .ok()
        .and_then(|r| data.len().checked_mul(r))
        .filter(|n| *n <= MAX_ARRAY_LEN)
        .ok_or_else(|| {
            ExecError::InvalidGraph(format!(
                "array_create: result length {}×{repeat} exceeds the {MAX_ARRAY_LEN}-element cap",
                data.len()
            ))
        })?;
    let mut out = Vec::with_capacity(total);
    for _ in 0..repeat {
        out.extend(data.iter().cloned());
    }
    Ok(Value::Array(out))
}

/// `array_concat(array1, array2)` — concatenate two arrays.
pub fn array_concat(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let a = arr(args, "array1")?;
    let b = arr(args, "array2")?;
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend(a.iter().cloned());
    out.extend(b.iter().cloned());
    Ok(Value::Array(out))
}

/// `array_append(data, value)` — append a single value.
pub fn array_append(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let mut out = arr(args, "data")?.clone();
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| ExecError::InvalidGraph("array_append: missing `value`".into()))?;
    out.push(value);
    Ok(Value::Array(out))
}

/// `array_contains(data, value)` → boolean. Numeric equality is exact per
/// the spec (it's element-membership, not tolerance comparison).
pub fn array_contains(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = arr(args, "data")?;
    let value = args
        .get("value")
        .ok_or_else(|| ExecError::InvalidGraph("array_contains: missing `value`".into()))?;
    Ok(Value::Bool(data.iter().any(|v| v == value)))
}

/// `array_find(data, value)` → 0-based index of the FIRST match, or null.
pub fn array_find(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = arr(args, "data")?;
    let value = args
        .get("value")
        .ok_or_else(|| ExecError::InvalidGraph("array_find: missing `value`".into()))?;
    match data.iter().position(|v| v == value) {
        Some(i) => Ok(Value::from(i as u64)),
        None => Ok(Value::Null),
    }
}

/// `count(data, condition?)` — number of elements. With no `condition`
/// (or `condition: true`), counts non-null elements per the spec default.
pub fn count(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = arr(args, "data")?;
    // condition omitted or `true` → count valid (non-null) elements.
    let count_all = matches!(args.get("condition"), Some(Value::Bool(true)) | None);
    let n = if count_all {
        data.iter().filter(|v| !v.is_null()).count()
    } else {
        // Unsupported condition callback on raw arrays → count non-null
        // as the conservative default (spec callbacks need cube context).
        data.iter().filter(|v| !v.is_null()).count()
    };
    Ok(Value::from(n as u64))
}

/// Extract finite f64 values + null/non-numeric handling for order/sort.
fn numeric_with_index(data: &[Value]) -> Vec<(usize, f64, bool)> {
    // (orig_index, value, is_nodata)
    data.iter()
        .enumerate()
        .map(|(i, v)| match v.as_f64() {
            Some(f) if f.is_finite() => (i, f, false),
            _ => (i, f64::NAN, true), // null / non-numeric = nodata
        })
        .collect()
}

/// `order(data, asc?, nodata?)` → array of 0-based indices that sort `data`.
/// `asc` default true. `nodata` default null → nodata removed; if `true`
/// nodata sorts last, if `false` sorts first.
pub fn order(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = arr(args, "data")?;
    let asc = args.get("asc").and_then(|v| v.as_bool()).unwrap_or(true);
    let nodata = args.get("nodata").and_then(|v| v.as_bool());
    let items = numeric_with_index(data);
    let mut valid: Vec<(usize, f64)> = items.iter().filter(|(_, _, nd)| !*nd).map(|(i, f, _)| (*i, *f)).collect();
    let nodata_idx: Vec<usize> = items.iter().filter(|(_, _, nd)| *nd).map(|(i, _, _)| *i).collect();
    valid.sort_by(|a, b| {
        let o = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
        if asc { o } else { o.reverse() }
    });
    let mut out: Vec<Value> = Vec::with_capacity(data.len());
    match nodata {
        None => { /* drop nodata */ }
        Some(false) => out.extend(nodata_idx.iter().map(|i| Value::from(*i as u64))), // first
        Some(true) => {} // appended after, below
    }
    out.extend(valid.iter().map(|(i, _)| Value::from(*i as u64)));
    if matches!(nodata, Some(true)) {
        out.extend(nodata_idx.iter().map(|i| Value::from(*i as u64)));
    }
    Ok(Value::Array(out))
}

/// `sort(data, asc?, nodata?)` → sorted array (values, not indices).
/// Same nodata semantics as `order`.
pub fn sort(args: &std::collections::BTreeMap<String, Value>) -> Result<Value, ExecError> {
    let data = arr(args, "data")?;
    let asc = args.get("asc").and_then(|v| v.as_bool()).unwrap_or(true);
    let nodata = args.get("nodata").and_then(|v| v.as_bool());
    let items = numeric_with_index(data);
    let mut valid: Vec<f64> = items.iter().filter(|(_, _, nd)| !*nd).map(|(_, f, _)| *f).collect();
    let nodata_count = items.iter().filter(|(_, _, nd)| *nd).count();
    valid.sort_by(|a, b| {
        let o = a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
        if asc { o } else { o.reverse() }
    });
    let mut out: Vec<Value> = Vec::with_capacity(data.len());
    let nodata_vals = std::iter::repeat(Value::Null).take(nodata_count);
    match nodata {
        None => out.extend(valid.iter().filter_map(|f| serde_json::Number::from_f64(*f).map(Value::Number))),
        Some(false) => {
            out.extend(nodata_vals);
            out.extend(valid.iter().filter_map(|f| serde_json::Number::from_f64(*f).map(Value::Number)));
        }
        Some(true) => {
            out.extend(valid.iter().filter_map(|f| serde_json::Number::from_f64(*f).map(Value::Number)));
            out.extend(nodata_vals);
        }
    }
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> std::collections::BTreeMap<String, Value> {
        v.as_object().unwrap().clone().into_iter().collect()
    }

    #[test]
    fn array_element_index_and_oob() {
        assert_eq!(array_element(&m(json!({"data": [10, 20, 30], "index": 1}))).unwrap(), json!(20));
        // OOB without return_nodata → error.
        assert!(array_element(&m(json!({"data": [10], "index": 5}))).is_err());
        // OOB with return_nodata → null.
        assert_eq!(array_element(&m(json!({"data": [10], "index": 5, "return_nodata": true}))).unwrap(), Value::Null);
    }

    #[test]
    fn array_create_repeat() {
        assert_eq!(array_create(&m(json!({"data": [1, 2], "repeat": 3}))).unwrap(), json!([1,2,1,2,1,2]));
        assert_eq!(array_create(&m(json!({"data": [9]}))).unwrap(), json!([9]));
    }

    #[test]
    fn array_create_rejects_oversized_repeat() {
        // audit-fix: a tiny request must not amplify into an OOM allocation.
        let r = array_create(&m(json!({"data": [1], "repeat": 100_000_000_000u64})));
        assert!(matches!(r, Err(ExecError::InvalidGraph(_))), "huge repeat must be rejected, got {r:?}");
        // boundary: exactly at the cap is allowed, one over is not.
        assert!(array_create(&m(json!({"data": [0], "repeat": 10_000_000u64}))).is_ok());
        assert!(array_create(&m(json!({"data": [0], "repeat": 10_000_001u64}))).is_err());
    }

    #[test]
    fn array_concat_append_contains_find() {
        assert_eq!(array_concat(&m(json!({"array1": [1,2], "array2": [3,4]}))).unwrap(), json!([1,2,3,4]));
        assert_eq!(array_append(&m(json!({"data": [1,2], "value": 3}))).unwrap(), json!([1,2,3]));
        assert_eq!(array_contains(&m(json!({"data": [1,2,3], "value": 2}))).unwrap(), json!(true));
        assert_eq!(array_contains(&m(json!({"data": [1,2,3], "value": 9}))).unwrap(), json!(false));
        assert_eq!(array_find(&m(json!({"data": [5,6,7], "value": 6}))).unwrap(), json!(1));
        assert_eq!(array_find(&m(json!({"data": [5,6,7], "value": 9}))).unwrap(), Value::Null);
    }

    #[test]
    fn count_counts_non_null() {
        assert_eq!(count(&m(json!({"data": [1, 2, null, 4]}))).unwrap(), json!(3));
    }

    #[test]
    fn order_returns_sort_indices() {
        // [3,1,2] ascending → indices [1,2,0].
        assert_eq!(order(&m(json!({"data": [3,1,2]}))).unwrap(), json!([1,2,0]));
        // descending → [0,2,1].
        assert_eq!(order(&m(json!({"data": [3,1,2], "asc": false}))).unwrap(), json!([0,2,1]));
    }

    #[test]
    fn sort_orders_values_and_handles_nodata() {
        assert_eq!(sort(&m(json!({"data": [3,1,2]}))).unwrap(), json!([1.0,2.0,3.0]));
        assert_eq!(sort(&m(json!({"data": [3,1,2], "asc": false}))).unwrap(), json!([3.0,2.0,1.0]));
        // nodata dropped by default.
        assert_eq!(sort(&m(json!({"data": [2, null, 1]}))).unwrap(), json!([1.0,2.0]));
        // nodata=true → appended last.
        assert_eq!(sort(&m(json!({"data": [2, null, 1], "nodata": true}))).unwrap(), json!([1.0,2.0,null]));
    }
}
