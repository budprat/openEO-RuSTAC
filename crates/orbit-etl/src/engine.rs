//! Pipeline execution engine.
//!
//! The engine:
//! 1. Validates the spec
//! 2. Reads the source file with Polars (lazy mode where possible)
//! 3. Optionally applies a Polars SQL transform
//! 4. Streams batches into SQLite, recording progress
//! 5. Persists job state to a `jobs` metadata table for resume / listing

use crate::spec::{validate_path_under_root, validate_sql_identifier, FileFormat, PipelineSpec};
use dashmap::DashMap;
use futures::stream::Stream;
use orbit_core::{Error, JobId, JobState, JobStatus, Result};
use polars::prelude::*;
use serde_json::Value as JsonValue;
use sqlx::{sqlite::SqlitePool, Executor, Row};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Observable event emitted while a pipeline runs.
#[derive(Debug, Clone)]
pub enum Event {
    Started { job_id: JobId },
    Progress { job_id: JobId, rows_read: u64, rows_written: u64 },
    Completed { job_id: JobId, rows_read: u64, rows_written: u64 },
    Failed { job_id: JobId, error: String },
    Cancelled { job_id: JobId, rows_read: u64, rows_written: u64 },
}

/// Default upper bound on the Polars SQL + collect step. Protects the engine
/// against unbounded-cost user queries (cartesian joins, regex bombs, etc.).
pub const DEFAULT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Engine — manages running jobs and shared SQLite pool.
#[derive(Clone)]
pub struct Engine {
    pool: SqlitePool,
    jobs: Arc<DashMap<JobId, JobStatus>>,
    /// If set, every source path must canonicalise under this directory.
    /// When `None`, any path the process can read is permitted (developer
    /// mode; do NOT use in production with an untrusted gRPC client).
    data_root: Option<Arc<std::path::PathBuf>>,
    /// Maximum wall-clock time for the Polars SQL + collect step.
    query_timeout: std::time::Duration,
    /// Per-job cancellation flags. Set to `true` by [`Engine::cancel_job`];
    /// the executor polls the flag between batches and aborts cleanly.
    cancels: Arc<DashMap<JobId, Arc<AtomicBool>>>,
}

impl Engine {
    /// Open or create the SQLite database and run migrations.
    ///
    /// `db_url` example: `sqlite://./data/orbit.db?mode=rwc`
    pub async fn open(db_url: &str) -> Result<Self> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteJournalMode, SqliteSynchronous};
        use std::str::FromStr;
        use std::time::Duration;

        let opts = SqliteConnectOptions::from_str(db_url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(30))
            .pragma("foreign_keys", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(3))
            .connect_with(opts)
            .await?;

        // Bootstrap the metadata table (idempotent).
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS orbit_jobs (
                id            TEXT PRIMARY KEY,
                state         TEXT NOT NULL,
                rows_read     INTEGER NOT NULL DEFAULT 0,
                rows_written  INTEGER NOT NULL DEFAULT 0,
                started_at    TEXT NOT NULL,
                finished_at   TEXT,
                error         TEXT,
                spec_json     TEXT NOT NULL
            )"#,
        )
        .execute(&pool)
        .await?;

        Ok(Self {
            pool,
            jobs: Arc::new(DashMap::new()),
            data_root: None,
            query_timeout: DEFAULT_QUERY_TIMEOUT,
            cancels: Arc::new(DashMap::new()),
        })
    }

    /// Restrict every source path to canonicalise under `root`.
    ///
    /// Recommended when accepting pipeline specs from untrusted RPC callers.
    /// Without a data-root configured, any locally-readable path is permitted.
    pub fn with_data_root(mut self, root: std::path::PathBuf) -> Self {
        self.data_root = Some(Arc::new(root));
        self
    }

    /// Override the per-job timeout for the Polars SQL + collect step.
    ///
    /// Defaults to [`DEFAULT_QUERY_TIMEOUT`]. Set lower when accepting
    /// arbitrary SQL from untrusted clients to limit DoS exposure.
    pub fn with_query_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.query_timeout = timeout;
        self
    }

    /// Spawn a pipeline; returns a stream of progress events.
    ///
    /// The job runs in the background; closing the stream does NOT cancel it.
    /// Use [`cancel_job`](Self::cancel_job) for explicit cancellation.
    pub async fn run(&self, spec: PipelineSpec) -> Result<(JobId, impl Stream<Item = Event> + Send + 'static)> {
        spec.validate()?;

        // Path-traversal guard: when a data_root is configured, every source
        // path must canonicalise under it. Prevents an unauthenticated gRPC
        // caller from reading arbitrary files on the host.
        if let Some(root) = self.data_root.as_deref() {
            validate_path_under_root(&spec.source.path, root)?;
        }

        let job_id = JobId::new();
        let started_at = OffsetDateTime::now_utc();

        let status = JobStatus {
            id: job_id,
            state: JobState::Pending,
            rows_read: 0,
            rows_written: 0,
            started_at,
            finished_at: None,
            error: None,
        };
        self.jobs.insert(job_id, status.clone());

        // Register a per-job cancellation flag, polled by execute() between
        // insert batches. Removed when the job finishes (any terminal state).
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.cancels.insert(job_id, cancel_flag.clone());

        // Persist initial job state
        let spec_json = serde_json::to_string(&spec)?;
        sqlx::query(
            r#"INSERT INTO orbit_jobs (id, state, started_at, spec_json) VALUES (?, ?, ?, ?)"#,
        )
        .bind(job_id.to_string())
        .bind(serde_json::to_string(&JobState::Pending)?)
        .bind(started_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default())
        .bind(spec_json)
        .execute(&self.pool)
        .await?;

        let (tx, rx) = mpsc::channel::<Event>(32);
        let engine = self.clone();
        let flag_for_task = cancel_flag.clone();
        tokio::spawn(async move {
            let result = engine.execute(job_id, spec, tx.clone(), flag_for_task).await;
            // Drop the cancel flag whatever the outcome; the job is terminal.
            engine.cancels.remove(&job_id);
            if let Err(e) = result {
                match e {
                    Error::Cancelled => {
                        tracing::info!(job_id = %job_id, "pipeline cancelled");
                        let snapshot = engine
                            .jobs
                            .get(&job_id)
                            .map(|s| (s.rows_read, s.rows_written))
                            .unwrap_or((0, 0));
                        let _ = engine.mark_cancelled(job_id).await;
                        let _ = tx
                            .send(Event::Cancelled {
                                job_id,
                                rows_read: snapshot.0,
                                rows_written: snapshot.1,
                            })
                            .await;
                    }
                    other => {
                        let msg = other.to_string();
                        tracing::error!(error = %msg, job_id = %job_id, "pipeline failed");
                        let _ = engine.mark_failed(job_id, &msg).await;
                        let _ = tx.send(Event::Failed { job_id, error: msg }).await;
                    }
                }
            }
        });

        Ok((job_id, ReceiverStream::new(rx)))
    }

    /// Internal: actually run the pipeline.
    async fn execute(
        &self,
        job_id: JobId,
        spec: PipelineSpec,
        tx: mpsc::Sender<Event>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        self.mark_running(job_id).await?;
        let _ = tx.send(Event::Started { job_id }).await;

        // 1) Build a LazyFrame from the source
        let lf = read_source(&spec).await?;

        // 2 + 3) Apply the optional SQL transform and collect, bounded by
        // `query_timeout` so a runaway user-supplied SQL can't burn a
        // blocking-pool worker forever.
        let sql_opt = spec.sql_transform.clone();
        let df = run_blocking_with_timeout(self.query_timeout, move || -> Result<DataFrame> {
            let lf = if let Some(sql) = sql_opt {
                let mut ctx = polars::sql::SQLContext::new();
                ctx.register("input", lf);
                ctx.execute(&sql)?
            } else {
                lf
            };
            Ok(lf.collect()?)
        })
        .await?;
        let rows_read = df.height() as u64;

        // 4) Create destination table if needed
        ensure_table(&self.pool, &spec.destination_table, &df, spec.dedupe_column.as_deref()).await?;

        // 5) Insert in batches. Poll the cancel flag between batches so the
        // worker terminates cleanly when `cancel_job` is called.
        if cancel_flag.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let mut rows_written: u64 = 0;
        let batch = spec.batch_size.max(1) as usize;
        for chunk_start in (0..df.height()).step_by(batch) {
            if cancel_flag.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let chunk_len = batch.min(df.height() - chunk_start);
            let chunk = df.slice(chunk_start as i64, chunk_len);
            rows_written += insert_chunk(&self.pool, &spec.destination_table, &chunk, spec.dedupe_column.as_deref()).await?;

            // Update progress
            self.bump_progress(job_id, rows_read, rows_written).await?;
            let _ = tx.send(Event::Progress { job_id, rows_read, rows_written }).await;
        }

        self.mark_completed(job_id, rows_read, rows_written).await?;
        let _ = tx.send(Event::Completed { job_id, rows_read, rows_written }).await;
        Ok(())
    }

    pub async fn status(&self, job_id: JobId) -> Result<JobStatus> {
        if let Some(s) = self.jobs.get(&job_id) { return Ok(s.value().clone()); }
        // Fall back to DB
        let row = sqlx::query("SELECT * FROM orbit_jobs WHERE id = ?")
            .bind(job_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| Error::JobNotFound(job_id.to_string()))?;
        Ok(row_to_status(&row)?)
    }

    pub async fn list(&self, limit: u32) -> Result<Vec<JobStatus>> {
        let rows = sqlx::query("SELECT * FROM orbit_jobs ORDER BY started_at DESC LIMIT ?")
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_status).collect()
    }

    /// Request cancellation of a running job.
    ///
    /// Returns `true` if a running job was found and signalled, `false` if
    /// the job id is unknown or has already finished. Cancellation is
    /// cooperative — the executor polls the flag between insert batches and
    /// aborts cleanly with [`Error::Cancelled`].
    pub async fn cancel_job(&self, job_id: JobId) -> Result<bool> {
        match self.cancels.get(&job_id) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ── private helpers ───────────────────────────────────────────────

    async fn mark_running(&self, job_id: JobId) -> Result<()> {
        if let Some(mut s) = self.jobs.get_mut(&job_id) { s.state = JobState::Running; }
        sqlx::query("UPDATE orbit_jobs SET state = ? WHERE id = ?")
            .bind(serde_json::to_string(&JobState::Running)?)
            .bind(job_id.to_string())
            .execute(&self.pool).await?;
        Ok(())
    }

    async fn bump_progress(&self, job_id: JobId, rows_read: u64, rows_written: u64) -> Result<()> {
        if let Some(mut s) = self.jobs.get_mut(&job_id) {
            s.rows_read = rows_read;
            s.rows_written = rows_written;
        }
        sqlx::query("UPDATE orbit_jobs SET rows_read = ?, rows_written = ? WHERE id = ?")
            .bind(rows_read as i64)
            .bind(rows_written as i64)
            .bind(job_id.to_string())
            .execute(&self.pool).await?;
        Ok(())
    }

    async fn mark_completed(&self, job_id: JobId, rows_read: u64, rows_written: u64) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        if let Some(mut s) = self.jobs.get_mut(&job_id) {
            s.state = JobState::Completed;
            s.rows_read = rows_read;
            s.rows_written = rows_written;
            s.finished_at = Some(now);
        }
        sqlx::query("UPDATE orbit_jobs SET state = ?, rows_read = ?, rows_written = ?, finished_at = ? WHERE id = ?")
            .bind(serde_json::to_string(&JobState::Completed)?)
            .bind(rows_read as i64)
            .bind(rows_written as i64)
            .bind(now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default())
            .bind(job_id.to_string())
            .execute(&self.pool).await?;
        Ok(())
    }

    async fn mark_failed(&self, job_id: JobId, err: &str) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        if let Some(mut s) = self.jobs.get_mut(&job_id) {
            s.state = JobState::Failed;
            s.finished_at = Some(now);
            s.error = Some(err.into());
        }
        sqlx::query("UPDATE orbit_jobs SET state = ?, finished_at = ?, error = ? WHERE id = ?")
            .bind(serde_json::to_string(&JobState::Failed)?)
            .bind(now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default())
            .bind(err)
            .bind(job_id.to_string())
            .execute(&self.pool).await?;
        Ok(())
    }

    async fn mark_cancelled(&self, job_id: JobId) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        if let Some(mut s) = self.jobs.get_mut(&job_id) {
            s.state = JobState::Cancelled;
            s.finished_at = Some(now);
            s.error = Some("cancelled".into());
        }
        sqlx::query("UPDATE orbit_jobs SET state = ?, finished_at = ?, error = ? WHERE id = ?")
            .bind(serde_json::to_string(&JobState::Cancelled)?)
            .bind(now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default())
            .bind("cancelled")
            .bind(job_id.to_string())
            .execute(&self.pool).await?;
        Ok(())
    }
}

// ── source readers ────────────────────────────────────────────────────

async fn read_source(spec: &PipelineSpec) -> Result<LazyFrame> {
    let path = spec.source.path.clone();
    let format = spec.source.format;
    let has_header = spec.source.has_header;
    let delim = spec.source.delimiter.as_bytes().first().copied().unwrap_or(b',');

    // Polars I/O is sync; run on blocking pool to keep async runtime healthy.
    let lf = tokio::task::spawn_blocking(move || -> Result<LazyFrame> {
        Ok(match format {
            FileFormat::Csv => {
                let pl_path = PlRefPath::try_from_path(&path)?;
                LazyCsvReader::new(pl_path)
                    .with_has_header(has_header)
                    .with_separator(delim)
                    .finish()?
            }
            FileFormat::Parquet => {
                let pl_path = PlRefPath::try_from_path(&path)?;
                LazyFrame::scan_parquet(pl_path, ScanArgsParquet::default())?
            }
            FileFormat::Json => {
                // JSON has no streaming reader; fall back to eager
                let df = JsonReader::new(std::fs::File::open(&path)?)
                    .finish()?;
                df.lazy()
            }
        })
    })
    .await
    .map_err(|e| Error::Internal(format!("blocking task: {e}")))??;
    Ok(lf)
}

// ── SQLite writers ────────────────────────────────────────────────────

async fn ensure_table(
    pool: &SqlitePool,
    table: &str,
    df: &DataFrame,
    dedupe_column: Option<&str>,
) -> Result<()> {
    // Defensive: re-validate table name even though PipelineSpec::validate
    // already checked it. Defence in depth at every interpolation boundary.
    validate_sql_identifier(table, "destination_table")?;
    if let Some(dc) = dedupe_column {
        validate_sql_identifier(dc, "dedupe_column")?;
    }
    // Infer column types from Polars DataFrame, validating every header to
    // prevent SQL-identifier injection from attacker-controlled CSV/Parquet
    // column names (e.g. `foo"); DROP TABLE x;--`).
    let mut cols = Vec::new();
    for s in df.columns() {
        let name = s.name();
        validate_sql_identifier(name.as_ref(), "source column name")?;
        let ty = polars_dtype_to_sqlite(s.dtype());
        let suffix = if Some(name.as_ref()) == dedupe_column { " UNIQUE" } else { "" };
        cols.push(format!("\"{name}\" {ty}{suffix}"));
    }
    let ddl = format!("CREATE TABLE IF NOT EXISTS \"{table}\" ({})", cols.join(", "));
    pool.execute(ddl.as_str()).await?;
    Ok(())
}

fn polars_dtype_to_sqlite(dt: &DataType) -> &'static str {
    match dt {
        DataType::Boolean => "INTEGER",
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64
        | DataType::Int8  | DataType::Int16  | DataType::Int32  | DataType::Int64 => "INTEGER",
        DataType::Float32 | DataType::Float64 => "REAL",
        DataType::String => "TEXT",
        DataType::Date | DataType::Datetime(_, _) | DataType::Time => "TEXT",
        _ => "TEXT",
    }
}

/// SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 999 (raised to 32766 in
/// 3.32+, but the conservative limit is what the bundled lib often ships
/// with). We stay well under this so multi-row VALUES inserts always fit.
const SQLITE_BIND_LIMIT: usize = 900;

async fn insert_chunk(
    pool: &SqlitePool,
    table: &str,
    df: &DataFrame,
    dedupe_column: Option<&str>,
) -> Result<u64> {
    if df.height() == 0 {
        return Ok(0);
    }

    // Defence in depth: validate every identifier we interpolate.
    validate_sql_identifier(table, "destination_table")?;
    if let Some(dc) = dedupe_column {
        validate_sql_identifier(dc, "dedupe_column")?;
    }
    for s in df.columns() {
        validate_sql_identifier(s.name().as_ref(), "source column name")?;
    }
    let col_names: Vec<String> = df.columns().iter().map(|s| s.name().to_string()).collect();
    let col_list = col_names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let conflict_clause = if dedupe_column.is_some() {
        " ON CONFLICT DO NOTHING"
    } else {
        ""
    };

    // Multi-row INSERT batching: pack as many rows as the SQLite bind limit
    // allows. Previously this loop did one round-trip *per row*; for a 1M-row
    // file that was a million `query.execute()` calls. With ~N=10 columns the
    // new batch holds 90 rows per round-trip — order-of-magnitude fewer
    // statements and a much faster transaction.
    let n_cols = col_names.len();
    let rows_per_batch = (SQLITE_BIND_LIMIT / n_cols.max(1)).max(1);

    let mut tx = pool.begin().await?;
    let mut total_inserted: u64 = 0;
    let columns = df.columns();
    let height = df.height();

    let mut row_idx = 0usize;
    while row_idx < height {
        let batch_end = (row_idx + rows_per_batch).min(height);
        let batch_rows = batch_end - row_idx;

        // NB: `push_values` injects the literal `VALUES ` keyword itself —
        // we deliberately omit it from the init string so the final SQL is
        // `INSERT INTO "t" (cols) VALUES (?, ?, ?), (?, ?, ?), ...`.
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "INSERT INTO \"{table}\" ({col_list}) "
        ));

        // Pre-compute each cell's JsonValue so the closure inside push_values
        // (which is Fn-ish) doesn't need to call fallible Polars accessors.
        let mut row_values: Vec<Vec<JsonValue>> = Vec::with_capacity(batch_rows);
        for i in row_idx..batch_end {
            let mut cells = Vec::with_capacity(n_cols);
            for col in columns {
                let av = col
                    .get(i)
                    .map_err(|e| Error::Internal(format!("col get: {e}")))?;
                cells.push(anyvalue_to_json(&av));
            }
            row_values.push(cells);
        }

        qb.push_values(row_values.iter(), |mut sep, row| {
            for v in row {
                bind_json_value(&mut sep, v);
            }
        });
        qb.push(conflict_clause);

        let r = qb.build().execute(&mut *tx).await?;
        total_inserted += r.rows_affected();
        row_idx = batch_end;
    }
    tx.commit().await?;
    Ok(total_inserted)
}

/// Bind one JSON value into the current `Separated` slot for batched insert.
fn bind_json_value(
    sep: &mut sqlx::query_builder::Separated<'_, '_, sqlx::Sqlite, &'static str>,
    v: &JsonValue,
) {
    match v {
        JsonValue::Null => {
            sep.push_bind(Option::<String>::None);
        }
        JsonValue::Bool(b) => {
            sep.push_bind(i64::from(*b));
        }
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                sep.push_bind(i);
            } else if let Some(f) = n.as_f64() {
                sep.push_bind(f);
            } else {
                sep.push_bind(n.to_string());
            }
        }
        JsonValue::String(s) => {
            sep.push_bind(s.clone());
        }
        other => {
            sep.push_bind(other.to_string());
        }
    }
}

fn anyvalue_to_json(av: &AnyValue<'_>) -> JsonValue {
    match av {
        AnyValue::Null => JsonValue::Null,
        AnyValue::Boolean(b) => JsonValue::Bool(*b),
        AnyValue::String(s) => JsonValue::String((*s).to_string()),
        AnyValue::StringOwned(s) => JsonValue::String(s.to_string()),
        AnyValue::Int8(v)   => JsonValue::Number((*v).into()),
        AnyValue::Int16(v)  => JsonValue::Number((*v).into()),
        AnyValue::Int32(v)  => JsonValue::Number((*v).into()),
        AnyValue::Int64(v)  => JsonValue::Number((*v).into()),
        AnyValue::UInt8(v)  => JsonValue::Number((*v).into()),
        AnyValue::UInt16(v) => JsonValue::Number((*v).into()),
        AnyValue::UInt32(v) => JsonValue::Number((*v).into()),
        AnyValue::UInt64(v) => JsonValue::Number((*v).into()),
        AnyValue::Float32(v) => serde_json::Number::from_f64(f64::from(*v)).map(JsonValue::Number).unwrap_or(JsonValue::Null),
        AnyValue::Float64(v) => serde_json::Number::from_f64(*v).map(JsonValue::Number).unwrap_or(JsonValue::Null),
        other => JsonValue::String(other.to_string()),
    }
}

// ── DB row → JobStatus ────────────────────────────────────────────────

fn row_to_status(row: &sqlx::sqlite::SqliteRow) -> Result<JobStatus> {
    let id_str: String = row.try_get("id")?;
    let id: JobId = id_str.parse().map_err(|e: uuid::Error| Error::Internal(e.to_string()))?;
    let state_str: String = row.try_get("state")?;
    let state: JobState = serde_json::from_str(&state_str)?;
    let rows_read: i64 = row.try_get("rows_read")?;
    let rows_written: i64 = row.try_get("rows_written")?;
    let started_at_str: String = row.try_get("started_at")?;
    let started_at = OffsetDateTime::parse(&started_at_str, &time::format_description::well_known::Rfc3339)
        .map_err(|e| Error::Internal(format!("time parse: {e}")))?;
    let finished_at_str: Option<String> = row.try_get("finished_at")?;
    let finished_at = match finished_at_str {
        Some(s) => Some(OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
            .map_err(|e| Error::Internal(format!("time parse: {e}")))?),
        None => None,
    };
    let error: Option<String> = row.try_get("error")?;
    Ok(JobStatus {
        id, state,
        rows_read: rows_read as u64,
        rows_written: rows_written as u64,
        started_at, finished_at, error,
    })
}

/// Run a blocking closure on the blocking pool, bounded by `timeout`.
///
/// Returns:
/// - `Ok(value)` if the closure completes within `timeout`,
/// - `Err(Error::Timeout)` if the wall-clock deadline elapses first,
/// - `Err(Error::Internal)` if the blocking task panics or is cancelled,
/// - the closure's own `Err` if it fails on its own.
///
/// Used to protect against unbounded user-supplied Polars SQL.
async fn run_blocking_with_timeout<F, T>(timeout: std::time::Duration, work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let join_fut = tokio::task::spawn_blocking(work);
    match tokio::time::timeout(timeout, join_fut).await {
        Ok(Ok(r)) => r,
        Ok(Err(je)) => Err(Error::Internal(format!("blocking task: {je}"))),
        Err(_) => Err(Error::Timeout(timeout)),
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn timeout_fires_when_work_exceeds_budget() {
        let r: Result<i32> = run_blocking_with_timeout(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(500));
            Ok(42)
        })
        .await;
        match r {
            Err(Error::Timeout(d)) => assert_eq!(d, Duration::from_millis(50)),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_returns_value_when_work_is_fast() {
        let r: Result<i32> =
            run_blocking_with_timeout(Duration::from_secs(2), || Ok(7)).await;
        assert!(matches!(r, Ok(7)));
    }

    #[tokio::test]
    async fn timeout_propagates_closure_error() {
        let r: Result<i32> = run_blocking_with_timeout(Duration::from_secs(2), || {
            Err(Error::Internal("boom".into()))
        })
        .await;
        match r {
            Err(Error::Internal(s)) => assert_eq!(s, "boom"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_unknown_job_returns_false() {
        let tmp = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
        let db_url = format!("sqlite://{}?mode=rwc", tmp.path().display());
        let engine = Engine::open(&db_url).await.expect("open");
        // Cancel a never-registered job id.
        let r = engine.cancel_job(JobId::new()).await.expect("cancel");
        assert!(!r, "cancel_job on unknown id should return false");
    }
}
