//! Bulk rebuild and outbox follower for the catalog index.

use crate::schema::document;
use crate::{CatalogIndex, Result, PROJECTION_NAME};
use eidos_catalog::jobs::OutboxRow;
use eidos_catalog::Catalog;
use eidos_domain::{ObjectId, SourceId, SourceState};
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
        self.writer().commit()?;
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
        self.writer().commit()?;
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
            let built = catalog
                .projection_source(PROJECTION_NAME, s.id)?
                .map(|p| p.generation)
                .unwrap_or(-1);
            if force || built != published {
                out.push(self.rebuild_source(catalog, s.id)?);
            }
        }
        Ok(out)
    }

    fn reindex_object(
        &self,
        catalog: &Catalog,
        object: ObjectId,
        stats: &mut FollowStats,
    ) -> Result<()> {
        let f = self.fields();
        let writer = self.writer();
        writer.delete_term(Term::from_field_u64(f.object_id, object.0 as u64));
        stats.documents_deleted += 1;
        for row in catalog.projection_rows_for_object(object)? {
            writer.add_document(document(f, &row)).map_err(|e| {
                eidos_catalog::CatalogError::InvalidState(format!("index write: {e}"))
            })?;
            stats.documents_added += 1;
        }
        Ok(())
    }

    /// Apply outbox rows (idempotent: each object is deleted then re-added
    /// from the catalog's current state). Commits once.
    pub fn apply_outbox(&self, catalog: &Catalog, rows: &[OutboxRow]) -> Result<FollowStats> {
        let started = Instant::now();
        let mut stats = FollowStats::default();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            stats.rows += 1;
            stats.last_seq = row.seq;
            match row.op.as_str() {
                "subtree" => {
                    stats.subtree_rebuilds += 1;
                    if seen.insert(row.object_id) {
                        self.reindex_object(catalog, row.object_id, &mut stats)?;
                    }
                    for d in catalog.descendant_object_ids(row.object_id)? {
                        if seen.insert(d) {
                            self.reindex_object(catalog, d, &mut stats)?;
                        }
                    }
                }
                _ => {
                    if seen.insert(row.object_id) {
                        self.reindex_object(catalog, row.object_id, &mut stats)?;
                    }
                }
            }
        }
        if !rows.is_empty() {
            self.writer().commit()?;
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
            return Ok((rebuilt, None));
        }
        let stats = self.apply_outbox(catalog, &rows)?;
        // Index committed; now record the position and consume.
        catalog.set_projection_position(PROJECTION_NAME, stats.last_seq)?;
        catalog.outbox_consume(stats.last_seq)?;
        Ok((rebuilt, Some(stats)))
    }
}
