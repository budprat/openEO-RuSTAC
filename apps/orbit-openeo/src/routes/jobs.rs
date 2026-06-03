//! Batch jobs routes.
//!
//! `POST /jobs`                          — submit a process graph; returns 201 + `OpenEO-Identifier`.
//! `GET /jobs`                           — list jobs for the caller.
//! `GET /jobs/{job_id}`                  — detailed job status.
//! `PATCH /jobs/{job_id}`                — update title / description.
//! `DELETE /jobs/{job_id}`               — drop job.
//! `GET /jobs/{job_id}/estimate`         — cost / duration estimate.
//! `POST /jobs/{job_id}/results`         — start processing.
//! `GET /jobs/{job_id}/results`          — fetch result manifest.
//! `DELETE /jobs/{job_id}/results`       — discard results.
//!
//! Caller identity: until OIDC plumbing lands, every job is owned by
//! `"anonymous"`. The job store already filters by user_id so the listing
//! endpoint will Just Work once the auth layer attaches a real identity.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::file_store::{FileError, FileKey};
use crate::job_store::{JobError, JobStatus};
use crate::process_graph::ProcessGraphAnalysis;
use crate::runner;
use crate::AppState;

const DEFAULT_USER: &str = "anonymous";

/// Mount jobs routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/jobs", get(list_jobs).post(create_job))
        .route(
            "/jobs/{job_id}",
            get(get_job).patch(update_job).delete(delete_job),
        )
        .route("/jobs/{job_id}/estimate", get(estimate_job))
        .route(
            "/jobs/{job_id}/results",
            get(get_results).post(start_processing).delete(discard_results),
        )
        .route("/jobs/{job_id}/results/{asset_name}", get(download_asset))
}

async fn list_jobs(State(state): State<AppState>) -> Json<Value> {
    let jobs = state
        .jobs
        .list_for_user(DEFAULT_USER)
        .await
        .unwrap_or_default();
    let arr: Vec<Value> = jobs.iter().map(|j| j.to_openeo_json()).collect();
    Json(json!({ "jobs": arr, "links": [] }))
}

async fn create_job(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let title = body.get("title").and_then(|v| v.as_str()).map(String::from);
    let description = body.get("description").and_then(|v| v.as_str()).map(String::from);

    // Capture the inner `Process` object up-front — we'll persist that
    // (not the outer envelope) so GET /jobs/{id}.process matches the
    // openEO 1.3.0 Process schema. Falls back to `{"process_graph": pg}`
    // for loose clients that posted just `{"process_graph": ...}`.
    let process_inner = body
        .get("process")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "process_graph": body.get("process_graph").cloned().unwrap_or(Value::Null) }));
    let pg_val = match body.get("process").and_then(|p| p.get("process_graph")) {
        Some(v) => v.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "code": "ProcessGraphMissing",
                    "message": "request body must contain process.process_graph"
                })),
            )
                .into_response()
        }
    };
    let nodes: std::collections::BTreeMap<String, eo_process::Process> =
        match serde_json::from_value(pg_val) {
            Ok(n) => n,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "code": "ProcessGraphInvalid",
                        "message": e.to_string()
                    })),
                )
                    .into_response()
            }
        };
    if let Err(e) = ProcessGraphAnalysis::build(&eo_process::ProcessGraph { nodes }) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "ProcessGraphInvalid", "message": e.to_string() })),
        )
            .into_response();
    }

    let rec = match state
        .jobs
        .create(DEFAULT_USER, title, description, process_inner)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "Internal", "message": e.to_string() })),
            )
                .into_response()
        }
    };

    let location = format!("/jobs/{}", rec.id);
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, location.clone()),
            (
                header::HeaderName::from_static("openeo-identifier"),
                rec.id.clone(),
            ),
        ],
        Json(rec.to_openeo_json()),
    )
        .into_response()
}

async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    match state.jobs.get(&job_id).await {
        Ok(rec) => (StatusCode::OK, Json(rec.to_openeo_json())).into_response(),
        Err(JobError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "JobNotFound", "message": format!("job '{job_id}' not found") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
        )
            .into_response(),
    }
}

async fn update_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Json(patch): Json<Value>,
) -> impl IntoResponse {
    // audit-fix (2026-06-03): actually PERSIST the patch. Previously this
    // returned 204 while silently discarding `title`/`description`. Only the
    // fields present in the body are updated (openEO PATCH semantics).
    let title = patch.get("title").and_then(|v| v.as_str()).map(str::to_string);
    let description = patch
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    match state.jobs.set_metadata(&job_id, title, description).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(JobError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "JobNotFound" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
        )
            .into_response(),
    }
}

async fn delete_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    // audit-fix (2026-06-03): surface store failures instead of always
    // reporting 204. A missing job → 404 (openEO `JobNotFound`); a real
    // backend error → 500, so a client is never told a job was deleted when
    // it was not.
    match state.jobs.delete(&job_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(JobError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "JobNotFound", "message": format!("job '{job_id}' not found") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
        )
            .into_response(),
    }
}

async fn estimate_job(Path(_job_id): Path<String>) -> Json<Value> {
    Json(json!({
        "costs": 0,
        "duration": "PT0S",
        "size": 0,
        "downloads_included": 0
    }))
}

async fn start_processing(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    let rec = match state.jobs.get(&job_id).await {
        Ok(r) => r,
        Err(JobError::NotFound(_)) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "code": "JobNotFound" }))).into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "Internal", "message": e.to_string() })),
            )
                .into_response()
        }
    };

    // **Idempotent start (audit-fix 2026-06-03)**: re-`POST`ing results for a
    // job that is already queued or running must NOT spawn a second runner —
    // that double-runs the graph (double download + double compute) and
    // corrupts progress/asset state. openEO clients that retry the start verb
    // get a `409` instead. Created/Finished/Error/Canceled may be (re)started.
    if matches!(rec.status, JobStatus::Queued | JobStatus::Running) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "JobLocked",
                "message": format!("job '{job_id}' is already {}", rec.status.as_str())
            })),
        )
            .into_response();
    }

    // **P0-5 / P1-9 (audit-fix 2026-06-03)**: acquire a permit before
    // spawning so concurrent requests can't fork-bomb the runtime. We use
    // `try_acquire_owned` (non-blocking) so that when `job_sem` is exhausted
    // the caller gets an immediate `503 QueueFull` instead of an unbounded
    // queue of waiters. The permit is moved INTO the supervisor task below
    // and is released only when `run_job` actually completes — previously it
    // was bound to this handler's stack and dropped on return (microseconds),
    // so the cap was never enforced (the spawned job outlived the permit).
    let sem = state.job_sem.clone();
    let permit = match sem.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "code": "QueueFull",
                    "message": "maximum concurrent jobs reached; retry later"
                })),
            )
                .into_response();
        }
    };
    let runner = runner::DefaultRunner {
        store: state.jobs.clone(),
        executor: state.executor.clone(),
        bus: state.events.clone(),
        files: state.files.clone(),
        metrics: state.metrics.clone(),
    };
    use crate::runner::JobRunner;
    // Supervisor task: owns the permit for the job's FULL lifetime (it awaits
    // the inner run_job handle), races it against a cancel signal, and converts
    // a panic into a clean `Error` transition + log. Registered in the job
    // registry so `DELETE /results` can cancel it and graceful shutdown can
    // drain it. (audit-fix 2026-06-03 — permit lifetime, panic supervision,
    // cooperative cancellation, drain.)
    let store_for_super = state.jobs.clone();
    let registry = state.job_registry.clone();
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
    let cancel_for_task = cancel.clone();
    let jid = job_id.clone();
    let uid = rec.user_id.clone();
    let body = rec.process.clone();
    let handle = tokio::spawn(async move {
        let _permit = permit; // released only when the job completes or cancels
        let mut inner = runner.spawn(jid.clone(), uid, body);
        tokio::select! {
            // Cooperative cancellation via DELETE /jobs/{id}/results. Aborts
            // the inner run_job at its next await point (a spawn_blocking GDAL
            // call already in flight finishes detached, its result discarded).
            _ = cancel_for_task.notified() => {
                inner.abort();
                tracing::info!(job_id = %jid, "job cancelled via DELETE /results");
                if let Ok(rec) = store_for_super.get(&jid).await {
                    if !matches!(
                        rec.status,
                        JobStatus::Finished | JobStatus::Error | JobStatus::Canceled
                    ) {
                        let _ = store_for_super.set_status(&jid, JobStatus::Canceled).await;
                    }
                }
            }
            joined = &mut inner => {
                if let Err(join_err) = joined {
                    if !join_err.is_cancelled() {
                        tracing::error!(job_id = %jid, error = %join_err, "run_job task panicked; marking job Error");
                        if let Ok(rec) = store_for_super.get(&jid).await {
                            if !matches!(
                                rec.status,
                                JobStatus::Finished | JobStatus::Error | JobStatus::Canceled
                            ) {
                                let _ = store_for_super.set_status(&jid, JobStatus::Error).await;
                            }
                        }
                    }
                }
            }
        }
        registry.deregister(&jid);
    });
    // Register synchronously (no await between spawn and here) so the
    // supervisor — not polled until this handler yields — cannot deregister
    // before this insert.
    state.job_registry.register(job_id.clone(), cancel, handle);
    StatusCode::ACCEPTED.into_response()
}

async fn get_results(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    let rec = match state.jobs.get(&job_id).await {
        Ok(r) => r,
        Err(JobError::NotFound(_)) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "code": "JobNotFound" }))).into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "Internal", "message": e.to_string() })),
            )
                .into_response()
        }
    };
    if rec.status != JobStatus::Finished {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "JobNotFinished",
                "message": format!("job '{job_id}' is in state '{}'", rec.status.as_str())
            })),
        )
            .into_response();
    }
    // Render each asset as an openEO Item Asset:
    //   "result.json": { "href": "/jobs/{id}/results/result.json",
    //                    "type": "application/json", "roles": ["data"] }
    let mut assets_obj = serde_json::Map::new();
    for a in &rec.assets {
        assets_obj.insert(
            a.name.clone(),
            json!({
                "href": format!("/jobs/{}/results/{}", rec.id, a.name),
                "type": a.media_type,
                "roles": ["data"],
                "file:size": a.size,
            }),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "id": rec.id,
            "stac_version": "1.0.0",
            "type": "Feature",
            "assets": Value::Object(assets_obj),
            "links": []
        })),
    )
        .into_response()
}

async fn download_asset(
    State(state): State<AppState>,
    Path((job_id, asset_name)): Path<(String, String)>,
) -> impl IntoResponse {
    // Require the job to exist (returns 404 JobNotFound if not) and the
    // asset to be one of the recorded entries (so we don't expose
    // arbitrary file-store contents).
    let rec = match state.jobs.get(&job_id).await {
        Ok(r) => r,
        Err(JobError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "code": "JobNotFound" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "Internal", "message": e.to_string() })),
            )
                .into_response();
        }
    };
    let asset = match rec.assets.iter().find(|a| a.name == asset_name) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "code": "ResultNotFound",
                    "message": format!("asset '{asset_name}' not produced by job '{job_id}'")
                })),
            )
                .into_response();
        }
    };
    let key = FileKey::new(&rec.id, &asset.name);
    match state.files.get(&key).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, asset.media_type.clone()),
                (header::CONTENT_LENGTH, asset.size.to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(FileError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "ResultNotFound" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
        )
            .into_response(),
    }
}

async fn discard_results(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    // openEO `DELETE /jobs/{id}/results` = cancel processing. audit-fix
    // (2026-06-03): previously a no-op stub that always 204'd. Now it
    // cooperatively cancels an in-flight job (signals its supervisor) and
    // transitions a queued/running job to `Canceled`. NB: cancellation is
    // best-effort — a spawn_blocking GDAL call mid-flight cannot be preempted,
    // but its result is discarded and the job is marked Canceled.
    match state.jobs.get(&job_id).await {
        Ok(rec) => {
            let was_active = matches!(rec.status, JobStatus::Queued | JobStatus::Running);
            state.job_registry.cancel(&job_id);
            if was_active {
                let _ = state.jobs.set_status(&job_id, JobStatus::Canceled).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(JobError::NotFound(_)) => {
            (StatusCode::NOT_FOUND, Json(json!({ "code": "JobNotFound" }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "Internal", "message": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStateBuilder;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        Router::new().merge(router()).with_state(AppStateBuilder::new().build())
    }

    fn graph_body() -> &'static str {
        r#"{
            "process": {
                "process_graph": {
                    "load1": {
                        "process_id": "load_collection",
                        "arguments": { "id": "SENTINEL2_L2A" }
                    },
                    "save": {
                        "process_id": "save_result",
                        "arguments": { "data": { "from_node": "load1" } },
                        "result": true
                    }
                }
            },
            "title": "test job",
            "description": "smoke test"
        }"#
    }

    async fn body_to_json(resp: axum::http::Response<Body>) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_jobs_creates_201_with_location_and_openeo_identifier() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        assert!(resp.headers().get("location").unwrap().to_str().unwrap().starts_with("/jobs/"));
        assert!(resp.headers().get("openeo-identifier").is_some());
        let v = body_to_json(resp).await;
        assert_eq!(v["status"], "created");
        assert_eq!(v["title"], "test job");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_jobs_400_on_missing_process_graph() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"title":"no graph"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let v = body_to_json(resp).await;
        assert_eq!(v["code"], "ProcessGraphMissing");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_jobs_400_on_cycle() {
        let body = r#"{"process":{"process_graph":{
            "a":{"process_id":"add","arguments":{"x":{"from_node":"b"},"y":1}},
            "b":{"process_id":"add","arguments":{"x":{"from_node":"a"},"y":1},"result":true}
        }}}"#;
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let v = body_to_json(resp).await;
        assert_eq!(v["code"], "ProcessGraphInvalid");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_jobs_lists_created_job() {
        let app = app();
        let _ = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        let arr = v["jobs"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "test job");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_job_by_id_returns_record() {
        let app = app();
        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created
            .headers()
            .get("openeo-identifier")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/jobs/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        assert_eq!(v["id"], id);
        assert_eq!(v["status"], "created");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_unknown_job_returns_404() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/jobs/never_existed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let v = body_to_json(resp).await;
        assert_eq!(v["code"], "JobNotFound");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_job_returns_204() {
        let app = app();
        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/jobs/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_results_before_finished_returns_404() {
        let app = app();
        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/jobs/{id}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let v = body_to_json(resp).await;
        assert_eq!(v["code"], "JobNotFinished");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_results_runs_executor_and_emits_events() {
        let bus = std::sync::Arc::new(crate::event_bus::InMemoryEventBus::new(64))
            as std::sync::Arc<dyn crate::event_bus::EventBus>;
        let mut sub = bus.subscribe();
        let state = AppStateBuilder::new().with_events(bus.clone()).build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"process":{"process_graph":{
                            "a":{"process_id":"add","arguments":{"x":1,"y":2},"result":true}
                        }}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/jobs/{id}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);

        // Lifecycle (P3b): Started → 3× Progress → Completed.
        let mut events = Vec::new();
        for _ in 0..5 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv())
                .await.unwrap().unwrap();
            events.push(ev);
        }
        assert_eq!(events.first().unwrap().kind, crate::event_bus::JobEventKind::Started);
        assert_eq!(events.last().unwrap().kind, crate::event_bus::JobEventKind::Completed);
        let progress_count = events.iter()
            .filter(|e| e.kind == crate::event_bus::JobEventKind::Progress)
            .count();
        assert!(progress_count >= 2, "expected ≥2 Progress events, got {progress_count}");
        assert_eq!(events.first().unwrap().job_id, id);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rec = state.jobs.get(&id).await.unwrap();
        assert_eq!(rec.status, JobStatus::Finished);
        assert_eq!(rec.progress, Some(100.0));
    }

    /// E3 — once the job is finished, GET /jobs/{id}/results returns a
    /// STAC-shaped manifest where `assets.result.json` includes the href,
    /// media_type, roles, and file:size.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_results_manifest_lists_assets_after_finish() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"process":{"process_graph":{
                            "a":{"process_id":"add","arguments":{"x":7,"y":35},"result":true}
                        }}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();

        let _ = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/jobs/{id}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Poll until finished (max 1 s).
        let mut got = None;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let rec = state.jobs.get(&id).await.unwrap();
            if rec.status == JobStatus::Finished { got = Some(rec); break; }
        }
        assert!(got.is_some(), "job never finished");

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/jobs/{id}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_to_json(resp).await;
        let assets = v["assets"].as_object().expect("assets object");
        assert!(assets.contains_key("result.json"), "got assets={assets:?}");
        let r = &assets["result.json"];
        assert_eq!(r["href"], format!("/jobs/{id}/results/result.json"));
        assert_eq!(r["type"], "application/json");
        assert_eq!(r["roles"][0], "data");
        assert!(r["file:size"].as_u64().unwrap() > 0);
    }

    /// E4 — GET /jobs/{id}/results/{asset_name} streams the bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn download_asset_returns_bytes_with_correct_content_type() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"process":{"process_graph":{
                            "a":{"process_id":"add","arguments":{"x":100,"y":1},"result":true}
                        }}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();

        let _ = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/jobs/{id}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if state.jobs.get(&id).await.unwrap().status == JobStatus::Finished { break; }
        }

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/jobs/{id}/results/result.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.as_f64().unwrap(), 101.0);
    }

    /// E4 — unknown asset_name returns 404 ResultNotFound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn download_unknown_asset_returns_404_result_not_found() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = created.headers().get("openeo-identifier").unwrap().to_str().unwrap().to_string();

        // Don't start processing — asset list is empty. Asking for any
        // asset name must 404.
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/jobs/{id}/results/anything.bin"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let v = body_to_json(resp).await;
        assert_eq!(v["code"], "ResultNotFound");
    }

    /// E4 — unknown job returns 404 JobNotFound (not ResultNotFound).
    #[tokio::test(flavor = "current_thread")]
    async fn download_unknown_job_returns_404_job_not_found() {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/jobs/no-such-job/results/result.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let v = body_to_json(resp).await;
        assert_eq!(v["code"], "JobNotFound");
    }

    // ---- audit-fix QW2: semaphore permit is held for the job's lifetime ----

    /// Executor that blocks inside `run_sync` until a gate is released, so a
    /// started job stays in-flight (and its permit held) while the test fires
    /// a second start request.
    struct SlowExecutor {
        gate: std::sync::Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl crate::executor::ProcessGraphExecutor for SlowExecutor {
        async fn run_sync(
            &self,
            _body: &Value,
        ) -> Result<crate::executor::SyncResult, crate::executor::ExecError> {
            self.gate.notified().await;
            Ok(crate::executor::SyncResult::json(&json!({"ok": true})))
        }
        async fn enqueue(
            &self,
            _body: &Value,
        ) -> Result<String, crate::executor::ExecError> {
            Ok("noop".into())
        }
    }

    async fn create_job(app: &axum::Router) -> String {
        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(graph_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        created
            .headers()
            .get("openeo-identifier")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    async fn start_job(app: &axum::Router, id: &str) -> axum::http::StatusCode {
        app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/jobs/{id}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_processing_holds_permit_until_job_finishes() {
        // concurrency = 1: once one job is running, the next start must 503.
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let exec = std::sync::Arc::new(SlowExecutor { gate: gate.clone() })
            as std::sync::Arc<dyn crate::executor::ProcessGraphExecutor>;
        let state = AppStateBuilder::new()
            .with_executor(exec)
            .with_job_concurrency(1)
            .build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let j1 = create_job(&app).await;
        let j2 = create_job(&app).await;

        // Start j1 — acquires the only permit synchronously, then blocks in run_sync.
        assert_eq!(start_job(&app, &j1).await, 202);
        // Start j2 — permit is still held by j1's supervisor task → QueueFull.
        assert_eq!(
            start_job(&app, &j2).await,
            503,
            "second start must 503 while the only permit is held by the running job"
        );

        // Release j1: `Notify::notify_waiters()` does NOT buffer, so signal
        // repeatedly until j1's run_sync has actually parked and woken and the
        // job reaches a terminal state (proving its permit was returned).
        let mut j1_done = false;
        for _ in 0..100 {
            gate.notify_waiters();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if let Ok(r) = state.jobs.get(&j1).await {
                if matches!(r.status, JobStatus::Finished | JobStatus::Error) {
                    j1_done = true;
                    break;
                }
            }
        }
        assert!(j1_done, "j1 should reach a terminal state once the gate is released");

        // Permit is now free → j2 can start.
        assert_eq!(
            start_job(&app, &j2).await,
            202,
            "permit must be released after the first job completes"
        );
        gate.notify_waiters(); // let j2 drain
    }

    #[tokio::test(flavor = "current_thread")]
    async fn patch_job_persists_title_and_description() {
        let state = AppStateBuilder::new().build();
        let app = Router::new().merge(router()).with_state(state.clone());
        let id = create_job(&app).await;

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri(format!("/jobs/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"title":"My NDVI job","description":"Wien composite"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        // GET the job and confirm the fields persisted.
        let rec = state.jobs.get(&id).await.unwrap();
        assert_eq!(rec.title.as_deref(), Some("My NDVI job"));
        assert_eq!(rec.description.as_deref(), Some("Wien composite"));

        // PATCH on a missing job → 404.
        let missing = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri("/jobs/job-deadbeef")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"title":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_results_cancels_running_job() {
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let exec = std::sync::Arc::new(SlowExecutor { gate: gate.clone() })
            as std::sync::Arc<dyn crate::executor::ProcessGraphExecutor>;
        let state = AppStateBuilder::new()
            .with_executor(exec)
            .with_job_concurrency(4)
            .build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let j = create_job(&app).await;
        assert_eq!(start_job(&app, &j).await, 202);
        // Wait until it is running (blocked in run_sync) and registered.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if state.job_registry.in_flight() >= 1 {
                break;
            }
        }

        // DELETE /jobs/{id}/results = cancel.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/jobs/{j}/results"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);

        // The job transitions to Canceled.
        let mut canceled = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if let Ok(r) = state.jobs.get(&j).await {
                if r.status == JobStatus::Canceled {
                    canceled = true;
                    break;
                }
            }
        }
        gate.notify_waiters();
        assert!(canceled, "DELETE /results must transition the job to Canceled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_processing_is_idempotent_rejects_double_start() {
        // A second start of an already-running job must 409, not spawn a
        // second runner (concurrency is high enough that the permit is not
        // the limiter — this asserts the status guard specifically).
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let exec = std::sync::Arc::new(SlowExecutor { gate: gate.clone() })
            as std::sync::Arc<dyn crate::executor::ProcessGraphExecutor>;
        let state = AppStateBuilder::new()
            .with_executor(exec)
            .with_job_concurrency(4)
            .build();
        let app = Router::new().merge(router()).with_state(state.clone());

        let j = create_job(&app).await;
        assert_eq!(start_job(&app, &j).await, 202);
        // Wait until the runner has marked it Running (blocked in run_sync).
        let mut running = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if let Ok(r) = state.jobs.get(&j).await {
                if matches!(r.status, JobStatus::Running | JobStatus::Queued) {
                    running = true;
                    break;
                }
            }
        }
        assert!(running, "job should be Running/Queued after first start");
        assert_eq!(
            start_job(&app, &j).await,
            409,
            "re-starting a running job must 409 JobLocked, not double-run"
        );
        gate.notify_waiters();
    }
}
