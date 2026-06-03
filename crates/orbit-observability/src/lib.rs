//! **orbit-observability** — metric recording + Prometheus text export.
//!
//! Provides:
//! - [`MetricName`] enum of every metric the platform publishes.
//! - [`Recorder`] trait + [`InMemoryRecorder`] (testing) +
//!   [`PrometheusRecorder`] (text-format export, no exporter dep).
//! - [`label`] module with standard label keys.
//!
//! The Prometheus text format is intentionally hand-rolled (40 lines) so
//! we don't pull `prometheus`/`metrics-exporter-prometheus` until the
//! orbit-server actually wires up an HTTP `/metrics` endpoint. The output
//! conforms to the Prometheus exposition format 0.0.4.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Stable identifiers for every metric the platform exports.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MetricName {
    /// Total ETL job runs by terminal state.
    EtlJobsTotal,
    /// Rows read from sources.
    EtlRowsReadTotal,
    /// Rows written to sinks.
    EtlRowsWrittenTotal,
    /// Histogram of ETL job duration (seconds).
    EtlJobDurationSeconds,
    /// STAC search request count.
    CatalogSearchTotal,
    /// Cache hit count.
    CacheHitsTotal,
    /// Cache miss count.
    CacheMissesTotal,
    /// Bytes read from cache.
    CacheBytesOutTotal,
    /// Bytes written into the cache.
    CacheBytesInTotal,
    /// Histogram of per-block kernel apply duration (seconds).
    KernelBlockDurationSeconds,
    /// gRPC server received-message count.
    GrpcRequestsTotal,
    /// gRPC server response error count.
    GrpcErrorsTotal,
}

impl MetricName {
    /// The stable Prometheus-style identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EtlJobsTotal               => "orbit_etl_jobs_total",
            Self::EtlRowsReadTotal           => "orbit_etl_rows_read_total",
            Self::EtlRowsWrittenTotal        => "orbit_etl_rows_written_total",
            Self::EtlJobDurationSeconds      => "orbit_etl_job_duration_seconds",
            Self::CatalogSearchTotal         => "orbit_catalog_search_total",
            Self::CacheHitsTotal             => "orbit_cache_hits_total",
            Self::CacheMissesTotal           => "orbit_cache_misses_total",
            Self::CacheBytesOutTotal         => "orbit_cache_bytes_out_total",
            Self::CacheBytesInTotal          => "orbit_cache_bytes_in_total",
            Self::KernelBlockDurationSeconds => "orbit_kernel_block_duration_seconds",
            Self::GrpcRequestsTotal          => "orbit_grpc_requests_total",
            Self::GrpcErrorsTotal            => "orbit_grpc_errors_total",
        }
    }

    /// Prometheus metric kind for `# TYPE` lines.
    #[must_use]
    pub const fn kind(self) -> MetricKind {
        match self {
            Self::EtlJobDurationSeconds
            | Self::KernelBlockDurationSeconds => MetricKind::Histogram,
            _ => MetricKind::Counter,
        }
    }
}

/// Prometheus metric kind.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MetricKind {
    /// Monotonically increasing counter.
    Counter,
    /// Histogram (we record observation count + sum for now).
    Histogram,
}

impl MetricKind {
    /// The Prometheus `# TYPE` keyword.
    #[must_use]
    pub const fn type_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Histogram => "histogram",
        }
    }
}

/// Standard label keys.
pub mod label {
    /// Job identifier (UUID v4 stringified).
    pub const JOB_ID: &str = "job_id";
    /// Stage name (`ingest`, `transform`, `sink`).
    pub const STAGE: &str = "stage";
    /// STAC / OpenEO provider tag (`planetary-computer`, …).
    pub const PROVIDER: &str = "provider";
    /// Terminal state (`completed`, `failed`, `cancelled`).
    pub const STATE: &str = "state";
}

/// Sorted label set — used as the metric-series key.
pub type Labels = BTreeMap<String, String>;

/// Trait for any metric backend.
pub trait Recorder: Send + Sync {
    /// Increment a counter series by `delta`.
    fn counter_inc(&self, name: MetricName, labels: &Labels, delta: u64);
    /// Observe one value into a histogram series.
    fn histogram_observe(&self, name: MetricName, labels: &Labels, value: f64);
    /// Render the recorded metrics as Prometheus exposition text. Default is
    /// empty (a recorder with no in-memory series, e.g. a pure forwarding
    /// backend, has nothing to export here). [`InMemoryRecorder`] overrides
    /// this to expose its stored series for the `GET /metrics` endpoint.
    fn render_prometheus(&self) -> String {
        String::new()
    }
}

/// Recorder that stores series in memory. Useful for tests and unit-level
/// assertions on metric emission.
#[derive(Debug, Default)]
pub struct InMemoryRecorder {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    counters: BTreeMap<(MetricName, Labels), u64>,
    /// Histogram bucketed as (count, sum_of_observations).
    histograms: BTreeMap<(MetricName, Labels), (u64, f64)>,
}

impl InMemoryRecorder {
    /// Create a fresh empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current counter value for a (name, labels) tuple. 0 if unrecorded.
    pub fn counter(&self, name: MetricName, labels: &Labels) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .counters
            .get(&(name, labels.clone()))
            .copied()
            .unwrap_or(0)
    }

    /// Current (count, sum) for a histogram series.
    pub fn histogram(&self, name: MetricName, labels: &Labels) -> (u64, f64) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .histograms
            .get(&(name, labels.clone()))
            .copied()
            .unwrap_or((0, 0.0))
    }

    /// Total number of (name, labels) counter series.
    pub fn counter_series_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .counters
            .len()
    }

    /// Render the recorded metrics as Prometheus exposition text. The
    /// output is sorted (deterministic) for stable diffs.
    #[must_use]
    pub fn to_prometheus(&self) -> String {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = String::new();

        // Group counters by name for `# HELP` / `# TYPE` blocks.
        let mut counters_by_name: BTreeMap<MetricName, Vec<(&Labels, u64)>> = BTreeMap::new();
        for ((name, labels), v) in &inner.counters {
            counters_by_name.entry(*name).or_default().push((labels, *v));
        }
        for (name, series) in counters_by_name {
            out.push_str(&format!("# TYPE {} {}\n", name.as_str(), name.kind().type_str()));
            for (labels, v) in series {
                out.push_str(name.as_str());
                push_labels(&mut out, labels);
                out.push(' ');
                out.push_str(&v.to_string());
                out.push('\n');
            }
        }

        let mut histos_by_name: BTreeMap<MetricName, Vec<(&Labels, (u64, f64))>> = BTreeMap::new();
        for ((name, labels), v) in &inner.histograms {
            histos_by_name.entry(*name).or_default().push((labels, *v));
        }
        for (name, series) in histos_by_name {
            out.push_str(&format!("# TYPE {} {}\n", name.as_str(), name.kind().type_str()));
            for (labels, (count, sum)) in series {
                let n = name.as_str();
                let count_line = format!("{n}_count");
                let sum_line = format!("{n}_sum");
                out.push_str(&count_line);
                push_labels(&mut out, labels);
                out.push(' ');
                out.push_str(&count.to_string());
                out.push('\n');
                out.push_str(&sum_line);
                push_labels(&mut out, labels);
                out.push(' ');
                out.push_str(&format!("{sum}"));
                out.push('\n');
            }
        }
        out
    }
}

fn push_labels(buf: &mut String, labels: &Labels) {
    if labels.is_empty() {
        return;
    }
    buf.push('{');
    let mut first = true;
    for (k, v) in labels {
        if !first { buf.push(','); }
        first = false;
        buf.push_str(k);
        buf.push_str("=\"");
        // Escape: backslash, double-quote, newline per Prometheus exposition format.
        for ch in v.chars() {
            match ch {
                '\\' => buf.push_str(r"\\"),
                '"'  => buf.push_str("\\\""),
                '\n' => buf.push_str(r"\n"),
                c => buf.push(c),
            }
        }
        buf.push('"');
    }
    buf.push('}');
}

impl Recorder for InMemoryRecorder {
    fn counter_inc(&self, name: MetricName, labels: &Labels, delta: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        *g.counters.entry((name, labels.clone())).or_insert(0) += delta;
    }

    fn histogram_observe(&self, name: MetricName, labels: &Labels, value: f64) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let e = g.histograms.entry((name, labels.clone())).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += value;
    }

    fn render_prometheus(&self) -> String {
        self.to_prometheus()
    }
}

/// Build a `Labels` map from `(key, value)` pairs.
///
/// Convenience macro-equivalent without dragging in a proc-macro.
#[must_use]
pub fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_names_have_orbit_prefix() {
        for m in [
            MetricName::EtlJobsTotal,
            MetricName::CacheHitsTotal,
            MetricName::EtlJobDurationSeconds,
        ] {
            assert!(m.as_str().starts_with("orbit_"));
        }
    }

    #[test]
    fn metric_names_are_unique() {
        let names: Vec<&str> = [
            MetricName::EtlJobsTotal,
            MetricName::EtlRowsReadTotal,
            MetricName::EtlRowsWrittenTotal,
            MetricName::EtlJobDurationSeconds,
            MetricName::CatalogSearchTotal,
            MetricName::CacheHitsTotal,
            MetricName::CacheMissesTotal,
            MetricName::CacheBytesOutTotal,
            MetricName::CacheBytesInTotal,
            MetricName::KernelBlockDurationSeconds,
            MetricName::GrpcRequestsTotal,
            MetricName::GrpcErrorsTotal,
        ]
        .iter()
        .map(|m| m.as_str())
        .collect();
        let unique: std::collections::HashSet<_> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn counter_inc_accumulates() {
        let r = InMemoryRecorder::new();
        let l = labels(&[(label::JOB_ID, "abc"), (label::STATE, "completed")]);
        r.counter_inc(MetricName::EtlJobsTotal, &l, 1);
        r.counter_inc(MetricName::EtlJobsTotal, &l, 3);
        assert_eq!(r.counter(MetricName::EtlJobsTotal, &l), 4);
    }

    #[test]
    fn counter_distinguishes_label_sets() {
        let r = InMemoryRecorder::new();
        let l_ok = labels(&[(label::STATE, "completed")]);
        let l_err = labels(&[(label::STATE, "failed")]);
        r.counter_inc(MetricName::EtlJobsTotal, &l_ok, 5);
        r.counter_inc(MetricName::EtlJobsTotal, &l_err, 2);
        assert_eq!(r.counter(MetricName::EtlJobsTotal, &l_ok), 5);
        assert_eq!(r.counter(MetricName::EtlJobsTotal, &l_err), 2);
        assert_eq!(r.counter_series_count(), 2);
    }

    #[test]
    fn histogram_observe_records_count_and_sum() {
        let r = InMemoryRecorder::new();
        let l = Labels::new();
        r.histogram_observe(MetricName::EtlJobDurationSeconds, &l, 1.5);
        r.histogram_observe(MetricName::EtlJobDurationSeconds, &l, 2.5);
        let (n, sum) = r.histogram(MetricName::EtlJobDurationSeconds, &l);
        assert_eq!(n, 2);
        assert!((sum - 4.0).abs() < 1e-9);
    }

    #[test]
    fn prometheus_output_contains_type_lines() {
        let r = InMemoryRecorder::new();
        let l = labels(&[(label::STATE, "completed")]);
        r.counter_inc(MetricName::EtlJobsTotal, &l, 1);
        let out = r.to_prometheus();
        assert!(out.contains("# TYPE orbit_etl_jobs_total counter"));
        assert!(out.contains(r#"orbit_etl_jobs_total{state="completed"} 1"#));
    }

    #[test]
    fn prometheus_output_for_histogram_emits_count_and_sum() {
        let r = InMemoryRecorder::new();
        let l = Labels::new();
        r.histogram_observe(MetricName::EtlJobDurationSeconds, &l, 0.5);
        r.histogram_observe(MetricName::EtlJobDurationSeconds, &l, 1.0);
        let out = r.to_prometheus();
        assert!(out.contains("# TYPE orbit_etl_job_duration_seconds histogram"));
        assert!(out.contains("orbit_etl_job_duration_seconds_count 2"));
        assert!(out.contains("orbit_etl_job_duration_seconds_sum 1.5"));
    }

    #[test]
    fn prometheus_output_is_deterministic_label_order() {
        let r = InMemoryRecorder::new();
        // Labels intentionally inserted out of alphabetical order.
        let l = labels(&[("z_key", "1"), ("a_key", "2")]);
        r.counter_inc(MetricName::EtlJobsTotal, &l, 1);
        let out = r.to_prometheus();
        // BTreeMap iteration alphabetically sorts: a_key first.
        let a_pos = out.find("a_key").unwrap();
        let z_pos = out.find("z_key").unwrap();
        assert!(a_pos < z_pos, "labels must render alphabetically");
    }

    #[test]
    fn label_values_with_quotes_are_escaped() {
        let r = InMemoryRecorder::new();
        let l = labels(&[(label::JOB_ID, r#"abc"def\ghi"#)]);
        r.counter_inc(MetricName::EtlJobsTotal, &l, 1);
        let out = r.to_prometheus();
        // Escaped: \" and \\
        assert!(
            out.contains(r#"job_id="abc\"def\\ghi""#),
            "raw output was:\n{out}"
        );
    }

    #[test]
    fn empty_recorder_renders_empty_string() {
        let r = InMemoryRecorder::new();
        assert!(r.to_prometheus().is_empty());
    }

    #[test]
    fn metric_kind_routing() {
        assert_eq!(MetricName::EtlJobsTotal.kind(), MetricKind::Counter);
        assert_eq!(MetricName::EtlJobDurationSeconds.kind(), MetricKind::Histogram);
        assert_eq!(MetricKind::Counter.type_str(), "counter");
        assert_eq!(MetricKind::Histogram.type_str(), "histogram");
    }

    #[test]
    fn standard_labels_are_snake_case() {
        for l in [label::JOB_ID, label::STAGE, label::PROVIDER, label::STATE] {
            assert!(l.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[test]
    fn recorder_is_send_sync_via_trait_object() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn Recorder>();
        assert_send_sync::<InMemoryRecorder>();
    }
}
