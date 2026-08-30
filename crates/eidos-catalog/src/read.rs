//! Administrative writes (hosts, sources, volumes) and read queries.

use crate::model::*;
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{
    ContentState, Freshness, HostId, ObjectId, SourceCompleteness, SourceId, SourceKind,
    SourceState, UnixNanos, VolumeId,
};
use eidos_scanner::VolumeInfo;
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) fn get_source_conn(conn: &Connection, id: SourceId) -> Result<Option<SourceRecord>> {
    Ok(conn
        .prepare_cached("SELECT * FROM sources WHERE source_id = ?1")?
        .query_row(params![id.0], SourceRecord::from_row)
        .optional()?)
}

impl Catalog {
    // ----- hosts / sources / volumes -------------------------------------

    pub fn ensure_host(&self, name: &str, platform: &str) -> Result<HostId> {
        self.with_writer(|conn| {
            if let Some(id) = conn
                .query_row(
                    "SELECT host_id FROM hosts WHERE name = ?1",
                    params![name],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            {
                return Ok(HostId(id));
            }
            conn.execute(
                "INSERT INTO hosts (name, platform, created_at) VALUES (?1, ?2, ?3)",
                params![name, platform, UnixNanos::now().0],
            )?;
            Ok(HostId(conn.last_insert_rowid()))
        })
    }

    pub fn add_source(&self, s: &NewSource) -> Result<SourceId> {
        if s.kind == SourceKind::Remote {
            return Err(CatalogError::InvalidState(
                "remote sources are created by fleet replication, not by hand".into(),
            ));
        }
        self.with_writer(|conn| {
            let now = UnixNanos::now().0;
            conn.execute(
                "INSERT INTO sources (host_id, name, kind, root_path, aliases, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'new', ?6, ?6)",
                params![
                    s.host_id.0,
                    s.name,
                    s.kind.as_str(),
                    s.root_path,
                    serde_json::to_string(&s.aliases)?,
                    now
                ],
            )?;
            Ok(SourceId(conn.last_insert_rowid()))
        })
    }

    pub fn get_source(&self, id: SourceId) -> Result<Option<SourceRecord>> {
        self.with_reader(|conn| get_source_conn(conn, id))
    }

    pub fn find_source_by_name(&self, name: &str) -> Result<Option<SourceRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached("SELECT * FROM sources WHERE name = ?1")?
                .query_row(params![name], SourceRecord::from_row)
                .optional()?)
        })
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached("SELECT * FROM sources ORDER BY source_id")?
                .query_map([], SourceRecord::from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn set_source_state(
        &self,
        id: SourceId,
        state: SourceState,
        reason: Option<&str>,
    ) -> Result<()> {
        self.with_writer(|conn| {
            let n = conn.execute(
                "UPDATE sources SET state = ?2, state_reason = ?3, updated_at = ?4 WHERE source_id = ?1",
                params![id.0, state.as_str(), reason, UnixNanos::now().0],
            )?;
            if n == 0 {
                return Err(CatalogError::NotFound(format!("source {id}")));
            }
            Ok(())
        })
    }

    pub fn set_source_kind(&self, id: SourceId, kind: SourceKind) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE sources SET kind = ?2, updated_at = ?3 WHERE source_id = ?1",
                params![id.0, kind.as_str(), UnixNanos::now().0],
            )?;
            Ok(())
        })
    }

    /// Record the volume beneath a source and link it.
    pub fn upsert_volume(
        &self,
        host: HostId,
        source: SourceId,
        v: &VolumeInfo,
    ) -> Result<VolumeId> {
        self.with_writer(|conn| {
            let now = UnixNanos::now().0;
            conn.execute(
                "INSERT INTO volumes (host_id, volume_serial, filesystem, volume_name, drive_type, fs_flags, bytes_per_cluster,
                    volume_root, supports_usn, supports_file_ids, case_sensitive, native_feed, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(host_id, volume_serial) DO UPDATE SET filesystem = excluded.filesystem, volume_name = excluded.volume_name,
                    drive_type = excluded.drive_type, fs_flags = excluded.fs_flags, bytes_per_cluster = excluded.bytes_per_cluster,
                    volume_root = excluded.volume_root, supports_usn = excluded.supports_usn,
                    supports_file_ids = excluded.supports_file_ids, case_sensitive = excluded.case_sensitive,
                    native_feed = excluded.native_feed, updated_at = excluded.updated_at",
                params![
                    host.0,
                    v.volume_serial as i64,
                    v.filesystem,
                    v.volume_name,
                    format!("{:?}", v.drive_type).to_lowercase(),
                    v.fs_flags as i64,
                    v.bytes_per_cluster as i64,
                    v.volume_root,
                    v.supports_usn as i64,
                    v.supports_file_ids as i64,
                    match v.case_sensitive {
                        Some(true) => "sensitive",
                        Some(false) => "insensitive",
                        None => "unknown",
                    },
                    v.native_feed.as_str(),
                    now
                ],
            )?;
            let id: i64 = conn.query_row(
                "SELECT volume_id FROM volumes WHERE host_id = ?1 AND volume_serial = ?2",
                params![host.0, v.volume_serial as i64],
                |r| r.get(0),
            )?;
            conn.execute(
                "UPDATE sources SET volume_id = ?2, updated_at = ?3 WHERE source_id = ?1",
                params![source.0, id, now],
            )?;
            Ok(VolumeId(id))
        })
    }

    // ----- objects / entries ---------------------------------------------

    pub fn get_object(&self, id: ObjectId) -> Result<Option<ObjectRecord>> {
        self.with_reader(|conn| get_object_conn(conn, id))
    }

    /// Current generation of each live object in `objects`.
    ///
    /// Ids that are unknown or tombstoned are absent from the map, so a
    /// caller validating a derived artefact (an index document, a stored
    /// chunk) against the catalog can treat "absent" as "not current".
    pub fn object_generations(
        &self,
        objects: &[ObjectId],
    ) -> Result<std::collections::HashMap<ObjectId, u32>> {
        let mut out = std::collections::HashMap::with_capacity(objects.len());
        if objects.is_empty() {
            return Ok(out);
        }
        // One prepared statement for the whole set, whatever its size: a
        // caller validating a broad candidate list holds a pooled reader for
        // a single scan rather than a chain of round trips.
        let ids = serde_json::to_string(&objects.iter().map(|o| o.0).collect::<Vec<i64>>())?;
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT o.object_id, o.generation FROM json_each(?1) j
                 JOIN objects o ON o.object_id = j.value
                 WHERE o.deleted_at IS NULL",
            )?;
            let mut rows = stmt.query(params![ids])?;
            while let Some(r) = rows.next()? {
                out.insert(ObjectId(r.get::<_, i64>(0)?), r.get::<_, i64>(1)? as u32);
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Render the current path of an object by following parent links.
    pub fn render_path(&self, id: ObjectId) -> Result<Option<String>> {
        self.with_reader(|conn| render_path_conn(conn, id))
    }

    /// Whether names on this source's volume differ by case. Unknown volumes
    /// and sources without one resolve case-insensitively, which is what
    /// Windows and the default APFS layout do.
    pub(crate) fn source_is_case_sensitive(conn: &Connection, source: SourceId) -> Result<bool> {
        Ok(conn
            .prepare_cached(
                "SELECT COALESCE(CASE WHEN s.kind = 'remote' THEN s.case_sensitive ELSE v.case_sensitive END, 'unknown')
                 FROM sources s LEFT JOIN volumes v ON v.volume_id = s.volume_id
                 WHERE s.source_id = ?1",
            )?
            .query_row(params![source.0], |r| r.get::<_, String>(0))
            .optional()?
            .is_some_and(|value| value == "sensitive"))
    }

    pub fn is_source_case_sensitive(&self, source: SourceId) -> Result<bool> {
        self.with_reader(|conn| Self::source_is_case_sensitive(conn, source))
    }

    /// Resolve a path relative to the source root. POSIX sources split only
    /// on `/`, because `\` is legal inside one of their file names; Windows
    /// sources accept either separator.
    ///
    /// A case-sensitive volume can hold `Report.txt` and `report.txt` side by
    /// side, so the folded name is only a candidate filter there and the exact
    /// name decides. The folded index still drives the lookup in both cases.
    pub fn resolve_relative(&self, source: SourceId, relative: &str) -> Result<Option<ObjectId>> {
        self.with_reader(|conn| {
            let src = get_source_conn(conn, source)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {source}")))?;
            let mut cur = match src.root_object_id {
                Some(r) => r,
                None => return Ok(None),
            };
            let case_sensitive = Self::source_is_case_sensitive(conn, source)?;
            let separator = crate::paths::separator(&src.root_path);
            let mut folded_stmt = conn.prepare_cached(
                "SELECT object_id FROM entries WHERE source_id = ?1 AND parent_id = ?2 AND name_folded = ?3 AND deleted_at IS NULL LIMIT 1",
            )?;
            let mut exact_stmt = conn.prepare_cached(
                "SELECT object_id FROM entries WHERE source_id = ?1 AND parent_id = ?2 AND name_folded = ?3 AND name = ?4 AND deleted_at IS NULL LIMIT 1",
            )?;
            for comp in relative
                .split(|c| c == '/' || (separator == '\\' && c == '\\'))
                .filter(|c| !c.is_empty())
            {
                let folded = crate::policy::fold(comp);
                let found = if case_sensitive {
                    exact_stmt
                        .query_row(params![source.0, cur.0, folded, comp], |r| r.get::<_, i64>(0))
                        .optional()?
                } else {
                    folded_stmt
                        .query_row(params![source.0, cur.0, folded], |r| r.get::<_, i64>(0))
                        .optional()?
                };
                match found {
                    Some(next) => cur = ObjectId(next),
                    None => return Ok(None),
                }
            }
            Ok(Some(cur))
        })
    }

    pub fn list_children(&self, dir: ObjectId, page: &ChildrenPage) -> Result<ChildrenResult> {
        self.with_reader(|conn| {
            let hidden_filter = if page.include_hidden {
                ""
            } else {
                " AND (o.attributes & 6) = 0"
            };
            let dir_sort = if page.descending { "ASC" } else { "DESC" };
            let order = match page.sort {
                ChildSort::Name => format!("(o.kind = 'directory') {dir_sort}, e.name_folded {}", sort_dir(page.descending)),
                ChildSort::Size => format!("COALESCE(a.logical_bytes, o.size) {}, e.name_folded ASC", sort_dir(page.descending)),
                ChildSort::AllocatedSize => {
                    format!("COALESCE(a.allocated_bytes, o.allocated) {}, e.name_folded ASC", sort_dir(page.descending))
                }
                ChildSort::Modified => {
                    format!("COALESCE(a.newest_modified, o.modified) {}, e.name_folded ASC", sort_dir(page.descending))
                }
                ChildSort::Kind => format!("o.kind {}, e.extension {}, e.name_folded ASC", sort_dir(page.descending), sort_dir(page.descending)),
            };
            let total: i64 = conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM entries e JOIN objects o ON o.object_id = e.object_id
                     WHERE e.parent_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL{hidden_filter}"
                ),
                params![dir.0],
                |r| r.get(0),
            )?;
            let sql = format!(
                "SELECT {ENTRY_COLUMNS}, {OBJECT_COLUMNS}, {AGG_COLUMNS}
                 FROM entries e JOIN objects o ON o.object_id = e.object_id
                 LEFT JOIN directory_aggregates a ON a.object_id = o.object_id
                 WHERE e.parent_id = ?1 AND e.deleted_at IS NULL AND o.deleted_at IS NULL{hidden_filter}
                 ORDER BY {order} LIMIT ?2 OFFSET ?3"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt
                .query_map(params![dir.0, page.limit as i64, page.offset as i64], |r| {
                    Ok(ChildRow {
                        entry: EntryRecord::from_row_at(r, 0)?,
                        object: ObjectRecord::from_row_at(r, 10)?,
                        aggregate: DirectoryAggregate::from_row_opt(r, 32)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ChildrenResult {
                rows,
                total: total as u64,
                offset: page.offset,
            })
        })
    }

    pub fn directory_aggregate(&self, dir: ObjectId) -> Result<Option<DirectoryAggregate>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {AGG_COLUMNS} FROM directory_aggregates a WHERE a.object_id = ?1"
                ))?
                .query_row(params![dir.0], |r| DirectoryAggregate::from_row_at(r, 0))
                .optional()?)
        })
    }

    pub fn extension_counts(&self, dir: ObjectId, limit: u32) -> Result<Vec<ExtensionCount>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT extension, count, bytes FROM directory_extension_counts WHERE object_id = ?1
                     ORDER BY count DESC, extension LIMIT ?2",
                )?
                .query_map(params![dir.0, limit as i64], |r| {
                    Ok(ExtensionCount {
                        extension: r.get(0)?,
                        count: r.get::<_, i64>(1)? as u64,
                        bytes: r.get::<_, i64>(2)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Entries referring to an object (more than one means hard links).
    pub fn entries_for_object(&self, id: ObjectId) -> Result<Vec<EntryRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM entries e WHERE e.object_id = ?1 AND e.deleted_at IS NULL ORDER BY e.entry_id"
                ))?
                .query_map(params![id.0], |r| EntryRecord::from_row_at(r, 0))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn policy_decisions(&self, id: ObjectId) -> Result<Vec<PolicyDecisionRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT object_id, stage, included, reason, rule, policy_version, user_override FROM policy_decisions WHERE object_id = ?1",
                )?
                .query_map(params![id.0], |r| {
                    Ok(PolicyDecisionRecord {
                        object_id: ObjectId(r.get(0)?),
                        stage: r.get(1)?,
                        included: r.get::<_, i64>(2)? != 0,
                        reason: r.get(3)?,
                        rule: r.get(4)?,
                        policy_version: r.get::<_, i64>(5)? as u32,
                        user_override: r.get::<_, i64>(6)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    // ----- health / completeness ------------------------------------------

    pub fn source_counts(&self, id: SourceId) -> Result<SourceCounts> {
        self.with_reader(|conn| source_counts_conn(conn, id))
    }

    /// Completeness for search responses. Uses the root directory aggregate
    /// (O(1)) rather than grouped counts so it stays cheap on every query.
    pub fn source_completeness(&self, id: SourceId) -> Result<SourceCompleteness> {
        self.with_reader(|conn| {
            let src = get_source_conn(conn, id)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {id}")))?;
            let counts = light_counts_conn(conn, &src)?;
            let listing_errors = published_listing_errors_conn(conn, &src)?;
            Ok(completeness_from(&src, &counts, listing_errors))
        })
    }

    /// Listing errors recorded by the published generation of a source.
    pub fn published_listing_errors(&self, id: SourceId) -> Result<u64> {
        self.with_reader(|conn| {
            let src = get_source_conn(conn, id)?
                .ok_or_else(|| CatalogError::NotFound(format!("source {id}")))?;
            published_listing_errors_conn(conn, &src)
        })
    }

    pub fn list_generations(&self, id: SourceId, limit: u32) -> Result<Vec<ScanGenerationRecord>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT * FROM scan_generations WHERE source_id = ?1 ORDER BY generation DESC LIMIT ?2",
                )?
                .query_map(params![id.0, limit as i64], ScanGenerationRecord::from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn list_errors(
        &self,
        source: Option<SourceId>,
        include_resolved: bool,
        limit: u32,
    ) -> Result<Vec<ErrorRecord>> {
        self.with_reader(|conn| {
            let mut sql = String::from(
                "SELECT error_id, source_id, object_id, generation, stage, kind, code, path, message, occurred_at, resolved_at FROM errors WHERE (?1 IS NULL OR source_id = ?1)",
            );
            if !include_resolved {
                sql.push_str(" AND resolved_at IS NULL");
            }
            sql.push_str(" ORDER BY occurred_at DESC LIMIT ?2");
            let map = |r: &rusqlite::Row<'_>| {
                Ok(ErrorRecord {
                    id: r.get(0)?,
                    source_id: SourceId(r.get(1)?),
                    object_id: r.get::<_, Option<i64>>(2)?.map(ObjectId),
                    generation: r.get(3)?,
                    stage: r.get(4)?,
                    kind: r.get(5)?,
                    code: r.get(6)?,
                    path: r.get(7)?,
                    message: r.get(8)?,
                    occurred_at: UnixNanos(r.get(9)?),
                    resolved_at: r.get::<_, Option<i64>>(10)?.map(UnixNanos),
                })
            };
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt
                .query_map(params![source.map(|s| s.0), limit as i64], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Exclusion summary for a source: reason -> (count, bytes).
    pub fn exclusion_summary(&self, id: SourceId) -> Result<Vec<(String, String, u64, u64)>> {
        self.with_reader(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT p.stage, p.reason, COUNT(*), COALESCE(SUM(o.size), 0)
                     FROM policy_decisions p JOIN objects o ON o.object_id = p.object_id
                     WHERE o.source_id = ?1 AND o.deleted_at IS NULL AND p.included = 0
                     GROUP BY p.stage, p.reason ORDER BY 3 DESC",
                )?
                .query_map(params![id.0], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
}

fn sort_dir(desc: bool) -> &'static str {
    if desc {
        "DESC"
    } else {
        "ASC"
    }
}

pub(crate) fn get_object_conn(conn: &Connection, id: ObjectId) -> Result<Option<ObjectRecord>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {OBJECT_COLUMNS} FROM objects o WHERE o.object_id = ?1"
        ))?
        .query_row(params![id.0], |r| ObjectRecord::from_row_at(r, 0))
        .optional()?)
}

pub(crate) fn render_path_conn(conn: &Connection, id: ObjectId) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT parent_id, name, source_id FROM entries WHERE object_id = ?1 AND deleted_at IS NULL ORDER BY entry_id LIMIT 1",
    )?;
    let mut parts: Vec<String> = Vec::new();
    let mut cur = id;
    let mut source_id: Option<i64> = None;
    for _ in 0..512 {
        let row = stmt
            .query_row(params![cur.0], |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .optional()?;
        match row {
            None => return Ok(None),
            Some((parent, name, sid)) => {
                source_id = Some(sid);
                match parent {
                    None => break,
                    Some(p) => {
                        parts.push(name);
                        cur = ObjectId(p);
                    }
                }
            }
        }
    }
    let sid = match source_id {
        Some(s) => s,
        None => return Ok(None),
    };
    let root: String = conn.query_row(
        "SELECT root_path FROM sources WHERE source_id = ?1",
        params![sid],
        |r| r.get(0),
    )?;
    if parts.is_empty() {
        return Ok(Some(crate::paths::root_display(&root)));
    }
    let mut out = crate::paths::root_display(&root);
    for p in parts.iter().rev() {
        out = crate::paths::join(&root, &out, p);
    }
    Ok(Some(out))
}

/// Cheap counts from the root aggregate only (no table scans).
pub(crate) fn light_counts_conn(conn: &Connection, src: &SourceRecord) -> Result<SourceCounts> {
    let mut c = SourceCounts::default();
    if let Some(root) = src.root_object_id {
        if let Some(row) = conn
            .query_row(
                "SELECT file_count, dir_count, logical_bytes, allocated_bytes, content_pending, content_indexed,
                        content_failed, content_excluded FROM directory_aggregates WHERE object_id = ?1",
                params![root.0],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?
        {
            c.files = row.0 as u64;
            c.directories = row.1 as u64;
            c.logical_bytes = row.2 as u64;
            c.allocated_bytes = row.3 as u64;
            c.content_pending = row.4.max(0) as u64;
            c.content_indexed = row.5.max(0) as u64;
            c.content_failed = row.6.max(0) as u64;
            c.content_excluded = row.7.max(0) as u64;
        }
    }
    Ok(c)
}

pub(crate) fn source_counts_conn(conn: &Connection, id: SourceId) -> Result<SourceCounts> {
    let mut c = SourceCounts::default();
    let mut stmt = conn.prepare_cached(
        "SELECT content_state, COUNT(*) FROM objects WHERE source_id = ?1 AND deleted_at IS NULL AND kind = 'file' GROUP BY content_state",
    )?;
    let rows = stmt.query_map(params![id.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    for row in rows {
        let (state, n) = row?;
        match ContentState::parse(&state) {
            Some(ContentState::Pending) | Some(ContentState::Stale) => c.content_pending += n,
            Some(ContentState::Indexed) | Some(ContentState::Partial) => c.content_indexed += n,
            Some(ContentState::Failed) => c.content_failed += n,
            Some(ContentState::Excluded) => c.content_excluded += n,
            Some(ContentState::Unsupported) => c.content_unsupported += n,
            _ => {}
        }
    }
    let (objects, entries): (i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM objects WHERE source_id = ?1 AND deleted_at IS NULL),
                (SELECT COUNT(*) FROM entries WHERE source_id = ?1 AND deleted_at IS NULL AND parent_id IS NOT NULL)",
        params![id.0],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    c.objects = objects as u64;
    c.entries = entries as u64;
    if let Some(root) = conn
        .query_row(
            "SELECT root_object_id FROM sources WHERE source_id = ?1",
            params![id.0],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
    {
        if let Some((f, d, lb, ab)) = conn
            .query_row(
                "SELECT file_count, dir_count, logical_bytes, allocated_bytes FROM directory_aggregates WHERE object_id = ?1",
                params![root],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
        {
            c.files = f as u64;
            c.directories = d as u64;
            c.logical_bytes = lb as u64;
            c.allocated_bytes = ab as u64;
        }
    }
    c.open_errors = conn.query_row(
        "SELECT COUNT(*) FROM errors WHERE source_id = ?1 AND resolved_at IS NULL",
        params![id.0],
        |r| r.get::<_, i64>(0),
    )? as u64;
    Ok(c)
}

pub(crate) fn published_listing_errors_conn(conn: &Connection, src: &SourceRecord) -> Result<u64> {
    match src.published_generation {
        None => Ok(0),
        Some(g) => Ok(conn
            .query_row(
                "SELECT errors FROM scan_generations WHERE source_id = ?1 AND generation = ?2",
                params![src.id.0, g],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0) as u64),
    }
}

pub fn completeness_from(
    src: &SourceRecord,
    counts: &SourceCounts,
    listing_errors: u64,
) -> SourceCompleteness {
    let metadata_complete = src.published_generation.is_some() && src.state.metadata_complete();
    let content_complete = metadata_complete && counts.content_pending == 0;
    let freshness = match (src.state, src.checkpoint_kind.as_deref()) {
        (SourceState::Degraded, _) | (SourceState::Offline, _) | (SourceState::Stale, _) => {
            Freshness::Unknown
        }
        (_, kind) if crate::changes::is_native_feed_checkpoint(kind) => Freshness::Live,
        _ => Freshness::Periodic,
    };
    let checkpoint_age_ms = src
        .checkpoint_at
        .map(|t| (UnixNanos::now().as_millis() - t.as_millis()).max(0) as u64);
    let mut note = src.state_reason.clone();
    if src.state == SourceState::Enumerating {
        note = Some("initial enumeration in progress; results are partial".into());
    } else if src.state == SourceState::Reconciling {
        note = Some(
            "rescan in progress; showing last published generation plus new observations".into(),
        );
    }
    SourceCompleteness {
        source_id: src.id,
        name: src.name.clone(),
        state: src.state,
        metadata_complete,
        content_complete,
        content_pending: counts.content_pending,
        content_failed: counts.content_failed,
        listing_errors,
        last_scan_completed: src.last_scan_completed_at,
        checkpoint_age_ms,
        freshness,
        note,
    }
}
