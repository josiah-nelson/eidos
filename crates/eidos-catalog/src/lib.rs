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
pub mod changes;
pub mod content;
pub mod jobs;
pub mod model;
pub mod policy;
pub mod projection;
pub mod read;
pub mod scan;
pub mod schema;

pub use model::*;

use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    readers: crossbeam_channel::Receiver<Connection>,
    readers_return: crossbeam_channel::Sender<Connection>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog").field("path", &self.path).finish()
    }
}

const READER_POOL: usize = 4;

impl Catalog {
    /// Open (creating if necessary) and migrate the catalog at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Catalog>> {
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
            readers: rx,
            readers_return: tx,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open an additional write connection (used by long-running scan
    /// sessions so they do not hold the shared writer lock).
    pub fn open_writer(&self) -> Result<Connection> {
        open_connection(&self.path)
    }

    /// Run `f` with the shared writer connection.
    pub fn with_writer<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let mut guard = self.writer.lock();
        f(&mut guard)
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

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(mode, "catalog could not enable WAL");
    }
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;
         PRAGMA mmap_size = 268435456;
         PRAGMA foreign_keys = OFF;",
    )?;
    Ok(conn)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RecoveryReport {
    pub aborted_generations: Vec<(eidos_domain::SourceId, i64)>,
}
