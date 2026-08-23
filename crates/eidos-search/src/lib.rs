//! Tantivy-backed search indexes.
//!
//! `CatalogIndex` is the metadata projection: one document per live entry,
//! rebuilt per source from the published generation and kept current by the
//! outbox follower. Query execution (`exec`) compiles the typed AST into
//! Tantivy queries, verifies exact/case-sensitive semantics against stored
//! originals, and joins current state from the catalog.

pub mod content;
pub mod exec;
pub mod facets;
pub mod pipeline;
pub mod projection;
pub mod regex_plan;
pub mod schema;

pub use content::ContentIndex;

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

pub const PROJECTION_NAME: &str = "catalog_index";
const META_FILE: &str = "eidos-schema.json";

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("index: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("catalog: {0}")]
    Catalog(#[from] eidos_catalog::CatalogError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Query(#[from] eidos_domain::QueryError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, SearchError>;

pub struct CatalogIndex {
    dir: PathBuf,
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: schema::Fields,
    /// Set when `open` (re)created the directory: the catalog's recorded
    /// projection state is then stale and every source must be rebuilt.
    recreated: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for CatalogIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogIndex")
            .field("dir", &self.dir)
            .finish()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    schema_version: u32,
}

impl CatalogIndex {
    /// Open the index at `dir`, creating it (or recreating it on a schema
    /// version change). A recreated index has no documents until the
    /// follower rebuilds each source.
    pub fn open(dir: impl AsRef<Path>) -> Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let (schema, fields) = schema::build_schema();
        let meta_path = dir.join(META_FILE);
        let current: Option<Meta> = std::fs::read(&meta_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let needs_create = match current {
            Some(ref m) if m.schema_version == schema::CATALOG_SCHEMA_VERSION => {
                !dir.join("meta.json").exists()
            }
            _ => true,
        };
        if needs_create {
            // Wipe anything stale so Tantivy sees a clean directory.
            if dir.join("meta.json").exists() || current.is_some() {
                tracing::warn!(dir = %dir.display(), "recreating catalog index (schema changed or missing)");
                for entry in std::fs::read_dir(&dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            Index::create_in_dir(&dir, schema.clone())?;
            std::fs::write(
                &meta_path,
                serde_json::to_vec(&Meta {
                    schema_version: schema::CATALOG_SCHEMA_VERSION,
                })
                .expect("meta"),
            )?;
        }
        let index = Index::open_in_dir(&dir)?;
        content::register_tokenizers(&index);
        let writer = index.writer_with_num_threads(2, 96 * 1024 * 1024)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Arc::new(Self {
            dir,
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
            recreated: std::sync::atomic::AtomicBool::new(needs_create),
        }))
    }

    pub fn fields(&self) -> &schema::Fields {
        &self.fields
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }

    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    pub fn writer(&self) -> parking_lot::MutexGuard<'_, IndexWriter> {
        self.writer.lock()
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the index was just (re)created and is empty.
    pub fn is_empty(&self) -> bool {
        self.num_docs() == 0
    }

    /// True once after a (re)creation; `sync_sources` consumes it.
    pub fn take_recreated(&self) -> bool {
        self.recreated
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}
