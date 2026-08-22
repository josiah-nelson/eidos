//! Change-feed watchers and periodic reconciliation.
//!
//! One thread per native (USN) source polls the journal from the stored
//! checkpoint, translates records, and applies them with the checkpoint in
//! the same catalog transaction. Overflow or an invalid journal ID marks the
//! source `degraded`, clears the checkpoint, and triggers a reconciliation
//! scan, after which watching resumes from a fresh checkpoint. Sources
//! without a live feed (SMB, generic, or journal unavailable) are rescanned
//! periodically by the reconciler thread.

use crate::scanner::{self, ScanProgress};
use crate::state::AppState;
use eidos_catalog::changes::Checkpoint;
use eidos_catalog::scan::ScanSummary;
use eidos_domain::{SourceId, SourceState, UnixNanos};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const DEFAULT_RECONCILE_INTERVAL_S: i64 = 6 * 3600;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherState {
    Starting,
    Live,
    Reconciling,
    Stopped,
}

#[derive(Debug)]
pub struct WatcherStatus {
    pub source_id: SourceId,
    pub cancel: AtomicBool,
    state: Mutex<(WatcherState, Option<String>)>,
    pub started: Instant,
    pub batches: AtomicU64,
    pub events: AtomicU64,
    pub records: AtomicU64,
    pub reconciles: AtomicU64,
    pub last_usn: AtomicI64,
    last_batch: Mutex<Option<Instant>>,
    last_apply_ms: AtomicU64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WatcherView {
    pub state: WatcherState,
    pub live: bool,
    pub detail: Option<String>,
    pub batches: u64,
    pub events: u64,
    pub records: u64,
    pub reconciles: u64,
    pub last_usn: i64,
    pub last_batch_ms_ago: Option<u64>,
    pub last_apply_ms: u64,
    pub uptime_s: u64,
}

impl WatcherStatus {
    fn new(source_id: SourceId) -> Self {
        Self {
            source_id,
            cancel: AtomicBool::new(false),
            state: Mutex::new((WatcherState::Starting, None)),
            started: Instant::now(),
            batches: AtomicU64::new(0),
            events: AtomicU64::new(0),
            records: AtomicU64::new(0),
            reconciles: AtomicU64::new(0),
            last_usn: AtomicI64::new(0),
            last_batch: Mutex::new(None),
            last_apply_ms: AtomicU64::new(0),
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
            last_usn: self.last_usn.load(Ordering::Relaxed),
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
    let st = state.clone();
    let s2 = status.clone();
    std::thread::Builder::new()
        .name(format!("usn-watch-{}", source_id.0))
        .spawn(move || {
            #[cfg(windows)]
            watch_loop(st, source_id, s2);
            #[cfg(not(windows))]
            s2.set(
                WatcherState::Stopped,
                Some("change feeds are Windows-only in v0.5".into()),
            );
        })
        .expect("spawn watcher");
    status
}

pub fn stop_watcher(state: &AppState, source_id: SourceId) {
    if let Some(w) = state.watchers.lock().get(&source_id) {
        w.cancel.store(true, Ordering::Relaxed);
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

#[cfg(windows)]
fn watch_loop(state: Arc<AppState>, source_id: SourceId, status: Arc<WatcherStatus>) {
    use eidos_scanner::usn::{read_journal, ReadOutcome, UsnError, VolumeHandle};

    let mut buf = vec![0u8; 1024 * 1024];
    let mut vol: Option<VolumeHandle> = None;
    let mut io_failures = 0u32;
    let stop = |status: &WatcherStatus, reason: String| {
        tracing::info!(source = source_id.0, reason, "watcher stopped");
        status.set(WatcherState::Stopped, Some(reason));
    };
    loop {
        if status.cancel.load(Ordering::Relaxed) || state.shutdown.load(Ordering::Relaxed) {
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
                // Establish a checkpoint through a reconciliation scan.
                status.set(
                    WatcherState::Reconciling,
                    Some("establishing checkpoint".into()),
                );
                status.reconciles.fetch_add(1, Ordering::Relaxed);
                match scanner::start_scan(&state, source_id) {
                    Ok(_) | Err(scanner::StartScanError::AlreadyRunning) => {}
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
            match VolumeHandle::open(&cp.volume_root) {
                Ok(v) => vol = Some(v),
                Err(e) => {
                    io_failures += 1;
                    if io_failures == 5 {
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
        match read_journal(v, cp.journal_id, cp.next_usn, &mut buf) {
            Ok(ReadOutcome::Records { records, next_usn }) => {
                io_failures = 0;
                if records.is_empty() {
                    if next_usn != cp.next_usn {
                        let _ = state.catalog.set_checkpoint(
                            source_id,
                            &UsnCheckpoint {
                                next_usn,
                                ..cp.clone()
                            }
                            .to_checkpoint(),
                        );
                        status.last_usn.store(next_usn, Ordering::Relaxed);
                    }
                    status.set(WatcherState::Live, None);
                    std::thread::sleep(POLL_INTERVAL);
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
                match state
                    .catalog
                    .apply_changes(source_id, &events, Some(&new_cp.to_checkpoint()))
                {
                    Ok(astats) => {
                        status.batches.fetch_add(1, Ordering::Relaxed);
                        status.events.fetch_add(astats.events, Ordering::Relaxed);
                        status.records.fetch_add(tstats.records, Ordering::Relaxed);
                        status.last_usn.store(next_usn, Ordering::Relaxed);
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
                            let _ = restore_state(&state, source_id);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "watcher: apply failed; will retry batch");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
            Ok(ReadOutcome::EntryDeleted) | Ok(ReadOutcome::JournalChanged) => {
                let reason = "USN journal overflowed or was recreated; reconciling".to_string();
                tracing::warn!(source = source_id.0, reason, "change feed invalid");
                let _ =
                    state
                        .catalog
                        .set_source_state(source_id, SourceState::Degraded, Some(&reason));
                let _ = state.catalog.clear_checkpoint(source_id);
                status.reconciles.fetch_add(1, Ordering::Relaxed);
                status.set(WatcherState::Reconciling, Some(reason));
                vol = None;
                match scanner::start_scan(&state, source_id) {
                    Ok(_) | Err(scanner::StartScanError::AlreadyRunning) => {}
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
                let _ = state.catalog.clear_checkpoint(source_id);
                let _ = state
                    .catalog
                    .set_source_state(source_id, source.state, Some(&reason));
                stop(&status, reason);
                return;
            }
            Err(UsnError::Io(e)) => {
                io_failures += 1;
                tracing::warn!(source = source_id.0, error = %e, failures = io_failures, "journal read failed");
                vol = None;
                if io_failures >= 5 {
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

/// Native scan sequence (SPEC 7.3): checkpoint → enumerate → replay →
/// publish → watch. Falls back to a plain scan when the journal is not
/// available.
pub fn native_scan_sequence(
    state: &Arc<AppState>,
    source_id: SourceId,
    progress: &ScanProgress,
) -> anyhow::Result<ScanSummary> {
    #[cfg(windows)]
    {
        use eidos_scanner::usn::{query_journal, read_journal, ReadOutcome, VolumeHandle};
        let source = state
            .catalog
            .get_source(source_id)?
            .ok_or_else(|| anyhow::anyhow!("source {source_id} not found"))?;
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
        let mut cp = UsnCheckpoint {
            journal_id: journal.journal_id,
            next_usn: journal.next_usn,
            volume_root: vi.volume_root.clone(),
        };
        progress.set_phase("enumerating");
        let summary = scanner::run_full_scan(state, source_id, progress)?;
        // Replay everything that happened during enumeration.
        progress.set_phase("replaying changes");
        let serial = vi.volume_serial;
        let mut buf = vec![0u8; 1024 * 1024];
        let mut replayed = 0u64;
        loop {
            match read_journal(&vol, cp.journal_id, cp.next_usn, &mut buf) {
                Ok(ReadOutcome::Records { records, next_usn }) => {
                    if records.is_empty() {
                        cp.next_usn = next_usn;
                        break;
                    }
                    let translator = crate::usn_apply::Translator {
                        vol: &vol,
                        volume_serial: serial,
                        catalog: &state.catalog,
                        source_id,
                    };
                    let (events, _) = translator.translate(&records);
                    let next = UsnCheckpoint {
                        next_usn,
                        ..cp.clone()
                    };
                    let stats = state.catalog.apply_changes(
                        source_id,
                        &events,
                        Some(&next.to_checkpoint()),
                    )?;
                    replayed += stats.events;
                    cp = next;
                }
                Ok(ReadOutcome::EntryDeleted) | Ok(ReadOutcome::JournalChanged) => {
                    // Extremely busy volume: the journal wrapped during the
                    // scan. Publish what we have, degraded, and let the
                    // watcher reconcile again.
                    let _ = state.catalog.clear_checkpoint(source_id);
                    let _ = state.catalog.set_source_state(
                        source_id,
                        SourceState::Degraded,
                        Some("USN journal wrapped during enumeration; another reconciliation is required"),
                    );
                    ensure_watcher(state, source_id);
                    return Ok(summary);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "replay read failed; checkpoint kept at last applied USN");
                    break;
                }
            }
        }
        state
            .catalog
            .set_checkpoint(source_id, &cp.to_checkpoint())?;
        tracing::info!(
            source = source_id.0,
            replayed,
            next_usn = cp.next_usn,
            "checkpoint established"
        );
        progress.set_phase("done");
        ensure_watcher(state, source_id);
        Ok(summary)
    }
    #[cfg(not(windows))]
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
    let now = UnixNanos::now();
    for s in state.catalog.list_sources()? {
        if s.published_generation.is_none() || s.state == SourceState::Retired {
            continue;
        }
        if state.scan_progress(s.id).is_some_and(|p| !p.is_finished()) {
            continue;
        }
        let has_feed = s.checkpoint_kind.as_deref() == Some("usn")
            && state
                .watchers
                .lock()
                .get(&s.id)
                .is_some_and(|w| !w.is_stopped());
        if has_feed {
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
            let _ = scanner::start_scan(state, s.id);
        }
    }
    Ok(())
}
