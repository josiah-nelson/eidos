//! Content worker pool: claims `content_text` jobs, runs the extraction
//! pipeline, and publishes in batches after each content-index commit.
//!
//! One coordinator thread owns commits and publication; `workers` threads
//! extract concurrently, each respecting the per-source concurrency budget
//! (`sources.content_concurrency`) so a slow HDD or an SMB share cannot
//! starve NVMe sources. The coordinator also keeps the queue topped up from
//! objects whose `content_state` is `pending`/`stale`.
//!
//! A worker never claims work it has not already paid for: capacity is
//! reserved atomically inside the claiming transaction (see
//! [`reserve_and_claim`] and [`crate::source_budget`]), and the RAII
//! reservation is released when the batch ends, however it ends.
//!
//! Claiming is gated by three independent conditions — the process switch,
//! the operator pause, and an index rebuild that owns the writer. All three
//! stop *new claims* only; see [`crate::content_control`] for what the
//! operator sees and why the pause is durable.

use crate::source_budget::{SourceConcurrencyView, SourceReservation};
use crate::state::AppState;
use eidos_catalog::jobs::JobRecord;
use eidos_content::Limits;
use eidos_domain::{JobStage, ObjectId, SourceId, SourceState};
use eidos_search::pipeline::{process_object, ProcessResult};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ts_rs::TS;

pub const COMMIT_INTERVAL: Duration = Duration::from_secs(2);
pub const COMMIT_DOCS: u64 = 20_000;
pub const ENQUEUE_INTERVAL: Duration = Duration::from_secs(5);
/// Keep at least this many jobs queued per enabled source.
pub const QUEUE_LOW_WATER: u64 = 2_000;
pub const ENQUEUE_BATCH: u32 = 10_000;
const IDLE_SLEEP: Duration = Duration::from_millis(500);
const IDLE_FALLBACK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, serde::Serialize, TS)]
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
    /// Desired pool size. A worker whose index is at or past this parks
    /// instead of claiming; resized at runtime by [`resize_workers`].
    pub workers: AtomicUsize,
    /// Threads actually spawned this process. Never shrinks: surplus
    /// threads park until the pool grows over them again.
    pub spawned: AtomicUsize,
    pub current: Mutex<HashMap<String, (Instant, WorkerCurrent)>>,
    /// Per-source concurrency budgets and the reservations held against
    /// them. Workers take capacity from here before claiming.
    pub budgets: Arc<crate::source_budget::SourceBudgets>,
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
    /// Persistent publication faults stop new extraction until a retry lands.
    pub publication_blocked: AtomicBool,
    /// A commit/reader-reload failure needs the index step retried even when
    /// it left no dirty operations. Catalog-only retries need no extra commit.
    retry_index_commit: AtomicBool,
    /// `(instant, bytes)` samples for the last minute of throughput.
    pub samples: Mutex<VecDeque<(Instant, u64)>>,
    pub started: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone, serde::Serialize, TS)]
pub struct ContentWorkersView {
    pub workers: usize,
    pub current: Vec<WorkerCurrent>,
    /// Per-source budget, live reservations, and the high-water mark.
    pub concurrency: Vec<SourceConcurrencyView>,
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
            concurrency: self.budgets.snapshot(),
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
    let workers = workers.clamp(1, MAX_WORKERS);
    state
        .content_workers
        .workers
        .store(workers, Ordering::Relaxed);
    state
        .content_workers
        .spawned
        .store(workers, Ordering::Relaxed);
    *state.content_workers.started.lock() = Some(Instant::now());
    // Install persisted budgets *before* the first worker can claim, or a
    // source configured below the default would be oversubscribed for the
    // few seconds until the coordinator's first refresh. If this fails the
    // pool still starts, but every source stays unknown and admits nothing
    // until the coordinator gets the policy through.
    if let Err(e) = refresh_budgets(state) {
        tracing::error!(
            error = %e,
            "loading content concurrency budgets failed; workers idle until policy loads"
        );
    }
    for i in 0..workers {
        let st = state.clone();
        std::thread::Builder::new()
            .name(format!("content-{i}"))
            .spawn(move || worker_loop(&st, i, &format!("content-{i}")))
            .expect("spawn content worker");
    }
    let st = state.clone();
    std::thread::Builder::new()
        .name("content-coordinator".into())
        .spawn(move || coordinator_loop(&st))
        .expect("spawn content coordinator");
}

/// One file per worker round trip: pause/shrink drains at most the current
/// file, never a preclaimed backlog of up to sixteen large files per worker.
pub const CLAIM_BATCH: u32 = 1;

/// Durable operator override for the pool size, next to the pause marker.
pub const WORKERS_MARKER: &str = "content-workers.json";

/// Upper bound accepted by [`resize_workers`] (and the UI input).
pub const MAX_WORKERS: usize = 64;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct WorkersMarker {
    workers: usize,
}

/// The operator-chosen pool size persisted in `data_dir`, if any. An
/// unreadable or unparsable marker is ignored: unlike the pause marker
/// there is nothing unsafe about falling back to the configured size.
pub fn load_workers_override(data_dir: &std::path::Path) -> Option<usize> {
    let raw = std::fs::read(data_dir.join(WORKERS_MARKER)).ok()?;
    let marker: WorkersMarker = serde_json::from_slice(&raw).ok()?;
    Some(marker.workers.clamp(1, MAX_WORKERS))
}

/// Resize the global pool at runtime and return the effective size.
///
/// Growing first creates the missing threads in a parked state, then writes
/// [`WORKERS_MARKER`] and exposes the new desired size. A spawn or marker
/// failure therefore retains the previous effective size, while a retry
/// continues after any threads already created. Shrinking writes the marker
/// before parking the surplus after their current batch. Nothing in flight is
/// interrupted, and per-source budgets still apply on top.
pub fn resize_workers(state: &Arc<AppState>, workers: usize) -> std::io::Result<usize> {
    resize_workers_with(state, workers, spawn_worker_thread)
}

fn spawn_worker_thread(index: usize, state: Arc<AppState>) -> io::Result<()> {
    std::thread::Builder::new()
        .name(format!("content-{index}"))
        .spawn(move || worker_loop(&state, index, &format!("content-{index}")))?;
    Ok(())
}

/// Resize with an injectable thread spawner so partial operating-system
/// failures are testable. Newly spawned workers park behind the old desired
/// size until every requested thread and the durable marker are ready. A
/// failed grow can therefore leave extra parked threads, but it cannot claim
/// work at an uncommitted size or make restart advertise a size that never
/// came into existence.
fn resize_workers_with<F>(state: &Arc<AppState>, workers: usize, mut spawn: F) -> io::Result<usize>
where
    F: FnMut(usize, Arc<AppState>) -> io::Result<()>,
{
    // The control/claim admission gate serialises resizes: two overlapping
    // calls would otherwise read the same `spawned` count and spawn the
    // same worker indices twice, exceeding the cap they just agreed on.
    let _admission = state.content_pause.admission_guard();
    let workers = workers.clamp(1, MAX_WORKERS);
    let status = &state.content_workers;
    let spawned = status.spawned.load(Ordering::Relaxed);

    // Grow the parked high-water mark first, advancing `spawned` after each
    // successful thread. If a later spawn fails, retry resumes at the first
    // missing index instead of duplicating the threads already created.
    for i in spawned..workers {
        spawn(i, state.clone())?;
        status.spawned.store(i + 1, Ordering::Relaxed);
    }

    // Only a fully realizable size becomes durable and visible. Marker
    // failure is also safe: any just-created threads remain parked because
    // `workers` still holds the previous desired size.
    let path = state.data_dir.join(WORKERS_MARKER);
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(&WorkersMarker { workers }).map_err(io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    status.workers.store(workers, Ordering::Relaxed);
    state.content_pause.work.notify_all();
    tracing::info!(workers, "content worker pool resized");
    Ok(workers)
}

/// Whether a worker may claim a new batch right now.
///
/// Three independent conditions stop claiming, and every one of them stops
/// *only* claiming:
///
/// - `--no-content`, the process switch chosen at start-up;
/// - an operator pause, which is durable (see [`crate::content_control`]);
/// - a content-index rebuild, which owns the index writer — a job claimed
///   during one would sit on that gate holding its source budget.
///
/// A batch claimed a moment before any of them turned on is already past
/// this check and runs to completion in [`run_batch`]: it commits and
/// publishes normally, is never left stranded in `running` for startup
/// recovery to repair, and the extraction already paid for is never
/// discarded. This is the whole reason the gate is here, in front of the
/// claim, rather than inside the batch loop.
pub fn claiming_allowed(state: &AppState) -> bool {
    state.content_enabled.load(Ordering::Relaxed)
        && !state.content_pause.is_paused()
        && !state.content_index.is_rebuilding()
        && admission_blocked_reason(state).is_none()
}

pub fn admission_blocked_reason(state: &AppState) -> Option<String> {
    if state
        .content_workers
        .publication_blocked
        .load(Ordering::Acquire)
    {
        Some(
            "content publication failed; extraction waits for the automatic retry (see last error)"
                .into(),
        )
    } else {
        state.resources.blocked_reason()
    }
}

/// Reserve one unit of a source's content budget and claim a batch from
/// exactly that source.
///
/// The reservation is taken inside the claiming transaction, before any job
/// is marked `running`, so the budget can never be oversubscribed by workers
/// racing on a stale count. Sources with no free capacity are skipped in
/// favour of the next eligible one, so a saturated source (an HDD pinned at
/// one reader) does not hold up the rest of the pool.
///
/// `Ok(None)` means no source has due work with capacity to spare; nothing
/// is reserved in that case. The global gates and active source scans are
/// checked under the same admission lock that serialises pause transitions
/// and scan registration, so neither can race this claim. Dropping the
/// returned guard releases the unit.
pub fn reserve_and_claim(
    state: &AppState,
    worker: &str,
    limit: u32,
) -> eidos_catalog::Result<Option<(ContentReservation, Vec<JobRecord>)>> {
    let _admission = state.content_pause.admission_guard();
    if !claiming_allowed(state) {
        return Ok(None);
    }
    // Scan starts also hold the admission gate while inserting their
    // progress entry. A finished entry is harmless and does not keep its
    // source idle during the 30-second diagnostics retention window, and
    // neither does one still queued for a scan slot: an unadmitted scan has
    // opened no generation and read nothing from the source.
    let active_scans: HashSet<SourceId> = state
        .scans
        .lock()
        .iter()
        .filter_map(|(source, progress)| {
            (progress.is_admitted() && !progress.is_finished()).then_some(*source)
        })
        .collect();
    let budgets = state.content_workers.budgets.clone();
    let mut admit = |source: SourceId| {
        if active_scans.contains(&source) {
            return None;
        }
        // Check the shared-device gate before charging the source budget. A
        // device refusal holds every source at once, and a source unit taken
        // and immediately dropped here would still raise that source's peak
        // reservation for work the device gate is what actually held.
        state
            .devices
            .would_admit(source.0, crate::device_budget::WorkKind::Content, 1)
            .ok()?;
        let source_reservation = budgets.try_reserve(source)?;
        let device = state
            .devices
            .try_reserve(source.0, crate::device_budget::WorkKind::Content, 1)
            .ok()?;
        Some(ContentReservation {
            _source: source_reservation,
            _device: device,
        })
    };
    let mut claimed =
        state
            .catalog
            .claim_jobs_admitted(&[JobStage::ContentText], worker, limit, &mut admit)?;
    // Count the units only once the claim is durable. `admit` runs inside the
    // claiming transaction: a losing racer for the last device slot, and a
    // transaction that then fails, both release without ever reading a file,
    // and neither should leave a peak behind describing work nobody did.
    if let Some((reservation, _)) = claimed.as_mut() {
        reservation.confirm();
    }
    Ok(claimed)
}

pub struct ContentReservation {
    _source: SourceReservation,
    _device: crate::device_budget::DeviceLease,
}

impl ContentReservation {
    /// Count these units towards the reported high-water marks. The caller
    /// does this once the claim has committed, never from inside `admit`.
    fn confirm(&mut self) {
        self._source.confirm();
    }

    pub fn source(&self) -> SourceId {
        self._source.source()
    }
}

fn worker_loop(state: &AppState, index: usize, name: &str) {
    let status = &state.content_workers;
    let limits = Limits::default();
    loop {
        let observed = state.content_pause.work.epoch();
        if state.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // A worker at or past the desired pool size parks: shrinking never
        // interrupts a claimed batch, and growth reuses the parked thread.
        if index >= status.workers.load(Ordering::Relaxed) {
            // Only a control change can make this thread eligible again, so
            // it parks where no work hint can be spent on it.
            state
                .content_pause
                .work
                .wait_for_control(observed, IDLE_FALLBACK);
            continue;
        }
        let (reservation, jobs) = match reserve_and_claim(state, name, CLAIM_BATCH) {
            Ok(Some(claimed)) => {
                // A claim that got through is the only evidence admission is
                // letting content work in right now, so hand the baton to one
                // more parked worker before extracting. A real backlog fills
                // the pool in a chain; one admission refuses stops here.
                state.content_pause.work.notify_one();
                claimed
            }
            Ok(None) => {
                state.content_pause.work.wait(observed, IDLE_FALLBACK);
                continue;
            }
            Err(e) => {
                tracing::error!(error = %e, "claim_jobs failed");
                *status.last_error.lock() = Some(e.to_string());
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        run_batch(state, name, &limits, jobs);
        // Released here on every path, and while unwinding from a panic.
        drop(reservation);
    }
}

/// Extract one claimed batch. All jobs belong to one source, whose budget
/// the caller holds a reservation for.
fn run_batch(state: &AppState, name: &str, limits: &Limits, jobs: Vec<JobRecord>) {
    let status = &state.content_workers;
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
            limits,
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
}

/// Commit + publish, and top up the queue.
fn coordinator_loop(state: &AppState) {
    let status = &state.content_workers;
    let mut last_commit = Instant::now();
    let mut last_enqueue = Instant::now() - ENQUEUE_INTERVAL;
    let mut last_wake = Instant::now() - IDLE_SLEEP;
    loop {
        state.resources.refresh_disk();
        if state.shutdown.load(Ordering::Relaxed) {
            // Final commit so finished files are not re-extracted at restart.
            let _ = commit_and_publish(state);
            return;
        }
        let uncommitted = state.content_index.uncommitted();
        let pending = status.pending_publish.lock().len();
        // A rebuild owns the writer; commits resume when it is done.
        let rebuilding = state.content_index.is_rebuilding();
        if !rebuilding {
            if let Err(e) = apply_policies_once(state) {
                *status.last_error.lock() = Some(format!("exclusion application: {e}"));
            }
        }
        // `is_dirty`, not `uncommitted`: a reindex that produced no chunks
        // (the file turned binary, empty, unreadable, or unsupported) queues
        // only a deletion, and its old chunks stay searchable until it is
        // committed.
        if !rebuilding
            && (pending > 0
                || state.content_index.is_dirty()
                || status.publication_blocked.load(Ordering::Acquire))
            && (last_commit.elapsed() >= COMMIT_INTERVAL
                || (uncommitted >= COMMIT_DOCS
                    && !status.publication_blocked.load(Ordering::Acquire)))
        {
            if let Err(e) = commit_and_publish(state) {
                tracing::error!(error = %e, "content index commit failed");
                *status.last_error.lock() = Some(e.to_string());
            }
            last_commit = Instant::now();
        }
        // Commits above run regardless: a paused pipeline must still publish
        // what its draining workers extracted.
        if state.content_enabled.load(Ordering::Relaxed)
            && last_enqueue.elapsed() >= ENQUEUE_INTERVAL
        {
            last_enqueue = Instant::now();
            // Topping the queue up walks every source and writes new job
            // rows — exactly the catalog load an operator pauses to stop.
            // The backlog is durable, so it is still there at resume.
            //
            // Budgets are reconciled either way. A `content_concurrency`
            // change made while paused has to be in force the moment
            // claiming resumes, not up to ENQUEUE_INTERVAL afterwards, and
            // a source added while paused must have a budget to admit work
            // against. A pause stops claiming; it is not a reason for the
            // rest of the pipeline's bookkeeping to go stale.
            // The check and the catalog work share the control/claim gate.
            // Once a pause response completes, a coordinator that observed
            // the old state cannot still top the queue up behind it.
            let _admission = state.content_pause.admission_guard();
            let outcome = if !claiming_allowed(state) {
                refresh_budgets(state).map(|_| 0)
            } else {
                top_up_queue(state)
            };
            if let Err(e) = outcome {
                tracing::error!(error = %e, "content enqueue failed");
                *status.last_error.lock() = Some(e.to_string());
            }
        }
        // One bounded read-only readiness check for the whole pool. No empty
        // writer claims or catalog-follower wakeups per idle worker. Use the
        // existing coordinator cadence; do not add another idle timer/thread.
        if last_wake.elapsed() >= IDLE_SLEEP {
            last_wake = Instant::now();
            wake_due_workers(state);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wake_due_workers(state: &AppState) {
    if !claiming_allowed(state) {
        return;
    }
    match state
        .catalog
        .has_due_jobs(JobStage::ContentText, eidos_domain::UnixNanos::now())
    {
        // One worker, not the pool: the readiness row may still be refused by
        // a source budget, a device lease, an active scan or an unapplied
        // policy, and waking every worker into the serialized writer path for
        // that is the exact cost this change exists to remove. The claiming
        // worker wakes the next one, so an admittable backlog still ramps up.
        Ok(true) => state.content_pause.work.notify_one(),
        Ok(false) => {}
        Err(error) => {
            *state.content_workers.last_error.lock() = Some(format!("content readiness: {error}"));
        }
    }
}

pub fn commit_and_publish(state: &AppState) -> anyhow::Result<u64> {
    let status = &state.content_workers;
    let started = Instant::now();
    let objects: Vec<ObjectId> = std::mem::take(&mut *status.pending_publish.lock());
    // Pending IDs were queued only after their index operations. If those
    // operations were already committed, retry only the catalog acknowledgement
    // instead of rewriting/syncing clean index metadata every two seconds.
    if state.content_index.is_dirty() || status.retry_index_commit.load(Ordering::Acquire) {
        match state.content_index.commit() {
            Ok(_) => {}
            Err(e) => {
                status.pending_publish.lock().extend(objects);
                status.retry_index_commit.store(true, Ordering::Release);
                status.publication_blocked.store(true, Ordering::Release);
                return Err(e.into());
            }
        }
        status.retry_index_commit.store(false, Ordering::Release);
        status.commits.fetch_add(1, Ordering::Relaxed);
    }
    let n = match state.catalog.mark_content_indexed(&objects) {
        Ok(n) => n,
        Err(error) => {
            // The index commit succeeded but the catalog acknowledgement did
            // not. Keep IDs for a later coordinator attempt, merging them with
            // anything workers finished while the commit was in progress.
            status.pending_publish.lock().extend(objects);
            status.publication_blocked.store(true, Ordering::Release);
            return Err(error.into());
        }
    };
    status.published.fetch_add(n, Ordering::Relaxed);
    status.publication_blocked.store(false, Ordering::Release);
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

/// A bounded application turn, independent of content pause/enable controls.
/// Explicit policy changes must also work for metadata-only sources.
pub fn apply_policies_once(state: &AppState) -> anyhow::Result<()> {
    for source in state.catalog.pending_policy_sources()? {
        let result = (|| -> anyhow::Result<()> {
            if state
                .catalog
                .active_job_counts(source, JobStage::ContentText)?
                .1
                > 0
            {
                return Ok(());
            }
            state.catalog.apply_policy_batch(source)?;
            let deletes = state.catalog.policy_cleanup_batch(source)?;
            if !deletes.is_empty() {
                for id in &deletes {
                    state.content_index.delete_object(*id);
                }
                commit_and_publish(state)?;
            }
            state.catalog.acknowledge_policy_cleanup(source, &deletes)?;
            Ok(())
        })();
        if let Err(error) = result {
            state
                .catalog
                .set_policy_error(source, Some(&error.to_string()))?;
        }
    }
    Ok(())
}

/// Enqueue pending objects for every enabled, published source whose queue
/// is below the low-water mark. Also refreshes per-source budgets.
pub fn top_up_queue(state: &AppState) -> anyhow::Result<u64> {
    let status = &state.content_workers;
    let by_source = state.catalog.jobs_by_source(JobStage::ContentText)?;
    let mut total = 0;
    let mut device_work = false;
    for s in refresh_budgets(state)? {
        if !s.content_enabled
            || s.published_generation.is_none()
            || matches!(s.state, SourceState::Retired | SourceState::Offline)
            || s.kind.is_remote()
        {
            continue;
        }
        let queued = by_source.get(&s.id).map(|q| q.0).unwrap_or(0);
        device_work |= by_source.get(&s.id).is_some_and(|q| q.0 > 0 || q.1 > 0);
        if queued >= QUEUE_LOW_WATER {
            continue;
        }
        let n = state.catalog.enqueue_pending_content(s.id, ENQUEUE_BATCH)?;
        if n > 0 {
            tracing::info!(source = s.id.0, name = %s.name, enqueued = n, "content jobs enqueued");
        }
        total += n;
    }
    status.enqueued.fetch_add(total, Ordering::Relaxed);
    if device_work || total > 0 {
        state.devices.refresh();
    }
    Ok(total)
}

/// Load every source's `content_concurrency` into the reservation table and
/// return the sources. Live reservations are preserved, so this is safe to
/// call while workers are running; it is called once before the pool starts
/// and again on every enqueue interval.
pub fn refresh_budgets(state: &AppState) -> anyhow::Result<Vec<eidos_catalog::SourceRecord>> {
    let sources = state.catalog.list_sources()?;
    let budgets: HashMap<SourceId, u32> = sources
        .iter()
        .map(|s| (s.id, s.content_concurrency))
        .collect();
    state.content_workers.budgets.set_all(&budgets);
    state.devices.set_sources(
        sources
            .iter()
            .filter(|source| !source.kind.is_remote() && source.state != SourceState::Retired)
            .map(|source| (source.id.0, std::path::PathBuf::from(&source.root_path)))
            .collect(),
    );
    Ok(sources)
}

#[cfg(test)]
mod resize_tests {
    use super::*;
    use crate::ServiceConfig;

    #[test]
    fn partial_grow_failure_keeps_the_old_size_and_retry_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(
            AppState::open(&ServiceConfig {
                data_dir: dir.path().join("data"),
                auto_reconcile: false,
                content_workers: 2,
                ..Default::default()
            })
            .unwrap(),
        );

        // Establish the same two-thread state startup would have, without
        // creating real background threads in this focused failure test.
        resize_workers_with(&state, 2, |_index, _state| Ok(())).unwrap();
        assert_eq!(state.content_workers.workers.load(Ordering::Relaxed), 2);
        assert_eq!(state.content_workers.spawned.load(Ordering::Relaxed), 2);

        let mut attempted = Vec::new();
        let error = resize_workers_with(&state, 6, |index, _state| {
            attempted.push(index);
            if index == 4 {
                Err(io::Error::other("injected spawn failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "injected spawn failure");
        assert_eq!(attempted, vec![2, 3, 4]);
        assert_eq!(state.content_workers.spawned.load(Ordering::Relaxed), 4);
        assert_eq!(state.content_workers.workers.load(Ordering::Relaxed), 2);
        assert_eq!(load_workers_override(&state.data_dir), Some(2));

        let mut retried = Vec::new();
        assert_eq!(
            resize_workers_with(&state, 6, |index, _state| {
                retried.push(index);
                Ok(())
            })
            .unwrap(),
            6
        );
        assert_eq!(retried, vec![4, 5]);
        assert_eq!(state.content_workers.spawned.load(Ordering::Relaxed), 6);
        assert_eq!(state.content_workers.workers.load(Ordering::Relaxed), 6);
        assert_eq!(load_workers_override(&state.data_dir), Some(6));
        state.request_shutdown();
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use crate::ServiceConfig;
    use std::time::Instant;

    fn await_condition(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "worker condition timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct Fixture {
        state: Arc<AppState>,
        _dir: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let state = Arc::new(
                AppState::open(&ServiceConfig {
                    data_dir: dir.path().join("data"),
                    auto_reconcile: false,
                    fleet: false,
                    update_check: false,
                    content_workers: 8,
                    ..Default::default()
                })
                .unwrap(),
            );
            Self { state, _dir: dir }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.state.request_shutdown();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Arc::strong_count(&self.state) > 1 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            // Preserve the original assertion if a test is already unwinding.
            if !std::thread::panicking() {
                assert_eq!(Arc::strong_count(&self.state), 1, "workers did not stop");
            }
        }
    }

    #[test]
    fn eight_drained_workers_park_without_repeated_writer_claims_and_shutdown_wakes_them() {
        let f = Fixture::new();
        spawn_content_workers(&f.state, 8);
        await_condition(|| f.state.content_pause.work.waiting() == 8);
        let before = f.state.catalog.writer_stats().acquisitions;
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(f.state.content_pause.work.waiting(), 8);
        assert_eq!(f.state.catalog.writer_stats().acquisitions, before);
        // Drop verifies shutdown wakes the parked threads without their 30s timeout.
    }

    #[test]
    fn a_job_added_after_parking_wakes_through_the_coordinator_and_resume_is_prompt() {
        use eidos_catalog::{jobs::NewJob, NewSource};
        use eidos_domain::{JobState, Priority, SourceKind};
        let f = Fixture::new();
        let source = f
            .state
            .catalog
            .add_source(&NewSource {
                host_id: f.state.host_id,
                name: "idle fixture".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: "synthetic-idle-root".into(),
                aliases: vec![],
            })
            .unwrap();
        spawn_content_workers(&f.state, 8);
        await_condition(|| f.state.content_pause.work.waiting() == 8);
        let enqueue = |key: &str| {
            f.state
                .catalog
                .enqueue(&NewJob {
                    source_id: source,
                    object_id: None,
                    object_generation: 1,
                    stage: JobStage::ContentText,
                    priority: Priority::NormalText,
                    idempotency_key: key.into(),
                    payload: None,
                    estimated_cost: 0,
                })
                .unwrap()
                .unwrap()
        };
        let first = enqueue("after parking");
        await_condition(|| {
            f.state.catalog.get_job(first).unwrap().unwrap().state == JobState::Done
        });
        f.state.content_pause.set_paused(true).unwrap();
        await_condition(|| f.state.content_pause.work.waiting() == 8);
        let second = enqueue("while paused");
        std::thread::sleep(Duration::from_millis(750));
        assert_eq!(
            f.state.catalog.get_job(second).unwrap().unwrap().state,
            JobState::Queued
        );
        f.state.content_pause.set_paused(false).unwrap();
        await_condition(|| {
            f.state.catalog.get_job(second).unwrap().unwrap().state == JobState::Done
        });

        // A future retry must become runnable without another catalog write or
        // a control notification at its deadline (and without the 30s fallback).
        f.state.content_pause.set_paused(true).unwrap();
        let future = enqueue("future retry");
        f.state
            .catalog
            .with_writer(|conn| {
                conn.execute(
                    "UPDATE jobs SET scheduled_at = ?1 WHERE job_id = ?2",
                    [eidos_domain::UnixNanos::now().0 + 1_000_000_000, future.0],
                )?;
                Ok(())
            })
            .unwrap();
        f.state.content_pause.set_paused(false).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            f.state.catalog.get_job(future).unwrap().unwrap().state,
            JobState::Queued
        );
        await_condition(|| {
            f.state.catalog.get_job(future).unwrap().unwrap().state == JobState::Done
        });
    }

    /// The surplus branch a shrink creates has no other wakeup: without the
    /// resize notification a regrown pool would wait out the 30-second
    /// fallback before its parked thread claimed anything.
    #[test]
    fn growing_the_pool_wakes_a_parked_surplus_worker() {
        use eidos_catalog::{jobs::NewJob, NewSource};
        use eidos_domain::{JobState, Priority, SourceKind};
        let f = Fixture::new();
        let source = f
            .state
            .catalog
            .add_source(&NewSource {
                host_id: f.state.host_id,
                name: "surplus fixture".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: "synthetic-surplus-root".into(),
                aliases: vec![],
            })
            .unwrap();
        refresh_budgets(&f.state).unwrap();
        let job = f
            .state
            .catalog
            .enqueue(&NewJob {
                source_id: source,
                object_id: None,
                object_generation: 1,
                stage: JobStage::ContentText,
                priority: Priority::NormalText,
                idempotency_key: "surplus".into(),
                payload: None,
                estimated_cost: 0,
            })
            .unwrap()
            .unwrap();

        // One desired worker but two threads' worth of index space: index 1 is
        // exactly what a shrink leaves parked. No coordinator is started here,
        // so the readiness poll cannot stand in for the resize notification.
        f.state.content_workers.workers.store(1, Ordering::Relaxed);
        f.state.content_workers.spawned.store(2, Ordering::Relaxed);
        let st = f.state.clone();
        std::thread::Builder::new()
            .name("content-1".into())
            .spawn(move || worker_loop(&st, 1, "content-1"))
            .unwrap();
        await_condition(|| f.state.content_pause.work.waiting() == 1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            f.state.catalog.get_job(job).unwrap().unwrap().state,
            JobState::Queued,
            "a parked surplus worker must not claim"
        );

        assert_eq!(resize_workers(&f.state, 2).unwrap(), 2);
        await_condition(|| f.state.catalog.get_job(job).unwrap().unwrap().state == JobState::Done);
    }

    /// Readiness stays true forever when a due backlog cannot pass admission.
    /// Waking the whole pool for that would put every worker back on the
    /// serialized writer path twice a second — the polling this change
    /// removes — so the hint must cost one empty claim, not one per worker.
    #[test]
    fn a_backlog_admission_refuses_costs_one_empty_claim_per_readiness_hint() {
        use eidos_catalog::{jobs::NewJob, NewSource};
        use eidos_domain::{JobState, Priority, SourceKind};
        let f = Fixture::new();
        let source = f
            .state
            .catalog
            .add_source(&NewSource {
                host_id: f.state.host_id,
                name: "refused fixture".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: "synthetic-refused-root".into(),
                aliases: vec![],
            })
            .unwrap();
        // Pin the source to one unit and hold that unit for the whole test,
        // so every claim reaches the writer and comes back empty. Budget
        // refreshes preserve live reservations, so the coordinator's periodic
        // refresh cannot hand the pool a second unit.
        f.state
            .catalog
            .with_writer(|conn| {
                conn.execute(
                    "UPDATE sources SET content_concurrency = 1 WHERE source_id = ?1",
                    [source.0],
                )?;
                Ok(())
            })
            .unwrap();
        refresh_budgets(&f.state).unwrap();
        let _held = f
            .state
            .content_workers
            .budgets
            .try_reserve(source)
            .expect("the fixture holds the only content unit");
        let queued: Vec<_> = (0..4)
            .map(|i| {
                f.state
                    .catalog
                    .enqueue(&NewJob {
                        source_id: source,
                        object_id: None,
                        object_generation: 1,
                        stage: JobStage::ContentText,
                        priority: Priority::NormalText,
                        idempotency_key: format!("refused-{i}"),
                        payload: None,
                        estimated_cost: 0,
                    })
                    .unwrap()
                    .unwrap()
            })
            .collect();
        spawn_content_workers(&f.state, 8);
        // One worker may be mid-hint at any moment; the rest must be parked.
        await_condition(|| f.state.content_pause.work.waiting() >= 7);

        let window = Duration::from_millis(2_000);
        let before = f.state.catalog.writer_stats().acquisitions;
        std::thread::sleep(window);
        let claims = f.state.catalog.writer_stats().acquisitions - before;
        // Readiness must have stayed true for the whole window, or the count
        // above would be low for the wrong reason.
        for job in queued {
            assert_eq!(
                f.state.catalog.get_job(job).unwrap().unwrap().state,
                JobState::Queued,
                "admission must have refused every job for the whole window"
            );
        }
        // Four hints fit in the window. Waking all eight workers for each of
        // them would take roughly thirty writer turns instead.
        assert!(
            claims <= 12,
            "{claims} empty writer claims in {window:?} for a refused backlog"
        );
    }

    /// A shrink can leave far more parked threads than the pool admits. A
    /// single-worker hint spent on one of those does nothing, and the due job
    /// then waits for the next hint — sixty-three times over for a pool cut
    /// from sixty-four to one.
    #[test]
    fn a_work_hint_reaches_the_eligible_worker_behind_a_queue_of_surplus_ones() {
        use eidos_catalog::{jobs::NewJob, NewSource};
        use eidos_domain::{JobState, Priority, SourceKind};
        let f = Fixture::new();
        let source = f
            .state
            .catalog
            .add_source(&NewSource {
                host_id: f.state.host_id,
                name: "hint fixture".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: "synthetic-hint-root".into(),
                aliases: vec![],
            })
            .unwrap();
        refresh_budgets(&f.state).unwrap();

        // A pool of one with eight threads alive: indices 1..8 are surplus.
        f.state.content_workers.workers.store(1, Ordering::Relaxed);
        f.state.content_workers.spawned.store(8, Ordering::Relaxed);
        let spawn = |index: usize| {
            let st = f.state.clone();
            std::thread::Builder::new()
                .name(format!("content-{index}"))
                .spawn(move || worker_loop(&st, index, &format!("content-{index}")))
                .unwrap();
        };
        // Park the surplus threads first so they sit ahead of the one worker
        // that can claim: a shared wait set would hand the hint to them.
        for index in 1..8 {
            spawn(index);
        }
        await_condition(|| f.state.content_pause.work.waiting() == 7);
        spawn(0);
        await_condition(|| f.state.content_pause.work.waiting() == 8);

        let job = f
            .state
            .catalog
            .enqueue(&NewJob {
                source_id: source,
                object_id: None,
                object_generation: 1,
                stage: JobStage::ContentText,
                priority: Priority::NormalText,
                idempotency_key: "behind the surplus".into(),
                payload: None,
                estimated_cost: 0,
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            f.state.catalog.get_job(job).unwrap().unwrap().state,
            JobState::Queued
        );
        // No coordinator runs here, so this is the only hint the pool gets.
        wake_due_workers(&f.state);
        await_condition(|| f.state.catalog.get_job(job).unwrap().unwrap().state == JobState::Done);
    }
}
