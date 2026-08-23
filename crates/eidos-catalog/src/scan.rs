//! Scan sessions: ingest walker events into a scan generation and publish.
//!
//! Publication model (SPEC 7.3, ARCHITECTURE 7):
//!
//! - Rows become visible progressively as batches commit; the source state
//!   (`enumerating`/`reconciling`) tells consumers the generation is open.
//! - `finish` tombstones entries that were not re-observed *under
//!   directories that were successfully listed*, cascades deletions,
//!   rebuilds aggregates, and flips `published_generation` — all in one
//!   transaction. [`PublishOptions`] lets the caller commit the change-feed
//!   checkpoint in that same transaction and publish in a `degraded` state,
//!   so a native scan's "replay overlapping changes → publish → checkpoint"
//!   sequence has no window in which the generation looks complete while the
//!   replayed state is not durable.
//! - `abort` (or crash recovery) never publishes; the previous generation
//!   remains the published truth and the source becomes `degraded`.

use crate::aggregates;
use crate::changes::{clear_checkpoint_conn, set_checkpoint_conn, Checkpoint};
use crate::model::SourceRecord;
use crate::policy::{ContentDecision, PolicyCtx, PolicyEngine};
use crate::{Catalog, CatalogError, RecoveryReport, Result};
use eidos_domain::{
    extension_of, ContentState, IdentityConfidence, ObjectId, ObjectKind, PolicyStage, SourceId,
    SourceState, UnixNanos,
};
use eidos_scanner::{DirEvent, RawEntry};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanKind {
    Full,
    Reconcile,
}

impl ScanKind {
    fn as_str(self) -> &'static str {
        match self {
            ScanKind::Full => "full",
            ScanKind::Reconcile => "reconcile",
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanStats {
    pub dirs_listed: u64,
    pub entries_seen: u64,
    pub errors: u64,
    pub objects_created: u64,
    pub objects_updated: u64,
    pub content_changed: u64,
    pub entries_created: u64,
    pub entries_replaced: u64,
    pub hard_links: u64,
    pub commits: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanSummary {
    pub source_id: SourceId,
    pub generation: i64,
    pub stats: ScanStats,
    pub tombstoned_entries: u64,
    pub tombstoned_objects: u64,
    pub aggregates: aggregates::AggStats,
    pub elapsed_ms: f64,
    pub published: bool,
    pub final_state: SourceState,
}

/// How a generation is published (see [`ScanSession::finish_with`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct PublishOptions<'a> {
    /// Change-feed checkpoint to store in the publish transaction: the feed
    /// position up to which overlapping changes have been applied to this
    /// generation.
    pub checkpoint: Option<&'a Checkpoint>,
    /// Drop any stored checkpoint in the publish transaction (the feed can
    /// no longer continue from it; a watcher will reconcile again).
    pub clear_checkpoint: bool,
    /// Publish the generation but leave the source `degraded` with this
    /// reason (e.g. the feed wrapped during enumeration, so some overlapping
    /// changes may be missing until the next reconciliation).
    pub degraded: Option<&'a str>,
}

pub struct ScanSession {
    conn: Connection,
    source: SourceRecord,
    generation: i64,
    root: ObjectId,
    tokens: HashMap<u64, (ObjectId, PolicyCtx)>,
    policy: PolicyEngine,
    in_tx: bool,
    pending_rows: usize,
    last_commit: Instant,
    started: Instant,
    stats: ScanStats,
    unlisted: HashSet<ObjectId>,
    root_error: Option<eidos_scanner::ScanErrorKind>,
    batch_rows: usize,
    batch_interval: std::time::Duration,
}

impl Catalog {
    /// Open a scan generation for `source_id`.
    pub fn begin_scan(&self, source_id: SourceId, kind: ScanKind) -> Result<ScanSession> {
        let now = UnixNanos::now().0;
        let (source, generation, root) = self.with_writer(|conn| {
            let tx = conn.transaction()?;
            let mut source = crate::read::get_source_conn(&tx, source_id)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
            let open: i64 = tx.query_row(
                "SELECT COUNT(*) FROM scan_generations WHERE source_id = ?1 AND state = 'open'",
                params![source_id.0],
                |r| r.get(0),
            )?;
            if open > 0 {
                return Err(CatalogError::InvalidState(format!(
                    "source {source_id} already has an open scan generation"
                )));
            }
            let generation: i64 = tx.query_row(
                "SELECT COALESCE(MAX(generation), 0) + 1 FROM scan_generations WHERE source_id = ?1",
                params![source_id.0],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO scan_generations (source_id, generation, kind, state, started_at) VALUES (?1, ?2, ?3, 'open', ?4)",
                params![source_id.0, generation, kind.as_str(), now],
            )?;
            let root = match source.root_object_id {
                Some(r) => r,
                None => {
                    tx.execute(
                        "INSERT INTO objects (source_id, kind, identity_confidence, content_state, first_seen_generation, last_seen_generation)
                         VALUES (?1, 'directory', 'path_derived', 'not_applicable', ?2, ?2)",
                        params![source_id.0, generation],
                    )?;
                    let root = ObjectId(tx.last_insert_rowid());
                    tx.execute(
                        "INSERT INTO entries (source_id, parent_id, object_id, name, name_folded, extension, first_seen_generation, last_seen_generation)
                         VALUES (?1, NULL, ?2, '', '', '', ?3, ?3)",
                        params![source_id.0, root.0, generation],
                    )?;
                    tx.execute(
                        "UPDATE sources SET root_object_id = ?2 WHERE source_id = ?1",
                        params![source_id.0, root.0],
                    )?;
                    source.root_object_id = Some(root);
                    root
                }
            };
            let state = if source.published_generation.is_some() {
                SourceState::Reconciling
            } else {
                SourceState::Enumerating
            };
            tx.execute(
                "UPDATE sources SET state = ?2, state_reason = NULL, last_scan_started_at = ?3, updated_at = ?3 WHERE source_id = ?1",
                params![source_id.0, state.as_str(), now],
            )?;
            tx.commit()?;
            Ok((source, generation, root))
        })?;
        let conn = self.open_writer()?;
        Ok(ScanSession {
            conn,
            source,
            generation,
            root,
            tokens: HashMap::new(),
            policy: PolicyEngine::new(),
            in_tx: false,
            pending_rows: 0,
            last_commit: Instant::now(),
            started: Instant::now(),
            stats: ScanStats::default(),
            unlisted: HashSet::new(),
            root_error: None,
            batch_rows: 4000,
            batch_interval: std::time::Duration::from_millis(500),
        })
    }
}

struct ExistingObject {
    id: ObjectId,
    kind: String,
    size: i64,
    modified: Option<i64>,
    changed: Option<i64>,
    generation: i64,
    content_state: String,
}

impl ScanSession {
    pub fn generation(&self) -> i64 {
        self.generation
    }
    pub fn source_id(&self) -> SourceId {
        self.source.id
    }
    pub fn root_object(&self) -> ObjectId {
        self.root
    }
    pub fn stats(&self) -> &ScanStats {
        &self.stats
    }

    /// Tune batching (rows per commit, max time between commits).
    pub fn set_batching(&mut self, rows: usize, interval: std::time::Duration) {
        self.batch_rows = rows.max(1);
        self.batch_interval = interval;
    }

    fn ensure_tx(&mut self) -> Result<()> {
        if !self.in_tx {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            self.in_tx = true;
        }
        Ok(())
    }

    /// Commit the open batch (if any), making rows visible to readers.
    pub fn commit(&mut self) -> Result<()> {
        if self.in_tx {
            self.conn.execute_batch("COMMIT")?;
            self.in_tx = false;
            self.stats.commits += 1;
        }
        self.pending_rows = 0;
        self.last_commit = Instant::now();
        Ok(())
    }

    fn maybe_commit(&mut self) -> Result<()> {
        if self.pending_rows >= self.batch_rows || self.last_commit.elapsed() >= self.batch_interval
        {
            self.commit()?;
        }
        Ok(())
    }

    /// Ingest one walker event.
    pub fn ingest(&mut self, ev: DirEvent) -> Result<()> {
        self.ensure_tx()?;
        let (dir, ctx) = if ev.token.0 == 0 {
            (self.root, PolicyCtx::root())
        } else {
            match self.tokens.remove(&ev.token.0) {
                Some(x) => x,
                None => {
                    return Err(CatalogError::InvalidState(format!(
                        "walker token {} delivered before its parent",
                        ev.token.0
                    )))
                }
            }
        };
        match ev.result {
            Err(err) => {
                self.stats.errors += 1;
                self.unlisted.insert(dir);
                if dir == self.root {
                    self.root_error = Some(err.kind);
                }
                let path = eidos_scanner_display(&ev.path);
                self.conn.execute(
                    "INSERT INTO errors (source_id, object_id, generation, stage, kind, code, path, message, occurred_at)
                     VALUES (?1, ?2, ?3, 'enumerate', ?4, ?5, ?6, ?7, ?8)",
                    params![
                        self.source.id.0,
                        dir.0,
                        self.generation,
                        format!("{:?}", err.kind).to_lowercase(),
                        err.code,
                        path,
                        err.message,
                        UnixNanos::now().0
                    ],
                )?;
                // The directory itself was observed by its parent; keep it alive.
                self.conn.execute(
                    "UPDATE objects SET last_seen_generation = ?2 WHERE object_id = ?1",
                    params![dir.0, self.generation],
                )?;
                self.pending_rows += 1;
            }
            Ok(entries) => {
                self.stats.dirs_listed += 1;
                self.stats.entries_seen += entries.len() as u64;
                self.conn.execute(
                    "UPDATE objects SET listed_generation = ?2, last_seen_generation = ?2 WHERE object_id = ?1",
                    params![dir.0, self.generation],
                )?;
                let mut child_tokens = ev.child_tokens.iter().peekable();
                for (i, e) in entries.iter().enumerate() {
                    let obj = self.upsert_object(dir, e, &ctx)?;
                    self.upsert_entry(dir, e, obj)?;
                    if let Some((idx, tok)) = child_tokens.peek() {
                        if *idx == i {
                            let child_ctx = self.policy.directory(&e.name, &ctx);
                            self.tokens.insert(tok.0, (obj, child_ctx));
                            child_tokens.next();
                        }
                    }
                }
                self.pending_rows += entries.len() + 1;
            }
        }
        self.maybe_commit()
    }

    fn upsert_object(
        &mut self,
        parent: ObjectId,
        e: &RawEntry,
        ctx: &PolicyCtx,
    ) -> Result<ObjectId> {
        let gen = self.generation;
        let source_id = self.source.id.0;
        let kind = e.kind.as_str();
        let native = e
            .native_id
            .filter(|n| n.confidence != IdentityConfidence::PathDerived);
        let existing: Option<ExistingObject> = match native {
            Some(n) => self
                .conn
                .prepare_cached(
                    "SELECT object_id, kind, size, modified, changed, generation, content_state FROM objects
                     WHERE source_id = ?1 AND native_volume_serial = ?2 AND native_id_high = ?3 AND native_id_low = ?4 AND deleted_at IS NULL",
                )?
                .query_row(
                    params![
                        source_id,
                        n.volume_serial as i64,
                        n.file_id_high as i64,
                        n.file_id_low as i64
                    ],
                    map_existing,
                )
                .optional()?,
            None => self
                .conn
                .prepare_cached(
                    "SELECT o.object_id, o.kind, o.size, o.modified, o.changed, o.generation, o.content_state
                     FROM entries en JOIN objects o ON o.object_id = en.object_id
                     WHERE en.source_id = ?1 AND en.parent_id = ?2 AND en.name = ?3 AND en.deleted_at IS NULL AND o.deleted_at IS NULL",
                )?
                .query_row(params![source_id, parent.0, e.name], map_existing)
                .optional()?,
        };

        let decision = match e.kind {
            ObjectKind::File => self.policy.file(&e.name, e.attributes, e.reparse_tag, ctx),
            _ => ContentDecision::Unsupported,
        };
        let (confidence, serial, high, low) = match native {
            Some(n) => (
                n.confidence.as_str(),
                Some(n.volume_serial as i64),
                Some(n.file_id_high as i64),
                Some(n.file_id_low as i64),
            ),
            None => (IdentityConfidence::PathDerived.as_str(), None, None, None),
        };
        let size = e.size as i64;
        let alloc = e.allocated.unwrap_or(e.size) as i64;

        if let Some(ex) = existing {
            if ex.kind != kind {
                // Same identity now a different kind (path-derived replacement):
                // retire the old object and create a fresh one.
                self.conn.execute(
                    "UPDATE objects SET deleted_at = ?2 WHERE object_id = ?1",
                    params![ex.id.0, UnixNanos::now().0],
                )?;
            } else {
                // ChangeTime moves on renames and attribute edits, so only size
                // and LastWriteTime indicate new content (USN reasons refine
                // this in Milestone 2; BLAKE3 verifies it in Milestone 4).
                let content_changed = e.kind == ObjectKind::File
                    && (ex.size != size || ex.modified != e.modified.map(|t| t.0));
                let _ = ex.changed;
                let (generation, content_state) = if content_changed {
                    self.stats.content_changed += 1;
                    (
                        ex.generation + 1,
                        initial_content_state(decision, e.kind).as_str(),
                    )
                } else {
                    (ex.generation, ex.content_state.as_str())
                };
                let content_state = content_state.to_string();
                if content_changed {
                    crate::archive::retire_virtual_tree(&self.conn, ex.id, UnixNanos::now().0)?;
                }
                self.conn
                    .prepare_cached(
                        "UPDATE objects SET size = ?2, allocated = ?3, attributes = ?4, created = ?5, modified = ?6, changed = ?7,
                            accessed = ?8, reparse_tag = ?9, generation = ?10, content_state = ?11, last_seen_generation = ?12
                         WHERE object_id = ?1",
                    )?
                    .execute(params![
                        ex.id.0,
                        size,
                        alloc,
                        e.attributes.0 as i64,
                        e.created.map(|t| t.0),
                        e.modified.map(|t| t.0),
                        e.changed.map(|t| t.0),
                        e.accessed.map(|t| t.0),
                        e.reparse_tag as i64,
                        generation,
                        content_state,
                        gen,
                    ])?;
                if content_changed {
                    self.record_policy(ex.id, decision)?;
                }
                self.stats.objects_updated += 1;
                return Ok(ex.id);
            }
        }

        let content_state = initial_content_state(decision, e.kind);
        self.conn
            .prepare_cached(
                "INSERT INTO objects (source_id, kind, native_volume_serial, native_id_high, native_id_low, identity_confidence,
                    generation, size, allocated, attributes, created, modified, changed, accessed, reparse_tag, link_count,
                    content_state, first_seen_generation, last_seen_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1, ?15, ?16, ?16)",
            )?
            .execute(params![
                source_id,
                kind,
                serial,
                high,
                low,
                confidence,
                size,
                alloc,
                e.attributes.0 as i64,
                e.created.map(|t| t.0),
                e.modified.map(|t| t.0),
                e.changed.map(|t| t.0),
                e.accessed.map(|t| t.0),
                e.reparse_tag as i64,
                content_state.as_str(),
                gen,
            ])?;
        let id = ObjectId(self.conn.last_insert_rowid());
        self.record_policy(id, decision)?;
        self.stats.objects_created += 1;
        Ok(id)
    }

    fn record_policy(&mut self, id: ObjectId, decision: ContentDecision) -> Result<()> {
        match decision {
            ContentDecision::Excluded { reason, rule } => {
                self.conn
                    .prepare_cached(
                        "INSERT INTO policy_decisions (object_id, stage, included, reason, rule, policy_version)
                         VALUES (?1, ?2, 0, ?3, ?4, ?5)
                         ON CONFLICT(object_id, stage) DO UPDATE SET included = 0, reason = excluded.reason, rule = excluded.rule,
                            policy_version = excluded.policy_version WHERE user_override = 0",
                    )?
                    .execute(params![
                        id.0,
                        PolicyStage::Content.as_str(),
                        reason.as_str(),
                        rule,
                        self.policy.version as i64
                    ])?;
            }
            _ => {
                self.conn
                    .prepare_cached(
                        "DELETE FROM policy_decisions WHERE object_id = ?1 AND stage = ?2 AND user_override = 0",
                    )?
                    .execute(params![id.0, PolicyStage::Content.as_str()])?;
            }
        }
        Ok(())
    }

    fn upsert_entry(&mut self, parent: ObjectId, e: &RawEntry, obj: ObjectId) -> Result<()> {
        let gen = self.generation;
        let source_id = self.source.id.0;
        let ext = if e.kind == ObjectKind::File {
            extension_of(&e.name)
        } else {
            String::new()
        };
        let existing: Option<(i64, i64)> = self
            .conn
            .prepare_cached(
                "SELECT entry_id, object_id FROM entries WHERE source_id = ?1 AND parent_id = ?2 AND name = ?3 AND deleted_at IS NULL",
            )?
            .query_row(params![source_id, parent.0, e.name], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match existing {
            Some((entry_id, old_obj)) if old_obj == obj.0 => {
                self.conn
                    .prepare_cached(
                        "UPDATE entries SET last_seen_generation = ?2, extension = ?3 WHERE entry_id = ?1",
                    )?
                    .execute(params![entry_id, gen, ext])?;
                return Ok(());
            }
            Some((entry_id, _)) => {
                self.conn
                    .prepare_cached("UPDATE entries SET deleted_at = ?2 WHERE entry_id = ?1")?
                    .execute(params![entry_id, UnixNanos::now().0])?;
                self.stats.entries_replaced += 1;
            }
            None => {}
        }
        let folded = crate::policy::fold(&e.name);
        self.conn
            .prepare_cached(
                "INSERT INTO entries (source_id, parent_id, object_id, name, name_folded, extension, first_seen_generation, last_seen_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            )?
            .execute(params![source_id, parent.0, obj.0, e.name, folded, ext, gen])?;
        self.stats.entries_created += 1;
        Ok(())
    }

    /// Validate, tombstone, aggregate, and publish this generation.
    ///
    /// A generation whose *root* could not be listed is never published: it
    /// would otherwise look like a complete, empty source.
    pub fn finish(self) -> Result<ScanSummary> {
        self.finish_with(&PublishOptions::default())
    }

    /// [`finish`](Self::finish) with an explicit publication policy: the
    /// checkpoint (if any) and the source state flip commit atomically with
    /// the generation.
    pub fn finish_with(mut self, opts: &PublishOptions<'_>) -> Result<ScanSummary> {
        self.commit()?;
        if self.unlisted.contains(&self.root) {
            // An unreachable root (not found / transient network failure)
            // means the source is offline; access denied means degraded.
            // Either way the previous generation stays published.
            let offline = matches!(
                self.root_error,
                Some(eidos_scanner::ScanErrorKind::NotFound)
                    | Some(eidos_scanner::ScanErrorKind::Transient)
            );
            let reason = if offline {
                "root directory unreachable; source is offline and last-known results are preserved"
            } else {
                "root directory could not be listed; generation not published"
            };
            let sid = self.source.id;
            let gen = self.generation;
            if self.in_tx {
                let _ = self.conn.execute_batch("ROLLBACK");
                self.in_tx = false;
            }
            let override_state = if offline && self.source.published_generation.is_some() {
                Some(SourceState::Offline)
            } else {
                None
            };
            abort_generation(&self.conn, sid, gen, reason, override_state)?;
            return Err(CatalogError::InvalidState(format!(
                "source {sid} generation {gen}: {reason}"
            )));
        }
        let now = UnixNanos::now().0;
        let gen = self.generation;
        let sid = self.source.id.0;
        let root = self.root;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        self.in_tx = true;

        // 1. Entries not re-observed under directories we listed this generation.
        let mut tomb_entries = self.conn.execute(
            "UPDATE entries SET deleted_at = ?3 WHERE source_id = ?1 AND deleted_at IS NULL AND last_seen_generation < ?2
               AND parent_id IN (SELECT object_id FROM objects WHERE source_id = ?1 AND listed_generation = ?2)",
            params![sid, gen, now],
        )? as u64;
        // 2. Cascade: objects with no live entry die; entries under dead directories die.
        let mut tomb_objects = 0u64;
        loop {
            let n1 = self.conn.execute(
                "UPDATE objects SET deleted_at = ?3 WHERE source_id = ?1 AND deleted_at IS NULL AND object_id != ?2
                   AND NOT EXISTS (SELECT 1 FROM entries e WHERE e.object_id = objects.object_id AND e.deleted_at IS NULL)",
                params![sid, root.0, now],
            )? as u64;
            let n2 = self.conn.execute(
                "UPDATE entries SET deleted_at = ?2 WHERE source_id = ?1 AND deleted_at IS NULL
                   AND parent_id IN (SELECT object_id FROM objects WHERE source_id = ?1 AND deleted_at IS NOT NULL)",
                params![sid, now],
            )? as u64;
            tomb_objects += n1;
            tomb_entries += n2;
            if n1 + n2 == 0 {
                break;
            }
        }
        // 3. Hard-link counts.
        self.conn.execute(
            "UPDATE objects SET link_count = (SELECT COUNT(*) FROM entries e WHERE e.object_id = objects.object_id AND e.deleted_at IS NULL)
             WHERE source_id = ?1 AND deleted_at IS NULL AND kind = 'file' AND last_seen_generation = ?2",
            params![sid, gen],
        )?;
        let hard_links: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM objects WHERE source_id = ?1 AND deleted_at IS NULL AND link_count > 1",
            params![sid],
            |r| r.get(0),
        )?;
        self.stats.hard_links = hard_links as u64;
        // 4. Aggregates.
        let agg =
            aggregates::rebuild_source(&self.conn, self.source.id, root, gen, &self.unlisted)?;
        // 5. Resolve enumeration errors from older generations.
        self.conn.execute(
            "UPDATE errors SET resolved_at = ?3 WHERE source_id = ?1 AND stage = 'enumerate' AND resolved_at IS NULL AND generation < ?2",
            params![sid, gen, now],
        )?;
        // 6. Publish.
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM objects WHERE source_id = ?1 AND deleted_at IS NULL AND content_state IN ('pending','stale')",
            params![sid],
            |r| r.get(0),
        )?;
        let final_state = if opts.degraded.is_some() {
            SourceState::Degraded
        } else if pending > 0 {
            SourceState::ContentPending
        } else {
            SourceState::MetadataComplete
        };
        self.conn.execute(
            "UPDATE scan_generations SET state = 'published', finished_at = ?3, published_at = ?3, dirs_listed = ?4,
                entries_seen = ?5, errors = ?6, tombstoned = ?7 WHERE source_id = ?1 AND generation = ?2",
            params![
                sid,
                gen,
                now,
                self.stats.dirs_listed as i64,
                self.stats.entries_seen as i64,
                self.stats.errors as i64,
                (tomb_entries + tomb_objects) as i64
            ],
        )?;
        let mut reasons: Vec<String> = Vec::new();
        if let Some(d) = opts.degraded {
            reasons.push(d.to_string());
        }
        if self.stats.errors > 0 {
            reasons.push(format!(
                "{} directories could not be listed; their previous contents were preserved",
                self.stats.errors
            ));
        }
        let reason = (!reasons.is_empty()).then(|| reasons.join("; "));
        self.conn.execute(
            "UPDATE sources SET published_generation = ?2, state = ?3, state_reason = ?4, last_scan_completed_at = ?5, updated_at = ?5
             WHERE source_id = ?1",
            params![sid, gen, final_state.as_str(), reason, now],
        )?;
        if let Some(cp) = opts.checkpoint {
            set_checkpoint_conn(&self.conn, self.source.id, cp)?;
        } else if opts.clear_checkpoint {
            clear_checkpoint_conn(&self.conn, self.source.id)?;
        }
        self.conn.execute_batch("COMMIT")?;
        self.in_tx = false;
        self.stats.commits += 1;
        tracing::info!(
            source = sid,
            generation = gen,
            dirs = self.stats.dirs_listed,
            entries = self.stats.entries_seen,
            errors = self.stats.errors,
            tombstoned = tomb_entries + tomb_objects,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "scan generation published"
        );
        Ok(ScanSummary {
            source_id: self.source.id,
            generation: gen,
            stats: self.stats.clone(),
            tombstoned_entries: tomb_entries,
            tombstoned_objects: tomb_objects,
            aggregates: agg,
            elapsed_ms: self.started.elapsed().as_secs_f64() * 1000.0,
            published: true,
            final_state,
        })
    }

    /// Abandon this generation without publishing.
    pub fn abort(mut self, reason: &str) -> Result<()> {
        if self.in_tx {
            let _ = self.conn.execute_batch("ROLLBACK");
            self.in_tx = false;
        }
        abort_generation(&self.conn, self.source.id, self.generation, reason, None)
    }
}

fn abort_generation(
    conn: &Connection,
    source_id: SourceId,
    generation: i64,
    reason: &str,
    override_state: Option<SourceState>,
) -> Result<()> {
    let now = UnixNanos::now().0;
    conn.execute(
        "UPDATE scan_generations SET state = 'aborted', finished_at = ?3, note = ?4 WHERE source_id = ?1 AND generation = ?2",
        params![source_id.0, generation, now, reason],
    )?;
    let published: Option<i64> = conn.query_row(
        "SELECT published_generation FROM sources WHERE source_id = ?1",
        params![source_id.0],
        |r| r.get(0),
    )?;
    let state = match override_state {
        Some(s) => s,
        None if published.is_some() => SourceState::Degraded,
        None => SourceState::New,
    };
    conn.execute(
        "UPDATE sources SET state = ?2, state_reason = ?3, updated_at = ?4 WHERE source_id = ?1",
        params![source_id.0, state.as_str(), reason, now],
    )?;
    tracing::warn!(
        source = source_id.0,
        generation,
        reason,
        "scan generation aborted"
    );
    Ok(())
}

/// Crash recovery: abort every generation still marked `open`.
pub fn recover_open_generations(conn: &mut Connection) -> Result<RecoveryReport> {
    let open: Vec<(i64, i64)> = conn
        .prepare("SELECT source_id, generation FROM scan_generations WHERE state = 'open'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut report = RecoveryReport::default();
    for (sid, gen) in open {
        abort_generation(
            conn,
            SourceId(sid),
            gen,
            "scan interrupted before publication (recovered at startup)",
            None,
        )?;
        report.aborted_generations.push((SourceId(sid), gen));
    }
    Ok(report)
}

/// Options for a complete walk-and-ingest of a source.
#[derive(Debug, Clone)]
pub struct RunScanOptions {
    pub walk: eidos_scanner::WalkOptions,
    pub kind: ScanKind,
    /// If set, rows per commit / max commit interval.
    pub batching: Option<(usize, std::time::Duration)>,
}

impl Default for RunScanOptions {
    fn default() -> Self {
        Self {
            walk: eidos_scanner::WalkOptions::default(),
            kind: ScanKind::Full,
            batching: None,
        }
    }
}

/// Walk the source root with `lister` and ingest into a new generation.
/// Publishes on success; aborts (never publishes) if ingestion fails or the
/// walk is cancelled.
pub fn run_scan(
    catalog: &Catalog,
    source_id: SourceId,
    lister: &dyn eidos_scanner::DirectoryLister,
    opts: &RunScanOptions,
) -> Result<ScanSummary> {
    let source = catalog
        .get_source(source_id)?
        .ok_or_else(|| CatalogError::NotFound(format!("source {source_id}")))?;
    let mut session = catalog.begin_scan(source_id, opts.kind)?;
    if let Some((rows, interval)) = opts.batching {
        session.set_batching(rows, interval);
    }
    if let Ok(root_entry) = lister.stat(std::path::Path::new(&source.root_path)) {
        if let Some(native) = root_entry.native_id {
            catalog.set_root_identity(source_id, native)?;
        }
    }
    let cancel = opts
        .walk
        .cancel
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let mut walk_opts = opts.walk.clone();
    walk_opts.cancel = Some(cancel.clone());
    let root = std::path::PathBuf::from(&source.root_path);
    let mut ingest_error: Option<CatalogError> = None;
    let stats = eidos_scanner::walk(&root, lister, &walk_opts, |ev| {
        if ingest_error.is_some() {
            return;
        }
        if let Err(e) = session.ingest(ev) {
            tracing::error!(error = %e, "ingest failed; cancelling walk");
            ingest_error = Some(e);
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    if let Some(e) = ingest_error {
        session.abort(&format!("ingest error: {e}"))?;
        return Err(e);
    }
    if stats.cancelled {
        session.abort("walk cancelled before completion")?;
        return Err(CatalogError::InvalidState("scan cancelled".into()));
    }
    session.finish()
}

fn map_existing(r: &rusqlite::Row<'_>) -> rusqlite::Result<ExistingObject> {
    Ok(ExistingObject {
        id: ObjectId(r.get(0)?),
        kind: r.get(1)?,
        size: r.get(2)?,
        modified: r.get(3)?,
        changed: r.get(4)?,
        generation: r.get(5)?,
        content_state: r.get(6)?,
    })
}

fn initial_content_state(decision: ContentDecision, kind: ObjectKind) -> ContentState {
    if kind != ObjectKind::File {
        return ContentState::NotApplicable;
    }
    match decision {
        ContentDecision::Candidate => ContentState::Pending,
        ContentDecision::Unsupported => ContentState::Unsupported,
        ContentDecision::Excluded { .. } => ContentState::Excluded,
    }
}

fn eidos_scanner_display(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s
}
