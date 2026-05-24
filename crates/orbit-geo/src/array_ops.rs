//! **Ndarray utilities** ported from the upstream raster crate's `array_ops` module.
//!
//! Trim helpers, argmax, nodata fill, and rect-view extraction. Used by
//! workers that need finer control than the apply* methods provide.

use ndarray::{Array2, Array3, ArrayView3, ArrayView4};

/// Return the index of the maximum element. Panics on empty slice.
pub fn argmax<T: PartialOrd>(xs: &[T]) -> usize {
    let mut best = 0;
    for i in 1..xs.len() {
        if xs[i] > xs[best] {
            best = i;
        }
    }
    best
}

/// Trim `overlap_size` pixels from each edge of an `Array3<T>` shaped `(layers, rows, cols)`.
pub fn trimm_array3<T>(array: &Array3<T>, overlap_size: usize) -> ArrayView3<'_, T> {
    use ndarray::s;
    let (_layers, rows, cols) = array.dim();
    let row_end = rows.saturating_sub(overlap_size);
    let col_end = cols.saturating_sub(overlap_size);
    array.slice(s![.., overlap_size..row_end, overlap_size..col_end])
}

/// Trim from each edge of an `Array4<T>` shaped `(times, layers, rows, cols)`.
pub fn trimm_array4<T>(array: &ndarray::Array4<T>, overlap_size: usize) -> ArrayView4<'_, T> {
    use ndarray::s;
    let (_t, _l, rows, cols) = array.dim();
    let row_end = rows.saturating_sub(overlap_size);
    let col_end = cols.saturating_sub(overlap_size);
    array.slice(s![.., .., overlap_size..row_end, overlap_size..col_end])
}

/// Fill NaN pixels in an `Array3<f32>` with `nodata` value.
pub fn fill_nodata_simple(array: &mut Array3<f32>, nodata: f32) {
    for v in array.iter_mut() {
        if v.is_nan() {
            *v = nodata;
        }
    }
}

/// Extract a rectangular subview of `Array2<T>` at `(row, col)` with `(h, w)` size.
pub fn rect_view<T>(array: &Array2<T>, row: usize, col: usize, h: usize, w: usize) -> ndarray::ArrayView2<'_, T> {
    use ndarray::s;
    array.slice(s![row..row + h, col..col + w])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::{array, Array2, Array3};

    #[test]
    fn argmax_finds_largest() {
        assert_eq!(argmax(&[1, 5, 3, 2]), 1);
        assert_eq!(argmax(&[10]), 0);
        assert_eq!(argmax(&[3, 3, 3, 3]), 0); // ties → first
    }

    #[test]
    fn trimm_array3_removes_border() {
        let a: Array3<i16> = Array3::from_elem((1, 4, 4), 9);
        let trimmed = trimm_array3(&a, 1);
        assert_eq!(trimmed.dim(), (1, 2, 2));
    }

    #[test]
    fn trimm_array4_removes_border() {
        let a: ndarray::Array4<i16> = ndarray::Array4::from_elem((2, 1, 4, 4), 9);
        let trimmed = trimm_array4(&a, 1);
        assert_eq!(trimmed.dim(), (2, 1, 2, 2));
    }

    #[test]
    fn fill_nodata_replaces_nans() {
        let mut a: Array3<f32> = array![[[1.0_f32, f32::NAN, 3.0]]];
        fill_nodata_simple(&mut a, -9999.0);
        assert_eq!(a[[0, 0, 1]], -9999.0);
        assert_eq!(a[[0, 0, 0]], 1.0);
    }

    #[test]
    fn rect_view_returns_subview() {
        let a: Array2<i16> = Array2::from_shape_fn((4, 4), |(r, c)| (r * 4 + c) as i16);
        let sub = rect_view(&a, 1, 1, 2, 2);
        assert_eq!(sub.dim(), (2, 2));
        assert_eq!(sub[[0, 0]], 5); // row=1, col=1 → 1*4+1=5
    }
}
