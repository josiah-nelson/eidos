//! Background scan execution with observable progress.

use crate::state::{
    AppState, ReconciliationDeferral, ReconciliationDeferralCause as DeferralCause,
};
use crate::watcher;
use eidos_catalog::scan::{ScanKind, ScanSession, ScanSummary};
use eidos_domain::{JobStage, SourceId, SourceKind, UnixNanos};
use eidos_scanner::WalkOptions;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ts_rs::TS;

#[derive(Debug)]
pub struct ScanProgress {
    pub source_id: SourceId,
    pub started: Instant,
    pub dirs: AtomicU64,
    pub entries: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: Arc<AtomicBool>,
    phase: Mutex<String>,
    finished: AtomicBool,
    finished_at: Mutex<Option<Instant>>,
    pub result: Mutex<Option<Result<ScanSummary, String>>>,
}

#[derive(Debug, Clone, serde::Serialize, TS)]
#[ts(optional_fields, rename = "ScanProgress")]
pub struct ScanProgressView {
    pub source_id: SourceId,
    pub running: bool,
    pub phase: String,
    pub elapsed_ms: u64,
    pub dirs: u64,
    pub entries: u64,
    pub errors: u64,
    pub entries_per_sec: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ScanSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ScanProgress {
    pub fn new(source_id: SourceId) -> Self {
        Self {
            source_id,
            started: Instant::now(),
            dirs: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            cancel: Arc::new(AtomicBool::new(false)),
            phase: Mutex::new("starting".into()),
            finished: AtomicBool::new(false),
            finished_at: Mutex::new(None),
            result: Mutex::new(None),
        }
    }

    pub fn set_phase(&self, phase: &str) {
        *self.phase.lock() = phase.to_string();
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn finished_for(&self) -> Option<Duration> {
        self.finished_at.lock().map(|t| t.elapsed())
    }

    pub fn view(&self) -> ScanProgressView {
        let elapsed = self
            .finished_at
            .lock()
            .map(|t| t.duration_since(self.started))
            .unwrap_or_else(|| self.started.elapsed());
        let entries = self.entries.load(Ordering::Relaxed);
        let result = self.result.lock();
        ScanProgressView {
            source_id: self.source_id,
            running: !self.is_finished(),
            phase: self.phase.lock().clone(),
            elapsed_ms: elapsed.as_millis() as u64,
            dirs: self.dirs.load(Ordering::Relaxed),
            entries,
            errors: self.errors.load(Ordering::Relaxed),
            entries_per_sec: if elapsed.as_secs_f64() > 0.0 {
                entries as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            },
            summary: result.as_ref().and_then(|r| r.as_ref().ok().cloned()),
            error: result.as_ref().and_then(|r| r.as_ref().err().cloned()),
        }
    }

    fn finish(&self, r: Result<ScanSummary, String>) {
        *self.result.lock() = Some(r);
        *self.finished_at.lock() = Some(Instant::now());
        self.finished.store(true, Ordering::Release);
        self.set_phase("done");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartScanError {
    #[error("a scan is already running for this source")]
    AlreadyRunning,
    #[error("a fleet replica is scanned on its origin node, not here")]
    RemoteSource,
    #[error("{0}")]
    Catalog(#[from] eidos_catalog::CatalogError),
}

fn ensure_scannable(state: &AppState, source_id: SourceId) -> Result<(), StartScanError> {
    let source = state
        .catalog
        .get_source(source_id)?
        .ok_or_else(|| eidos_catalog::CatalogError::NotFound(format!("source {source_id}")))?;
    if source.kind == SourceKind::Remote {
        return Err(StartScanError::RemoteSource);
    }
    Ok(())
}

/// Result of asking the scheduler to start a reconciliation. Deferral is a
/// normal operational outcome rather than an error.
#[derive(Debug)]
pub enum AutomaticScanOutcome {
    Started(Arc<ScanProgress>),
    Deferred(ReconciliationDeferral),
}

const AUTOMATIC_RETRY_DELAY: Duration = Duration::from_secs(60);

fn deferral(now: UnixNanos, reason: String) -> ReconciliationDeferral {
    ReconciliationDeferral {
        reason,
        next_eligible_at: UnixNanos(
            now.0
                .saturating_add(AUTOMATIC_RETRY_DELAY.as_nanos() as i64),
        ),
    }
}

fn spawn_scan(state: &Arc<AppState>, source_id: SourceId, progress: Arc<ScanProgress>) {
    let st = state.clone();
    let p = progress.clone();
    std::thread::Builder::new()
        .name(format!("scan-{}", source_id.0))
        .spawn(move || {
            let r = watcher::native_scan_sequence(&st, source_id, &p).map_err(|e| e.to_string());
            if let Err(e) = &r {
                tracing::error!(source = source_id.0, error = %e, "scan failed");
            }
            p.finish(r);
        })
        .expect("spawn scan thread");
}

/// Start a scan on a dedicated thread. Native sources run the full
/// checkpoint → enumerate → replay → watch sequence. Returns immediately.
pub fn start_scan(
    state: &Arc<AppState>,
    source_id: SourceId,
) -> Result<Arc<ScanProgress>, StartScanError> {
    ensure_scannable(state, source_id)?;
    let progress = {
        let mut scans = state.scans.lock();
        if let Some(p) = scans.get(&source_id) {
            if !p.is_finished() {
                return Err(StartScanError::AlreadyRunning);
            }
        }
        if state.catalog.open_scan_generation(source_id)?.is_some() {
            return Err(StartScanError::AlreadyRunning);
        }
        let progress = Arc::new(ScanProgress::new(source_id));
        scans.insert(source_id, progress.clone());
        state.clear_reconciliation_deferral(source_id);
        progress
    };
    spawn_scan(state, source_id, progress.clone());
    Ok(progress)
}

/// Start an automatic reconciliation when it will not overlap scan or
/// content work. A remembered deferral prevents fast scheduler/watcher loops
/// from repeatedly probing the same busy source.
pub fn start_automatic_scan(
    state: &Arc<AppState>,
    source_id: SourceId,
    now: UnixNanos,
) -> Result<AutomaticScanOutcome, StartScanError> {
    start_automatic_scan_with(state, source_id, now, true)
}

/// A missing or invalid native checkpoint must be repaired to restore the
/// live feed. It bypasses only the content-work delay; scan ownership is the
/// same as every other automatic start.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn start_feed_recovery_scan(
    state: &Arc<AppState>,
    source_id: SourceId,
    now: UnixNanos,
) -> Result<AutomaticScanOutcome, StartScanError> {
    start_automatic_scan_with(state, source_id, now, false)
}

fn start_automatic_scan_with(
    state: &Arc<AppState>,
    source_id: SourceId,
    now: UnixNanos,
    coordinate_content: bool,
) -> Result<AutomaticScanOutcome, StartScanError> {
    ensure_scannable(state, source_id)?;
    let progress = {
        let mut scans = state.scans.lock();
        if let Some(p) = scans.get(&source_id) {
            if !p.is_finished() {
                let d = deferral(now, "scan already running".into());
                state.defer_reconciliation(source_id, d.clone(), DeferralCause::Scan);
                return Ok(AutomaticScanOutcome::Deferred(d));
            }
        }
        if let Some((existing, cause)) = state.scheduled_reconciliation_deferral(source_id) {
            if now.0 < existing.next_eligible_at.0
                && (coordinate_content || cause == DeferralCause::Scan)
            {
                return Ok(AutomaticScanOutcome::Deferred(existing));
            }
        }
        if let Some(generation) = state.catalog.open_scan_generation(source_id)? {
            let d = deferral(now, format!("scan generation {generation} is already open"));
            state.defer_reconciliation(source_id, d.clone(), DeferralCause::Scan);
            return Ok(AutomaticScanOutcome::Deferred(d));
        }
        if coordinate_content {
            let (queued, running) = state
                .catalog
                .active_job_counts(source_id, JobStage::ContentText)?;
            // A queued backlog only justifies deferring a rescan if
            // something is going to claim it. A paused pipeline claims
            // nothing, so — exactly as with `--no-content` — its queue must
            // not hold reconciliation off indefinitely. Jobs already
            // `running` still defer: those are draining and touching the
            // volume the scan would walk.
            let content_scheduled = state.content_enabled.load(Ordering::Relaxed)
                && !state.content_pause.is_paused()
                && state
                    .catalog
                    .get_source(source_id)?
                    .is_some_and(|source| source.content_enabled);
            if running > 0 || (queued > 0 && content_scheduled) {
                let d = deferral(
                    now,
                    format!("content crawl active ({queued} queued, {running} running)"),
                );
                state.defer_reconciliation(source_id, d.clone(), DeferralCause::Content);
                return Ok(AutomaticScanOutcome::Deferred(d));
            }
        }
        let progress = Arc::new(ScanProgress::new(source_id));
        scans.insert(source_id, progress.clone());
        state.clear_reconciliation_deferral(source_id);
        progress
    };
    spawn_scan(state, source_id, progress.clone());
    Ok(AutomaticScanOutcome::Started(progress))
}

/// Wait (polling) for a running scan to finish. Test/CLI helper.
pub fn wait_for_scan(
    progress: &ScanProgress,
    timeout: Duration,
) -> Option<Result<ScanSummary, String>> {
    let start = Instant::now();
    while !progress.is_finished() {
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    progress.result.lock().clone()
}

/// Enumerate the source and publish a generation (no change-feed handling).
pub fn run_full_scan(
    state: &Arc<AppState>,
    source_id: SourceId,
    progress: &ScanProgress,
) -> anyhow::Result<ScanSummary> {
    let session = enumerate(state, source_id, progress)?;
    progress.set_phase("publishing");
    Ok(session.finish()?)
}

/// Open a generation and stream the source's directory tree into it. The
/// returned session is still open: the caller replays overlapping change
/// feed records into it (native sources) and then publishes or aborts it.
/// Ingest failures and cancellation abort the generation here.
pub fn enumerate(
    state: &Arc<AppState>,
    source_id: SourceId,
    progress: &ScanProgress,
) -> anyhow::Result<ScanSession> {
    let source = state
        .catalog
        .get_source(source_id)?
        .ok_or_else(|| anyhow::anyhow!("source {source_id} not found"))?;
    anyhow::ensure!(
        source.kind != SourceKind::Remote,
        "source {source_id} is a fleet replica and cannot be scanned here"
    );
    // Refresh volume capabilities (read-only probe).
    match state
        .lister
        .volume_info(std::path::Path::new(&source.root_path))
    {
        Ok(v) => {
            state.catalog.upsert_volume(state.host_id, source_id, &v)?;
            let kind = v.source_kind();
            if kind != source.kind {
                state.catalog.set_source_kind(source_id, kind)?;
            }
        }
        Err(e) => tracing::warn!(source = source_id.0, error = %e, "volume probe failed"),
    }
    let kind = if source.published_generation.is_some() {
        ScanKind::Reconcile
    } else {
        ScanKind::Full
    };
    let mut session = state.catalog.begin_scan(source_id, kind)?;
    if let Ok(root_entry) = state.lister.stat(std::path::Path::new(&source.root_path)) {
        if let Some(native) = root_entry.native_id {
            state.catalog.set_root_identity(source_id, native)?;
        }
    }
    let walk_opts = WalkOptions {
        threads: state.scan_threads,
        cancel: Some(progress.cancel.clone()),
        ..Default::default()
    };
    let root = std::path::PathBuf::from(&source.root_path);
    let mut ingest_error: Option<eidos_catalog::CatalogError> = None;
    let cancel = progress.cancel.clone();
    let stats = eidos_scanner::walk(&root, state.lister.as_ref(), &walk_opts, |ev| {
        if ingest_error.is_some() {
            return;
        }
        progress.dirs.fetch_add(1, Ordering::Relaxed);
        match &ev.result {
            Ok(entries) => {
                progress
                    .entries
                    .fetch_add(entries.len() as u64, Ordering::Relaxed);
            }
            Err(_) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Err(e) = session.ingest(ev) {
            ingest_error = Some(e);
            cancel.store(true, Ordering::Relaxed);
        }
    });
    if let Some(e) = ingest_error {
        session.abort(&format!("ingest error: {e}"))?;
        return Err(e.into());
    }
    if stats.cancelled {
        session.abort("scan cancelled")?;
        anyhow::bail!("scan cancelled");
    }
    Ok(session)
}
