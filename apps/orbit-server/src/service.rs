//! Tonic gRPC service impl — translates proto types ↔ orbit-core types.

use futures::stream::StreamExt;
use orbit_core::{JobId, JobState, JobStatus};
use orbit_etl::{Engine, Event, FileFormat, FileSource, PipelineSpec};
use orbit_proto::etl::v1 as pb;
use orbit_proto::etl::v1::etl_service_server::EtlService;
use std::pin::Pin;
use std::str::FromStr;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub struct EtlServer {
    engine: Engine,
}

impl EtlServer {
    pub const fn new(engine: Engine) -> Self { Self { engine } }
}

type EventStream = Pin<Box<dyn futures::Stream<Item = std::result::Result<pb::PipelineEvent, Status>> + Send>>;

#[tonic::async_trait]
impl EtlService for EtlServer {
    type RunPipelineStream = EventStream;

    async fn run_pipeline(
        &self,
        req: Request<pb::PipelineSpec>,
    ) -> std::result::Result<Response<Self::RunPipelineStream>, Status> {
        let spec = decode_spec(req.into_inner())?;

        let (_job_id, events) = self.engine.run(spec).await.map_err(to_status)?;

        let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<pb::PipelineEvent, Status>>(32);
        tokio::spawn(async move {
            let mut events = Box::pin(events);
            while let Some(ev) = events.next().await {
                if tx.send(Ok(encode_event(ev))).await.is_err() { break; }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn get_status(&self, req: Request<pb::JobId>) -> std::result::Result<Response<pb::JobStatus>, Status> {
        let id = JobId::from_str(&req.into_inner().id)
            .map_err(|e| Status::invalid_argument(format!("invalid job id: {e}")))?;
        let s = self.engine.status(id).await.map_err(to_status)?;
        Ok(Response::new(encode_status(s)))
    }

    async fn list_jobs(&self, req: Request<pb::ListJobsRequest>) -> std::result::Result<Response<pb::JobList>, Status> {
        let lim = req.into_inner().limit;
        let lim = if lim == 0 { 50 } else { lim };
        let jobs = self.engine.list(lim).await.map_err(to_status)?;
        Ok(Response::new(pb::JobList {
            jobs: jobs.into_iter().map(encode_status).collect(),
        }))
    }

    async fn cancel_job(&self, req: Request<pb::JobId>) -> std::result::Result<Response<pb::CancelResponse>, Status> {
        let id = JobId::from_str(&req.into_inner().id)
            .map_err(|e| Status::invalid_argument(format!("invalid job id: {e}")))?;
        let ok = self.engine.cancel_job(id).await.map_err(to_status)?;
        Ok(Response::new(pb::CancelResponse { cancelled: ok }))
    }
}

// ── encode/decode helpers ─────────────────────────────────────────────

fn decode_spec(p: pb::PipelineSpec) -> std::result::Result<PipelineSpec, Status> {
    let source = match p.source.ok_or_else(|| Status::invalid_argument("source required"))? {
        pb::pipeline_spec::Source::File(f) => FileSource {
            path: std::path::PathBuf::from(f.path),
            format: match pb::FileFormat::try_from(f.format).unwrap_or(pb::FileFormat::Unspecified) {
                pb::FileFormat::Csv => FileFormat::Csv,
                pb::FileFormat::Parquet => FileFormat::Parquet,
                pb::FileFormat::Json => FileFormat::Json,
                pb::FileFormat::Unspecified => return Err(Status::invalid_argument("file format required")),
            },
            has_header: f.has_header,
            delimiter: if f.delimiter.is_empty() { ",".into() } else { f.delimiter },
        },
    };
    Ok(PipelineSpec {
        source,
        destination_table: p.destination_table,
        sql_transform: (!p.sql_transform.is_empty()).then_some(p.sql_transform),
        dedupe_column: (!p.dedupe_column.is_empty()).then_some(p.dedupe_column),
        batch_size: if p.batch_size == 0 { 1024 } else { p.batch_size },
    })
}

fn encode_event(ev: Event) -> pb::PipelineEvent {
    use pb::pipeline_event::Event as PE;
    let inner = match ev {
        Event::Started { job_id }   => PE::Started(pb::JobStarted { job_id: job_id.to_string() }),
        Event::Progress { job_id, rows_read, rows_written } =>
            PE::Progress(pb::JobProgress { job_id: job_id.to_string(), rows_read, rows_written }),
        Event::Completed { job_id, rows_read, rows_written } =>
            PE::Completed(pb::JobCompleted { job_id: job_id.to_string(), rows_read, rows_written }),
        Event::Failed { job_id, error } =>
            PE::Failed(pb::JobFailed { job_id: job_id.to_string(), error }),
        // The proto schema predates the dedicated Cancelled variant; emit it
        // as a JobFailed with a sentinel error string until the proto adds a
        // first-class JobCancelled message. Clients can disambiguate on the
        // literal "cancelled" prefix.
        Event::Cancelled { job_id, .. } =>
            PE::Failed(pb::JobFailed { job_id: job_id.to_string(), error: "cancelled".into() }),
    };
    pb::PipelineEvent { event: Some(inner) }
}

fn encode_status(s: JobStatus) -> pb::JobStatus {
    pb::JobStatus {
        id: s.id.to_string(),
        state: encode_state(s.state) as i32,
        rows_read: s.rows_read,
        rows_written: s.rows_written,
        started_at: s.started_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        finished_at: s.finished_at
            .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
            .unwrap_or_default(),
        error: s.error.unwrap_or_default(),
    }
}

const fn encode_state(s: JobState) -> pb::JobState {
    match s {
        JobState::Pending   => pb::JobState::Pending,
        JobState::Running   => pb::JobState::Running,
        JobState::Completed => pb::JobState::Completed,
        JobState::Failed    => pb::JobState::Failed,
        JobState::Cancelled => pb::JobState::Cancelled,
    }
}

fn to_status(e: orbit_core::Error) -> Status {
    use orbit_core::Error as E;
    match e {
        E::InvalidSpec(m) | E::Serde(m) | E::Internal(m) => Status::invalid_argument(m),
        E::SourceNotFound(p) => Status::not_found(format!("source not found: {p}")),
        E::JobNotFound(id) => Status::not_found(format!("job not found: {id}")),
        E::JobTerminated => Status::failed_precondition("job already terminated"),
        e => Status::internal(e.to_string()),
    }
}

// Suppress an unused-import warning if streams come back without it.
#[allow(dead_code)]
fn _unused(_: Streaming<pb::PipelineSpec>) {}
