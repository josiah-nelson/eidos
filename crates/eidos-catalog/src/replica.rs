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
    record_digest, SyncBatch, SyncEpoch, SyncRow, SyncRowImage, SYNC_ROW_IMAGE_VERSION,
};
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{
    extension_of, ContentState, HostId, ObjectId, ObjectKind, SourceId, SourceKind, SourceState,
    UnixNanos,
};
use eidos_sync::identity::{
    AdmissionState, BatchDecision, ChainHash, HelloDecision, SourceEpoch, CHAIN_GENESIS,
};
use eidos_sync::merkle::{leaf_index, RecordDigest, MAX_FLEET_LEAF_BITS};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// What a peer says about one of its sources when it offers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSourceDescriptor {
    pub remote_source_id: SourceId,
    pub name: String,
    pub kind: SourceKind,
    pub root_path: String,
    #[serde(default)]
    pub aliases: Vec<String>,
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
    pub reported_compacted: u64,
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
}

const REMOTE_GENERATION: i64 = 1;
const MAX_SQLITE_SEQUENCE: u64 = i64::MAX as u64;

fn admission_from_json(json: &str) -> Result<AdmissionState> {
    serde_json::from_str(json).map_err(|e| {
        CatalogError::InvalidState(format!("replica admission state is unreadable: {e}"))
    })
}

fn admission_to_json(state: &AdmissionState) -> Result<String> {
    Ok(serde_json::to_string(state)?)
}

fn node_id_from_blob(blob: Vec<u8>) -> Result<[u8; 16]> {
    blob.try_into()
        .map_err(|_| CatalogError::InvalidState("fleet node id is not 16 bytes".into()))
}

fn state_conn(conn: &Connection, source: SourceId) -> Result<Option<ReplicaSourceState>> {
    conn.prepare_cached(
        "SELECT node_id, remote_source_id, admission, resync_target, reported_head,
                reported_compacted, reported_at, applied_at, image_version
         FROM sync_replica_sources WHERE source_id = ?1",
    )?
    .query_row(params![source.0], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })
    .optional()?
    .map(
        |(node, remote, admission, target, head, compacted, reported, applied, version)| {
            Ok(ReplicaSourceState {
                source_id: source,
                node_id: node_id_from_blob(node)?,
                remote_source_id: SourceId(remote),
                admission: admission_from_json(&admission)?,
                resync_target: target.map(|t| t as u64),
                reported_head: head as u64,
                reported_compacted: compacted as u64,
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
        if placeholder && stored_epoch.as_slice() != epoch.0.as_slice() {
            tx.prepare_cached(
                "UPDATE sync_replica_rows SET epoch = ?3
                 WHERE source_id = ?1 AND remote_object_id = ?2",
            )?
            .execute(params![source.0, remote.0, epoch.0.as_slice()])?;
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
            generation, deleted, placeholder)
         VALUES (?1, ?2, ?3, ?4, 0, 0, 0, 1)",
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
) -> Result<()> {
    let mut existing = row_state_conn(tx, source, row.object)?;
    // A rebuilt catalog may assign a new remote object id while preserving
    // the filesystem's stable native identity. Reuse that local object before
    // applying the new epoch row; otherwise the still-live old epoch row
    // collides with the source-scoped native-identity unique index.
    if existing.is_none() {
        if let Some(native) = row.image.as_ref().and_then(|image| image.object.native) {
            let old_remote = tx
                .prepare_cached(
                    "SELECT rr.remote_object_id
                     FROM sync_replica_rows rr
                     JOIN objects o ON o.object_id = rr.local_object_id
                     WHERE rr.source_id = ?1 AND rr.epoch != ?2 AND rr.placeholder = 0
                       AND rr.deleted = 0 AND o.deleted_at IS NULL
                       AND o.native_volume_serial = ?3 AND o.native_id_high = ?4
                       AND o.native_id_low = ?5",
                )?
                .query_row(
                    params![
                        source.0,
                        epoch.0.as_slice(),
                        native.volume_serial as i64,
                        native.file_id_high as i64,
                        native.file_id_low as i64,
                    ],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(old_remote) = old_remote {
                tx.prepare_cached(
                    "UPDATE sync_replica_rows SET remote_object_id = ?3
                     WHERE source_id = ?1 AND remote_object_id = ?2",
                )?
                .execute(params![source.0, old_remote, row.object.0])?;
                existing = row_state_conn(tx, source, row.object)?;
            }
        }
    }
    if let Some(state) = &existing {
        // Sequences only order rows within one epoch; a row of another
        // epoch is always superseded by the epoch being streamed.
        if !state.placeholder
            && !authoritative
            && state.epoch.as_slice() == epoch.0.as_slice()
            && state.seq >= row.seq
        {
            return Ok(());
        }
    }
    let was_placeholder = existing.as_ref().is_some_and(|s| s.placeholder);
    let local = match &existing {
        Some(state) => state.local,
        None => local_object_conn(tx, source, epoch, row.object)?,
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
        "UPDATE sync_replica_rows SET epoch = ?3, seq = ?4, generation = ?5, deleted = ?6, placeholder = 0
         WHERE source_id = ?1 AND remote_object_id = ?2",
    )?
    .execute(params![
        source.0,
        row.object.0,
        epoch.0.as_slice(),
        row.seq as i64,
        row.generation as i64,
        row.image.is_none() as i64,
    ])?;
    counts.applied += 1;
    Ok(())
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
) -> Result<u64> {
    let stale: Vec<(i64, i64, bool, bool)> = tx
        .prepare_cached(
            "SELECT remote_object_id, local_object_id, deleted, placeholder
             FROM sync_replica_rows WHERE source_id = ?1 AND epoch != ?2",
        )?
        .query_map(params![source.0, epoch.0.as_slice()], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, i64>(3)? != 0,
            ))
        })?
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
    Ok(stale.len() as u64)
}

fn mark_applied_conn(tx: &Transaction<'_>, source: SourceId, now: i64) -> Result<()> {
    tx.prepare_cached(
        "UPDATE sync_replica_sources SET applied_at = ?2, updated_at = ?2 WHERE source_id = ?1",
    )?
    .execute(params![source.0, now])?;
    // The first applied batch publishes the source: from here on the index
    // follower projects it and search reports it as metadata complete.
    tx.prepare_cached(
        "UPDATE sources SET published_generation = ?2, state = ?3, state_reason = NULL,
            last_scan_completed_at = COALESCE(last_scan_completed_at, ?4), updated_at = ?4
         WHERE source_id = ?1 AND (published_generation IS NULL OR state != ?3)",
    )?
    .execute(params![
        source.0,
        REMOTE_GENERATION,
        SourceState::MetadataComplete.as_str(),
        now
    ])?;
    Ok(())
}

fn replica_digests_conn(conn: &Connection, source: SourceId) -> Result<Vec<RecordDigest>> {
    Ok(conn
        .prepare_cached(
            "SELECT remote_object_id, generation, deleted FROM sync_replica_rows
             WHERE source_id = ?1 AND placeholder = 0 ORDER BY remote_object_id",
        )?
        .query_map(params![source.0], |r| {
            Ok(record_digest(
                ObjectId(r.get(0)?),
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

impl Catalog {
    /// Make sure a peer's source has a local `sources` row and a durable
    /// cursor, creating both on first contact. Returns the replica state.
    pub fn replica_ensure_source(
        &self,
        node: &RemoteNode,
        descriptor: &RemoteSourceDescriptor,
        epoch: SourceEpoch,
    ) -> Result<ReplicaSourceState> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
                            content_enabled, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'new', 'awaiting the first replicated batch', 0, ?6, ?6)",
                        params![
                            host.0,
                            name,
                            SourceKind::Remote.as_str(),
                            descriptor.root_path,
                            serde_json::to_string(&descriptor.aliases)?,
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
        self.with_reader(|conn| {
            let Some(state) = state_conn(conn, source)? else {
                return Ok(None);
            };
            let node_name: String = conn
                .query_row(
                    "SELECT name FROM fleet_peers WHERE node_id = ?1",
                    params![state.node_id.as_slice()],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_else(|| "unknown node".into());
            Ok(Some(ReplicaCoverage {
                node_id: state.node_id,
                node_name,
                remote_source_id: state.remote_source_id,
                epoch: state.admission.epoch,
                applied_seq: state.admission.applied_seq,
                reported_head: state.reported_head,
                applied_at: state.applied_at,
                reported_at: state.reported_at,
                resyncing: state.resync_target.is_some(),
                alarms: 0,
            }))
        })
    }

    /// Admit a peer's offer of a source (the protocol's `Hello`) and commit
    /// the resulting cursor before answering.
    pub fn replica_admit_hello(
        &self,
        source: SourceId,
        epoch: SourceEpoch,
        head_seq: u64,
        head_chain: ChainHash,
        compacted_through: u64,
    ) -> Result<HelloOutcome> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            let now = UnixNanos::now().0;
            let pending_rewind = state.admission.pending_epoch() == Some(epoch)
                && state.reported_at.is_some()
                && head_seq < state.reported_head;
            let malformed = head_seq > MAX_SQLITE_SEQUENCE
                || compacted_through > MAX_SQLITE_SEQUENCE
                || compacted_through > head_seq
                || (head_seq == 0 && head_chain != CHAIN_GENESIS);
            let outcome = if malformed {
                HelloOutcome::Rejected {
                    reason: "malformed source head".into(),
                }
            } else if pending_rewind {
                HelloOutcome::Rejected {
                    reason: format!(
                        "same-epoch sequence rewind during resync: central={}, source={head_seq}",
                        state.reported_head
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
                    requires_repair: after_seq < compacted_through,
                },
                HelloDecision::FullResync { new_epoch, .. } => {
                    // Every live object of the new incarnation is stamped at
                    // or below the head offered now; once the stream passes
                    // it, rows of other epochs are authoritatively absent.
                    state.resync_target = Some(
                        state
                            .resync_target
                            .map_or(head_seq, |target| target.max(head_seq)),
                    );
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
                tx.execute(
                    "UPDATE sync_replica_sources SET reported_head = ?2, reported_compacted = ?3, reported_at = ?4
                     WHERE source_id = ?1",
                    params![source.0, head_seq as i64, compacted_through as i64, now],
                )?;
            }
            store_admission_conn(&tx, source, &state.admission, state.resync_target)?;
            tx.commit()?;
            Ok(outcome)
        })
    }

    /// Apply a materialized batch: admission, effects, cursor, one
    /// transaction. `Applied` means the acknowledgement may be sent. The
    /// batch's `source_id` is the peer's; `source` is the local replica.
    pub fn replica_apply_batch(&self, source: SourceId, batch: &SyncBatch) -> Result<BatchOutcome> {
        let epoch = batch.epoch.to_source_epoch();
        let (after_seq, after_chain, through_seq, through_chain) = (
            batch.after_seq,
            batch.after_chain,
            batch.through_seq,
            batch.through_chain,
        );
        let rows = batch.rows.as_slice();
        let unique = rows.iter().map(|r| r.object).collect::<BTreeSet<_>>().len() == rows.len();
        let empty_snapshot = after_seq == 0
            && through_seq == 0
            && batch.head_seq == 0
            && after_chain == CHAIN_GENESIS
            && through_chain == CHAIN_GENESIS
            && rows.is_empty();
        if (after_seq >= through_seq && !empty_snapshot)
            || !unique
            || after_seq > MAX_SQLITE_SEQUENCE
            || through_seq > MAX_SQLITE_SEQUENCE
            || batch.head_seq > MAX_SQLITE_SEQUENCE
            || through_seq > batch.head_seq
            || rows.iter().any(|r| {
                r.seq <= after_seq || r.seq > through_seq || r.generation > MAX_SQLITE_SEQUENCE
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
            if batch.source_id != state.remote_source_id {
                return Ok(BatchOutcome::Rejected {
                    reason: format!(
                        "batch source {} does not match replica source {}",
                        batch.source_id, state.remote_source_id
                    ),
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
            for row in rows {
                apply_row_conn(&tx, source, &sync_epoch, row, false, now, &mut counts)?;
            }
            let mut retired = 0;
            if let Some(target) = state.resync_target {
                if state.admission.applied_seq >= target && through_seq == batch.head_seq {
                    retired = retire_other_epochs_conn(&tx, source, &sync_epoch, now)?;
                    state.resync_target = None;
                }
            }
            store_admission_conn(&tx, source, &state.admission, state.resync_target)?;
            mark_applied_conn(&tx, source, now)?;
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
    pub fn replica_repair_offer(
        &self,
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaf_hashes: &[[u8; 32]],
    ) -> Result<RepairOfferOutcome> {
        self.with_reader(|conn| {
            let state = state_conn(conn, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if state.admission.epoch != epoch || through_seq < state.admission.applied_seq {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "stale repair offer".into(),
                });
            }
            if through_seq == state.admission.applied_seq
                && through_chain != state.admission.applied_chain
            {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: format!(
                        "history fork at sequence {through_seq}: repair offer differs at the cursor"
                    ),
                });
            }
            if leaf_bits > MAX_FLEET_LEAF_BITS || leaf_hashes.len() != 1usize << leaf_bits {
                return Ok(RepairOfferOutcome::Rejected {
                    reason: "invalid Merkle shape".into(),
                });
            }
            let tree = eidos_sync::merkle::MerkleTree::with_leaf_bits(
                leaf_bits,
                replica_digests_conn(conn, source)?,
            );
            let local = tree.leaf_hashes();
            let leaves = local
                .iter()
                .zip(leaf_hashes)
                .enumerate()
                .filter_map(|(leaf, (mine, theirs))| (mine != theirs).then_some(leaf as u32))
                .collect();
            Ok(RepairOfferOutcome::Request { leaf_bits, leaves })
        })
    }

    /// Apply the rows of the requested leaves authoritatively: rows present
    /// replace the replica's, rows absent from a requested leaf are
    /// tombstoned, and the cursor moves to the offered head.
    #[allow(clippy::too_many_arguments)]
    pub fn replica_apply_repair(
        &self,
        source: SourceId,
        epoch: SourceEpoch,
        through_seq: u64,
        through_chain: ChainHash,
        leaf_bits: u8,
        leaves: &[u32],
        rows: &[SyncRow],
    ) -> Result<RepairOutcome> {
        let unique = rows.iter().map(|r| r.object).collect::<BTreeSet<_>>().len() == rows.len();
        if leaf_bits > MAX_FLEET_LEAF_BITS
            || through_seq > MAX_SQLITE_SEQUENCE
            || leaves.iter().any(|l| *l >= (1u32 << leaf_bits))
            || rows
                .iter()
                .any(|r| r.seq > through_seq || r.generation > MAX_SQLITE_SEQUENCE)
            || !unique
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
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut state = state_conn(&tx, source)?.ok_or_else(|| {
                CatalogError::InvalidState(format!("source {source} is not a replica"))
            })?;
            if state.admission.epoch != epoch || through_seq < state.admission.applied_seq {
                return Ok(RepairOutcome::Rejected {
                    reason: "stale repair rows".into(),
                });
            }
            if through_seq == state.admission.applied_seq
                && through_chain != state.admission.applied_chain
            {
                return Ok(RepairOutcome::Rejected {
                    reason: format!(
                        "history fork at sequence {through_seq}: repair rows differ at the cursor"
                    ),
                });
            }
            let now = UnixNanos::now().0;
            let sync_epoch = SyncEpoch::from_source_epoch(epoch);
            let present: BTreeSet<ObjectId> = rows.iter().map(|r| r.object).collect();
            let known: Vec<(i64, i64, i64)> = tx
                .prepare_cached(
                    "SELECT remote_object_id, local_object_id, deleted FROM sync_replica_rows
                     WHERE source_id = ?1 AND placeholder = 0",
                )?
                .query_map(params![source.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut removed = 0;
            for (remote, local, deleted) in known {
                let object = ObjectId(remote);
                if !wanted.contains(&leaf_index(leaf_bits, object)) || present.contains(&object) {
                    continue;
                }
                if deleted == 0 {
                    tx.prepare_cached(
                        "UPDATE objects SET deleted_at = COALESCE(deleted_at, ?2) WHERE object_id = ?1",
                    )?
                    .execute(params![local, now])?;
                    tx.prepare_cached(
                        "UPDATE entries SET deleted_at = ?2 WHERE object_id = ?1 AND deleted_at IS NULL",
                    )?
                    .execute(params![local, now])?;
                    outbox_append_conn(&tx, source, ObjectId(local), "delete", 0)?;
                }
                // Authoritative absence: the source has no row at all, so
                // neither does the replica's Merkle image.
                tx.prepare_cached(
                    "DELETE FROM sync_replica_rows WHERE source_id = ?1 AND remote_object_id = ?2",
                )?
                .execute(params![source.0, remote])?;
                removed += 1;
            }
            let mut counts = ApplyCounts::default();
            for row in rows {
                apply_row_conn(&tx, source, &sync_epoch, row, true, now, &mut counts)?;
            }
            state.admission.applied(through_seq, through_chain);
            if state
                .resync_target
                .is_some_and(|target| state.admission.applied_seq >= target)
            {
                retire_other_epochs_conn(&tx, source, &sync_epoch, now)?;
                state.resync_target = None;
            }
            store_admission_conn(&tx, source, &state.admission, state.resync_target)?;
            mark_applied_conn(&tx, source, now)?;
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
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Retire a replicated source: its rows disappear from search and the
    /// replica mapping is dropped. A later offer starts from scratch.
    pub fn replica_retire_source(&self, source: SourceId) -> Result<bool> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = UnixNanos::now().0;
            let n = tx.execute(
                "DELETE FROM sync_replica_sources WHERE source_id = ?1",
                params![source.0],
            )?;
            if n == 0 {
                return Ok(false);
            }
            tx.execute(
                "DELETE FROM sync_replica_rows WHERE source_id = ?1",
                params![source.0],
            )?;
            tx.execute(
                "UPDATE sources SET state = ?2, state_reason = 'retired from the fleet', updated_at = ?3
                 WHERE source_id = ?1",
                params![source.0, SourceState::Retired.as_str(), now],
            )?;
            tx.commit()?;
            Ok(true)
        })
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
