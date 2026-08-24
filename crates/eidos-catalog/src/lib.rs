//! Canonical catalog: the source of truth for filesystem state.
//!
//! SQLite in WAL mode. One writer at a time (SQLite's rule) with a shared
//! `busy_timeout`; readers use a small pool of separate connections so the
//! API can serve browse/health requests while a scan is committing batches.
//!
//! Invariants enforced here (ARCHITECTURE.md section 2):
//! - objects are distinct from entries (identity vs. path);
//! - scan generations publish completeness atomically; rows become visible
//!   progressively as batches commit, but the source's
//!   `published_generation` and state flip only at the end of a validated
//!   scan;
//! - a directory that could not be listed keeps its previous children.

pub mod aggregates;
pub mod archive;
pub mod changes;
pub mod content;
pub mod interactions;
pub mod jobs;
pub mod model;
pub mod policy;
pub mod projection;
pub mod read;
pub mod retry;
pub mod scan;
pub mod schema;

pub use model::*;

use parking_lot::{ArcMutexGuard, Mutex, RawMutex};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use ts_rs::TS;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid state: {0}")]
    InvalidState(String),
}

pub type Result<T> = std::result::Result<T, CatalogError>;

/// Handle to the catalog database.
pub struct Catalog {
    path: PathBuf,
    writer: Mutex<Connection>,
    writer_coordination: Arc<WriterCoordination>,
    readers: crossbeam_channel::Receiver<Connection>,
    readers_return: crossbeam_channel::Sender<Connection>,
    /// Batches recorded by [`Catalog::record_interactions`]; every Nth one
    /// enforces interaction retention in the same transaction.
    interaction_batches: AtomicU64,
}

#[derive(Debug, Default)]
pub(crate) struct WriterCoordination {
    gate: Arc<Mutex<()>>,
    acquisitions: AtomicU64,
    contended_acquisitions: AtomicU64,
    waiting: AtomicU64,
    total_wait_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    total_hold_ns: AtomicU64,
    max_hold_ns: AtomicU64,
}

impl WriterCoordination {
    pub(crate) fn acquire(self: &Arc<Self>) -> WriterPermit {
        let (guard, waited) = match self.gate.try_lock_arc() {
            Some(guard) => (guard, 0),
            None => {
                self.contended_acquisitions.fetch_add(1, Ordering::Relaxed);
                self.waiting.fetch_add(1, Ordering::Relaxed);
                let started = Instant::now();
                let guard = self.gate.lock_arc();
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                (guard, elapsed_ns(started))
            }
        };
        self.acquisitions.fetch_add(1, Ordering::Relaxed);
        self.total_wait_ns.fetch_add(waited, Ordering::Relaxed);
        self.max_wait_ns.fetch_max(waited, Ordering::Relaxed);
        WriterPermit {
            coordination: self.clone(),
            guard: Some(guard),
            acquired_at: Instant::now(),
        }
    }

    fn view(&self) -> CatalogWriterStats {
        CatalogWriterStats {
            acquisitions: self.acquisitions.load(Ordering::Relaxed),
            contended_acquisitions: self.contended_acquisitions.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            total_wait_ms: self.total_wait_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            max_wait_ms: self.max_wait_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            total_hold_ms: self.total_hold_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            max_hold_ms: self.max_hold_ns.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        }
    }
}

pub(crate) struct WriterPermit {
    coordination: Arc<WriterCoordination>,
    guard: Option<ArcMutexGuard<RawMutex, ()>>,
    acquired_at: Instant,
}

impl Drop for WriterPermit {
    fn drop(&mut self) {
        let held = elapsed_ns(self.acquired_at);
        self.coordination
            .total_hold_ns
            .fetch_add(held, Ordering::Relaxed);
        self.coordination
            .max_hold_ns
            .fetch_max(held, Ordering::Relaxed);
        if let Some(guard) = self.guard.take() {
            ArcMutexGuard::unlock_fair(guard);
        }
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

#[derive(Debug, Default, Clone, serde::Serialize, TS)]
pub struct CatalogWriterStats {
    pub acquisitions: u64,
    pub contended_acquisitions: u64,
    pub waiting: u64,
    pub total_wait_ms: f64,
    pub max_wait_ms: f64,
    pub total_hold_ms: f64,
    pub max_hold_ms: f64,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog").field("path", &self.path).finish()
    }
}

const READER_POOL: usize = 12;

impl Catalog {
    /// Open (creating if necessary) and migrate the catalog at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Catalog>> {
        configure_sqlite();
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut writer = open_connection(&path)?;
        schema::migrate(&mut writer)?;
        let (tx, rx) = crossbeam_channel::bounded(READER_POOL);
        for _ in 0..READER_POOL {
            let c = open_connection(&path)?;
            c.execute_batch("PRAGMA query_only = ON;")?;
            tx.send(c).expect("pool channel");
        }
        Ok(Arc::new(Catalog {
            path,
            writer: Mutex::new(writer),
            writer_coordination: Arc::new(WriterCoordination::default()),
            readers: rx,
            readers_return: tx,
            interaction_batches: AtomicU64::new(0),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open an additional write connection (used by long-running scan
    /// sessions so they do not hold the shared writer lock).
    pub(crate) fn open_writer(&self) -> Result<Connection> {
        open_connection(&self.path)
    }

    /// Open an uncoordinated connection for cross-crate failure injection.
    /// Never enable this feature in a production dependency graph.
    #[cfg(feature = "test-utils")]
    #[doc(hidden)]
    pub fn open_uncoordinated_writer_for_fault_injection(&self) -> Result<Connection> {
        open_connection(&self.path)
    }

    /// Run `f` with the shared writer connection.
    pub fn with_writer<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let _permit = self.writer_coordination.acquire();
        let mut guard = self.writer.lock();
        f(&mut guard)
    }

    pub fn writer_stats(&self) -> CatalogWriterStats {
        self.writer_coordination.view()
    }

    /// Run `f` with a pooled read-only connection.
    pub fn with_reader<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let conn = self.readers.recv().expect("reader pool alive");
        let out = f(&conn);
        let _ = self.readers_return.send(conn);
        out
    }

    /// Mark any scan generation left `open` by a crash as aborted, and put
    /// its source into a truthful state. Never publishes.
    pub fn recover(&self) -> Result<RecoveryReport> {
        self.with_writer(scan::recover_open_generations)
    }
}

static SQLITE_CONFIGURED: std::sync::Once = std::sync::Once::new();

/// Process-wide SQLite configuration, applied before the first connection.
/// Memory statistics are off: with them on, every allocation in every
/// connection takes one global mutex, which serialises parallel readers.
fn configure_sqlite() {
    SQLITE_CONFIGURED.call_once(|| {
        // SAFETY: called once, before any connection exists in this process
        // (sqlite3_config must precede sqlite3_initialize); the argument list
        // matches SQLITE_CONFIG_MEMSTATUS's (int).
        let rc =
            unsafe { rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0i32) };
        if rc != rusqlite::ffi::SQLITE_OK {
            tracing::warn!(
                rc,
                "sqlite3_config(MEMSTATUS) failed; readers may serialise"
            );
        }
    });
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // In-process writers coordinate before BEGIN IMMEDIATE; this is only a
    // short last-resort bound for unexpected external SQLite ownership.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(mode, "catalog could not enable WAL");
    }
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;
         PRAGMA mmap_size = 1099511627776;
         PRAGMA foreign_keys = OFF;",
    )?;
    Ok(conn)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RecoveryReport {
    pub aborted_generations: Vec<(eidos_domain::SourceId, i64)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn writer_coordination_reports_contention_and_hold_time() {
        let coordination = Arc::new(WriterCoordination::default());
        let first = coordination.acquire();
        let waiting_coordination = coordination.clone();
        let waiter = std::thread::spawn(move || {
            let _permit = waiting_coordination.acquire();
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while coordination.view().waiting == 0 {
            assert!(
                Instant::now() < deadline,
                "writer never entered the wait queue"
            );
            std::thread::yield_now();
        }
        let while_blocked = coordination.view();
        assert_eq!(while_blocked.acquisitions, 1);
        assert_eq!(while_blocked.contended_acquisitions, 1);
        assert_eq!(while_blocked.waiting, 1);

        drop(first);
        waiter.join().unwrap();

        let after = coordination.view();
        assert_eq!(after.acquisitions, 2);
        assert_eq!(after.contended_acquisitions, 1);
        assert_eq!(after.waiting, 0);
        assert!(after.total_wait_ms > 0.0);
        assert!(after.max_wait_ms > 0.0);
        assert!(after.total_hold_ms >= after.max_hold_ms);
        assert!(after.max_hold_ms > 0.0);
    }
}
