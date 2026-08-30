//! Per-object content pipeline shared by the service workers, tests, and
//! tools: `extract → store chunks → index chunks → record outcome`.
//!
//! Publication is two-phase (ADR-0005): the content record is written as
//! `indexing` before the index commit, and `Catalog::mark_content_indexed`
//! flips the object's state after the commit. A crash in between leaves an
//! `indexing` record that `requeue_unfinished_content` re-queues at startup.
//!
//! Writing chunks can fail for reasons that have nothing to do with the
//! file: SQLite contention, a full disk, an index writer error. Those come
//! back as a typed [`SinkError`], are classified `transient` (retried under
//! the job's backoff) rather than as deterministic extraction failures, and
//! take everything the attempt wrote with them — the record that publishes
//! a generation is never written unless all of its chunks and documents
//! are.

use crate::content::ContentIndex;
use crate::{Result, SearchError};
use eidos_archive::{ArchiveError, ArchiveLimits};
use eidos_catalog::archive::{ArchiveMember, ArchiveRecord};
use eidos_catalog::content::{ContentRecord, ContentTarget};
use eidos_catalog::Catalog;
use eidos_content::{extract, Chunk, Limits, SinkFailure, EXTRACTION_VERSION};
use eidos_domain::archive::{archive_format, ArchiveFormat};
use eidos_domain::{ContentState, Coverage, FailureClass, JobId, ObjectId, SourceId, UnixNanos};
use std::sync::Arc;
use std::time::Instant;

/// Chunks buffered before a catalog write + index add (files up to this
/// many chunks are stored in a single transaction).
pub const CHUNK_BATCH: usize = 64;

/// Which store rejected a chunk write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkStage {
    /// The catalog's chunk store (SQLite).
    Catalog,
    /// The content index writer (Tantivy).
    Index,
}

impl SinkStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Index => "index",
        }
    }
}

impl std::fmt::Display for SinkStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A chunk write failed in the catalog or in the content index.
///
/// This is infrastructure failing, never a property of the file being
/// extracted, so it keeps its own classification instead of being folded
/// into the extractor's decode/content failures.
#[derive(Debug, thiserror::Error)]
#[error("{stage} write failed for object {object} generation {generation}")]
pub struct SinkError {
    pub stage: SinkStage,
    pub object: ObjectId,
    pub generation: u32,
    #[source]
    pub source: SearchError,
}

impl SinkError {
    /// Storage and index failures are transient by definition: the job
    /// retries them under its normal backoff (`Catalog::fail_job`).
    pub const fn class(&self) -> FailureClass {
        FailureClass::Transient
    }

    /// The whole `error: cause: cause` chain, for the job's `last_error`
    /// and the operator-visible reason.
    pub fn chain(&self) -> String {
        let mut s = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(e) = source {
            s.push_str(": ");
            s.push_str(&e.to_string());
            source = e.source();
        }
        s
    }

    fn as_sink_failure(&self) -> SinkFailure {
        SinkFailure {
            class: self.class(),
            message: self.chain(),
        }
    }
}

/// Fault injection for chunk writes, used by the pipeline's fault tests.
///
/// The hook is consulted before every catalog and index write with the
/// stage and the number of chunks already written for the object;
/// returning a message fails that write exactly as the store would.
/// Production paths ([`process_object`], [`drain_content_jobs`]) pass an
/// empty set.
#[derive(Clone, Default)]
pub struct SinkFaults {
    #[allow(clippy::type_complexity)]
    hook: Option<Arc<dyn Fn(SinkStage, u32) -> Option<String> + Send + Sync>>,
}

impl SinkFaults {
    pub fn new(hook: impl Fn(SinkStage, u32) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            hook: Some(Arc::new(hook)),
        }
    }

    fn check(&self, stage: SinkStage, written: u32) -> Option<SearchError> {
        self.hook
            .as_ref()
            .and_then(|h| h(stage, written))
            .map(SearchError::Other)
    }
}

impl std::fmt::Debug for SinkFaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkFaults")
            .field("armed", &self.hook.is_some())
            .finish()
    }
}

/// Where one object generation's chunks go: rows in the catalog and
/// documents in the content index writer.
struct Sink<'a> {
    catalog: &'a Catalog,
    index: &'a ContentIndex,
    object: ObjectId,
    source: SourceId,
    generation: u32,
    /// Chunks handed to both stores so far (the fault hook keys on it).
    written: u32,
    faults: &'a SinkFaults,
}

impl Sink<'_> {
    fn err(&self, stage: SinkStage, source: SearchError) -> SinkError {
        SinkError {
            stage,
            object: self.object,
            generation: self.generation,
            source,
        }
    }

    fn check(&self, stage: SinkStage) -> std::result::Result<(), SinkError> {
        match self.faults.check(stage, self.written) {
            Some(e) => Err(self.err(stage, e)),
            None => Ok(()),
        }
    }

    /// Store a streamed batch: chunk rows first, then documents, so a
    /// catalog failure leaves nothing queued in the index writer.
    fn flush(&mut self, batch: &mut Vec<Chunk>) -> std::result::Result<(), SinkError> {
        self.check(SinkStage::Catalog)?;
        self.catalog
            .write_chunks(self.object, self.generation, batch)
            .map_err(|e| self.err(SinkStage::Catalog, e.into()))?;
        self.check(SinkStage::Index)?;
        self.index
            .add_chunks(self.object, self.source, self.generation, batch)
            .map_err(|e| self.err(SinkStage::Index, e))?;
        self.written += batch.len() as u32;
        batch.clear();
        Ok(())
    }

    /// Store the last batch together with the `indexing` content record
    /// (one transaction, which also completes the job).
    ///
    /// The documents go to the index writer first: the record is what the
    /// next commit publishes, so it must never exist unless every document
    /// of the generation is already queued behind it.
    fn finish(
        &mut self,
        rec: &ContentRecord,
        batch: &[Chunk],
        job: Option<JobId>,
    ) -> std::result::Result<(), SinkError> {
        self.check(SinkStage::Index)?;
        self.index
            .add_chunks(self.object, self.source, self.generation, batch)
            .map_err(|e| self.err(SinkStage::Index, e))?;
        self.check(SinkStage::Catalog)?;
        self.catalog
            .store_content(rec, batch, false, job)
            .map_err(|e| self.err(SinkStage::Catalog, e.into()))?;
        self.written += batch.len() as u32;
        Ok(())
    }

    /// Discard everything this attempt wrote: the generation's chunk rows
    /// go away and its documents are deleted at the next commit (the
    /// delete is ordered after the adds, so it removes them).
    ///
    /// Both halves are idempotent, so the next attempt — in this process or
    /// after a restart, where the startup requeue brings the job back —
    /// can repeat them.
    fn discard(&self) -> Result<()> {
        self.discard_documents();
        self.catalog.delete_chunks(self.object, self.generation)?;
        Ok(())
    }

    /// The index half of [`Sink::discard`], for callers that drop the
    /// chunk rows in a transaction of their own.
    fn discard_documents(&self) {
        self.index.delete_object(self.object);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessStats {
    pub object_id: ObjectId,
    pub generation: u32,
    pub state: ContentState,
    pub bytes: u64,
    pub chunks: u32,
    pub elapsed_ms: f64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessResult {
    /// Chunks are in the index writer; publish after the next commit.
    Indexed(ProcessStats),
    /// Outcome recorded and published immediately (unsupported, terminal failure).
    Done(ProcessStats),
    /// Nothing to do; the job should complete.
    Skipped(&'static str),
    /// Content policy disabled for the source; the job should be dropped so
    /// re-enabling re-queues it.
    Disabled,
    /// Transient failure: the job should retry.
    Retry { class: FailureClass, error: String },
}

/// Process one object at `expected_generation`. When `job` is given it is
/// marked done in the same transaction that stores the outcome
/// (`Indexed`/`Done`); for `Skipped`, `Disabled`, and `Retry` the caller
/// owns the job.
pub fn process_object(
    catalog: &Catalog,
    index: &ContentIndex,
    object: ObjectId,
    expected_generation: u32,
    limits: &Limits,
    job: Option<JobId>,
) -> Result<ProcessResult> {
    process_object_with_faults(
        catalog,
        index,
        object,
        expected_generation,
        limits,
        job,
        &SinkFaults::default(),
    )
}

/// [`process_object`] with chunk-write fault injection (see [`SinkFaults`]).
pub fn process_object_with_faults(
    catalog: &Catalog,
    index: &ContentIndex,
    object: ObjectId,
    expected_generation: u32,
    limits: &Limits,
    job: Option<JobId>,
    faults: &SinkFaults,
) -> Result<ProcessResult> {
    let started = Instant::now();
    let target = match catalog.content_target(object)? {
        Some(t) => t,
        None => return Ok(ProcessResult::Skipped("object gone")),
    };
    if target.generation != expected_generation {
        return Ok(ProcessResult::Skipped("superseded by a newer generation"));
    }
    if !target.content_enabled {
        return Ok(ProcessResult::Disabled);
    }
    match target.content_state {
        ContentState::Pending | ContentState::Stale | ContentState::Failed => {}
        ContentState::Indexed | ContentState::Partial => {
            // Already indexed for this generation (e.g. duplicate job).
            if let Some(rec) = catalog.content_record(object)? {
                if rec.generation == target.generation
                    && rec.extraction_version == EXTRACTION_VERSION
                {
                    return Ok(ProcessResult::Skipped("already indexed"));
                }
            }
        }
        ContentState::Excluded
        | ContentState::Unsupported
        | ContentState::NotApplicable
        | ContentState::NotReplicated => {
            return Ok(ProcessResult::Skipped("not a content candidate"));
        }
    }

    // Old chunks (any generation) go away with the same commit that adds
    // the new ones.
    index.delete_object(object);

    if let Some(format) = archive_format(&target.path) {
        if let Some(r) = process_archive(catalog, &target, format, job, started)? {
            return Ok(r);
        }
    }

    let (source, generation) = (target.source_id, target.generation);
    let mut sink = Sink {
        catalog,
        index,
        object,
        source,
        generation,
        written: 0,
        faults,
    };
    // Chunks of a small file stay in `batch` and are stored in the same
    // transaction as the content record; larger files stream batches.
    let mut batch: Vec<Chunk> = Vec::with_capacity(CHUNK_BATCH);
    let mut sink_err: Option<SinkError> = None;
    let outcome = {
        let mut deliver = |c: Chunk| -> eidos_content::SinkResult {
            batch.push(c);
            if batch.len() >= CHUNK_BATCH {
                if let Err(e) = sink.flush(&mut batch) {
                    let failure = e.as_sink_failure();
                    sink_err = Some(e);
                    return Err(failure);
                }
            }
            Ok(())
        };
        extract(std::path::Path::new(&target.path), limits, &mut deliver)
    };
    let mut outcome = outcome;
    if let Some(e) = &sink_err {
        // The extractor already passed the sink's verdict through; keep the
        // typed error authoritative for the class and the error chain.
        debug_assert!(outcome.sink_failed);
        outcome.state = ContentState::Failed;
        outcome.failure = Some((e.class(), e.chain()));
    }

    let stats = ProcessStats {
        object_id: object,
        generation,
        state: outcome.state,
        bytes: outcome.indexed_bytes,
        chunks: outcome.chunk_count,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        path: target.path.clone(),
    };
    let rec = ContentRecord {
        object_id: object,
        source_id: source,
        generation,
        extraction_version: EXTRACTION_VERSION,
        encoding: outcome.encoding.map(|e| e.as_str().to_string()),
        coverage: outcome.coverage,
        indexed_bytes: outcome.indexed_bytes,
        total_bytes: outcome.total_bytes,
        chunk_count: outcome.chunk_count,
        line_count: outcome.line_count,
        chars: outcome.chars,
        content_id: outcome.content_id,
        hash_complete: outcome.hash_complete,
        state: outcome.state,
        failure_class: outcome.failure.as_ref().map(|f| f.0),
        error: outcome.failure.as_ref().map(|f| f.1.clone()),
        reason: outcome.reason.clone(),
        processed_at: UnixNanos::now(),
        elapsed_ms: outcome.elapsed_ms,
    };
    match outcome.state {
        ContentState::Indexed | ContentState::Partial => match sink.finish(&rec, &batch, job) {
            Ok(()) => Ok(ProcessResult::Indexed(stats)),
            Err(e) => {
                tracing::warn!(object = object.0, error = %e.chain(), "storing content failed");
                sink.discard()?;
                Ok(ProcessResult::Retry {
                    class: e.class(),
                    error: e.chain(),
                })
            }
        },
        ContentState::Failed => {
            let (class, error) = outcome
                .failure
                .clone()
                .unwrap_or((FailureClass::Deterministic, "unknown failure".into()));
            if class.retryable() {
                // Whatever this attempt already flushed must not survive to
                // be published by a later commit.
                sink.discard()?;
                Ok(ProcessResult::Retry { class, error })
            } else {
                // Terminal, and equally partial: the record claims no
                // coverage and no chunks, so the transaction that writes it
                // also drops the rows this attempt flushed, and the queued
                // delete takes their documents at the next commit.
                sink.discard_documents();
                let rec = ContentRecord {
                    coverage: Coverage::None,
                    chunk_count: 0,
                    indexed_bytes: 0,
                    ..rec
                };
                catalog.store_content(&rec, &[], true, job)?;
                Ok(ProcessResult::Done(stats))
            }
        }
        _ => {
            // Unsupported (binary) and anything else terminal.
            catalog.store_content(&rec, &[], true, job)?;
            Ok(ProcessResult::Done(stats))
        }
    }
}

/// Manifest path for container files (ADR-0010): read the container's own
/// directory, store members and a content record that publishes the
/// object's state. `None` when the file is not actually a ZIP, so the
/// caller falls back to text extraction (a misnamed text file still gets
/// indexed; a binary one ends `unsupported` as before).
fn process_archive(
    catalog: &Catalog,
    target: &ContentTarget,
    format: ArchiveFormat,
    job: Option<JobId>,
    started: Instant,
) -> Result<Option<ProcessResult>> {
    let limits = ArchiveLimits::default();
    let (object, source, generation) = (target.object_id, target.source_id, target.generation);
    let outcome = eidos_archive::inventory(std::path::Path::new(&target.path), &limits);
    let content_record = |state: ContentState,
                          coverage: Coverage,
                          failure: Option<(FailureClass, String)>,
                          reason: Option<String>,
                          indexed_bytes: u64,
                          elapsed_ms: f64| ContentRecord {
        object_id: object,
        source_id: source,
        generation,
        extraction_version: EXTRACTION_VERSION,
        encoding: None,
        coverage,
        indexed_bytes,
        total_bytes: target.size,
        chunk_count: 0,
        line_count: 0,
        chars: 0,
        content_id: None,
        hash_complete: false,
        state,
        failure_class: failure.as_ref().map(|f| f.0),
        error: failure.as_ref().map(|f| f.1.clone()),
        reason,
        processed_at: UnixNanos::now(),
        elapsed_ms,
    };
    let stats = |state: ContentState, bytes: u64, members: u32| ProcessStats {
        object_id: object,
        generation,
        state,
        bytes,
        chunks: members,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        path: target.path.clone(),
    };
    let base_record = |state: ContentState| ArchiveRecord {
        object_id: object,
        source_id: source,
        generation,
        format: format.as_str().to_string(),
        member_count: 0,
        dir_count: 0,
        implicit_dir_count: 0,
        suspicious_count: 0,
        declared_size: 0,
        compressed_size: 0,
        claimed_entries: 0,
        zip64: false,
        truncated: false,
        comment: None,
        state,
        error: None,
        reason: None,
        processed_at: UnixNanos::now(),
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    };
    match outcome {
        Err(ArchiveError::NotZip) => {
            // Remember the verdict so `requeue_archives` does not queue the
            // file again; the text path records the content outcome.
            let rec = ArchiveRecord {
                reason: Some("no end-of-central-directory record; processed as text".into()),
                ..base_record(ContentState::Unsupported)
            };
            if !catalog.store_archive_marker(&rec)? {
                return Ok(Some(ProcessResult::Skipped(
                    "superseded while reading archive metadata",
                )));
            }
            Ok(None)
        }
        Err(ArchiveError::Io(e)) => Ok(Some(ProcessResult::Retry {
            class: FailureClass::Transient,
            error: e.to_string(),
        })),
        Err(ArchiveError::Corrupt(msg)) => {
            let error = format!("corrupt {} archive: {msg}", format.as_str());
            let rec = ArchiveRecord {
                error: Some(error.clone()),
                ..base_record(ContentState::Failed)
            };
            let content = content_record(
                ContentState::Failed,
                Coverage::None,
                Some((FailureClass::Corrupt, error)),
                None,
                0,
                rec.elapsed_ms,
            );
            if !catalog.store_archive(&rec, &[], &content, job)? {
                return Ok(Some(ProcessResult::Skipped(
                    "superseded while reading archive metadata",
                )));
            }
            Ok(Some(ProcessResult::Done(stats(ContentState::Failed, 0, 0))))
        }
        Ok(inv) => {
            let state = if inv.truncated {
                ContentState::Partial
            } else {
                ContentState::Indexed
            };
            let mut reason = format!(
                "{} manifest: {} members, {} directories",
                format.as_str(),
                inv.member_count,
                inv.dir_count
            );
            if let Some(t) = &inv.truncated_reason {
                reason.push_str("; ");
                reason.push_str(t);
            }
            let rec = ArchiveRecord {
                member_count: inv.member_count,
                dir_count: inv.dir_count,
                implicit_dir_count: inv.implicit_dir_count,
                suspicious_count: inv.suspicious_count,
                declared_size: inv.declared_size,
                compressed_size: inv.compressed_size,
                claimed_entries: inv.claimed_entries,
                zip64: inv.zip64,
                truncated: inv.truncated,
                comment: inv.comment.clone(),
                reason: Some(reason.clone()),
                elapsed_ms: inv.elapsed_ms,
                ..base_record(state)
            };
            let members: Vec<ArchiveMember> = inv
                .members
                .iter()
                .map(|m| ArchiveMember {
                    ordinal: m.ordinal,
                    path: m.path.clone(),
                    name: m.name.clone(),
                    parent: m.parent.clone(),
                    raw_name: m.raw_name.clone(),
                    is_dir: m.is_dir,
                    implicit: m.implicit,
                    size: m.size,
                    compressed: m.compressed,
                    method: m.method,
                    crc32: m.crc32,
                    modified: m.modified,
                    encrypted: m.encrypted,
                    flags: m.flags,
                })
                .collect();
            let content = content_record(
                state,
                if inv.truncated {
                    Coverage::Prefix
                } else {
                    Coverage::Full
                },
                None,
                Some(reason),
                inv.bytes_read,
                inv.elapsed_ms,
            );
            if !catalog.store_archive(&rec, &members, &content, job)? {
                return Ok(Some(ProcessResult::Skipped(
                    "superseded while reading archive metadata",
                )));
            }
            Ok(Some(ProcessResult::Done(stats(
                state,
                inv.bytes_read,
                inv.member_count as u32,
            ))))
        }
    }
}

/// Drain every queued content job in-process (tests, CLI tools). Returns
/// the number of objects published.
pub fn drain_content_jobs(
    catalog: &Catalog,
    index: &ContentIndex,
    limits: &Limits,
    worker: &str,
) -> Result<u64> {
    drain_content_jobs_with_faults(catalog, index, limits, worker, &SinkFaults::default())
}

/// [`drain_content_jobs`] with chunk-write fault injection (see
/// [`SinkFaults`]).
pub fn drain_content_jobs_with_faults(
    catalog: &Catalog,
    index: &ContentIndex,
    limits: &Limits,
    worker: &str,
    faults: &SinkFaults,
) -> Result<u64> {
    use eidos_domain::JobStage;
    let mut pending: Vec<ObjectId> = Vec::new();
    while let Some(job) = catalog.claim_job(&[JobStage::ContentText], worker)? {
        let object = match job.object_id {
            Some(o) => o,
            None => {
                catalog.complete_job(job.id)?;
                continue;
            }
        };
        match process_object_with_faults(
            catalog,
            index,
            object,
            job.object_generation,
            limits,
            Some(job.id),
            faults,
        )? {
            ProcessResult::Indexed(_) => pending.push(object),
            ProcessResult::Done(_) => {}
            ProcessResult::Skipped(_) => catalog.complete_job(job.id)?,
            ProcessResult::Disabled => catalog.delete_job(job.id)?,
            ProcessResult::Retry { class, error } => {
                catalog.fail_job(job.id, class, &error)?;
            }
        }
    }
    index.commit()?;
    let n = catalog.mark_content_indexed(&pending)?;
    Ok(n)
}
