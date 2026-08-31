//! HTTP API (Axum).
//!
//! Every listing is paginated server-side. Every source-level response
//! carries completeness. Errors are JSON `{ "error": ..., "kind": ... }`.

use crate::admission::{AdmissionError, Expensive};
use crate::api_json::ApiJson;
use crate::export;
use crate::scanner::{start_scan, ScanProgressView, StartScanError};
use crate::state::AppState;
use crate::web::WebAssets;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use eidos_catalog::{
    ChildSort, ChildrenPage, ChildrenResult, DirectoryAggregate, EntryRecord, ErrorRecord,
    ExtensionCount, NewSource, ObjectRecord, PolicyDecisionRecord, ScanGenerationRecord,
    SourceCounts, SourceRecord,
};
use eidos_domain::{ObjectId, SourceCompleteness, SourceId, SourceKind};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use ts_rs::TS;

/// API router plus, optionally, the web UI served from a directory on disk.
/// `None` is API only; see [`router_with_web`] for the embedded UI.
pub fn router(state: Arc<AppState>, web_dir: Option<&std::path::Path>) -> Router {
    let web = match web_dir {
        Some(dir) => WebAssets::Directory(dir.to_path_buf()),
        None => WebAssets::Disabled,
    };
    router_with_web(state, &web)
}

/// API router with the web UI taken from `web`.
pub fn router_with_web(state: Arc<AppState>, web: &WebAssets) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/sources", get(list_sources).post(add_source))
        .route("/sources/{id}", get(get_source))
        .route("/sources/{id}/scan", post(scan_source))
        .route("/sources/{id}/scan/cancel", post(cancel_scan))
        .route("/sources/{id}/errors", get(source_errors))
        .route("/sources/{id}/exclusions", get(source_exclusions))
        .route("/sources/{id}/generations", get(source_generations))
        .route("/sources/{id}/content", post(set_content_policy))
        .route("/activity", get(activity))
        .route("/objects/{id}", get(get_object))
        .route("/objects/{id}/children", get(children))
        .route("/objects/{id}/extensions", get(extensions))
        .route("/objects/{id}/archive", get(archive))
        .route(
            "/objects/{id}/content",
            get(crate::content_preview::object_content),
        )
        .route("/sources/{id}/archives", post(requeue_archives))
        .merge(crate::content_control::routes())
        .merge(crate::retry_api::routes())
        .merge(crate::interactions_api::routes())
        .merge(crate::fleet_api::routes())
        .route("/resolve", get(resolve))
        .route("/search", post(search).get(search_get))
        .route(
            "/search/export",
            get(export::export_get).post(export::export_post),
        )
        .route("/search/parse", get(search_parse))
        .route("/index", get(index_status))
        // Every API body is a small JSON document; nothing here streams
        // uploads. hyper's HTTP/1 defaults bound the request line and the
        // header block, this bounds the body.
        .layer(DefaultBodyLimit::max(
            state.admission.config().max_body_bytes,
        ))
        .with_state(state);
    let app = Router::new().nest("/api", api);
    let app = crate::web::mount(app, web);
    app.layer(tower_http::trace::TraceLayer::new_for_http())
}

// ----- errors --------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    /// Emitted as `Retry-After` (seconds) when load is shed.
    retry_after_s: Option<u64>,
    /// Extra machine-readable fields merged into the error body.
    details: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, TS)]
pub(crate) struct ApiErrorBody {
    error: String,
    kind: String,
}

impl ApiError {
    fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after_s: None,
            details: None,
        }
    }
    pub(crate) fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", msg)
    }
    pub(crate) fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", msg)
    }
    pub(crate) fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", msg)
    }
    pub(crate) fn unavailable(msg: impl Into<String>, retry_after_s: Option<u64>) -> Self {
        let mut e = Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", msg);
        e.retry_after_s = retry_after_s;
        e
    }
    pub(crate) fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg)
    }
    /// Load shed by a bound of our own rather than by the admission gate.
    pub(crate) fn busy(msg: impl Into<String>, retry_after_s: u64) -> Self {
        Self {
            retry_after_s: Some(retry_after_s),
            ..Self::new(StatusCode::SERVICE_UNAVAILABLE, "busy", msg)
        }
    }
    /// Replace the stable error code clients switch on.
    pub(crate) fn with_kind(mut self, kind: &'static str) -> Self {
        self.kind = kind;
        self
    }
    /// Merge an object of extra fields into the error body.
    pub(crate) fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl From<AdmissionError> for ApiError {
    fn from(e: AdmissionError) -> Self {
        let message = e.to_string();
        match e {
            // Load shedding: the request never ran, so retrying is safe.
            AdmissionError::Busy { retry_after_s, .. } => ApiError {
                retry_after_s: Some(retry_after_s),
                ..ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "busy", message)
            },
            // The response gave up; the blocking work runs to completion and
            // releases its permit then, so a retry may still find the gate
            // full for a while.
            AdmissionError::Timeout { .. } => ApiError {
                retry_after_s: Some(1),
                ..ApiError::new(StatusCode::GATEWAY_TIMEOUT, "timeout", message)
            },
            AdmissionError::Failed(_) => ApiError::internal(message),
        }
    }
}

impl From<eidos_catalog::CatalogError> for ApiError {
    fn from(e: eidos_catalog::CatalogError) -> Self {
        match e {
            eidos_catalog::CatalogError::NotFound(m) => ApiError::not_found(m),
            eidos_catalog::CatalogError::InvalidState(m) => ApiError::conflict(m),
            other => ApiError::internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let base = ApiErrorBody {
            error: self.message,
            kind: self.kind.into(),
        };
        let serde_json::Value::Object(mut body) =
            serde_json::to_value(base).expect("API error body is serializable")
        else {
            unreachable!("API error body is an object")
        };
        if let Some(serde_json::Value::Object(extra)) = self.details {
            body.extend(extra);
        }
        let body = ApiJson(serde_json::Value::Object(body));
        match self.retry_after_s {
            Some(s) => (self.status, [(header::RETRY_AFTER, s.to_string())], body).into_response(),
            None => (self.status, body).into_response(),
        }
    }
}

pub(crate) type ApiResult<T> = Result<ApiJson<T>, ApiError>;

const MAX_SHORT_BLOCKING_OPERATIONS: usize = 16;
static SHORT_BLOCKING_OPERATIONS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

/// Run a short synchronous catalog/configuration operation without occupying
/// an async runtime worker. Long query and browse operations use the bounded
/// admission gate instead; this is for operator mutations and lookups.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    let admission = SHORT_BLOCKING_OPERATIONS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_SHORT_BLOCKING_OPERATIONS)))
        .clone();
    let permit = admission
        .try_acquire_owned()
        .map_err(|_| ApiError::busy("operator work is at capacity", 1))?;
    tokio::task::spawn_blocking(move || {
        // Keep the permit with the blocking work even if its HTTP request is
        // cancelled; spawn_blocking tasks cannot be aborted once running.
        let _permit = permit;
        f()
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
}

// ----- health --------------------------------------------------------------

/// On-disk footprint of the durable stores, computed lazily and cached
/// briefly by [`AppState::storage`].
#[derive(Debug, Clone, Copy, Default, Serialize, TS)]
pub struct StorageView {
    /// `catalog.db` plus its WAL and shared-memory files.
    pub catalog_db_bytes: u64,
    /// The name/metadata (catalog) index directory.
    pub catalog_index_bytes: u64,
    /// The content (chunk) index directory.
    pub content_index_bytes: u64,
}

#[derive(Serialize, TS)]
pub(crate) struct Health {
    version: &'static str,
    schema_version: u32,
    host: String,
    uptime_s: u64,
    catalog_path: String,
    sources: usize,
    running_scans: usize,
    /// Hard cap on rows a single `/api/search/export` may emit.
    export_max_rows: u64,
    /// Whether content extraction is running, paused, or draining, and how
    /// complete content search is. Derived by
    /// [`crate::content_control::content_status`], the same function
    /// `/api/activity` and the CLI use, so a health check and the Activity
    /// page can never disagree about the pipeline.
    content_status: crate::content_control::ContentStatusView,
    /// On-disk footprint of the catalog and indexes.
    storage: StorageView,
}

async fn health(State(st): State<Arc<AppState>>) -> ApiResult<Health> {
    // Catalog reads and the (cached) storage walk are blocking work; keep
    // them off the async runtime workers so health stays answerable while
    // the blocking pool is busy elsewhere.
    let view = tokio::task::spawn_blocking(move || -> Result<Health, ApiError> {
        let sources = st.catalog.list_sources()?;
        let running = st
            .scans
            .lock()
            .values()
            .filter(|p| !p.is_finished())
            .count();
        Ok(Health {
            version: env!("CARGO_PKG_VERSION"),
            schema_version: eidos_domain::SCHEMA_VERSION,
            host: st.host_name.clone(),
            uptime_s: st.started_at.elapsed().as_secs(),
            catalog_path: st.catalog.path().display().to_string(),
            sources: sources.len(),
            running_scans: running,
            export_max_rows: st.export.max_rows,
            content_status: crate::content_control::content_status(&st),
            storage: st.storage(),
        })
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(ApiJson(view))
}

// ----- sources -------------------------------------------------------------

#[derive(Serialize, TS)]
#[ts(optional_fields)]
pub struct SourceView {
    pub source: SourceRecord,
    pub counts: SourceCounts,
    pub completeness: SourceCompleteness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan: Option<ScanProgressView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<crate::watcher::WatcherView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_deferred: Option<crate::state::ReconciliationDeferral>,
}

pub(crate) fn source_view(st: &AppState, s: SourceRecord) -> Result<SourceView, ApiError> {
    let counts = st.catalog.source_counts(s.id)?;
    let listing_errors = st.catalog.published_listing_errors(s.id)?;
    let mut completeness = if s.kind.is_remote() {
        st.catalog.source_completeness(s.id)?
    } else {
        eidos_catalog::read::completeness_from(&s, &counts, listing_errors)
    };
    let scan = st.scan_progress(s.id).map(|p| p.view());
    let watcher = st.watcher_status(s.id).map(|w| w.view());
    let reconciliation_deferred = st.reconciliation_deferral(s.id);
    // A stored checkpoint only means "live" while a watcher is actually
    // consuming it.
    if !s.kind.is_remote()
        && completeness.freshness == eidos_domain::Freshness::Live
        && !watcher.as_ref().is_some_and(|w| w.live)
    {
        completeness.freshness = eidos_domain::Freshness::Unknown;
    }
    Ok(SourceView {
        source: s,
        counts,
        completeness,
        scan,
        watcher,
        reconciliation_deferred,
    })
}

async fn list_sources(State(st): State<Arc<AppState>>) -> ApiResult<Vec<SourceView>> {
    let st2 = st.clone();
    let views = st
        .admission
        .run(
            Expensive::Counts,
            move || -> Result<Vec<SourceView>, ApiError> {
                st2.catalog
                    .list_sources()?
                    .into_iter()
                    .map(|s| source_view(&st2, s))
                    .collect()
            },
        )
        .await??;
    Ok(ApiJson(views))
}

#[derive(Deserialize, TS)]
#[ts(optional_fields = nullable)]
pub(crate) struct AddSourceBody {
    name: String,
    root_path: String,
    #[serde(default)]
    kind: Option<SourceKind>,
    #[serde(default)]
    aliases: Vec<String>,
    /// Start a scan immediately.
    #[serde(default)]
    scan: bool,
}

async fn add_source(
    State(st): State<Arc<AppState>>,
    Json(body): Json<AddSourceBody>,
) -> Result<(StatusCode, ApiJson<SourceView>), ApiError> {
    if body.name.trim().is_empty() || body.root_path.trim().is_empty() {
        return Err(ApiError::bad_request("name and root_path are required"));
    }
    let root_path = eidos_scanner::normalize_root(body.root_path.trim());
    let root = std::path::Path::new(&root_path);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "root_path is not an accessible directory: {root_path}"
        )));
    }
    let kind = match body.kind {
        Some(SourceKind::Remote) => {
            return Err(ApiError::bad_request(
                "remote sources are created by fleet replication, not by hand",
            ))
        }
        Some(k) => k,
        None => match st.lister.volume_info(root) {
            Ok(v) => v.source_kind(),
            Err(_) => eidos_scanner::GENERIC_SOURCE_KIND,
        },
    };
    if st.catalog.find_source_by_name(&body.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "source '{}' already exists",
            body.name
        )));
    }
    let id = st.catalog.add_source(&NewSource {
        host_id: st.host_id,
        name: body.name.clone(),
        kind,
        root_path: root_path.clone(),
        aliases: body.aliases.clone(),
    })?;
    if let Ok(v) = st.lister.volume_info(root) {
        st.catalog.upsert_volume(st.host_id, id, &v)?;
    }
    if body.scan {
        start_scan(&st, id).map_err(start_scan_api_error)?;
    }
    let s = st
        .catalog
        .get_source(id)?
        .ok_or_else(|| ApiError::not_found("source vanished"))?;
    Ok((StatusCode::CREATED, ApiJson(source_view(&st, s)?)))
}

#[derive(Serialize, TS)]
pub(crate) struct SourceDetail {
    #[serde(flatten)]
    view: SourceView,
    generations: Vec<ScanGenerationRecord>,
    root_aggregate: Option<DirectoryAggregate>,
    exclusions: Vec<ExclusionRow>,
}

#[derive(Serialize, TS)]
pub(crate) struct ExclusionRow {
    stage: String,
    reason: String,
    count: u64,
    bytes: u64,
}

async fn get_source(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<SourceDetail> {
    let sid = SourceId(id);
    let gate = st.admission.clone();
    let detail = gate
        .run(
            Expensive::Counts,
            move || -> Result<SourceDetail, ApiError> {
                let s = st
                    .catalog
                    .get_source(sid)?
                    .ok_or_else(|| ApiError::not_found(format!("source {sid}")))?;
                let root_aggregate = match s.root_object_id {
                    Some(r) => st.catalog.directory_aggregate(r)?,
                    None => None,
                };
                let view = source_view(&st, s)?;
                let generations = st.catalog.list_generations(sid, 20)?;
                let exclusions = st
                    .catalog
                    .exclusion_summary(sid)?
                    .into_iter()
                    .map(|(stage, reason, count, bytes)| ExclusionRow {
                        stage,
                        reason,
                        count,
                        bytes,
                    })
                    .collect();
                Ok(SourceDetail {
                    view,
                    generations,
                    root_aggregate,
                    exclusions,
                })
            },
        )
        .await??;
    Ok(ApiJson(detail))
}

async fn scan_source(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<(StatusCode, ApiJson<ScanProgressView>), ApiError> {
    let sid = SourceId(id);
    let result = blocking(move || Ok(start_scan(&st, sid))).await?;
    match result {
        Ok(p) => Ok((StatusCode::ACCEPTED, ApiJson(p.view()))),
        Err(error) => Err(start_scan_api_error(error)),
    }
}

fn start_scan_api_error(error: StartScanError) -> ApiError {
    match error {
        StartScanError::AlreadyRunning => ApiError::conflict("scan already running"),
        StartScanError::RemoteSource => {
            ApiError::bad_request("a replicated source is scanned on its origin node, not here")
        }
        StartScanError::Catalog(error) => error.into(),
        StartScanError::Spawn(error) => {
            ApiError::internal(format!("the scan could not be started: {error}"))
        }
    }
}

async fn cancel_scan(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<ScanProgressView> {
    let sid = SourceId(id);
    let p = st
        .scan_progress(sid)
        .ok_or_else(|| ApiError::not_found("no scan running"))?;
    p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(ApiJson(p.view()))
}

#[derive(Deserialize, TS)]
pub(crate) struct ErrorsQuery {
    #[serde(default)]
    include_resolved: bool,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    200
}

async fn source_errors(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<ErrorsQuery>,
) -> ApiResult<Vec<ErrorRecord>> {
    Ok(ApiJson(st.catalog.list_errors(
        Some(SourceId(id)),
        q.include_resolved,
        q.limit.min(5000),
    )?))
}

async fn source_exclusions(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Vec<ExclusionRow>> {
    Ok(ApiJson(
        st.catalog
            .exclusion_summary(SourceId(id))?
            .into_iter()
            .map(|(stage, reason, count, bytes)| ExclusionRow {
                stage,
                reason,
                count,
                bytes,
            })
            .collect(),
    ))
}

async fn source_generations(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Vec<ScanGenerationRecord>> {
    Ok(ApiJson(st.catalog.list_generations(SourceId(id), 50)?))
}

// ----- objects -------------------------------------------------------------

#[derive(Serialize, TS)]
pub(crate) struct ObjectDetail {
    object: ObjectRecord,
    path: Option<String>,
    entries: Vec<EntryRecord>,
    aggregate: Option<DirectoryAggregate>,
    policy: Vec<PolicyDecisionRecord>,
    source: SourceCompleteness,
}

async fn get_object(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<ObjectDetail> {
    let oid = ObjectId(id);
    let gate = st.admission.clone();
    let detail = gate
        .run(
            Expensive::Browse,
            move || -> Result<ObjectDetail, ApiError> {
                let object = st
                    .catalog
                    .get_object(oid)?
                    .ok_or_else(|| ApiError::not_found(format!("object {oid}")))?;
                let path = st.catalog.render_path(oid)?;
                let entries = st.catalog.entries_for_object(oid)?;
                let aggregate = if object.kind.is_directory_like() {
                    st.catalog.directory_aggregate(oid)?
                } else {
                    None
                };
                let policy = st.catalog.policy_decisions(oid)?;
                let source = st.catalog.source_completeness(object.source_id)?;
                Ok(ObjectDetail {
                    object,
                    path,
                    entries,
                    aggregate,
                    policy,
                    source,
                })
            },
        )
        .await??;
    Ok(ApiJson(detail))
}

#[derive(Deserialize, TS)]
pub(crate) struct ChildrenQuery {
    #[serde(default)]
    sort: ChildSort,
    #[serde(default)]
    desc: bool,
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_children_limit")]
    limit: u32,
    #[serde(default = "default_true")]
    hidden: bool,
}

fn default_children_limit() -> u32 {
    200
}
fn default_true() -> bool {
    true
}

#[derive(Serialize, TS)]
pub(crate) struct ChildrenView {
    #[serde(flatten)]
    result: ChildrenResult,
    path: Option<String>,
    parent_id: Option<ObjectId>,
    source: SourceCompleteness,
}

async fn children(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<ChildrenQuery>,
) -> ApiResult<ChildrenView> {
    let oid = ObjectId(id);
    let page = ChildrenPage {
        sort: q.sort,
        descending: q.desc,
        offset: q.offset,
        limit: q.limit.clamp(1, 2000),
        include_hidden: q.hidden,
    };
    let gate = st.admission.clone();
    let view = gate
        .run(
            Expensive::Browse,
            move || -> Result<ChildrenView, ApiError> {
                let object = st
                    .catalog
                    .get_object(oid)?
                    .ok_or_else(|| ApiError::not_found(format!("object {oid}")))?;
                if !object.kind.is_directory_like() {
                    return Err(ApiError::bad_request("object is not a directory"));
                }
                let result = st.catalog.list_children(oid, &page)?;
                let path = st.catalog.render_path(oid)?;
                let parent_id = st
                    .catalog
                    .entries_for_object(oid)?
                    .first()
                    .and_then(|e| e.parent_id);
                let source = st.catalog.source_completeness(object.source_id)?;
                Ok(ChildrenView {
                    result,
                    path,
                    parent_id,
                    source,
                })
            },
        )
        .await??;
    Ok(ApiJson(view))
}

#[derive(Deserialize, TS)]
pub(crate) struct ArchiveQuery {
    /// List the children of this virtual directory (`""` for the root).
    parent: Option<String>,
    /// List every member whose path starts with this prefix.
    prefix: Option<String>,
    #[serde(default)]
    offset: u32,
    #[serde(default = "default_member_limit")]
    limit: u32,
}
fn default_member_limit() -> u32 {
    200
}

#[derive(Serialize, TS)]
pub struct ArchiveView {
    pub object_id: ObjectId,
    pub path: Option<String>,
    pub record: eidos_catalog::archive::ArchiveRecord,
    pub members: Vec<eidos_catalog::archive::ArchiveMember>,
    pub total: u64,
    pub query: eidos_catalog::archive::MemberQuery,
}

/// Manifest of a container object: its record plus one page of members.
async fn archive(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<ArchiveQuery>,
) -> ApiResult<ArchiveView> {
    let oid = ObjectId(id);
    let query = eidos_catalog::archive::MemberQuery {
        parent: q.parent,
        prefix: q.prefix,
        offset: q.offset,
        limit: q.limit.clamp(1, 5000),
    };
    let gate = st.admission.clone();
    let view = gate
        .run(
            Expensive::Archive,
            move || -> Result<ArchiveView, ApiError> {
                let record = st.catalog.archive_record(oid)?.ok_or_else(|| {
                    ApiError::not_found(format!("object {oid} has no archive manifest"))
                })?;
                let (members, total) = st.catalog.archive_members(oid, &query)?;
                Ok(ArchiveView {
                    object_id: oid,
                    path: st.catalog.render_path(oid)?,
                    record,
                    members,
                    total,
                    query,
                })
            },
        )
        .await??;
    Ok(ApiJson(view))
}

#[derive(Serialize, TS)]
pub struct RequeueView {
    pub queued: u64,
}

/// Queue manifest jobs for every container file of the source that has no
/// manifest for its current generation.
async fn requeue_archives(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<RequeueView> {
    let sid = SourceId(id);
    let gate = st.admission.clone();
    let queued = gate
        .run(Expensive::Maintenance, move || -> Result<u64, ApiError> {
            st.catalog
                .get_source(sid)?
                .ok_or_else(|| ApiError::not_found(format!("source {sid}")))?;
            Ok(st.catalog.requeue_archives(Some(sid))?)
        })
        .await??;
    Ok(ApiJson(RequeueView { queued }))
}

#[derive(Deserialize, TS)]
pub(crate) struct LimitQuery {
    #[serde(default = "default_ext_limit")]
    limit: u32,
}
fn default_ext_limit() -> u32 {
    50
}

async fn extensions(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Vec<ExtensionCount>> {
    let gate = st.admission.clone();
    let counts = gate
        .run(Expensive::Counts, move || {
            st.catalog.extension_counts(ObjectId(id), q.limit.min(1000))
        })
        .await??;
    Ok(ApiJson(counts))
}

#[derive(Deserialize, TS)]
pub(crate) struct ResolveQuery {
    source: i64,
    #[serde(default)]
    path: String,
}

#[derive(Serialize, TS)]
pub(crate) struct ResolveView {
    object_id: ObjectId,
    path: Option<String>,
}

// ----- search --------------------------------------------------------------

/// Body of `POST /api/search`. Either `q` (syntax) or `query` (AST) must be
/// present; when both are given the AST wins.
#[derive(Deserialize, TS)]
#[ts(optional_fields = nullable)]
pub(crate) struct SearchBody {
    #[serde(default)]
    pub(crate) q: Option<String>,
    #[serde(default)]
    pub(crate) query: Option<eidos_domain::Query>,
    #[serde(default)]
    pub(crate) mode: eidos_domain::ResultMode,
    #[serde(default)]
    pub(crate) sort: eidos_domain::Sort,
    #[serde(default = "default_search_limit")]
    pub(crate) limit: u32,
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    #[serde(default)]
    pub(crate) explain: bool,
    #[serde(default)]
    pub(crate) facets: Vec<eidos_domain::FacetRequest>,
    #[serde(default)]
    pub(crate) include_retired: bool,
    /// Total-count policy: `auto` (default), `exact`, or `none`.
    #[serde(default)]
    pub(crate) count: eidos_domain::CountPolicy,
}

fn default_search_limit() -> u32 {
    50
}

#[derive(Serialize, TS)]
pub(crate) struct SearchView {
    #[serde(flatten)]
    response: eidos_domain::SearchResponse,
    /// The compiled AST (editable interpretation).
    query: eidos_domain::Query,
    /// Readable rendering of the AST in query syntax.
    rendered: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

pub(crate) fn build_request(
    body: SearchBody,
) -> Result<(eidos_domain::SearchRequest, Vec<String>), ApiError> {
    let (query, notes) = match (body.query, body.q) {
        (Some(q), _) => (q, Vec::new()),
        (None, Some(text)) => {
            let parsed =
                eidos_query::parse(&text).map_err(|e| ApiError::bad_request(e.to_string()))?;
            (parsed.query, parsed.notes)
        }
        (None, None) => (
            eidos_domain::Query::All,
            vec!["empty query matches everything".into()],
        ),
    };
    Ok((
        eidos_domain::SearchRequest {
            query,
            mode: body.mode,
            sort: body.sort,
            limit: body.limit,
            cursor: body.cursor,
            explain: body.explain,
            snippets: true,
            facets: body.facets,
            include_retired: body.include_retired,
            count: body.count,
        },
        notes,
    ))
}

async fn run_search(st: Arc<AppState>, body: SearchBody) -> ApiResult<SearchView> {
    let (req, notes) = build_request(body)?;
    let query = req.query.clone();
    let rendered = eidos_query::render(&query);
    let st2 = st.clone();
    let response = st
        .admission
        .run(Expensive::Search, move || {
            eidos_search::exec::search_with_content(
                &st2.index,
                Some(&st2.content_index),
                &st2.catalog,
                &req,
                &st2.exec_opts,
            )
        })
        .await?
        .map_err(search_error)?;
    Ok(ApiJson(SearchView {
        response,
        query,
        rendered,
        notes,
    }))
}

pub(crate) fn search_error(e: eidos_search::SearchError) -> ApiError {
    match e {
        eidos_search::SearchError::Query(q) => {
            ApiError::new(StatusCode::BAD_REQUEST, "query", q.to_string())
        }
        eidos_search::SearchError::Catalog(c) => c.into(),
        other => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "search",
            other.to_string(),
        ),
    }
}

async fn search(
    State(st): State<Arc<AppState>>,
    Json(body): Json<SearchBody>,
) -> ApiResult<SearchView> {
    run_search(st, body).await
}

#[derive(Deserialize, TS)]
pub(crate) struct SearchGetQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    mode: Option<eidos_domain::ResultMode>,
    #[serde(default)]
    sort: Option<eidos_domain::SortField>,
    #[serde(default)]
    desc: bool,
    #[serde(default = "default_search_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    explain: bool,
    /// Comma-separated facet fields.
    #[serde(default)]
    facets: String,
    #[serde(default)]
    count: eidos_domain::CountPolicy,
}

async fn search_get(
    State(st): State<Arc<AppState>>,
    Query(q): Query<SearchGetQuery>,
) -> ApiResult<SearchView> {
    let facets = q
        .facets
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            serde_json::from_value::<eidos_domain::FacetField>(serde_json::Value::String(
                s.to_string(),
            ))
            .ok()
        })
        .map(|field| eidos_domain::FacetRequest { field, limit: 20 })
        .collect();
    run_search(
        st,
        SearchBody {
            q: Some(q.q),
            query: None,
            mode: q.mode.unwrap_or_default(),
            sort: eidos_domain::Sort {
                field: q.sort.unwrap_or_default(),
                descending: q.desc,
            },
            limit: q.limit,
            cursor: q.cursor,
            explain: q.explain,
            facets,
            include_retired: false,
            count: q.count,
        },
    )
    .await
}

#[derive(Deserialize, TS)]
pub(crate) struct ParseQuery {
    #[serde(default)]
    q: String,
}

#[derive(Serialize, TS)]
pub(crate) struct ParseView {
    query: eidos_domain::Query,
    rendered: String,
    notes: Vec<String>,
    needs_content: bool,
}

async fn search_parse(Query(q): Query<ParseQuery>) -> ApiResult<ParseView> {
    let parsed = eidos_query::parse(&q.q).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let rendered = eidos_query::render(&parsed.query);
    let needs_content = parsed.query.needs_content();
    Ok(ApiJson(ParseView {
        query: parsed.query,
        rendered,
        notes: parsed.notes,
        needs_content,
    }))
}

#[derive(Deserialize, TS)]
#[ts(optional_fields = nullable)]
pub(crate) struct ContentPolicyBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    concurrency: Option<u32>,
}

async fn set_content_policy(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<ContentPolicyBody>,
) -> ApiResult<SourceView> {
    let sid = SourceId(id);
    let view = blocking(move || {
        let s = st
            .catalog
            .get_source(sid)?
            .ok_or_else(|| ApiError::not_found(format!("source {id}")))?;
        if s.kind == SourceKind::Remote {
            return Err(ApiError::bad_request(
                "a replicated source carries no content here; set its policy on the origin node",
            ));
        }
        let enabled = body.enabled.unwrap_or(s.content_enabled);
        let concurrency = body
            .concurrency
            .unwrap_or(s.content_concurrency)
            .clamp(1, 64);
        st.catalog.set_content_policy(sid, enabled, concurrency)?;
        st.content_budgets().set(sid, concurrency);
        let s = st
            .catalog
            .get_source(sid)?
            .ok_or_else(|| ApiError::not_found(format!("source {id}")))?;
        source_view(&st, s)
    })
    .await?;
    Ok(ApiJson(view))
}

#[derive(Serialize, TS)]
#[ts(optional_fields)]
pub struct ActivitySourceView {
    pub source_id: SourceId,
    pub name: String,
    pub state: eidos_domain::SourceState,
    pub content_enabled: bool,
    pub content_concurrency: u32,
    /// Units of `content_concurrency` reserved by workers right now.
    pub content_reserved: u32,
    /// High-water mark of `content_reserved` since the service started.
    pub content_peak_reserved: u32,
    pub jobs_queued: u64,
    pub jobs_running: u64,
    pub jobs_failed: u64,
    /// Estimated bytes behind the failed jobs of this source.
    pub jobs_failed_bytes: u64,
    pub content_states: std::collections::BTreeMap<String, u64>,
    pub content_bytes_indexed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_deferred: Option<crate::state::ReconciliationDeferral>,
}

#[derive(Serialize, TS)]
pub struct ActivityView {
    /// Process-level switch only. Kept for compatibility within this schema
    /// version; `content_status.enabled` is the same bit, and
    /// `content_status` carries the operator pause and drain state that this
    /// boolean cannot express.
    pub content_enabled: bool,
    /// Extraction flow, operator pause, and content-search completeness.
    pub content_status: crate::content_control::ContentStatusView,
    /// Durable scan/job repairs performed synchronously at this process start.
    pub startup_recovery: crate::state::StartupRecovery,
    pub jobs: eidos_catalog::jobs::JobCounts,
    pub content: eidos_catalog::content::ContentStats,
    pub archives: eidos_catalog::archive::ArchiveStats,
    pub workers: crate::content_workers::ContentWorkersView,
    pub follower: crate::follower::FollowerView,
    pub content_index_documents: u64,
    /// Rebuild-from-stored-chunks state of the content index.
    pub content_rebuild: eidos_search::content::RebuildStatus,
    /// Admission-control gauges and totals for expensive API operations.
    pub admission: crate::admission::AdmissionView,
    /// Process-wide catalog writer wait time, including scan batch handoffs.
    pub catalog_writer: eidos_catalog::CatalogWriterStats,
    /// On-disk footprint of the catalog and indexes.
    pub storage: StorageView,
    pub sources: Vec<ActivitySourceView>,
    pub recent_failures: Vec<eidos_catalog::jobs::JobRecord>,
}

async fn activity(State(st): State<Arc<AppState>>) -> ApiResult<ActivityView> {
    let st2 = st.clone();
    let view = tokio::task::spawn_blocking(move || -> Result<ActivityView, ApiError> {
        let by_source = st2
            .catalog
            .jobs_by_source(eidos_domain::JobStage::ContentText)?;
        let failed_by_source = st2
            .catalog
            .failed_work_by_source(eidos_domain::JobStage::ContentText)?;
        let mut sources = Vec::new();
        for s in st2.catalog.list_sources()? {
            let q = by_source.get(&s.id).copied().unwrap_or((0, 0));
            let failed = failed_by_source.get(&s.id).copied().unwrap_or((0, 0));
            let stats = st2.catalog.content_stats(Some(s.id))?;
            sources.push(ActivitySourceView {
                source_id: s.id,
                name: s.name.clone(),
                state: s.state,
                content_enabled: s.content_enabled,
                content_concurrency: s.content_concurrency,
                content_reserved: st2.content_budgets().reserved(s.id),
                content_peak_reserved: st2.content_budgets().peak_reserved(s.id),
                jobs_queued: q.0,
                jobs_running: q.1,
                jobs_failed: failed.0,
                jobs_failed_bytes: failed.1,
                content_states: st2.catalog.content_state_counts(s.id)?,
                content_bytes_indexed: stats.indexed_bytes,
                reconciliation_deferred: st2.reconciliation_deferral(s.id),
            });
        }
        Ok(ActivityView {
            content_enabled: st2
                .content_enabled
                .load(std::sync::atomic::Ordering::Relaxed),
            content_status: crate::content_control::content_status(&st2),
            startup_recovery: st2.startup_recovery,
            jobs: st2.catalog.job_counts(None)?,
            content: st2.catalog.content_stats(None)?,
            archives: st2.catalog.archive_stats(None)?,
            workers: st2.content_workers.view(st2.content_index.uncommitted()),
            follower: st2.follower.view(&st2),
            content_index_documents: st2.content_index.num_docs(),
            content_rebuild: st2.content_index.rebuild_status(),
            admission: st2.admission.view(),
            catalog_writer: st2.catalog.writer_stats(),
            storage: st2.storage(),
            sources,
            recent_failures: st2.catalog.recent_failed_jobs(20)?,
        })
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))??;
    Ok(ApiJson(view))
}

#[derive(Serialize, TS)]
pub(crate) struct IndexStatus {
    follower: crate::follower::FollowerView,
    /// Admission-control gauges and totals for expensive API operations.
    admission: crate::admission::AdmissionView,
    sources: Vec<IndexSourceState>,
}

#[derive(Serialize, TS)]
pub(crate) struct IndexSourceState {
    source_id: SourceId,
    name: String,
    published_generation: Option<i64>,
    indexed_generation: Option<i64>,
    documents: u64,
    in_sync: bool,
}

async fn index_status(State(st): State<Arc<AppState>>) -> ApiResult<IndexStatus> {
    let mut sources = Vec::new();
    for s in st.catalog.list_sources()? {
        let p = st
            .catalog
            .projection_source(eidos_search::PROJECTION_NAME, s.id)?;
        sources.push(IndexSourceState {
            source_id: s.id,
            name: s.name.clone(),
            published_generation: s.published_generation,
            indexed_generation: p.as_ref().map(|p| p.generation),
            documents: p.as_ref().map(|p| p.documents).unwrap_or(0),
            in_sync: p
                .as_ref()
                .map(|p| Some(p.generation) == s.published_generation)
                .unwrap_or(false),
        });
    }
    Ok(ApiJson(IndexStatus {
        follower: st.follower.view(&st),
        admission: st.admission.view(),
        sources,
    }))
}

async fn resolve(
    State(st): State<Arc<AppState>>,
    Query(q): Query<ResolveQuery>,
) -> ApiResult<ResolveView> {
    let id = st
        .catalog
        .resolve_relative(SourceId(q.source), &q.path)?
        .ok_or_else(|| ApiError::not_found(format!("path not in catalog: {}", q.path)))?;
    Ok(ApiJson(ResolveView {
        object_id: id,
        path: st.catalog.render_path(id)?,
    }))
}
