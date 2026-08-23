//! Operator retry endpoints for failed jobs.
//!
//! `POST /api/jobs/{id}/retry` requeues one failed job;
//! `POST /api/sources/{id}/content/retry` requeues a source's failed content
//! work — jobs left `failed`, and objects whose extraction failed for good —
//! optionally filtered by failure class and error prefix, and can preview
//! instead of acting. Both answer with
//! the same additive [`eidos_catalog::retry::RetryReport`] shape:
//! `{ accepted, skipped, rejected, bytes, ... }`.

use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use eidos_catalog::retry::{RetryReport, RetrySelector};
use eidos_domain::{FailureClass, JobId, JobStage, SourceId};
use serde::Deserialize;
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/jobs/{id}/retry", post(retry_job))
        .route("/sources/{id}/content/retry", post(retry_source_content))
}

#[derive(Debug, Default, Deserialize)]
pub struct RetryBody {
    /// Only jobs that failed with this class (`deterministic`, `corrupt`, …).
    #[serde(default)]
    pub class: Option<String>,
    /// Only jobs whose `last_error` starts with this text.
    #[serde(default)]
    pub reason_prefix: Option<String>,
    /// Count what would be retried without changing anything.
    #[serde(default)]
    pub preview: bool,
    /// Cap on jobs touched by one call.
    #[serde(default)]
    pub limit: Option<u32>,
}

fn parse_class(raw: Option<&str>) -> Result<Option<FailureClass>, ApiError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => FailureClass::parse(s)
            .map(Some)
            .ok_or_else(|| ApiError::bad_request(format!("unknown failure class '{s}'"))),
    }
}

async fn run(st: Arc<AppState>, sel: RetrySelector) -> ApiResult<RetryReport> {
    let report = tokio::task::spawn_blocking(move || st.catalog.retry_failed_jobs(&sel))
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))??;
    if !report.preview && report.accepted > 0 {
        tracing::info!(
            accepted = report.accepted,
            skipped = report.skipped,
            rejected = report.rejected,
            bytes = report.bytes,
            "operator requeued failed jobs"
        );
    }
    Ok(Json(report))
}

/// Requeue one failed job. A running job is rejected, not interrupted.
async fn retry_job(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    body: Option<Json<RetryBody>>,
) -> ApiResult<RetryReport> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let job = st
        .catalog
        .get_job(JobId(id))?
        .ok_or_else(|| ApiError::not_found(format!("job {id}")))?;
    let sel = RetrySelector {
        preview: body.preview,
        ..RetrySelector::job(job.id)
    };
    run(st, sel).await
}

/// Requeue a source's failed content jobs (or preview the effect).
async fn retry_source_content(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    body: Option<Json<RetryBody>>,
) -> ApiResult<RetryReport> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let sid = SourceId(id);
    st.catalog
        .get_source(sid)?
        .ok_or_else(|| ApiError::not_found(format!("source {sid}")))?;
    let sel = RetrySelector {
        class: parse_class(body.class.as_deref())?,
        reason_prefix: body.reason_prefix.filter(|p: &String| !p.trim().is_empty()),
        preview: body.preview,
        limit: body.limit,
        ..RetrySelector::source(sid, JobStage::ContentText)
    };
    run(st, sel).await
}
