//! Job persistence for the openEO `/jobs` routes.
//!
//! Replaces the request-scoped stub with a pluggable store. The default
//! implementation is in-memory (good for tests, single-node demos);
//! a SQLite-backed store lands when we add `sqlx`.
//!
//! `JobRecord` carries everything the openEO 1.3.0 spec defines for the
//! "Detailed Job" object (§4 — minimal subset that the openEO Python
//! client expects).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Lifecycle state of a single job. Matches openEO 1.3.0 values
/// (`created`, `queued`, `running`, `canceled`, `finished`, `error`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Submitted but not yet enqueued for processing.
    Created,
    /// In the queue, waiting for a worker.
    Queued,
    /// Currently executing.
    Running,
    /// Cancelled by the user.
    Canceled,
    /// Completed successfully.
    Finished,
    /// Crashed during execution.
    Error,
}

impl JobStatus {
    /// String label per the openEO 1.3.0 spec.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Canceled => "canceled",
            Self::Finished => "finished",
            Self::Error => "error",
        }
    }
}

/// One output asset attached to a finished job. Mirrors the openEO
/// "Item Asset" shape (`href`, `type`, `roles`, optional `size`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobAsset {
    /// User-facing asset name (used in the URL).
    pub name: String,
    /// IANA media type of the stored bytes.
    pub media_type: String,
    /// Byte count of the stored object.
    pub size: u64,
}

/// One persisted job. The openEO "Detailed Job" extends this — vendor
/// extras stored under `extra` for round-trip.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobRecord {
    /// Stable job id (URL-safe, no `/`).
    pub id: String,
    /// Owning user. Used by the `events.publish` user filter and listing.
    pub user_id: String,
    /// Caller-supplied title.
    pub title: Option<String>,
    /// Caller-supplied description.
    pub description: Option<String>,
    /// The raw process graph as submitted (whole `process` envelope).
    pub process: Value,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Optional progress 0–100.
    pub progress: Option<f64>,
    /// Wall-clock created-at (UNIX seconds).
    pub created: u64,
    /// Wall-clock last-updated (UNIX seconds).
    pub updated: u64,
    /// Output assets produced by the runner. Empty until status=finished.
    #[serde(default)]
    pub assets: Vec<JobAsset>,
}

impl JobRecord {
    /// Serialise to the openEO `Job` JSON shape.
    #[must_use]
    pub fn to_openeo_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "description": self.description,
            "process": self.process,
            "status": self.status.as_str(),
            "progress": self.progress,
            "created": iso8601(self.created),
            "updated": iso8601(self.updated),
        })
    }
}

fn iso8601(unix_secs: u64) -> String {
    // Minimal ISO-8601 without dragging in chrono. openEO clients accept
    // "1970-01-01T00:00:00Z"-shaped strings — we emit the same.
    let secs = unix_secs as i64;
    let days_since_epoch = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u64;
    let (y, m, d) = days_to_ymd(days_since_epoch);
    let (h, min, s) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

/// Civil-from-days conversion (Hinnant 2013). Avoids the chrono dep.
fn days_to_ymd(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // 0..=146_096
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Errors a job store can surface.
#[derive(Debug, Error)]
pub enum JobError {
    /// No job exists with that id.
    #[error("job not found: {0}")]
    NotFound(String),
    /// Backend failure.
    #[error("job store error: {0}")]
    Backend(String),
}

/// Pluggable job store.
#[async_trait]
pub trait JobStore: Send + Sync {
    /// Persist a fresh job. Implementation generates the id and writes
    /// `created`/`updated` timestamps.
    async fn create(
        &self,
        user_id: &str,
        title: Option<String>,
        description: Option<String>,
        process: Value,
    ) -> Result<JobRecord, JobError>;
    /// Get one job by id.
    async fn get(&self, id: &str) -> Result<JobRecord, JobError>;
    /// List jobs owned by a user.
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<JobRecord>, JobError>;
    /// Update the status (and bump `updated`).
    async fn set_status(&self, id: &str, status: JobStatus) -> Result<(), JobError>;
    /// Update progress (and bump `updated`).
    async fn set_progress(&self, id: &str, progress: f64) -> Result<(), JobError>;
    /// Delete a job by id. No-op if absent.
    async fn delete(&self, id: &str) -> Result<(), JobError>;
    /// Attach an output asset entry to a job. The bytes themselves live
    /// in [`crate::file_store::FileStore`]; this stores only metadata.
    async fn add_asset(&self, id: &str, asset: JobAsset) -> Result<(), JobError>;

    /// **Orphan recovery (2026-05-25, ported from JonaAI
    /// `jobs.py` startup recovery)**: transition every job stuck in a
    /// non-terminal state (`Queued` or `Running`) to `Error`. Called once
    /// at server startup: a job left `running` means the process died
    /// mid-execution (crash / restart / OOM), so it can never finish and
    /// must not appear "in progress" forever. Returns the number of jobs
    /// recovered. Terminal states (`Finished`/`Error`/`Canceled`) and
    /// `Created` (never started) are left untouched.
    async fn recover_orphans(&self) -> Result<usize, JobError>;
}

/// In-memory store backed by a `Mutex<Vec<JobRecord>>` and an atomic counter.
#[derive(Debug, Default)]
pub struct InMemoryJobStore {
    inner: Mutex<Vec<JobRecord>>,
    counter: AtomicU64,
}

impl InMemoryJobStore {
    /// New empty store.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

#[async_trait]
impl JobStore for InMemoryJobStore {
    async fn create(
        &self,
        user_id: &str,
        title: Option<String>,
        description: Option<String>,
        process: Value,
    ) -> Result<JobRecord, JobError> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        // Deterministic-but-unique-per-call id. We deliberately don't
        // expose epoch seconds in the id so two jobs created in the same
        // second don't collide.
        let id = format!("job-{n:08x}");
        let now = Self::now();
        let rec = JobRecord {
            id: id.clone(),
            user_id: user_id.into(),
            title,
            description,
            process,
            status: JobStatus::Created,
            progress: Some(0.0),
            created: now,
            updated: now,
            assets: Vec::new(),
        };
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.push(rec.clone());
        Ok(rec)
    }

    async fn get(&self, id: &str) -> Result<JobRecord, JobError> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| JobError::NotFound(id.into()))
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<JobRecord>, JobError> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(g.iter().filter(|r| r.user_id == user_id).cloned().collect())
    }

    async fn set_status(&self, id: &str, status: JobStatus) -> Result<(), JobError> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let rec = g
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| JobError::NotFound(id.into()))?;
        rec.status = status;
        rec.updated = Self::now();
        Ok(())
    }

    async fn set_progress(&self, id: &str, progress: f64) -> Result<(), JobError> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let rec = g
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| JobError::NotFound(id.into()))?;
        rec.progress = Some(progress.clamp(0.0, 100.0));
        rec.updated = Self::now();
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), JobError> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|r| r.id != id);
        Ok(())
    }

    async fn add_asset(&self, id: &str, asset: JobAsset) -> Result<(), JobError> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let rec = g
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| JobError::NotFound(id.into()))?;
        rec.assets.push(asset);
        rec.updated = Self::now();
        Ok(())
    }

    async fn recover_orphans(&self) -> Result<usize, JobError> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let now = Self::now();
        let mut n = 0usize;
        for rec in g.iter_mut() {
            if matches!(rec.status, JobStatus::Queued | JobStatus::Running) {
                rec.status = JobStatus::Error;
                rec.updated = now;
                n += 1;
            }
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------
// SqliteJobStore — persistent storage via sqlx
// ---------------------------------------------------------------------

/// SQLite-backed `JobStore`. Jobs survive across server restarts.
/// Schema (single table `jobs`):
///
/// | id TEXT PK | user_id | title | description | process JSON
/// | status     | progress | created | updated | assets JSON
pub struct SqliteJobStore {
    pool: sqlx::SqlitePool,
    counter: AtomicU64,
}

impl std::fmt::Debug for SqliteJobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteJobStore").finish_non_exhaustive()
    }
}

impl SqliteJobStore {
    /// Open or create the store at the given sqlite URL. Examples:
    ///   `sqlite::memory:`       — in-process, no disk
    ///   `sqlite://./jobs.db?mode=rwc` — file-backed
    pub async fn open(url: &str) -> Result<Self, JobError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteJournalMode, SqliteSynchronous};
        use std::str::FromStr;
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| JobError::Backend(format!("parse url: {e}")))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .map_err(|e| JobError::Backend(format!("pool: {e}")))?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS jobs (
                id          TEXT PRIMARY KEY,
                user_id     TEXT NOT NULL,
                title       TEXT,
                description TEXT,
                process     TEXT NOT NULL,
                status      TEXT NOT NULL,
                progress    REAL,
                created     INTEGER NOT NULL,
                updated     INTEGER NOT NULL,
                assets      TEXT NOT NULL DEFAULT '[]'
            )"#,
        )
        .execute(&pool)
        .await
        .map_err(|e| JobError::Backend(format!("migrate: {e}")))?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_jobs_user ON jobs(user_id)")
            .execute(&pool)
            .await
            .map_err(|e| JobError::Backend(format!("index: {e}")))?;
        // Seed counter from max id suffix so restarts don't collide.
        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM jobs ORDER BY created DESC LIMIT 1")
                .fetch_optional(&pool)
                .await
                .map_err(|e| JobError::Backend(format!("seed: {e}")))?;
        let start = row
            .and_then(|(id,)| u64::from_str_radix(id.trim_start_matches("job-"), 16).ok())
            .map(|n| n + 1)
            .unwrap_or(0);
        Ok(Self { pool, counter: AtomicU64::new(start) })
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn row_to_record(
        id: String,
        user_id: String,
        title: Option<String>,
        description: Option<String>,
        process_json: String,
        status_str: String,
        progress: Option<f64>,
        created: i64,
        updated: i64,
        assets_json: String,
    ) -> Result<JobRecord, JobError> {
        let process: Value = serde_json::from_str(&process_json)
            .map_err(|e| JobError::Backend(format!("process json: {e}")))?;
        let status: JobStatus = serde_json::from_str(&format!("\"{status_str}\""))
            .map_err(|e| JobError::Backend(format!("status: {e}")))?;
        let assets: Vec<JobAsset> = serde_json::from_str(&assets_json)
            .map_err(|e| JobError::Backend(format!("assets: {e}")))?;
        Ok(JobRecord {
            id, user_id, title, description, process, status, progress,
            created: created as u64,
            updated: updated as u64,
            assets,
        })
    }
}

#[async_trait]
impl JobStore for SqliteJobStore {
    async fn create(
        &self,
        user_id: &str,
        title: Option<String>,
        description: Option<String>,
        process: Value,
    ) -> Result<JobRecord, JobError> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("job-{n:08x}");
        let now = Self::now() as i64;
        let process_json = serde_json::to_string(&process)
            .map_err(|e| JobError::Backend(format!("encode process: {e}")))?;
        sqlx::query(
            "INSERT INTO jobs (id, user_id, title, description, process, status, progress, created, updated, assets)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '[]')"
        )
        .bind(&id)
        .bind(user_id)
        .bind(title.as_deref())
        .bind(description.as_deref())
        .bind(&process_json)
        .bind(JobStatus::Created.as_str())
        .bind(0.0_f64)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| JobError::Backend(format!("insert: {e}")))?;
        Ok(JobRecord {
            id, user_id: user_id.into(), title, description, process,
            status: JobStatus::Created,
            progress: Some(0.0),
            created: now as u64,
            updated: now as u64,
            assets: vec![],
        })
    }

    async fn get(&self, id: &str) -> Result<JobRecord, JobError> {
        let row: Option<(String, String, Option<String>, Option<String>, String, String, Option<f64>, i64, i64, String)> =
            sqlx::query_as(
                "SELECT id, user_id, title, description, process, status, progress, created, updated, assets
                 FROM jobs WHERE id = ?",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| JobError::Backend(format!("select: {e}")))?;
        let (id, user_id, title, description, process_json, status_str, progress, created, updated, assets_json) = row
            .ok_or_else(|| JobError::NotFound(id.into()))?;
        Self::row_to_record(id, user_id, title, description, process_json, status_str, progress, created, updated, assets_json)
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<JobRecord>, JobError> {
        let rows: Vec<(String, String, Option<String>, Option<String>, String, String, Option<f64>, i64, i64, String)> =
            sqlx::query_as(
                "SELECT id, user_id, title, description, process, status, progress, created, updated, assets
                 FROM jobs WHERE user_id = ? ORDER BY created ASC",
            )
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| JobError::Backend(format!("select: {e}")))?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, user_id, title, description, process_json, status_str, progress, created, updated, assets_json) in rows {
            out.push(Self::row_to_record(id, user_id, title, description, process_json, status_str, progress, created, updated, assets_json)?);
        }
        Ok(out)
    }

    async fn set_status(&self, id: &str, status: JobStatus) -> Result<(), JobError> {
        let affected = sqlx::query("UPDATE jobs SET status = ?, updated = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(Self::now() as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| JobError::Backend(format!("update: {e}")))?
            .rows_affected();
        if affected == 0 {
            return Err(JobError::NotFound(id.into()));
        }
        Ok(())
    }

    async fn set_progress(&self, id: &str, progress: f64) -> Result<(), JobError> {
        let p = progress.clamp(0.0, 100.0);
        let affected = sqlx::query("UPDATE jobs SET progress = ?, updated = ? WHERE id = ?")
            .bind(p)
            .bind(Self::now() as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| JobError::Backend(format!("update: {e}")))?
            .rows_affected();
        if affected == 0 {
            return Err(JobError::NotFound(id.into()));
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), JobError> {
        sqlx::query("DELETE FROM jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| JobError::Backend(format!("delete: {e}")))?;
        Ok(())
    }

    async fn add_asset(&self, id: &str, asset: JobAsset) -> Result<(), JobError> {
        // Append-mode: read current assets JSON, push, write back. Atomic
        // via a transaction so we don't race a concurrent add_asset.
        let mut tx = self.pool.begin().await
            .map_err(|e| JobError::Backend(format!("tx: {e}")))?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT assets FROM jobs WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| JobError::Backend(format!("select: {e}")))?;
        let (assets_json,) = row.ok_or_else(|| JobError::NotFound(id.into()))?;
        let mut assets: Vec<JobAsset> = serde_json::from_str(&assets_json)
            .map_err(|e| JobError::Backend(format!("decode assets: {e}")))?;
        assets.push(asset);
        let assets_json = serde_json::to_string(&assets)
            .map_err(|e| JobError::Backend(format!("encode assets: {e}")))?;
        sqlx::query("UPDATE jobs SET assets = ?, updated = ? WHERE id = ?")
            .bind(&assets_json)
            .bind(Self::now() as i64)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| JobError::Backend(format!("update: {e}")))?;
        tx.commit().await.map_err(|e| JobError::Backend(format!("commit: {e}")))?;
        Ok(())
    }

    async fn recover_orphans(&self) -> Result<usize, JobError> {
        // Single UPDATE: flip queued/running → error. Matches JonaAI's
        // startup raw-SQL recovery (jobs.py:93).
        let res = sqlx::query(
            "UPDATE jobs SET status = 'error', updated = ? \
             WHERE status IN ('queued', 'running')",
        )
        .bind(Self::now() as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| JobError::Backend(format!("recover_orphans: {e}")))?;
        Ok(res.rows_affected() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Value {
        serde_json::json!({
            "process_graph": {
                "load1": { "process_id": "load_collection", "arguments": { "id": "SENTINEL2_L2A" } },
                "save":  { "process_id": "save_result", "arguments": { "data": { "from_node": "load1" } }, "result": true }
            }
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_assigns_unique_ids() {
        let s = InMemoryJobStore::new();
        let a = s.create("alice", None, None, graph()).await.unwrap();
        let b = s.create("alice", None, None, graph()).await.unwrap();
        assert_ne!(a.id, b.id);
        assert!(a.id.starts_with("job-"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_returns_created_record() {
        let s = InMemoryJobStore::new();
        let a = s
            .create("alice", Some("Test".into()), Some("desc".into()), graph())
            .await
            .unwrap();
        let got = s.get(&a.id).await.unwrap();
        assert_eq!(got.id, a.id);
        assert_eq!(got.title, Some("Test".into()));
        assert_eq!(got.status, JobStatus::Created);
        assert_eq!(got.progress, Some(0.0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_unknown_id_returns_not_found() {
        let s = InMemoryJobStore::new();
        assert!(matches!(s.get("missing").await, Err(JobError::NotFound(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recover_orphans_flips_running_and_queued_to_error() {
        let s = InMemoryJobStore::new();
        let running = s.create("u", None, None, graph()).await.unwrap();
        let queued = s.create("u", None, None, graph()).await.unwrap();
        let created = s.create("u", None, None, graph()).await.unwrap();
        let finished = s.create("u", None, None, graph()).await.unwrap();
        s.set_status(&running.id, JobStatus::Running).await.unwrap();
        s.set_status(&queued.id, JobStatus::Queued).await.unwrap();
        s.set_status(&finished.id, JobStatus::Finished).await.unwrap();
        // created stays Created.
        let n = s.recover_orphans().await.unwrap();
        assert_eq!(n, 2, "only running + queued recovered");
        assert_eq!(s.get(&running.id).await.unwrap().status, JobStatus::Error);
        assert_eq!(s.get(&queued.id).await.unwrap().status, JobStatus::Error);
        assert_eq!(s.get(&created.id).await.unwrap().status, JobStatus::Created,
                   "Created (never started) must not be touched");
        assert_eq!(s.get(&finished.id).await.unwrap().status, JobStatus::Finished,
                   "Finished (terminal) must not be touched");
        // Idempotent: second call recovers nothing.
        assert_eq!(s.recover_orphans().await.unwrap(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_for_user_filters_by_owner() {
        let s = InMemoryJobStore::new();
        s.create("alice", None, None, graph()).await.unwrap();
        s.create("alice", None, None, graph()).await.unwrap();
        s.create("bob", None, None, graph()).await.unwrap();
        let alice = s.list_for_user("alice").await.unwrap();
        let bob = s.list_for_user("bob").await.unwrap();
        assert_eq!(alice.len(), 2);
        assert_eq!(bob.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_status_updates_and_bumps_updated() {
        let s = InMemoryJobStore::new();
        let a = s.create("alice", None, None, graph()).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        s.set_status(&a.id, JobStatus::Running).await.unwrap();
        let got = s.get(&a.id).await.unwrap();
        assert_eq!(got.status, JobStatus::Running);
        assert!(got.updated >= a.updated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_progress_clamps_to_unit_interval() {
        let s = InMemoryJobStore::new();
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.set_progress(&a.id, 150.0).await.unwrap();
        assert_eq!(s.get(&a.id).await.unwrap().progress, Some(100.0));
        s.set_progress(&a.id, -5.0).await.unwrap();
        assert_eq!(s.get(&a.id).await.unwrap().progress, Some(0.0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_removes_record() {
        let s = InMemoryJobStore::new();
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.delete(&a.id).await.unwrap();
        assert!(matches!(s.get(&a.id).await, Err(JobError::NotFound(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_is_idempotent() {
        let s = InMemoryJobStore::new();
        s.delete("never_existed").await.unwrap();
    }

    #[test]
    fn iso8601_renders_epoch_correctly() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // 2024-01-01T00:00:00Z = 1_704_067_200
        assert_eq!(iso8601(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn status_string_round_trips_through_serde() {
        for s in [
            JobStatus::Created,
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Canceled,
            JobStatus::Finished,
            JobStatus::Error,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: JobStatus = serde_json::from_str(&j).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn to_openeo_json_has_required_fields() {
        let now = 1_704_067_200;
        let rec = JobRecord {
            id: "job-1".into(),
            user_id: "alice".into(),
            title: Some("t".into()),
            description: None,
            process: serde_json::json!({}),
            status: JobStatus::Queued,
            progress: Some(50.0),
            created: now,
            updated: now,
            assets: vec![],
        };
        let j = rec.to_openeo_json();
        assert_eq!(j["id"], "job-1");
        assert_eq!(j["status"], "queued");
        assert_eq!(j["progress"], 50.0);
        assert_eq!(j["created"], "2024-01-01T00:00:00Z");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn newly_created_job_has_no_assets() {
        let s = InMemoryJobStore::new();
        let a = s.create("alice", None, None, graph()).await.unwrap();
        assert!(a.assets.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_asset_appends_to_record() {
        let s = InMemoryJobStore::new();
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.add_asset(
            &a.id,
            JobAsset {
                name: "result.json".into(),
                media_type: "application/json".into(),
                size: 42,
            },
        )
        .await
        .unwrap();
        let got = s.get(&a.id).await.unwrap();
        assert_eq!(got.assets.len(), 1);
        assert_eq!(got.assets[0].name, "result.json");
        assert_eq!(got.assets[0].media_type, "application/json");
        assert_eq!(got.assets[0].size, 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_asset_unknown_job_is_not_found() {
        let s = InMemoryJobStore::new();
        let r = s
            .add_asset(
                "ghost",
                JobAsset { name: "x".into(), media_type: "*/*".into(), size: 0 },
            )
            .await;
        assert!(matches!(r, Err(JobError::NotFound(_))));
    }

    // ---------- P2b — SqliteJobStore (sqlx) ----------

    async fn sqlite_store() -> SqliteJobStore {
        SqliteJobStore::open("sqlite::memory:").await.expect("open mem db")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_round_trips_create_then_get() {
        let s = sqlite_store().await;
        let a = s.create("alice", Some("t".into()), Some("d".into()), graph()).await.unwrap();
        let got = s.get(&a.id).await.unwrap();
        assert_eq!(got.id, a.id);
        assert_eq!(got.title.as_deref(), Some("t"));
        assert_eq!(got.description.as_deref(), Some("d"));
        assert_eq!(got.status, JobStatus::Created);
        assert_eq!(got.progress, Some(0.0));
        assert!(got.assets.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_recover_orphans_flips_running_queued_to_error() {
        let s = sqlite_store().await;
        let running = s.create("u", None, None, graph()).await.unwrap();
        let queued = s.create("u", None, None, graph()).await.unwrap();
        let finished = s.create("u", None, None, graph()).await.unwrap();
        s.set_status(&running.id, JobStatus::Running).await.unwrap();
        s.set_status(&queued.id, JobStatus::Queued).await.unwrap();
        s.set_status(&finished.id, JobStatus::Finished).await.unwrap();
        let n = s.recover_orphans().await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(s.get(&running.id).await.unwrap().status, JobStatus::Error);
        assert_eq!(s.get(&queued.id).await.unwrap().status, JobStatus::Error);
        assert_eq!(s.get(&finished.id).await.unwrap().status, JobStatus::Finished);
        assert_eq!(s.recover_orphans().await.unwrap(), 0, "idempotent");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_get_unknown_is_not_found() {
        let s = sqlite_store().await;
        assert!(matches!(s.get("nope").await, Err(JobError::NotFound(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_list_for_user_filters() {
        let s = sqlite_store().await;
        s.create("alice", None, None, graph()).await.unwrap();
        s.create("alice", None, None, graph()).await.unwrap();
        s.create("bob", None, None, graph()).await.unwrap();
        assert_eq!(s.list_for_user("alice").await.unwrap().len(), 2);
        assert_eq!(s.list_for_user("bob").await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_set_status_persists() {
        let s = sqlite_store().await;
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.set_status(&a.id, JobStatus::Running).await.unwrap();
        assert_eq!(s.get(&a.id).await.unwrap().status, JobStatus::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_set_progress_clamps() {
        let s = sqlite_store().await;
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.set_progress(&a.id, 200.0).await.unwrap();
        assert_eq!(s.get(&a.id).await.unwrap().progress, Some(100.0));
        s.set_progress(&a.id, -1.0).await.unwrap();
        assert_eq!(s.get(&a.id).await.unwrap().progress, Some(0.0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_delete_removes_record() {
        let s = sqlite_store().await;
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.delete(&a.id).await.unwrap();
        assert!(matches!(s.get(&a.id).await, Err(JobError::NotFound(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_add_asset_persists_and_round_trips() {
        let s = sqlite_store().await;
        let a = s.create("alice", None, None, graph()).await.unwrap();
        s.add_asset(&a.id, JobAsset {
            name: "result.tif".into(),
            media_type: "image/tiff".into(),
            size: 6552,
        }).await.unwrap();
        s.add_asset(&a.id, JobAsset {
            name: "thumbnail.png".into(),
            media_type: "image/png".into(),
            size: 2048,
        }).await.unwrap();
        let got = s.get(&a.id).await.unwrap();
        assert_eq!(got.assets.len(), 2);
        assert_eq!(got.assets[0].name, "result.tif");
        assert_eq!(got.assets[1].size, 2048);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_survives_pool_reopen_on_same_file() {
        // Use a temp file path so a second `open()` can read the data
        // written by the first.
        let dir = std::env::temp_dir().join(format!(
            "orbit-jobdb-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/jobs.db?mode=rwc", dir.display());
        let saved_id = {
            let s = SqliteJobStore::open(&url).await.unwrap();
            let a = s.create("alice", Some("persist".into()), None, graph()).await.unwrap();
            s.set_status(&a.id, JobStatus::Finished).await.unwrap();
            a.id
        };
        // Reopen and verify the record is still there with Finished status.
        let s2 = SqliteJobStore::open(&url).await.unwrap();
        let got = s2.get(&saved_id).await.unwrap();
        assert_eq!(got.title.as_deref(), Some("persist"));
        assert_eq!(got.status, JobStatus::Finished);
        // Counter should resume past the persisted id.
        let next = s2.create("alice", None, None, graph()).await.unwrap();
        assert_ne!(next.id, saved_id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_set_status_unknown_id_is_not_found() {
        let s = sqlite_store().await;
        assert!(matches!(
            s.set_status("missing", JobStatus::Running).await,
            Err(JobError::NotFound(_))
        ));
    }
}
