//! Per-object content pipeline shared by the service workers, tests, and
//! tools: `extract → store chunks → index chunks → record outcome`.
//!
//! Publication is two-phase (ADR-0005): the content record is written as
//! `indexing` before the index commit, and `Catalog::mark_content_indexed`
//! flips the object's state after the commit. A crash in between leaves an
//! `indexing` record that `requeue_unfinished_content` re-queues at startup.

use crate::content::ContentIndex;
use crate::Result;
use eidos_catalog::content::ContentRecord;
use eidos_catalog::Catalog;
use eidos_content::{extract, Chunk, Limits, EXTRACTION_VERSION};
use eidos_domain::{ContentState, Coverage, FailureClass, ObjectId, UnixNanos};
use std::time::Instant;

/// Chunks buffered before a catalog write + index add.
pub const CHUNK_BATCH: usize = 32;

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

/// Process one object at `expected_generation`.
pub fn process_object(
    catalog: &Catalog,
    index: &ContentIndex,
    object: ObjectId,
    expected_generation: u32,
    limits: &Limits,
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

    let (source, generation) = (target.source_id, target.generation);
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
        let o = extract(std::path::Path::new(&target.path), limits, &mut sink);
        if !batch.is_empty() {
            if let Err(e) = flush(catalog, index, object, source, generation, &mut batch) {
                sink_err = Some(e.to_string());
            }
        }
        o
    };
    let mut outcome = outcome;
    if let Some(e) = sink_err {
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
            catalog.finish_content(&rec, false)?;
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
                catalog.finish_content(&rec, true)?;
                Ok(ProcessResult::Done(stats))
            }
        }
        _ => {
            // Unsupported (binary) and anything else terminal.
            catalog.finish_content(&rec, true)?;
            Ok(ProcessResult::Done(stats))
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
        match process_object(catalog, index, object, job.object_generation, limits)? {
            ProcessResult::Indexed(_) => {
                pending.push(object);
                catalog.complete_job(job.id)?;
            }
            ProcessResult::Done(_) | ProcessResult::Skipped(_) => catalog.complete_job(job.id)?,
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
