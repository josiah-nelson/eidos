//! Source sync ledger (ADR-0015).
//!
//! The ledger lives beside the catalog rows it describes and is stamped in
//! the same writer transaction, so a shipped batch can certify that every
//! change through its `through_seq` is represented. It records **touches,
//! not images**: per object, the per-source sequence at which the object was
//! last changed, its generation, and whether that change deleted it. Row
//! images are materialized from the live catalog at ship time
//! ([`Catalog::sync_rows_after`]) inside one read transaction, which is why
//! "objects touched after the consumer's watermark" is exactly the coalesced
//! batch the protocol wants and no change log has to exist on disk.
//!
//! Stamping sites:
//! - every notification-outbox append ([`crate::jobs::outbox_append_conn`]),
//!   which covers incremental change application, archive publication, and
//!   content-state flips;
//! - the scan session, which does not use the outbox: object insert, object
//!   change, entry relink, and the publish-time "not re-observed" tombstones.
//!
//! Storage is O(live objects + unacknowledged tombstones) per source.
//! Deletion rows are collected only below the oldest consumer watermark.

use crate::model::{ObjectRecord, OBJECT_COLUMNS};
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{ObjectId, SourceId, UnixNanos};
use eidos_sync::identity::{chain_next, SourceEpoch};
pub use eidos_sync::identity::{ChainHash, CHAIN_GENESIS};
use eidos_sync::merkle::{leaf_index, RecordDigest};
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fmt;

const MAX_RETAINED_CHAIN_ROWS: i64 = 1_000_000;
const CHAIN_PRUNE_BATCH: i64 = 100_000;

/// Version of the row image produced by [`Catalog::sync_rows_after`].
pub const SYNC_ROW_IMAGE_VERSION: u32 = 1;

/// The history chain hashes touches, not images: the sequence of
/// `(object, generation, live-or-tombstone)` a source minted. Two
/// incarnations that stamped the same touches in the same order share a
/// chain, which is exactly when their histories are interchangeable; a
/// source restored to an older state and edited again diverges at the first
/// differing touch. The image itself is materialized at ship time and is
/// not part of the chain (ADR-0023).
fn chain_after(
    previous: &ChainHash,
    object: ObjectId,
    generation: u64,
    deleted: bool,
) -> ChainHash {
    chain_next(
        previous,
        object.0,
        generation,
        if deleted { None } else { Some(&[]) },
    )
}

/// The Merkle record of one ledger row, computed identically on the source
/// and on a replica so divergent leaves can be found without images.
pub fn record_digest(object: ObjectId, generation: u64, deleted: bool) -> RecordDigest {
    RecordDigest::from_value(object, generation, if deleted { None } else { Some(&[]) })
}

pub(crate) fn register_merkle_leaf_function(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "eidos_merkle_leaf",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let leaf_bits = ctx.get::<i64>(0)? as u8;
            let object = ObjectId(ctx.get::<i64>(1)?);
            Ok(leaf_index(leaf_bits, object) as i64)
        },
    )?;
    Ok(())
}

/// One `(object, generation, deleted)` triple of a source's retained rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncLedgerEntry {
    pub object: ObjectId,
    pub seq: u64,
    pub generation: u64,
    pub deleted: bool,
}

/// Rows and age a consumer has not acknowledged yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncBacklog {
    pub rows: u64,
    pub tombstones: u64,
    /// Oldest unacknowledged touch, when there is one.
    pub oldest_touched_at: Option<UnixNanos>,
}

/// Sixteen-byte source incarnation token. Consumers key their cursor to it;
/// a different epoch means "full snapshot, not resume".
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncEpoch(pub [u8; 16]);

impl SyncEpoch {
    pub fn to_source_epoch(self) -> SourceEpoch {
        SourceEpoch::from_bytes(self.0)
    }

    pub fn from_source_epoch(epoch: SourceEpoch) -> Self {
        Self(epoch.as_bytes())
    }

    fn mint() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|e| CatalogError::InvalidState(format!("epoch entropy unavailable: {e}")))?;
        // UUID v4 shape so it prints and compares like the sync crate's epochs.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for SyncEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SyncEpoch({self})")
    }
}

impl fmt::Display for SyncEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Ledger state of one sync-enabled source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSourceState {
    pub source_id: SourceId,
    pub epoch: SyncEpoch,
    /// Highest sequence minted for this epoch.
    pub head_seq: u64,
    /// History chain hash at `head_seq`.
    pub head_chain: ChainHash,
    /// Deletion rows at or below this sequence may have been collected; a
    /// consumer whose cursor is below it must take the repair path.
    pub compacted_through: u64,
    /// Native change-journal identity the epoch was minted against, when the
    /// source has one.
    pub journal_id: Option<i64>,
    /// `false` until the backfill of pre-existing live objects has finished;
    /// nothing may be shipped before then.
    pub ready: bool,
    pub backfill_after: ObjectId,
}

/// Progress of one bounded backfill step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillProgress {
    pub stamped: u64,
    pub done: bool,
}

/// A live entry of an object as shipped: the parent object and the name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncEntryImage {
    pub parent: Option<ObjectId>,
    pub name: String,
    pub is_virtual: bool,
}

/// Versioned materialized image of one object (`sync-row/1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRowImage {
    pub version: u32,
    pub object: ObjectRecord,
    pub archive_container: Option<ObjectId>,
    pub entries: Vec<SyncEntryImage>,
}

/// One ledger row as shipped: the final image (or a tombstone) for an object
/// touched after the requested cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRow {
    pub seq: u64,
    pub object: ObjectId,
    pub generation: u64,
    /// `None` is a generation-bearing tombstone.
    pub image: Option<SyncRowImage>,
}

/// Rows touched in `(after_seq, through_seq]`, read in one snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncBatch {
    pub source_id: SourceId,
    pub epoch: SyncEpoch,
    pub after_seq: u64,
    /// History chain at `after_seq`; the consumer applies the batch only if
    /// this equals the chain it certified for its own cursor.
    pub after_chain: ChainHash,
    pub through_seq: u64,
    pub through_chain: ChainHash,
    /// Head at the time of the read; `through_seq < head_seq` means the
    /// batch was cut by `limit` and more rows follow.
    pub head_seq: u64,
    pub rows: Vec<SyncRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConsumer {
    pub consumer_id: [u8; 16],
    pub watermark: u64,
    pub updated_at: UnixNanos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CollectStats {
    pub removed_tombstones: u64,
    pub compacted_through: u64,
    /// Deletion rows still retained at or below the floor because `limit`
    /// cut the pass short.
    pub remaining_below_floor: u64,
}

const EPOCH_LEN: usize = 16;

fn epoch_from_blob(blob: Vec<u8>) -> Result<SyncEpoch> {
    let bytes: [u8; EPOCH_LEN] = blob
        .try_into()
        .map_err(|_| CatalogError::InvalidState("sync epoch is not 16 bytes".into()))?;
    Ok(SyncEpoch(bytes))
}

fn chain_from_blob(blob: Vec<u8>) -> Result<ChainHash> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState("sync chain hash is not 32 bytes".into()))
}

fn source_state_conn(conn: &Connection, source: SourceId) -> Result<Option<SyncSourceState>> {
    conn.query_row(
        "SELECT epoch, head_seq, compacted_through, journal_id, ready, backfill_after, head_chain
         FROM sync_sources WHERE source_id = ?1",
        params![source.0],
        |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Vec<u8>>(6)?,
            ))
        },
    )
    .optional()?
    .map(|(epoch, head, compacted, journal, ready, after, chain)| {
        Ok(SyncSourceState {
            source_id: source,
            epoch: epoch_from_blob(epoch)?,
            head_seq: head as u64,
            head_chain: chain_from_blob(chain)?,
            compacted_through: compacted as u64,
            journal_id: journal,
            ready: ready != 0,
            backfill_after: ObjectId(after),
        })
    })
    .transpose()
}

/// Head sequence and chain of a sync-enabled source, or `None` when the
/// source is not sync-enabled (which makes every stamping site a cheap
/// no-op there).
fn head_conn(conn: &Connection, source: SourceId) -> Result<Option<(i64, ChainHash)>> {
    conn.prepare_cached("SELECT head_seq, head_chain FROM sync_sources WHERE source_id = ?1")?
        .query_row(params![source.0], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .optional()?
        .map(|(seq, chain)| Ok((seq, chain_from_blob(chain)?)))
        .transpose()
}

fn set_head_conn(conn: &Connection, source: SourceId, seq: i64, chain: &ChainHash) -> Result<()> {
    conn.prepare_cached(
        "UPDATE sync_sources SET head_seq = ?2, head_chain = ?3, updated_at = ?4 WHERE source_id = ?1",
    )?
    .execute(params![source.0, seq, chain.as_slice(), UnixNanos::now().0])?;
    Ok(())
}

fn insert_chain_conn(
    conn: &Connection,
    source: SourceId,
    seq: i64,
    chain: &ChainHash,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO sync_chain (source_id, seq, chain) VALUES (?1, ?2, ?3)
         ON CONFLICT(source_id, seq) DO UPDATE SET chain = excluded.chain",
    )?
    .execute(params![source.0, seq, chain.as_slice()])?;
    // Retention is part of stamping, not optional caller maintenance. Keep
    // the exact floor chain so a consumer at `compacted_through` can prove
    // its next batch, and prune older history in a bounded step.
    let retention_floor = seq.saturating_sub(MAX_RETAINED_CHAIN_ROWS - 1);
    if retention_floor > 0 {
        conn.prepare_cached(
            "UPDATE sync_sources
             SET compacted_through = MAX(compacted_through, ?2), updated_at = ?3
             WHERE source_id = ?1",
        )?
        .execute(params![source.0, retention_floor, UnixNanos::now().0])?;
        conn.prepare_cached(
            "DELETE FROM sync_chain WHERE (source_id, seq) IN (
                SELECT source_id, seq FROM sync_chain
                WHERE source_id = ?1 AND seq < ?2 ORDER BY seq LIMIT ?3)",
        )?
        .execute(params![source.0, retention_floor, CHAIN_PRUNE_BATCH])?;
    }
    Ok(())
}

/// Stamp one object with the next sequence. Generation and deletion state
/// are read from the object row, so callers stamp after their own update.
pub(crate) fn touch_conn(conn: &Connection, source: SourceId, object: ObjectId) -> Result<bool> {
    let Some((head, head_chain)) = head_conn(conn, source)? else {
        return Ok(false);
    };
    let Some((generation, deleted)) = conn
        .prepare_cached(
            "SELECT generation, deleted_at IS NOT NULL FROM objects WHERE object_id = ?2 AND source_id = ?1",
        )?
        .query_row(params![source.0, object.0], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0))
        })
        .optional()?
    else {
        return Ok(false);
    };
    let seq = head + 1;
    let chain = chain_after(&head_chain, object, generation as u64, deleted);
    conn.prepare_cached(
        "INSERT INTO sync_rows (source_id, object_id, seq, generation, deleted, touched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(source_id, object_id) DO UPDATE SET
            seq = excluded.seq, generation = excluded.generation, deleted = excluded.deleted,
            touched_at = excluded.touched_at",
    )?
    .execute(params![
        source.0,
        object.0,
        seq,
        generation,
        deleted as i64,
        UnixNanos::now().0
    ])?;
    insert_chain_conn(conn, source, seq, &chain)?;
    set_head_conn(conn, source, seq, &chain)?;
    Ok(true)
}

/// Stamp a container and every object it owns (archive members). For a
/// plain directory the member query is empty and only the directory is
/// touched: descendants' own rows did not change when their ancestor moved.
pub(crate) fn touch_subtree_conn(
    conn: &Connection,
    source: SourceId,
    container: ObjectId,
) -> Result<u64> {
    touch_set_conn(
        conn,
        source,
        "(object_id = ?2 OR archive_container_id = ?2)",
        &[container.0],
    )
}

/// Stamp every object of `source` matching `predicate`, assigning
/// consecutive sequences in object-id order. `?1` is the source id and
/// `?2..` are `args`; the helper appends its own base-sequence parameter.
/// Used for set-shaped changes (publish-time tombstones, archive member
/// replacement, backfill) where a per-row round trip would be O(n)
/// statements. No-op for a source that is not sync-enabled.
fn touch_set_conn(
    conn: &Connection,
    source: SourceId,
    predicate: &str,
    args: &[i64],
) -> Result<u64> {
    let Some((head, head_chain)) = head_conn(conn, source)? else {
        return Ok(0);
    };
    let mut bound: Vec<i64> = Vec::with_capacity(args.len() + 3);
    bound.push(source.0);
    bound.extend_from_slice(args);
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM objects WHERE source_id = ?1 AND {predicate}"),
        rusqlite::params_from_iter(bound.iter()),
        |r| r.get(0),
    )?;
    if count == 0 {
        return Ok(0);
    }
    let base = head;
    let now = UnixNanos::now().0;
    let base_param = bound.len() + 1;
    bound.push(base);
    let now_param = bound.len() + 1;
    bound.push(now);
    let n = conn.execute(
        &format!(
            "INSERT INTO sync_rows (source_id, object_id, seq, generation, deleted, touched_at)
             SELECT ?1, object_id, ?{base_param} + ROW_NUMBER() OVER (ORDER BY object_id), generation,
                    deleted_at IS NOT NULL, ?{now_param}
             FROM objects WHERE source_id = ?1 AND {predicate}
             ON CONFLICT(source_id, object_id) DO UPDATE SET
                seq = excluded.seq, generation = excluded.generation, deleted = excluded.deleted,
                touched_at = excluded.touched_at"
        ),
        rusqlite::params_from_iter(bound.iter()),
    )?;
    // The chain is sequential by definition, so the rows just minted are
    // walked once in sequence order to extend it.
    let minted: Vec<(i64, i64, i64, bool)> = conn
        .prepare_cached(
            "SELECT seq, object_id, generation, deleted FROM sync_rows
             WHERE source_id = ?1 AND seq > ?2 ORDER BY seq",
        )?
        .query_map(params![source.0, base], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut chain = head_chain;
    let mut last_seq = base;
    for (seq, object, generation, deleted) in minted {
        chain = chain_after(&chain, ObjectId(object), generation as u64, deleted);
        insert_chain_conn(conn, source, seq, &chain)?;
        last_seq = seq;
    }
    debug_assert_eq!(last_seq, base + count);
    set_head_conn(conn, source, last_seq, &chain)?;
    Ok(n as u64)
}

/// Stamp every object the scan publish step tombstoned at `now`. Called
/// inside the publish transaction after the cascade has settled.
pub(crate) fn stamp_publish_tombstones_conn(
    conn: &Connection,
    source: SourceId,
    now: i64,
) -> Result<u64> {
    touch_set_conn(conn, source, "deleted_at = ?2", &[now])
}

fn entries_conn(conn: &Connection, object: ObjectId) -> Result<Vec<SyncEntryImage>> {
    Ok(conn
        .prepare_cached(
            "SELECT parent_id, name, is_virtual FROM entries
             WHERE object_id = ?1 AND deleted_at IS NULL ORDER BY entry_id",
        )?
        .query_map(params![object.0], |r| {
            Ok(SyncEntryImage {
                parent: r.get::<_, Option<i64>>(0)?.map(ObjectId),
                name: r.get(1)?,
                is_virtual: r.get::<_, i64>(2)? != 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn chain_at_conn(conn: &Connection, source: SourceId, seq: u64) -> Result<Option<ChainHash>> {
    conn.prepare_cached("SELECT chain FROM sync_chain WHERE source_id = ?1 AND seq = ?2")?
        .query_row(params![source.0, seq as i64], |r| r.get::<_, Vec<u8>>(0))
        .optional()?
        .map(chain_from_blob)
        .transpose()
}

/// Materialize `(seq, object, generation, deleted)` ledger rows into their
/// versioned images from the live catalog tables, inside the caller's
/// snapshot.
fn materialize_conn(
    conn: &Connection,
    touched: Vec<(i64, i64, i64, bool)>,
) -> Result<Vec<SyncRow>> {
    let mut rows = Vec::with_capacity(touched.len());
    let mut object_stmt = conn.prepare_cached(&format!(
        "SELECT {OBJECT_COLUMNS}, o.archive_container_id FROM objects o WHERE o.object_id = ?1"
    ))?;
    for (seq, object_id, generation, deleted) in touched {
        let object = ObjectId(object_id);
        let image = if deleted {
            None
        } else {
            let (record, container) = object_stmt.query_row(params![object_id], |r| {
                Ok((
                    ObjectRecord::from_row_at(r, 0)?,
                    r.get::<_, Option<i64>>(22)?.map(ObjectId),
                ))
            })?;
            Some(SyncRowImage {
                version: SYNC_ROW_IMAGE_VERSION,
                entries: entries_conn(conn, object)?,
                object: record,
                archive_container: container,
            })
        };
        rows.push(SyncRow {
            seq: seq as u64,
            object,
            generation: generation as u64,
            image,
        });
    }
    Ok(rows)
}

fn floor_conn(conn: &Connection, source: SourceId, head: u64) -> Result<u64> {
    // With no registered consumer nothing is waiting on history, so the
    // floor is the head; a consumer that registers later starts from a
    // snapshot, exactly as one whose cursor fell below `compacted_through`.
    let min: Option<i64> = conn.query_row(
        "SELECT MIN(watermark) FROM sync_consumers WHERE source_id = ?1",
        params![source.0],
        |r| r.get(0),
    )?;
    Ok(min.map(|m| m as u64).unwrap_or(head))
}

impl Catalog {
    /// Enable sync for a source, minting a fresh epoch. Re-enabling an
    /// already enabled source is also an epoch change: it discards the
    /// previous ledger and consumer watermarks and restarts the backfill.
    pub fn sync_enable(
        &self,
        source: SourceId,
        journal_id: Option<i64>,
    ) -> Result<SyncSourceState> {
        let epoch = SyncEpoch::mint()?;
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let kind: Option<String> = tx
                .query_row(
                    "SELECT kind FROM sources WHERE source_id = ?1",
                    params![source.0],
                    |r| r.get(0),
                )
                .optional()?;
            match kind.as_deref() {
                None => return Err(CatalogError::NotFound(format!("source {source}"))),
                Some("remote") => {
                    return Err(CatalogError::InvalidState(
                        "a replicated source cannot be sync-enabled as a shipper".into(),
                    ))
                }
                Some(_) => {}
            }
            let now = UnixNanos::now().0;
            tx.execute(
                "DELETE FROM sync_rows WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.execute(
                "DELETE FROM sync_consumers WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.execute(
                "DELETE FROM sync_chain WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.execute(
                "INSERT INTO sync_sources (source_id, epoch, head_seq, head_chain, compacted_through,
                    journal_id, backfill_after, ready, created_at, updated_at)
                 VALUES (?1, ?2, 0, ?5, 0, ?3, 0, 0, ?4, ?4)
                 ON CONFLICT(source_id) DO UPDATE SET epoch = excluded.epoch, head_seq = 0,
                    head_chain = excluded.head_chain, compacted_through = 0,
                    journal_id = excluded.journal_id, backfill_after = 0,
                    ready = 0, updated_at = excluded.updated_at",
                params![
                    source.0,
                    epoch.0.as_slice(),
                    journal_id,
                    now,
                    CHAIN_GENESIS.as_slice()
                ],
            )?;
            insert_chain_conn(&tx, source, 0, &CHAIN_GENESIS)?;
            let state = source_state_conn(&tx, source)?.expect("just inserted");
            tx.commit()?;
            Ok(state)
        })
    }

    /// Remove a source's ledger. Re-enabling later mints a new epoch.
    pub fn sync_disable(&self, source: SourceId) -> Result<bool> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "DELETE FROM sync_rows WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.execute(
                "DELETE FROM sync_consumers WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.execute(
                "DELETE FROM sync_chain WHERE source_id = ?1",
                params![source.0],
            )?;
            let n = tx.execute(
                "DELETE FROM sync_sources WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.commit()?;
            Ok(n > 0)
        })
    }

    pub fn sync_source(&self, source: SourceId) -> Result<Option<SyncSourceState>> {
        self.with_reader(|conn| source_state_conn(conn, source))
    }

    pub fn sync_sources(&self) -> Result<Vec<SyncSourceState>> {
        self.with_reader(|conn| {
            let ids: Vec<i64> = conn
                .prepare("SELECT source_id FROM sync_sources ORDER BY source_id")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            ids.into_iter()
                .filter_map(|id| source_state_conn(conn, SourceId(id)).transpose())
                .collect()
        })
    }

    /// Stamp up to `batch` pre-existing live objects, resuming from the
    /// stored cursor. Objects the feed touches meanwhile are stamped by the
    /// ordinary paths; the backfill simply re-stamps any it reaches later.
    /// Returns `done = true` once every live object has a ledger row.
    pub fn sync_backfill(&self, source: SourceId, batch: u32) -> Result<BackfillProgress> {
        let batch = batch.max(1) as i64;
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state = source_state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not sync-enabled"))
            })?;
            if state.ready {
                return Ok(BackfillProgress {
                    stamped: 0,
                    done: true,
                });
            }
            let last: Option<i64> = tx
                .query_row(
                    "SELECT MAX(object_id) FROM (
                        SELECT object_id FROM objects
                        WHERE source_id = ?1 AND deleted_at IS NULL AND object_id > ?2
                        ORDER BY object_id LIMIT ?3)",
                    params![source.0, state.backfill_after.0, batch],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let progress = match last {
                None => {
                    tx.execute(
                        "UPDATE sync_sources SET ready = 1, updated_at = ?2 WHERE source_id = ?1",
                        params![source.0, UnixNanos::now().0],
                    )?;
                    BackfillProgress {
                        stamped: 0,
                        done: true,
                    }
                }
                Some(last) => {
                    let stamped = touch_set_conn(
                        &tx,
                        source,
                        "deleted_at IS NULL AND object_id > ?2 AND object_id <= ?3",
                        &[state.backfill_after.0, last],
                    )?;
                    let done = (stamped as i64) < batch;
                    tx.execute(
                        "UPDATE sync_sources SET backfill_after = ?2, ready = ?3, updated_at = ?4
                         WHERE source_id = ?1",
                        params![source.0, last, done as i64, UnixNanos::now().0],
                    )?;
                    BackfillProgress { stamped, done }
                }
            };
            tx.commit()?;
            Ok(progress)
        })
    }

    /// Materialize the rows touched after `after_seq`, at most `limit` of
    /// them in sequence order, from one read snapshot. The result certifies
    /// every change in `(after_seq, through_seq]` by final image.
    pub fn sync_rows_after(
        &self,
        source: SourceId,
        after_seq: u64,
        limit: u32,
    ) -> Result<SyncBatch> {
        let limit = limit.max(1);
        self.with_reader(|conn| {
            let tx = conn.unchecked_transaction()?;
            let state = source_state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not sync-enabled"))
            })?;
            if !state.ready {
                return Err(CatalogError::InvalidState(format!(
                    "source {source} sync backfill has not finished"
                )));
            }
            if after_seq < state.compacted_through {
                return Err(CatalogError::InvalidState(format!(
                    "cursor {after_seq} predates retained history (compacted through {})",
                    state.compacted_through
                )));
            }
            let touched: Vec<(i64, i64, i64, bool)> = tx
                .prepare_cached(
                    "SELECT seq, object_id, generation, deleted FROM sync_rows
                     WHERE source_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )?
                .query_map(params![source.0, after_seq as i64, limit as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let rows = materialize_conn(&tx, touched)?;
            let through_seq = match rows.last() {
                Some(last) if rows.len() as u32 >= limit => last.seq,
                _ => state.head_seq,
            };
            let after_chain = chain_at_conn(&tx, source, after_seq)?.ok_or_else(|| {
                CatalogError::InvalidState(format!(
                    "no history chain retained at cursor {after_seq}"
                ))
            })?;
            let through_chain = if through_seq == state.head_seq {
                state.head_chain
            } else {
                chain_at_conn(&tx, source, through_seq)?.ok_or_else(|| {
                    CatalogError::InvalidState(format!(
                        "no history chain retained at sequence {through_seq}"
                    ))
                })?
            };
            tx.commit()?;
            Ok(SyncBatch {
                source_id: source,
                epoch: state.epoch,
                after_seq,
                after_chain,
                through_seq,
                through_chain,
                head_seq: state.head_seq,
                rows,
            })
        })
    }

    /// History chain at `seq`, if that point of history is still retained.
    pub fn sync_chain_at(&self, source: SourceId, seq: u64) -> Result<Option<ChainHash>> {
        self.with_reader(|conn| chain_at_conn(conn, source, seq))
    }

    /// Every retained ledger row of a source as `(object, seq, generation,
    /// deleted)`, in object order: the input to a Merkle tree over the
    /// source image, read in one snapshot together with the head it
    /// describes.
    pub fn sync_ledger_entries(
        &self,
        source: SourceId,
    ) -> Result<(SyncSourceState, Vec<SyncLedgerEntry>)> {
        self.with_reader(|conn| {
            let tx = conn.unchecked_transaction()?;
            let state = source_state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not sync-enabled"))
            })?;
            let entries = tx
                .prepare_cached(
                    "SELECT object_id, seq, generation, deleted FROM sync_rows
                     WHERE source_id = ?1 ORDER BY object_id",
                )?
                .query_map(params![source.0], |r| {
                    Ok(SyncLedgerEntry {
                        object: ObjectId(r.get(0)?),
                        seq: r.get::<_, i64>(1)? as u64,
                        generation: r.get::<_, i64>(2)? as u64,
                        deleted: r.get::<_, i64>(3)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            tx.commit()?;
            Ok((state, entries))
        })
    }

    /// Materialize the current rows of specific objects (a Merkle repair
    /// answer), together with the head the images belong to. Objects that
    /// have no ledger row are absent from the result: at the source they do
    /// not exist, which the consumer treats as authoritative absence.
    pub fn sync_rows_for_objects(
        &self,
        source: SourceId,
        objects: &[ObjectId],
    ) -> Result<(SyncSourceState, Vec<SyncRow>)> {
        self.with_reader(|conn| {
            let tx = conn.unchecked_transaction()?;
            let state = source_state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not sync-enabled"))
            })?;
            let mut touched = Vec::with_capacity(objects.len());
            {
                let mut stmt = tx.prepare_cached(
                    "SELECT seq, object_id, generation, deleted FROM sync_rows
                     WHERE source_id = ?1 AND object_id = ?2",
                )?;
                for object in objects {
                    if let Some(row) = stmt
                        .query_row(params![source.0, object.0], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
                        })
                        .optional()?
                    {
                        touched.push(row);
                    }
                }
            }
            let rows = materialize_conn(&tx, touched)?;
            tx.commit()?;
            Ok((state, rows))
        })
    }

    /// Rows a consumer has not acknowledged, and how old the oldest is.
    pub fn sync_backlog(&self, source: SourceId, consumer: [u8; 16]) -> Result<SyncBacklog> {
        self.with_reader(|conn| {
            let watermark: i64 = conn
                .query_row(
                    "SELECT watermark FROM sync_consumers WHERE source_id = ?1 AND consumer_id = ?2",
                    params![source.0, consumer.as_slice()],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(deleted), 0), MIN(touched_at) FROM sync_rows
                 WHERE source_id = ?1 AND seq > ?2",
                params![source.0, watermark],
                |r| {
                    Ok(SyncBacklog {
                        rows: r.get::<_, i64>(0)? as u64,
                        tombstones: r.get::<_, i64>(1)? as u64,
                        oldest_touched_at: r
                            .get::<_, Option<i64>>(2)?
                            .filter(|t| *t > 0)
                            .map(UnixNanos),
                    })
                },
            )
            .map_err(Into::into)
        })
    }

    /// Record that `consumer` durably applied everything through
    /// `through_seq`. Watermarks never move backwards; a rewind is ignored
    /// and reported as `false`.
    pub fn sync_acknowledge(
        &self,
        source: SourceId,
        epoch: SyncEpoch,
        consumer: [u8; 16],
        through_seq: u64,
    ) -> Result<bool> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state = source_state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not sync-enabled"))
            })?;
            if epoch != state.epoch {
                return Err(CatalogError::InvalidState(format!(
                    "acknowledgement is for retired epoch {epoch}; active epoch is {}",
                    state.epoch
                )));
            }
            if through_seq > state.head_seq {
                return Err(CatalogError::InvalidState(format!(
                    "acknowledgement {through_seq} is beyond head {}",
                    state.head_seq
                )));
            }
            let now = UnixNanos::now().0;
            let advanced = tx.execute(
                "INSERT INTO sync_consumers (source_id, consumer_id, watermark, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source_id, consumer_id) DO UPDATE SET
                    watermark = excluded.watermark, updated_at = excluded.updated_at
                    WHERE excluded.watermark > sync_consumers.watermark",
                params![source.0, consumer.as_slice(), through_seq as i64, now],
            )?;
            tx.commit()?;
            Ok(advanced > 0)
        })
    }

    pub fn sync_consumers(&self, source: SourceId) -> Result<Vec<SyncConsumer>> {
        self.with_reader(|conn| {
            conn.prepare_cached(
                "SELECT consumer_id, watermark, updated_at FROM sync_consumers
                 WHERE source_id = ?1 ORDER BY consumer_id",
            )?
            .query_map(params![source.0], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .map(|row| {
                let (id, watermark, updated_at) = row?;
                let consumer_id: [u8; 16] = id.try_into().map_err(|_| {
                    CatalogError::InvalidState("consumer id is not 16 bytes".into())
                })?;
                Ok(SyncConsumer {
                    consumer_id,
                    watermark: watermark as u64,
                    updated_at: UnixNanos(updated_at),
                })
            })
            .collect()
        })
    }

    /// Release deletion rows every consumer has crossed, at most `limit` per
    /// call, and advance `compacted_through` once none remain below the
    /// floor. Live rows are never collected: they are the source image.
    pub fn sync_collect(&self, source: SourceId, limit: u32) -> Result<CollectStats> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state = source_state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not sync-enabled"))
            })?;
            let consumer_floor = floor_conn(&tx, source, state.head_seq)?;
            let retention_floor = tx
                .query_row(
                    "SELECT seq FROM sync_chain WHERE source_id = ?1
                     ORDER BY seq DESC LIMIT 1 OFFSET ?2",
                    params![source.0, MAX_RETAINED_CHAIN_ROWS - 1],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0) as u64;
            let floor = consumer_floor.max(retention_floor);
            let removed = tx.execute(
                "DELETE FROM sync_rows WHERE (source_id, object_id) IN (
                    SELECT source_id, object_id FROM sync_rows
                    WHERE source_id = ?1 AND deleted = 1 AND seq <= ?2
                    ORDER BY seq LIMIT ?3)",
                params![source.0, floor as i64, limit.max(1) as i64],
            )? as u64;
            let remaining: i64 = tx.query_row(
                "SELECT COUNT(*) FROM sync_rows WHERE source_id = ?1 AND deleted = 1 AND seq <= ?2",
                params![source.0, floor as i64],
                |r| r.get(0),
            )?;
            let mut compacted_through = state.compacted_through;
            if remaining == 0 && floor > compacted_through {
                tx.execute(
                    "UPDATE sync_sources SET compacted_through = ?2, updated_at = ?3 WHERE source_id = ?1",
                    params![source.0, floor as i64, UnixNanos::now().0],
                )?;
                compacted_through = floor;
            }
            // The chain at the floor itself stays: a consumer exactly there
            // resumes with it as its `after_chain`. Physical reclamation is
            // bounded so a long-offline consumer cannot create one giant
            // writer transaction when the retention cap advances the floor.
            tx.execute(
                "DELETE FROM sync_chain WHERE (source_id, seq) IN (
                    SELECT source_id, seq FROM sync_chain
                    WHERE source_id = ?1 AND seq < ?2 ORDER BY seq LIMIT ?3)",
                params![source.0, compacted_through as i64, CHAIN_PRUNE_BATCH],
            )?;
            tx.commit()?;
            Ok(CollectStats {
                removed_tombstones: removed,
                compacted_through,
                remaining_below_floor: remaining as u64,
            })
        })
    }
}
