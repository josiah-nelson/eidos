//! Content worker pool: claims `content_text` jobs, runs the extraction
//! pipeline, and publishes in batches after each content-index commit.
//!
//! One coordinator thread owns commits and publication; `workers` threads
//! extract concurrently, each respecting the per-source concurrency budget
//! (`sources.content_concurrency`) so a slow HDD or an SMB share cannot
//! starve NVMe sources. The coordinator also keeps the queue topped up from
//! objects whose `content_state` is `pending`/`stale`.

use crate::state::AppState;
use eidos_content::Limits;
use eidos_domain::{JobStage, ObjectId, SourceId, SourceState};
use eidos_search::pipeline::{process_object, ProcessResult};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const COMMIT_INTERVAL: Duration = Duration::from_secs(2);
pub const COMMIT_DOCS: u64 = 20_000;
pub const ENQUEUE_INTERVAL: Duration = Duration::from_secs(5);
/// Keep at least this many jobs queued per enabled source.
pub const QUEUE_LOW_WATER: u64 = 2_000;
pub const ENQUEUE_BATCH: u32 = 10_000;
const IDLE_SLEEP: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkerCurrent {
    pub worker: String,
    pub source_id: SourceId,
    pub object_id: ObjectId,
    pub path: String,
    pub size: u64,
    pub started_ms_ago: u64,
}

#[derive(Debug, Default)]
pub struct ContentWorkersStatus {
    pub workers: AtomicUsize,
    pub current: Mutex<HashMap<String, (Instant, WorkerCurrent)>>,
    pub running_per_source: Mutex<HashMap<SourceId, u32>>,
    pub files_indexed: AtomicU64,
    pub files_unsupported: AtomicU64,
    pub files_failed: AtomicU64,
    pub files_skipped: AtomicU64,
    pub files_retried: AtomicU64,
    pub bytes_read: AtomicU64,
    pub chunks_written: AtomicU64,
    pub commits: AtomicU64,
    pub published: AtomicU64,
    pub enqueued: AtomicU64,
    pub last_commit_ms: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    pub pending_publish: Mutex<Vec<ObjectId>>,
    /// `(instant, bytes)` samples for the last minute of throughput.
    pub samples: Mutex<VecDeque<(Instant, u64)>>,
    pub started: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentWorkersView {
    pub workers: usize,
    pub current: Vec<WorkerCurrent>,
    pub files_indexed: u64,
    pub files_unsupported: u64,
    pub files_failed: u64,
    pub files_skipped: u64,
    pub files_retried: u64,
    pub bytes_read: u64,
    pub chunks_written: u64,
    pub commits: u64,
    pub published: u64,
    pub enqueued: u64,
    pub pending_publish: u64,
    pub uncommitted_documents: u64,
    pub last_commit_ms: u64,
    pub last_error: Option<String>,
    /// Bytes per second over the last 60 s.
    pub throughput_bytes_per_s: f64,
    pub uptime_s: u64,
}

impl ContentWorkersStatus {
    pub fn view(&self, uncommitted: u64) -> ContentWorkersView {
        let now = Instant::now();
        let current: Vec<WorkerCurrent> = self
            .current
            .lock()
            .values()
            .map(|(t, c)| WorkerCurrent {
                started_ms_ago: now.duration_since(*t).as_millis() as u64,
                ..c.clone()
            })
            .collect();
        let throughput = {
            let samples = self.samples.lock();
            let window = Duration::from_secs(60);
            let bytes: u64 = samples
                .iter()
                .filter(|(t, _)| now.duration_since(*t) <= window)
                .map(|(_, b)| *b)
                .sum();
            let span = samples
                .front()
                .map(|(t, _)| now.duration_since(*t).min(window).as_secs_f64())
                .unwrap_or(0.0)
                .max(1.0);
            bytes as f64 / span
        };
        ContentWorkersView {
            workers: self.workers.load(Ordering::Relaxed),
            current,
            files_indexed: self.files_indexed.load(Ordering::Relaxed),
            files_unsupported: self.files_unsupported.load(Ordering::Relaxed),
            files_failed: self.files_failed.load(Ordering::Relaxed),
            files_skipped: self.files_skipped.load(Ordering::Relaxed),
            files_retried: self.files_retried.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            chunks_written: self.chunks_written.load(Ordering::Relaxed),
            commits: self.commits.load(Ordering::Relaxed),
            published: self.published.load(Ordering::Relaxed),
            enqueued: self.enqueued.load(Ordering::Relaxed),
            pending_publish: self.pending_publish.lock().len() as u64,
            uncommitted_documents: uncommitted,
            last_commit_ms: self.last_commit_ms.load(Ordering::Relaxed),
            last_error: self.last_error.lock().clone(),
            throughput_bytes_per_s: throughput,
            uptime_s: self
                .started
                .lock()
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0),
        }
    }

    fn record_bytes(&self, bytes: u64) {
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
        let mut s = self.samples.lock();
        let now = Instant::now();
        s.push_back((now, bytes));
        while s
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(60))
        {
            s.pop_front();
        }
    }
}

/// Start `workers` extraction threads plus the coordinator.
pub fn spawn_content_workers(state: &Arc<AppState>, workers: usize) {
    let workers = workers.max(1);
    state
        .content_workers
        .workers
        .store(workers, Ordering::Relaxed);
    *state.content_workers.started.lock() = Some(Instant::now());
    match state.catalog.requeue_unfinished_content() {
        Ok(n) if n > 0 => tracing::warn!(
            n,
            "re-queued content records left `indexing` by a previous process"
        ),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "requeue_unfinished_content failed"),
    }
    for i in 0..workers {
        let st = state.clone();
        std::thread::Builder::new()
            .name(format!("content-{i}"))
            .spawn(move || worker_loop(&st, &format!("content-{i}")))
            .expect("spawn content worker");
    }
    let st = state.clone();
    std::thread::Builder::new()
        .name("content-coordinator".into())
        .spawn(move || coordinator_loop(&st))
        .expect("spawn content coordinator");
}

fn saturated_sources(state: &AppState) -> Vec<SourceId> {
    let running = state.content_workers.running_per_source.lock();
    let budgets = state.content_budgets.lock();
    running
        .iter()
        .filter(|(sid, n)| **n >= budgets.get(sid).copied().unwrap_or(2))
        .map(|(sid, _)| *sid)
        .collect()
}

/// Jobs claimed per worker round trip (all from one source).
pub const CLAIM_BATCH: u32 = 16;

fn worker_loop(state: &AppState, name: &str) {
    let status = &state.content_workers;
    let limits = Limits::default();
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            return;
        }
        if !state.content_enabled.load(Ordering::Relaxed) {
            std::thread::sleep(IDLE_SLEEP);
            continue;
        }
        let exclude = saturated_sources(state);
        let jobs =
            match state
                .catalog
                .claim_jobs(&[JobStage::ContentText], name, &exclude, CLAIM_BATCH)
            {
                Ok(j) if j.is_empty() => {
                    std::thread::sleep(IDLE_SLEEP);
                    continue;
                }
                Ok(j) => j,
                Err(e) => {
                    tracing::error!(error = %e, "claim_jobs failed");
                    *status.last_error.lock() = Some(e.to_string());
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
        let source = jobs[0].source_id;
        *status.running_per_source.lock().entry(source).or_default() += 1;
        for job in jobs {
            if state.shutdown.load(Ordering::Relaxed) {
                // Leave the rest `running`; startup re-queues them.
                break;
            }
            let object = match job.object_id {
                Some(o) => o,
                None => {
                    let _ = state.catalog.complete_job(job.id);
                    continue;
                }
            };
            let started = Instant::now();
            status.current.lock().insert(
                name.to_string(),
                (
                    started,
                    WorkerCurrent {
                        worker: name.to_string(),
                        source_id: job.source_id,
                        object_id: object,
                        path: String::new(),
                        size: job.estimated_cost,
                        started_ms_ago: 0,
                    },
                ),
            );
            let result = process_object(
                &state.catalog,
                &state.content_index,
                object,
                job.object_generation,
                &limits,
                Some(job.id),
            );
            status.current.lock().remove(name);
            let outcome = match result {
                Ok(ProcessResult::Indexed(st)) => {
                    status.files_indexed.fetch_add(1, Ordering::Relaxed);
                    status.record_bytes(st.bytes);
                    status
                        .chunks_written
                        .fetch_add(st.chunks as u64, Ordering::Relaxed);
                    status.pending_publish.lock().push(object);
                    Ok(())
                }
                Ok(ProcessResult::Done(st)) => {
                    if st.state == eidos_domain::ContentState::Failed {
                        status.files_failed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        status.files_unsupported.fetch_add(1, Ordering::Relaxed);
                    }
                    status.record_bytes(st.bytes);
                    Ok(())
                }
                Ok(ProcessResult::Skipped(why)) => {
                    tracing::debug!(object = object.0, why, "content job skipped");
                    status.files_skipped.fetch_add(1, Ordering::Relaxed);
                    state.catalog.complete_job(job.id)
                }
                Ok(ProcessResult::Disabled) => state.catalog.delete_job(job.id),
                Ok(ProcessResult::Retry { class, error }) => {
                    status.files_retried.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(object = object.0, %error, "content extraction will retry");
                    state.catalog.fail_job(job.id, class, &error).map(|_| ())
                }
                Err(e) => {
                    status.files_retried.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(object = object.0, error = %e, "content pipeline error");
                    state
                        .catalog
                        .fail_job(
                            job.id,
                            eidos_domain::FailureClass::Transient,
                            &e.to_string(),
                        )
                        .map(|_| ())
                }
            };
            if let Err(e) = outcome {
                tracing::error!(error = %e, "job bookkeeping failed");
                *status.last_error.lock() = Some(e.to_string());
            }
        }
        let mut r = status.running_per_source.lock();
        if let Some(n) = r.get_mut(&source) {
            *n = n.saturating_sub(1);
        }
    }
}

/// Commit + publish, and top up the queue.
fn coordinator_loop(state: &AppState) {
    let status = &state.content_workers;
    let mut last_commit = Instant::now();
    let mut last_enqueue = Instant::now() - ENQUEUE_INTERVAL;
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            // Final commit so finished files are not re-extracted at restart.
            let _ = commit_and_publish(state);
            return;
        }
        let uncommitted = state.content_index.uncommitted();
        let pending = status.pending_publish.lock().len();
        // `is_dirty`, not `uncommitted`: a reindex that produced no chunks
        // (the file turned binary, empty, unreadable, or unsupported) queues
        // only a deletion, and its old chunks stay searchable until it is
        // committed.
        if (pending > 0 || state.content_index.is_dirty())
            && (last_commit.elapsed() >= COMMIT_INTERVAL || uncommitted >= COMMIT_DOCS)
        {
            if let Err(e) = commit_and_publish(state) {
                tracing::error!(error = %e, "content index commit failed");
                *status.last_error.lock() = Some(e.to_string());
            }
            last_commit = Instant::now();
        }
        if state.content_enabled.load(Ordering::Relaxed)
            && last_enqueue.elapsed() >= ENQUEUE_INTERVAL
        {
            last_enqueue = Instant::now();
            if let Err(e) = top_up_queue(state) {
                tracing::error!(error = %e, "content enqueue failed");
                *status.last_error.lock() = Some(e.to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn commit_and_publish(state: &AppState) -> anyhow::Result<u64> {
    let status = &state.content_workers;
    let started = Instant::now();
    let objects: Vec<ObjectId> = std::mem::take(&mut *status.pending_publish.lock());
    match state.content_index.commit() {
        Ok(_) => {}
        Err(e) => {
            // Put the objects back; they stay `indexing` until a commit lands.
            status.pending_publish.lock().extend(objects);
            return Err(e.into());
        }
    }
    status.commits.fetch_add(1, Ordering::Relaxed);
    let n = state.catalog.mark_content_indexed(&objects)?;
    status.published.fetch_add(n, Ordering::Relaxed);
    status
        .last_commit_ms
        .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    if n > 0 {
        tracing::debug!(
            published = n,
            ms = started.elapsed().as_millis() as u64,
            "content published"
        );
    }
    Ok(n)
}

/// Enqueue pending objects for every enabled, published source whose queue
/// is below the low-water mark. Also refreshes per-source budgets.
pub fn top_up_queue(state: &AppState) -> anyhow::Result<u64> {
    let status = &state.content_workers;
    let by_source = state.catalog.jobs_by_source(JobStage::ContentText)?;
    let mut total = 0;
    let mut budgets = HashMap::new();
    for s in state.catalog.list_sources()? {
        budgets.insert(s.id, s.content_concurrency);
        if !s.content_enabled
            || s.published_generation.is_none()
            || matches!(s.state, SourceState::Retired | SourceState::Offline)
        {
            continue;
        }
        let queued = by_source.get(&s.id).map(|q| q.0).unwrap_or(0);
        if queued >= QUEUE_LOW_WATER {
            continue;
        }
        let n = state.catalog.enqueue_pending_content(s.id, ENQUEUE_BATCH)?;
        if n > 0 {
            tracing::info!(source = s.id.0, name = %s.name, enqueued = n, "content jobs enqueued");
        }
        total += n;
    }
    *state.content_budgets.lock() = budgets;
    status.enqueued.fetch_add(total, Ordering::Relaxed);
    Ok(total)
}
