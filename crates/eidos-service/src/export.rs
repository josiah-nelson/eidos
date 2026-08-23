//! Bounded, streaming export of a search result set.
//!
//! `GET|POST /api/search/export` runs the same query as `/api/search` and then
//! walks the result cursors server-side, emitting one page at a time. Nothing
//! larger than a single page is ever held in memory, and the walk stops as
//! soon as the client goes away: the response body owns the receiving end of a
//! bounded channel, so dropping it makes the producer's next send fail.
//!
//! Three formats share one row schema (see [`COLUMNS`]): RFC 4180 CSV, a
//! single JSON document, and newline-delimited JSON.

use crate::api::{build_request, search_error, ApiError, SearchBody};
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue};
use axum::response::Response;
use axum::Json;
use eidos_domain::{Hit, SearchRequest, SearchResponse, UnixNanos};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use ts_rs::TS;

/// Version tag carried by every JSON/NDJSON export envelope.
pub const EXPORT_SCHEMA: &str = "eidos-export/2";

/// Stable column order of the CSV export; also the field order of a row in
/// the JSON and NDJSON exports.
pub const COLUMNS: [&str; 14] = [
    "object_id",
    "entry_id",
    "source",
    "kind",
    "name",
    "path",
    "extension",
    "size",
    "allocated_size",
    "modified",
    "created",
    "content_state",
    "hard_link_count",
    "score",
];

/// Server-side bounds on an export. Configured through [`crate::ServiceConfig`].
#[derive(Debug, Clone, Copy)]
pub struct ExportLimits {
    /// Rows fetched per cursor step. Also the streaming chunk size.
    pub page_size: u32,
    /// Hard cap on exported rows; a request may ask for fewer, never more.
    pub max_rows: u64,
    /// Exports allowed to stream at once. Because an export holds at most one
    /// admission permit at a time, this is also the most of the admission gate
    /// exports can occupy — it is clamped below the gate's own concurrency so
    /// interactive queries always keep a slot.
    pub concurrency: usize,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            page_size: 500,
            max_rows: 100_000,
            concurrency: 2,
        }
    }
}

/// Process-wide export counters. Exposed for tests and diagnostics; the
/// cancellation test asserts that `pages` stops growing once the client drops
/// the response body.
#[derive(Debug, Default)]
pub struct ExportStats {
    pub started: AtomicU64,
    pub finished: AtomicU64,
    pub cancelled: AtomicU64,
    /// Exports that ended early because a page could not be fetched.
    pub failed: AtomicU64,
    /// Export requests refused because `ExportLimits::concurrency` was reached.
    pub rejected: AtomicU64,
    /// Result pages fetched from the search executor, across all exports.
    pub pages: AtomicU64,
    pub rows: AtomicU64,
}

// ----- request -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Csv,
    Json,
    Ndjson,
}

impl ExportFormat {
    fn content_type(self) -> &'static str {
        match self {
            // RFC 4180 media type. UTF-8 is explicit so no BOM is needed.
            Self::Csv => "text/csv; charset=utf-8",
            Self::Json => "application/json; charset=utf-8",
            Self::Ndjson => "application/x-ndjson; charset=utf-8",
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Ndjson => "ndjson",
        }
    }
}

/// Accepts `1`/`0` as well as `true`/`false` so query strings stay terse
/// (`bom=1`).
fn de_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let raw = String::deserialize(d)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => Err(serde::de::Error::custom(format!(
            "expected a boolean flag, got `{other}`"
        ))),
    }
}

#[derive(Debug, Deserialize, TS)]
pub struct ExportGetQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    format: ExportFormat,
    #[serde(default)]
    mode: Option<eidos_domain::ResultMode>,
    #[serde(default)]
    sort: Option<eidos_domain::SortField>,
    #[serde(default, deserialize_with = "de_flag")]
    desc: bool,
    /// Maximum rows to export; clamped to the configured hard cap.
    #[serde(default)]
    limit: Option<u64>,
    /// Prefix a UTF-8 BOM (CSV only) so Excel detects the encoding.
    #[serde(default, deserialize_with = "de_flag")]
    bom: bool,
    #[serde(default, deserialize_with = "de_flag")]
    include_retired: bool,
}

/// POST body: the search body's query fields plus the export options. `limit`
/// is the row cap of the whole export, not a page size, and `cursor` has no
/// meaning here — the export always walks from the first row.
#[derive(Debug, Deserialize, TS)]
#[ts(optional_fields = nullable)]
pub struct ExportBody {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    query: Option<eidos_domain::Query>,
    #[serde(default)]
    mode: eidos_domain::ResultMode,
    #[serde(default)]
    sort: eidos_domain::Sort,
    #[serde(default)]
    format: ExportFormat,
    #[serde(default)]
    #[ts(as = "Option<u64>")]
    #[serde(with = "eidos_domain::json::option_u64_string")]
    limit: Option<u64>,
    #[serde(default)]
    bom: bool,
    #[serde(default)]
    include_retired: bool,
}

/// Everything the streaming producer needs; built once, up front.
struct Plan {
    req: SearchRequest,
    format: ExportFormat,
    bom: bool,
    max_rows: u64,
    meta: QueryMeta,
}

#[derive(Debug, Clone, Serialize, TS)]
pub(crate) struct QueryMeta {
    /// The submitted query text, when the request used `q`.
    q: Option<String>,
    /// The compiled AST rendered back into query syntax.
    rendered: String,
    mode: eidos_domain::ResultMode,
    sort: eidos_domain::Sort,
    include_retired: bool,
    ast: eidos_domain::Query,
}

/// Fields shared by the JSON document and the first NDJSON record.
#[derive(Debug, Serialize, TS)]
pub(crate) struct ExportHeader {
    schema: &'static str,
    query: QueryMeta,
    exported_at: String,
    total: eidos_domain::TotalCount,
    max_rows: u64,
    completeness: Vec<eidos_domain::SourceCompleteness>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize, TS)]
pub(crate) struct ExportSummary {
    rows_exported: u64,
    truncated: bool,
    error: Option<String>,
}

/// Complete `format=json` wire contract. The writer streams its three parts
/// independently, but flattening keeps the generated type identical to the
/// single object consumers receive.
#[derive(Debug, Serialize, TS)]
pub struct ExportDocument {
    #[serde(flatten)]
    header: ExportHeader,
    rows: Vec<ExportRow>,
    #[serde(flatten)]
    summary: ExportSummary,
}

#[derive(Debug, Serialize, TS)]
pub(crate) struct ExportNdjsonHeader {
    #[serde(rename = "type")]
    #[ts(type = "\"header\"")]
    record_type: &'static str,
    #[serde(flatten)]
    header: ExportHeader,
}

#[derive(Debug, Serialize, TS)]
pub(crate) struct ExportNdjsonSummary {
    schema: &'static str,
    #[serde(rename = "type")]
    #[ts(type = "\"summary\"")]
    record_type: &'static str,
    #[serde(flatten)]
    summary: ExportSummary,
}

fn plan(
    st: &AppState,
    body: SearchBody,
    format: ExportFormat,
    bom: bool,
    limit: Option<u64>,
) -> Result<Plan, ApiError> {
    let q = body.q.clone();
    let (mut req, _notes) = build_request(body)?;
    let meta = QueryMeta {
        q,
        rendered: eidos_query::render(&req.query),
        mode: req.mode,
        sort: req.sort,
        include_retired: req.include_retired,
        ast: req.query.clone(),
    };
    // Exports are metadata only: no snippets, facets, or plan explanation, so
    // a page costs the same regardless of how many pages follow.
    req.limit = st.export.page_size.max(1);
    req.cursor = None;
    req.explain = false;
    req.snippets = false;
    req.facets = Vec::new();
    let cap = st.export.max_rows;
    let max_rows = match limit {
        Some(n) => n.min(cap),
        None => cap,
    };
    Ok(Plan {
        req,
        format,
        bom,
        max_rows,
        meta,
    })
}

pub async fn export_get(
    State(st): State<Arc<AppState>>,
    Query(q): Query<ExportGetQuery>,
) -> Result<Response, ApiError> {
    let body = SearchBody {
        q: Some(q.q),
        query: None,
        mode: q.mode.unwrap_or_default(),
        sort: eidos_domain::Sort {
            field: q.sort.unwrap_or_default(),
            descending: q.desc,
        },
        limit: st.export.page_size,
        cursor: None,
        explain: false,
        facets: Vec::new(),
        include_retired: q.include_retired,
        count: eidos_domain::CountPolicy::Auto,
    };
    let p = plan(&st, body, q.format, q.bom, q.limit)?;
    start(st, p).await
}

pub async fn export_post(
    State(st): State<Arc<AppState>>,
    Json(b): Json<ExportBody>,
) -> Result<Response, ApiError> {
    let body = SearchBody {
        q: b.q,
        query: b.query,
        mode: b.mode,
        sort: b.sort,
        limit: st.export.page_size,
        cursor: None,
        explain: false,
        facets: Vec::new(),
        include_retired: b.include_retired,
        count: eidos_domain::CountPolicy::Auto,
    };
    let p = plan(&st, body, b.format, b.bom, b.limit)?;
    start(st, p).await
}

// ----- streaming -----------------------------------------------------------

/// Fetch one page under the admission gate.
///
/// A permit is taken *per page*, not for the whole stream: an export of
/// 100,000 rows can run for minutes, and holding one of the few expensive-
/// operation permits for that long would starve interactive search. Per page
/// the cost is exactly one ordinary search. Export pages never join the shared
/// queue and yield when interactive work is waiting; a busy service therefore
/// ends an export rather than letting its repeated page requests get ahead of
/// searches. A stalled client cannot pin a permit — between pages the export
/// holds nothing.
async fn fetch_page(st: &Arc<AppState>, req: &SearchRequest) -> Result<SearchResponse, ApiError> {
    let (st2, req) = (st.clone(), req.clone());
    st.admission
        .run_export(move || {
            st2.export_stats.pages.fetch_add(1, Ordering::Relaxed);
            eidos_search::exec::search_with_content(
                &st2.index,
                Some(&st2.content_index),
                &st2.catalog,
                &req,
                &st2.exec_opts,
            )
        })
        .await?
        .map_err(search_error)
}

/// Run the first page eagerly so query errors, load shedding, and timeouts
/// surface as a normal JSON error response instead of a truncated stream,
/// then hand the walk to a background task.
async fn start(st: Arc<AppState>, p: Plan) -> Result<Response, ApiError> {
    // One slot per streaming export, taken for the whole stream and released
    // when the producer task ends however it ends. Exports therefore never
    // occupy more than `export.concurrency` admission permits at any instant,
    // which is what keeps a burst of concurrent exports from crowding
    // interactive search out of the shared gate.
    let slot = st.export_gate.clone().try_acquire_owned().map_err(|_| {
        st.export_stats.rejected.fetch_add(1, Ordering::Relaxed);
        ApiError::busy(
            format!(
                "{} exports are already streaming (the limit); retry when one finishes",
                st.export.concurrency
            ),
            5,
        )
    })?;
    let first = fetch_page(&st, &p.req).await?;

    let total = first.total;
    let format = p.format;
    let max_rows = p.max_rows;
    // Capacity 1: the producer may prepare at most one page beyond the one the
    // client is reading, so peak memory stays at a couple of pages.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    st.export_stats.started.fetch_add(1, Ordering::Relaxed);
    let st3 = st.clone();
    tokio::spawn(async move {
        produce(st3, p, first, tx).await;
        drop(slot);
    });

    let mut resp = Response::new(Body::from_stream(ChannelStream { rx }));
    let filename = format!(
        "eidos-export-{}.{}",
        compact_stamp(UnixNanos::now()),
        format.extension()
    );
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    // Row counts let a CSV consumer — which has no envelope to read — detect
    // truncation by comparing the total with the cap.
    h.insert("x-eidos-export-max-rows", num_header(max_rows));
    h.insert("x-eidos-export-total", num_header(total.value));
    h.insert(
        "x-eidos-export-total-exact",
        HeaderValue::from_static(if total.exact { "true" } else { "false" }),
    );
    Ok(resp)
}

fn num_header(n: u64) -> HeaderValue {
    HeaderValue::from_str(&n.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0"))
}

/// `20260823T014203Z`, for the download file name.
fn compact_stamp(t: UnixNanos) -> String {
    t.to_rfc3339_nanos()
        .chars()
        .take_while(|c| *c != '.')
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        + "Z"
}

struct ChannelStream {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

impl futures_core::Stream for ChannelStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Send whatever has accumulated. `false` means the client is gone.
async fn flush(
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: &mut Vec<u8>,
) -> bool {
    if buf.is_empty() {
        return true;
    }
    let chunk = Bytes::from(std::mem::take(buf));
    tx.send(Ok(chunk)).await.is_ok()
}

async fn produce(
    st: Arc<AppState>,
    p: Plan,
    first: SearchResponse,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    if p.bom && p.format == ExportFormat::Csv {
        buf.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    let names: HashMap<i64, String> = first
        .completeness
        .iter()
        .map(|c| (c.source_id.0, c.name.clone()))
        .collect();
    write_header(&mut buf, &p, &first);

    let mut emitted: u64 = 0;
    let mut truncated = false;
    let mut error: Option<String> = None;
    let mut page = first;
    let mut cancelled = false;
    loop {
        let mut hit_cap = false;
        for hit in &page.hits {
            if emitted >= p.max_rows {
                hit_cap = true;
                break;
            }
            write_row(&mut buf, p.format, &row(hit, &names), emitted);
            emitted += 1;
        }
        if !flush(&tx, &mut buf).await {
            cancelled = true;
            break;
        }
        if hit_cap {
            truncated = true;
            break;
        }
        if emitted >= p.max_rows {
            truncated = page.next_cursor.is_some();
            break;
        }
        let Some(cursor) = page.next_cursor.clone() else {
            break;
        };
        let mut req = p.req.clone();
        req.cursor = Some(cursor);
        match fetch_page(&st, &req).await {
            Ok(next) if next.hits.is_empty() => break,
            Ok(next) => page = next,
            Err(e) => {
                tracing::error!(error = %e.message, "export walk ended early");
                error = Some(e.message);
                truncated = true;
                break;
            }
        }
    }

    st.export_stats.rows.fetch_add(emitted, Ordering::Relaxed);
    if cancelled {
        st.export_stats.cancelled.fetch_add(1, Ordering::Relaxed);
        return;
    }
    write_footer(&mut buf, p.format, emitted, truncated, error.as_deref());
    if let Some(message) = error {
        st.export_stats.failed.fetch_add(1, Ordering::Relaxed);
        // Send the footer (JSON and NDJSON stay parseable and carry `error`),
        // then fail the body. Ending it cleanly would hand a CSV consumer —
        // which has no envelope to read — a short file that looks complete;
        // aborting the transfer makes every client see a failed download.
        let _ = flush(&tx, &mut buf).await;
        let _ = tx
            .send(Err(std::io::Error::other(format!(
                "export ended early: {message}"
            ))))
            .await;
        return;
    }
    if flush(&tx, &mut buf).await {
        st.export_stats.finished.fetch_add(1, Ordering::Relaxed);
    } else {
        st.export_stats.cancelled.fetch_add(1, Ordering::Relaxed);
    }
}

// ----- rows ----------------------------------------------------------------

/// One exported result. Unlike [`eidos_domain::Hit`], absent values are
/// serialised as explicit `null` rather than omitted, so consumers can tell
/// "unknown" from "not requested".
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct ExportRow {
    pub object_id: i64,
    pub entry_id: Option<i64>,
    /// Source name (unique within a catalog).
    pub source: String,
    pub kind: &'static str,
    pub name: String,
    pub path: Option<String>,
    pub extension: String,
    pub size: u64,
    pub allocated_size: u64,
    /// RFC 3339 UTC with the full nanosecond field.
    pub modified: Option<String>,
    pub created: Option<String>,
    pub content_state: &'static str,
    pub hard_link_count: u32,
    pub score: Option<f32>,
}

fn row(h: &Hit, names: &HashMap<i64, String>) -> ExportRow {
    ExportRow {
        object_id: h.object_id.0,
        entry_id: h.entry_id.map(|e| e.0),
        source: names
            .get(&h.source_id.0)
            .cloned()
            .unwrap_or_else(|| h.source_id.0.to_string()),
        kind: h.kind.as_str(),
        name: h.name.clone(),
        path: h.path.clone(),
        extension: h.extension.clone(),
        size: h.size,
        allocated_size: h.allocated_size,
        modified: h.modified.map(|t| t.to_rfc3339_nanos()),
        created: h.created.map(|t| t.to_rfc3339_nanos()),
        content_state: h.content.state.as_str(),
        hard_link_count: h.hard_link_count,
        score: h.score,
    }
}

/// RFC 4180: quote a field only when it contains a delimiter, a quote, or a
/// line break; escape an embedded quote by doubling it.
pub fn csv_field(out: &mut Vec<u8>, s: &str) {
    if s.bytes().any(|b| matches!(b, b',' | b'"' | b'\r' | b'\n')) {
        out.push(b'"');
        for b in s.bytes() {
            if b == b'"' {
                out.push(b'"');
            }
            out.push(b);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(s.as_bytes());
    }
}

fn opt(s: &Option<String>) -> Cow<'_, str> {
    match s {
        Some(v) => Cow::Borrowed(v.as_str()),
        None => Cow::Borrowed(""),
    }
}

fn write_csv_row(out: &mut Vec<u8>, r: &ExportRow) {
    let cells: [Cow<'_, str>; COLUMNS.len()] = [
        Cow::Owned(r.object_id.to_string()),
        r.entry_id
            .map(|v| Cow::Owned(v.to_string()))
            .unwrap_or(Cow::Borrowed("")),
        Cow::Borrowed(r.source.as_str()),
        Cow::Borrowed(r.kind),
        Cow::Borrowed(r.name.as_str()),
        opt(&r.path),
        Cow::Borrowed(r.extension.as_str()),
        Cow::Owned(r.size.to_string()),
        Cow::Owned(r.allocated_size.to_string()),
        opt(&r.modified),
        opt(&r.created),
        Cow::Borrowed(r.content_state),
        Cow::Owned(r.hard_link_count.to_string()),
        r.score
            .map(|v| Cow::Owned(v.to_string()))
            .unwrap_or(Cow::Borrowed("")),
    ];
    for (i, c) in cells.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        csv_field(out, c);
    }
    out.extend_from_slice(b"\r\n");
}

// ----- envelopes -----------------------------------------------------------

fn json_of<T: Serialize>(v: &T) -> String {
    crate::api_json::to_vec(v)
        .map(|bytes| String::from_utf8(bytes).expect("JSON serialization must produce UTF-8"))
        .unwrap_or_else(|_| "null".into())
}

fn write_header(out: &mut Vec<u8>, p: &Plan, first: &SearchResponse) {
    if p.format == ExportFormat::Csv {
        out.extend_from_slice(COLUMNS.join(",").as_bytes());
        out.extend_from_slice(b"\r\n");
        return;
    }
    let ndjson = p.format == ExportFormat::Ndjson;
    let head = ExportHeader {
        schema: EXPORT_SCHEMA,
        query: p.meta.clone(),
        exported_at: UnixNanos::now().to_rfc3339_nanos(),
        total: first.total,
        max_rows: p.max_rows,
        completeness: first.completeness.clone(),
        warnings: first.warnings.clone(),
    };
    if ndjson {
        out.extend_from_slice(
            json_of(&ExportNdjsonHeader {
                record_type: "header",
                header: head,
            })
            .as_bytes(),
        );
        out.push(b'\n');
    } else {
        // Leave the envelope object open so `rows` can stream into it.
        let s = json_of(&head);
        out.extend_from_slice(s.strip_suffix('}').unwrap_or(&s).as_bytes());
        out.extend_from_slice(b",\"rows\":[");
    }
}

fn write_row(out: &mut Vec<u8>, format: ExportFormat, r: &ExportRow, index: u64) {
    match format {
        ExportFormat::Csv => write_csv_row(out, r),
        ExportFormat::Json => {
            if index > 0 {
                out.push(b',');
            }
            out.extend_from_slice(json_of(r).as_bytes());
        }
        ExportFormat::Ndjson => {
            out.extend_from_slice(json_of(r).as_bytes());
            out.push(b'\n');
        }
    }
}

/// `truncated` and the exported row count are only known once the walk ends,
/// so they close the JSON object after `rows` (JSON object members are
/// unordered) and form the final NDJSON line.
fn write_footer(
    out: &mut Vec<u8>,
    format: ExportFormat,
    rows: u64,
    truncated: bool,
    error: Option<&str>,
) {
    if format == ExportFormat::Csv {
        return;
    }
    let tail = ExportSummary {
        rows_exported: rows,
        truncated,
        error: error.map(str::to_owned),
    };
    if format == ExportFormat::Json {
        // Close `rows`, then merge the summary into the still-open envelope.
        let s = json_of(&tail);
        out.extend_from_slice(b"],");
        out.extend_from_slice(s.strip_prefix('{').unwrap_or(&s).as_bytes());
    } else {
        out.extend_from_slice(
            json_of(&ExportNdjsonSummary {
                schema: EXPORT_SCHEMA,
                record_type: "summary",
                summary: tail,
            })
            .as_bytes(),
        );
    }
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(s: &str) -> String {
        let mut out = Vec::new();
        csv_field(&mut out, s);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn rfc4180_quoting() {
        assert_eq!(field("plain"), "plain");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("line1\r\nline2"), "\"line1\r\nline2\"");
        // Unicode needs no quoting and must pass through byte for byte.
        assert_eq!(field("naïve — Ωμέγα 漢字"), "naïve — Ωμέγα 漢字");
        assert_eq!(field(""), "");
    }

    fn sample_row() -> ExportRow {
        ExportRow {
            object_id: 1,
            entry_id: None,
            source: "zephyr".into(),
            kind: "file",
            name: "a,b.txt".into(),
            path: None,
            extension: "txt".into(),
            size: 0,
            allocated_size: 0,
            modified: None,
            created: None,
            content_state: "pending",
            hard_link_count: 1,
            score: None,
        }
    }

    #[test]
    fn absent_values_serialise_as_explicit_nulls() {
        let json = serde_json::to_string(&sample_row()).unwrap();
        for key in ["entry_id", "path", "modified", "created", "score"] {
            assert!(json.contains(&format!("\"{key}\":null")), "{key} in {json}");
        }
        // Field order in the document matches the documented column order.
        let pos: Vec<usize> = COLUMNS
            .iter()
            .map(|c| json.find(&format!("\"{c}\":")).expect(c))
            .collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{json}");
    }

    #[test]
    fn column_count_matches_row_cells() {
        let r = sample_row();
        let mut out = Vec::new();
        write_csv_row(&mut out, &r);
        let line = String::from_utf8(out).unwrap();
        assert!(line.ends_with("\r\n"));
        assert_eq!(line.matches(',').count() - 1, COLUMNS.len() - 1);
    }
}
