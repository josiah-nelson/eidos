//! Bulk rebuild and outbox follower for the catalog index.

use crate::schema::document;
use crate::{CatalogIndex, Result, PROJECTION_NAME};
use eidos_catalog::jobs::OutboxRow;
use eidos_catalog::Catalog;
use eidos_domain::{ObjectId, SourceId, SourceState};
use std::collections::HashSet;
use std::time::Instant;
use tantivy::Term;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RebuildStats {
    pub source_id: i64,
    pub generation: i64,
    pub documents: u64,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FollowStats {
    pub rows: u64,
    pub documents_added: u64,
    pub documents_deleted: u64,
    pub subtree_rebuilds: u64,
    pub last_seq: i64,
    pub elapsed_ms: f64,
}

/// Subtree roots that lie under another root of the same batch. One
/// ancestor query per root, and none at all for the common single-root
/// batch.
fn nested_roots(
    catalog: &Catalog,
    subtrees: &[ObjectId],
    roots: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let mut nested = HashSet::new();
    if subtrees.len() < 2 {
        return Ok(nested);
    }
    for root in subtrees {
        if catalog
            .ancestor_object_ids(*root)?
            .iter()
            .any(|a| roots.contains(a))
        {
            nested.insert(*root);
        }
    }
    Ok(nested)
}

impl CatalogIndex {
    /// Replace every document of a source with the catalog's published
    /// generation. Commits once at the end.
    pub fn rebuild_source(&self, catalog: &Catalog, source_id: SourceId) -> Result<RebuildStats> {
        let started = Instant::now();
        let src = catalog
            .get_source(source_id)?
            .ok_or_else(|| crate::SearchError::Other(format!("source {source_id} not found")))?;
        let generation = src.published_generation.unwrap_or(0);
        let f = self.fields();
        let writer = self.writer();
        writer.delete_term(Term::from_field_u64(f.source_id, source_id.0 as u64));
        let mut documents = 0u64;
        catalog.for_each_projection_row(source_id, |row| {
            writer.add_document(document(f, &row)).map_err(|e| {
                eidos_catalog::CatalogError::InvalidState(format!("index write: {e}"))
            })?;
            documents += 1;
            Ok(())
        })?;
        drop(writer);
        self.commit_and_reload()?;
        catalog.set_projection_source(PROJECTION_NAME, source_id, generation, documents)?;
        let stats = RebuildStats {
            source_id: source_id.0,
            generation,
            documents,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        };
        tracing::info!(
            source = source_id.0,
            generation,
            documents,
            ms = stats.elapsed_ms as u64,
            "catalog index rebuilt"
        );
        Ok(stats)
    }

    /// Remove every document of a source (retired/removed sources).
    pub fn remove_source(&self, catalog: &Catalog, source_id: SourceId) -> Result<()> {
        let f = self.fields();
        self.writer()
            .delete_term(Term::from_field_u64(f.source_id, source_id.0 as u64));
        self.commit_and_reload()?;
        catalog.clear_projection_source(PROJECTION_NAME, source_id)?;
        Ok(())
    }

    /// Rebuild every source whose published generation is newer than the
    /// projection's. Returns the sources rebuilt.
    pub fn sync_sources(&self, catalog: &Catalog) -> Result<Vec<RebuildStats>> {
        let mut out = Vec::new();
        // A recreated (empty) index invalidates the catalog's record of what
        // was built; without this, equal generation numbers would make an
        // empty index look synchronised.
        let force = self.take_recreated();
        if force {
            tracing::warn!("catalog index was recreated; rebuilding every source");
        }
        // Same situation left behind by a process that recreated the index
        // and exited before rebuilding: an empty index whose projection
        // record claims documents.
        let empty = self.num_docs() == 0;
        for s in catalog.list_sources()? {
            let published = match s.published_generation {
                Some(g) => g,
                None => continue,
            };
            if s.state == SourceState::Retired {
                if catalog.projection_source(PROJECTION_NAME, s.id)?.is_some() {
                    self.remove_source(catalog, s.id)?;
                }
                continue;
            }
            let record = catalog.projection_source(PROJECTION_NAME, s.id)?;
            let built = record.as_ref().map(|p| p.generation).unwrap_or(-1);
            let stale_empty = empty && record.as_ref().is_some_and(|p| p.documents > 0);
            if force || stale_empty || built != published {
                out.push(self.rebuild_source(catalog, s.id)?);
            }
        }
        Ok(out)
    }

    /// Apply outbox rows (idempotent: each affected object is deleted then
    /// re-added from the catalog's current state). Commits once.
    ///
    /// Work is coalesced across the batch: every object is deleted and
    /// reindexed exactly once, no matter how many rows name it or how many
    /// subtrees contain it. Deletions all precede the additions, so a
    /// subtree replacement cannot remove documents added earlier in the same
    /// batch (Tantivy deletes only apply to documents added before them).
    pub fn apply_outbox(&self, catalog: &Catalog, rows: &[OutboxRow]) -> Result<FollowStats> {
        let started = Instant::now();
        let mut stats = FollowStats::default();
        let mut subtrees: Vec<ObjectId> = Vec::new();
        let mut affected: Vec<ObjectId> = Vec::new();
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut subtree_seen: HashSet<ObjectId> = HashSet::new();
        for row in rows {
            stats.rows += 1;
            stats.last_seq = row.seq;
            if row.op.as_str() == "subtree" {
                stats.subtree_rebuilds += 1;
                if subtree_seen.insert(row.object_id) {
                    subtrees.push(row.object_id);
                }
            }
            if seen.insert(row.object_id) {
                affected.push(row.object_id);
            }
        }
        // Expand subtrees. A root nested inside another root of the same
        // batch needs no walk of its own: its live descendants are a subset.
        // Which roots those are is settled up front, from each root's
        // ancestors, so the saving does not depend on the order the rows
        // arrived in. Nested roots are still deleted by ancestry below,
        // because a document orphaned by an older generation may name one of
        // them as an ancestor without naming the outer root.
        let nested = nested_roots(catalog, &subtrees, &subtree_seen)?;
        for root in &subtrees {
            if nested.contains(root) {
                continue;
            }
            for d in catalog.descendant_object_ids(*root)? {
                if seen.insert(d) {
                    affected.push(d);
                }
            }
        }
        if !affected.is_empty() {
            let f = self.fields();
            let writer = self.writer();
            // A subtree replacement may use fresh object ids (archive
            // generations do), so remove documents of the old tree by
            // ancestry before walking the current catalog rows.
            for root in &subtrees {
                writer.delete_term(Term::from_field_u64(f.ancestors, root.0 as u64));
            }
            for object in &affected {
                writer.delete_term(Term::from_field_u64(f.object_id, object.0 as u64));
                stats.documents_deleted += 1;
            }
            let mut added = 0u64;
            catalog.for_each_projection_row_of(&affected, |row| {
                writer.add_document(document(f, &row)).map_err(|e| {
                    eidos_catalog::CatalogError::InvalidState(format!("index write: {e}"))
                })?;
                added += 1;
                Ok(())
            })?;
            stats.documents_added += added;
        }
        if !rows.is_empty() {
            self.commit_and_reload()?;
        }
        stats.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(stats)
    }

    /// One follower iteration: sync generations, then drain the outbox from
    /// the stored position. Returns `(rebuilt, follow)`.
    pub fn follow_once(
        &self,
        catalog: &Catalog,
        batch: u32,
    ) -> Result<(Vec<RebuildStats>, Option<FollowStats>)> {
        let rebuilt = self.sync_sources(catalog)?;
        let position = catalog.projection_position(PROJECTION_NAME)?;
        let rows = catalog.outbox_poll(position, batch)?;
        if rows.is_empty() {
            // Idle iterations still release a bounded slice of consumed rows
            // so a backlog left by an upgrade, or one larger than the
            // per-iteration allowance, drains without waiting for writes —
            // but only when something is actually prunable. The prune is a
            // writer transaction and every writer transaction bumps the
            // write signal, so pruning unconditionally here made an idle
            // follower wake itself in a tight loop forever (~2,200 writer
            // acquisitions/s, ~90% writer hold time measured on v0.5.0),
            // starving scan batches and content workers.
            if catalog.outbox_has_prunable()? {
                catalog.outbox_prune(batch.max(1).saturating_mul(4))?;
            }
            return Ok((rebuilt, None));
        }
        let stats = self.apply_outbox(catalog, &rows)?;
        // Index committed; now record the position and consume.
        catalog.set_projection_position(PROJECTION_NAME, stats.last_seq)?;
        catalog.outbox_consume(stats.last_seq)?;
        // Consumed rows below every projection position are dead weight;
        // release a bounded slice each iteration so the table stays O(pending).
        catalog.outbox_prune(batch.max(1).saturating_mul(4))?;
        Ok((rebuilt, Some(stats)))
    }
}
