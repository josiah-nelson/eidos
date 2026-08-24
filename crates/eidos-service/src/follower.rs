//! Catalog-index follower: rebuilds sources whose published generation
//! changed and drains the outbox into the Tantivy index.

use crate::state::AppState;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ts_rs::TS;

/// Fallback wake-up interval. The follower is event-driven — every completed
/// catalog writer transaction signals it — so this only bounds staleness if a
/// signal is ever missed.
pub const FOLLOW_INTERVAL: Duration = Duration::from_millis(500);
pub const FOLLOW_BATCH: u32 = 2000;

#[derive(Debug, Default)]
pub struct FollowerStatus {
    pub iterations: AtomicU64,
    pub rebuilds: AtomicU64,
    pub rows_applied: AtomicU64,
    pub documents_added: AtomicU64,
    pub last_seq: AtomicI64,
    pub last_rebuild_ms: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    pub last_activity: Mutex<Option<Instant>>,
    pub rebuilding: Mutex<Option<i64>>,
}

#[derive(Debug, Clone, serde::Serialize, TS)]
pub struct FollowerView {
    pub iterations: u64,
    pub rebuilds: u64,
    pub rows_applied: u64,
    pub documents_added: u64,
    pub last_seq: i64,
    pub last_rebuild_ms: u64,
    pub last_error: Option<String>,
    pub last_activity_ms_ago: Option<u64>,
    pub rebuilding_source: Option<i64>,
    pub index_documents: u64,
    pub outbox_pending: u64,
}

impl FollowerStatus {
    pub fn view(&self, state: &AppState) -> FollowerView {
        FollowerView {
            iterations: self.iterations.load(Ordering::Relaxed),
            rebuilds: self.rebuilds.load(Ordering::Relaxed),
            rows_applied: self.rows_applied.load(Ordering::Relaxed),
            documents_added: self.documents_added.load(Ordering::Relaxed),
            last_seq: self.last_seq.load(Ordering::Relaxed),
            last_rebuild_ms: self.last_rebuild_ms.load(Ordering::Relaxed),
            last_error: self.last_error.lock().clone(),
            last_activity_ms_ago: self
                .last_activity
                .lock()
                .map(|t| t.elapsed().as_millis() as u64),
            rebuilding_source: *self.rebuilding.lock(),
            index_documents: state.index.num_docs(),
            outbox_pending: state.catalog.outbox_pending().unwrap_or(0),
        }
    }
}

/// Run one follower iteration synchronously (used by tests and the CLI).
pub fn follow_once(state: &AppState) -> anyhow::Result<()> {
    let status = &state.follower;
    status.iterations.fetch_add(1, Ordering::Relaxed);
    let (rebuilt, follow) = state.index.follow_once(&state.catalog, FOLLOW_BATCH)?;
    for r in &rebuilt {
        status.rebuilds.fetch_add(1, Ordering::Relaxed);
        status
            .last_rebuild_ms
            .store(r.elapsed_ms as u64, Ordering::Relaxed);
        status
            .documents_added
            .fetch_add(r.documents, Ordering::Relaxed);
        *status.last_activity.lock() = Some(Instant::now());
    }
    if let Some(f) = follow {
        status.rows_applied.fetch_add(f.rows, Ordering::Relaxed);
        status
            .documents_added
            .fetch_add(f.documents_added, Ordering::Relaxed);
        status.last_seq.store(f.last_seq, Ordering::Relaxed);
        *status.last_activity.lock() = Some(Instant::now());
        tracing::debug!(
            rows = f.rows,
            added = f.documents_added,
            deleted = f.documents_deleted,
            ms = f.elapsed_ms as u64,
            "index follower applied outbox batch"
        );
    }
    *status.last_error.lock() = None;
    Ok(())
}

pub fn spawn_follower(state: &Arc<AppState>) {
    spawn_follower_with_fallback(state, FOLLOW_INTERVAL);
}

/// Spawn the follower with an explicit fallback interval. Visibility is
/// driven by the catalog's write signal; the fallback only bounds staleness
/// if a signal is missed (tests pass a long interval to prove the signal
/// path alone delivers visibility).
pub fn spawn_follower_with_fallback(state: &Arc<AppState>, fallback: Duration) {
    let st = state.clone();
    std::thread::Builder::new()
        .name("index-follower".into())
        .spawn(move || loop {
            if st.shutdown.load(Ordering::Relaxed) {
                return;
            }
            // Observe the write sequence before draining: a write landing
            // while this iteration runs makes the wait below return at once,
            // so nothing that happened mid-drain is left waiting.
            let seen = st.catalog.write_seq();
            if let Err(e) = follow_once(&st) {
                tracing::error!(error = %e, "index follower iteration failed");
                *st.follower.last_error.lock() = Some(e.to_string());
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            // A batch-limited drain leaves rows pending: loop immediately.
            if st.catalog.outbox_pending().unwrap_or(0) > 0 {
                continue;
            }
            st.catalog.wait_for_write(seen, fallback);
        })
        .expect("spawn follower");
}
