//! Shared application state.

use crate::scanner::ScanProgress;
use crate::watcher::WatcherStatus;
use crate::ServiceConfig;
use eidos_catalog::Catalog;
use eidos_domain::{HostId, SourceId};
use eidos_scanner::DirectoryLister;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

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
    pub content_worker_count: usize,
    pub exec_opts: eidos_search::exec::ExecOptions,
    pub host_id: HostId,
    pub host_name: String,
    pub lister: Arc<dyn DirectoryLister>,
    pub scans: Mutex<HashMap<SourceId, Arc<ScanProgress>>>,
    pub watchers: Mutex<HashMap<SourceId, Arc<WatcherStatus>>>,
    pub started_at: Instant,
    pub scan_threads: usize,
    pub shutdown: Arc<AtomicBool>,
    /// Whether the reconciler may start periodic rescans on its own.
    pub auto_reconcile: bool,
    /// A content-index rebuild from stored chunks was scheduled at open and
    /// runs once `start_background` spawns it.
    pub content_rebuild: bool,
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
        let requeued = catalog.requeue_running_jobs()?;
        if requeued > 0 {
            tracing::warn!(
                requeued,
                "re-queued jobs left running by a previous process"
            );
        }
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
        let state = Self {
            admission: Arc::new(crate::admission::Admission::new(config.admission.clone())),
            catalog,
            index,
            content_index,
            follower: Arc::new(crate::follower::FollowerStatus::default()),
            content_workers: Arc::new(crate::content_workers::ContentWorkersStatus::default()),
            content_enabled: AtomicBool::new(config.content),
            content_worker_count: config.content_workers,
            exec_opts: eidos_search::exec::ExecOptions::default(),
            host_id,
            host_name,
            lister: Arc::from(eidos_scanner::default_lister()),
            scans: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            started_at: Instant::now(),
            scan_threads: config.scan_threads,
            shutdown: Arc::new(AtomicBool::new(false)),
            auto_reconcile: config.auto_reconcile,
            content_rebuild: rebuild_content,
        };
        Ok(state)
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

    /// Per-source content concurrency budgets and live reservations.
    pub fn content_budgets(&self) -> &Arc<crate::source_budget::SourceBudgets> {
        &self.content_workers.budgets
    }

    pub fn watcher_status(&self, id: SourceId) -> Option<Arc<WatcherStatus>> {
        self.watchers.lock().get(&id).cloned()
    }

    /// Start change-feed watchers for every published native source and the
    /// periodic reconciler.
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
                && s.kind == eidos_domain::SourceKind::WindowsLocal
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
        for w in self.watchers.lock().values() {
            w.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for p in self.scans.lock().values() {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
