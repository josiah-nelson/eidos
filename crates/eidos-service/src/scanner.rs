//! Background scan execution with observable progress.

use crate::state::AppState;
use crate::watcher;
use eidos_catalog::scan::{ScanKind, ScanSummary};
use eidos_domain::{SourceId, SourceKind};
use eidos_scanner::WalkOptions;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

#[derive(Debug, Clone, serde::Serialize)]
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
    fn new(source_id: SourceId) -> Self {
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
    #[error("{0}")]
    Catalog(#[from] eidos_catalog::CatalogError),
}

/// Start a scan on a dedicated thread. Native sources run the full
/// checkpoint → enumerate → replay → watch sequence. Returns immediately.
pub fn start_scan(
    state: &Arc<AppState>,
    source_id: SourceId,
) -> Result<Arc<ScanProgress>, StartScanError> {
    let progress = {
        let mut scans = state.scans.lock();
        if let Some(p) = scans.get(&source_id) {
            if !p.is_finished() {
                return Err(StartScanError::AlreadyRunning);
            }
        }
        let progress = Arc::new(ScanProgress::new(source_id));
        scans.insert(source_id, progress.clone());
        progress
    };
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
    Ok(progress)
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
    let source = state
        .catalog
        .get_source(source_id)?
        .ok_or_else(|| anyhow::anyhow!("source {source_id} not found"))?;
    // Refresh volume capabilities (read-only probe).
    match state
        .lister
        .volume_info(std::path::Path::new(&source.root_path))
    {
        Ok(v) => {
            state.catalog.upsert_volume(state.host_id, source_id, &v)?;
            let kind = if v.is_remote() {
                SourceKind::Smb
            } else if v.is_native_local() {
                SourceKind::WindowsLocal
            } else {
                SourceKind::WindowsGeneric
            };
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
    Ok(session.finish()?)
}
