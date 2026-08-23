//! Directory aggregates (SPEC 7.4).
//!
//! `rebuild_source` computes every directory's subtree totals bottom-up from
//! the live entries of a source in one pass and writes them in the caller's
//! transaction. `apply_delta` propagates a change through the ancestor chain
//! for incremental updates; subtree moves subtract at the old parent chain
//! and add at the new one instead of recomputing descendants.

use crate::Result;
use eidos_domain::{ContentState, ObjectId, ObjectKind, SourceId, UnixNanos};
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acc {
    pub file_count: u64,
    pub dir_count: u64,
    pub logical: u64,
    pub allocated: u64,
    pub archive_declared: u64,
    pub archive_compressed: u64,
    pub newest: Option<i64>,
    pub oldest: Option<i64>,
    pub pending: u64,
    pub indexed: u64,
    pub failed: u64,
    pub excluded: u64,
    pub ext: HashMap<String, (u64, u64)>,
    pub complete: bool,
}

impl Default for Acc {
    /// A fresh accumulator is complete until an unlisted directory or an
    /// incomplete child says otherwise.
    fn default() -> Self {
        Self {
            file_count: 0,
            dir_count: 0,
            logical: 0,
            allocated: 0,
            archive_declared: 0,
            archive_compressed: 0,
            newest: None,
            oldest: None,
            pending: 0,
            indexed: 0,
            failed: 0,
            excluded: 0,
            ext: HashMap::new(),
            complete: true,
        }
    }
}

impl Acc {
    fn add_file(&mut self, ext: &str, size: u64, alloc: u64, mtime: Option<i64>, state: &str) {
        self.file_count += 1;
        self.logical += size;
        self.allocated += alloc;
        if let Some(m) = mtime {
            self.newest = Some(self.newest.map_or(m, |n| n.max(m)));
            self.oldest = Some(self.oldest.map_or(m, |o| o.min(m)));
        }
        match state {
            "pending" | "stale" => self.pending += 1,
            "indexed" | "partial" => self.indexed += 1,
            "failed" => self.failed += 1,
            "excluded" => self.excluded += 1,
            _ => {}
        }
        let e = self.ext.entry(ext.to_string()).or_insert((0, 0));
        e.0 += 1;
        e.1 += size;
    }

    fn add_virtual_file(&mut self, ext: &str, declared: u64, compressed: u64, mtime: Option<i64>) {
        self.file_count += 1;
        self.archive_declared += declared;
        self.archive_compressed += compressed;
        if let Some(m) = mtime {
            self.newest = Some(self.newest.map_or(m, |n| n.max(m)));
            self.oldest = Some(self.oldest.map_or(m, |o| o.min(m)));
        }
        // Counts participate in `has:` uniformly, but physical extension
        // bytes remain physical; declared archive bytes have their own field.
        self.ext.entry(ext.to_string()).or_insert((0, 0)).0 += 1;
    }

    fn merge_contents(&mut self, child: &Acc) {
        self.file_count += child.file_count;
        self.dir_count += child.dir_count;
        self.logical += child.logical;
        self.allocated += child.allocated;
        self.archive_declared += child.archive_declared;
        self.archive_compressed += child.archive_compressed;
        if let Some(m) = child.newest {
            self.newest = Some(self.newest.map_or(m, |n| n.max(m)));
        }
        if let Some(m) = child.oldest {
            self.oldest = Some(self.oldest.map_or(m, |o| o.min(m)));
        }
        self.pending += child.pending;
        self.indexed += child.indexed;
        self.failed += child.failed;
        self.excluded += child.excluded;
        self.complete &= child.complete;
        for (k, (c, b)) in &child.ext {
            let e = self.ext.entry(k.clone()).or_insert((0, 0));
            e.0 += c;
            e.1 += b;
        }
    }

    fn merge_child(&mut self, child: &Acc) {
        self.merge_contents(child);
        self.dir_count += 1;
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AggStats {
    pub directories: u64,
    pub extension_rows: u64,
    pub unreachable_directories: u64,
}

/// Recompute all aggregates of a source. Must be called inside a transaction.
pub fn rebuild_source(
    conn: &Connection,
    source_id: SourceId,
    root: ObjectId,
    generation: i64,
    unlisted: &HashSet<ObjectId>,
) -> Result<AggStats> {
    let mut children: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut own: HashMap<ObjectId, Acc> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT e.parent_id, e.object_id, e.extension, o.kind, o.size, o.allocated, o.modified, o.content_state
             FROM entries e JOIN objects o ON o.object_id = e.object_id
             WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL
               AND e.is_virtual = 0 AND e.parent_id IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![source_id.0])?;
        while let Some(r) = rows.next()? {
            let parent = ObjectId(r.get::<_, i64>(0)?);
            let obj = ObjectId(r.get::<_, i64>(1)?);
            let ext: String = r.get(2)?;
            let kind: String = r.get(3)?;
            if kind == ObjectKind::Directory.as_str() {
                children.entry(parent).or_default().push(obj);
            } else {
                let size = r.get::<_, i64>(4)? as u64;
                let alloc = r.get::<_, i64>(5)? as u64;
                let mtime: Option<i64> = r.get(6)?;
                let state: String = r.get(7)?;
                own.entry(parent)
                    .or_default()
                    .add_file(&ext, size, alloc, mtime, &state);
            }
        }
    }

    // BFS from the root to establish a parent-before-child order.
    let mut order: Vec<(ObjectId, Option<ObjectId>)> = vec![(root, None)];
    let mut i = 0;
    while i < order.len() {
        let (dir, _) = order[i];
        if let Some(kids) = children.get(&dir) {
            for k in kids {
                order.push((*k, Some(dir)));
            }
        }
        i += 1;
    }
    let reachable = order.len() as u64;
    let total_dirs = 1 + children.values().map(|v| v.len() as u64).sum::<u64>();

    conn.execute(
        "DELETE FROM directory_extension_counts WHERE object_id IN (SELECT object_id FROM directory_aggregates WHERE source_id = ?1)",
        params![source_id.0],
    )?;
    conn.execute(
        "DELETE FROM directory_aggregates WHERE source_id = ?1",
        params![source_id.0],
    )?;

    let mut ins_agg = conn.prepare_cached(
        "INSERT INTO directory_aggregates (object_id, source_id, file_count, dir_count, logical_bytes, allocated_bytes,
            archive_declared_bytes, archive_compressed_bytes, newest_modified, oldest_modified,
            content_pending, content_indexed, content_failed, content_excluded, generation, complete)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )?;
    let mut ins_ext = conn.prepare_cached(
        "INSERT INTO directory_extension_counts (object_id, extension, count, bytes) VALUES (?1, ?2, ?3, ?4)",
    )?;

    let mut acc: HashMap<ObjectId, Acc> = HashMap::new();
    let mut stats = AggStats {
        directories: 0,
        extension_rows: 0,
        unreachable_directories: total_dirs - reachable,
    };
    for (dir, parent) in order.iter().rev() {
        let mut a = acc.remove(dir).unwrap_or_default();
        if let Some(o) = own.remove(dir) {
            // Own files were accumulated separately from children merges.
            let mut merged = o;
            merged.merge_contents(&a);
            a = merged;
        }
        if unlisted.contains(dir) {
            a.complete = false;
        }
        ins_agg.execute(params![
            dir.0,
            source_id.0,
            a.file_count as i64,
            a.dir_count as i64,
            a.logical as i64,
            a.allocated as i64,
            a.archive_declared as i64,
            a.archive_compressed as i64,
            a.newest,
            a.oldest,
            a.pending as i64,
            a.indexed as i64,
            a.failed as i64,
            a.excluded as i64,
            generation,
            a.complete as i64,
        ])?;
        stats.directories += 1;
        for (ext, (c, b)) in &a.ext {
            ins_ext.execute(params![dir.0, ext, *c as i64, *b as i64])?;
            stats.extension_rows += 1;
        }
        if let Some(p) = parent {
            acc.entry(*p).or_default().merge_child(&a);
        }
    }
    drop(ins_agg);
    drop(ins_ext);

    // Virtual trees hang beneath file objects, so reduce each archive
    // separately and add its counts/declared bytes to every physical link of
    // the container. Physical logical/allocated bytes remain untouched.
    let containers: Vec<ObjectId> = conn
        .prepare_cached(
            "SELECT DISTINCT archive_container_id FROM objects
             WHERE source_id = ?1 AND archive_container_id IS NOT NULL AND deleted_at IS NULL",
        )?
        .query_map(params![source_id.0], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(ObjectId)
        .collect();
    for container in containers {
        let archive = rebuild_archive(conn, source_id, container, generation)?;
        apply_to_parents(conn, container, &archive.delta)?;
        stats.directories += archive.directories;
        stats.extension_rows += archive.extension_rows;
        conn.execute(
            "UPDATE archive_records SET aggregate_generation = generation
             WHERE object_id = ?1 AND generation = (
                 SELECT generation FROM objects WHERE object_id = ?1
             )",
            params![container.0],
        )?;
    }
    Ok(stats)
}

/// Signed change to propagate up from a directory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AggDelta {
    pub file_count: i64,
    pub dir_count: i64,
    pub logical: i64,
    pub allocated: i64,
    pub archive_declared: i64,
    pub archive_compressed: i64,
    pub pending: i64,
    pub indexed: i64,
    pub failed: i64,
    pub excluded: i64,
    /// `(extension, count delta, bytes delta)`
    pub ext: Vec<(String, i64, i64)>,
    /// Newest modified candidate (only raises `newest_modified`).
    pub touch_modified: Option<UnixNanos>,
}

impl AggDelta {
    pub fn for_file(ext: &str, size: u64, alloc: u64, state: ContentState, sign: i64) -> Self {
        let mut d = AggDelta {
            file_count: sign,
            logical: sign * size as i64,
            allocated: sign * alloc as i64,
            ext: vec![(ext.to_string(), sign, sign * size as i64)],
            ..Default::default()
        };
        match state {
            ContentState::Pending | ContentState::Stale => d.pending = sign,
            ContentState::Indexed | ContentState::Partial => d.indexed = sign,
            ContentState::Failed => d.failed = sign,
            ContentState::Excluded => d.excluded = sign,
            _ => {}
        }
        d
    }

    pub fn negate(&self) -> Self {
        AggDelta {
            file_count: -self.file_count,
            dir_count: -self.dir_count,
            logical: -self.logical,
            allocated: -self.allocated,
            archive_declared: -self.archive_declared,
            archive_compressed: -self.archive_compressed,
            pending: -self.pending,
            indexed: -self.indexed,
            failed: -self.failed,
            excluded: -self.excluded,
            ext: self
                .ext
                .iter()
                .map(|(e, c, b)| (e.clone(), -c, -b))
                .collect(),
            touch_modified: None,
        }
    }

    /// Build the delta that represents an entire subtree (for moves).
    pub fn from_subtree(conn: &Connection, dir: ObjectId) -> Result<Self> {
        let row = conn.query_row(
            "SELECT file_count, dir_count, logical_bytes, allocated_bytes,
                    archive_declared_bytes, archive_compressed_bytes,
                    content_pending, content_indexed, content_failed, content_excluded
             FROM directory_aggregates WHERE object_id = ?1",
            params![dir.0],
            |r| {
                Ok(AggDelta {
                    file_count: r.get(0)?,
                    dir_count: r.get::<_, i64>(1)? + 1,
                    logical: r.get(2)?,
                    allocated: r.get(3)?,
                    archive_declared: r.get(4)?,
                    archive_compressed: r.get(5)?,
                    pending: r.get(6)?,
                    indexed: r.get(7)?,
                    failed: r.get(8)?,
                    excluded: r.get(9)?,
                    ext: Vec::new(),
                    touch_modified: None,
                })
            },
        )?;
        let mut stmt = conn.prepare_cached(
            "SELECT extension, count, bytes FROM directory_extension_counts WHERE object_id = ?1",
        )?;
        let ext = stmt
            .query_map(params![dir.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(AggDelta { ext, ..row })
    }
}

#[derive(Debug, Default)]
pub struct ArchiveAggStats {
    pub delta: AggDelta,
    pub directories: u64,
    pub extension_rows: u64,
}

/// Current contribution of one live virtual tree to each physical directory
/// containing the archive. This is cheap enough to subtract before replacing
/// or retiring a manifest generation.
pub fn archive_delta(conn: &Connection, container: ObjectId) -> Result<AggDelta> {
    let (files, dirs, declared, compressed): (i64, i64, i64, i64) = conn.query_row(
        "SELECT
             COALESCE(SUM(o.kind = 'virtual_file'), 0),
             COALESCE(SUM(o.kind = 'virtual_directory'), 0),
             COALESCE(SUM(CASE WHEN o.kind = 'virtual_file' THEN o.size ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN o.kind = 'virtual_file' THEN am.compressed ELSE 0 END), 0)
         FROM objects o
         LEFT JOIN archive_members am
           ON am.object_id = o.archive_container_id
          AND am.generation = o.archive_generation
          AND am.ordinal = o.archive_member_ordinal
         WHERE o.archive_container_id = ?1 AND o.deleted_at IS NULL",
        params![container.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let ext = conn
        .prepare_cached(
            "SELECT e.extension, COUNT(*) FROM entries e
             JOIN objects o ON o.object_id = e.object_id
             WHERE o.archive_container_id = ?1 AND o.kind = 'virtual_file'
               AND o.deleted_at IS NULL AND e.deleted_at IS NULL
             GROUP BY e.extension",
        )?
        .query_map(params![container.0], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, 0))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(AggDelta {
        file_count: files,
        dir_count: dirs,
        archive_declared: declared,
        archive_compressed: compressed,
        ext,
        ..Default::default()
    })
}

/// Apply one archive contribution once for every live physical entry of its
/// container (hard links count once per entry, matching ordinary file sizes).
pub fn apply_to_parents(conn: &Connection, container: ObjectId, delta: &AggDelta) -> Result<u32> {
    let parents = conn
        .prepare_cached(
            "SELECT parent_id FROM entries
             WHERE object_id = ?1 AND is_virtual = 0 AND deleted_at IS NULL
               AND parent_id IS NOT NULL",
        )?
        .query_map(params![container.0], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut touched = 0;
    for parent in parents {
        touched += apply_delta(conn, ObjectId(parent), delta)?;
    }
    Ok(touched)
}

/// Rebuild aggregate rows inside one archive and return the whole virtual
/// tree's contribution. The container itself is a transparent topology edge:
/// it remains a physical file while its members reduce into its parent dirs.
pub fn rebuild_archive(
    conn: &Connection,
    source_id: SourceId,
    container: ObjectId,
    generation: i64,
) -> Result<ArchiveAggStats> {
    let mut children: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut parents: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut own: HashMap<ObjectId, Acc> = HashMap::new();
    let mut dirs = HashSet::new();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT e.parent_id, o.object_id, e.extension, o.kind, o.size, o.modified,
                    COALESCE(am.compressed, 0)
             FROM objects o JOIN entries e ON e.object_id = o.object_id
             LEFT JOIN archive_members am
               ON am.object_id = o.archive_container_id
              AND am.generation = o.archive_generation
              AND am.ordinal = o.archive_member_ordinal
             WHERE o.archive_container_id = ?1 AND o.deleted_at IS NULL
               AND e.deleted_at IS NULL",
        )?;
        let mut rows = stmt.query(params![container.0])?;
        while let Some(r) = rows.next()? {
            let parent = ObjectId(r.get::<_, i64>(0)?);
            let object = ObjectId(r.get::<_, i64>(1)?);
            let ext: String = r.get(2)?;
            let kind: String = r.get(3)?;
            if kind == ObjectKind::VirtualDirectory.as_str() {
                dirs.insert(object);
                parents.insert(object, parent);
                children.entry(parent).or_default().push(object);
            } else {
                own.entry(parent).or_default().add_virtual_file(
                    &ext,
                    r.get::<_, i64>(4)? as u64,
                    r.get::<_, i64>(6)? as u64,
                    r.get(5)?,
                );
            }
        }
    }

    let mut order = Vec::with_capacity(dirs.len());
    let mut queue: Vec<ObjectId> = dirs
        .iter()
        .filter(|d| parents.get(d).is_none_or(|p| !dirs.contains(p)))
        .copied()
        .collect();
    let mut i = 0;
    while i < queue.len() {
        let dir = queue[i];
        order.push(dir);
        if let Some(kids) = children.get(&dir) {
            queue.extend(kids);
        }
        i += 1;
    }
    if order.len() != dirs.len() {
        return Err(crate::CatalogError::InvalidState(format!(
            "archive {container} virtual directory graph is cyclic"
        )));
    }

    let mut acc: HashMap<ObjectId, Acc> = HashMap::new();
    let mut total = own.remove(&container).unwrap_or_default();
    let mut stats = ArchiveAggStats::default();
    let mut ins_agg = conn.prepare_cached(
        "INSERT OR REPLACE INTO directory_aggregates (object_id, source_id, file_count,
             dir_count, logical_bytes, allocated_bytes, archive_declared_bytes,
             archive_compressed_bytes, newest_modified, oldest_modified, content_pending,
             content_indexed, content_failed, content_excluded, generation, complete)
         VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6, ?7, ?8, 0, 0, 0, 0, ?9, 1)",
    )?;
    let mut ins_ext = conn.prepare_cached(
        "INSERT OR REPLACE INTO directory_extension_counts (object_id, extension, count, bytes)
         VALUES (?1, ?2, ?3, 0)",
    )?;
    for dir in order.into_iter().rev() {
        let mut a = own.remove(&dir).unwrap_or_default();
        if let Some(descendants) = acc.remove(&dir) {
            a.merge_contents(&descendants);
        }
        ins_agg.execute(params![
            dir.0,
            source_id.0,
            a.file_count as i64,
            a.dir_count as i64,
            a.archive_declared as i64,
            a.archive_compressed as i64,
            a.newest,
            a.oldest,
            generation,
        ])?;
        stats.directories += 1;
        for (ext, (count, _)) in &a.ext {
            ins_ext.execute(params![dir.0, ext, *count as i64])?;
            stats.extension_rows += 1;
        }
        match parents.get(&dir).copied() {
            Some(parent) if dirs.contains(&parent) => {
                acc.entry(parent).or_default().merge_child(&a);
            }
            _ => total.merge_child(&a),
        }
    }
    stats.delta = AggDelta {
        file_count: total.file_count as i64,
        dir_count: total.dir_count as i64,
        archive_declared: total.archive_declared as i64,
        archive_compressed: total.archive_compressed as i64,
        ext: total
            .ext
            .into_iter()
            .map(|(ext, (count, _))| (ext, count as i64, 0))
            .collect(),
        touch_modified: total.newest.map(UnixNanos),
        ..Default::default()
    };
    Ok(stats)
}

/// Apply `delta` to `dir` and every ancestor up to the source root. Must run
/// inside a transaction. Returns the number of directories touched.
pub fn apply_delta(conn: &Connection, dir: ObjectId, delta: &AggDelta) -> Result<u32> {
    let mut upd = conn.prepare_cached(
        "UPDATE directory_aggregates SET
            file_count = file_count + ?2, dir_count = dir_count + ?3,
            logical_bytes = logical_bytes + ?4, allocated_bytes = allocated_bytes + ?5,
            archive_declared_bytes = archive_declared_bytes + ?6,
            archive_compressed_bytes = archive_compressed_bytes + ?7,
            content_pending = content_pending + ?8, content_indexed = content_indexed + ?9,
            content_failed = content_failed + ?10, content_excluded = content_excluded + ?11,
            newest_modified = CASE WHEN ?12 IS NOT NULL AND (newest_modified IS NULL OR ?12 > newest_modified) THEN ?12 ELSE newest_modified END
         WHERE object_id = ?1",
    )?;
    let mut upd_ext = conn.prepare_cached(
        "INSERT INTO directory_extension_counts (object_id, extension, count, bytes) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(object_id, extension) DO UPDATE SET count = count + excluded.count, bytes = bytes + excluded.bytes",
    )?;
    let mut del_ext = conn
        .prepare_cached("DELETE FROM directory_extension_counts WHERE object_id = ?1 AND extension = ?2 AND count <= 0")?;
    let mut parent_of = conn.prepare_cached(
        "SELECT parent_id FROM entries WHERE object_id = ?1 AND deleted_at IS NULL ORDER BY entry_id LIMIT 1",
    )?;
    let mut current = Some(dir);
    let mut touched = 0u32;
    let mut guard = 0;
    while let Some(d) = current {
        guard += 1;
        if guard > 1024 {
            return Err(crate::CatalogError::InvalidState(
                "ancestor chain too deep".into(),
            ));
        }
        let n = upd.execute(params![
            d.0,
            delta.file_count,
            delta.dir_count,
            delta.logical,
            delta.allocated,
            delta.archive_declared,
            delta.archive_compressed,
            delta.pending,
            delta.indexed,
            delta.failed,
            delta.excluded,
            delta.touch_modified.map(|t| t.0),
        ])?;
        if n > 0 {
            touched += 1;
            for (ext, c, b) in &delta.ext {
                upd_ext.execute(params![d.0, ext, c, b])?;
                if *c < 0 {
                    del_ext.execute(params![d.0, ext])?;
                }
            }
        }
        current = parent_of
            .query_row(params![d.0], |r| r.get::<_, Option<i64>>(0))
            .ok()
            .flatten()
            .map(ObjectId);
    }
    Ok(touched)
}
