//! Rows for derived-index projections and projection bookkeeping.
//!
//! A projection (e.g. the Tantivy catalog index) is rebuilt per source from
//! the published generation and then follows the outbox. The catalog owns
//! the follower position (`projection_state`) and the per-source built
//! generation (`projection_sources`) so restarts resume exactly and the API
//! can report when the search index lags the catalog.

use crate::read::get_source_conn;
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{ContentState, FileAttributes, ObjectId, ObjectKind, SourceId, UnixNanos};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
        a.file_count, a.dir_count, a.logical_bytes, a.allocated_bytes, a.newest_modified, a.complete
     FROM entries e JOIN objects o ON o.object_id = e.object_id
     LEFT JOIN directory_aggregates a ON a.object_id = o.object_id";

struct DirMap {
    /// object_id -> (parent_id, name)
    parents: HashMap<i64, (Option<i64>, String)>,
    root_path: String,
    paths: HashMap<i64, (String, Vec<ObjectId>)>,
}

impl DirMap {
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
    source_id: SourceId,
    path: String,
    ancestors: Vec<ObjectId>,
    desc_extensions: Vec<String>,
) -> rusqlite::Result<ProjectionRow> {
    let kind_s: String = r.get(5)?;
    let state_s: String = r.get(11)?;
    Ok(ProjectionRow {
        entry_id: r.get(0)?,
        object_id: ObjectId(r.get(1)?),
        source_id,
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

fn desc_extensions(conn: &Connection, dir: i64) -> Result<Vec<String>> {
    Ok(conn
        .prepare_cached("SELECT extension FROM directory_extension_counts WHERE object_id = ?1")?
        .query_map(params![dir], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

impl Catalog {
    /// Stream every live entry of a source (including the root entry) to `f`.
    pub fn for_each_projection_row(
        &self,
        source_id: SourceId,
        mut f: impl FnMut(ProjectionRow) -> Result<()>,
    ) -> Result<u64> {
        self.with_reader(|conn| {
            let src = get_source_conn(conn, source_id)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
            let mut dirs = DirMap::load(conn, source_id, &src.root_path)?;
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
                let exts = if is_dir {
                    desc_extensions(conn, object_id)?
                } else {
                    Vec::new()
                };
                f(row_from(r, source_id, path, ancestors, exts)?)?;
                n += 1;
            }
            Ok(n)
        })
    }

    /// Projection rows for one object (one per live entry). Empty when the
    /// object is deleted.
    pub fn projection_rows_for_object(&self, object: ObjectId) -> Result<Vec<ProjectionRow>> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "{ROW_SQL} WHERE e.object_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL"
            ))?;
            let mut rows = stmt.query(params![object.0])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                let source_id: i64 = conn.query_row(
                    "SELECT source_id FROM entries WHERE entry_id = ?1",
                    params![r.get::<_, i64>(0)?],
                    |x| x.get(0),
                )?;
                let parent: Option<i64> = r.get(2)?;
                let kind: String = r.get(5)?;
                let (path, ancestors) = path_and_ancestors(conn, object, parent)?;
                let exts = if ObjectKind::parse(&kind).is_some_and(ObjectKind::is_directory_like) {
                    desc_extensions(conn, object.0)?
                } else {
                    Vec::new()
                };
                out.push(row_from(r, SourceId(source_id), path, ancestors, exts)?);
            }
            Ok(out)
        })
    }

    /// Object ids of every live descendant of a directory (not including it).
    pub fn descendant_object_ids(&self, dir: ObjectId) -> Result<Vec<ObjectId>> {
        self.with_reader(|conn| {
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

fn path_and_ancestors(
    conn: &Connection,
    object: ObjectId,
    parent: Option<i64>,
) -> Result<(String, Vec<ObjectId>)> {
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
        if guard > 512 {
            break;
        }
        cur = stmt
            .query_row(params![p], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten();
    }
    ancestors.reverse();
    Ok((path, ancestors))
}
