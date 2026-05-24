//! **Tier 3.6 — minimal pure-Rust ML classifier for satellite-imagery
//! workflows**. Currently implements binary logistic regression with batch
//! gradient descent.
//!
//! **Why not LightGBM?** `lightgbm-sys 0.3` requires CMake ≥ 4 compatibility
//! that hasn't been patched into LightGBM's CMakeLists.txt yet, plus
//! OpenMP detection on Apple Silicon needs out-of-tree env tweaks. To keep
//! orbit-geo building cleanly we ship a pure-Rust classifier here; the API
//! shape (`fit_classifier` / `predict_classifier`) is preserved so callers
//! can swap to LightGBM later without code changes.

#![cfg(feature = "use_ml")]

use crate::error::{Error, Result};
use ndarray::{Array1, ArrayView1, ArrayView2};

#[cfg(test)]
use ndarray::Array2;

/// Fitted binary logistic regression model.
#[derive(Debug, Clone)]
pub struct ClassifierModel {
    /// Weight vector (one entry per feature).
    pub weights: Array1<f64>,
    /// Intercept term.
    pub bias: f64,
}

/// Fit a binary classifier on `(X, y)`.
///
/// `X`: `(n_samples, n_features)` feature matrix
/// `y`: `(n_samples,)` labels in `{0, 1}`
///
/// Trains via batch gradient descent for `iterations` epochs at `lr`
/// learning rate.
pub fn fit_classifier(
    x: ArrayView2<f64>,
    y: ArrayView1<u8>,
    iterations: usize,
    lr: f64,
) -> Result<ClassifierModel> {
    let (n, d) = x.dim();
    if y.len() != n {
        return Err(Error::Other(format!(
            "fit_classifier: X has {n} rows but y has {} entries",
            y.len()
        )));
    }
    let mut w = Array1::<f64>::zeros(d);
    let mut b = 0.0_f64;
    let y_f = y.mapv(|v| v as f64);
    for _ in 0..iterations {
        // p = sigmoid(X @ w + b)
        let logits = x.dot(&w) + b;
        let p = logits.mapv(|z| 1.0 / (1.0 + (-z).exp()));
        let err = &p - &y_f;
        // grad_w = X^T @ err / n; grad_b = mean(err)
        let grad_w = x.t().dot(&err) / n as f64;
        let grad_b = err.mean().unwrap_or(0.0);
        w = &w - &(grad_w * lr);
        b -= grad_b * lr;
    }
    Ok(ClassifierModel { weights: w, bias: b })
}

/// Predict class labels for `X` using a fitted model. Returns `Array1<u8>`
/// in `{0, 1}` (threshold at p ≥ 0.5).
pub fn predict_classifier(model: &ClassifierModel, x: ArrayView2<f64>) -> Array1<u8> {
    let logits = x.dot(&model.weights) + model.bias;
    logits.mapv(|z| if 1.0 / (1.0 + (-z).exp()) >= 0.5 { 1 } else { 0 })
}

/// Compute classification accuracy of `predicted` against `truth`.
pub fn accuracy(predicted: ArrayView1<u8>, truth: ArrayView1<u8>) -> f64 {
    let n = predicted.len();
    if n == 0 {
        return 0.0;
    }
    let correct = predicted
        .iter()
        .zip(truth.iter())
        .filter(|(a, b)| a == b)
        .count();
    correct as f64 / n as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

    /// **RED T3.6/A1+A2**: fit + predict round-trips on a linearly-separable
    /// dataset with > 95% training accuracy.
    #[test]
    fn fit_and_predict_linearly_separable_dataset() {
        // y = 1 if x[0] > x[1], else 0
        let x: Array2<f64> = array![
            [1.0, 0.0], [2.0, 0.0], [3.0, 0.0], [4.0, 1.0],
            [0.0, 1.0], [0.0, 2.0], [0.0, 3.0], [1.0, 4.0],
            [5.0, 1.0], [1.0, 5.0],
        ];
        let y: Array1<u8> = array![1, 1, 1, 1, 0, 0, 0, 0, 1, 0];
        let model = fit_classifier(x.view(), y.view(), 500, 0.5).expect("fit ok");
        let pred = predict_classifier(&model, x.view());
        let acc = accuracy(pred.view(), y.view());
        assert!(acc >= 0.9, "expected ≥90% training accuracy, got {acc}");
    }

    /// **RED T3.6/A3**: mismatched shapes return Err.
    #[test]
    fn fit_classifier_errors_on_shape_mismatch() {
        let x: Array2<f64> = array![[1.0, 0.0], [0.0, 1.0]];
        let y: Array1<u8> = array![1, 0, 1]; // length 3 vs X has 2 rows
        let r = fit_classifier(x.view(), y.view(), 100, 0.1);
        assert!(r.is_err());
    }
}
