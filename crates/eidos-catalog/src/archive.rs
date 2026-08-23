//! Archive manifests (ADR-0010): one record per container object and its
//! virtual members as read from the container's own directory, never from
//! member data. Stored alongside content records so the content job queue,
//! budgets, and activity views carry archives without a second pipeline.

use crate::content::{flip_state, upsert_content_record, ContentRecord};
use crate::jobs::{enqueue_conn, outbox_append_conn, NewJob};
use crate::{Catalog, Result};
use eidos_domain::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveRecord {
    pub object_id: ObjectId,
    pub source_id: SourceId,
    pub generation: u32,
    pub format: String,
    /// Explicit members (files and directory entries).
    pub member_count: u64,
    /// Directories, explicit and implicit.
    pub dir_count: u64,
    pub implicit_dir_count: u64,
    pub suspicious_count: u64,
    pub declared_size: u64,
    pub compressed_size: u64,
    pub claimed_entries: u64,
    pub zip64: bool,
    pub truncated: bool,
    pub comment: Option<String>,
    /// `Indexed`, `Partial` (truncated by a budget), or `Failed`.
    pub state: ContentState,
    pub error: Option<String>,
    pub reason: Option<String>,
    pub processed_at: UnixNanos,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveMember {
    pub ordinal: u32,
    pub path: String,
    pub name: String,
    pub parent: String,
    pub raw_name: String,
    pub is_dir: bool,
    pub implicit: bool,
    pub size: u64,
    pub compressed: u64,
    pub method: u16,
    pub crc32: u32,
    pub modified: Option<UnixNanos>,
    pub encrypted: bool,
    pub flags: u32,
}

/// Member listing: children of one virtual directory (`parent`) or every
/// member under a path prefix, paged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberQuery {
    pub parent: Option<String>,
    pub prefix: Option<String>,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchiveStats {
    pub archives: u64,
    pub members: u64,
    pub declared_size: u64,
    pub truncated: u64,
    pub failed: u64,
}

fn member_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveMember> {
    Ok(ArchiveMember {
        ordinal: r.get::<_, i64>(0)? as u32,
        path: r.get(1)?,
        name: r.get(2)?,
        parent: r.get(3)?,
        raw_name: r.get(4)?,
        is_dir: r.get::<_, i64>(5)? != 0,
        implicit: r.get::<_, i64>(6)? != 0,
        size: r.get::<_, i64>(7)? as u64,
        compressed: r.get::<_, i64>(8)? as u64,
        method: r.get::<_, i64>(9)? as u16,
        crc32: r.get::<_, i64>(10)? as u32,
        modified: r.get::<_, Option<i64>>(11)?.map(UnixNanos),
        encrypted: r.get::<_, i64>(12)? != 0,
        flags: r.get::<_, i64>(13)? as u32,
    })
}

const MEMBER_COLUMNS: &str =
    "ordinal, path, name, parent, raw_name, is_dir, implicit, size, compressed, method, crc32, modified, encrypted, flags";

/// Release SQLite's single writer between bounded manifest batches.
const ARCHIVE_MEMBER_BATCH: usize = 1_024;
const ARCHIVE_REQUEUE_BATCH: usize = 256;

fn archive_generation_is_current(conn: &Connection, rec: &ArchiveRecord) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM objects
             WHERE object_id = ?1 AND source_id = ?2 AND generation = ?3
               AND deleted_at IS NULL AND kind = 'file'",
            params![rec.object_id.0, rec.source_id.0, rec.generation as i64],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn archive_generation_is_published(conn: &Connection, rec: &ArchiveRecord) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(aggregate_generation = generation, 0)
               AND (member_count + implicit_dir_count) = (
                 SELECT COUNT(*) FROM objects v
                 WHERE v.archive_container_id = archive_records.object_id
                   AND v.archive_generation = archive_records.generation
                   AND v.deleted_at IS NULL
             )
             FROM archive_records WHERE object_id = ?1 AND generation = ?2",
            params![rec.object_id.0, rec.generation as i64],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}

fn insert_archive_members(
    conn: &Connection,
    rec: &ArchiveRecord,
    members: &[ArchiveMember],
) -> Result<()> {
    let mut stmt = conn.prepare_cached(&format!(
        "INSERT OR REPLACE INTO archive_members (object_id, generation, {MEMBER_COLUMNS})
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
    ))?;
    for m in members {
        stmt.execute(params![
            rec.object_id.0,
            rec.generation as i64,
            m.ordinal as i64,
            m.path,
            m.name,
            m.parent,
            m.raw_name,
            m.is_dir as i64,
            m.implicit as i64,
            m.size as i64,
            m.compressed as i64,
            m.method as i64,
            m.crc32 as i64,
            m.modified.map(|t| t.0),
            m.encrypted as i64,
            m.flags as i64,
        ])?;
    }
    Ok(())
}

fn virtual_member_count(rec: &ArchiveRecord) -> Result<u32> {
    u32::try_from(
        rec.member_count
            .checked_add(rec.implicit_dir_count)
            .ok_or_else(|| {
                crate::CatalogError::InvalidState("archive virtual-member count overflow".into())
            })?,
    )
    .map_err(|_| {
        crate::CatalogError::InvalidState("archive has more than u32::MAX virtual members".into())
    })
}

fn member_depth(member: &ArchiveMember) -> usize {
    member.path.bytes().filter(|b| *b == b'/').count()
}

/// Tombstone every materialized member owned by a container. Callers decide
/// whether a source-generation rebuild or a subtree outbox row publishes the
/// change. Archive records remain as generation-bound processing history.
pub(crate) fn retire_virtual_tree(
    conn: &Connection,
    container: ObjectId,
    now: i64,
) -> Result<(u64, u64)> {
    let accounted = conn
        .query_row(
            "SELECT COALESCE(aggregate_generation = generation, 0) FROM archive_records
             WHERE object_id = ?1",
            params![container.0],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    if accounted {
        let delta = crate::aggregates::archive_delta(conn, container)?.negate();
        crate::aggregates::apply_to_parents(conn, container, &delta)?;
    }
    let entries = conn.execute(
        "UPDATE entries SET deleted_at = ?2
         WHERE is_virtual = 1 AND deleted_at IS NULL AND object_id IN (
             SELECT object_id FROM objects WHERE archive_container_id = ?1
         )",
        params![container.0, now],
    )? as u64;
    conn.execute(
        "DELETE FROM directory_extension_counts WHERE object_id IN (
             SELECT object_id FROM objects WHERE archive_container_id = ?1
         )",
        params![container.0],
    )?;
    conn.execute(
        "DELETE FROM directory_aggregates WHERE object_id IN (
             SELECT object_id FROM objects WHERE archive_container_id = ?1
         )",
        params![container.0],
    )?;
    let objects = conn.execute(
        "UPDATE objects SET deleted_at = ?2
         WHERE archive_container_id = ?1 AND deleted_at IS NULL",
        params![container.0, now],
    )? as u64;
    conn.execute(
        "UPDATE archive_records SET aggregate_generation = NULL WHERE object_id = ?1",
        params![container.0],
    )?;
    Ok((entries, objects))
}

/// Make the staged generation live and retire the previous virtual tree.
/// The single subtree outbox row lets projections replace old object ids by
/// ancestry instead of emitting one row per archive member.
fn publish_virtual_tree(conn: &Connection, rec: &ArchiveRecord) -> Result<()> {
    let now = UnixNanos::now().0;
    retire_virtual_tree(conn, rec.object_id, now)?;
    conn.execute(
        "UPDATE objects SET deleted_at = NULL
         WHERE archive_container_id = ?1 AND archive_generation = ?2",
        params![rec.object_id.0, rec.generation as i64],
    )?;
    conn.execute(
        "UPDATE entries SET deleted_at = NULL WHERE is_virtual = 1 AND object_id IN (
             SELECT object_id FROM objects
             WHERE archive_container_id = ?1 AND archive_generation = ?2
         )",
        params![rec.object_id.0, rec.generation as i64],
    )?;
    let source_generation: i64 = conn.query_row(
        "SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?1",
        params![rec.source_id.0],
        |r| r.get(0),
    )?;
    let archive =
        crate::aggregates::rebuild_archive(conn, rec.source_id, rec.object_id, source_generation)?;
    crate::aggregates::apply_to_parents(conn, rec.object_id, &archive.delta)?;
    outbox_append_conn(
        conn,
        rec.source_id,
        rec.object_id,
        "subtree",
        rec.generation as i64,
    )?;
    Ok(())
}

/// SQL list of archive extensions for `lower(extension) IN (...)`.
pub(crate) fn archive_extension_list() -> String {
    eidos_domain::archive::ZIP_EXTENSIONS
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(",")
}

fn upsert_archive_record(tx: &rusqlite::Connection, rec: &ArchiveRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO archive_records (object_id, source_id, generation, format, member_count, dir_count,
            implicit_dir_count, suspicious_count, declared_size, compressed_size, claimed_entries, zip64,
            truncated, comment, state, error, reason, processed_at, elapsed_ms, aggregate_generation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?3)
         ON CONFLICT(object_id) DO UPDATE SET source_id = excluded.source_id, generation = excluded.generation,
            format = excluded.format, member_count = excluded.member_count, dir_count = excluded.dir_count,
            implicit_dir_count = excluded.implicit_dir_count, suspicious_count = excluded.suspicious_count,
            declared_size = excluded.declared_size, compressed_size = excluded.compressed_size,
            claimed_entries = excluded.claimed_entries, zip64 = excluded.zip64, truncated = excluded.truncated,
            comment = excluded.comment, state = excluded.state, error = excluded.error, reason = excluded.reason,
            processed_at = excluded.processed_at, elapsed_ms = excluded.elapsed_ms,
            aggregate_generation = excluded.aggregate_generation",
        params![
            rec.object_id.0,
            rec.source_id.0,
            rec.generation as i64,
            rec.format,
            rec.member_count as i64,
            rec.dir_count as i64,
            rec.implicit_dir_count as i64,
            rec.suspicious_count as i64,
            rec.declared_size as i64,
            rec.compressed_size as i64,
            rec.claimed_entries as i64,
            rec.zip64 as i64,
            rec.truncated as i64,
            rec.comment,
            rec.state.as_str(),
            rec.error,
            rec.reason,
            rec.processed_at.0,
            rec.elapsed_ms,
        ],
    )?;
    Ok(())
}

impl Catalog {
    /// Stage real object/entry rows for a manifest without exposing them to
    /// readers. Directories are ordered before descendants; duplicate paths
    /// remain distinct entries, with the first directory owning children.
    fn stage_virtual_tree(&self, rec: &ArchiveRecord, members: &[ArchiveMember]) -> Result<bool> {
        let mut ordered: Vec<&ArchiveMember> = members.iter().collect();
        ordered.sort_by_key(|m| (member_depth(m), !m.is_dir, m.ordinal));
        let mut directories: HashMap<&str, ObjectId> = HashMap::new();

        for batch in ordered.chunks(ARCHIVE_MEMBER_BATCH) {
            let stored = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if !archive_generation_is_current(&tx, rec)? {
                    return Ok(false);
                }
                let scan_generation: i64 = tx.query_row(
                    "SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?1",
                    params![rec.source_id.0],
                    |r| r.get(0),
                )?;
                for member in batch {
                    let parent = if member.parent.is_empty() {
                        rec.object_id
                    } else {
                        directories
                            .get(member.parent.as_str())
                            .copied()
                            .ok_or_else(|| {
                                crate::CatalogError::InvalidState(format!(
                                    "archive member {} has no directory row for parent {:?}",
                                    member.ordinal, member.parent
                                ))
                            })?
                    };
                    let existing = tx
                        .query_row(
                            "SELECT object_id FROM objects
                             WHERE archive_container_id = ?1 AND archive_generation = ?2
                               AND archive_member_ordinal = ?3",
                            params![
                                rec.object_id.0,
                                rec.generation as i64,
                                member.ordinal as i64
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?;
                    let kind = if member.is_dir {
                        ObjectKind::VirtualDirectory
                    } else {
                        ObjectKind::VirtualFile
                    };
                    let attributes = if member.is_dir {
                        FileAttributes::DIRECTORY
                    } else {
                        0
                    };
                    let object = match existing {
                        Some(id) => {
                            tx.execute(
                                "UPDATE objects SET source_id = ?2, kind = ?3, generation = ?4,
                                     size = ?5, allocated = 0, attributes = ?6, modified = ?7,
                                     content_state = 'not_applicable', last_seen_generation = ?8,
                                     deleted_at = ?9 WHERE object_id = ?1",
                                params![
                                    id,
                                    rec.source_id.0,
                                    kind.as_str(),
                                    rec.generation as i64,
                                    member.size as i64,
                                    attributes as i64,
                                    member.modified.map(|t| t.0),
                                    scan_generation,
                                    rec.processed_at.0,
                                ],
                            )?;
                            ObjectId(id)
                        }
                        None => {
                            tx.execute(
                                "INSERT INTO objects (source_id, kind, identity_confidence,
                                     generation, size, allocated, attributes, modified, link_count,
                                     content_state, first_seen_generation, last_seen_generation,
                                     deleted_at, archive_container_id, archive_generation,
                                     archive_member_ordinal)
                                 VALUES (?1, ?2, 'path_derived', ?3, ?4, 0, ?5, ?6, 1,
                                     'not_applicable', ?7, ?7, ?8, ?9, ?3, ?10)",
                                params![
                                    rec.source_id.0,
                                    kind.as_str(),
                                    rec.generation as i64,
                                    member.size as i64,
                                    attributes as i64,
                                    member.modified.map(|t| t.0),
                                    scan_generation,
                                    rec.processed_at.0,
                                    rec.object_id.0,
                                    member.ordinal as i64,
                                ],
                            )?;
                            ObjectId(tx.last_insert_rowid())
                        }
                    };
                    let extension = if member.is_dir {
                        String::new()
                    } else {
                        extension_of(&member.name)
                    };
                    let entry = tx
                        .query_row(
                            "SELECT entry_id FROM entries WHERE object_id = ?1 AND is_virtual = 1",
                            params![object.0],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?;
                    match entry {
                        Some(entry) => {
                            tx.execute(
                                "UPDATE entries SET source_id = ?2, parent_id = ?3, name = ?4,
                                     name_folded = ?5, extension = ?6, last_seen_generation = ?7,
                                     deleted_at = ?8 WHERE entry_id = ?1",
                                params![
                                    entry,
                                    rec.source_id.0,
                                    parent.0,
                                    member.name,
                                    crate::policy::fold(&member.name),
                                    extension,
                                    scan_generation,
                                    rec.processed_at.0,
                                ],
                            )?;
                        }
                        None => {
                            tx.execute(
                                "INSERT INTO entries (source_id, parent_id, object_id, name,
                                     name_folded, extension, is_virtual, first_seen_generation,
                                     last_seen_generation, deleted_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7, ?8)",
                                params![
                                    rec.source_id.0,
                                    parent.0,
                                    object.0,
                                    member.name,
                                    crate::policy::fold(&member.name),
                                    extension,
                                    scan_generation,
                                    rec.processed_at.0,
                                ],
                            )?;
                        }
                    }
                    if member.is_dir {
                        directories.entry(&member.path).or_insert(object);
                    }
                }
                tx.commit()?;
                Ok(true)
            })?;
            if !stored {
                self.discard_staged_virtual_tree(rec)?;
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn discard_staged_virtual_tree(&self, rec: &ArchiveRecord) -> Result<()> {
        loop {
            let deleted = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if archive_generation_is_published(&tx, rec)? {
                    return Ok(0);
                }
                tx.execute(
                    "DELETE FROM entries WHERE is_virtual = 1 AND object_id IN (
                         SELECT object_id FROM objects
                         WHERE archive_container_id = ?1 AND archive_generation = ?2
                           AND deleted_at IS NOT NULL
                         ORDER BY archive_member_ordinal LIMIT ?3
                     )",
                    params![
                        rec.object_id.0,
                        rec.generation as i64,
                        ARCHIVE_MEMBER_BATCH as i64
                    ],
                )?;
                let deleted = tx.execute(
                    "DELETE FROM objects WHERE object_id IN (
                         SELECT object_id FROM objects
                         WHERE archive_container_id = ?1 AND archive_generation = ?2
                           AND deleted_at IS NOT NULL
                         ORDER BY archive_member_ordinal LIMIT ?3
                     )",
                    params![
                        rec.object_id.0,
                        rec.generation as i64,
                        ARCHIVE_MEMBER_BATCH as i64
                    ],
                )?;
                tx.commit()?;
                Ok(deleted)
            })?;
            if deleted < ARCHIVE_MEMBER_BATCH {
                return Ok(());
            }
        }
    }

    fn trim_staged_virtual_tail(&self, rec: &ArchiveRecord, first_ordinal: u32) -> Result<bool> {
        loop {
            let (current, deleted) = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if !archive_generation_is_current(&tx, rec)? {
                    return Ok((false, 0));
                }
                tx.execute(
                    "DELETE FROM entries WHERE is_virtual = 1 AND object_id IN (
                         SELECT object_id FROM objects WHERE archive_container_id = ?1
                           AND archive_generation = ?2 AND archive_member_ordinal >= ?3
                         ORDER BY archive_member_ordinal LIMIT ?4
                     )",
                    params![
                        rec.object_id.0,
                        rec.generation as i64,
                        first_ordinal as i64,
                        ARCHIVE_MEMBER_BATCH as i64
                    ],
                )?;
                let deleted = tx.execute(
                    "DELETE FROM objects WHERE object_id IN (
                         SELECT object_id FROM objects WHERE archive_container_id = ?1
                           AND archive_generation = ?2 AND archive_member_ordinal >= ?3
                         ORDER BY archive_member_ordinal LIMIT ?4
                     )",
                    params![
                        rec.object_id.0,
                        rec.generation as i64,
                        first_ordinal as i64,
                        ARCHIVE_MEMBER_BATCH as i64
                    ],
                )?;
                tx.commit()?;
                Ok((true, deleted))
            })?;
            if !current {
                return Ok(false);
            }
            if deleted < ARCHIVE_MEMBER_BATCH {
                return Ok(true);
            }
        }
    }

    /// Store a manifest and its members with the content record that
    /// publishes the object's content state. Member rows are staged in
    /// bounded transactions; the final record publication is atomic and
    /// only succeeds while the object is still at `rec.generation`. Returns
    /// `false` when a newer generation superseded this result.
    pub fn store_archive(
        &self,
        rec: &ArchiveRecord,
        members: &[ArchiveMember],
        content: &ContentRecord,
        complete_job: Option<JobId>,
    ) -> Result<bool> {
        if content.object_id != rec.object_id
            || content.source_id != rec.source_id
            || content.generation != rec.generation
        {
            return Err(crate::CatalogError::InvalidState(
                "archive and content records identify different object generations".into(),
            ));
        }
        if members
            .iter()
            .enumerate()
            .any(|(ordinal, member)| member.ordinal as usize != ordinal)
        {
            return Err(crate::CatalogError::InvalidState(
                "archive member ordinals must be contiguous from zero".into(),
            ));
        }
        let member_count = u32::try_from(members.len()).map_err(|_| {
            crate::CatalogError::InvalidState("archive has more than u32::MAX members".into())
        })?;
        // The parser is deterministic for an object generation. A duplicate
        // retry must not stage over or trim the member set that readers are
        // already using for that same generation.
        let preflight = self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !archive_generation_is_current(&tx, rec)? {
                return Ok(None);
            }
            if archive_generation_is_published(&tx, rec)? {
                if let Some(job) = complete_job {
                    tx.execute(
                        "UPDATE jobs SET state = 'done', finished_at = ?2, last_error = NULL WHERE job_id = ?1",
                        params![job.0, UnixNanos::now().0],
                    )?;
                }
                tx.commit()?;
                return Ok(Some(true));
            }
            tx.commit()?;
            Ok(Some(false))
        })?;
        match preflight {
            None => return Ok(false),
            Some(true) => return Ok(true),
            Some(false) => {}
        }
        if member_count != virtual_member_count(rec)? {
            return Err(crate::CatalogError::InvalidState(format!(
                "archive record describes {} virtual members but {} were supplied",
                virtual_member_count(rec)?,
                member_count
            )));
        }

        for batch in members.chunks(ARCHIVE_MEMBER_BATCH) {
            let stored = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if !archive_generation_is_current(&tx, rec)? {
                    return Ok(false);
                }
                insert_archive_members(&tx, rec, batch)?;
                tx.commit()?;
                Ok(true)
            })?;
            if !stored {
                self.discard_unpublished_archive_members(rec)?;
                return Ok(false);
            }
        }

        // A retry using the same generation may contain fewer members than
        // its earlier attempt. Trim that tail in bounded writer windows.
        if !self.trim_archive_member_tail(rec, member_count)? {
            self.discard_unpublished_archive_members(rec)?;
            return Ok(false);
        }
        if !self.stage_virtual_tree(rec, members)?
            || !self.trim_staged_virtual_tail(rec, member_count)?
        {
            self.discard_unpublished_archive_members(rec)?;
            self.discard_staged_virtual_tree(rec)?;
            return Ok(false);
        }

        let stored = self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !archive_generation_is_current(&tx, rec)? {
                return Ok(false);
            }
            publish_virtual_tree(&tx, rec)?;
            upsert_archive_record(&tx, rec)?;
            upsert_content_record(&tx, content, true)?;
            // A container that was text in an earlier generation keeps no chunks.
            tx.execute(
                "DELETE FROM chunks WHERE object_id = ?1",
                params![rec.object_id.0],
            )?;
            flip_state(&tx, rec.object_id, content.state, None)?;
            if let Some(job) = complete_job {
                tx.execute(
                    "UPDATE jobs SET state = 'done', finished_at = ?2, last_error = NULL WHERE job_id = ?1",
                    params![job.0, UnixNanos::now().0],
                )?;
            }
            tx.commit()?;
            Ok(true)
        })?;
        if !stored {
            self.discard_unpublished_archive_members(rec)?;
            self.discard_staged_virtual_tree(rec)?;
            return Ok(false);
        }

        // The record now points only at the new generation, so older rows
        // are invisible and can be reclaimed without one large transaction.
        self.purge_older_archive_members(rec.object_id, rec.generation)?;
        Ok(true)
    }

    /// Record a verdict without members or a content state change (a
    /// container by extension that is not one by content). Returns `false`
    /// when the object has already advanced to a newer generation.
    pub fn store_archive_marker(&self, rec: &ArchiveRecord) -> Result<bool> {
        let preflight = self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !archive_generation_is_current(&tx, rec)? {
                return Ok(None);
            }
            let published = archive_generation_is_published(&tx, rec)?;
            tx.commit()?;
            Ok(Some(published))
        })?;
        match preflight {
            None => return Ok(false),
            Some(true) => return Ok(true),
            Some(false) => {}
        }
        if !self.trim_archive_member_tail(rec, 0)? {
            self.discard_unpublished_archive_members(rec)?;
            return Ok(false);
        }
        if !self.trim_staged_virtual_tail(rec, 0)? {
            return Ok(false);
        }
        let stored = self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !archive_generation_is_current(&tx, rec)? {
                return Ok(false);
            }
            publish_virtual_tree(&tx, rec)?;
            upsert_archive_record(&tx, rec)?;
            tx.commit()?;
            Ok(true)
        })?;
        if stored {
            self.purge_older_archive_members(rec.object_id, rec.generation)?;
        }
        Ok(stored)
    }

    fn trim_archive_member_tail(&self, rec: &ArchiveRecord, first_ordinal: u32) -> Result<bool> {
        loop {
            let (current, deleted) = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if !archive_generation_is_current(&tx, rec)? {
                    return Ok((false, 0));
                }
                let n = tx.execute(
                    "DELETE FROM archive_members
                     WHERE object_id = ?1 AND generation = ?2 AND ordinal IN (
                         SELECT ordinal FROM archive_members
                         WHERE object_id = ?1 AND generation = ?2 AND ordinal >= ?3
                         ORDER BY ordinal LIMIT ?4
                     )",
                    params![
                        rec.object_id.0,
                        rec.generation as i64,
                        first_ordinal as i64,
                        ARCHIVE_MEMBER_BATCH as i64
                    ],
                )?;
                tx.commit()?;
                Ok((true, n))
            })?;
            if !current {
                return Ok(false);
            }
            if deleted < ARCHIVE_MEMBER_BATCH {
                return Ok(true);
            }
        }
    }

    fn discard_unpublished_archive_members(&self, rec: &ArchiveRecord) -> Result<()> {
        loop {
            let deleted = self.with_writer(|conn| {
                let published: Option<i64> = conn
                    .query_row(
                        "SELECT generation FROM archive_records WHERE object_id = ?1",
                        params![rec.object_id.0],
                        |r| r.get(0),
                    )
                    .optional()?;
                if published == Some(rec.generation as i64) {
                    return Ok(0);
                }
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let n = tx.execute(
                    "DELETE FROM archive_members
                     WHERE object_id = ?1 AND generation = ?2 AND ordinal IN (
                         SELECT ordinal FROM archive_members
                         WHERE object_id = ?1 AND generation = ?2
                         ORDER BY ordinal LIMIT ?3
                     )",
                    params![
                        rec.object_id.0,
                        rec.generation as i64,
                        ARCHIVE_MEMBER_BATCH as i64
                    ],
                )?;
                tx.commit()?;
                Ok(n)
            })?;
            if deleted < ARCHIVE_MEMBER_BATCH {
                return Ok(());
            }
        }
    }

    fn purge_older_archive_members(&self, object: ObjectId, generation: u32) -> Result<()> {
        loop {
            let deleted = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let n = tx.execute(
                    "DELETE FROM archive_members
                     WHERE object_id = ?1 AND generation < ?2 AND (generation, ordinal) IN (
                         SELECT generation, ordinal FROM archive_members
                         WHERE object_id = ?1 AND generation < ?2
                         ORDER BY generation, ordinal LIMIT ?3
                     )",
                    params![object.0, generation as i64, ARCHIVE_MEMBER_BATCH as i64],
                )?;
                tx.commit()?;
                Ok(n)
            })?;
            if deleted < ARCHIVE_MEMBER_BATCH {
                return Ok(());
            }
        }
    }

    pub fn archive_record(&self, object: ObjectId) -> Result<Option<ArchiveRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .query_row(
                    "SELECT object_id, source_id, generation, format, member_count, dir_count, implicit_dir_count,
                            suspicious_count, declared_size, compressed_size, claimed_entries, zip64, truncated,
                            comment, state, error, reason, processed_at, elapsed_ms
                     FROM archive_records WHERE object_id = ?1",
                    params![object.0],
                    |r| {
                        Ok(ArchiveRecord {
                            object_id: ObjectId(r.get(0)?),
                            source_id: SourceId(r.get(1)?),
                            generation: r.get::<_, i64>(2)? as u32,
                            format: r.get(3)?,
                            member_count: r.get::<_, i64>(4)? as u64,
                            dir_count: r.get::<_, i64>(5)? as u64,
                            implicit_dir_count: r.get::<_, i64>(6)? as u64,
                            suspicious_count: r.get::<_, i64>(7)? as u64,
                            declared_size: r.get::<_, i64>(8)? as u64,
                            compressed_size: r.get::<_, i64>(9)? as u64,
                            claimed_entries: r.get::<_, i64>(10)? as u64,
                            zip64: r.get::<_, i64>(11)? != 0,
                            truncated: r.get::<_, i64>(12)? != 0,
                            comment: r.get(13)?,
                            state: ContentState::parse(&r.get::<_, String>(14)?)
                                .unwrap_or(ContentState::Failed),
                            error: r.get(15)?,
                            reason: r.get(16)?,
                            processed_at: UnixNanos(r.get(17)?),
                            elapsed_ms: r.get(18)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    /// Members of the object's current manifest: `(page, total matching)`.
    /// Directories sort before files, then by name; prefix listings sort by
    /// path.
    pub fn archive_members(
        &self,
        object: ObjectId,
        q: &MemberQuery,
    ) -> Result<(Vec<ArchiveMember>, u64)> {
        self.with_reader(|conn| {
            let generation: Option<i64> = conn
                .query_row(
                    "SELECT generation FROM archive_records WHERE object_id = ?1",
                    params![object.0],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(generation) = generation else {
                return Ok((Vec::new(), 0));
            };
            let limit = q.limit.clamp(1, 5000) as i64;
            let (filter_sql, filter_arg): (&str, String) = match (&q.parent, &q.prefix) {
                (Some(p), _) => ("parent = ?3", p.clone()),
                (None, Some(p)) => ("substr(path, 1, length(?3)) = ?3", p.clone()),
                (None, None) => ("?3 = ''", String::new()),
            };
            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM archive_members WHERE object_id = ?1 AND generation = ?2 AND {filter_sql}"),
                params![object.0, generation, filter_arg],
                |r| r.get(0),
            )?;
            let order = if q.parent.is_some() {
                "is_dir DESC, name"
            } else {
                "path"
            };
            let rows = conn
                .prepare(&format!(
                    "SELECT {MEMBER_COLUMNS} FROM archive_members
                     WHERE object_id = ?1 AND generation = ?2 AND {filter_sql}
                     ORDER BY {order} LIMIT ?4 OFFSET ?5"
                ))?
                .query_map(
                    params![object.0, generation, filter_arg, limit, q.offset as i64],
                    member_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((rows, total as u64))
        })
    }

    pub fn archive_stats(&self, source: Option<SourceId>) -> Result<ArchiveStats> {
        self.with_reader(|conn| {
            let (filter, arg) = match source {
                Some(s) => ("WHERE source_id = ?1", s.0),
                None => ("WHERE ?1 = 0", 0),
            };
            conn.query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(member_count), 0), COALESCE(SUM(declared_size), 0),
                            COALESCE(SUM(truncated), 0), COALESCE(SUM(state = 'failed'), 0)
                     FROM archive_records {filter} AND state != 'unsupported'"
                ),
                params![arg],
                |r| {
                    Ok(ArchiveStats {
                        archives: r.get::<_, i64>(0)? as u64,
                        members: r.get::<_, i64>(1)? as u64,
                        declared_size: r.get::<_, i64>(2)? as u64,
                        truncated: r.get::<_, i64>(3)? as u64,
                        failed: r.get::<_, i64>(4)? as u64,
                    })
                },
            )
            .map_err(Into::into)
        })
    }

    /// Queue a manifest job for every container file (by extension) whose
    /// current generation has no manifest — typically files crawled before
    /// archive support existed and left `unsupported`. Returns the number
    /// queued.
    pub fn requeue_archives(&self, source: Option<SourceId>) -> Result<u64> {
        let mut total = 0u64;
        let mut after_object_id = i64::MIN;
        loop {
            let exts = archive_extension_list();
            let (selected, queued, last_object_id) = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let rows: Vec<(i64, i64, i64, i64)> = {
                    let mut stmt = tx.prepare(&format!(
                        "SELECT o.object_id, o.source_id, o.generation, o.size FROM objects o
                         WHERE o.deleted_at IS NULL AND o.kind = 'file'
                           AND lower(COALESCE((
                               SELECT e.extension FROM entries e
                               WHERE e.object_id = o.object_id AND e.deleted_at IS NULL
                               ORDER BY e.entry_id LIMIT 1
                           ), '')) IN ({exts})
                           AND o.content_state NOT IN ('excluded')
                           AND (?1 IS NULL OR o.source_id = ?1)
                           AND o.object_id > ?2
                           AND (
                               NOT EXISTS (
                                   SELECT 1 FROM archive_records a
                                   WHERE a.object_id = o.object_id AND a.generation = o.generation
                               )
                               OR EXISTS (
                                   SELECT 1 FROM archive_records a
                                   WHERE a.object_id = o.object_id AND a.generation = o.generation
                                     AND (
                                         a.aggregate_generation IS NULL
                                         OR a.aggregate_generation != a.generation
                                         OR (a.member_count + a.implicit_dir_count > 0 AND NOT EXISTS (
                                             SELECT 1 FROM objects v
                                             WHERE v.archive_container_id = o.object_id
                                               AND v.archive_generation = o.generation
                                               AND v.deleted_at IS NULL
                                         ))
                                     )
                               )
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM jobs j
                               WHERE j.idempotency_key = 'content_text:' || o.object_id || ':' || o.generation
                                 AND j.state IN ('queued', 'running')
                           )
                         ORDER BY o.object_id LIMIT ?3"
                    ))?;
                    let rows = stmt
                        .query_map(
                            params![
                                source.map(|s| s.0),
                                after_object_id,
                                ARCHIVE_REQUEUE_BATCH as i64
                            ],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )?
                        .collect::<rusqlite::Result<_>>()?;
                    rows
                };
                let selected = rows.len();
                let last_object_id = rows.last().map(|row| row.0);
                let mut queued = 0u64;
                for (obj, src, generation, size) in rows {
                    let object = ObjectId(obj);
                    flip_state(&tx, object, ContentState::Pending, None)?;
                    let key = NewJob::object_key(
                        JobStage::ContentText,
                        object,
                        generation as u32,
                    );
                    tx.execute(
                        "DELETE FROM jobs WHERE idempotency_key = ?1 AND state IN ('done','failed','superseded')",
                        params![key],
                    )?;
                    if enqueue_conn(
                        &tx,
                        &NewJob {
                            source_id: SourceId(src),
                            object_id: Some(object),
                            object_generation: generation as u32,
                            stage: JobStage::ContentText,
                            priority: Priority::ArchiveManifest,
                            idempotency_key: key,
                            payload: None,
                            estimated_cost: size as u64,
                        },
                    )?
                    .is_some()
                    {
                        queued += 1;
                    }
                }
                tx.commit()?;
                Ok((selected, queued, last_object_id))
            })?;
            total += queued;
            let Some(last_object_id) = last_object_id else {
                return Ok(total);
            };
            after_object_id = last_object_id;
            if selected < ARCHIVE_REQUEUE_BATCH {
                return Ok(total);
            }
        }
    }
}
