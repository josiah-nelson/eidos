//! Central-side replica of a peer's sources (ADR-0023).
//!
//! A replicated source is an ordinary `sources` row of kind `remote` whose
//! `objects` and `entries` are applied from the peer's materialized batches
//! (`sync-row/1` images, [`crate::sync::SyncRow`]). Nothing on this host
//! scans, watches, or extracts content for it; projection, search and browse
//! treat it like any other source, which is what makes central search the
//! union of the participating catalogs.
//!
//! Every admission decision and every apply runs inside one writer
//! transaction that also commits the durable cursor
//! ([`eidos_sync::identity::AdmissionState`]: epoch, applied sequence and
//! history chain). The caller sends the acknowledgement only after the
//! transaction returns, so an acknowledged sequence is always durable
//! (ADR-0013). Decisions are re-taken inside the transaction from the stored
//! cursor, never from state the caller remembered.
//!
//! Remote object ids are mapped to local ids through `sync_replica_rows`.
//! A batch cut by the source's row limit can carry a child before its
//! parent (the parent was touched again later and therefore has a later
//! sequence); the parent's local object is allocated as a *placeholder*
//! without entries and filled in when its own row arrives, at which point
//! the subtree is re-projected.

use crate::jobs::outbox_append_conn;
use crate::sync::{
    image_hash_from_blob, record_digest, register_merkle_leaf_function, sync_row_image_hash,
    SyncBatch, SyncEpoch, SyncRow, SyncRowImage, SYNC_ROW_IMAGE_VERSION,
};
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{
    extension_of, ContentState, HostId, ObjectId, ObjectKind, SourceId, SourceKind, SourceState,
    UnixNanos,
};
use eidos_sync::identity::{
    AdmissionState, BatchDecision, ChainHash, HelloDecision, SourceEpoch, CHAIN_GENESIS,
};
use eidos_sync::merkle::{
    leaf_index, MerkleLeafHasher, RecordDigest, MAX_FLEET_LEAF_BITS, MIN_REPAIR_LEAF_BITS,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What a peer says about one of its sources when it offers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSourceDescriptor {
    pub remote_source_id: SourceId,
    pub name: String,
    pub kind: SourceKind,
    pub root_path: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Whether the origin distinguishes names by case.
    #[serde(default)]
    pub case_sensitive: bool,
}

/// The peer a replicated source comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNode {
    pub node_id: [u8; 16],
    pub name: String,
    pub platform: String,
}

/// Durable state of one replicated source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaSourceState {
    /// The local `sources` row.
    pub source_id: SourceId,
    pub node_id: [u8; 16],
    pub remote_source_id: SourceId,
    pub admission: AdmissionState,
    /// While an epoch change is being streamed: the head the new epoch must
    /// reach before rows of the previous epoch are known to be absent.
    pub resync_target: Option<u64>,
    pub reported_head: u64,
    /// Chain hash most recently authenticated for `reported_head`. A batch
    /// can discover a newer numeric head before carrying its final chain, in
    /// which case this is temporarily `None` and repair stays fenced.
    pub reported_chain: Option<ChainHash>,
    pub reported_compacted: u64,
    /// Retained-image revision most recently authenticated by Hello.
    pub reported_image_revision: u64,
    /// Retained-image revision the replica has reconciled by repair.
    pub applied_image_revision: u64,
    pub reported_at: Option<UnixNanos>,
    pub applied_at: Option<UnixNanos>,
    pub image_version: u32,
}

/// Outcome of offering a source (the protocol's `Hello`), already durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloOutcome {
    /// Resume after `after_seq`; `requires_repair` when the source no longer
    /// retains that point of history.
    Resume {
        epoch: SourceEpoch,
        after_seq: u64,
        requires_repair: bool,
    },
    /// The epoch changed (or the cursor is new): stream the source from
    /// sequence zero of `epoch`.
    FullResync { epoch: SourceEpoch },
    /// The current epoch can no longer prove continuity from the replica's
    /// durable cursor. The shipper must mint a fresh epoch before retrying.
    NewEpochRequired { reason: String },
    /// Fenced; the reason is safe to send to the peer.
    Rejected { reason: String },
}

/// Outcome of applying a batch, already durable when `Applied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    Applied {
        through_seq: u64,
        rows: u64,
        /// Rows of the previous epoch tombstoned because the new epoch's
        /// stream reached its target.
        retired_rows: u64,
    },
    AlreadyApplied {
        applied_seq: u64,
    },
    /// Ask for an exact resume from `applied_seq`.
    Stale {
        applied_seq: u64,
    },
    FullResyncRequired {
        epoch: SourceEpoch,
    },
    Rejected {
        reason: String,
    },
}

/// Outcome of applying a Merkle repair answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairOutcome {
    Staged {
        replaced: u64,
        removed: u64,
        remaining_leaves: u64,
    },
    Applied {
        through_seq: u64,
        replaced: u64,
        removed: u64,
    },
    Rejected {
        reason: String,
    },
}

/// A repair request the replica wants answered: the leaves whose hashes
/// differ from the offered manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairOfferOutcome {
    Request { leaf_bits: u8, leaves: Vec<u32> },
    Rejected { reason: String },
}

/// Coverage facts of a replicated source for search responses and status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaCoverage {
    pub node_id: [u8; 16],
    pub node_name: String,
    pub remote_source_id: SourceId,
    pub epoch: SourceEpoch,
    pub applied_seq: u64,
    pub reported_head: u64,
    pub applied_at: Option<UnixNanos>,
    pub reported_at: Option<UnixNanos>,
    pub resyncing: bool,
    pub alarms: u64,
    /// A session with the origin node is open.
    pub connected: bool,
}

const REMOTE_GENERATION: i64 = 1;
const MAX_SQLITE_SEQUENCE: u64 = i64::MAX as u64;
const RETIRE_STEP_ROWS: usize = 10_000;
const MAX_APPLY_ROWS: usize = 10_000;
const MAX_APPLY_ENTRIES: usize = 100_000;

fn encode_leaf_hashes(hashes: &[[u8; 32]]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(hashes.len() * 32);
    for hash in hashes {
        encoded.extend_from_slice(hash);
    }
    encoded
}

fn decode_leaf_hashes(encoded: Vec<u8>) -> Result<Vec<[u8; 32]>> {
    let (hashes, remainder) = encoded.as_chunks::<32>();
    if !remainder.is_empty() {
        return Err(CatalogError::InvalidState(
            "repair leaf hashes are not a sequence of 32-byte digests".into(),
        ));
    }
    Ok(hashes.to_vec())
}

fn admission_from_json(json: &str) -> Result<AdmissionState> {
    serde_json::from_str(json).map_err(|e| {
        CatalogError::InvalidState(format!("replica admission state is unreadable: {e}"))
    })
}

fn admission_to_json(state: &AdmissionState) -> Result<String> {
    Ok(serde_json::to_string(state)?)
}

fn repair_chain_from_blob(blob: Vec<u8>) -> Result<ChainHash> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState("repair chain is not 32 bytes".into()))
}

fn optional_chain_from_blob(blob: Option<Vec<u8>>) -> Result<Option<ChainHash>> {
    blob.map(repair_chain_from_blob).transpose()
}

fn node_id_from_blob(blob: Vec<u8>) -> Result<[u8; 16]> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState("fleet node id is not 16 bytes".into()))
}

fn state_conn(conn: &Connection, source: SourceId) -> Result<Option<ReplicaSourceState>> {
    conn.prepare_cached(
        "SELECT node_id, remote_source_id, admission, resync_target, reported_head,
                reported_chain, reported_compacted, reported_image_revision,
                applied_image_revision, reported_at, applied_at, image_version
         FROM sync_replica_sources WHERE source_id = ?1",
    )?
    .query_row(params![source.0], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<Vec<u8>>>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, Option<i64>>(9)?,
            r.get::<_, Option<i64>>(10)?,
            r.get::<_, i64>(11)?,
        ))
    })
    .optional()?
    .map(
        |(
            node,
            remote,
            admission,
            target,
            head,
            chain,
            compacted,
            reported_revision,
            applied_revision,
            reported,
            applied,
            version,
        )| {
            Ok(ReplicaSourceState {
                source_id: source,
                node_id: node_id_from_blob(node)?,
                remote_source_id: SourceId(remote),
                admission: admission_from_json(&admission)?,
                resync_target: target.map(|t| t as u64),
                reported_head: head as u64,
                reported_chain: optional_chain_from_blob(chain)?,
                reported_compacted: compacted as u64,
                reported_image_revision: reported_revision as u64,
                applied_image_revision: applied_revision as u64,
                reported_at: reported.map(UnixNanos),
                applied_at: applied.map(UnixNanos),
                image_version: version as u32,
            })
        },
    )
    .transpose()
}

fn store_admission_conn(
    conn: &Connection,
    source: SourceId,
    admission: &AdmissionState,
    resync_target: Option<u64>,
) -> Result<()> {
    conn.prepare_cached(
        "UPDATE sync_replica_sources SET admission = ?2, resync_target = ?3, updated_at = ?4
         WHERE source_id = ?1",
    )?
    .execute(params![
        source.0,
        admission_to_json(admission)?,
        resync_target.map(|t| t as i64),
        UnixNanos::now().0
    ])?;
    Ok(())
}

fn ensure_host_conn(
    conn: &Connection,
    node_id: &[u8; 16],
    name: &str,
    platform: &str,
) -> Result<HostId> {
    if let Some(id) = conn
        .query_row(
            "SELECT s.host_id FROM sync_replica_sources r
             JOIN sources s ON s.source_id = r.source_id
             WHERE r.node_id = ?1 LIMIT 1",
            params![node_id.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(HostId(id));
    }
    let suffix: String = node_id.iter().map(|b| format!("{b:02x}")).collect();
    let stored_name = format!("{name}#{suffix}");
    if let Some(id) = conn
        .query_row(
            "SELECT host_id FROM hosts WHERE name = ?1",
            params![stored_name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(HostId(id));
    }
    conn.execute(
        "INSERT INTO hosts (name, platform, created_at) VALUES (?1, ?2, ?3)",
        params![stored_name, platform, UnixNanos::now().0],
    )?;
    Ok(HostId(conn.last_insert_rowid()))
}

/// Local object id for a remote object, allocating a placeholder when the
/// remote object has not been applied yet (a child arrived first).
fn local_object_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
    remote: ObjectId,
) -> Result<ObjectId> {
    if let Some((id, placeholder, stored_epoch)) = tx
        .prepare_cached(
            "SELECT local_object_id, placeholder, epoch FROM sync_replica_rows
             WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .query_row(params![source.0, remote.0], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .optional()?
    {
        if stored_epoch.as_slice() != epoch.0.as_slice() {
            if placeholder {
                tx.prepare_cached(
                    "UPDATE sync_replica_rows SET epoch = ?3
                     WHERE source_id = ?1 AND remote_object_id = ?2",
                )?
                .execute(params![source.0, remote.0, epoch.0.as_slice()])?;
            } else {
                // A new-epoch row may refer to a parent/container before that
                // target's own row arrives. Do not retain the target's stale
                // old-epoch outgoing topology: it can form a transient cycle
                // with the authoritative new relation. Turn the mapping into
                // a current-epoch placeholder so Merkle repair requests the
                // missing row and completeness remains withheld.
                tx.prepare_cached("DELETE FROM entries WHERE object_id = ?1")?
                    .execute(params![id])?;
                tx.prepare_cached(
                    "UPDATE objects SET archive_container_id = NULL, deleted_at = NULL
                     WHERE object_id = ?1",
                )?
                .execute(params![id])?;
                tx.prepare_cached(
                    "UPDATE sync_replica_rows SET epoch = ?3, seq = 0, generation = 0,
                         deleted = 0, image_hash = zeroblob(32), placeholder = 1
                     WHERE source_id = ?1 AND remote_object_id = ?2",
                )?
                .execute(params![source.0, remote.0, epoch.0.as_slice()])?;
                outbox_append_conn(tx, source, ObjectId(id), "subtree", 0)?;
            }
        }
        return Ok(ObjectId(id));
    }
    tx.prepare_cached(
        "INSERT INTO objects (source_id, kind, identity_confidence, content_state, generation,
            first_seen_generation, last_seen_generation)
         VALUES (?1, 'directory', 'path_derived', 'not_applicable', 0, 0, 0)",
    )?
    .execute(params![source.0])?;
    let local = ObjectId(tx.last_insert_rowid());
    tx.prepare_cached(
        "INSERT INTO sync_replica_rows (source_id, remote_object_id, local_object_id, epoch, seq,
            generation, deleted, image_hash, placeholder)
         VALUES (?1, ?2, ?3, ?4, 0, 0, 0, zeroblob(32), 1)",
    )?
    .execute(params![source.0, remote.0, local.0, epoch.0.as_slice()])?;
    Ok(local)
}

struct RowState {
    local: ObjectId,
    epoch: Vec<u8>,
    seq: u64,
    placeholder: bool,
}

fn temporary_remote_id_conn(tx: &Transaction<'_>, source: SourceId) -> Result<ObjectId> {
    let minimum: Option<i64> = tx.query_row(
        "SELECT MIN(remote_object_id) FROM sync_replica_rows WHERE source_id = ?1",
        params![source.0],
        |r| r.get(0),
    )?;
    let remote = match minimum.filter(|remote| *remote < 0) {
        Some(minimum) => minimum.checked_sub(1).ok_or_else(|| {
            CatalogError::InvalidState("temporary replica object ids exhausted".into())
        })?,
        None => -1,
    };
    Ok(ObjectId(remote))
}

fn native_matches_conn(
    tx: &Transaction<'_>,
    local: ObjectId,
    native: eidos_domain::NativeIdentity,
) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM objects WHERE object_id = ?1
               AND native_volume_serial = ?2 AND native_id_high = ?3 AND native_id_low = ?4)",
        params![
            local.0,
            native.volume_serial as i64,
            native.file_id_high as i64,
            native.file_id_low as i64,
        ],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

/// Move references already installed in `epoch` away from a local object
/// whose remote numeric id has been reassigned. The referenced parent or
/// archive container can arrive after its children, so those children must
/// follow the replacement mapping instead of remaining attached to the old
/// native object.
fn reparent_current_epoch_refs_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
    from: ObjectId,
    to: ObjectId,
) -> Result<()> {
    // The new epoch is authoritative. Remove only names which collide with
    // current-epoch references that are about to move to the replacement.
    tx.prepare_cached(
        "DELETE FROM entries
         WHERE source_id = ?1 AND parent_id = ?2 AND deleted_at IS NULL
           AND is_virtual = 0 AND name IN (
               SELECT e.name FROM entries e
               JOIN sync_replica_rows rr ON rr.local_object_id = e.object_id
               WHERE e.source_id = ?1 AND e.parent_id = ?3
                 AND e.deleted_at IS NULL AND e.is_virtual = 0
                 AND rr.source_id = ?1 AND rr.epoch = ?4)",
    )?
    .execute(params![source.0, to.0, from.0, epoch.0.as_slice()])?;
    tx.prepare_cached(
        "UPDATE entries SET parent_id = ?3
         WHERE source_id = ?1 AND parent_id = ?2
           AND object_id IN (
               SELECT local_object_id FROM sync_replica_rows
               WHERE source_id = ?1 AND epoch = ?4)",
    )?
    .execute(params![source.0, from.0, to.0, epoch.0.as_slice()])?;
    tx.prepare_cached(
        "UPDATE objects SET archive_container_id = ?3
         WHERE source_id = ?1 AND archive_container_id = ?2
           AND object_id IN (
               SELECT local_object_id FROM sync_replica_rows
               WHERE source_id = ?1 AND epoch = ?4)",
    )?
    .execute(params![source.0, from.0, to.0, epoch.0.as_slice()])?;
    Ok(())
}

fn row_state_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    remote: ObjectId,
) -> Result<Option<RowState>> {
    Ok(tx
        .prepare_cached(
            "SELECT local_object_id, epoch, seq, placeholder FROM sync_replica_rows
             WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .query_row(params![source.0, remote.0], |r| {
            Ok(RowState {
                local: ObjectId(r.get(0)?),
                epoch: r.get(1)?,
                seq: r.get::<_, i64>(2)? as u64,
                placeholder: r.get::<_, i64>(3)? != 0,
            })
        })
        .optional()?)
}

/// Reuse an old-epoch local object when a rebuilt source changes the remote
/// object id but preserves the filesystem's stable native identity. A child
/// may already have allocated a placeholder for the new id; in that case its
/// children and archive members must follow the real local object before the
/// placeholder is removed.
fn reuse_native_object_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
    row: &SyncRow,
) -> Result<Option<RowState>> {
    let existing = row_state_conn(tx, source, row.object)?;
    let Some(native) = row.image.as_ref().and_then(|image| image.object.native) else {
        return Ok(existing);
    };
    if let Some(state) = existing.as_ref().filter(|state| !state.placeholder) {
        if state.epoch.as_slice() == epoch.0.as_slice()
            || native_matches_conn(tx, state.local, native)?
        {
            return Ok(existing);
        }
    }
    let excluded_local = existing.as_ref().map(|state| state.local.0);
    let old = tx
        .prepare_cached(
            "SELECT rr.remote_object_id, rr.local_object_id
             FROM sync_replica_rows rr
             JOIN objects o ON o.object_id = rr.local_object_id
             WHERE rr.source_id = ?1 AND rr.epoch != ?2 AND rr.placeholder = 0
               AND rr.deleted = 0 AND o.deleted_at IS NULL
               AND o.native_volume_serial = ?3 AND o.native_id_high = ?4
               AND o.native_id_low = ?5
               AND (?6 IS NULL OR rr.local_object_id != ?6)",
        )?
        .query_row(
            params![
                source.0,
                epoch.0.as_slice(),
                native.volume_serial as i64,
                native.file_id_high as i64,
                native.file_id_low as i64,
                excluded_local,
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((old_remote, old_local)) = old else {
        if let Some(displaced) = existing.as_ref().filter(|state| !state.placeholder) {
            let temporary = temporary_remote_id_conn(tx, source)?;
            tx.prepare_cached(
                "UPDATE sync_replica_rows SET remote_object_id = ?3
                 WHERE source_id = ?1 AND remote_object_id = ?2",
            )?
            .execute(params![source.0, row.object.0, temporary.0])?;
            let replacement = local_object_conn(tx, source, epoch, row.object)?;
            reparent_current_epoch_refs_conn(tx, source, epoch, displaced.local, replacement)?;
        }
        return row_state_conn(tx, source, row.object);
    };

    if let Some(placeholder) = existing.as_ref().filter(|state| state.placeholder) {
        // Reparenting can meet stale old-epoch names. The incoming subtree is
        // authoritative, so remove only the colliding old names first.
        tx.prepare_cached(
            "DELETE FROM entries
             WHERE source_id = ?1 AND parent_id = ?2 AND deleted_at IS NULL
               AND is_virtual = 0 AND name IN (
                   SELECT name FROM entries
                   WHERE source_id = ?1 AND parent_id = ?3
                     AND deleted_at IS NULL AND is_virtual = 0)",
        )?
        .execute(params![source.0, old_local, placeholder.local.0])?;
        tx.prepare_cached("UPDATE entries SET parent_id = ?2 WHERE parent_id = ?1")?
            .execute(params![placeholder.local.0, old_local])?;
        tx.prepare_cached(
            "UPDATE objects SET archive_container_id = ?2 WHERE archive_container_id = ?1",
        )?
        .execute(params![placeholder.local.0, old_local])?;
        tx.prepare_cached(
            "UPDATE sources SET root_object_id = ?2 WHERE source_id = ?3 AND root_object_id = ?1",
        )?
        .execute(params![placeholder.local.0, old_local, source.0])?;
        tx.prepare_cached(
            "DELETE FROM sync_replica_rows WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .execute(params![source.0, row.object.0])?;
        tx.prepare_cached("DELETE FROM objects WHERE object_id = ?1")?
            .execute(params![placeholder.local.0])?;
    } else if let Some(displaced) = existing {
        // The rebuilt source reused two remote numeric ids for different
        // native objects. Swap the mappings through a negative id which can
        // never arrive on the wire; later rows in the same batch can then
        // resolve their native identities without a uniqueness collision.
        let temporary = temporary_remote_id_conn(tx, source)?;
        tx.prepare_cached(
            "UPDATE sync_replica_rows SET remote_object_id = ?3
             WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .execute(params![source.0, row.object.0, temporary.0])?;
        tx.prepare_cached(
            "UPDATE sync_replica_rows SET remote_object_id = ?3
             WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .execute(params![source.0, old_remote, row.object.0])?;
        tx.prepare_cached(
            "UPDATE sync_replica_rows SET remote_object_id = ?3
             WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .execute(params![source.0, temporary.0, old_remote])?;
        reparent_current_epoch_refs_conn(tx, source, epoch, displaced.local, ObjectId(old_local))?;
        // Treat the replacement like a resolved child-first placeholder so
        // applying its row emits a subtree projection for the moved children.
        tx.prepare_cached(
            "UPDATE sync_replica_rows SET placeholder = 1
             WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .execute(params![source.0, row.object.0])?;
        return row_state_conn(tx, source, row.object);
    }
    tx.prepare_cached(
        "UPDATE sync_replica_rows SET remote_object_id = ?3
         WHERE source_id = ?1 AND remote_object_id = ?2",
    )?
    .execute(params![source.0, old_remote, row.object.0])?;
    row_state_conn(tx, source, row.object)
}

/// Resolve native-id remaps before changing names, then remove all current
/// entries of the incoming objects. This makes a batch of final row images
/// atomic even when siblings swap names.
fn prepare_rows_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
    rows: &[SyncRow],
    authoritative: bool,
) -> Result<()> {
    for row in rows {
        reuse_native_object_conn(tx, source, epoch, row)?;
    }
    let mut remove = tx.prepare_cached("DELETE FROM entries WHERE object_id = ?1")?;
    for row in rows {
        if let Some(state) = row_state_conn(tx, source, row.object)? {
            if !authoritative
                && !state.placeholder
                && state.epoch.as_slice() == epoch.0.as_slice()
                && state.seq >= row.seq
            {
                continue;
            }
            remove.execute(params![state.local.0])?;
        }
    }
    drop(remove);
    // A bounded batch or repair part can split the two sides of a rename
    // swap. Once the incoming object's old names are gone, also remove a
    // stale sibling occupying each authoritative final name. That sibling's
    // later row restores its final name; completeness stays withheld until
    // the batch or repair reaches the reported head.
    let mut remove_conflict = tx.prepare_cached(
        "DELETE FROM entries
         WHERE source_id = ?1 AND parent_id IS ?2 AND name = ?3
           AND is_virtual = 0 AND deleted_at IS NULL AND object_id != ?4",
    )?;
    for row in rows {
        let Some(image) = &row.image else {
            continue;
        };
        let Some(local) = row_state_conn(tx, source, row.object)?.map(|state| state.local) else {
            continue;
        };
        let state = row_state_conn(tx, source, row.object)?.expect("local was just resolved");
        if !authoritative
            && !state.placeholder
            && state.epoch.as_slice() == epoch.0.as_slice()
            && state.seq >= row.seq
        {
            continue;
        }
        for entry in &image.entries {
            if entry.is_virtual {
                continue;
            }
            let parent = match entry.parent {
                Some(remote) => Some(local_object_conn(tx, source, epoch, remote)?.0),
                None => None,
            };
            remove_conflict.execute(params![source.0, parent, entry.name, local.0])?;
        }
    }
    Ok(())
}

fn wire_row_is_valid(row: &SyncRow) -> bool {
    row.object.0 > 0
        && row.image.as_ref().is_none_or(|image| {
            image.object.id.0 > 0
                && u64::from(image.object.generation) == row.generation
                && image.object.size <= i64::MAX as u64
                && image.object.allocated <= i64::MAX as u64
                && image
                    .archive_container
                    .is_none_or(|object| object.0 > 0 && object != row.object)
                && image.entries.iter().all(|entry| {
                    entry
                        .parent
                        .is_none_or(|object| object.0 > 0 && object != row.object)
                })
        })
}

fn parent_would_cycle_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    object: ObjectId,
    parent: ObjectId,
) -> Result<bool> {
    Ok(tx.query_row(
        "WITH RECURSIVE ancestors(object_id) AS (
             SELECT ?3
             UNION
             SELECT e.parent_id FROM entries e
             JOIN ancestors a ON a.object_id = e.object_id
             WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND e.parent_id IS NOT NULL
         )
         SELECT EXISTS(SELECT 1 FROM ancestors WHERE object_id = ?2)",
        params![source.0, object.0, parent.0],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

fn archive_container_would_cycle_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    object: ObjectId,
    container: ObjectId,
) -> Result<bool> {
    Ok(tx.query_row(
        "WITH RECURSIVE containers(object_id) AS (
             SELECT ?3
             UNION
             SELECT o.archive_container_id FROM objects o
             JOIN containers c ON c.object_id = o.object_id
             WHERE o.source_id = ?1 AND o.deleted_at IS NULL
               AND o.archive_container_id IS NOT NULL
         )
         SELECT EXISTS(SELECT 1 FROM containers WHERE object_id = ?2)",
        params![source.0, object.0, container.0],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

fn wire_entry_count(rows: &[SyncRow]) -> Option<usize> {
    rows.iter().try_fold(0usize, |count, row| {
        count.checked_add(row.image.as_ref().map_or(0, |image| image.entries.len()))
    })
}

fn wire_leaf_hashes(rows: &[SyncRow], leaf_bits: u8) -> Result<BTreeMap<u32, [u8; 32]>> {
    let mut grouped = BTreeMap::<u32, Vec<&SyncRow>>::new();
    for row in rows {
        grouped
            .entry(leaf_index(leaf_bits, row.object))
            .or_default()
            .push(row);
    }
    grouped
        .into_iter()
        .map(|(leaf, mut rows)| {
            rows.sort_unstable_by_key(|row| row.object);
            let mut hasher = MerkleLeafHasher::new();
            for row in rows {
                let image_hash = match &row.image {
                    Some(image) => sync_row_image_hash(image)?,
                    None => [0; 32],
                };
                hasher.update(&record_digest(
                    row.object,
                    row.generation,
                    row.image.is_none(),
                    &image_hash,
                ));
            }
            Ok((leaf, hasher.finalize()))
        })
        .collect()
}

fn repair_pending_conn(tx: &Transaction<'_>, source: SourceId) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sync_replica_repairs
            WHERE source_id = ?1 AND json_array_length(remaining) > 0)",
        params![source.0],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

fn source_is_retired_conn(conn: &Connection, source: SourceId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT state = ?2 FROM sources WHERE source_id = ?1",
        params![source.0, SourceState::Retired.as_str()],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

fn node_is_authorized_conn(conn: &Connection, node: &[u8; 16]) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM fleet_peers
            WHERE node_id = ?1 AND enabled = 1 AND role = 'node')",
        params![node.as_slice()],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

fn source_peer_is_authorized_conn(
    conn: &Connection,
    source: SourceId,
    caller: &[u8; 16],
) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sync_replica_sources r
            JOIN fleet_peers p ON p.node_id = r.node_id
            WHERE r.source_id = ?1 AND r.node_id = ?2
              AND p.enabled = 1 AND p.role = 'node')",
        params![source.0, caller.as_slice()],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

/// Content at a replica is never present: files say so explicitly, and
/// everything that had no content to begin with keeps saying so.
fn replica_content_state(image: &SyncRowImage) -> ContentState {
    match image.object.kind {
        ObjectKind::File | ObjectKind::VirtualFile => ContentState::NotReplicated,
        _ => ContentState::NotApplicable,
    }
}

#[derive(Default)]
struct ApplyCounts {
    applied: u64,
}

/// Apply one row image or tombstone. `authoritative` (repair) applies the
/// row even when the stored sequence is not older; ordinary batches skip
/// rows a later delivery already superseded, which is what makes duplicate
/// and overlapping batches idempotent.
fn apply_row_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
    row: &SyncRow,
    authoritative: bool,
    now: i64,
    counts: &mut ApplyCounts,
) -> Result<Option<String>> {
    let existing = reuse_native_object_conn(tx, source, epoch, row)?;
    if let Some(state) = &existing {
        // Sequences only order rows within one epoch; a row of another
        // epoch is always superseded by the epoch being streamed.
        if !state.placeholder
            && !authoritative
            && state.epoch.as_slice() == epoch.0.as_slice()
            && state.seq >= row.seq
        {
            return Ok(None);
        }
    }
    let was_placeholder = existing.as_ref().is_some_and(|s| s.placeholder);
    let local = match &existing {
        Some(state) => state.local,
        None => local_object_conn(tx, source, epoch, row.object)?,
    };
    let image_hash = match &row.image {
        Some(image) => sync_row_image_hash(image)?,
        None => [0; 32],
    };
    match &row.image {
        None => {
            tx.prepare_cached(
                "UPDATE objects SET deleted_at = COALESCE(deleted_at, ?2), generation = MAX(generation, ?3)
                 WHERE object_id = ?1",
            )?
            .execute(params![local.0, now, row.generation as i64])?;
            tx.prepare_cached(
                "UPDATE entries SET deleted_at = ?2 WHERE object_id = ?1 AND deleted_at IS NULL",
            )?
            .execute(params![local.0, now])?;
            outbox_append_conn(tx, source, local, "delete", row.generation as i64)?;
        }
        Some(image) => {
            if image.version != SYNC_ROW_IMAGE_VERSION {
                return Err(CatalogError::InvalidState(format!(
                    "row image version {} is not supported (expected {SYNC_ROW_IMAGE_VERSION})",
                    image.version
                )));
            }
            let o = &image.object;
            let (serial, high, low) = match &o.native {
                Some(n) => (
                    Some(n.volume_serial as i64),
                    Some(n.file_id_high as i64),
                    Some(n.file_id_low as i64),
                ),
                None => (None, None, None),
            };
            let container = match image.archive_container {
                Some(remote) => Some(local_object_conn(tx, source, epoch, remote)?.0),
                None => None,
            };
            if let Some(container) = container {
                if archive_container_would_cycle_conn(tx, source, local, ObjectId(container))? {
                    return Ok(Some(format!(
                        "replica topology cycle: object {} cannot use archive container {}",
                        row.object,
                        image.archive_container.expect("container was present")
                    )));
                }
            }
            tx.prepare_cached(
                "UPDATE objects SET kind = ?2, native_volume_serial = ?3, native_id_high = ?4,
                    native_id_low = ?5, identity_confidence = ?6, generation = ?7, size = ?8,
                    allocated = ?9, attributes = ?10, created = ?11, modified = ?12, changed = ?13,
                    accessed = ?14, reparse_tag = ?15, link_count = ?16, content_state = ?17,
                    content_id = ?18, first_seen_generation = ?19, last_seen_generation = ?19,
                    listed_generation = NULL, deleted_at = NULL, archive_container_id = ?20
                 WHERE object_id = ?1",
            )?
            .execute(params![
                local.0,
                o.kind.as_str(),
                serial,
                high,
                low,
                o.identity_confidence.as_str(),
                row.generation as i64,
                o.size as i64,
                o.allocated as i64,
                o.attributes.0 as i64,
                o.created.map(|t| t.0),
                o.modified.map(|t| t.0),
                o.changed.map(|t| t.0),
                o.accessed.map(|t| t.0),
                o.reparse_tag as i64,
                o.link_count as i64,
                replica_content_state(image).as_str(),
                o.content_id.map(|c| c.0.to_vec()),
                REMOTE_GENERATION,
                container,
            ])?;
            tx.prepare_cached("DELETE FROM entries WHERE object_id = ?1")?
                .execute(params![local.0])?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO entries (source_id, parent_id, object_id, name, name_folded, extension,
                    is_virtual, first_seen_generation, last_seen_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            )?;
            for entry in &image.entries {
                let parent = match entry.parent {
                    Some(remote_parent) => {
                        Some(local_object_conn(tx, source, epoch, remote_parent)?.0)
                    }
                    None => None,
                };
                if let Some(parent) = parent {
                    if parent_would_cycle_conn(tx, source, local, ObjectId(parent))? {
                        return Ok(Some(format!(
                            "replica topology cycle: object {} cannot use parent {}",
                            row.object,
                            entry.parent.expect("parent was present")
                        )));
                    }
                }
                let ext = if matches!(o.kind, ObjectKind::File | ObjectKind::VirtualFile) {
                    extension_of(&entry.name)
                } else {
                    String::new()
                };
                insert.execute(params![
                    source.0,
                    parent,
                    local.0,
                    entry.name,
                    crate::policy::fold(&entry.name),
                    ext,
                    entry.is_virtual as i64,
                    REMOTE_GENERATION,
                ])?;
                if parent.is_none() {
                    tx.prepare_cached(
                        "UPDATE sources SET root_object_id = ?2 WHERE source_id = ?1",
                    )?
                    .execute(params![source.0, local.0])?;
                }
            }
            // A placeholder that becomes real has children projected with an
            // unrenderable path; rebuilding the subtree fixes them.
            let op = if was_placeholder { "subtree" } else { "upsert" };
            outbox_append_conn(tx, source, local, op, row.generation as i64)?;
        }
    }
    tx.prepare_cached(
        "UPDATE sync_replica_rows SET epoch = ?3, seq = ?4, generation = ?5, deleted = ?6,
             image_hash = ?7, placeholder = 0
         WHERE source_id = ?1 AND remote_object_id = ?2",
    )?
    .execute(params![
        source.0,
        row.object.0,
        epoch.0.as_slice(),
        row.seq as i64,
        row.generation as i64,
        row.image.is_none() as i64,
        image_hash.as_slice(),
    ])?;
    counts.applied += 1;
    Ok(None)
}

/// Retire every mapping that does not belong to `epoch`: after an epoch
/// change has caught up to one source snapshot, anything not re-shipped is
/// authoritatively absent. Applied live rows are tombstoned locally for
/// projection history, while their replica mappings (and unused
/// placeholders) disappear from the Merkle image.
fn retire_other_epochs_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
    now: i64,
) -> Result<(u64, bool)> {
    let stale: Vec<(i64, i64, bool, bool)> = tx
        .prepare_cached(
            "SELECT remote_object_id, local_object_id, deleted, placeholder
             FROM sync_replica_rows WHERE source_id = ?1 AND epoch != ?2
             ORDER BY remote_object_id LIMIT ?3",
        )?
        .query_map(
            params![source.0, epoch.0.as_slice(), RETIRE_STEP_ROWS as i64],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, i64>(3)? != 0,
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    for (remote, local, deleted, placeholder) in &stale {
        if *placeholder {
            tx.prepare_cached("DELETE FROM objects WHERE object_id = ?1")?
                .execute(params![local])?;
        } else if !deleted {
            tx.prepare_cached(
                "UPDATE objects SET deleted_at = COALESCE(deleted_at, ?2) WHERE object_id = ?1",
            )?
            .execute(params![local, now])?;
            tx.prepare_cached(
                "UPDATE entries SET deleted_at = ?2 WHERE object_id = ?1 AND deleted_at IS NULL",
            )?
            .execute(params![local, now])?;
            outbox_append_conn(tx, source, ObjectId(*local), "delete", 0)?;
        }
        tx.prepare_cached(
            "DELETE FROM sync_replica_rows WHERE source_id = ?1 AND remote_object_id = ?2",
        )?
        .execute(params![source.0, remote])?;
    }
    tx.prepare_cached(
        "UPDATE sources SET root_object_id = NULL
         WHERE source_id = ?1 AND root_object_id IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM sync_replica_rows rr
               WHERE rr.source_id = ?1 AND rr.local_object_id = sources.root_object_id
                 AND rr.epoch = ?2 AND rr.deleted = 0 AND rr.placeholder = 0)",
    )?
    .execute(params![source.0, epoch.0.as_slice()])?;
    let remaining = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_replica_rows WHERE source_id = ?1 AND epoch != ?2)",
        params![source.0, epoch.0.as_slice()],
        |r| r.get::<_, i64>(0),
    )? != 0;
    Ok((stale.len() as u64, !remaining))
}

fn mark_applied_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    now: i64,
    complete: bool,
) -> Result<()> {
    tx.prepare_cached(
        "UPDATE sync_replica_sources SET applied_at = ?2, updated_at = ?2 WHERE source_id = ?1",
    )?
    .execute(params![source.0, now])?;
    mark_completeness_conn(tx, source, now, complete)
}

/// Whether every live relationship in the current image resolves to a live,
/// non-placeholder row from the same epoch. Child-first delivery is valid,
/// but it must never make an incomplete image look complete.
fn replica_topology_is_closed_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    epoch: &SyncEpoch,
) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT NOT EXISTS (
             SELECT 1 FROM entries e
             JOIN sync_replica_rows child ON child.source_id = ?1
                  AND child.local_object_id = e.object_id
                  AND child.epoch = ?2 AND child.deleted = 0 AND child.placeholder = 0
             WHERE e.source_id = ?1 AND e.deleted_at IS NULL AND e.parent_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM sync_replica_rows parent
                   JOIN objects po ON po.object_id = parent.local_object_id
                   WHERE parent.source_id = ?1 AND parent.local_object_id = e.parent_id
                     AND parent.epoch = ?2 AND parent.deleted = 0 AND parent.placeholder = 0
                     AND po.deleted_at IS NULL)
             UNION ALL
             SELECT 1 FROM objects o
             JOIN sync_replica_rows child ON child.source_id = ?1
                  AND child.local_object_id = o.object_id
                  AND child.epoch = ?2 AND child.deleted = 0 AND child.placeholder = 0
             WHERE o.source_id = ?1 AND o.deleted_at IS NULL
               AND o.archive_container_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM sync_replica_rows container
                   JOIN objects co ON co.object_id = container.local_object_id
                   WHERE container.source_id = ?1
                     AND container.local_object_id = o.archive_container_id
                     AND container.epoch = ?2 AND container.deleted = 0
                     AND container.placeholder = 0 AND co.deleted_at IS NULL)
             LIMIT 1
         )",
        params![source.0, epoch.0.as_slice()],
        |row| row.get::<_, i64>(0),
    )? != 0)
}

fn mark_completeness_conn(
    tx: &Transaction<'_>,
    source: SourceId,
    now: i64,
    complete: bool,
) -> Result<()> {
    let (state, reason) = if complete {
        (SourceState::MetadataComplete, None)
    } else {
        (
            SourceState::Reconciling,
            Some("fleet snapshot is still arriving"),
        )
    };
    tx.prepare_cached(
        "UPDATE sources SET published_generation = CASE WHEN ?5 THEN ?2 ELSE NULL END,
            state = ?3, state_reason = ?4,
            last_scan_completed_at = CASE WHEN ?5 THEN ?6
                                          ELSE last_scan_completed_at END,
            updated_at = ?6
         WHERE source_id = ?1",
    )?
    .execute(params![
        source.0,
        REMOTE_GENERATION,
        state.as_str(),
        reason,
        complete,
        now
    ])?;
    Ok(())
}

fn mark_aggregates_pending_conn(tx: &Transaction<'_>, source: SourceId, now: i64) -> Result<()> {
    tx.prepare_cached(
        "UPDATE sources SET published_generation = NULL, state = ?2, state_reason = ?3,
             updated_at = ?4 WHERE source_id = ?1",
    )?
    .execute(params![
        source.0,
        SourceState::Reconciling.as_str(),
        "fleet metadata is applied; directory totals are rebuilding",
        now
    ])?;
    Ok(())
}

fn replica_digests_conn(conn: &Connection, source: SourceId) -> Result<Vec<RecordDigest>> {
    conn.prepare_cached(
        "SELECT remote_object_id, generation, deleted, image_hash FROM sync_replica_rows
             WHERE source_id = ?1 AND placeholder = 0 ORDER BY remote_object_id",
    )?
    .query_map(params![source.0], |r| {
        Ok((
            ObjectId(r.get(0)?),
            r.get::<_, i64>(1)? as u64,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, Vec<u8>>(3)?,
        ))
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?
    .into_iter()
    .map(|(object, generation, deleted, image_hash)| {
        Ok(record_digest(
            object,
            generation,
            deleted,
            &image_hash_from_blob(image_hash)?,
        ))
    })
    .collect::<Result<_>>()
}

fn replica_leaf_hashes_conn(
    conn: &Connection,
    source: SourceId,
    leaf_bits: u8,
) -> Result<Vec<[u8; 32]>> {
    let empty = MerkleLeafHasher::new().finalize();
    let mut hashes = vec![empty; 1usize << leaf_bits];
    let mut active_leaf = None;
    let mut active = MerkleLeafHasher::new();
    let mut stmt = conn.prepare_cached(
        "SELECT remote_object_id, generation, deleted, image_hash,
            eidos_merkle_leaf(?2, remote_object_id) AS leaf
         FROM sync_replica_rows
         WHERE source_id = ?1 AND placeholder = 0 ORDER BY leaf, remote_object_id",
    )?;
    let mut rows = stmt.query(params![source.0, leaf_bits as i64])?;
    while let Some(row) = rows.next()? {
        let leaf = row.get::<_, i64>(4)? as usize;
        if active_leaf.is_some_and(|previous| previous != leaf) {
            let previous = active_leaf.expect("checked above");
            hashes[previous] = std::mem::replace(&mut active, MerkleLeafHasher::new()).finalize();
        }
        active_leaf = Some(leaf);
        active.update(&record_digest(
            ObjectId(row.get(0)?),
            row.get::<_, i64>(1)? as u64,
            row.get::<_, i64>(2)? != 0,
            &image_hash_from_blob(row.get(3)?)?,
        ));
    }
    if let Some(leaf) = active_leaf {
        hashes[leaf] = active.finalize();
    }
    Ok(hashes)
}

/// Coverage facts of a replicated source, for the completeness row of a
/// search response. `None` for a local source.
pub(crate) fn coverage_conn(
    conn: &Connection,
    source: SourceId,
) -> Result<Option<ReplicaCoverage>> {
    let Some(state) = state_conn(conn, source)? else {
        return Ok(None);
    };
    let peer: Option<(String, i64)> = conn
        .query_row(
            "SELECT name, connected FROM fleet_peers WHERE node_id = ?1",
            params![state.node_id.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Some(ReplicaCoverage {
        node_id: state.node_id,
        node_name: peer
            .as_ref()
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| "unknown node".into()),
        connected: peer.is_some_and(|(_, c)| c != 0),
        remote_source_id: state.remote_source_id,
        epoch: state.admission.epoch,
        applied_seq: state.admission.applied_seq,
        reported_head: state.reported_head,
        applied_at: state.applied_at,
        reported_at: state.reported_at,
        resyncing: state.resync_target.is_some(),
        alarms: 0,
    }))
}

impl Catalog {
    /// Make sure a peer's source has a local `sources` row and a durable
    /// cursor, creating both on first contact. Returns the replica state.
    pub fn replica_ensure_source(
        &self,
        caller: [u8; 16],
        node: &RemoteNode,
        descriptor: &RemoteSourceDescriptor,
        epoch: SourceEpoch,
    ) -> Result<ReplicaSourceState> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if caller != node.node_id || !node_is_authorized_conn(&tx, &caller)? {
                return Err(CatalogError::InvalidState(
                    "replica peer is disabled or no longer enrolled".into(),
                ));
            }
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT source_id FROM sync_replica_sources WHERE node_id = ?1 AND remote_source_id = ?2",
                    params![node.node_id.as_slice(), descriptor.remote_source_id.0],
                    |r| r.get(0),
                )
                .optional()?;
            let source = match existing {
                Some(id) => SourceId(id),
                None => {
                    let host = ensure_host_conn(&tx, &node.node_id, &node.name, &node.platform)?;
                    let now = UnixNanos::now().0;
                    // Distinct source identities stay distinct even when two
                    // nodes expose the same name or path.
                    let base = format!("{}/{}", node.name, descriptor.name);
                    let mut name = base.clone();
                    let mut suffix = 2;
                    while tx
                        .query_row(
                            "SELECT 1 FROM sources WHERE name = ?1",
                            params![name],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()?
                        .is_some()
                    {
                        name = format!("{base}#{suffix}");
                        suffix += 1;
                    }
                    tx.execute(
                        "INSERT INTO sources (host_id, name, kind, root_path, aliases, state, state_reason,
                            content_enabled, case_sensitive, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'new', 'awaiting the first replicated batch', 0, ?6, ?7, ?7)",
                        params![
                            host.0,
                            name,
                            SourceKind::Remote.as_str(),
                            descriptor.root_path,
                            serde_json::to_string(&descriptor.aliases)?,
                            if descriptor.case_sensitive { "sensitive" } else { "insensitive" },
                            now
                        ],
                    )?;
                    let source = SourceId(tx.last_insert_rowid());
                    let admission = AdmissionState::new(descriptor.remote_source_id, epoch);
                    tx.execute(
                        "INSERT INTO sync_replica_sources (source_id, node_id, remote_source_id, admission,
                            image_version, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                        params![
                            source.0,
                            node.node_id.as_slice(),
                            descriptor.remote_source_id.0,
                            admission_to_json(&admission)?,
                            SYNC_ROW_IMAGE_VERSION,
                            now
                        ],
                    )?;
                    source
                }
            };
            tx.execute(
                "UPDATE sources SET case_sensitive = ?2, updated_at = ?3 WHERE source_id = ?1",
                params![
                    source.0,
                    if descriptor.case_sensitive { "sensitive" } else { "insensitive" },
                    UnixNanos::now().0,
                ],
            )?;
            let state = state_conn(&tx, source)?.expect("replica row exists");
            tx.commit()?;
            Ok(state)
        })
    }

    pub fn replica_source(&self, source: SourceId) -> Result<Option<ReplicaSourceState>> {
        self.with_reader(|conn| state_conn(conn, source))
    }

    pub fn replica_sources(&self) -> Result<Vec<ReplicaSourceState>> {
        self.with_reader(|conn| {
            let ids: Vec<i64> = conn
                .prepare("SELECT source_id FROM sync_replica_sources ORDER BY source_id")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            ids.into_iter()
                .filter_map(|id| state_conn(conn, SourceId(id)).transpose())
                .collect()
        })
    }

    /// Coverage facts of a replicated source, `None` for a local one.
    pub fn replica_coverage(&self, source: SourceId) -> Result<Option<ReplicaCoverage>> {
        self.with_reader(|conn| coverage_conn(conn, source))
    }

    /// Admit a peer's offer of a source (the protocol's `Hello`) and commit
    /// the resulting cursor before answering.
    #[allow(clippy::too_many_arguments)]
    pub fn replica_admit_hello(
        &self,
        caller: [u8; 16],
        source: SourceId,
        epoch: SourceEpoch,
        head_seq: u64,
        head_chain: ChainHash,
        compacted_through: u64,
        image_revision: u64,
    ) -> Result<HelloOutcome> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if source_is_retired_conn(&tx, source)? {
                return Ok(HelloOutcome::Rejected {
                    reason: "replica source is retired".into(),
                });
            }
            if !source_peer_is_authorized_conn(&tx, source, &caller)? {
                return Ok(HelloOutcome::Rejected {
                    reason: "replica peer is disabled or no longer enrolled".into(),
                });
            }
            let now = UnixNanos::now().0;
            let same_incarnation = state.admission.epoch == epoch
                || state.admission.pending_epoch() == Some(epoch);
            let same_epoch_rewind = same_incarnation
                && state.reported_at.is_some()
                && head_seq < state.reported_head;
            let same_epoch_fork = same_incarnation
                && state.reported_at.is_some()
                && head_seq == state.reported_head
                && state.reported_chain.is_some_and(|chain| chain != head_chain);
            let same_epoch_image_rewind = same_incarnation
                && state.reported_at.is_some()
                && image_revision < state.reported_image_revision;
            let reconnecting_pending_epoch = state.admission.pending_epoch() == Some(epoch);
            let active_epoch_lost_anchor = state.admission.epoch == epoch
                && !reconnecting_pending_epoch
                && head_seq > state.admission.applied_seq
                && compacted_through > state.admission.applied_seq;
            let malformed = head_seq > MAX_SQLITE_SEQUENCE
                || compacted_through > MAX_SQLITE_SEQUENCE
                || compacted_through > head_seq
                || image_revision > MAX_SQLITE_SEQUENCE
                || (head_seq == 0 && head_chain != CHAIN_GENESIS);
            let outcome = if malformed {
                HelloOutcome::Rejected {
                    reason: "malformed source head".into(),
                }
            } else if same_epoch_rewind {
                HelloOutcome::Rejected {
                    reason: format!(
                        "same-epoch sequence rewind: reported={}, source={head_seq}",
                        state.reported_head
                    ),
                }
            } else if same_epoch_fork {
                HelloOutcome::Rejected {
                    reason: format!(
                        "history fork at sequence {head_seq}: offered head differs from the durable head"
                    ),
                }
            } else if same_epoch_image_rewind {
                HelloOutcome::Rejected {
                    reason: format!(
                        "same-epoch retained-image rewind: reported={}, source={image_revision}",
                        state.reported_image_revision
                    ),
                }
            } else if active_epoch_lost_anchor {
                HelloOutcome::NewEpochRequired {
                    reason: format!(
                        "history continuity from durable cursor {} is no longer retained; mint a new epoch",
                        state.admission.applied_seq
                    ),
                }
            } else {
                match state.admission.admit_hello(epoch, head_seq) {
                HelloDecision::Incremental { after_seq }
                    if head_seq == after_seq && head_chain != state.admission.applied_chain =>
                {
                    HelloOutcome::Rejected {
                        reason: format!(
                            "history fork at sequence {after_seq}: head hash differs at the cursor"
                        ),
                    }
                }
                HelloDecision::Incremental { after_seq } => HelloOutcome::Resume {
                    epoch,
                    after_seq,
                    requires_repair: after_seq < compacted_through
                        || image_revision > state.applied_image_revision
                        || repair_pending_conn(&tx, source)?,
                },
                HelloDecision::FullResync { new_epoch, .. } => {
                    // Every live object of the new incarnation is stamped at
                    // or below the head offered now; once the stream passes
                    // it, rows of other epochs are authoritatively absent.
                    state.resync_target = Some(if reconnecting_pending_epoch {
                        state
                            .resync_target
                            .map_or(head_seq, |target| target.max(head_seq))
                    } else {
                        head_seq
                    });
                    HelloOutcome::FullResync { epoch: new_epoch }
                }
                HelloDecision::RejectAndAlarm {
                    applied_seq,
                    offered_seq,
                } => HelloOutcome::Rejected {
                    reason: format!(
                        "same-epoch sequence rewind: central={applied_seq}, source={offered_seq}"
                    ),
                },
                HelloDecision::RejectEpochAndAlarm {
                    current_epoch,
                    offered_epoch,
                } => HelloOutcome::Rejected {
                    reason: format!(
                        "retired/conflicting epoch {offered_epoch}; active epoch is {current_epoch}"
                    ),
                },
                }
            };
            if matches!(
                outcome,
                HelloOutcome::Resume { .. } | HelloOutcome::FullResync { .. }
            ) {
                let restarting_pending_epoch = reconnecting_pending_epoch
                    && (head_seq != state.reported_head
                        || state.reported_chain != Some(head_chain)
                        || image_revision != state.reported_image_revision);
                if restarting_pending_epoch {
                    // Rows staged for an older snapshot of the pending epoch
                    // must become retirement candidates again. Otherwise an
                    // object deleted and compacted before the replacement
                    // repair remains tagged as current and survives forever.
                    tx.execute(
                        "UPDATE sync_replica_rows SET epoch = ?3
                         WHERE source_id = ?1 AND epoch = ?2",
                        params![
                            source.0,
                            epoch.as_bytes().as_slice(),
                            state.admission.epoch.as_bytes().as_slice()
                        ],
                    )?;
                }
                let fresh_epoch = matches!(outcome, HelloOutcome::FullResync { .. })
                    && !reconnecting_pending_epoch;
                tx.execute(
                    "UPDATE sync_replica_sources SET reported_head = ?2, reported_chain = ?3,
                         reported_compacted = ?4, reported_image_revision = ?5,
                         applied_image_revision = CASE WHEN ?6 THEN 0 ELSE applied_image_revision END,
                         reported_at = ?7
                     WHERE source_id = ?1",
                    params![
                        source.0,
                        head_seq as i64,
                        head_chain.as_slice(),
                        compacted_through as i64,
                        image_revision as i64,
                        fresh_epoch,
                        now
                    ],
                )?;
                // A different repair head or epoch no longer describes the
                // accepted offer. Preserve an exact staged repair across a
                // reconnect, but discard stale repair state before resuming.
                tx.execute(
                    "DELETE FROM sync_replica_repairs
                     WHERE source_id = ?1
                       AND (epoch != ?2 OR through_seq != ?3 OR through_chain != ?4
                            OR image_revision != ?5)",
                    params![
                        source.0,
                        epoch.as_bytes().as_slice(),
                        head_seq as i64,
                        head_chain.as_slice(),
                        image_revision as i64
                    ],
                )?;
                if head_seq > state.admission.applied_seq
                    || state.resync_target.is_some()
                    || image_revision > state.applied_image_revision
                {
                    mark_completeness_conn(&tx, source, now, false)?;
                }
            }
            store_admission_conn(&tx, source, &state.admission, state.resync_target)?;
            tx.commit()?;
            Ok(outcome)
        })
    }

    /// Apply a materialized batch: admission, effects, cursor, one
    /// transaction. `Applied` means the acknowledgement may be sent. The
    /// batch's `source_id` is the peer's; `source` is the local replica.
    pub fn replica_apply_batch(
        &self,
        caller: [u8; 16],
        source: SourceId,
        batch: &SyncBatch,
    ) -> Result<BatchOutcome> {
        let epoch = batch.epoch.to_source_epoch();
        let (after_seq, after_chain, through_seq, through_chain) = (
            batch.after_seq,
            batch.after_chain,
            batch.through_seq,
            batch.through_chain,
        );
        let rows = batch.rows.as_slice();
        let unique = rows.iter().map(|r| r.object).collect::<BTreeSet<_>>().len() == rows.len();
        let entry_count = wire_entry_count(rows);
        let empty_snapshot = after_seq == 0
            && through_seq == 0
            && batch.head_seq == 0
            && after_chain == CHAIN_GENESIS
            && through_chain == CHAIN_GENESIS
            && rows.is_empty();
        let covers_through = empty_snapshot || rows.iter().any(|row| row.seq == through_seq);
        if (after_seq >= through_seq && !empty_snapshot)
            || !covers_through
            || !unique
            || after_seq > MAX_SQLITE_SEQUENCE
            || through_seq > MAX_SQLITE_SEQUENCE
            || batch.head_seq > MAX_SQLITE_SEQUENCE
            || through_seq > batch.head_seq
            || rows.len() > MAX_APPLY_ROWS
            || entry_count.is_none_or(|count| count > MAX_APPLY_ENTRIES)
            || rows.iter().any(|r| {
                r.seq <= after_seq
                    || r.seq > through_seq
                    || r.generation > u64::from(u32::MAX)
                    || !wire_row_is_valid(r)
            })
        {
            return Ok(BatchOutcome::Rejected {
                reason: "malformed batch interval".into(),
            });
        }
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if source_is_retired_conn(&tx, source)? {
                return Ok(BatchOutcome::Rejected {
                    reason: "replica source is retired".into(),
                });
            }
            if !source_peer_is_authorized_conn(&tx, source, &caller)? {
                return Ok(BatchOutcome::Rejected {
                    reason: "replica peer is disabled or no longer enrolled".into(),
                });
            }
            if batch.source_id != state.remote_source_id {
                return Ok(BatchOutcome::Rejected {
                    reason: format!(
                        "batch source {} does not match replica source {}",
                        batch.source_id, state.remote_source_id
                    ),
                });
            }
            // A source may advance between Hello and materializing its next
            // batch. Accept that newer authenticated head, but never let a
            // batch which would advance the cursor regress the durable head.
            // Already-applied old batches remain harmlessly idempotent.
            if batch.head_seq < state.reported_head
                && through_seq > state.admission.applied_seq
            {
                return Ok(BatchOutcome::Rejected {
                    reason: format!(
                        "batch head {} regresses the durable offered head {}",
                        batch.head_seq, state.reported_head
                    ),
                });
            }
            if batch.head_seq == state.reported_head
                && through_seq == batch.head_seq
                && state
                    .reported_chain
                    .is_some_and(|chain| chain != through_chain)
            {
                return Ok(BatchOutcome::Rejected {
                    reason: format!(
                        "history fork at sequence {through_seq}: batch head differs from the durable head"
                    ),
                });
            }
            if rows.iter().any(|row| {
                row.image.as_ref().is_some_and(|image| {
                    image.object.id != row.object
                        || image.object.source_id != state.remote_source_id
                })
            }) {
                return Ok(BatchOutcome::Rejected {
                    reason: "row image identity does not match its enclosing row and source".into(),
                });
            }
            let now = UnixNanos::now().0;
            let sync_epoch = SyncEpoch::from_source_epoch(epoch);
            // A stream from sequence zero of the epoch we asked for is the
            // snapshot: it installs the epoch and starts the resync window.
            let snapshot_start = state.admission.pending_epoch() == Some(epoch)
                && after_seq == 0
                && after_chain == CHAIN_GENESIS;
            if snapshot_start {
                if !state
                    .admission
                    .snapshot_applied(epoch, through_seq, through_chain)
                {
                    return Ok(BatchOutcome::Rejected {
                        reason: "snapshot was not requested for this epoch".into(),
                    });
                }
                // Snapshot streaming is an explicit fallback from a staged
                // repair. The equal-sequence row guard below preserves rows
                // already installed by that repair while the snapshot fills
                // the rest of the epoch.
                tx.prepare_cached("DELETE FROM sync_replica_repairs WHERE source_id = ?1")?
                    .execute(params![source.0])?;
            } else {
                match state
                    .admission
                    .admit_batch(epoch, after_seq, &after_chain, through_seq)
                {
                    BatchDecision::Apply => {}
                    BatchDecision::AlreadyApplied => {
                        return Ok(BatchOutcome::AlreadyApplied {
                            applied_seq: state.admission.applied_seq,
                        })
                    }
                    BatchDecision::Stale { applied_seq } => {
                        return Ok(BatchOutcome::Stale { applied_seq })
                    }
                    BatchDecision::Gap {
                        expected_at_most,
                        received,
                    } => {
                        return Ok(BatchOutcome::Rejected {
                            reason: format!(
                                "sequence gap: central={expected_at_most}, batch-after={received}"
                            ),
                        })
                    }
                    BatchDecision::HistoryFork { applied_seq, .. } => {
                        return Ok(BatchOutcome::Rejected {
                            reason: format!(
                                "history fork at sequence {applied_seq}: source was rewound and rewritten"
                            ),
                        })
                    }
                    BatchDecision::FullResyncRequired => {
                        return Ok(BatchOutcome::FullResyncRequired {
                            epoch: state
                                .admission
                                .pending_epoch()
                                .unwrap_or(state.admission.epoch),
                        })
                    }
                }
                state.admission.applied(through_seq, through_chain);
            }
            let mut counts = ApplyCounts::default();
            prepare_rows_conn(&tx, source, &sync_epoch, rows, false)?;
            for row in rows {
                if let Some(reason) =
                    apply_row_conn(&tx, source, &sync_epoch, row, false, now, &mut counts)?
                {
                    return Ok(BatchOutcome::Rejected { reason });
                }
            }
            let mut retired = 0;
            if let Some(target) = state.resync_target {
                if state.admission.applied_seq >= target && through_seq == batch.head_seq {
                    let (step, done) = retire_other_epochs_conn(&tx, source, &sync_epoch, now)?;
                    retired = step;
                    if done {
                        state.resync_target = None;
                    }
                }
            }
            store_admission_conn(&tx, source, &state.admission, state.resync_target)?;
            tx.prepare_cached(
                "UPDATE sync_replica_sources
                 SET reported_at = CASE WHEN ?2 > reported_head THEN ?3 ELSE reported_at END,
                     reported_chain = CASE
                         WHEN ?2 > reported_head THEN
                             CASE WHEN ?4 = ?2 THEN ?5 ELSE NULL END
                         WHEN ?2 = reported_head AND ?4 = ?2 THEN ?5
                         ELSE reported_chain
                     END,
                     reported_head = MAX(reported_head, ?2)
                 WHERE source_id = ?1",
            )?
            .execute(params![
                source.0,
                batch.head_seq as i64,
                now,
                through_seq as i64,
                through_chain.as_slice()
            ])?;
            let reported_head = state.reported_head.max(batch.head_seq);
            let image_complete = through_seq == batch.head_seq
                && through_seq >= reported_head
                && state.resync_target.is_none()
                && state.applied_image_revision >= state.reported_image_revision
                && !repair_pending_conn(&tx, source)?
                && replica_topology_is_closed_conn(&tx, source, &sync_epoch)?;
            // The durable image is not publishable until its directory
            // aggregates have been rebuilt and a subtree projection event
            // has been staged by `replica_rebuild_aggregates`.
            mark_applied_conn(&tx, source, now, false)?;
            if image_complete {
                mark_aggregates_pending_conn(&tx, source, now)?;
            }
            tx.commit()?;
            Ok(BatchOutcome::Applied {
                through_seq,
                rows: counts.applied,
                retired_rows: retired,
            })
        })
    }

    /// Compare an offered Merkle leaf manifest against the replica and say
    /// which leaves to request.
    #[allow(clippy::too_many_arguments)]
    pub fn replica_repair_offer(
        &self,
        caller: [u8; 16],
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        image_revision: u64,
        anchor_chain: Option<ChainHash>,
        leaf_bits: u8,
        leaf_hashes: &[[u8; 32]],
    ) -> Result<RepairOfferOutcome> {
        self.with_writer(|conn| {
            register_merkle_leaf_function(conn)?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if source_is_retired_conn(&tx, source)? {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "replica source is retired".into(),
                });
            }
            if !source_peer_is_authorized_conn(&tx, source, &caller)? {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "replica peer is disabled or no longer enrolled".into(),
                });
            }
            if through_seq != state.reported_head {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: format!(
                        "repair head {through_seq} does not match the durable offered head {}",
                        state.reported_head
                    ),
                });
            }
            if state.reported_chain != Some(through_chain) {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "history fork: repair chain does not match the durable offered head"
                        .into(),
                });
            }
            if image_revision != state.reported_image_revision {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: format!(
                        "repair retained-image revision {image_revision} does not match the durable offered revision {}",
                        state.reported_image_revision
                    ),
                });
            }
            let pending_epoch = state.admission.pending_epoch() == Some(epoch);
            if (!pending_epoch && state.admission.epoch != epoch)
                || (!pending_epoch && through_seq < state.admission.applied_seq)
            {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "stale repair offer".into(),
                });
            }
            if !pending_epoch
                && through_seq > state.admission.applied_seq
                && anchor_chain != Some(state.admission.applied_chain)
            {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: format!(
                        "repair has no valid history proof at durable cursor {}; mint a new epoch",
                        state.admission.applied_seq
                    ),
                });
            }
            if !pending_epoch
                && through_seq == state.admission.applied_seq
                && through_chain != state.admission.applied_chain
            {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: format!(
                        "history fork at sequence {through_seq}: repair offer differs at the cursor"
                    ),
                });
            }
            if !(MIN_REPAIR_LEAF_BITS..=MAX_FLEET_LEAF_BITS).contains(&leaf_bits)
                || leaf_hashes.len() != 1usize << leaf_bits
            {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "invalid Merkle shape".into(),
                });
            }
            let existing = tx
                .prepare_cached(
                    "SELECT epoch, through_seq, through_chain, image_revision,
                            leaf_bits, leaves, hashes, remaining
                     FROM sync_replica_repairs WHERE source_id = ?1",
                )?
                .query_row(params![source.0], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Vec<u8>>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .optional()?;
            if let Some((
                requested_epoch,
                requested_seq,
                requested_chain,
                requested_revision,
                requested_bits,
                requested_leaves,
                requested_hashes,
                remaining,
            )) = existing
            {
                let identical_request = requested_epoch.as_slice() == epoch.as_bytes().as_slice()
                    && requested_seq as u64 == through_seq
                    && repair_chain_from_blob(requested_chain)? == through_chain
                    && requested_revision as u64 == image_revision
                    && requested_bits as u8 == leaf_bits;
                if identical_request {
                    let requested_leaves: Vec<u32> = serde_json::from_str(&requested_leaves)
                        .map_err(|error| {
                            CatalogError::InvalidState(format!(
                                "repair leaf scope is unreadable: {error}"
                            ))
                        })?;
                    let requested_hashes = decode_leaf_hashes(requested_hashes)?;
                    if requested_hashes.len() != requested_leaves.len() {
                        return Err(CatalogError::InvalidState(
                            "repair leaf scope and expected hashes have different lengths".into(),
                        ));
                    }
                    if requested_leaves.iter().zip(&requested_hashes).any(|(leaf, hash)| {
                        leaf_hashes.get(*leaf as usize) != Some(hash)
                    }) {
                        return Ok(RepairOfferOutcome::Rejected {
                            reason: "history fork: repeated repair offer changed its leaf manifest"
                                .into(),
                        });
                    }
                    let remaining: Vec<u32> = serde_json::from_str(&remaining).map_err(|error| {
                        CatalogError::InvalidState(format!(
                            "remaining repair scope is unreadable: {error}"
                        ))
                    })?;
                    tx.commit()?;
                    return Ok(RepairOfferOutcome::Request {
                        leaf_bits,
                        leaves: remaining,
                    });
                }
            }
            let local = replica_leaf_hashes_conn(&tx, source, leaf_bits)?;
            let leaves = if state.resync_target.is_some() {
                // Completing an epoch resync by repair must materialize every
                // current-epoch row before old epochs are retired. Equal
                // leaf digests alone cannot retag their stored row mappings.
                (0..local.len() as u32).collect::<Vec<_>>()
            } else {
                local
                    .iter()
                    .zip(leaf_hashes)
                    .enumerate()
                    .filter_map(|(leaf, (mine, theirs))| (mine != theirs).then_some(leaf as u32))
                    .collect::<Vec<_>>()
            };
            let requested_hashes = leaves
                .iter()
                .map(|leaf| leaf_hashes[*leaf as usize])
                .collect::<Vec<_>>();
            let requested_hashes = encode_leaf_hashes(&requested_hashes);
            tx.prepare_cached(
                "INSERT INTO sync_replica_repairs
                    (source_id, epoch, through_seq, through_chain, image_revision,
                     leaf_bits, leaves, hashes, remaining, last_part, requested_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '[]', ?10)
                 ON CONFLICT(source_id) DO UPDATE SET
                    epoch = excluded.epoch, through_seq = excluded.through_seq,
                    through_chain = excluded.through_chain,
                    image_revision = excluded.image_revision,
                    leaf_bits = excluded.leaf_bits,
                    leaves = excluded.leaves, hashes = excluded.hashes,
                    remaining = excluded.remaining, last_part = excluded.last_part,
                    requested_at = excluded.requested_at",
            )?
            .execute(params![
                source.0,
                epoch.as_bytes().as_slice(),
                through_seq as i64,
                through_chain.as_slice(),
                image_revision as i64,
                leaf_bits as i64,
                serde_json::to_string(&leaves)?,
                requested_hashes,
                serde_json::to_string(&leaves)?,
                UnixNanos::now().0,
            ])?;
            mark_completeness_conn(&tx, source, UnixNanos::now().0, false)?;
            tx.commit()?;
            Ok(RepairOfferOutcome::Request { leaf_bits, leaves })
        })
    }

    /// Apply the rows of the requested leaves authoritatively: rows present
    /// replace the replica's, rows absent from a requested leaf are
    /// tombstoned, and the cursor moves to the offered head.
    #[allow(clippy::too_many_arguments)]
    pub fn replica_apply_repair(
        &self,
        caller: [u8; 16],
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        image_revision: u64,
        leaf_bits: u8,
        leaves: &[u32],
        rows: &[SyncRow],
    ) -> Result<RepairOutcome> {
        self.replica_apply_repair_part(
            caller,
            source,
            epoch,
            through_seq,
            through_chain,
            image_revision,
            leaf_bits,
            leaves,
            rows,
            true,
        )
    }

    /// Apply one bounded part of a durable Merkle repair. Each part owns a
    /// disjoint subset of the outstanding leaves; the cursor and completeness
    /// marker advance only with the final part.
    #[allow(clippy::too_many_arguments)]
    pub fn replica_apply_repair_part(
        &self,
        caller: [u8; 16],
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        image_revision: u64,
        leaf_bits: u8,
        leaves: &[u32],
        rows: &[SyncRow],
        final_part: bool,
    ) -> Result<RepairOutcome> {
        let unique = rows.iter().map(|r| r.object).collect::<BTreeSet<_>>().len() == rows.len();
        let unique_leaves = leaves.iter().copied().collect::<BTreeSet<_>>().len() == leaves.len();
        let entry_count = wire_entry_count(rows);
        if !(MIN_REPAIR_LEAF_BITS..=MAX_FLEET_LEAF_BITS).contains(&leaf_bits)
            || through_seq > MAX_SQLITE_SEQUENCE
            || rows.len() > MAX_APPLY_ROWS
            || entry_count.is_none_or(|count| count > MAX_APPLY_ENTRIES)
            || leaves.iter().any(|l| *l >= (1u32 << leaf_bits))
            || rows.iter().any(|r| {
                r.seq > through_seq || r.generation > u64::from(u32::MAX) || !wire_row_is_valid(r)
            })
            || !unique
            || !unique_leaves
            || (!final_part && leaves.is_empty())
        {
            return Ok(RepairOutcome::Rejected {
                reason: "malformed repair response".into(),
            });
        }
        let wanted: BTreeSet<u32> = leaves.iter().copied().collect();
        if rows
            .iter()
            .any(|r| !wanted.contains(&leaf_index(leaf_bits, r.object)))
        {
            return Ok(RepairOutcome::Rejected {
                reason: "repair row outside requested leaf".into(),
            });
        }
        self.with_writer(|conn| {
            register_merkle_leaf_function(conn)?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if source_is_retired_conn(&tx, source)? {
                return Ok(RepairOutcome::Rejected {
                    reason: "replica source is retired".into(),
                });
            }
            if !source_peer_is_authorized_conn(&tx, source, &caller)? {
                return Ok(RepairOutcome::Rejected {
                    reason: "replica peer is disabled or no longer enrolled".into(),
                });
            }
            if through_seq != state.reported_head {
                return Ok(RepairOutcome::Rejected {
                    reason: format!(
                        "repair head {through_seq} does not match the durable offered head {}",
                        state.reported_head
                    ),
                });
            }
            if state.reported_chain != Some(through_chain) {
                return Ok(RepairOutcome::Rejected {
                    reason: "history fork: repair chain does not match the durable offered head"
                        .into(),
                });
            }
            if image_revision != state.reported_image_revision {
                return Ok(RepairOutcome::Rejected {
                    reason: format!(
                        "repair retained-image revision {image_revision} does not match the durable offered revision {}",
                        state.reported_image_revision
                    ),
                });
            }
            let pending_epoch = state.admission.pending_epoch() == Some(epoch);
            if (!pending_epoch && state.admission.epoch != epoch)
                || (!pending_epoch && through_seq < state.admission.applied_seq)
            {
                return Ok(RepairOutcome::Rejected {
                    reason: "stale repair rows".into(),
                });
            }
            if rows.iter().any(|row| {
                row.image.as_ref().is_some_and(|image| {
                    image.object.id != row.object
                        || image.object.source_id != state.remote_source_id
                })
            }) {
                return Ok(RepairOutcome::Rejected {
                    reason: "row image identity does not match its enclosing row and source".into(),
                });
            }
            if !pending_epoch
                && through_seq == state.admission.applied_seq
                && through_chain != state.admission.applied_chain
            {
                return Ok(RepairOutcome::Rejected {
                    reason: format!(
                        "history fork at sequence {through_seq}: repair rows differ at the cursor"
                    ),
                });
            }
            let response_hashes = wire_leaf_hashes(rows, leaf_bits)?;
            let empty_hash = MerkleLeafHasher::new().finalize();
            let request = tx
                .prepare_cached(
                    "SELECT epoch, through_seq, through_chain, image_revision,
                            leaf_bits, leaves, hashes, remaining, last_part
                     FROM sync_replica_repairs WHERE source_id = ?1",
                )?
                .query_row(params![source.0], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Vec<u8>>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, String>(8)?,
                    ))
                })
                .optional()?;
            let Some((requested_epoch, requested_seq, requested_chain, requested_revision, requested_bits, requested_leaves, requested_hashes, remaining_leaves, last_part)) = request else {
                return Ok(RepairOutcome::Rejected {
                    reason: "repair response has no outstanding request".into(),
                });
            };
            let requested_leaves: Vec<u32> = serde_json::from_str(&requested_leaves).map_err(|e| {
                CatalogError::InvalidState(format!("repair leaf scope is unreadable: {e}"))
            })?;
            let requested_hashes = decode_leaf_hashes(requested_hashes)?;
            if requested_hashes.len() != requested_leaves.len() {
                return Err(CatalogError::InvalidState(
                    "repair leaf scope and expected hashes have different lengths".into(),
                ));
            }
            let expected_by_leaf = requested_leaves
                .iter()
                .copied()
                .zip(requested_hashes)
                .collect::<BTreeMap<_, _>>();
            let requested_scope: BTreeSet<u32> = requested_leaves.into_iter().collect();
            if expected_by_leaf.len() != requested_scope.len() {
                return Err(CatalogError::InvalidState(
                    "repair leaf scope contains duplicates".into(),
                ));
            }
            let remaining_leaves: Vec<u32> = serde_json::from_str(&remaining_leaves).map_err(|e| {
                CatalogError::InvalidState(format!("remaining repair scope is unreadable: {e}"))
            })?;
            let remaining_scope: BTreeSet<u32> = remaining_leaves.iter().copied().collect();
            if remaining_scope.len() != remaining_leaves.len()
                || !remaining_scope.is_subset(&requested_scope)
            {
                return Err(CatalogError::InvalidState(
                    "remaining repair scope is invalid".into(),
                ));
            }
            let last_part: Vec<u32> = serde_json::from_str(&last_part).map_err(|e| {
                CatalogError::InvalidState(format!("last repair part is unreadable: {e}"))
            })?;
            let last_scope: BTreeSet<u32> = last_part.iter().copied().collect();
            if last_scope.len() != last_part.len() || !last_scope.is_subset(&requested_scope) {
                return Err(CatalogError::InvalidState(
                    "last repair part scope is invalid".into(),
                ));
            }
            let applying_new_part = wanted.is_subset(&remaining_scope);
            let replaying_applied_part = wanted == last_scope && !wanted.is_empty();
            if requested_epoch.as_slice() != epoch.as_bytes().as_slice()
                || requested_seq as u64 != through_seq
                || repair_chain_from_blob(requested_chain)? != through_chain
                || requested_revision as u64 != image_revision
                || requested_bits as u8 != leaf_bits
                || (!applying_new_part && !replaying_applied_part)
                || (applying_new_part && final_part != (wanted == remaining_scope))
                || (replaying_applied_part && final_part != remaining_scope.is_empty())
            {
                return Ok(RepairOutcome::Rejected {
                    reason: "repair response does not match the outstanding request".into(),
                });
            }
            if leaves.iter().any(|leaf| {
                response_hashes.get(leaf).unwrap_or(&empty_hash)
                    != expected_by_leaf
                        .get(leaf)
                        .expect("wanted leaves were checked against the requested scope")
            }) {
                return Ok(RepairOutcome::Rejected {
                    reason: "repair response does not match the requested Merkle leaf hash".into(),
                });
            }
            if replaying_applied_part {
                tx.commit()?;
                return if final_part {
                    Ok(RepairOutcome::Applied {
                        through_seq,
                        replaced: 0,
                        removed: 0,
                    })
                } else {
                    Ok(RepairOutcome::Staged {
                        replaced: 0,
                        removed: 0,
                        remaining_leaves: remaining_scope.len() as u64,
                    })
                };
            }
            let now = UnixNanos::now().0;
            let sync_epoch = SyncEpoch::from_source_epoch(epoch);
            let present: BTreeSet<ObjectId> = rows.iter().map(|r| r.object).collect();
            let mut removed = 0;
            // During an epoch rebuild, keep every old-epoch mapping until all
            // repair parts have run native matching. Final bounded retirement
            // then removes what was truly absent. Incremental repair has no
            // old epoch to retire, so preflight its entire authoritative leaf
            // scope and reject before mutation if it exceeds the write bound.
            let known = if pending_epoch {
                Vec::new()
            } else {
                let leaves_json = serde_json::to_string(leaves)?;
                let known = tx
                    .prepare_cached(
                        "SELECT remote_object_id, local_object_id, deleted
                         FROM sync_replica_rows
                         WHERE source_id = ?1 AND placeholder = 0
                           AND eidos_merkle_leaf(?2, remote_object_id) IN
                                (SELECT value FROM json_each(?3))
                         ORDER BY remote_object_id LIMIT ?4",
                    )?
                    .query_map(
                        params![
                            source.0,
                            leaf_bits as i64,
                            leaves_json,
                            (MAX_APPLY_ROWS + 1) as i64
                        ],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, i64>(1)?,
                                r.get::<_, i64>(2)?,
                            ))
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if known.len() > MAX_APPLY_ROWS {
                    return Ok(RepairOutcome::Rejected {
                        reason: format!(
                            "repair leaf scope exceeds the {MAX_APPLY_ROWS}-mapping transaction limit"
                        ),
                    });
                }
                known
            };
            let mut counts = ApplyCounts::default();
            prepare_rows_conn(&tx, source, &sync_epoch, rows, true)?;
            for row in rows {
                if let Some(reason) =
                    apply_row_conn(&tx, source, &sync_epoch, row, true, now, &mut counts)?
                {
                    return Ok(RepairOutcome::Rejected { reason });
                }
            }
            for (remote, _, _) in known {
                if present.contains(&ObjectId(remote)) {
                    continue;
                }
                // Native remapping above may have moved a mapping away from
                // this absent remote id. Delete whichever mapping, if any,
                // still occupies the authoritative absent id.
                if let Some(state) = row_state_conn(&tx, source, ObjectId(remote))? {
                    if !state.placeholder {
                        let deleted: bool = tx.query_row(
                            "SELECT deleted != 0 FROM sync_replica_rows
                             WHERE source_id = ?1 AND remote_object_id = ?2",
                            params![source.0, remote],
                            |r| r.get(0),
                        )?;
                        if !deleted {
                            tx.prepare_cached(
                                "UPDATE objects SET deleted_at = COALESCE(deleted_at, ?2) WHERE object_id = ?1",
                            )?
                            .execute(params![state.local.0, now])?;
                            tx.prepare_cached(
                                "UPDATE entries SET deleted_at = ?2 WHERE object_id = ?1 AND deleted_at IS NULL",
                            )?
                            .execute(params![state.local.0, now])?;
                            outbox_append_conn(&tx, source, state.local, "delete", 0)?;
                        }
                    }
                    tx.prepare_cached(
                        "DELETE FROM sync_replica_rows WHERE source_id = ?1 AND remote_object_id = ?2",
                    )?
                    .execute(params![source.0, remote])?;
                    removed += 1;
                }
            }
            let remaining = remaining_scope
                .difference(&wanted)
                .copied()
                .collect::<Vec<_>>();
            if !final_part {
                tx.prepare_cached(
                    "UPDATE sync_replica_repairs SET remaining = ?2, last_part = ?3 WHERE source_id = ?1",
                )?
                .execute(params![
                    source.0,
                    serde_json::to_string(&remaining)?,
                    serde_json::to_string(leaves)?
                ])?;
                mark_applied_conn(&tx, source, now, false)?;
                tx.commit()?;
                return Ok(RepairOutcome::Staged {
                    replaced: counts.applied,
                    removed,
                    remaining_leaves: remaining.len() as u64,
                });
            }
            if pending_epoch {
                if !state
                    .admission
                    .snapshot_applied(epoch, through_seq, through_chain)
                {
                    return Ok(RepairOutcome::Rejected {
                        reason: "repair snapshot was not requested for this epoch".into(),
                    });
                }
            } else {
                state.admission.applied(through_seq, through_chain);
            }
            if state
                .resync_target
                .is_some_and(|target| state.admission.applied_seq >= target)
            {
                let (_, done) = retire_other_epochs_conn(&tx, source, &sync_epoch, now)?;
                if done {
                    state.resync_target = None;
                }
            }
            store_admission_conn(&tx, source, &state.admission, state.resync_target)?;
            tx.prepare_cached(
                "UPDATE sync_replica_sources SET applied_image_revision = ?2
                 WHERE source_id = ?1",
            )?
            .execute(params![source.0, image_revision as i64])?;
            tx.prepare_cached(
                "UPDATE sync_replica_repairs SET remaining = '[]', last_part = ?2
                 WHERE source_id = ?1",
            )?
            .execute(params![source.0, serde_json::to_string(leaves)?])?;
            let image_complete = state.resync_target.is_none()
                && state.admission.applied_seq >= state.reported_head
                && image_revision >= state.reported_image_revision
                && replica_topology_is_closed_conn(&tx, source, &sync_epoch)?;
            mark_applied_conn(&tx, source, now, false)?;
            if image_complete {
                mark_aggregates_pending_conn(&tx, source, now)?;
            }
            tx.commit()?;
            Ok(RepairOutcome::Applied {
                through_seq,
                replaced: counts.applied,
                removed,
            })
        })
    }

    /// Merkle records of a replicated source, for tests and status.
    pub fn replica_digests(&self, source: SourceId) -> Result<Vec<RecordDigest>> {
        self.with_reader(|conn| replica_digests_conn(conn, source))
    }

    /// Recompute directory aggregates of a replicated source from its
    /// applied rows. Bounded by the source size; callers throttle it.
    pub fn replica_rebuild_aggregates(&self, source: SourceId) -> Result<()> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if source_is_retired_conn(&tx, source)? {
                return Ok(());
            }
            let root: Option<i64> = tx
                .query_row(
                    "SELECT root_object_id FROM sources WHERE source_id = ?1",
                    params![source.0],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(root) = root {
                crate::aggregates::rebuild_source(
                    &tx,
                    source,
                    ObjectId(root),
                    REMOTE_GENERATION,
                    &Default::default(),
                )?;
                // Earlier row events may already have reached the follower
                // with stale or absent directory totals. Re-project the
                // complete tree after aggregate rows are durable.
                outbox_append_conn(&tx, source, ObjectId(root), "subtree", REMOTE_GENERATION)?;
            }
            let epoch = SyncEpoch::from_source_epoch(state.admission.epoch);
            let image_complete = state.resync_target.is_none()
                && state.admission.applied_seq >= state.reported_head
                && state.applied_image_revision >= state.reported_image_revision
                && !repair_pending_conn(&tx, source)?
                && replica_topology_is_closed_conn(&tx, source, &epoch)?;
            mark_completeness_conn(&tx, source, UnixNanos::now().0, image_complete)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Continue a bounded old-epoch retirement after the new image reached
    /// its offered head. Returns rows retired in this step.
    pub fn replica_continue_retirement(&self, source: SourceId) -> Result<u64> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if source_is_retired_conn(&tx, source)? {
                return Ok(0);
            }
            let Some(target) = state.resync_target else {
                return Ok(0);
            };
            if state.admission.applied_seq < target
                || state.admission.applied_seq < state.reported_head
                || state.reported_chain.is_none()
            {
                return Ok(0);
            }
            let epoch = SyncEpoch::from_source_epoch(state.admission.epoch);
            let now = UnixNanos::now().0;
            let (retired, done) = retire_other_epochs_conn(&tx, source, &epoch, now)?;
            if done {
                state.resync_target = None;
                store_admission_conn(&tx, source, &state.admission, None)?;
                let image_complete = state.admission.applied_seq >= state.reported_head
                    && state.applied_image_revision >= state.reported_image_revision
                    && !repair_pending_conn(&tx, source)?
                    && replica_topology_is_closed_conn(&tx, source, &epoch)?;
                mark_applied_conn(&tx, source, now, false)?;
                if image_complete {
                    mark_aggregates_pending_conn(&tx, source, now)?;
                }
            }
            tx.commit()?;
            Ok(retired)
        })
    }

    /// Retire a replicated source: its rows disappear from search and the
    /// replica mapping is dropped. A later offer starts from scratch.
    pub fn replica_retire_source(&self, source: SourceId) -> Result<bool> {
        let mut found = false;
        loop {
            let (exists, done) = self.with_writer(|conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let exists = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_replica_sources WHERE source_id = ?1)",
                    params![source.0],
                    |r| r.get::<_, i64>(0),
                )? != 0;
                if !exists {
                    return Ok((false, true));
                }
                let now = UnixNanos::now().0;
                tx.execute(
                    "UPDATE sources SET state = ?2,
                         state_reason = 'retired from the fleet', updated_at = ?3
                     WHERE source_id = ?1",
                    params![source.0, SourceState::Retired.as_str(), now],
                )?;
                // Fence and discard repair state before mapping cleanup. All
                // protocol writers reject a retired source between chunks.
                tx.execute(
                    "DELETE FROM sync_replica_repairs WHERE source_id = ?1",
                    params![source.0],
                )?;
                tx.execute(
                    "DELETE FROM sync_replica_rows
                     WHERE source_id = ?1 AND remote_object_id IN (
                         SELECT remote_object_id FROM sync_replica_rows
                         WHERE source_id = ?1 ORDER BY remote_object_id LIMIT ?2)",
                    params![source.0, RETIRE_STEP_ROWS as i64],
                )?;
                let remaining = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_replica_rows WHERE source_id = ?1)",
                    params![source.0],
                    |r| r.get::<_, i64>(0),
                )? != 0;
                if !remaining {
                    tx.execute(
                        "DELETE FROM sync_replica_sources WHERE source_id = ?1",
                        params![source.0],
                    )?;
                }
                tx.commit()?;
                Ok((true, !remaining))
            })?;
            if !exists {
                return Ok(found);
            }
            found = true;
            if done {
                return Ok(true);
            }
            std::thread::yield_now();
        }
    }

    /// Every replicated source of a peer.
    pub fn replica_sources_of(&self, node_id: [u8; 16]) -> Result<Vec<ReplicaSourceState>> {
        Ok(self
            .replica_sources()?
            .into_iter()
            .filter(|s| s.node_id == node_id)
            .collect())
    }
}
