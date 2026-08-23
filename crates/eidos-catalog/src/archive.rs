//! Archive manifests (ADR-0010): one record per container object and its
//! virtual members as read from the container's own directory, never from
//! member data. Stored alongside content records so the content job queue,
//! budgets, and activity views carry archives without a second pipeline.

use crate::content::{flip_state, upsert_content_record, ContentRecord};
use crate::jobs::{enqueue_conn, NewJob};
use crate::{Catalog, Result};
use eidos_domain::*;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

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
            truncated, comment, state, error, reason, processed_at, elapsed_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(object_id) DO UPDATE SET source_id = excluded.source_id, generation = excluded.generation,
            format = excluded.format, member_count = excluded.member_count, dir_count = excluded.dir_count,
            implicit_dir_count = excluded.implicit_dir_count, suspicious_count = excluded.suspicious_count,
            declared_size = excluded.declared_size, compressed_size = excluded.compressed_size,
            claimed_entries = excluded.claimed_entries, zip64 = excluded.zip64, truncated = excluded.truncated,
            comment = excluded.comment, state = excluded.state, error = excluded.error, reason = excluded.reason,
            processed_at = excluded.processed_at, elapsed_ms = excluded.elapsed_ms",
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
    /// Store a manifest and its members with the content record that
    /// publishes the object's content state, in one transaction; marks the
    /// job done when given. Replaces any earlier manifest of the object.
    pub fn store_archive(
        &self,
        rec: &ArchiveRecord,
        members: &[ArchiveMember],
        content: &ContentRecord,
        complete_job: Option<JobId>,
    ) -> Result<()> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            upsert_archive_record(&tx, rec)?;
            tx.execute(
                "DELETE FROM archive_members WHERE object_id = ?1",
                params![rec.object_id.0],
            )?;
            {
                let mut stmt = tx.prepare_cached(&format!(
                    "INSERT INTO archive_members (object_id, generation, {MEMBER_COLUMNS})
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
            }
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
            Ok(())
        })
    }

    /// Record a verdict without members or a content state change (a
    /// container by extension that is not one by content).
    pub fn store_archive_marker(&self, rec: &ArchiveRecord) -> Result<()> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            upsert_archive_record(&tx, rec)?;
            tx.execute(
                "DELETE FROM archive_members WHERE object_id = ?1",
                params![rec.object_id.0],
            )?;
            tx.commit()?;
            Ok(())
        })
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
        self.with_writer(|conn| {
            let exts = archive_extension_list();
            let source_filter = match source {
                Some(_) => "AND o.source_id = ?1",
                None => "AND ?1 = ?1",
            };
            let rows: Vec<(i64, i64, i64, i64)> = conn
                .prepare(&format!(
                    "SELECT o.object_id, o.source_id, o.generation, o.size FROM objects o
                     JOIN entries e ON e.object_id = o.object_id AND e.deleted_at IS NULL
                     WHERE o.deleted_at IS NULL AND o.kind = 'file' AND lower(e.extension) IN ({exts})
                       AND o.content_state NOT IN ('excluded')
                       AND NOT EXISTS (SELECT 1 FROM archive_records a WHERE a.object_id = o.object_id AND a.generation = o.generation)
                       {source_filter}"
                ))?
                .query_map(params![source.map(|s| s.0).unwrap_or(0)], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut n = 0u64;
            for (obj, src, gen, size) in rows {
                let object = ObjectId(obj);
                flip_state(&tx, object, ContentState::Pending, None)?;
                let key = NewJob::object_key(JobStage::ContentText, object, gen as u32);
                tx.execute(
                    "DELETE FROM jobs WHERE idempotency_key = ?1 AND state IN ('done','failed','superseded')",
                    params![key],
                )?;
                if enqueue_conn(
                    &tx,
                    &NewJob {
                        source_id: SourceId(src),
                        object_id: Some(object),
                        object_generation: gen as u32,
                        stage: JobStage::ContentText,
                        priority: Priority::ArchiveManifest,
                        idempotency_key: key,
                        payload: None,
                        estimated_cost: size as u64,
                    },
                )?
                .is_some()
                {
                    n += 1;
                }
            }
            tx.commit()?;
            Ok(n)
        })
    }
}
