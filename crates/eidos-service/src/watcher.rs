//! Change-feed watchers and periodic reconciliation.
//!
//! One thread per native (USN) source polls the journal from the stored
//! checkpoint, translates records, and applies them with the checkpoint in
//! the same catalog transaction. Overflow or an invalid journal ID marks the
//! source `degraded`, clears the checkpoint, and triggers a reconciliation
//! scan, after which watching resumes from a fresh checkpoint. Sources
//! without a live feed (SMB, generic, or journal unavailable) are rescanned
//! periodically by the reconciler thread.
//!
//! The native scan sequence is checkpoint → enumerate → replay the records
//! that overlapped enumeration into the still-open generation → publish the
//! generation together with the checkpoint in one transaction → watch. The
//! source stays `enumerating`/`reconciling` until that final transaction, so
//! it is never advertised as complete while overlapping changes are pending.

use crate::scanner::{self, ScanProgress};
use crate::state::AppState;
use eidos_catalog::changes::{ChangeEvent, Checkpoint};
use eidos_catalog::scan::{PublishOptions, ScanSession, ScanSummary};
use eidos_domain::{SourceId, SourceKind, SourceState, UnixNanos};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ts_rs::TS;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const DEFAULT_RECONCILE_INTERVAL_S: i64 = 6 * 3600;
/// Minimum pause before a watcher retries a reconciliation that failed.
pub const RECONCILE_RETRY_DELAY: Duration = Duration::from_secs(30);

/// The feed this build watches with. A host has exactly one.
#[cfg(target_os = "macos")]
const FEED: WatcherFeed = WatcherFeed::MacosFsEvents;
#[cfg(not(target_os = "macos"))]
const FEED: WatcherFeed = WatcherFeed::WindowsUsn;

/// Which native feed a watcher is driven by. The position below is a
/// position *in that feed*: a USN is a byte offset in a journal and an
/// FSEvents id is a per-volume event counter, and neither is comparable to
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum WatcherFeed {
    WindowsUsn,
    /// Spelled the way the volume record spells it, not the way Rust
    /// capitalises it, so one name means one thing across the wire.
    #[serde(rename = "macos_fsevents")]
    MacosFsEvents,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum WatcherState {
    Starting,
    Live,
    Reconciling,
    Stopped,
}

pub struct WatcherStatus {
    pub source_id: SourceId,
    pub cancel: AtomicBool,
    /// Serialises cancellation with watcher-owned catalog mutations. Once
    /// `request_cancel` returns, no later watcher mutation can begin.
    mutation_gate: Mutex<()>,
    state: Mutex<(WatcherState, Option<String>)>,
    pub started: Instant,
    pub batches: AtomicU64,
    pub events: AtomicU64,
    pub records: AtomicU64,
    pub reconciles: AtomicU64,
    /// Latest position applied from the native feed.
    pub last_position: AtomicI64,
    last_batch: Mutex<Option<Instant>>,
    last_apply_ms: AtomicU64,
    #[cfg(windows)]
    journal_cancel: eidos_scanner::usn::JournalCancellation,
}

#[derive(Debug, Clone, serde::Serialize, TS)]
pub struct WatcherView {
    pub state: WatcherState,
    pub live: bool,
    pub detail: Option<String>,
    pub feed: WatcherFeed,
    pub batches: u64,
    pub events: u64,
    pub records: u64,
    pub reconciles: u64,
    pub last_position: i64,
    pub last_batch_ms_ago: Option<u64>,
    pub last_apply_ms: u64,
    pub uptime_s: u64,
}

impl WatcherStatus {
    fn new(source_id: SourceId) -> Self {
        Self {
            source_id,
            cancel: AtomicBool::new(false),
            mutation_gate: Mutex::new(()),
            state: Mutex::new((WatcherState::Starting, None)),
            started: Instant::now(),
            batches: AtomicU64::new(0),
            events: AtomicU64::new(0),
            records: AtomicU64::new(0),
            reconciles: AtomicU64::new(0),
            last_position: AtomicI64::new(0),
            last_batch: Mutex::new(None),
            last_apply_ms: AtomicU64::new(0),
            #[cfg(windows)]
            journal_cancel: eidos_scanner::usn::JournalCancellation::new()
                .expect("create journal cancellation event"),
        }
    }

    pub fn request_cancel(&self) {
        let _mutation = self.mutation_gate.lock();
        self.cancel.store(true, Ordering::Release);
        #[cfg(windows)]
        self.journal_cancel.cancel();
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn mutation_guard<'a>(&'a self, state: &AppState) -> Option<parking_lot::MutexGuard<'a, ()>> {
        let guard = self.mutation_gate.lock();
        if self.cancel.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
            None
        } else {
            Some(guard)
        }
    }

    pub fn set(&self, s: WatcherState, detail: Option<String>) {
        *self.state.lock() = (s, detail);
    }

    pub fn is_stopped(&self) -> bool {
        self.state.lock().0 == WatcherState::Stopped
    }

    pub fn view(&self) -> WatcherView {
        let (state, detail) = self.state.lock().clone();
        WatcherView {
            live: state == WatcherState::Live,
            state,
            detail,
            batches: self.batches.load(Ordering::Relaxed),
            events: self.events.load(Ordering::Relaxed),
            records: self.records.load(Ordering::Relaxed),
            reconciles: self.reconciles.load(Ordering::Relaxed),
            feed: FEED,
            last_position: self.last_position.load(Ordering::Relaxed),
            last_batch_ms_ago: self
                .last_batch
                .lock()
                .map(|t| t.elapsed().as_millis() as u64),
            last_apply_ms: self.last_apply_ms.load(Ordering::Relaxed),
            uptime_s: self.started.elapsed().as_secs(),
        }
    }
}

/// Ensure a watcher thread exists for the source (spawning if absent or
/// stopped). Returns the status handle.
pub fn ensure_watcher(state: &Arc<AppState>, source_id: SourceId) -> Arc<WatcherStatus> {
    let mut watchers = state.watchers.lock();
    if let Some(w) = watchers.get(&source_id) {
        if !w.is_stopped() {
            return w.clone();
        }
    }
    let status = Arc::new(WatcherStatus::new(source_id));
    watchers.insert(source_id, status.clone());
    #[cfg(any(windows, target_os = "macos"))]
    let st = state.clone();
    let s2 = status.clone();
    std::thread::Builder::new()
        .name(format!("feed-watch-{}", source_id.0))
        .spawn(move || {
            #[cfg(any(windows, target_os = "macos"))]
            watch_loop(st, source_id, s2);
            #[cfg(not(any(windows, target_os = "macos")))]
            s2.set(
                WatcherState::Stopped,
                Some("this platform has no native change feed".into()),
            );
        })
        .expect("spawn watcher");
    status
}

pub fn stop_watcher(state: &AppState, source_id: SourceId) {
    let watcher = state.watchers.lock().get(&source_id).cloned();
    if let Some(w) = watcher {
        w.request_cancel();
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsnCheckpoint {
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_root: String,
}

impl UsnCheckpoint {
    pub fn to_checkpoint(&self) -> Checkpoint {
        Checkpoint {
            kind: "usn".into(),
            value: serde_json::to_value(self).expect("serialisable"),
        }
    }
    pub fn from_checkpoint(cp: &Checkpoint) -> Option<Self> {
        if cp.kind != "usn" {
            return None;
        }
        serde_json::from_value(cp.value.clone()).ok()
    }
}

/// Durable FSEvents position for one source. The event-store UUID travels
/// with the id because an id from a replaced store is not stale but
/// meaningless (ADR-0018).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FsEventsCheckpoint {
    pub cursor: eidos_scanner::fsevents::FsEventsCursor,
    pub root: String,
}

#[cfg(target_os = "macos")]
impl FsEventsCheckpoint {
    pub fn to_checkpoint(&self) -> Checkpoint {
        Checkpoint {
            kind: "fsevents".into(),
            value: serde_json::to_value(self).expect("serialisable"),
        }
    }
    pub fn from_checkpoint(cp: &Checkpoint) -> Option<Self> {
        if cp.kind != "fsevents" {
            return None;
        }
        serde_json::from_value(cp.value.clone()).ok()
    }
}

#[cfg(target_os = "macos")]
fn fsevents_batch_checkpoint(
    checkpoint: &FsEventsCheckpoint,
    event_id: u64,
    retryable_errors: u64,
) -> (FsEventsCheckpoint, bool) {
    if retryable_errors > 0 {
        return (checkpoint.clone(), true);
    }
    (
        FsEventsCheckpoint {
            cursor: eidos_scanner::fsevents::FsEventsCursor {
                store_uuid: checkpoint.cursor.store_uuid.clone(),
                event_id,
            },
            root: checkpoint.root.clone(),
        },
        false,
    )
}

/// Canonical form of a source root as FSEvents reports paths: symlinks
/// resolved, `/var` spelled `/private/var`. Comparing a notification against
/// an unresolved root would classify every path as out of scope.
#[cfg(target_os = "macos")]
fn canonical_root(root: &str) -> std::path::PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| std::path::PathBuf::from(root))
}

#[cfg(target_os = "macos")]
fn watch_loop(state: Arc<AppState>, source_id: SourceId, status: Arc<WatcherStatus>) {
    use eidos_scanner::fsevents::{FeedMessage, FsEventsFeed};

    let mut feed: Option<FsEventsFeed> = None;
    let stop = |status: &WatcherStatus, reason: String| {
        tracing::info!(source = source_id.0, reason, "watcher stopped");
        status.set(WatcherState::Stopped, Some(reason));
    };
    // Reconcile by enumeration and forget the cursor. Every path that loses
    // event continuity funnels through here so a partial feed can never be
    // mistaken for a complete one.
    let reconcile = |status: &Arc<WatcherStatus>, reason: String| -> bool {
        tracing::warn!(source = source_id.0, reason, "change feed cannot continue");
        let Some(_mutation) = status.mutation_guard(&state) else {
            return false;
        };
        let _ = state
            .catalog
            .set_source_state(source_id, SourceState::Degraded, Some(&reason));
        let _ = state.catalog.clear_checkpoint(source_id);
        match scanner::start_feed_recovery_scan(&state, source_id, UnixNanos::now()) {
            Ok(scanner::AutomaticScanOutcome::Started(_)) => {
                status.reconciles.fetch_add(1, Ordering::Relaxed);
                status.set(WatcherState::Reconciling, Some(reason));
            }
            Ok(scanner::AutomaticScanOutcome::Deferred(d)) => {
                status.set(WatcherState::Reconciling, Some(d.reason));
            }
            Err(e) => tracing::error!(error = %e, "reconcile start failed"),
        }
        true
    };
    loop {
        if status.cancel.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
            stop(&status, "cancelled".into());
            return;
        }
        let source = match state.catalog.get_source(source_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                stop(&status, "source removed".into());
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "watcher: catalog read failed");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        if source.state == SourceState::Retired {
            stop(&status, "source retired".into());
            return;
        }
        if source.kind.is_remote() {
            stop(&status, "fleet replicas have no local change feed".into());
            return;
        }
        if state
            .scan_progress(source_id)
            .is_some_and(|p| !p.is_finished())
        {
            // An in-flight scan owns the source; the stream is closed so the
            // watcher cannot apply events against a generation being built.
            feed = None;
            status.set(WatcherState::Reconciling, Some("scan in progress".into()));
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        let cp = match state.catalog.checkpoint(source_id) {
            Ok(Some((cp, _))) => FsEventsCheckpoint::from_checkpoint(&cp),
            Ok(None) => None,
            Err(e) => {
                tracing::error!(error = %e, "watcher: checkpoint read failed");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let Some(cp) = cp else {
            if source.published_generation.is_none() {
                stop(
                    &status,
                    "source has no published generation; scan it first".into(),
                );
                return;
            }
            let failed_recently = state.scan_progress(source_id).is_some_and(|p| {
                p.result.lock().as_ref().is_some_and(|r| r.is_err())
                    && p.finished_for().is_some_and(|d| d < RECONCILE_RETRY_DELAY)
            });
            if failed_recently {
                status.set(
                    WatcherState::Reconciling,
                    Some("waiting to retry after a failed reconciliation".into()),
                );
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            if !reconcile(&status, "establishing a change-feed cursor".into()) {
                stop(&status, "cancelled".into());
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
            continue;
        };
        let root = canonical_root(&cp.root);
        if feed.is_none() {
            match FsEventsFeed::open(&root, Some(&cp.cursor)) {
                Ok(f) => {
                    status.set(
                        WatcherState::Starting,
                        Some(if f.replaying() {
                            "replaying stored events".into()
                        } else {
                            "watching".to_string()
                        }),
                    );
                    feed = Some(f);
                }
                Err(e) if e.kind == eidos_scanner::ScanErrorKind::Unsupported => {
                    if !reconcile(&status, format!("{}; reconciling", e.message)) {
                        stop(&status, "cancelled".into());
                        return;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                Err(e) => {
                    status.set(
                        WatcherState::Starting,
                        Some(format!("opening the change feed failed: {e}")),
                    );
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            }
        }
        let message = feed.as_mut().expect("opened").recv_timeout(POLL_INTERVAL);
        let Some(message) = message else {
            status.set(WatcherState::Live, None);
            continue;
        };
        match message {
            FeedMessage::HistoryDone => {
                status.set(WatcherState::Live, None);
            }
            FeedMessage::Rescan(reason) => {
                feed = None;
                if !reconcile(&status, format!("{}; reconciling", reason.as_str())) {
                    stop(&status, "cancelled".into());
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            FeedMessage::Batch { changes, event_id } => {
                if status.cancel.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
                    stop(&status, "cancelled".into());
                    return;
                }
                let started = Instant::now();
                let translator = crate::fsevents_apply::PathTranslator {
                    lister: state.lister.as_ref(),
                    catalog: &state.catalog,
                    source_id,
                    root: &root,
                };
                let (events, tstats) = translator.translate(&changes);
                if tstats.needs_rescan {
                    feed = None;
                    if !reconcile(
                        &status,
                        "a moved subtree is larger than one batch can describe; reconciling".into(),
                    ) {
                        stop(&status, "cancelled".into());
                        return;
                    }
                    continue;
                }
                // A batch that hit a retryable read failure describes less
                // than it was asked to. Apply what was read, retain the old
                // checkpoint, then reopen the stream from that checkpoint so
                // a later clean batch cannot acknowledge past the missed path.
                let (next, replay_batch) =
                    fsevents_batch_checkpoint(&cp, event_id, tstats.retryable_errors);
                if replay_batch {
                    tracing::warn!(
                        source = source_id.0,
                        retryable_errors = tstats.retryable_errors,
                        "holding the change-feed cursor and reopening the stream: part of this batch could not be read"
                    );
                }
                let apply = {
                    let Some(_mutation) = status.mutation_guard(&state) else {
                        stop(&status, "cancelled".into());
                        return;
                    };
                    state.catalog.apply_feed_changes(
                        source_id,
                        &events,
                        &cp.to_checkpoint(),
                        &next.to_checkpoint(),
                    )
                };
                match apply {
                    Ok(Some(astats)) => {
                        status.batches.fetch_add(1, Ordering::Relaxed);
                        status.events.fetch_add(astats.events, Ordering::Relaxed);
                        status.records.fetch_add(tstats.paths, Ordering::Relaxed);
                        if !replay_batch {
                            status
                                .last_position
                                .store(event_id as i64, Ordering::Relaxed);
                        }
                        *status.last_batch.lock() = Some(Instant::now());
                        status
                            .last_apply_ms
                            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                        if replay_batch {
                            feed = None;
                            status.set(
                                WatcherState::Starting,
                                Some("retrying an incomplete change-feed batch".into()),
                            );
                        } else {
                            status.set(WatcherState::Live, None);
                        }
                        tracing::debug!(
                            source = source_id.0,
                            paths = tstats.paths,
                            events = astats.events,
                            created = astats.objects_created,
                            tombstoned = astats.objects_tombstoned,
                            expanded = tstats.expanded_directories,
                            out_of_scope = tstats.out_of_scope,
                            ms = started.elapsed().as_millis() as u64,
                            "applied change batch"
                        );
                        if source.state == SourceState::Offline {
                            let Some(_mutation) = status.mutation_guard(&state) else {
                                stop(&status, "cancelled".into());
                                return;
                            };
                            let _ = restore_state(&state, source_id);
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(
                            source = source_id.0,
                            "checkpoint changed while a feed batch was in flight; discarding the stale batch"
                        );
                        feed = None;
                    }
                    Err(e) => {
                        // The stream already consumed this batch. Reopen it at
                        // the unchanged durable checkpoint to actually retry.
                        feed = None;
                        tracing::error!(error = %e, "watcher: apply failed; reopening the feed to retry the batch");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn watch_loop(state: Arc<AppState>, source_id: SourceId, status: Arc<WatcherStatus>) {
    use eidos_scanner::usn::{read_journal_wait, ReadOutcome, UsnError, VolumeHandle};

    let mut buf = vec![0u8; 1024 * 1024];
    let mut vol: Option<VolumeHandle> = None;
    let mut io_failures = 0u32;
    let stop = |status: &WatcherStatus, reason: String| {
        tracing::info!(source = source_id.0, reason, "watcher stopped");
        status.set(WatcherState::Stopped, Some(reason));
    };
    loop {
        if status.cancel.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
            stop(&status, "cancelled".into());
            return;
        }
        let source = match state.catalog.get_source(source_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                stop(&status, "source removed".into());
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "watcher: catalog read failed");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        if source.state == SourceState::Retired {
            stop(&status, "source retired".into());
            return;
        }
        if source.kind.is_remote() {
            stop(&status, "fleet replicas have no local change feed".into());
            return;
        }
        if state
            .scan_progress(source_id)
            .is_some_and(|p| !p.is_finished())
        {
            status.set(WatcherState::Reconciling, Some("scan in progress".into()));
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        let cp = match state.catalog.checkpoint(source_id) {
            Ok(Some((cp, _))) => UsnCheckpoint::from_checkpoint(&cp),
            Ok(None) => None,
            Err(e) => {
                tracing::error!(error = %e, "watcher: checkpoint read failed");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let cp = match cp {
            Some(cp) => cp,
            None => {
                if source.published_generation.is_none() {
                    stop(
                        &status,
                        "source has no published generation; scan it first".into(),
                    );
                    return;
                }
                // Establish a checkpoint through a reconciliation scan. A
                // scan that just failed (for example its replay step) is
                // retried after a pause rather than in a tight loop.
                let failed_recently = state.scan_progress(source_id).is_some_and(|p| {
                    p.result.lock().as_ref().is_some_and(|r| r.is_err())
                        && p.finished_for().is_some_and(|d| d < RECONCILE_RETRY_DELAY)
                });
                if failed_recently {
                    status.set(
                        WatcherState::Reconciling,
                        Some("waiting to retry after a failed reconciliation".into()),
                    );
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                let recovery = {
                    let Some(_mutation) = status.mutation_guard(&state) else {
                        stop(&status, "cancelled".into());
                        return;
                    };
                    scanner::start_feed_recovery_scan(&state, source_id, UnixNanos::now())
                };
                match recovery {
                    Ok(scanner::AutomaticScanOutcome::Started(_)) => {
                        status.set(
                            WatcherState::Reconciling,
                            Some("establishing checkpoint".into()),
                        );
                        status.reconciles.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(scanner::AutomaticScanOutcome::Deferred(d)) => {
                        status.set(WatcherState::Reconciling, Some(d.reason));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "watcher: could not start reconcile scan");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        if vol.is_none() {
            match VolumeHandle::open_waitable(&cp.volume_root) {
                Ok(v) => vol = Some(v),
                Err(e) => {
                    io_failures += 1;
                    if io_failures == 5 {
                        let Some(_mutation) = status.mutation_guard(&state) else {
                            stop(&status, "cancelled".into());
                            return;
                        };
                        let _ = state.catalog.set_source_state(
                            source_id,
                            SourceState::Offline,
                            Some(&format!("volume {} unavailable: {e}", cp.volume_root)),
                        );
                    }
                    status.set(
                        WatcherState::Starting,
                        Some(format!("volume open failed: {e}")),
                    );
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            }
        }
        let v = vol.as_ref().expect("opened");
        // Block until either records arrive or the persistent cancellation
        // event is signaled. No timing window can lose a shutdown request.
        match read_journal_wait(
            v,
            cp.journal_id,
            cp.next_usn,
            &mut buf,
            &status.journal_cancel,
        ) {
            Ok(ReadOutcome::Records { records, next_usn }) => {
                // The cancellation may arrive after the scanner's final
                // check but before this thread regains control. Discard the
                // completed batch rather than mutate a stopped source.
                if status.cancel.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
                    stop(&status, "cancelled".into());
                    return;
                }
                io_failures = 0;
                if records.is_empty() {
                    if next_usn != cp.next_usn {
                        let next_cp = UsnCheckpoint {
                            next_usn,
                            ..cp.clone()
                        };
                        let advance = {
                            let Some(_mutation) = status.mutation_guard(&state) else {
                                stop(&status, "cancelled".into());
                                return;
                            };
                            state.catalog.advance_feed_checkpoint(
                                source_id,
                                &cp.to_checkpoint(),
                                &next_cp.to_checkpoint(),
                            )
                        };
                        match advance {
                            Ok(true) => {
                                status.last_position.store(next_usn, Ordering::Relaxed);
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    source = source_id.0,
                                    "checkpoint changed while an empty feed batch was in flight"
                                );
                                vol = None;
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "watcher: checkpoint advance failed");
                                std::thread::sleep(Duration::from_secs(2));
                                continue;
                            }
                        }
                    }
                    status.set(WatcherState::Live, None);
                    // No sleep: the next read blocks until records arrive.
                    continue;
                }
                let started = Instant::now();
                let serial = match state
                    .catalog
                    .get_source(source_id)
                    .ok()
                    .flatten()
                    .and_then(|s| s.root_object_id)
                {
                    Some(root) => state
                        .catalog
                        .get_object(root)
                        .ok()
                        .flatten()
                        .and_then(|o| o.native)
                        .map(|n| n.volume_serial),
                    None => None,
                };
                let serial = match serial {
                    Some(s) => s,
                    None => {
                        stop(
                            &status,
                            "root object has no native identity; rescan required".into(),
                        );
                        return;
                    }
                };
                let translator = crate::usn_apply::Translator {
                    vol: v,
                    volume_serial: serial,
                    catalog: &state.catalog,
                    source_id,
                };
                let (events, tstats) = translator.translate(&records);
                let new_cp = UsnCheckpoint {
                    next_usn,
                    ..cp.clone()
                };
                let expected_cp = cp.to_checkpoint();
                let next_cp = new_cp.to_checkpoint();
                let apply = {
                    let Some(_mutation) = status.mutation_guard(&state) else {
                        stop(&status, "cancelled".into());
                        return;
                    };
                    state
                        .catalog
                        .apply_feed_changes(source_id, &events, &expected_cp, &next_cp)
                };
                match apply {
                    Ok(Some(astats)) => {
                        status.batches.fetch_add(1, Ordering::Relaxed);
                        status.events.fetch_add(astats.events, Ordering::Relaxed);
                        status.records.fetch_add(tstats.records, Ordering::Relaxed);
                        status.last_position.store(next_usn, Ordering::Relaxed);
                        *status.last_batch.lock() = Some(Instant::now());
                        status
                            .last_apply_ms
                            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                        status.set(WatcherState::Live, None);
                        tracing::debug!(
                            source = source_id.0,
                            records = tstats.records,
                            events = astats.events,
                            created = astats.objects_created,
                            tombstoned = astats.objects_tombstoned,
                            out_of_scope = tstats.out_of_scope,
                            ms = started.elapsed().as_millis() as u64,
                            "applied change batch"
                        );
                        if source.state == SourceState::Offline {
                            let Some(_mutation) = status.mutation_guard(&state) else {
                                stop(&status, "cancelled".into());
                                return;
                            };
                            let _ = restore_state(&state, source_id);
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(
                            source = source_id.0,
                            "checkpoint changed while a feed batch was in flight; discarding the stale batch"
                        );
                        vol = None;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "watcher: apply failed; will retry batch");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
            Ok(ReadOutcome::EntryDeleted) | Ok(ReadOutcome::JournalChanged) => {
                if status.cancel.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
                    stop(&status, "cancelled".into());
                    return;
                }
                let reason = "USN journal overflowed or was recreated; reconciling".to_string();
                tracing::warn!(source = source_id.0, reason, "change feed invalid");
                let recovery = {
                    let Some(_mutation) = status.mutation_guard(&state) else {
                        stop(&status, "cancelled".into());
                        return;
                    };
                    let _ = state.catalog.set_source_state(
                        source_id,
                        SourceState::Degraded,
                        Some(&reason),
                    );
                    let _ = state.catalog.clear_checkpoint(source_id);
                    scanner::start_feed_recovery_scan(&state, source_id, UnixNanos::now())
                };
                status.set(WatcherState::Reconciling, Some(reason));
                vol = None;
                match recovery {
                    Ok(scanner::AutomaticScanOutcome::Started(_)) => {
                        status.reconciles.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(scanner::AutomaticScanOutcome::Deferred(d)) => {
                        status.set(WatcherState::Reconciling, Some(d.reason));
                    }
                    Err(e) => tracing::error!(error = %e, "reconcile start failed"),
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(UsnError::DeleteInProgress) => {
                status.set(
                    WatcherState::Starting,
                    Some("journal deletion in progress".into()),
                );
                std::thread::sleep(Duration::from_secs(5));
            }
            Err(e @ (UsnError::NotActive | UsnError::AccessDenied | UsnError::Unsupported)) => {
                let reason = format!("{e}; falling back to periodic reconciliation");
                {
                    let Some(_mutation) = status.mutation_guard(&state) else {
                        stop(&status, "cancelled".into());
                        return;
                    };
                    let _ = state.catalog.clear_checkpoint(source_id);
                    let _ = state
                        .catalog
                        .set_source_state(source_id, source.state, Some(&reason));
                }
                stop(&status, reason);
                return;
            }
            Err(UsnError::Cancelled) => {
                stop(&status, "cancelled".into());
                return;
            }
            Err(UsnError::Io(e)) => {
                io_failures += 1;
                tracing::warn!(source = source_id.0, error = %e, failures = io_failures, "journal read failed");
                vol = None;
                if io_failures >= 5 {
                    let Some(_mutation) = status.mutation_guard(&state) else {
                        stop(&status, "cancelled".into());
                        return;
                    };
                    let _ = state.catalog.set_source_state(
                        source_id,
                        SourceState::Offline,
                        Some(&format!("volume unreachable: {e}")),
                    );
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

/// Restore a source from `offline` to its truthful completeness state.
pub fn restore_state(state: &AppState, source_id: SourceId) -> anyhow::Result<()> {
    let counts = state.catalog.source_counts(source_id)?;
    let st = if counts.content_pending > 0 {
        SourceState::ContentPending
    } else {
        SourceState::MetadataComplete
    };
    state.catalog.set_source_state(source_id, st, None)?;
    Ok(())
}

/// One step of the feed that replays the changes overlapping enumeration.
#[derive(Debug)]
pub enum ReplayStep {
    /// Events translated from the records up to `next_usn` (possibly none,
    /// when every record was out of scope); the feed must be read again.
    Batch {
        events: Vec<ChangeEvent>,
        next_usn: i64,
    },
    /// No records remain; the feed position is `next_usn`.
    CaughtUp { next_usn: i64 },
    /// The journal wrapped or was recreated: the pending checkpoint cannot
    /// be continued and some overlapping changes are unrecoverable.
    JournalInvalid,
    /// The feed could not be read.
    Failed(String),
}

/// Source of overlapping-change batches for [`replay_and_publish`]. The
/// production implementation reads the USN journal; tests script it.
pub trait OverlapFeed {
    fn next(&mut self, next_usn: i64) -> ReplayStep;
}

enum ReplayOutcome {
    CaughtUp { cp: UsnCheckpoint, replayed: u64 },
    JournalInvalid { cp: UsnCheckpoint, replayed: u64 },
    Failed(String),
}

fn replay_overlap(
    state: &AppState,
    source_id: SourceId,
    mut cp: UsnCheckpoint,
    feed: &mut dyn OverlapFeed,
) -> ReplayOutcome {
    let mut replayed = 0u64;
    loop {
        match feed.next(cp.next_usn) {
            ReplayStep::Batch { events, next_usn } => {
                if !events.is_empty() {
                    match state.catalog.apply_changes(source_id, &events, None) {
                        Ok(stats) => replayed += stats.events,
                        Err(e) => return ReplayOutcome::Failed(format!("apply failed: {e}")),
                    }
                }
                cp.next_usn = next_usn;
            }
            ReplayStep::CaughtUp { next_usn } => {
                cp.next_usn = next_usn;
                return ReplayOutcome::CaughtUp { cp, replayed };
            }
            ReplayStep::JournalInvalid => return ReplayOutcome::JournalInvalid { cp, replayed },
            ReplayStep::Failed(e) => return ReplayOutcome::Failed(e),
        }
    }
}

/// Replay the changes that overlapped enumeration into the open `session`,
/// then publish it with the resulting checkpoint in the same transaction.
///
/// - Feed caught up: publish + checkpoint atomically; the source flips from
///   `enumerating`/`reconciling` to its complete state only here.
/// - Journal wrapped during enumeration: publish what was enumerated, but
///   `degraded` with the reason and without a checkpoint, so the watcher
///   reconciles again.
/// - Feed or catalog failure: abort the generation; the previous published
///   generation and its checkpoint stay in force and the source is
///   `degraded` with the reason. Rows from replay batches that already
///   committed remain visible, like enumeration rows of any aborted
///   generation (they are observed truth; see ADR-0003).
pub fn replay_and_publish(
    state: &AppState,
    source_id: SourceId,
    mut session: ScanSession,
    cp: UsnCheckpoint,
    feed: &mut dyn OverlapFeed,
    progress: &ScanProgress,
) -> anyhow::Result<ScanSummary> {
    // Release the session's write lock: replay batches use the shared writer.
    session.commit()?;
    progress.set_phase("replaying changes");
    let from_usn = cp.next_usn;
    let summary = match replay_overlap(state, source_id, cp, feed) {
        ReplayOutcome::CaughtUp { cp, replayed } => {
            progress.set_phase("publishing");
            let summary = session.finish_with(&PublishOptions {
                checkpoint: Some(&cp.to_checkpoint()),
                ..Default::default()
            })?;
            tracing::info!(
                source = source_id.0,
                generation = summary.generation,
                replayed,
                from_usn,
                next_usn = cp.next_usn,
                "overlapping changes replayed; generation published with checkpoint"
            );
            summary
        }
        ReplayOutcome::JournalInvalid { cp, replayed } => {
            let reason =
                "USN journal wrapped during enumeration; another reconciliation is required";
            tracing::warn!(
                source = source_id.0,
                replayed,
                from_usn,
                next_usn = cp.next_usn,
                reason,
                "publishing degraded"
            );
            progress.set_phase("publishing");
            session.finish_with(&PublishOptions {
                clear_checkpoint: true,
                degraded: Some(reason),
                ..Default::default()
            })?
        }
        ReplayOutcome::Failed(e) => {
            let reason = format!("replaying changes that overlapped enumeration failed: {e}");
            session.abort(&reason)?;
            anyhow::bail!(reason);
        }
    };
    progress.set_phase("done");
    Ok(summary)
}

/// USN-journal implementation of [`OverlapFeed`].
#[cfg(windows)]
struct JournalFeed<'a> {
    vol: &'a eidos_scanner::usn::VolumeHandle,
    journal_id: u64,
    volume_serial: u64,
    catalog: &'a eidos_catalog::Catalog,
    source_id: SourceId,
    buf: Vec<u8>,
}

#[cfg(windows)]
impl OverlapFeed for JournalFeed<'_> {
    fn next(&mut self, next_usn: i64) -> ReplayStep {
        use eidos_scanner::usn::{read_journal, ReadOutcome};
        match read_journal(self.vol, self.journal_id, next_usn, &mut self.buf) {
            Ok(ReadOutcome::Records { records, next_usn }) => {
                if records.is_empty() {
                    return ReplayStep::CaughtUp { next_usn };
                }
                let translator = crate::usn_apply::Translator {
                    vol: self.vol,
                    volume_serial: self.volume_serial,
                    catalog: self.catalog,
                    source_id: self.source_id,
                };
                let (events, _) = translator.translate(&records);
                ReplayStep::Batch { events, next_usn }
            }
            Ok(ReadOutcome::EntryDeleted) | Ok(ReadOutcome::JournalChanged) => {
                ReplayStep::JournalInvalid
            }
            Err(e) => ReplayStep::Failed(e.to_string()),
        }
    }
}

/// Native scan sequence (SPEC 7.3): checkpoint → enumerate → replay →
/// publish (with the checkpoint) → watch. Falls back to a plain scan when
/// the journal is not available.
pub fn native_scan_sequence(
    state: &Arc<AppState>,
    source_id: SourceId,
    progress: &ScanProgress,
) -> anyhow::Result<ScanSummary> {
    #[cfg(windows)]
    {
        use eidos_scanner::usn::{query_journal, VolumeHandle};
        let source = state
            .catalog
            .get_source(source_id)?
            .ok_or_else(|| anyhow::anyhow!("source {source_id} not found"))?;
        anyhow::ensure!(
            !source.kind.is_remote(),
            "source {source_id} is a fleet replica and cannot be scanned here"
        );
        let vi = state
            .lister
            .volume_info(std::path::Path::new(&source.root_path))
            .ok();
        let native = vi
            .as_ref()
            .is_some_and(|v| v.supports_usn && !v.is_remote());
        if !native {
            let summary = scanner::run_full_scan(state, source_id, progress)?;
            let _ = state.catalog.clear_checkpoint(source_id);
            return Ok(summary);
        }
        let vi = vi.expect("checked");
        let (vol, journal) = match VolumeHandle::open(&vi.volume_root)
            .and_then(|v| query_journal(&v).map(|j| (v, j)))
        {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(source = source_id.0, error = %e, "USN journal unavailable; generic scan");
                let summary = scanner::run_full_scan(state, source_id, progress)?;
                let _ = state.catalog.clear_checkpoint(source_id);
                let _ = state.catalog.set_source_state(
                    source_id,
                    summary.final_state,
                    Some(&format!("{e}; periodic reconciliation only")),
                );
                return Ok(summary);
            }
        };
        let cp = UsnCheckpoint {
            journal_id: journal.journal_id,
            next_usn: journal.next_usn,
            volume_root: vi.volume_root.clone(),
        };
        progress.set_phase("enumerating");
        let session = scanner::enumerate(state, source_id, progress)?;
        let mut feed = JournalFeed {
            vol: &vol,
            journal_id: cp.journal_id,
            volume_serial: vi.volume_serial,
            catalog: &state.catalog,
            source_id,
            buf: vec![0u8; 1024 * 1024],
        };
        let summary = replay_and_publish(state, source_id, session, cp, &mut feed, progress)?;
        ensure_watcher(state, source_id);
        Ok(summary)
    }
    #[cfg(target_os = "macos")]
    {
        let source = state
            .catalog
            .get_source(source_id)?
            .ok_or_else(|| anyhow::anyhow!("source {source_id} not found"))?;
        anyhow::ensure!(
            !source.kind.is_remote(),
            "source {source_id} is a fleet replica and cannot be scanned here"
        );
        let volume = state
            .lister
            .volume_info(std::path::Path::new(&source.root_path))
            .ok();
        let native = volume
            .as_ref()
            .is_some_and(|v| v.native_feed == eidos_scanner::NativeFeed::MacosFsEvents);
        let root = canonical_root(&source.root_path);
        // The cursor is taken *before* enumeration, so it is always behind
        // what the published generation contains. Replaying a change the walk
        // already saw is idempotent; skipping one is data loss. Unlike the USN
        // journal, FSEvents replays from a stored id on demand, so there is no
        // overlap window to drain before publishing.
        let cursor = if native {
            eidos_scanner::fsevents::current_cursor(&root)
        } else {
            None
        };
        let Some(cursor) = cursor else {
            let summary = scanner::run_full_scan(state, source_id, progress)?;
            let _ = state.catalog.clear_checkpoint(source_id);
            if native {
                let _ = state.catalog.set_source_state(
                    source_id,
                    summary.final_state,
                    Some("this volume keeps no event history; periodic reconciliation only"),
                );
            }
            // A recovery scan may have been launched by an existing watcher.
            // With no resumable cursor that watcher must retire; the periodic
            // reconciler owns this source from here.
            stop_watcher(state, source_id);
            return Ok(summary);
        };
        progress.set_phase("enumerating");
        let session = scanner::enumerate(state, source_id, progress)?;
        let checkpoint = FsEventsCheckpoint {
            cursor,
            root: root.display().to_string(),
        };
        progress.set_phase("publishing");
        let summary = session.finish_with(&PublishOptions {
            checkpoint: Some(&checkpoint.to_checkpoint()),
            ..Default::default()
        })?;
        tracing::info!(
            source = source_id.0,
            generation = summary.generation,
            event_id = checkpoint.cursor.event_id,
            "generation published with a change-feed cursor"
        );
        progress.set_phase("done");
        ensure_watcher(state, source_id);
        Ok(summary)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let summary = scanner::run_full_scan(state, source_id, progress)?;
        Ok(summary)
    }
}

/// Background thread: periodic reconciliation for sources without a live
/// feed, and staleness marking.
pub fn spawn_reconciler(state: &Arc<AppState>) {
    let st = state.clone();
    std::thread::Builder::new()
        .name("reconciler".into())
        .spawn(move || loop {
            if st.shutdown.load(Ordering::Relaxed) {
                return;
            }
            if let Err(e) = reconcile_tick(&st) {
                tracing::warn!(error = %e, "reconciler tick failed");
            }
            for _ in 0..60 {
                if st.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("spawn reconciler");
}

fn reconcile_tick(state: &Arc<AppState>) -> anyhow::Result<()> {
    reconcile_tick_at(state, UnixNanos::now())
}

fn reconcile_tick_at(state: &Arc<AppState>, now: UnixNanos) -> anyhow::Result<()> {
    for s in state.catalog.list_sources()? {
        if s.published_generation.is_none() || s.state == SourceState::Retired || s.kind.is_remote()
        {
            state.clear_reconciliation_deferral(s.id);
            continue;
        }
        let has_feed =
            eidos_catalog::changes::is_native_feed_checkpoint(s.checkpoint_kind.as_deref())
                && state
                    .watchers
                    .lock()
                    .get(&s.id)
                    .is_some_and(|w| !w.is_stopped());
        if has_feed {
            state.clear_reconciliation_deferral(s.id);
            continue;
        }
        let interval = s
            .reconcile_interval_s
            .unwrap_or(DEFAULT_RECONCILE_INTERVAL_S);
        let age_s = s
            .last_scan_completed_at
            .map(|t| (now.0 - t.0) / 1_000_000_000)
            .unwrap_or(i64::MAX);
        if age_s >= interval * 2
            && matches!(
                s.state,
                SourceState::MetadataComplete | SourceState::ContentPending | SourceState::Complete
            )
        {
            let _ = state.catalog.set_source_state(
                s.id,
                SourceState::Stale,
                Some("reconciliation overdue; results may be out of date"),
            );
        }
        if age_s >= interval && state.auto_reconcile {
            tracing::info!(source = s.id.0, age_s, "periodic reconciliation due");
            match scanner::start_automatic_scan(state, s.id, now) {
                Ok(scanner::AutomaticScanOutcome::Started(_)) => {
                    tracing::info!(source = s.id.0, "periodic reconciliation started");
                }
                Ok(scanner::AutomaticScanOutcome::Deferred(d)) => {
                    tracing::info!(
                        source = s.id.0,
                        reason = %d.reason,
                        next_eligible_at = d.next_eligible_at.0,
                        "periodic reconciliation deferred"
                    );
                }
                Err(e) => {
                    tracing::warn!(source = s.id.0, error = %e, "periodic reconciliation start failed");
                }
            }
        } else {
            state.clear_reconciliation_deferral(s.id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod cancellation_tests {
    use super::WatcherStatus;
    use eidos_domain::SourceId;
    use std::sync::atomic::Ordering;
    use std::sync::{mpsc, Arc};

    #[test]
    fn cancellation_waits_for_an_inflight_watcher_mutation() {
        let status = Arc::new(WatcherStatus::new(SourceId(1)));
        let mutation = status.mutation_gate.lock();
        let (started_tx, started_rx) = mpsc::channel();
        let cancelling = status.clone();
        let thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            cancelling.request_cancel();
        });

        started_rx.recv().unwrap();
        assert!(!status.cancel.load(Ordering::Acquire));
        drop(mutation);
        thread.join().unwrap();
        assert!(status.cancel.load(Ordering::Acquire));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod fsevents_checkpoint_tests {
    use super::{fsevents_batch_checkpoint, FsEventsCheckpoint};
    use eidos_scanner::fsevents::FsEventsCursor;

    fn checkpoint(event_id: u64) -> FsEventsCheckpoint {
        FsEventsCheckpoint {
            cursor: FsEventsCursor {
                store_uuid: "store".into(),
                event_id,
            },
            root: "/source".into(),
        }
    }

    #[test]
    fn an_incomplete_batch_retains_its_cursor_and_reopens_the_feed() {
        let current = checkpoint(10);
        let (next, reopen) = fsevents_batch_checkpoint(&current, 20, 1);

        assert!(reopen);
        assert_eq!(next.cursor.event_id, current.cursor.event_id);
        assert_eq!(next.cursor.store_uuid, current.cursor.store_uuid);
        assert_eq!(next.root, current.root);
    }

    #[test]
    fn a_complete_batch_advances_without_reopening_the_feed() {
        let current = checkpoint(10);
        let (next, reopen) = fsevents_batch_checkpoint(&current, 20, 0);

        assert!(!reopen);
        assert_eq!(next.cursor.event_id, 20);
        assert_eq!(next.cursor.store_uuid, current.cursor.store_uuid);
    }
}

#[cfg(test)]
mod coordination_tests {
    use super::{reconcile_tick_at, DEFAULT_RECONCILE_INTERVAL_S};
    use crate::scanner::{
        run_full_scan, start_automatic_scan, start_scan, wait_for_scan, AutomaticScanOutcome,
        ScanProgress, StartScanError,
    };
    use crate::state::AppState;
    use crate::ServiceConfig;
    use eidos_catalog::jobs::NewJob;
    use eidos_catalog::scan::ScanKind;
    use eidos_catalog::NewSource;
    use eidos_domain::{JobStage, Priority, SourceId, SourceKind, SourceState, UnixNanos};
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        _dir: tempfile::TempDir,
        state: Arc<AppState>,
        source_id: SourceId,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("source");
            std::fs::create_dir_all(root.join("docs")).unwrap();
            std::fs::write(root.join("docs").join("readme.txt"), b"hello").unwrap();
            let config = ServiceConfig {
                data_dir: dir.path().join("data"),
                scan_threads: 1,
                auto_reconcile: true,
                content: true,
                content_workers: 1,
                ..Default::default()
            };
            let mut state = AppState::open(&config).unwrap();
            // Keep the fixture independent of Windows volume and journal
            // capabilities, even when the test suite runs elevated.
            state.lister = Arc::new(eidos_scanner::std_lister::StdLister);
            let state = Arc::new(state);
            let source_id = state
                .catalog
                .add_source(&NewSource {
                    host_id: state.host_id,
                    name: "coordination-fixture".into(),
                    kind: SourceKind::WindowsGeneric,
                    root_path: root.display().to_string(),
                    aliases: vec![],
                })
                .unwrap();
            state
                .catalog
                .set_content_policy(source_id, false, 1)
                .unwrap();
            let summary = run_full_scan(&state, source_id, &ScanProgress::new(source_id)).unwrap();
            assert_eq!(summary.generation, 1);
            state
                .catalog
                .set_content_policy(source_id, true, 1)
                .unwrap();
            Self {
                _dir: dir,
                state,
                source_id,
            }
        }

        fn next_due(&self) -> UnixNanos {
            let completed = self
                .state
                .catalog
                .get_source(self.source_id)
                .unwrap()
                .unwrap()
                .last_scan_completed_at
                .unwrap();
            UnixNanos(completed.0 + (DEFAULT_RECONCILE_INTERVAL_S + 1) * 1_000_000_000)
        }

        fn queue_content(&self, count: u32) {
            let jobs = (0..count)
                .map(|i| NewJob {
                    source_id: self.source_id,
                    object_id: None,
                    object_generation: 1,
                    stage: JobStage::ContentText,
                    priority: Priority::NormalText,
                    idempotency_key: format!("coordination:{}:{i}", self.source_id.0),
                    payload: None,
                    estimated_cost: 0,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                self.state.catalog.enqueue_many(&jobs).unwrap(),
                count as usize
            );
        }

        fn drain_content(&self) {
            while let Some(job) = self
                .state
                .catalog
                .claim_job(&[JobStage::ContentText], "coordination-test")
                .unwrap()
            {
                self.state.catalog.complete_job(job.id).unwrap();
            }
        }

        fn wait(&self, progress: &ScanProgress) -> eidos_catalog::scan::ScanSummary {
            wait_for_scan(progress, Duration::from_secs(10))
                .expect("scan did not finish")
                .expect("scan failed")
        }
    }

    #[test]
    fn content_work_defers_ticks_but_manual_override_and_later_tick_run() {
        let fixture = Fixture::new();
        fixture.queue_content(2);
        let due = fixture.next_due();

        reconcile_tick_at(&fixture.state, due).unwrap();
        let first = fixture
            .state
            .reconciliation_deferral(fixture.source_id)
            .expect("content should defer reconciliation");
        assert_eq!(first.reason, "content crawl active (2 queued, 0 running)");
        assert!(first.next_eligible_at.0 > due.0);
        assert!(fixture.state.scan_progress(fixture.source_id).is_none());

        // A fast second scheduler tick returns the remembered result instead
        // of querying and attempting another generation.
        reconcile_tick_at(&fixture.state, UnixNanos(due.0 + 1_000_000_000)).unwrap();
        assert_eq!(
            fixture
                .state
                .reconciliation_deferral(fixture.source_id)
                .unwrap(),
            first
        );

        // Explicit operator intent overrides content coordination.
        let manual = start_scan(&fixture.state, fixture.source_id).unwrap();
        assert_eq!(fixture.wait(&manual).generation, 2);
        assert!(fixture
            .state
            .reconciliation_deferral(fixture.source_id)
            .is_none());

        fixture.drain_content();
        reconcile_tick_at(&fixture.state, fixture.next_due()).unwrap();
        let automatic = fixture
            .state
            .scan_progress(fixture.source_id)
            .expect("eligible reconciliation should start");
        assert_eq!(fixture.wait(&automatic).generation, 3);
    }

    #[test]
    fn durable_open_generation_blocks_manual_and_automatic_starts() {
        let fixture = Fixture::new();
        let session = fixture
            .state
            .catalog
            .begin_scan(fixture.source_id, ScanKind::Reconcile)
            .unwrap();
        assert_eq!(
            fixture
                .state
                .catalog
                .open_scan_generation(fixture.source_id)
                .unwrap(),
            Some(2)
        );
        assert!(matches!(
            start_scan(&fixture.state, fixture.source_id),
            Err(StartScanError::AlreadyRunning)
        ));

        let due = fixture.next_due();
        reconcile_tick_at(&fixture.state, due).unwrap();
        let first = fixture
            .state
            .reconciliation_deferral(fixture.source_id)
            .expect("open generation should defer reconciliation");
        assert_eq!(first.reason, "scan generation 2 is already open");
        reconcile_tick_at(&fixture.state, UnixNanos(due.0 + 1_000_000_000)).unwrap();
        assert_eq!(
            fixture
                .state
                .reconciliation_deferral(fixture.source_id)
                .unwrap(),
            first
        );

        session
            .abort("test releases durable scan ownership")
            .unwrap();
        let outcome =
            start_automatic_scan(&fixture.state, fixture.source_id, first.next_eligible_at)
                .unwrap();
        let AutomaticScanOutcome::Started(progress) = outcome else {
            panic!("reconciliation remained deferred after the open generation closed");
        };
        assert_eq!(fixture.wait(&progress).generation, 3);
    }

    #[test]
    fn paused_content_queue_does_not_block_automatic_reconciliation() {
        use std::sync::atomic::Ordering;

        let fixture = Fixture::new();
        fixture.queue_content(1);
        fixture
            .state
            .content_enabled
            .store(false, Ordering::Relaxed);

        let outcome =
            start_automatic_scan(&fixture.state, fixture.source_id, fixture.next_due()).unwrap();
        let AutomaticScanOutcome::Started(progress) = outcome else {
            panic!("a paused content queue should not defer reconciliation");
        };
        assert_eq!(fixture.wait(&progress).generation, 2);
    }

    #[test]
    fn feed_recovery_bypasses_content_but_not_scan_ownership() {
        let fixture = Fixture::new();
        fixture.queue_content(1);
        let outcome = crate::scanner::start_feed_recovery_scan(
            &fixture.state,
            fixture.source_id,
            fixture.next_due(),
        )
        .unwrap();
        let AutomaticScanOutcome::Started(progress) = outcome else {
            panic!("native feed recovery should bypass the content delay");
        };
        assert_eq!(fixture.wait(&progress).generation, 2);

        let session = fixture
            .state
            .catalog
            .begin_scan(fixture.source_id, ScanKind::Reconcile)
            .unwrap();
        let outcome = crate::scanner::start_feed_recovery_scan(
            &fixture.state,
            fixture.source_id,
            fixture.next_due(),
        )
        .unwrap();
        assert!(matches!(outcome, AutomaticScanOutcome::Deferred(_)));
        session
            .abort("test releases feed recovery ownership")
            .unwrap();
    }

    #[test]
    fn remote_sources_are_never_scheduled_for_local_reconciliation() {
        let fixture = Fixture::new();
        fixture
            .state
            .catalog
            .with_writer(|conn| {
                conn.execute(
                    "UPDATE sources SET kind = 'remote', last_scan_completed_at = 1,
                         state = 'metadata_complete' WHERE source_id = ?1",
                    [fixture.source_id.0],
                )?;
                Ok(())
            })
            .unwrap();

        let long_overdue = UnixNanos(1 + DEFAULT_RECONCILE_INTERVAL_S * 10 * 1_000_000_000);
        reconcile_tick_at(&fixture.state, long_overdue).unwrap();
        assert!(fixture.state.scan_progress(fixture.source_id).is_none());
        let source = fixture
            .state
            .catalog
            .get_source(fixture.source_id)
            .unwrap()
            .unwrap();
        assert_eq!(source.kind, SourceKind::Remote);
        assert_eq!(source.state, SourceState::MetadataComplete);
        assert!(fixture
            .state
            .reconciliation_deferral(fixture.source_id)
            .is_none());
    }
}
