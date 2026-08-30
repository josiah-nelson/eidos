//! Collector visibility and forwarding control.
//!
//! The collector is a separate daemon with a machine-wide control pipe;
//! nothing about it was visible in the service UI, which made "are nodes
//! uploading log bundles, and where?" unanswerable without a terminal on
//! the host. The service answers for the local daemon here: status (lanes
//! aside — `eidos observe` remains the deep view) and the forwarding
//! configuration (enabled, destination share, delivery hour).
//!
//! The pipe grants access to administrators and SYSTEM only. The service
//! runs as SYSTEM in production; a dev instance run unelevated reports the
//! denial as a state rather than an error.

use crate::api::{ApiError, ApiJson, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use ts_rs::TS;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/collector", get(status))
        .route("/collector/upload", post(set_upload))
}

/// Bundle forwarding as the daemon reports it.
#[derive(Debug, Clone, Default, serde::Serialize, TS)]
pub struct CollectorUploadView {
    pub enabled: bool,
    /// Directory the daily bundle is copied into (typically a UNC share).
    pub destination: String,
    /// Local hour 0-23 the delivery runs at or after.
    pub hour: u32,
    pub last_upload_unix_ns: Option<i64>,
    pub uploaded_total: u64,
    /// Bundles staged locally and not yet delivered.
    pub pending: u64,
    pub last_error: Option<String>,
}

/// The local collector daemon as seen over its control pipe.
#[derive(Debug, Clone, Default, serde::Serialize, TS)]
pub struct CollectorView {
    /// The daemon answered on its control pipe.
    pub available: bool,
    /// Why it did not: not installed or stopped, access denied, or a
    /// platform without a collector.
    pub detail: Option<String>,
    pub version: Option<String>,
    pub uptime_s: u64,
    pub spool_records: u64,
    pub capture_gaps: u64,
    pub upload: Option<CollectorUploadView>,
}

/// `POST /api/collector/upload`: fields present are changed, absent are kept.
#[derive(Debug, Clone, serde::Deserialize, TS)]
pub struct CollectorUploadBody {
    pub enabled: Option<bool>,
    pub destination: Option<String>,
    pub hour: Option<u32>,
}

async fn status(State(_st): State<Arc<AppState>>) -> ApiResult<CollectorView> {
    crate::api::blocking(move || Ok(ApiJson(query_status()))).await
}

async fn set_upload(
    State(_st): State<Arc<AppState>>,
    axum::Json(body): axum::Json<CollectorUploadBody>,
) -> ApiResult<CollectorView> {
    crate::api::blocking(move || {
        apply_upload(&body)?;
        Ok(ApiJson(query_status()))
    })
    .await
}

#[cfg(windows)]
fn query_status() -> CollectorView {
    use eidos_windows_collector::protocol::{Request, Response};
    match eidos_windows_collector::client::request(&Request::Status) {
        Ok(Response::Status { status }) => CollectorView {
            available: true,
            detail: None,
            version: Some(status.version.clone()),
            uptime_s: status.uptime_s,
            spool_records: status.spool.records,
            capture_gaps: status.capture_gaps as u64,
            upload: Some(CollectorUploadView {
                enabled: status.upload.enabled,
                destination: status.upload.destination.clone(),
                hour: status.upload.hour,
                last_upload_unix_ns: status.upload.last_upload_utc_ns,
                uploaded_total: status.upload.uploaded_total,
                pending: status.upload.pending,
                last_error: status.upload.last_error.clone(),
            }),
        },
        Ok(other) => unavailable(format!("unexpected collector response: {other:?}")),
        Err(e) => unavailable(collector_error(&e)),
    }
}

#[cfg(windows)]
fn apply_upload(body: &CollectorUploadBody) -> Result<(), ApiError> {
    use eidos_windows_collector::protocol::{Request, Response};
    let request = Request::SetUpload {
        enabled: body.enabled,
        destination: body.destination.clone(),
        hour: body.hour,
    };
    match eidos_windows_collector::client::request(&request) {
        Ok(Response::Accepted) => Ok(()),
        Ok(Response::Error { message }) => Err(ApiError::bad_request(message)),
        Ok(other) => Err(ApiError::internal(format!(
            "unexpected collector response: {other:?}"
        ))),
        Err(e) => Err(ApiError::unavailable(collector_error(&e), None)),
    }
}

/// The distinction operators actually need: not running vs. not allowed.
#[cfg(windows)]
fn collector_error(e: &anyhow::Error) -> String {
    let io = e
        .chain()
        .find_map(|c| c.downcast_ref::<std::io::Error>());
    match io.map(|io| io.kind()) {
        Some(std::io::ErrorKind::NotFound) => {
            "the collector service is not installed or not running on this host".into()
        }
        Some(std::io::ErrorKind::PermissionDenied) => {
            "access to the collector control pipe was denied (the pipe admits administrators and SYSTEM)"
                .into()
        }
        _ => format!("collector control pipe: {e:#}"),
    }
}

#[cfg(not(windows))]
fn query_status() -> CollectorView {
    unavailable("the collector runs on Windows hosts only".to_string())
}

#[cfg(not(windows))]
fn apply_upload(_body: &CollectorUploadBody) -> Result<(), ApiError> {
    Err(ApiError::unavailable(
        "the collector runs on Windows hosts only",
        None,
    ))
}

fn unavailable(detail: String) -> CollectorView {
    CollectorView {
        available: false,
        detail: Some(detail),
        ..Default::default()
    }
}
