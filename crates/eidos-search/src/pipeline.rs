//! Per-object content pipeline shared by the service workers, tests, and
//! tools: `extract → store chunks → index chunks → record outcome`.
//!
//! Publication is two-phase (ADR-0005): the content record is written as
//! `indexing` before the index commit, and `Catalog::mark_content_indexed`
//! flips the object's state after the commit. A crash in between leaves an
//! `indexing` record that `requeue_unfinished_content` re-queues at startup.

use crate::content::ContentIndex;
use crate::Result;
use eidos_archive::{ArchiveError, ArchiveLimits};
use eidos_catalog::archive::{ArchiveMember, ArchiveRecord};
use eidos_catalog::content::{ContentRecord, ContentTarget};
use eidos_catalog::Catalog;
use eidos_content::{extract, Chunk, Limits, EXTRACTION_VERSION};
use eidos_domain::archive::{archive_format, ArchiveFormat};
use eidos_domain::{ContentState, Coverage, FailureClass, ObjectId, UnixNanos};
use std::time::Instant;

/// Chunks buffered before a catalog write + index add (files up to this
/// many chunks are stored in a single transaction).
pub const CHUNK_BATCH: usize = 64;

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
    job: Option<eidos_domain::JobId>,
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
        ContentState::Excluded | ContentState::Unsupported | ContentState::NotApplicable => {
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
    // Chunks of a small file stay in `batch` and are stored in the same
    // transaction as the content record; larger files stream batches.
    let mut batch: Vec<Chunk> = Vec::with_capacity(CHUNK_BATCH);
    let mut sink_err: Option<String> = None;
    let outcome = {
        let mut sink = |c: Chunk| -> std::result::Result<(), String> {
            batch.push(c);
            if batch.len() >= CHUNK_BATCH {
                flush(catalog, index, object, source, generation, &mut batch)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        };
        extract(std::path::Path::new(&target.path), limits, &mut sink)
    };
    let mut outcome = outcome;
    if let Some(e) = sink_err.take() {
        outcome.state = ContentState::Failed;
        outcome.failure = Some((FailureClass::Transient, e));
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
        ContentState::Indexed | ContentState::Partial => {
            catalog.store_content(&rec, &batch, false, job)?;
            index.add_chunks(object, source, generation, &batch)?;
            Ok(ProcessResult::Indexed(stats))
        }
        ContentState::Failed => {
            let (class, error) = outcome
                .failure
                .clone()
                .unwrap_or((FailureClass::Deterministic, "unknown failure".into()));
            if class.retryable() {
                Ok(ProcessResult::Retry { class, error })
            } else {
                let rec = ContentRecord {
                    coverage: Coverage::None,
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
    job: Option<eidos_domain::JobId>,
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
            catalog.store_archive_marker(&rec)?;
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
            catalog.store_archive(&rec, &[], &content, job)?;
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
            catalog.store_archive(&rec, &members, &content, job)?;
            Ok(Some(ProcessResult::Done(stats(
                state,
                inv.bytes_read,
                inv.member_count as u32,
            ))))
        }
    }
}

fn flush(
    catalog: &Catalog,
    index: &ContentIndex,
    object: ObjectId,
    source: eidos_domain::SourceId,
    generation: u32,
    batch: &mut Vec<Chunk>,
) -> Result<()> {
    catalog.write_chunks(object, generation, batch)?;
    index.add_chunks(object, source, generation, batch)?;
    batch.clear();
    Ok(())
}

/// Drain every queued content job in-process (tests, CLI tools). Returns
/// the number of objects published.
pub fn drain_content_jobs(
    catalog: &Catalog,
    index: &ContentIndex,
    limits: &Limits,
    worker: &str,
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
        match process_object(
            catalog,
            index,
            object,
            job.object_generation,
            limits,
            Some(job.id),
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
