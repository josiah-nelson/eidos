//! Shared application state.

use crate::scanner::ScanProgress;
use crate::watcher::WatcherStatus;
use crate::ServiceConfig;
use eidos_catalog::Catalog;
use eidos_domain::{HostId, SourceId, UnixNanos};
use eidos_scanner::DirectoryLister;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use ts_rs::TS;

/// Why an automatic reconciliation is waiting and the earliest time the
/// scheduler will reconsider it. Manual scans intentionally ignore content
/// deferrals, but never overlap another scan generation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, TS)]
pub struct ReconciliationDeferral {
    pub reason: String,
    pub next_eligible_at: UnixNanos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconciliationDeferralCause {
    Scan,
    Content,
}

/// Durable work repaired synchronously by the most recent service open.
/// These counters stay fixed for the process lifetime so operators can see
/// what startup changed even after workers have drained the recovered queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, TS)]
pub struct StartupRecovery {
    pub aborted_scan_generations: u64,
    pub requeued_running_jobs: u64,
    pub requeued_unfinished_content: u64,
}

#[derive(Debug, Clone)]
struct StoredReconciliationDeferral {
    view: ReconciliationDeferral,
    cause: ReconciliationDeferralCause,
}

/// `path` with `suffix` appended to the file name (`catalog.db` ->
/// `catalog.db-wal`), which `Path::with_extension` cannot express.
fn with_suffix(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// Total size of the files under `dir`; unreadable entries count zero.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

pub struct AppState {
    /// Bounded gate in front of expensive HTTP operations.
    pub admission: Arc<crate::admission::Admission>,
    pub catalog: Arc<Catalog>,
    pub index: Arc<eidos_search::CatalogIndex>,
    pub content_index: Arc<eidos_search::ContentIndex>,
    pub follower: Arc<crate::follower::FollowerStatus>,
    pub content_workers: Arc<crate::content_workers::ContentWorkersStatus>,
    /// Global content switch (`--no-content` keeps workers idle).
    pub content_enabled: AtomicBool,
    /// Operator pause on claiming, and its durable marker. Unlike
    /// `content_enabled` this survives a restart; see
    /// [`crate::content_control`].
    pub content_pause: crate::content_control::ContentPause,
    pub content_worker_count: usize,
    /// Resolved data directory: `catalog.db`, the index directories, and
    /// the durable operator markers live here.
    pub data_dir: std::path::PathBuf,
    /// Cached on-disk footprint; recomputed lazily by [`AppState::storage`].
    pub storage_cache: Mutex<Option<(Instant, crate::api::StorageView)>>,
    pub exec_opts: eidos_search::exec::ExecOptions,
    /// Bounds and counters for `/api/search/export`.
    pub export: crate::export::ExportLimits,
    pub export_stats: Arc<crate::export::ExportStats>,
    /// One permit per streaming export (see [`crate::export::ExportLimits`]).
    pub export_gate: Arc<tokio::sync::Semaphore>,
    /// One permit per interaction batch waiting on the catalog writer. The
    /// capture endpoint answers without waiting for the write, so this is what
    /// keeps a burst of clients from queueing unboundedly behind a scan.
    pub interaction_writes: Arc<tokio::sync::Semaphore>,
    pub host_id: HostId,
    pub host_name: String,
    pub lister: Arc<dyn DirectoryLister>,
    pub scans: Mutex<HashMap<SourceId, Arc<ScanProgress>>>,
    reconciliation_deferrals: Mutex<HashMap<SourceId, StoredReconciliationDeferral>>,
    pub watchers: Mutex<HashMap<SourceId, Arc<WatcherStatus>>>,
    pub started_at: Instant,
    pub scan_threads: usize,
    pub shutdown: Arc<AtomicBool>,
    /// Whether the reconciler may start periodic rescans on its own.
    pub auto_reconcile: bool,
    /// A content-index rebuild from stored chunks was scheduled at open and
    /// runs once `start_background` spawns it.
    pub content_rebuild: bool,
    /// Synchronous durable-state repairs performed by [`AppState::open`].
    pub startup_recovery: StartupRecovery,
    /// The fleet runtime, once `run_with` has started it on the tokio
    /// runtime. `None` while starting up or when disabled.
    pub fleet: Mutex<Option<Arc<eidos_fleet::Fleet>>>,
}

impl AppState {
    pub fn open(config: &ServiceConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let catalog = Catalog::open(config.data_dir.join("catalog.db"))?;
        let report = catalog.recover()?;
        for (sid, gen) in &report.aborted_generations {
            tracing::warn!(
                source = sid.0,
                generation = gen,
                "aborted interrupted scan generation at startup"
            );
        }
        let requeued_running_jobs = catalog.requeue_running_jobs()?;
        if requeued_running_jobs > 0 {
            tracing::warn!(
                requeued = requeued_running_jobs,
                "re-queued jobs left running by a previous process"
            );
        }
        let requeued_unfinished_content = catalog.requeue_unfinished_content()?;
        if requeued_unfinished_content > 0 {
            tracing::warn!(
                requeued = requeued_unfinished_content,
                "re-queued content records left `indexing` by a previous process"
            );
        }
        // Interaction capture bounds itself from its own insert path, but a
        // service that is restarted often may never reach that point; one
        // prune at startup makes the bound hold regardless.
        let pruned_interactions = catalog
            .prune_interactions(eidos_catalog::interactions::InteractionRetention::default())?;
        if pruned_interactions > 0 {
            tracing::info!(
                pruned = pruned_interactions,
                "pruned interaction events past their retention bounds"
            );
        }
        let startup_recovery = StartupRecovery {
            aborted_scan_generations: report.aborted_generations.len() as u64,
            requeued_running_jobs,
            requeued_unfinished_content,
        };
        let host_name = eidos_domain::bench::hostname();
        let host_id = catalog.ensure_host(&host_name, std::env::consts::OS)?;
        let index =
            eidos_search::CatalogIndex::open(config.data_dir.join("index").join("catalog"))?;
        let content_index =
            eidos_search::ContentIndex::open(config.data_dir.join("index").join("content"))?;
        // A (re)created content index holds nothing, and an unfinished
        // rebuild leaves a partial one. The catalog keeps every chunk's text,
        // so the index is rebuilt from storage in the background instead of
        // re-reading the sources. The rebuild is *scheduled* here, before
        // anything is advertised, so search and readiness report a partial
        // content index from the first request; `start_background` runs it.
        let stats = catalog.content_stats(None)?;
        let rebuild_content = if content_index.is_rebuilding() {
            tracing::warn!(
                chunks = stats.chunks,
                "a previous content index rebuild did not finish; rebuilding again"
            );
            true
        } else if content_index.is_fresh() && stats.chunks > 0 {
            tracing::warn!(
                chunks = stats.chunks,
                "content index is empty; rebuilding it from stored chunks"
            );
            true
        } else {
            false
        };
        if rebuild_content {
            content_index.begin_rebuild(stats.chunks)?;
        }
        // An export holds at most one admission permit at a time. When the
        // shared gate has several slots, keep the export bound strictly below
        // it; with a one-slot gate, export pages are non-queueing and yield to
        // interactive waiters in Admission::run_export.
        let mut export_limits = config.export;
        export_limits.concurrency = export_limits
            .concurrency
            .min(config.admission.concurrency.saturating_sub(1))
            .max(1);
        let state = Self {
            admission: Arc::new(crate::admission::Admission::new(config.admission.clone())),
            catalog,
            index,
            content_index,
            follower: Arc::new(crate::follower::FollowerStatus::default()),
            content_workers: Arc::new(crate::content_workers::ContentWorkersStatus::default()),
            content_enabled: AtomicBool::new(config.content),
            content_pause: crate::content_control::ContentPause::load(&config.data_dir),
            content_worker_count: crate::content_workers::load_workers_override(&config.data_dir)
                .unwrap_or(config.content_workers),
            data_dir: config.data_dir.clone(),
            storage_cache: Mutex::new(None),
            exec_opts: eidos_search::exec::ExecOptions::default(),
            export: export_limits,
            export_stats: Arc::new(crate::export::ExportStats::default()),
            export_gate: Arc::new(tokio::sync::Semaphore::new(export_limits.concurrency)),
            interaction_writes: Arc::new(tokio::sync::Semaphore::new(
                crate::interactions_api::MAX_PENDING_INTERACTION_WRITES,
            )),
            host_id,
            host_name,
            lister: Arc::from(eidos_scanner::default_lister()),
            scans: Mutex::new(HashMap::new()),
            reconciliation_deferrals: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            started_at: Instant::now(),
            scan_threads: config.scan_threads,
            shutdown: Arc::new(AtomicBool::new(false)),
            auto_reconcile: config.auto_reconcile,
            content_rebuild: rebuild_content,
            startup_recovery,
            fleet: Mutex::new(None),
        };
        Ok(state)
    }

    /// On-disk footprint of the catalog and both indexes, cached briefly so
    /// the UI's 2-second activity poll does not walk index directories on
    /// every call.
    pub fn storage(&self) -> crate::api::StorageView {
        const REFRESH: std::time::Duration = std::time::Duration::from_secs(15);
        {
            let cache = self.storage_cache.lock();
            if let Some((at, view)) = *cache {
                if at.elapsed() < REFRESH {
                    return view;
                }
            }
        }
        let db = self.catalog.path();
        let catalog_db_bytes = [
            db.to_path_buf(),
            with_suffix(db, "-wal"),
            with_suffix(db, "-shm"),
        ]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
        let view = crate::api::StorageView {
            catalog_db_bytes,
            catalog_index_bytes: dir_bytes(&self.data_dir.join("index").join("catalog")),
            content_index_bytes: dir_bytes(&self.data_dir.join("index").join("content")),
        };
        *self.storage_cache.lock() = Some((Instant::now(), view));
        view
    }

    /// Progress of a running (or just-finished, not yet reaped) scan.
    pub fn scan_progress(&self, id: SourceId) -> Option<Arc<ScanProgress>> {
        let mut scans = self.scans.lock();
        let p = scans.get(&id).cloned()?;
        if p.is_finished() && p.finished_for().is_some_and(|d| d.as_secs() > 30) {
            scans.remove(&id);
        }
        Some(p)
    }

    pub fn reconciliation_deferral(&self, id: SourceId) -> Option<ReconciliationDeferral> {
        self.reconciliation_deferrals
            .lock()
            .get(&id)
            .map(|stored| stored.view.clone())
    }

    pub(crate) fn scheduled_reconciliation_deferral(
        &self,
        id: SourceId,
    ) -> Option<(ReconciliationDeferral, ReconciliationDeferralCause)> {
        self.reconciliation_deferrals
            .lock()
            .get(&id)
            .map(|stored| (stored.view.clone(), stored.cause))
    }

    pub(crate) fn defer_reconciliation(
        &self,
        id: SourceId,
        view: ReconciliationDeferral,
        cause: ReconciliationDeferralCause,
    ) {
        self.reconciliation_deferrals
            .lock()
            .insert(id, StoredReconciliationDeferral { view, cause });
    }

    pub(crate) fn clear_reconciliation_deferral(&self, id: SourceId) {
        self.reconciliation_deferrals.lock().remove(&id);
    }

    /// Per-source content concurrency budgets and live reservations.
    pub fn content_budgets(&self) -> &Arc<crate::source_budget::SourceBudgets> {
        &self.content_workers.budgets
    }

    pub fn watcher_status(&self, id: SourceId) -> Option<Arc<WatcherStatus>> {
        self.watchers.lock().get(&id).cloned()
    }

    /// Resume change-feed watchers for published sources with a durable native
    /// cursor, then start the periodic reconciler for feed-less sources.
    pub fn start_background(self: &Arc<Self>) -> anyhow::Result<()> {
        if self.content_rebuild {
            // Runs under the index writer gate: content workers and the
            // commit coordinator wait until it finishes (or fails).
            let st = self.clone();
            std::thread::Builder::new()
                .name("content-rebuild".into())
                .spawn(move || {
                    if let Err(e) = st.content_index.run_rebuild(&st.catalog, &st.shutdown) {
                        tracing::error!(error = %e, "content index rebuild failed");
                    }
                })
                .expect("spawn content rebuild");
        }
        for s in self.catalog.list_sources()? {
            if s.published_generation.is_some()
                && matches!(
                    s.kind,
                    eidos_domain::SourceKind::WindowsLocal | eidos_domain::SourceKind::MacosLocal
                )
                && eidos_catalog::changes::is_native_feed_checkpoint(s.checkpoint_kind.as_deref())
                && s.state != eidos_domain::SourceState::Retired
            {
                crate::watcher::ensure_watcher(self, s.id);
            }
        }
        crate::watcher::spawn_reconciler(self);
        crate::follower::spawn_follower(self);
        crate::content_workers::spawn_content_workers(self, self.content_worker_count);
        Ok(())
    }

    pub fn request_shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Shed queued expensive work at once: graceful shutdown then waits
        // only for the operations that already hold a permit.
        self.admission.close();
        // Sessions say goodbye and stop; nothing durable depends on them.
        if let Some(fleet) = self.fleet.lock().take() {
            fleet.shutdown();
        }
        // Do not hold the registry lock while cancellation waits for an
        // in-flight watcher mutation to finish. The mutation may start a
        // recovery scan, and shutdown should not introduce a lock-order
        // dependency between those independent registries.
        let watchers: Vec<_> = self.watchers.lock().values().cloned().collect();
        for w in watchers {
            w.request_cancel();
        }
        for p in self.scans.lock().values() {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
