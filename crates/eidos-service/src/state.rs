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
    pub catalog: Arc<Catalog>,
    pub index: Arc<eidos_search::CatalogIndex>,
    pub content_index: Arc<eidos_search::ContentIndex>,
    pub follower: Arc<crate::follower::FollowerStatus>,
    pub content_workers: Arc<crate::content_workers::ContentWorkersStatus>,
    /// Global content switch (`--no-content` keeps workers idle).
    pub content_enabled: AtomicBool,
    pub content_worker_count: usize,
    /// Per-source concurrency budgets, refreshed by the coordinator.
    pub content_budgets: Mutex<HashMap<SourceId, u32>>,
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
        if content_index.is_fresh() {
            // A (re)created content index holds nothing: every indexed object
            // must be re-extracted so the catalog and index agree.
            let n = catalog.reset_content_for_reindex()?;
            if n > 0 {
                tracing::warn!(
                    n,
                    "content index is empty; indexed objects reset to pending"
                );
            }
        }
        Ok(Self {
            catalog,
            index,
            content_index,
            follower: Arc::new(crate::follower::FollowerStatus::default()),
            content_workers: Arc::new(crate::content_workers::ContentWorkersStatus::default()),
            content_enabled: AtomicBool::new(config.content),
            content_worker_count: config.content_workers,
            content_budgets: Mutex::new(HashMap::new()),
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
        })
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

    pub fn watcher_status(&self, id: SourceId) -> Option<Arc<WatcherStatus>> {
        self.watchers.lock().get(&id).cloned()
    }

    /// Start change-feed watchers for every published native source and the
    /// periodic reconciler.
    pub fn start_background(self: &Arc<Self>) -> anyhow::Result<()> {
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
        for w in self.watchers.lock().values() {
            w.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for p in self.scans.lock().values() {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
