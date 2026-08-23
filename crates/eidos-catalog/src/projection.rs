//! Rows for derived-index projections and projection bookkeeping.
//!
//! A projection (e.g. the Tantivy catalog index) is rebuilt per source from
//! the published generation and then follows the outbox. The catalog owns
//! the follower position (`projection_state`) and the per-source built
//! generation (`projection_sources`) so restarts resume exactly and the API
//! can report when the search index lags the catalog.
//!
//! Reads here are batched on purpose. A rebuild resolves at most
//! [`PROJECTION_BATCH`] rows at a time and preloads their descendant
//! extensions with one query per batch; incremental updates resolve the
//! parent chain of a whole batch of objects through one cache, so a subtree
//! rebuild walks each ancestor once instead of once per descendant.

use crate::read::get_source_conn;
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{ContentState, FileAttributes, ObjectId, ObjectKind, SourceId, UnixNanos};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashMap;

/// Rows resolved per batch. Bounds what a rebuild holds at once — at most
/// this many [`ProjectionRow`]s plus their descendant-extension lists — and
/// keeps batched lookups well under SQLite's parameter limit.
pub const PROJECTION_BATCH: usize = 1024;

/// Ancestor-walk cap, matching `read::render_path_conn`.
const MAX_DEPTH: usize = 512;

thread_local! {
    static QUERIES: Cell<u64> = const { Cell::new(0) };
}

/// SQL statements this thread has executed on the projection read path.
///
/// Instrumentation only (projection reads are synchronous on the calling
/// thread, so the counter is per-thread and test-safe): the batching tests
/// assert a rebuild issues O(batches) queries, not one per directory.
pub fn query_count() -> u64 {
    QUERIES.with(|q| q.get())
}

/// Reset this thread's projection query counter.
pub fn reset_query_count() {
    QUERIES.with(|q| q.set(0));
}

fn counted() {
    QUERIES.with(|q| q.set(q.get() + 1));
}

/// Everything a search document needs for one live entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionRow {
    pub entry_id: i64,
    pub object_id: ObjectId,
    pub source_id: SourceId,
    pub parent_id: Option<ObjectId>,
    /// Ancestor object ids from the root down to the parent.
    pub ancestors: Vec<ObjectId>,
    pub name: String,
    pub path: String,
    pub extension: String,
    pub kind: ObjectKind,
    pub size: u64,
    pub allocated: u64,
    pub modified: Option<UnixNanos>,
    pub created: Option<UnixNanos>,
    pub attributes: FileAttributes,
    pub content_state: ContentState,
    pub generation: u32,
    pub link_count: u32,
    /// Directory aggregate (directories only).
    pub file_count: u64,
    pub dir_count: u64,
    pub subtree_logical: u64,
    pub subtree_allocated: u64,
    pub newest_modified: Option<UnixNanos>,
    pub agg_complete: bool,
    /// Extensions present anywhere beneath (directories only).
    pub desc_extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSourceState {
    pub source_id: SourceId,
    pub generation: i64,
    pub documents: u64,
    pub built_at: UnixNanos,
}

const ROW_SQL: &str = "SELECT e.entry_id, e.object_id, e.parent_id, e.name, e.extension, o.kind, o.size, o.allocated, o.modified,
        o.created, o.attributes, o.content_state, o.generation, o.link_count,
        a.file_count, a.dir_count, a.logical_bytes, a.allocated_bytes, a.newest_modified, a.complete,
        e.source_id
     FROM entries e JOIN objects o ON o.object_id = e.object_id
     LEFT JOIN directory_aggregates a ON a.object_id = o.object_id";

/// Every object that can appear inside a path: directories, virtual
/// directories, and archive containers (files that own virtual members).
const PATH_NODE_SQL: &str = "SELECT e.object_id, e.parent_id, e.name FROM entries e JOIN objects o ON o.object_id = e.object_id
     WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL
       AND (o.kind IN ('directory', 'virtual_directory')
            OR e.object_id IN (SELECT parent_id FROM entries
                               WHERE source_id = ?1 AND is_virtual = 1
                                 AND parent_id IS NOT NULL AND deleted_at IS NULL))
     ORDER BY e.entry_id";

struct DirMap {
    /// object_id -> (parent_id, name)
    parents: HashMap<i64, (Option<i64>, String)>,
    root_path: String,
    paths: HashMap<i64, (String, Vec<ObjectId>)>,
}

impl DirMap {
    fn load(conn: &Connection, source_id: SourceId, root_path: &str) -> Result<Self> {
        let mut parents: HashMap<i64, (Option<i64>, String)> = HashMap::new();
        counted();
        let mut stmt = conn.prepare_cached(PATH_NODE_SQL)?;
        let rows = stmt.query_map(params![source_id.0], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (obj, parent, name) = row?;
            // First live entry wins, as `read::render_path_conn` does.
            parents.entry(obj).or_insert((parent, name));
        }
        Ok(Self {
            parents,
            root_path: root_path.trim_end_matches(['\\', '/']).to_string(),
            paths: HashMap::new(),
        })
    }

    /// `(path, ancestors)` of a directory object.
    fn dir(&mut self, obj: i64) -> (String, Vec<ObjectId>) {
        if let Some(p) = self.paths.get(&obj) {
            return p.clone();
        }
        let result = match self.parents.get(&obj).cloned() {
            None => (self.root_path.clone(), Vec::new()),
            Some((None, _)) => (root_display(&self.root_path), Vec::new()),
            Some((Some(parent), name)) => {
                let (ppath, mut anc) = self.dir(parent);
                anc.push(ObjectId(parent));
                (format!("{}\\{}", ppath.trim_end_matches('\\'), name), anc)
            }
        };
        self.paths.insert(obj, result.clone());
        result
    }
}

fn root_display(root: &str) -> String {
    if root.len() == 2 && root.ends_with(':') {
        format!("{root}\\")
    } else {
        root.to_string()
    }
}

fn row_from(
    r: &rusqlite::Row<'_>,
    path: String,
    ancestors: Vec<ObjectId>,
    desc_extensions: Vec<String>,
) -> rusqlite::Result<ProjectionRow> {
    let kind_s: String = r.get(5)?;
    let state_s: String = r.get(11)?;
    Ok(ProjectionRow {
        entry_id: r.get(0)?,
        object_id: ObjectId(r.get(1)?),
        source_id: SourceId(r.get(20)?),
        parent_id: r.get::<_, Option<i64>>(2)?.map(ObjectId),
        ancestors,
        name: r.get(3)?,
        path,
        extension: r.get(4)?,
        kind: ObjectKind::parse(&kind_s).unwrap_or(ObjectKind::File),
        size: r.get::<_, i64>(6)? as u64,
        allocated: r.get::<_, i64>(7)? as u64,
        modified: r.get::<_, Option<i64>>(8)?.map(UnixNanos),
        created: r.get::<_, Option<i64>>(9)?.map(UnixNanos),
        attributes: FileAttributes(r.get::<_, i64>(10)? as u32),
        content_state: ContentState::parse(&state_s).unwrap_or(ContentState::Pending),
        generation: r.get::<_, i64>(12)? as u32,
        link_count: r.get::<_, i64>(13)? as u32,
        file_count: r.get::<_, Option<i64>>(14)?.unwrap_or(0) as u64,
        dir_count: r.get::<_, Option<i64>>(15)?.unwrap_or(0) as u64,
        subtree_logical: r.get::<_, Option<i64>>(16)?.unwrap_or(0) as u64,
        subtree_allocated: r.get::<_, Option<i64>>(17)?.unwrap_or(0) as u64,
        newest_modified: r.get::<_, Option<i64>>(18)?.map(UnixNanos),
        agg_complete: r.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
        desc_extensions,
    })
}

/// Placeholder list padded to the next power of two: `IN (?, ?)` tolerates
/// repeated ids, and the padding keeps the prepared-statement cache to a
/// handful of shapes instead of one per distinct batch size.
fn placeholders(ids: &[i64]) -> (String, Vec<i64>) {
    let n = ids.len().next_power_of_two();
    let mut padded = Vec::with_capacity(n);
    padded.extend_from_slice(ids);
    while padded.len() < n {
        padded.push(ids[ids.len() - 1]);
    }
    let mut sql = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
    (sql, padded)
}

/// Descendant extensions for a batch of directories, in one query. Rows come
/// back ordered by `(object_id, extension)`, which is the order a per-object
/// query on this `WITHOUT ROWID` table produces.
fn desc_extensions_batch(conn: &Connection, dirs: &[i64]) -> Result<HashMap<i64, Vec<String>>> {
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    if dirs.is_empty() {
        return Ok(out);
    }
    let (ph, padded) = placeholders(dirs);
    counted();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT object_id, extension FROM directory_extension_counts
         WHERE object_id IN ({ph}) ORDER BY object_id, extension"
    ))?;
    let rows = stmt.query_map(params_from_iter(padded.iter()), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (obj, ext) = row?;
        out.entry(obj).or_default().push(ext);
    }
    Ok(out)
}

/// Attach descendant extensions to a batch of rows with one query, then hand
/// each row to `f`. Returns the number of rows emitted.
fn flush_batch(
    conn: &Connection,
    batch: &mut Vec<ProjectionRow>,
    f: &mut impl FnMut(ProjectionRow) -> Result<()>,
) -> Result<u64> {
    if batch.is_empty() {
        return Ok(0);
    }
    let mut dirs: Vec<i64> = batch
        .iter()
        .filter(|r| r.kind.is_directory_like())
        .map(|r| r.object_id.0)
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    let exts = desc_extensions_batch(conn, &dirs)?;
    let n = batch.len() as u64;
    for mut row in batch.drain(..) {
        if row.kind.is_directory_like() {
            row.desc_extensions = exts.get(&row.object_id.0).cloned().unwrap_or_default();
        }
        f(row)?;
    }
    Ok(n)
}

/// Cache of parent chains for incremental updates: each object's first live
/// entry, each resolved path, and each ancestor list is looked up once for a
/// whole batch of objects instead of once per row.
///
/// Only chain nodes (directories, virtual directories, archive containers)
/// stay cached across batches; the entries of the objects being projected are
/// dropped at each batch boundary, so a subtree of millions of files costs
/// one batch of entries plus the chains of the directories it contains.
#[derive(Default)]
struct ChainCache {
    /// object_id -> first live entry `(parent_id, name)`; `None` when the
    /// object has no live entry. Cleared between batches.
    entries: HashMap<i64, Option<(Option<i64>, String)>>,
    /// The same, for objects reached by walking up from a batch: these are
    /// chain nodes by definition, so they are kept across batches.
    nodes: HashMap<i64, Option<(Option<i64>, String)>>,
    /// Chain node object_id -> rendered path; `None` when the chain is
    /// broken.
    paths: HashMap<i64, Option<String>>,
    /// parent object_id -> ancestors from the root down to that parent.
    ancestors: HashMap<i64, Vec<ObjectId>>,
    /// source_id -> root path, trimmed of trailing separators.
    roots: HashMap<i64, String>,
}

impl ChainCache {
    /// Start a batch: drop the previous batch's entries and load the first
    /// live entry of every object in `ids` with one query.
    fn preload(&mut self, conn: &Connection, ids: &[i64]) -> Result<()> {
        self.entries.clear();
        if ids.is_empty() {
            return Ok(());
        }
        let wanted = ids;
        let (ph, padded) = placeholders(wanted);
        counted();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT object_id, parent_id, name FROM entries
             WHERE object_id IN ({ph}) AND deleted_at IS NULL ORDER BY object_id, entry_id"
        ))?;
        let rows = stmt.query_map(params_from_iter(padded.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (obj, parent, name) = row?;
            self.entries.entry(obj).or_insert(Some((parent, name)));
        }
        for id in wanted {
            self.entries.entry(*id).or_insert(None);
        }
        Ok(())
    }

    /// Keep this batch object's entry after the batch ends: it is a chain
    /// node, so later batches will walk through it.
    fn promote(&mut self, object: i64) {
        if let Some(e) = self.entries.get(&object).cloned() {
            self.nodes.entry(object).or_insert(e);
        }
    }

    fn entry(&mut self, conn: &Connection, object: i64) -> Result<Option<(Option<i64>, String)>> {
        if let Some(e) = self
            .nodes
            .get(&object)
            .or_else(|| self.entries.get(&object))
        {
            return Ok(e.clone());
        }
        // Only an object outside the current batch gets here, and it is only
        // ever reached by walking up: a chain node worth keeping.
        counted();
        let found = conn
            .prepare_cached(
                "SELECT parent_id, name FROM entries WHERE object_id = ?1 AND deleted_at IS NULL
                 ORDER BY entry_id LIMIT 1",
            )?
            .query_row(params![object], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?))
            })
            .optional()?;
        self.nodes.insert(object, found.clone());
        Ok(found)
    }

    fn root(&mut self, conn: &Connection, source: SourceId) -> Result<String> {
        if let Some(r) = self.roots.get(&source.0) {
            return Ok(r.clone());
        }
        counted();
        let root: String = conn.query_row(
            "SELECT root_path FROM sources WHERE source_id = ?1",
            params![source.0],
            |r| r.get(0),
        )?;
        let root = root.trim_end_matches(['\\', '/']).to_string();
        self.roots.insert(source.0, root.clone());
        Ok(root)
    }

    /// Path of `object`, rendered exactly like `read::render_path_conn`:
    /// the first live entry of each node, from the source root down. Empty
    /// when a node in the chain has no live entry (or the chain is deeper
    /// than [`MAX_DEPTH`]).
    ///
    /// `is_node` says whether `object` can itself be a parent; leaves are
    /// resolved without being cached, since nothing else needs their path.
    fn path(
        &mut self,
        conn: &Connection,
        source: SourceId,
        object: i64,
        is_node: bool,
    ) -> Result<String> {
        if let Some(p) = self.paths.get(&object) {
            return Ok(p.clone().unwrap_or_default());
        }
        // Walk up to the nearest node with a known path (or the root),
        // remembering the names passed on the way down.
        let mut chain: Vec<(i64, String)> = Vec::new();
        let mut cur = object;
        let mut base: Option<String> = None;
        for _ in 0..MAX_DEPTH {
            if let Some(known) = self.paths.get(&cur) {
                base = known.clone();
                break;
            }
            match self.entry(conn, cur)? {
                None => break,
                Some((None, _)) => {
                    base = Some(root_display(&self.root(conn, source)?));
                    self.paths.insert(cur, base.clone());
                    break;
                }
                Some((Some(parent), name)) => {
                    chain.push((cur, name));
                    cur = parent;
                }
            }
        }
        for (node, name) in chain.into_iter().rev() {
            let rendered = base
                .as_ref()
                .map(|b| format!("{}\\{}", b.trim_end_matches('\\'), name));
            if is_node || node != object {
                self.paths.insert(node, rendered.clone());
            }
            base = rendered;
        }
        Ok(base.unwrap_or_default())
    }

    /// Ancestors from the root down to `parent` (empty when there is none).
    fn ancestors(&mut self, conn: &Connection, parent: Option<i64>) -> Result<Vec<ObjectId>> {
        let Some(parent) = parent else {
            return Ok(Vec::new());
        };
        if let Some(a) = self.ancestors.get(&parent) {
            return Ok(a.clone());
        }
        // Walk up collecting the chain, then fill the cache top-down.
        let mut chain = vec![parent];
        let mut cur = parent;
        let mut prefix: Vec<ObjectId> = Vec::new();
        for _ in 0..MAX_DEPTH {
            let next = self.entry(conn, cur)?.and_then(|(p, _)| p);
            match next {
                None => break,
                Some(p) => {
                    if let Some(a) = self.ancestors.get(&p) {
                        prefix = a.clone();
                        break;
                    }
                    chain.push(p);
                    cur = p;
                }
            }
        }
        for node in chain.into_iter().rev() {
            prefix.push(ObjectId(node));
            self.ancestors.insert(node, prefix.clone());
        }
        Ok(self.ancestors.get(&parent).cloned().unwrap_or_default())
    }
}

impl Catalog {
    /// Stream every live entry of a source (including the root entry) to `f`.
    pub fn for_each_projection_row(
        &self,
        source_id: SourceId,
        mut f: impl FnMut(ProjectionRow) -> Result<()>,
    ) -> Result<u64> {
        self.with_reader(|conn| {
            counted();
            let src = get_source_conn(conn, source_id)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
            let mut dirs = DirMap::load(conn, source_id, &src.root_path)?;
            counted();
            let mut stmt = conn.prepare_cached(&format!(
                "{ROW_SQL} WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL"
            ))?;
            let mut rows = stmt.query(params![source_id.0])?;
            let mut n = 0u64;
            let mut batch: Vec<ProjectionRow> = Vec::with_capacity(PROJECTION_BATCH);
            while let Some(r) = rows.next()? {
                let object_id: i64 = r.get(1)?;
                let parent: Option<i64> = r.get(2)?;
                let name: String = r.get(3)?;
                let kind: String = r.get(5)?;
                let is_dir = ObjectKind::parse(&kind).is_some_and(ObjectKind::is_directory_like);
                let (path, ancestors) = if is_dir {
                    dirs.dir(object_id)
                } else {
                    match parent {
                        Some(p) => {
                            let (ppath, mut anc) = dirs.dir(p);
                            anc.push(ObjectId(p));
                            (format!("{}\\{}", ppath.trim_end_matches('\\'), name), anc)
                        }
                        None => (root_display(&dirs.root_path), Vec::new()),
                    }
                };
                batch.push(row_from(r, path, ancestors, Vec::new())?);
                if batch.len() >= PROJECTION_BATCH {
                    n += flush_batch(conn, &mut batch, &mut f)?;
                }
            }
            n += flush_batch(conn, &mut batch, &mut f)?;
            Ok(n)
        })
    }

    /// Stream the projection rows of many objects (one per live entry),
    /// resolving parent chains and descendant extensions once per batch.
    ///
    /// Objects with no live entry contribute nothing. Duplicates in
    /// `objects` are not filtered; callers that must index each object once
    /// pass a deduplicated slice.
    pub fn for_each_projection_row_of(
        &self,
        objects: &[ObjectId],
        mut f: impl FnMut(ProjectionRow) -> Result<()>,
    ) -> Result<u64> {
        if objects.is_empty() {
            return Ok(0);
        }
        self.with_reader(|conn| {
            let mut chains = ChainCache::default();
            let mut n = 0u64;
            for chunk in objects.chunks(PROJECTION_BATCH) {
                let mut ids: Vec<i64> = chunk.iter().map(|o| o.0).collect();
                ids.sort_unstable();
                ids.dedup();
                chains.preload(conn, &ids)?;
                let (ph, padded) = placeholders(&ids);
                counted();
                let mut stmt = conn.prepare_cached(&format!(
                    "{ROW_SQL} WHERE e.object_id IN ({ph}) AND e.deleted_at IS NULL AND o.deleted_at IS NULL"
                ))?;
                let mut rows = stmt.query(params_from_iter(padded.iter()))?;
                let mut batch: Vec<ProjectionRow> = Vec::with_capacity(chunk.len());
                while let Some(r) = rows.next()? {
                    let object: i64 = r.get(1)?;
                    let parent: Option<i64> = r.get(2)?;
                    let source = SourceId(r.get(20)?);
                    // Containers are path nodes too, so anything but a plain
                    // file may be an ancestor worth caching.
                    let kind: String = r.get(5)?;
                    let is_node = ObjectKind::parse(&kind) != Some(ObjectKind::File);
                    if is_node {
                        chains.promote(object);
                    }
                    let path = chains.path(conn, source, object, is_node)?;
                    let ancestors = chains.ancestors(conn, parent)?;
                    batch.push(row_from(r, path, ancestors, Vec::new())?);
                }
                n += flush_batch(conn, &mut batch, &mut f)?;
            }
            Ok(n)
        })
    }

    /// Projection rows for one object (one per live entry). Empty when the
    /// object is deleted.
    pub fn projection_rows_for_object(&self, object: ObjectId) -> Result<Vec<ProjectionRow>> {
        let mut out = Vec::new();
        self.for_each_projection_row_of(&[object], |row| {
            out.push(row);
            Ok(())
        })?;
        Ok(out)
    }

    /// Object ids of every live descendant of a directory (not including it).
    pub fn descendant_object_ids(&self, dir: ObjectId) -> Result<Vec<ObjectId>> {
        self.with_reader(|conn| {
            counted();
            Ok(conn
                .prepare_cached(
                    "WITH RECURSIVE sub(object_id) AS (
                        SELECT object_id FROM entries WHERE parent_id = ?1 AND deleted_at IS NULL
                        UNION
                        SELECT e.object_id FROM entries e JOIN sub s ON e.parent_id = s.object_id WHERE e.deleted_at IS NULL
                     ) SELECT DISTINCT object_id FROM sub",
                )?
                .query_map(params![dir.0], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(ObjectId)
                .collect())
        })
    }

    // ----- projection bookkeeping -------------------------------------------

    pub fn projection_position(&self, name: &str) -> Result<i64> {
        self.with_reader(|conn| {
            Ok(conn
                .query_row(
                    "SELECT outbox_seq FROM projection_state WHERE name = ?1",
                    params![name],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0))
        })
    }

    pub fn set_projection_position(&self, name: &str, seq: i64) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO projection_state (name, outbox_seq, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET outbox_seq = excluded.outbox_seq, updated_at = excluded.updated_at",
                params![name, seq, UnixNanos::now().0],
            )?;
            Ok(())
        })
    }

    pub fn projection_source(
        &self,
        name: &str,
        source_id: SourceId,
    ) -> Result<Option<ProjectionSourceState>> {
        self.with_reader(|conn| {
            Ok(conn
                .query_row(
                    "SELECT generation, documents, built_at FROM projection_sources WHERE name = ?1 AND source_id = ?2",
                    params![name, source_id.0],
                    |r| {
                        Ok(ProjectionSourceState {
                            source_id,
                            generation: r.get(0)?,
                            documents: r.get::<_, i64>(1)? as u64,
                            built_at: UnixNanos(r.get(2)?),
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn set_projection_source(
        &self,
        name: &str,
        source_id: SourceId,
        generation: i64,
        documents: u64,
    ) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "INSERT INTO projection_sources (name, source_id, generation, documents, built_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(name, source_id) DO UPDATE SET generation = excluded.generation, documents = excluded.documents,
                    built_at = excluded.built_at",
                params![name, source_id.0, generation, documents as i64, UnixNanos::now().0],
            )?;
            Ok(())
        })
    }

    pub fn clear_projection_source(&self, name: &str, source_id: SourceId) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "DELETE FROM projection_sources WHERE name = ?1 AND source_id = ?2",
                params![name, source_id.0],
            )?;
            Ok(())
        })
    }
}

/// Pre-batching implementations, kept only so tests can prove the batched
/// path produces the same rows. Not used in production; a rebuild here costs
/// one query per directory and a per-object rebuild one query per ancestor,
/// which is exactly what the batched path removes.
#[doc(hidden)]
pub mod reference {
    use super::*;

    /// Directory-only path map: the shape the rebuild used before archive
    /// containers (files that own virtual members) became path nodes.
    struct RefDirs {
        parents: HashMap<i64, (Option<i64>, String)>,
        root_path: String,
        paths: HashMap<i64, (String, Vec<ObjectId>)>,
    }

    impl RefDirs {
        fn load(conn: &Connection, source_id: SourceId, root_path: &str) -> Result<Self> {
            let mut parents = HashMap::new();
            let mut stmt = conn.prepare_cached(
                "SELECT e.object_id, e.parent_id, e.name FROM entries e JOIN objects o ON o.object_id = e.object_id
                 WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL
                   AND o.kind IN ('directory', 'virtual_directory')",
            )?;
            let rows = stmt.query_map(params![source_id.0], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (obj, parent, name) = row?;
                parents.insert(obj, (parent, name));
            }
            Ok(Self {
                parents,
                root_path: root_path.trim_end_matches(['\\', '/']).to_string(),
                paths: HashMap::new(),
            })
        }

        fn dir(&mut self, obj: i64) -> (String, Vec<ObjectId>) {
            if let Some(p) = self.paths.get(&obj) {
                return p.clone();
            }
            let result = match self.parents.get(&obj).cloned() {
                None => (self.root_path.clone(), Vec::new()),
                Some((None, _)) => (root_display(&self.root_path), Vec::new()),
                Some((Some(parent), name)) => {
                    let (ppath, mut anc) = self.dir(parent);
                    anc.push(ObjectId(parent));
                    (format!("{}\\{}", ppath.trim_end_matches('\\'), name), anc)
                }
            };
            self.paths.insert(obj, result.clone());
            result
        }
    }

    fn desc_extensions(conn: &Connection, dir: i64) -> Result<Vec<String>> {
        counted();
        Ok(conn
            .prepare_cached(
                "SELECT extension FROM directory_extension_counts WHERE object_id = ?1",
            )?
            .query_map(params![dir], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The old per-object path and ancestor walk: one query per ancestor,
    /// plus the walk inside `render_path_conn`, which stays uninstrumented —
    /// so the counted total is a lower bound on what this path really costs.
    fn path_and_ancestors(
        conn: &Connection,
        object: ObjectId,
        parent: Option<i64>,
    ) -> Result<(String, Vec<ObjectId>)> {
        counted();
        let path = crate::read::render_path_conn(conn, object)?.unwrap_or_default();
        let mut ancestors = Vec::new();
        let mut cur = parent;
        let mut stmt = conn.prepare_cached(
            "SELECT parent_id FROM entries WHERE object_id = ?1 AND deleted_at IS NULL ORDER BY entry_id LIMIT 1",
        )?;
        let mut guard = 0;
        while let Some(p) = cur {
            ancestors.push(ObjectId(p));
            guard += 1;
            if guard > MAX_DEPTH {
                break;
            }
            counted();
            cur = stmt
                .query_row(params![p], |r| r.get::<_, Option<i64>>(0))
                .optional()?
                .flatten();
        }
        ancestors.reverse();
        Ok((path, ancestors))
    }

    impl Catalog {
        /// Stream rebuild rows the pre-batching way (one extension query per
        /// directory).
        pub fn reference_for_each_projection_row(
            &self,
            source_id: SourceId,
            mut f: impl FnMut(ProjectionRow) -> Result<()>,
        ) -> Result<u64> {
            self.with_reader(|conn| {
                counted();
                let src = get_source_conn(conn, source_id)?
                    .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
                let mut dirs = RefDirs::load(conn, source_id, &src.root_path)?;
                counted();
                let mut stmt = conn.prepare_cached(&format!(
                    "{ROW_SQL} WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL"
                ))?;
                let mut rows = stmt.query(params![source_id.0])?;
                let mut n = 0u64;
                while let Some(r) = rows.next()? {
                    let object_id: i64 = r.get(1)?;
                    let parent: Option<i64> = r.get(2)?;
                    let name: String = r.get(3)?;
                    let kind: String = r.get(5)?;
                    let is_dir =
                        ObjectKind::parse(&kind).is_some_and(ObjectKind::is_directory_like);
                    let (path, ancestors) = if is_dir {
                        dirs.dir(object_id)
                    } else {
                        match parent {
                            Some(p) => {
                                let (ppath, mut anc) = dirs.dir(p);
                                anc.push(ObjectId(p));
                                (format!("{}\\{}", ppath.trim_end_matches('\\'), name), anc)
                            }
                            None => (root_display(&dirs.root_path), Vec::new()),
                        }
                    };
                    let exts = if is_dir {
                        desc_extensions(conn, object_id)?
                    } else {
                        Vec::new()
                    };
                    f(row_from(r, path, ancestors, exts)?)?;
                    n += 1;
                }
                Ok(n)
            })
        }

        /// Per-object rows the pre-batching way (one query per ancestor).
        pub fn reference_projection_rows_for_object(
            &self,
            object: ObjectId,
        ) -> Result<Vec<ProjectionRow>> {
            self.with_reader(|conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "{ROW_SQL} WHERE e.object_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL"
                ))?;
                let mut rows = stmt.query(params![object.0])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    let parent: Option<i64> = r.get(2)?;
                    let kind: String = r.get(5)?;
                    let (path, ancestors) = path_and_ancestors(conn, object, parent)?;
                    let exts =
                        if ObjectKind::parse(&kind).is_some_and(ObjectKind::is_directory_like) {
                            desc_extensions(conn, object.0)?
                        } else {
                            Vec::new()
                        };
                    out.push(row_from(r, path, ancestors, exts)?);
                }
                Ok(out)
            })
        }
    }
}
