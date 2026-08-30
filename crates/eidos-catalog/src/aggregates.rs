//! Directory aggregates (SPEC 7.4).
//!
//! `rebuild_source` computes every directory's subtree totals bottom-up from
//! the live entries of a source in one pass and writes them in the caller's
//! transaction. `apply_delta` propagates a change through the ancestor chain
//! for incremental updates; subtree moves subtract at the old parent chain
//! and add at the new one instead of recomputing descendants.
//!
//! Both definitions of `newest_modified`/`oldest_modified` must agree: they
//! are the extrema of `objects.modified` over the **non-directory** live
//! entries of the subtree. A directory's own `modified` never contributes
//! (its aggregate row carries its children's extrema instead), and virtual
//! archive members are not part of any physical directory's subtree because
//! they hang off the container object, which has no aggregate row.
//!
//! Counters are monoids, so `apply_delta` can add signed values blindly.
//! Extrema are not: removing the entry that provided a directory's extremum
//! makes the stored value unknown, so `apply_delta` recomputes that one
//! directory from its direct children (one query) and keeps walking up only
//! while an ancestor's extremum is likewise invalidated.

use crate::Result;
use eidos_domain::{ContentState, ObjectId, ObjectKind, SourceId, UnixNanos};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acc {
    pub file_count: u64,
    pub dir_count: u64,
    pub logical: u64,
    pub allocated: u64,
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

    fn merge_child(&mut self, child: &Acc) {
        self.file_count += child.file_count;
        self.dir_count += child.dir_count + 1;
        self.logical += child.logical;
        self.allocated += child.allocated;
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
}

#[derive(Debug, Default, Clone, serde::Serialize, TS)]
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
    let mut directories = HashSet::from([root]);
    {
        let mut stmt = conn.prepare_cached(
            "SELECT e.parent_id, e.object_id, e.extension, o.kind, o.size, o.allocated, o.modified, o.content_state
             FROM entries e JOIN objects o ON o.object_id = e.object_id
             WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL AND e.parent_id IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![source_id.0])?;
        while let Some(r) = rows.next()? {
            let parent = ObjectId(r.get::<_, i64>(0)?);
            let obj = ObjectId(r.get::<_, i64>(1)?);
            let ext: String = r.get(2)?;
            let kind: String = r.get(3)?;
            if kind == ObjectKind::Directory.as_str() {
                directories.insert(obj);
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
    let mut seen = HashSet::from([root]);
    let mut i = 0;
    while i < order.len() {
        let (dir, _) = order[i];
        if let Some(kids) = children.get(&dir) {
            for k in kids {
                // A catalog should be acyclic, but rebuild is also used to
                // recover old/corrupt catalogs and must remain bounded when
                // topology is malformed.
                if seen.insert(*k) {
                    order.push((*k, Some(dir)));
                }
            }
        }
        i += 1;
    }
    let reachable = order.len() as u64;
    let total_dirs = directories.len() as u64;

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
            newest_modified, oldest_modified, content_pending, content_indexed, content_failed, content_excluded, generation, complete)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            merged.dir_count += a.dir_count;
            merged.file_count += a.file_count;
            merged.logical += a.logical;
            merged.allocated += a.allocated;
            merged.pending += a.pending;
            merged.indexed += a.indexed;
            merged.failed += a.failed;
            merged.excluded += a.excluded;
            merged.complete &= a.complete;
            if let Some(m) = a.newest {
                merged.newest = Some(merged.newest.map_or(m, |n| n.max(m)));
            }
            if let Some(m) = a.oldest {
                merged.oldest = Some(merged.oldest.map_or(m, |o| o.min(m)));
            }
            for (k, (c, b)) in a.ext.drain() {
                let e = merged.ext.entry(k).or_insert((0, 0));
                e.0 += c;
                e.1 += b;
            }
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
    Ok(stats)
}

/// Modification-time extrema of whatever is entering or leaving a subtree.
/// `None` means "contributes no timestamp" (an empty subtree, or a file
/// whose `modified` is unknown), which never invalidates anything.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Extrema {
    pub newest: Option<i64>,
    pub oldest: Option<i64>,
}

impl Extrema {
    /// The extrema of a single timestamp.
    fn point(at: Option<i64>) -> Self {
        Extrema {
            newest: at,
            oldest: at,
        }
    }
}

/// Signed change to propagate up from a directory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AggDelta {
    pub file_count: i64,
    pub dir_count: i64,
    pub logical: i64,
    pub allocated: i64,
    pub pending: i64,
    pub indexed: i64,
    pub failed: i64,
    pub excluded: i64,
    /// `(extension, count delta, bytes delta)`
    pub ext: Vec<(String, i64, i64)>,
    /// Timestamps this change adds to the subtree.
    pub added: Extrema,
    /// Timestamps this change removes from the subtree. When one of them is
    /// the directory's current extremum, that extremum is recomputed.
    pub removed: Extrema,
}

impl AggDelta {
    pub fn for_file(
        ext: &str,
        size: u64,
        alloc: u64,
        modified: Option<UnixNanos>,
        state: ContentState,
        sign: i64,
    ) -> Self {
        let stamp = Extrema::point(modified.map(|t| t.0));
        let (added, removed) = if sign >= 0 {
            (stamp, Extrema::default())
        } else {
            (Extrema::default(), stamp)
        };
        let mut d = AggDelta {
            file_count: sign,
            logical: sign * size as i64,
            allocated: sign * alloc as i64,
            ext: vec![(ext.to_string(), sign, sign * size as i64)],
            added,
            removed,
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
            pending: -self.pending,
            indexed: -self.indexed,
            failed: -self.failed,
            excluded: -self.excluded,
            ext: self
                .ext
                .iter()
                .map(|(e, c, b)| (e.clone(), -c, -b))
                .collect(),
            added: self.removed,
            removed: self.added,
        }
    }

    /// Build the delta that represents an entire subtree (for moves).
    pub fn from_subtree(conn: &Connection, dir: ObjectId) -> Result<Self> {
        let row = conn.query_row(
            "SELECT file_count, dir_count, logical_bytes, allocated_bytes, content_pending, content_indexed, content_failed, content_excluded,
                    newest_modified, oldest_modified
             FROM directory_aggregates WHERE object_id = ?1",
            params![dir.0],
            |r| {
                Ok(AggDelta {
                    file_count: r.get(0)?,
                    dir_count: r.get::<_, i64>(1)? + 1,
                    logical: r.get(2)?,
                    allocated: r.get(3)?,
                    pending: r.get(4)?,
                    indexed: r.get(5)?,
                    failed: r.get(6)?,
                    excluded: r.get(7)?,
                    ext: Vec::new(),
                    added: Extrema {
                        newest: r.get(8)?,
                        oldest: r.get(9)?,
                    },
                    removed: Extrema::default(),
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

/// Extrema of a directory's direct children, using the same definition as
/// `rebuild_source`: child directories contribute their aggregate extrema,
/// everything else (files, reparse points, archive containers) contributes
/// its own `modified`.
const CHILD_EXTREMA_SQL: &str = "SELECT
        MAX(CASE WHEN o.kind = 'directory' THEN a.newest_modified ELSE o.modified END),
        MIN(CASE WHEN o.kind = 'directory' THEN a.oldest_modified ELSE o.modified END)
     FROM entries e
     JOIN objects o ON o.object_id = e.object_id
     LEFT JOIN directory_aggregates a ON a.object_id = e.object_id
     WHERE e.parent_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL";

/// The directory's newest value after `removed` left and `added` arrived, or
/// `None` when the removal took the current maximum away and nothing added
/// replaces it — the caller must then recompute from the direct children.
fn next_newest(cur: Option<i64>, removed: Option<i64>, added: Option<i64>) -> Option<Option<i64>> {
    if let Some(r) = removed {
        let replaced = matches!(added, Some(a) if a >= r);
        if !replaced && !matches!(cur, Some(c) if c > r) {
            return None;
        }
    }
    Some(match (cur, added) {
        (Some(c), Some(a)) => Some(c.max(a)),
        (c, None) => c,
        (None, a) => a,
    })
}

/// Mirror of [`next_newest`] for the minimum.
fn next_oldest(cur: Option<i64>, removed: Option<i64>, added: Option<i64>) -> Option<Option<i64>> {
    if let Some(r) = removed {
        let replaced = matches!(added, Some(a) if a <= r);
        if !replaced && !matches!(cur, Some(c) if c < r) {
            return None;
        }
    }
    Some(match (cur, added) {
        (Some(c), Some(a)) => Some(c.min(a)),
        (c, None) => c,
        (None, a) => a,
    })
}

/// Move one directory's stored extrema to what `added`/`removed` imply,
/// recomputing from its direct children when a removal invalidated them.
/// Returns `(before, after)`: the change the parent now sees in this
/// directory, which is inert when the two are equal.
fn step_extrema(
    conn: &Connection,
    dir: ObjectId,
    added: Extrema,
    removed: Extrema,
) -> Result<(Extrema, Extrema)> {
    let cur = conn
        .prepare_cached(
            "SELECT newest_modified, oldest_modified FROM directory_aggregates WHERE object_id = ?1",
        )?
        .query_row(params![dir.0], |r| {
            Ok(Extrema {
                newest: r.get(0)?,
                oldest: r.get(1)?,
            })
        })
        .optional()?
        .unwrap_or_default();
    let newest = next_newest(cur.newest, removed.newest, added.newest);
    let oldest = next_oldest(cur.oldest, removed.oldest, added.oldest);
    let next = match (newest, oldest) {
        (Some(newest), Some(oldest)) => Extrema { newest, oldest },
        // One extremum lost its provider: both are cheap to re-derive from
        // the children we would have to visit anyway.
        _ => conn
            .prepare_cached(CHILD_EXTREMA_SQL)?
            .query_row(params![dir.0], |r| {
                Ok(Extrema {
                    newest: r.get(0)?,
                    oldest: r.get(1)?,
                })
            })?,
    };
    if next != cur {
        conn.prepare_cached(
            "UPDATE directory_aggregates SET newest_modified = ?2, oldest_modified = ?3 WHERE object_id = ?1",
        )?
        .execute(params![dir.0, next.newest, next.oldest])?;
    }
    Ok((cur, next))
}

/// Apply `delta` to `dir` and every ancestor up to the source root. Must run
/// inside a transaction. Returns the number of directories touched.
pub fn apply_delta(conn: &Connection, dir: ObjectId, delta: &AggDelta) -> Result<u32> {
    let mut upd = conn.prepare_cached(
        "UPDATE directory_aggregates SET
            file_count = file_count + ?2, dir_count = dir_count + ?3,
            logical_bytes = logical_bytes + ?4, allocated_bytes = allocated_bytes + ?5,
            content_pending = content_pending + ?6, content_indexed = content_indexed + ?7,
            content_failed = content_failed + ?8, content_excluded = content_excluded + ?9
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
    let mut added = delta.added;
    let mut removed = delta.removed;
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
            delta.pending,
            delta.indexed,
            delta.failed,
            delta.excluded,
        ])?;
        if n > 0 {
            touched += 1;
            for (ext, c, b) in &delta.ext {
                upd_ext.execute(params![d.0, ext, c, b])?;
                if *c < 0 {
                    del_ext.execute(params![d.0, ext])?;
                }
            }
            if added != removed {
                // The parent sees this directory swap its old extrema for
                // its new ones; equal values stop the walk short.
                (removed, added) = step_extrema(conn, d, added, removed)?;
            }
        } else {
            // No aggregate row: not a tracked directory (an archive
            // container's virtual subtree, say), so nothing above it changes.
            added = Extrema::default();
            removed = Extrema::default();
        }
        current = parent_of
            .query_row(params![d.0], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten()
            .map(ObjectId);
    }
    Ok(touched)
}
