//! Incremental change application.
//!
//! Change feeds (USN today; FSEvents/fanotify later) are normalised by the
//! scanner layer into [`ChangeEvent`]s keyed by native identity. Applying a
//! batch is one transaction: catalog rows, aggregate deltas, outbox rows for
//! derived indexes, and the feed checkpoint all commit together, so a crash
//! can never leave the checkpoint ahead of the durable state.

use crate::aggregates::{apply_delta, AggDelta};
use crate::jobs::outbox_append_conn;
use crate::policy::{ContentDecision, PolicyCtx, PolicyEngine};
use crate::read::get_source_conn;
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{
    extension_of, ContentState, FileAttributes, IdentityConfidence, NativeIdentity, ObjectId,
    ObjectKind, PolicyStage, SourceId, UnixNanos,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Native identity key (volume serial + 128-bit reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NativeKey {
    pub volume_serial: u64,
    pub id: u128,
}

impl From<NativeIdentity> for NativeKey {
    fn from(n: NativeIdentity) -> Self {
        NativeKey {
            volume_serial: n.volume_serial,
            id: n.file_id_u128(),
        }
    }
}

impl NativeKey {
    fn parts(&self) -> (i64, i64, i64) {
        (
            self.volume_serial as i64,
            (self.id >> 64) as u64 as i64,
            self.id as u64 as i64,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectSnapshot {
    pub native: NativeIdentity,
    pub kind: ObjectKind,
    pub attributes: FileAttributes,
    pub size: u64,
    pub allocated: u64,
    pub link_count: u32,
    pub created: Option<UnixNanos>,
    pub modified: Option<UnixNanos>,
    pub changed: Option<UnixNanos>,
    pub accessed: Option<UnixNanos>,
    pub reparse_tag: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeEvent {
    /// Entry `(parent, name)` now refers to the object described by `snapshot`.
    Link {
        parent: NativeKey,
        name: String,
        snapshot: ObjectSnapshot,
    },
    /// Entry `(parent, name)` no longer exists. The object survives if another
    /// entry (or a later `Link` in the batch) still refers to it.
    Unlink { parent: NativeKey, name: String },
    /// Object state changed; entries unchanged.
    Update { snapshot: ObjectSnapshot },
    /// Object (and, for directories, its subtree) is gone.
    Delete { object: NativeKey },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub kind: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyStats {
    pub events: u64,
    pub links: u64,
    pub unlinks: u64,
    pub updates: u64,
    pub deletes: u64,
    pub objects_created: u64,
    pub objects_tombstoned: u64,
    pub entries_created: u64,
    pub entries_tombstoned: u64,
    pub content_changed: u64,
    pub unmatched_parent: u64,
    pub unmatched_object: u64,
    pub outbox_rows: u64,
}

struct Existing {
    id: ObjectId,
    kind: String,
    size: i64,
    allocated: i64,
    modified: Option<i64>,
    generation: i64,
    content_state: String,
}

impl Catalog {
    /// Apply a batch of change events and (optionally) advance the checkpoint
    /// in the same transaction.
    pub fn apply_changes(
        &self,
        source_id: SourceId,
        events: &[ChangeEvent],
        checkpoint: Option<&Checkpoint>,
    ) -> Result<ApplyStats> {
        self.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut applier = Applier::new(&tx, source_id)?;
            for ev in events {
                applier.apply(ev)?;
            }
            applier.finish()?;
            let stats = applier.stats;
            if let Some(cp) = checkpoint {
                set_checkpoint_conn(&tx, source_id, cp)?;
            }
            tx.commit()?;
            Ok(stats)
        })
    }

    pub fn set_checkpoint(&self, source_id: SourceId, cp: &Checkpoint) -> Result<()> {
        self.with_writer(|conn| set_checkpoint_conn(conn, source_id, cp))
    }

    pub fn clear_checkpoint(&self, source_id: SourceId) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE sources SET checkpoint_kind = NULL, checkpoint_json = NULL, checkpoint_at = NULL, updated_at = ?2 WHERE source_id = ?1",
                params![source_id.0, UnixNanos::now().0],
            )?;
            Ok(())
        })
    }

    pub fn checkpoint(&self, source_id: SourceId) -> Result<Option<(Checkpoint, UnixNanos)>> {
        self.with_reader(|conn| {
            let row: Option<(Option<String>, Option<String>, Option<i64>)> = conn
                .query_row(
                    "SELECT checkpoint_kind, checkpoint_json, checkpoint_at FROM sources WHERE source_id = ?1",
                    params![source_id.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match row {
                Some((Some(kind), Some(json), at)) => Ok(Some((
                    Checkpoint {
                        kind,
                        value: serde_json::from_str(&json)?,
                    },
                    UnixNanos(at.unwrap_or(0)),
                ))),
                _ => Ok(None),
            }
        })
    }

    /// Set the native identity of a source's root object (from a stat of the
    /// root path) so change feeds can match events beneath it.
    pub fn set_root_identity(&self, source_id: SourceId, native: NativeIdentity) -> Result<()> {
        self.with_writer(|conn| {
            let src = get_source_conn(conn, source_id)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
            let root = match src.root_object_id {
                Some(r) => r,
                None => return Ok(()),
            };
            conn.execute(
                "UPDATE objects SET native_volume_serial = ?2, native_id_high = ?3, native_id_low = ?4, identity_confidence = ?5 WHERE object_id = ?1",
                params![
                    root.0,
                    native.volume_serial as i64,
                    native.file_id_high as i64,
                    native.file_id_low as i64,
                    native.confidence.as_str()
                ],
            )?;
            Ok(())
        })
    }

    /// Find an object by native identity.
    pub fn object_by_native(
        &self,
        source_id: SourceId,
        key: NativeKey,
    ) -> Result<Option<ObjectId>> {
        self.with_reader(|conn| find_object_id(conn, source_id, key))
    }
}

fn set_checkpoint_conn(conn: &Connection, source_id: SourceId, cp: &Checkpoint) -> Result<()> {
    let now = UnixNanos::now().0;
    conn.execute(
        "UPDATE sources SET checkpoint_kind = ?2, checkpoint_json = ?3, checkpoint_at = ?4, updated_at = ?4 WHERE source_id = ?1",
        params![source_id.0, cp.kind, cp.value.to_string(), now],
    )?;
    Ok(())
}

fn find_object_id(
    conn: &Connection,
    source_id: SourceId,
    key: NativeKey,
) -> Result<Option<ObjectId>> {
    let (s, h, l) = key.parts();
    Ok(conn
        .prepare_cached(
            "SELECT object_id FROM objects WHERE source_id = ?1 AND native_volume_serial = ?2 AND native_id_high = ?3 AND native_id_low = ?4 AND deleted_at IS NULL",
        )?
        .query_row(params![source_id.0, s, h, l], |r| r.get::<_, i64>(0))
        .optional()?
        .map(ObjectId))
}

struct Applier<'a> {
    tx: &'a Transaction<'a>,
    source_id: SourceId,
    root: ObjectId,
    policy: PolicyEngine,
    stats: ApplyStats,
    touched: HashSet<ObjectId>,
    now: i64,
}

impl<'a> Applier<'a> {
    fn new(tx: &'a Transaction<'a>, source_id: SourceId) -> Result<Self> {
        let src = get_source_conn(tx, source_id)?
            .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
        let root = src
            .root_object_id
            .ok_or_else(|| CatalogError::InvalidState("source has never been scanned".into()))?;
        Ok(Self {
            tx,
            source_id,
            root,
            policy: PolicyEngine::new(),
            stats: ApplyStats::default(),
            touched: HashSet::new(),
            now: UnixNanos::now().0,
        })
    }

    fn existing(&self, key: NativeKey) -> Result<Option<Existing>> {
        let (s, h, l) = key.parts();
        Ok(self
            .tx
            .prepare_cached(
                "SELECT object_id, kind, size, allocated, modified, generation, content_state FROM objects
                 WHERE source_id = ?1 AND native_volume_serial = ?2 AND native_id_high = ?3 AND native_id_low = ?4 AND deleted_at IS NULL",
            )?
            .query_row(params![self.source_id.0, s, h, l], |r| {
                Ok(Existing {
                    id: ObjectId(r.get(0)?),
                    kind: r.get(1)?,
                    size: r.get(2)?,
                    allocated: r.get(3)?,
                    modified: r.get(4)?,
                    generation: r.get(5)?,
                    content_state: r.get(6)?,
                })
            })
            .optional()?)
    }

    fn existing_by_id(&self, id: ObjectId) -> Result<Option<Existing>> {
        Ok(self
            .tx
            .prepare_cached(
                "SELECT object_id, kind, size, allocated, modified, generation, content_state FROM objects WHERE object_id = ?1 AND deleted_at IS NULL",
            )?
            .query_row(params![id.0], |r| {
                Ok(Existing {
                    id: ObjectId(r.get(0)?),
                    kind: r.get(1)?,
                    size: r.get(2)?,
                    allocated: r.get(3)?,
                    modified: r.get(4)?,
                    generation: r.get(5)?,
                    content_state: r.get(6)?,
                })
            })
            .optional()?)
    }

    fn find_entry(&self, parent: ObjectId, name: &str) -> Result<Option<(i64, ObjectId)>> {
        Ok(self
            .tx
            .prepare_cached(
                "SELECT entry_id, object_id FROM entries WHERE source_id = ?1 AND parent_id = ?2 AND name = ?3 AND deleted_at IS NULL",
            )?
            .query_row(params![self.source_id.0, parent.0, name], |r| {
                Ok((r.get::<_, i64>(0)?, ObjectId(r.get::<_, i64>(1)?)))
            })
            .optional()?)
    }

    /// Policy context for a directory, rebuilt from its ancestor names.
    fn policy_ctx(&self, dir: ObjectId) -> Result<PolicyCtx> {
        let mut names = Vec::new();
        let mut cur = dir;
        let mut stmt = self.tx.prepare_cached(
            "SELECT parent_id, name FROM entries WHERE object_id = ?1 AND deleted_at IS NULL ORDER BY entry_id LIMIT 1",
        )?;
        for _ in 0..512 {
            let row: Option<(Option<i64>, String)> = stmt
                .query_row(params![cur.0], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            match row {
                Some((Some(p), name)) => {
                    names.push(name);
                    cur = ObjectId(p);
                }
                _ => break,
            }
        }
        let mut ctx = PolicyCtx::root();
        for name in names.iter().rev() {
            ctx = self.policy.directory(name, &ctx);
        }
        Ok(ctx)
    }

    fn content_state_for(
        &self,
        snap: &ObjectSnapshot,
        name: &str,
        ctx: &PolicyCtx,
    ) -> (ContentState, ContentDecision) {
        if snap.kind != ObjectKind::File {
            return (ContentState::NotApplicable, ContentDecision::Unsupported);
        }
        let d = self
            .policy
            .file(name, snap.attributes, snap.reparse_tag, ctx);
        let st = match d {
            ContentDecision::Candidate => ContentState::Pending,
            ContentDecision::Unsupported => ContentState::Unsupported,
            ContentDecision::Excluded { .. } => ContentState::Excluded,
        };
        (st, d)
    }

    fn record_policy(&self, id: ObjectId, decision: ContentDecision) -> Result<()> {
        match decision {
            ContentDecision::Excluded { reason, rule } => {
                self.tx
                    .prepare_cached(
                        "INSERT INTO policy_decisions (object_id, stage, included, reason, rule, policy_version)
                         VALUES (?1, ?2, 0, ?3, ?4, ?5)
                         ON CONFLICT(object_id, stage) DO UPDATE SET included = 0, reason = excluded.reason, rule = excluded.rule,
                            policy_version = excluded.policy_version WHERE user_override = 0",
                    )?
                    .execute(params![id.0, PolicyStage::Content.as_str(), reason.as_str(), rule, self.policy.version as i64])?;
            }
            _ => {
                self.tx
                    .prepare_cached("DELETE FROM policy_decisions WHERE object_id = ?1 AND stage = ?2 AND user_override = 0")?
                    .execute(params![id.0, PolicyStage::Content.as_str()])?;
            }
        }
        Ok(())
    }

    fn outbox(&mut self, id: ObjectId, op: &str, generation: i64) -> Result<()> {
        outbox_append_conn(self.tx, self.source_id, id, op, generation)?;
        self.stats.outbox_rows += 1;
        Ok(())
    }

    /// Parent directories of all live entries of an object.
    fn parents_of(&self, id: ObjectId) -> Result<Vec<ObjectId>> {
        Ok(self
            .tx
            .prepare_cached("SELECT parent_id FROM entries WHERE object_id = ?1 AND deleted_at IS NULL AND parent_id IS NOT NULL")?
            .query_map(params![id.0], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(ObjectId)
            .collect())
    }

    /// Upsert object metadata from a snapshot. Returns `(id, created, delta
    /// to apply on every parent chain for the state change)`.
    fn upsert_object(
        &mut self,
        snap: &ObjectSnapshot,
        name_for_policy: &str,
        ctx: &PolicyCtx,
    ) -> Result<(ObjectId, bool, Option<AggDelta>)> {
        let key = NativeKey::from(snap.native);
        let kind = snap.kind.as_str();
        let (new_state, decision) = self.content_state_for(snap, name_for_policy, ctx);
        let ext = if snap.kind == ObjectKind::File {
            extension_of(name_for_policy)
        } else {
            String::new()
        };
        if let Some(ex) = self.existing(key)? {
            if ex.kind != kind {
                // Identity reused for a different kind: retire the old row.
                self.tombstone_object(ex.id)?;
            } else {
                let content_changed = snap.kind == ObjectKind::File
                    && (ex.size != snap.size as i64 || ex.modified != snap.modified.map(|t| t.0));
                let (generation, state) = if content_changed {
                    self.stats.content_changed += 1;
                    (ex.generation + 1, new_state)
                } else {
                    (
                        ex.generation,
                        ContentState::parse(&ex.content_state).unwrap_or(ContentState::Pending),
                    )
                };
                self.tx
                    .prepare_cached(
                        "UPDATE objects SET size = ?2, allocated = ?3, attributes = ?4, created = ?5, modified = ?6, changed = ?7,
                            accessed = ?8, reparse_tag = ?9, generation = ?10, content_state = ?11, link_count = ?12 WHERE object_id = ?1",
                    )?
                    .execute(params![
                        ex.id.0,
                        snap.size as i64,
                        snap.allocated as i64,
                        snap.attributes.0 as i64,
                        snap.created.map(|t| t.0),
                        snap.modified.map(|t| t.0),
                        snap.changed.map(|t| t.0),
                        snap.accessed.map(|t| t.0),
                        snap.reparse_tag as i64,
                        generation,
                        state.as_str(),
                        snap.link_count.max(1) as i64,
                    ])?;
                if content_changed {
                    let (entries, objects) =
                        crate::archive::retire_virtual_tree(self.tx, ex.id, self.now)?;
                    self.stats.entries_tombstoned += entries;
                    self.stats.objects_tombstoned += objects;
                    self.record_policy(ex.id, decision)?;
                    self.outbox(ex.id, "content", generation)?;
                    self.outbox(ex.id, "subtree", generation)?;
                    if state == ContentState::Pending {
                        crate::content::enqueue_content_for(
                            self.tx,
                            self.source_id,
                            ex.id,
                            generation as u32,
                            snap.size,
                        )?;
                    }
                } else {
                    self.outbox(ex.id, "upsert", generation)?;
                }
                self.touched.insert(ex.id);
                // Delta for size/state changes, applied per parent by caller.
                let delta = if snap.kind == ObjectKind::File
                    && (ex.size != snap.size as i64
                        || ex.allocated != snap.allocated as i64
                        || content_changed)
                {
                    let old_state =
                        ContentState::parse(&ex.content_state).unwrap_or(ContentState::Pending);
                    let mut d = AggDelta::for_file(
                        &ext,
                        ex.size as u64,
                        ex.allocated as u64,
                        ex.modified.map(UnixNanos),
                        old_state,
                        -1,
                    );
                    let add = AggDelta::for_file(
                        &ext,
                        snap.size,
                        snap.allocated,
                        snap.modified,
                        state,
                        1,
                    );
                    d.file_count += add.file_count;
                    d.logical += add.logical;
                    d.allocated += add.allocated;
                    d.pending += add.pending;
                    d.indexed += add.indexed;
                    d.failed += add.failed;
                    d.excluded += add.excluded;
                    d.ext.extend(add.ext);
                    d.added = add.added;
                    Some(d)
                } else {
                    None
                };
                return Ok((ex.id, false, delta));
            }
        }
        let n = snap.native;
        self.tx
            .prepare_cached(
                "INSERT INTO objects (source_id, kind, native_volume_serial, native_id_high, native_id_low, identity_confidence,
                    generation, size, allocated, attributes, created, modified, changed, accessed, reparse_tag, link_count,
                    content_state, first_seen_generation, last_seen_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                    (SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?1),
                    (SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?1))",
            )?
            .execute(params![
                self.source_id.0,
                kind,
                n.volume_serial as i64,
                n.file_id_high as i64,
                n.file_id_low as i64,
                if n.confidence == IdentityConfidence::PathDerived { "weak" } else { n.confidence.as_str() },
                snap.size as i64,
                snap.allocated as i64,
                snap.attributes.0 as i64,
                snap.created.map(|t| t.0),
                snap.modified.map(|t| t.0),
                snap.changed.map(|t| t.0),
                snap.accessed.map(|t| t.0),
                snap.reparse_tag as i64,
                snap.link_count.max(1) as i64,
                new_state.as_str(),
            ])?;
        let id = ObjectId(self.tx.last_insert_rowid());
        self.record_policy(id, decision)?;
        self.stats.objects_created += 1;
        self.touched.insert(id);
        if snap.kind == ObjectKind::Directory {
            self.tx.execute(
                "INSERT OR IGNORE INTO directory_aggregates (object_id, source_id, generation, complete)
                 VALUES (?1, ?2, (SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?2), 1)",
                params![id.0, self.source_id.0],
            )?;
        }
        let content_candidate = snap.kind == ObjectKind::File && new_state == ContentState::Pending;
        self.outbox(
            id,
            if content_candidate {
                "content"
            } else {
                "upsert"
            },
            1,
        )?;
        if content_candidate {
            crate::content::enqueue_content_for(self.tx, self.source_id, id, 1, snap.size)?;
        }
        Ok((id, true, None))
    }

    fn apply(&mut self, ev: &ChangeEvent) -> Result<()> {
        self.stats.events += 1;
        match ev {
            ChangeEvent::Link {
                parent,
                name,
                snapshot,
            } => self.link(*parent, name, snapshot),
            ChangeEvent::Unlink { parent, name } => self.unlink(*parent, name),
            ChangeEvent::Update { snapshot } => self.update(snapshot),
            ChangeEvent::Delete { object } => self.delete(*object),
        }
    }

    fn link(&mut self, parent_key: NativeKey, name: &str, snap: &ObjectSnapshot) -> Result<()> {
        self.stats.links += 1;
        let parent = match self.existing(parent_key)? {
            Some(p) if p.kind == ObjectKind::Directory.as_str() => p.id,
            _ => {
                self.stats.unmatched_parent += 1;
                return Ok(());
            }
        };
        let ctx = self.policy_ctx(parent)?;
        let (obj, created, state_delta) = self.upsert_object(snap, name, &ctx)?;
        let ext = if snap.kind == ObjectKind::File {
            extension_of(name)
        } else {
            String::new()
        };
        match self.find_entry(parent, name)? {
            Some((_, existing_obj)) if existing_obj == obj => {
                // Entry unchanged; propagate any state delta to all parents.
                if let Some(d) = state_delta {
                    for p in self.parents_of(obj)? {
                        apply_delta(self.tx, p, &d)?;
                    }
                }
                return Ok(());
            }
            Some((entry_id, other)) => {
                // Name now refers to a different object: replace.
                self.tombstone_entry(entry_id, other, parent)?;
            }
            None => {}
        }
        if let Some(d) = state_delta {
            for p in self.parents_of(obj)? {
                apply_delta(self.tx, p, &d)?;
            }
        }
        self.tx
            .prepare_cached(
                "INSERT INTO entries (source_id, parent_id, object_id, name, name_folded, extension, first_seen_generation, last_seen_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                    (SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?1),
                    (SELECT COALESCE(published_generation, 0) FROM sources WHERE source_id = ?1))",
            )?
            .execute(params![self.source_id.0, parent.0, obj.0, name, crate::policy::fold(name), ext])?;
        self.stats.entries_created += 1;
        // Aggregate contribution of this new entry on the parent chain.
        let delta = if snap.kind == ObjectKind::Directory {
            if created {
                AggDelta {
                    dir_count: 1,
                    ..Default::default()
                }
            } else {
                // Existing directory linked here (move in): add its subtree.
                AggDelta::from_subtree(self.tx, obj)?
            }
        } else {
            let st = if created {
                self.existing_by_id(obj)?
                    .map(|e| ContentState::parse(&e.content_state).unwrap_or(ContentState::Pending))
                    .unwrap_or(ContentState::Pending)
            } else {
                self.existing_by_id(obj)?
                    .map(|e| ContentState::parse(&e.content_state).unwrap_or(ContentState::Pending))
                    .unwrap_or(ContentState::Pending)
            };
            AggDelta::for_file(&ext, snap.size, snap.allocated, snap.modified, st, 1)
        };
        apply_delta(self.tx, parent, &delta)?;
        if !created && snap.kind == ObjectKind::Directory {
            // Moved/renamed directory: every descendant's path changed.
            self.outbox(obj, "subtree", 0)?;
        }
        if !created {
            // Hard link count may have changed.
            self.tx.execute(
                "UPDATE objects SET link_count = (SELECT COUNT(*) FROM entries e WHERE e.object_id = ?1 AND e.deleted_at IS NULL) WHERE object_id = ?1 AND kind = 'file'",
                params![obj.0],
            )?;
        }
        Ok(())
    }

    fn unlink(&mut self, parent_key: NativeKey, name: &str) -> Result<()> {
        self.stats.unlinks += 1;
        let parent = match self.existing(parent_key)? {
            Some(p) => p.id,
            None => {
                self.stats.unmatched_parent += 1;
                return Ok(());
            }
        };
        match self.find_entry(parent, name)? {
            Some((entry_id, obj)) => self.tombstone_entry(entry_id, obj, parent),
            None => {
                self.stats.unmatched_object += 1;
                Ok(())
            }
        }
    }

    /// Tombstone one entry and subtract its contribution from the parent
    /// chain. The object itself is left for orphan cleanup at batch end.
    fn tombstone_entry(&mut self, entry_id: i64, obj: ObjectId, parent: ObjectId) -> Result<()> {
        let ex = match self.existing_by_id(obj)? {
            Some(e) => e,
            None => return Ok(()),
        };
        let name: String = self.tx.query_row(
            "SELECT name FROM entries WHERE entry_id = ?1",
            params![entry_id],
            |r| r.get(0),
        )?;
        self.tx.execute(
            "UPDATE entries SET deleted_at = ?2 WHERE entry_id = ?1",
            params![entry_id, self.now],
        )?;
        self.stats.entries_tombstoned += 1;
        let delta = if ex.kind == ObjectKind::Directory.as_str() {
            AggDelta::from_subtree(self.tx, obj)?.negate()
        } else {
            let st = ContentState::parse(&ex.content_state).unwrap_or(ContentState::Pending);
            AggDelta::for_file(
                &extension_of(&name),
                ex.size as u64,
                ex.allocated as u64,
                ex.modified.map(UnixNanos),
                st,
                -1,
            )
        };
        apply_delta(self.tx, parent, &delta)?;
        self.touched.insert(obj);
        self.tx.execute(
            "UPDATE objects SET link_count = MAX(1, (SELECT COUNT(*) FROM entries e WHERE e.object_id = ?1 AND e.deleted_at IS NULL)) WHERE object_id = ?1 AND kind = 'file'",
            params![obj.0],
        )?;
        Ok(())
    }

    fn update(&mut self, snap: &ObjectSnapshot) -> Result<()> {
        self.stats.updates += 1;
        let key = NativeKey::from(snap.native);
        let ex = match self.existing(key)? {
            Some(e) => e,
            None => {
                self.stats.unmatched_object += 1;
                return Ok(());
            }
        };
        // Use the first live entry's name/parent for policy context.
        let (parent, name): (Option<i64>, String) = self
            .tx
            .query_row(
                "SELECT parent_id, name FROM entries WHERE object_id = ?1 AND deleted_at IS NULL ORDER BY entry_id LIMIT 1",
                params![ex.id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, String::new()));
        let ctx = match parent {
            Some(p) => self.policy_ctx(ObjectId(p))?,
            None => PolicyCtx::root(),
        };
        let (obj, _, delta) = self.upsert_object(snap, &name, &ctx)?;
        if let Some(d) = delta {
            for p in self.parents_of(obj)? {
                apply_delta(self.tx, p, &d)?;
            }
        }
        Ok(())
    }

    fn delete(&mut self, key: NativeKey) -> Result<()> {
        self.stats.deletes += 1;
        let id = match find_object_id(self.tx, self.source_id, key)? {
            Some(id) => id,
            None => {
                self.stats.unmatched_object += 1;
                return Ok(());
            }
        };
        if id == self.root {
            return Err(CatalogError::InvalidState(
                "refusing to delete the source root".into(),
            ));
        }
        // Tombstone every live entry (with deltas), then the object/subtree.
        let entries: Vec<(i64, i64)> = self
            .tx
            .prepare_cached("SELECT entry_id, parent_id FROM entries WHERE object_id = ?1 AND deleted_at IS NULL AND parent_id IS NOT NULL")?
            .query_map(params![id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (entry_id, parent) in entries {
            self.tombstone_entry(entry_id, id, ObjectId(parent))?;
        }
        self.tombstone_object(id)?;
        Ok(())
    }

    /// Tombstone an object and, for directories, its whole subtree.
    fn tombstone_object(&mut self, id: ObjectId) -> Result<()> {
        let kind: Option<String> = self
            .tx
            .query_row(
                "SELECT kind FROM objects WHERE object_id = ?1 AND deleted_at IS NULL",
                params![id.0],
                |r| r.get(0),
            )
            .optional()?;
        let kind = match kind {
            Some(k) => k,
            None => return Ok(()),
        };
        let mut victims = vec![id];
        // Files normally have no descendants, but archive containers own a
        // virtual subtree and must cascade just like physical directories.
        if kind == ObjectKind::Directory.as_str()
            || kind == ObjectKind::File.as_str()
            || kind == ObjectKind::VirtualDirectory.as_str()
        {
            let desc: Vec<i64> = self
                .tx
                .prepare_cached(
                    "WITH RECURSIVE sub(object_id) AS (
                        SELECT object_id FROM entries WHERE parent_id = ?1 AND deleted_at IS NULL
                        UNION
                        SELECT e.object_id FROM entries e JOIN sub s ON e.parent_id = s.object_id WHERE e.deleted_at IS NULL
                     ) SELECT object_id FROM sub",
                )?
                .query_map(params![id.0], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            victims.extend(desc.into_iter().map(ObjectId));
        }
        for v in victims {
            let gen: i64 = self
                .tx
                .query_row(
                    "SELECT generation FROM objects WHERE object_id = ?1",
                    params![v.0],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let n = self.tx.execute(
                "UPDATE objects SET deleted_at = ?2 WHERE object_id = ?1 AND deleted_at IS NULL",
                params![v.0, self.now],
            )?;
            if n == 0 {
                continue;
            }
            self.stats.objects_tombstoned += 1;
            let e = self.tx.execute(
                "UPDATE entries SET deleted_at = ?2 WHERE object_id = ?1 AND deleted_at IS NULL",
                params![v.0, self.now],
            )?;
            self.stats.entries_tombstoned += e as u64;
            self.tx.execute(
                "DELETE FROM directory_aggregates WHERE object_id = ?1",
                params![v.0],
            )?;
            self.tx.execute(
                "DELETE FROM directory_extension_counts WHERE object_id = ?1",
                params![v.0],
            )?;
            self.tx.execute(
                "UPDATE jobs SET state = 'superseded', finished_at = ?2 WHERE object_id = ?1 AND state = 'queued'",
                params![v.0, self.now],
            )?;
            self.outbox(v, "delete", gen)?;
        }
        Ok(())
    }

    /// Orphan cleanup: touched objects with no live entry are gone.
    fn finish(&mut self) -> Result<()> {
        let touched: Vec<ObjectId> = self.touched.iter().copied().collect();
        for id in touched {
            if id == self.root {
                continue;
            }
            let live: i64 = self.tx.query_row(
                "SELECT COUNT(*) FROM entries WHERE object_id = ?1 AND deleted_at IS NULL",
                params![id.0],
                |r| r.get(0),
            )?;
            if live == 0 {
                self.tombstone_object(id)?;
            }
        }
        Ok(())
    }
}
