//! Content records, stored chunks, and content job bookkeeping.
//!
//! The catalog is the extraction cache: every chunk's text is stored
//! (zstd-compressed) with exact byte/line ranges so search verification and
//! snippets read original text, and so derived indexes can be rebuilt
//! without touching source files.

use crate::aggregates::{apply_delta, AggDelta};
use crate::jobs::{enqueue_conn, outbox_append_conn, NewJob};
use crate::{Catalog, CatalogError, Result};
use eidos_content::Chunk;
use eidos_domain::{
    ContentId, ContentState, Coverage, FailureClass, JobStage, ObjectId, Priority, SourceId,
    UnixNanos,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ZSTD_LEVEL: i32 = 1;

/// Size tiers (bytes) for content job priorities.
pub const SMALL_TEXT_LIMIT: u64 = 256 * 1024;
pub const NORMAL_TEXT_LIMIT: u64 = 16 * 1024 * 1024;

pub fn priority_for_size(size: u64) -> Priority {
    if size < SMALL_TEXT_LIMIT {
        Priority::SmallText
    } else if size < NORMAL_TEXT_LIMIT {
        Priority::NormalText
    } else {
        Priority::LargeText
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentRecord {
    pub object_id: ObjectId,
    pub source_id: SourceId,
    pub generation: u32,
    pub extraction_version: u16,
    pub encoding: Option<String>,
    pub coverage: Coverage,
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    pub chunk_count: u32,
    pub line_count: u64,
    pub chars: u64,
    pub content_id: Option<ContentId>,
    pub hash_complete: bool,
    pub state: ContentState,
    pub failure_class: Option<FailureClass>,
    pub error: Option<String>,
    pub reason: Option<String>,
    pub processed_at: UnixNanos,
    pub elapsed_ms: f64,
}

/// Object facts a content worker needs before extracting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentTarget {
    pub object_id: ObjectId,
    pub source_id: SourceId,
    pub generation: u32,
    pub size: u64,
    pub path: String,
    pub content_state: ContentState,
    pub content_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRow {
    pub object_id: ObjectId,
    pub generation: u32,
    pub ordinal: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub chars: u32,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContentStats {
    /// state -> (files, indexed bytes, chunks)
    pub by_state: BTreeMap<String, (u64, u64, u64)>,
    pub total_records: u64,
    pub indexed_bytes: u64,
    pub chunks: u64,
}

fn compress(text: &str) -> Vec<u8> {
    zstd::bulk::compress(text.as_bytes(), ZSTD_LEVEL).unwrap_or_else(|_| text.as_bytes().to_vec())
}

fn decompress(blob: &[u8], chars_hint: u32) -> String {
    let cap = (chars_hint as usize).saturating_mul(4).max(64);
    match zstd::bulk::decompress(blob, cap.max(blob.len() * 8)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::from_utf8_lossy(blob).into_owned(),
    }
}

impl Catalog {
    /// Look up what a content job should process. `None` when the object is
    /// gone.
    pub fn content_target(&self, object: ObjectId) -> Result<Option<ContentTarget>> {
        self.with_reader(|conn| {
            let row: Option<(i64, i64, i64, String)> = conn
                .query_row(
                    "SELECT source_id, generation, size, content_state FROM objects WHERE object_id = ?1 AND deleted_at IS NULL AND kind = 'file'",
                    params![object.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let (source_id, generation, size, state) = match row {
                Some(r) => r,
                None => return Ok(None),
            };
            let path = match crate::read::render_path_conn(conn, object)? {
                Some(p) => p,
                None => return Ok(None),
            };
            let enabled: i64 = conn
                .query_row(
                    "SELECT content_enabled FROM sources WHERE source_id = ?1",
                    params![source_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(1);
            Ok(Some(ContentTarget {
                object_id: object,
                source_id: SourceId(source_id),
                generation: generation as u32,
                size: size as u64,
                path,
                content_state: ContentState::parse(&state).unwrap_or(ContentState::Pending),
                content_enabled: enabled != 0,
            }))
        })
    }

    /// Store a batch of chunks for an object generation (replacing any with
    /// the same ordinal).
    pub fn write_chunks(&self, object: ObjectId, generation: u32, chunks: &[Chunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        self.with_writer(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO chunks (object_id, generation, ordinal, byte_start, byte_end, line_start, line_end, chars, text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for c in chunks {
                    stmt.execute(params![
                        object.0,
                        generation as i64,
                        c.ordinal as i64,
                        c.byte_start as i64,
                        c.byte_end as i64,
                        c.line_start as i64,
                        c.line_end as i64,
                        c.text.chars().count() as i64,
                        compress(&c.text),
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Record the outcome of extraction and, when `publish` is true, flip the
    /// object's content state (with aggregate deltas and an outbox row).
    /// Workers call this with `publish = false` ("indexing") before the index
    /// commit and `mark_content_indexed` after it.
    pub fn finish_content(&self, rec: &ContentRecord, publish: bool) -> Result<()> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state_str = if publish { rec.state.as_str().to_string() } else { "indexing".to_string() };
            tx.execute(
                "INSERT INTO content_records (object_id, source_id, generation, extraction_version, encoding, coverage, indexed_bytes,
                    total_bytes, chunk_count, line_count, chars, content_id, hash_complete, state, failure_class, error, reason,
                    processed_at, elapsed_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(object_id) DO UPDATE SET source_id = excluded.source_id, generation = excluded.generation,
                    extraction_version = excluded.extraction_version, encoding = excluded.encoding, coverage = excluded.coverage,
                    indexed_bytes = excluded.indexed_bytes, total_bytes = excluded.total_bytes, chunk_count = excluded.chunk_count,
                    line_count = excluded.line_count, chars = excluded.chars, content_id = excluded.content_id,
                    hash_complete = excluded.hash_complete, state = excluded.state, failure_class = excluded.failure_class,
                    error = excluded.error, reason = excluded.reason, processed_at = excluded.processed_at, elapsed_ms = excluded.elapsed_ms",
                params![
                    rec.object_id.0,
                    rec.source_id.0,
                    rec.generation as i64,
                    rec.extraction_version as i64,
                    rec.encoding,
                    rec.coverage.as_str(),
                    rec.indexed_bytes as i64,
                    rec.total_bytes as i64,
                    rec.chunk_count as i64,
                    rec.line_count as i64,
                    rec.chars as i64,
                    rec.content_id.map(|c| c.0.to_vec()),
                    rec.hash_complete as i64,
                    state_str,
                    rec.failure_class.map(|f| f.as_str()),
                    rec.error,
                    rec.reason,
                    rec.processed_at.0,
                    rec.elapsed_ms,
                ],
            )?;
            // Older generations' chunks are no longer needed.
            tx.execute(
                "DELETE FROM chunks WHERE object_id = ?1 AND generation < ?2",
                params![rec.object_id.0, rec.generation as i64],
            )?;
            if publish {
                flip_state(&tx, rec.object_id, rec.state, rec.content_id)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// After the index commit: publish the content state for these objects.
    pub fn mark_content_indexed(&self, objects: &[ObjectId]) -> Result<u64> {
        if objects.is_empty() {
            return Ok(0);
        }
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut n = 0;
            for o in objects {
                let row: Option<(String, Option<Vec<u8>>, i64)> = tx
                    .query_row(
                        "SELECT coverage, content_id, generation FROM content_records WHERE object_id = ?1 AND state = 'indexing'",
                        params![o.0],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let (coverage, cid, gen) = match row {
                    Some(r) => r,
                    None => continue,
                };
                // Skip if the object moved on to a newer generation meanwhile.
                let current: Option<i64> = tx
                    .query_row(
                        "SELECT generation FROM objects WHERE object_id = ?1 AND deleted_at IS NULL",
                        params![o.0],
                        |r| r.get(0),
                    )
                    .optional()?;
                if current != Some(gen) {
                    tx.execute("UPDATE content_records SET state = 'stale' WHERE object_id = ?1", params![o.0])?;
                    continue;
                }
                let state = if coverage == Coverage::Full.as_str() {
                    ContentState::Indexed
                } else {
                    ContentState::Partial
                };
                tx.execute(
                    "UPDATE content_records SET state = ?2 WHERE object_id = ?1",
                    params![o.0, state.as_str()],
                )?;
                let content_id = cid.and_then(|v| {
                    if v.len() == 32 {
                        let mut a = [0u8; 32];
                        a.copy_from_slice(&v);
                        Some(ContentId(a))
                    } else {
                        None
                    }
                });
                flip_state(&tx, *o, state, content_id)?;
                n += 1;
            }
            tx.commit()?;
            Ok(n)
        })
    }

    pub fn content_record(&self, object: ObjectId) -> Result<Option<ContentRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT object_id, source_id, generation, extraction_version, encoding, coverage, indexed_bytes, total_bytes,
                            chunk_count, line_count, chars, content_id, hash_complete, state, failure_class, error, reason,
                            processed_at, elapsed_ms FROM content_records WHERE object_id = ?1",
                )?
                .query_row(params![object.0], record_from_row)
                .optional()?)
        })
    }

    /// Fetch chunks by ordinal for an object generation.
    pub fn chunks_for(
        &self,
        object: ObjectId,
        generation: u32,
        ordinals: &[u32],
    ) -> Result<Vec<ChunkRow>> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT ordinal, byte_start, byte_end, line_start, line_end, chars, text FROM chunks
                 WHERE object_id = ?1 AND generation = ?2 AND ordinal = ?3",
            )?;
            let mut out = Vec::with_capacity(ordinals.len());
            for o in ordinals {
                if let Some(row) = stmt
                    .query_row(params![object.0, generation as i64, *o as i64], |r| {
                        let chars: i64 = r.get(5)?;
                        let blob: Vec<u8> = r.get(6)?;
                        Ok(ChunkRow {
                            object_id: object,
                            generation,
                            ordinal: r.get::<_, i64>(0)? as u32,
                            byte_start: r.get::<_, i64>(1)? as u64,
                            byte_end: r.get::<_, i64>(2)? as u64,
                            line_start: r.get::<_, i64>(3)? as u64,
                            line_end: r.get::<_, i64>(4)? as u64,
                            chars: chars as u32,
                            text: decompress(&blob, chars as u32),
                        })
                    })
                    .optional()?
                {
                    out.push(row);
                }
            }
            Ok(out)
        })
    }

    /// Stream all chunks of an object generation in a range of ordinals.
    pub fn chunks_range(
        &self,
        object: ObjectId,
        generation: u32,
        from: u32,
        to_inclusive: u32,
    ) -> Result<Vec<ChunkRow>> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT ordinal, byte_start, byte_end, line_start, line_end, chars, text FROM chunks
                 WHERE object_id = ?1 AND generation = ?2 AND ordinal BETWEEN ?3 AND ?4 ORDER BY ordinal",
            )?;
            let rows = stmt
                .query_map(params![object.0, generation as i64, from as i64, to_inclusive as i64], |r| {
                    let chars: i64 = r.get(5)?;
                    let blob: Vec<u8> = r.get(6)?;
                    Ok(ChunkRow {
                        object_id: object,
                        generation,
                        ordinal: r.get::<_, i64>(0)? as u32,
                        byte_start: r.get::<_, i64>(1)? as u64,
                        byte_end: r.get::<_, i64>(2)? as u64,
                        line_start: r.get::<_, i64>(3)? as u64,
                        line_end: r.get::<_, i64>(4)? as u64,
                        chars: chars as u32,
                        text: decompress(&blob, chars as u32),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Enqueue content jobs for every pending file of a source that has no
    /// queued/running job for its current generation. Returns the number
    /// enqueued. Respects `sources.content_enabled`.
    pub fn enqueue_pending_content(&self, source: SourceId, limit: u32) -> Result<u64> {
        self.with_writer(|conn| enqueue_pending_content_conn(conn, source, limit))
    }

    /// Re-queue objects whose content records were left `indexing` by a
    /// crash (their index documents may be missing).
    pub fn requeue_unfinished_content(&self) -> Result<u64> {
        self.with_writer(|conn| {
            let rows: Vec<(i64, i64, i64, i64)> = conn
                .prepare(
                    "SELECT c.object_id, c.source_id, o.generation, o.size FROM content_records c
                     JOIN objects o ON o.object_id = c.object_id
                     WHERE c.state = 'indexing' AND o.deleted_at IS NULL",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let tx = conn.transaction()?;
            let mut n = 0;
            for (obj, src, gen, size) in rows {
                let key = NewJob::object_key(JobStage::ContentText, ObjectId(obj), gen as u32);
                tx.execute("DELETE FROM jobs WHERE idempotency_key = ?1 AND state IN ('done','failed','superseded')", params![key])?;
                if enqueue_conn(
                    &tx,
                    &NewJob {
                        source_id: SourceId(src),
                        object_id: Some(ObjectId(obj)),
                        object_generation: gen as u32,
                        stage: JobStage::ContentText,
                        priority: priority_for_size(size as u64),
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

    pub fn content_stats(&self, source: Option<SourceId>) -> Result<ContentStats> {
        self.with_reader(|conn| {
            let mut stats = ContentStats::default();
            let mut stmt = conn.prepare_cached(
                "SELECT state, COUNT(*), COALESCE(SUM(indexed_bytes), 0), COALESCE(SUM(chunk_count), 0) FROM content_records
                 WHERE (?1 IS NULL OR source_id = ?1) GROUP BY state",
            )?;
            let rows = stmt.query_map(params![source.map(|s| s.0)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })?;
            for row in rows {
                let (state, n, bytes, chunks) = row?;
                stats.total_records += n;
                stats.indexed_bytes += bytes;
                stats.chunks += chunks;
                stats.by_state.insert(state, (n, bytes, chunks));
            }
            Ok(stats)
        })
    }

    /// The content index was (re)created empty: every object whose content
    /// was published must be extracted again. Stored chunks are dropped so
    /// the catalog never claims coverage the index cannot serve.
    pub fn reset_content_for_reindex(&self) -> Result<u64> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let objects: Vec<i64> = tx
                .prepare("SELECT object_id FROM objects WHERE content_state IN ('indexed','partial') AND deleted_at IS NULL")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for o in &objects {
                flip_state(&tx, ObjectId(*o), ContentState::Pending, None)?;
            }
            tx.execute("DELETE FROM content_records", [])?;
            tx.execute("DELETE FROM chunks", [])?;
            tx.execute(
                "UPDATE jobs SET state = 'superseded', finished_at = ?1 WHERE stage = 'content_text' AND state IN ('queued','running')",
                params![UnixNanos::now().0],
            )?;
            tx.commit()?;
            Ok(objects.len() as u64)
        })
    }

    /// Objects per content state for a source (for activity views).
    pub fn content_state_counts(&self, source: SourceId) -> Result<BTreeMap<String, u64>> {
        self.with_reader(|conn| {
            let mut out = BTreeMap::new();
            let mut stmt = conn.prepare_cached(
                "SELECT content_state, COUNT(*) FROM objects WHERE source_id = ?1 AND deleted_at IS NULL AND kind = 'file' GROUP BY content_state",
            )?;
            for row in stmt.query_map(params![source.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))? {
                let (k, v) = row?;
                out.insert(k, v);
            }
            Ok(out)
        })
    }

    pub fn set_content_policy(
        &self,
        source: SourceId,
        enabled: bool,
        concurrency: u32,
    ) -> Result<()> {
        self.with_writer(|conn| {
            let n = conn.execute(
                "UPDATE sources SET content_enabled = ?2, content_concurrency = ?3, updated_at = ?4 WHERE source_id = ?1",
                params![source.0, enabled as i64, concurrency.max(1) as i64, UnixNanos::now().0],
            )?;
            if n == 0 {
                return Err(CatalogError::NotFound(format!("source {source}")));
            }
            Ok(())
        })
    }
}

fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ContentRecord> {
    let cid: Option<Vec<u8>> = r.get(11)?;
    Ok(ContentRecord {
        object_id: ObjectId(r.get(0)?),
        source_id: SourceId(r.get(1)?),
        generation: r.get::<_, i64>(2)? as u32,
        extraction_version: r.get::<_, i64>(3)? as u16,
        encoding: r.get(4)?,
        coverage: Coverage::parse(&r.get::<_, String>(5)?).unwrap_or(Coverage::None),
        indexed_bytes: r.get::<_, i64>(6)? as u64,
        total_bytes: r.get::<_, i64>(7)? as u64,
        chunk_count: r.get::<_, i64>(8)? as u32,
        line_count: r.get::<_, i64>(9)? as u64,
        chars: r.get::<_, i64>(10)? as u64,
        content_id: cid.and_then(|v| {
            if v.len() == 32 {
                let mut a = [0u8; 32];
                a.copy_from_slice(&v);
                Some(ContentId(a))
            } else {
                None
            }
        }),
        hash_complete: r.get::<_, i64>(12)? != 0,
        state: ContentState::parse(&r.get::<_, String>(13)?).unwrap_or(ContentState::Pending),
        failure_class: r
            .get::<_, Option<String>>(14)?
            .and_then(|s| FailureClass::parse(&s)),
        error: r.get(15)?,
        reason: r.get(16)?,
        processed_at: UnixNanos(r.get(17)?),
        elapsed_ms: r.get(18)?,
    })
}

/// Flip an object's content state and keep aggregates/outbox consistent.
fn flip_state(
    conn: &Connection,
    object: ObjectId,
    state: ContentState,
    content_id: Option<ContentId>,
) -> Result<()> {
    let row: Option<(String, i64, i64, i64, String)> = conn
        .query_row(
            "SELECT o.content_state, o.size, o.allocated, o.generation, COALESCE(e.extension, '')
             FROM objects o LEFT JOIN entries e ON e.object_id = o.object_id AND e.deleted_at IS NULL
             WHERE o.object_id = ?1 AND o.deleted_at IS NULL LIMIT 1",
            params![object.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (old_state, size, alloc, generation, ext) = match row {
        Some(r) => r,
        None => return Ok(()),
    };
    let old = ContentState::parse(&old_state).unwrap_or(ContentState::Pending);
    if old == state {
        return Ok(());
    }
    conn.execute(
        "UPDATE objects SET content_state = ?2, content_id = ?3 WHERE object_id = ?1",
        params![object.0, state.as_str(), content_id.map(|c| c.0.to_vec())],
    )?;
    // Aggregate content counters on every parent chain.
    let mut delta = AggDelta::for_file(&ext, size as u64, alloc as u64, old, -1);
    let add = AggDelta::for_file(&ext, size as u64, alloc as u64, state, 1);
    delta.file_count = 0;
    delta.logical = 0;
    delta.allocated = 0;
    delta.ext.clear();
    delta.pending += add.pending;
    delta.indexed += add.indexed;
    delta.failed += add.failed;
    delta.excluded += add.excluded;
    let parents: Vec<i64> = conn
        .prepare_cached("SELECT parent_id FROM entries WHERE object_id = ?1 AND deleted_at IS NULL AND parent_id IS NOT NULL")?
        .query_map(params![object.0], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for p in parents {
        apply_delta(conn, ObjectId(p), &delta)?;
    }
    let source_id: i64 = conn.query_row(
        "SELECT source_id FROM objects WHERE object_id = ?1",
        params![object.0],
        |r| r.get(0),
    )?;
    outbox_append_conn(conn, SourceId(source_id), object, "upsert", generation)?;
    Ok(())
}

pub(crate) fn enqueue_pending_content_conn(
    conn: &mut Connection,
    source: SourceId,
    limit: u32,
) -> Result<u64> {
    let enabled: i64 = conn
        .query_row(
            "SELECT content_enabled FROM sources WHERE source_id = ?1",
            params![source.0],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if enabled == 0 {
        return Ok(0);
    }
    let now = UnixNanos::now().0;
    let tx = conn.transaction()?;
    let n = tx.execute(
        "INSERT OR IGNORE INTO jobs (source_id, object_id, object_generation, stage, priority, state, idempotency_key, estimated_cost, created_at, scheduled_at)
         SELECT o.source_id, o.object_id, o.generation, 'content_text',
                CASE WHEN o.size < ?3 THEN 3 WHEN o.size < ?4 THEN 4 ELSE 5 END,
                'queued', 'content_text:' || o.object_id || ':' || o.generation, o.size, ?2, ?2
         FROM objects o
         WHERE o.source_id = ?1 AND o.deleted_at IS NULL AND o.kind = 'file' AND o.content_state IN ('pending', 'stale')
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.idempotency_key = 'content_text:' || o.object_id || ':' || o.generation)
         ORDER BY o.size ASC LIMIT ?5",
        params![source.0, now, SMALL_TEXT_LIMIT as i64, NORMAL_TEXT_LIMIT as i64, limit as i64],
    )?;
    tx.commit()?;
    Ok(n as u64)
}

/// Enqueue a content job for one object (incremental changes; the
/// coordinator's periodic top-up covers everything else).
pub(crate) fn enqueue_content_for(
    conn: &Connection,
    source: SourceId,
    object: ObjectId,
    generation: u32,
    size: u64,
) -> Result<()> {
    let enabled: i64 = conn
        .query_row(
            "SELECT content_enabled FROM sources WHERE source_id = ?1",
            params![source.0],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if enabled == 0 {
        return Ok(());
    }
    enqueue_conn(
        conn,
        &NewJob {
            source_id: source,
            object_id: Some(object),
            object_generation: generation,
            stage: JobStage::ContentText,
            priority: priority_for_size(size),
            idempotency_key: NewJob::object_key(JobStage::ContentText, object, generation),
            payload: None,
            estimated_cost: size,
        },
    )?;
    Ok(())
}
