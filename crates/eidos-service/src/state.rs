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
    pub follower: Arc<crate::follower::FollowerStatus>,
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
        Ok(Self {
            catalog,
            index,
            follower: Arc::new(crate::follower::FollowerStatus::default()),
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
