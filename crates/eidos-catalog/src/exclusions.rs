//! Ordered content rules, immutable self-store protection, and catalog-only
//! reapplication. Each apply batch and its cursor commit together. Content
//! admission stays closed until the derived-index cleanup is acknowledged.

use crate::policy::{ContentDecision, PolicyCtx, PolicyEngine, POLICY_VERSION};
use crate::{Catalog, CatalogError, Result};
use eidos_domain::{ContentState, FileAttributes, ObjectId, ReasonCode, SourceId};
use regex::{Regex, RegexBuilder};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ts_rs::TS;

const APPLY_OBJECTS: &str = "SELECT object_id FROM objects WHERE source_id = ?1 AND object_id > ?2 AND deleted_at IS NULL AND kind IN ('file','directory') ORDER BY object_id LIMIT 128";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Directory,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ExclusionRule {
    pub id: String,
    pub kind: RuleKind,
    pub pattern: String,
    pub include: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ExclusionPolicy {
    pub revision: u32,
    pub engine_version: u32,
    pub rules: Vec<ExclusionRule>,
    pub phase: String,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub processed: u64,
    #[serde(deserialize_with = "eidos_domain::json::u64_string::deserialize")]
    pub changed: u64,
    pub error: Option<String>,
    /// Source-relative boundaries; empty string means the entire root.
    pub protected_directories: Vec<String>,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApplyExclusions {
    pub expected_revision: u32,
    pub rules: Vec<ExclusionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PreviewExclusions {
    pub rules: Vec<ExclusionRule>,
    /// At most 50 source-relative file paths. Preview never opens a file.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ExclusionPreview {
    pub path: String,
    pub state: ContentState,
    pub reason: String,
    pub rule: String,
    pub catalogued: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct CompiledRule {
    pub rule: ExclusionRule,
    regex: Option<Regex>,
}

impl CompiledRule {
    pub(crate) fn matches(&self, path: &str, case_sensitive: bool) -> bool {
        match &self.regex {
            Some(regex) => regex.is_match(path),
            None => within(path, &self.rule.pattern, case_sensitive),
        }
    }
}

fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::InvalidState(message.into())
}

/// Component-aware prefix test. The input is normalized, not a glob.
pub(crate) fn within(path: &str, root: &str, sensitive: bool) -> bool {
    if !sensitive {
        return within(&path.to_lowercase(), &root.to_lowercase(), true);
    }
    root.is_empty() || path == root || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}

fn relative_path(path: &str) -> Result<()> {
    if path.len() > 4096
        || path.starts_with('/')
        || path.contains(':')
        || path.contains('\0')
        || path
            .split('/')
            .any(|c| c == ".." || c == "." || c.is_empty())
    {
        return Err(invalid(
            "use a nonempty source-relative path with / separators; no . or .. components",
        ));
    }
    Ok(())
}

fn compile(rules: &[ExclusionRule], sensitive: bool) -> Result<Vec<CompiledRule>> {
    if rules.len() > 100 {
        return Err(invalid("at most 100 exclusion rules are allowed"));
    }
    let mut ids = std::collections::HashSet::new();
    rules
        .iter()
        .map(|rule| {
            if rule.id.is_empty()
                || rule.id.len() > 64
                || !rule
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !ids.insert(&rule.id)
            {
                return Err(invalid(
                    "rule IDs must be unique, 1–64 ASCII letters, digits, - or _",
                ));
            }
            if rule.pattern.is_empty() || rule.pattern.len() > 4096 {
                return Err(invalid(format!(
                    "rule {}: pattern must be 1–4096 bytes",
                    rule.id
                )));
            }
            let regex = match rule.kind {
                RuleKind::Directory => {
                    relative_path(&rule.pattern)?;
                    None
                }
                RuleKind::Regex => Some(
                    RegexBuilder::new(&rule.pattern)
                        .case_insensitive(!sensitive)
                        .size_limit(256 * 1024)
                        .dfa_size_limit(256 * 1024)
                        .build()
                        .map_err(|e| invalid(format!("rule {}: {e}", rule.id)))?,
                ),
            };
            Ok(CompiledRule {
                rule: rule.clone(),
                regex,
            })
        })
        .collect()
}

/// Normalize stored absolute roots, including Windows extended-length paths.
/// POSIX backslashes remain ordinary filename characters.
pub fn normalized_absolute(path: &str) -> String {
    let path = if let Some(p) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{p}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    };
    let path = if crate::paths::separator(&path) == '\\' {
        path.replace('\\', "/")
    } else {
        path
    };
    path.trim_end_matches('/').to_owned()
}

pub(crate) fn engine_conn(conn: &Connection, source: SourceId) -> Result<PolicyEngine> {
    let src = crate::read::get_source_conn(conn, source)?
        .ok_or_else(|| CatalogError::NotFound(format!("source {source}")))?;
    let sensitive = Catalog::source_is_case_sensitive(conn, source)?;
    let saved: Option<(u32, String)> = conn
        .query_row(
            "SELECT revision, rules FROM source_policy WHERE source_id = ?1",
            [source.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (revision, rules) = match saved {
        Some((revision, rules)) => (
            revision,
            serde_json::from_str::<Vec<ExclusionRule>>(&rules)?,
        ),
        None => (0, Vec::new()),
    };
    let root = normalized_absolute(&src.root_path);
    let roots = conn
        .prepare_cached("SELECT path FROM protected_paths ORDER BY path")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let protected = roots
        .iter()
        .filter_map(|p| {
            if within(&root, p, sensitive) {
                Some(String::new())
            } else if within(p, &root, sensitive) {
                // Case-insensitive matching can fold to a different byte
                // length, so drop whole components instead of slicing at
                // `root.len()`: a non-boundary slice would silently yield ""
                // and protect the entire source.
                Some(
                    p.split('/')
                        .skip(root.split('/').count())
                        .collect::<Vec<_>>()
                        .join("/"),
                )
            } else {
                None
            }
        })
        .collect();
    Ok(PolicyEngine {
        version: POLICY_VERSION,
        revision,
        rules: compile(&rules, sensitive)?,
        protected,
        absolute_protected: roots,
        case_sensitive: sensitive,
    })
}

pub(crate) fn applying_conn(conn: &Connection, source: SourceId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM source_policy WHERE source_id = ?1 AND phase != 'applied')",
        [source.0],
        |r| r.get(0),
    )?)
}

/// A path change can alter inherited decisions without changing file bytes.
/// Restart the bounded catalog pass, retaining the operator's revision/rules.
///
/// A pass that is already running is *not* rewound: repeated moves on an
/// actively reorganized source would otherwise discard progress indefinitely
/// and hold its content claims closed. One more full pass is queued instead,
/// which `acknowledge_policy_cleanup` starts once this one has committed.
pub(crate) fn reapply_conn(conn: &Connection, source: SourceId) -> Result<()> {
    conn.execute(
        "INSERT INTO source_policy (source_id, revision, rules) VALUES (?1, 0, '[]')
        ON CONFLICT(source_id) DO UPDATE SET
            restart_requested = (phase != 'applied'),
            phase = 'applying',
            cursor = CASE WHEN phase = 'applied' THEN 0 ELSE cursor END,
            processed = CASE WHEN phase = 'applied' THEN 0 ELSE processed END,
            changed = CASE WHEN phase = 'applied' THEN 0 ELSE changed END",
        [source.0],
    )?;
    Ok(())
}

pub(crate) struct CachedPolicy {
    revision: u32,
    sensitive: bool,
    root: String,
    engine: std::sync::Arc<PolicyEngine>,
}

impl PolicyEngine {
    pub fn path_decision(
        &self,
        relative: &str,
        attributes: FileAttributes,
        tag: u32,
    ) -> ContentDecision {
        let mut parts = relative.rsplitn(2, '/');
        let name = parts.next().unwrap_or(relative);
        let mut ctx = PolicyCtx::root();
        if let Some(parent) = parts.next() {
            for name in parent.split('/') {
                ctx = self.directory(name, &ctx);
            }
        }
        self.file(name, attributes, tag, &ctx)
    }

    pub fn explanation(&self, decision: ContentDecision) -> (ReasonCode, String, bool) {
        match decision {
            ContentDecision::Excluded { reason, rule } => (reason, rule.into(), false),
            ContentDecision::Operator { include, index } => (
                if include {
                    ReasonCode::UserInclude
                } else {
                    ReasonCode::UserExclude
                },
                format!("operator:{}:{}", self.revision, self.rules[index].rule.id),
                include,
            ),
            ContentDecision::Candidate => (ReasonCode::Included, "default-candidate".into(), true),
            ContentDecision::Unsupported => {
                (ReasonCode::ExtensionRule, "no-extractor".into(), false)
            }
        }
    }

    pub(crate) fn record(
        &self,
        conn: &Connection,
        id: ObjectId,
        decision: ContentDecision,
    ) -> Result<()> {
        if matches!(
            decision,
            ContentDecision::Excluded { .. } | ContentDecision::Operator { .. }
        ) {
            let (reason, rule, included) = self.explanation(decision);
            conn.execute("INSERT INTO policy_decisions (object_id, stage, included, reason, rule, policy_version)
                VALUES (?1, 'content', ?2, ?3, ?4, ?5)
                ON CONFLICT(object_id, stage) DO UPDATE SET included = excluded.included, reason = excluded.reason, rule = excluded.rule, policy_version = excluded.policy_version WHERE user_override = 0",
                params![id.0, included, reason.as_str(), rule, self.version])?;
        } else {
            conn.execute("DELETE FROM policy_decisions WHERE object_id = ?1 AND stage = 'content' AND user_override = 0", [id.0])?;
        }
        Ok(())
    }

    pub(crate) fn record_boundary(&self, conn: &Connection, id: ObjectId) -> Result<()> {
        conn.execute("INSERT INTO policy_decisions (object_id, stage, included, reason, rule, policy_version)
            VALUES (?1, 'inventory', 0, 'self_store', 'self-store', ?2)
            ON CONFLICT(object_id, stage) DO UPDATE SET included = 0, reason = 'self_store', rule = 'self-store', policy_version = excluded.policy_version", params![id.0, self.version])?;
        conn.execute("WITH RECURSIVE ancestors(id) AS (SELECT ?1 UNION SELECT e.parent_id FROM entries e JOIN ancestors a ON e.object_id = a.id WHERE e.deleted_at IS NULL AND e.parent_id IS NOT NULL)
            UPDATE directory_aggregates SET complete = 0 WHERE object_id IN (SELECT id FROM ancestors)", [id.0])?;
        Ok(())
    }
}

impl Catalog {
    pub fn path_is_protected(&self, source: SourceId, absolute: &str) -> Result<bool> {
        self.with_reader(|conn| self.path_protected_conn(conn, source, absolute))
    }

    fn path_protected_conn(
        &self,
        conn: &Connection,
        source: SourceId,
        absolute: &str,
    ) -> Result<bool> {
        let engine = self.cached_policy_conn(conn, source)?;
        let path = normalized_absolute(absolute);
        Ok(engine
            .absolute_protected
            .iter()
            .any(|root| within(&path, root, engine.case_sensitive)))
    }

    pub fn object_is_protected(&self, source: SourceId, object: ObjectId) -> Result<bool> {
        self.with_reader(|conn| match crate::read::render_path_conn(conn, object)? {
            Some(path) => self.path_protected_conn(conn, source, &path),
            None => Ok(true),
        })
    }

    pub fn decorate_policy_coverage(&self, c: &mut eidos_domain::SourceCompleteness) -> Result<()> {
        self.with_reader(|conn| self.policy_coverage_conn(conn, c))
    }

    pub(crate) fn policy_coverage_conn(
        &self,
        conn: &Connection,
        c: &mut eidos_domain::SourceCompleteness,
    ) -> Result<()> {
        if c.content_not_replicated {
            return Ok(());
        }
        let engine = self.cached_policy_conn(conn, c.source_id)?;
        let mut notes = Vec::new();
        let previous_boundary: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM policy_decisions p JOIN objects o ON o.object_id = p.object_id WHERE o.source_id = ?1 AND o.deleted_at IS NULL AND p.stage = 'inventory' AND p.reason = 'self_store')", [c.source_id.0], |r| r.get(0))?;
        if !engine.protected.is_empty() || previous_boundary {
            c.metadata_complete = false;
            c.content_complete = false;
            notes.push("Eidos storage boundaries are not enumerated; their contents and totals are unknown or last-known".to_owned());
        }
        if applying_conn(conn, c.source_id)? {
            c.content_complete = false;
            notes.push("exclusion policy application is pending; search coverage changes progressively (see source policy progress/errors)".into());
        }
        if !notes.is_empty() {
            c.policy_note = Some(notes.join("; "));
        }
        Ok(())
    }

    pub(crate) fn cached_policy_conn(
        &self,
        conn: &Connection,
        source: SourceId,
    ) -> Result<std::sync::Arc<PolicyEngine>> {
        let (root, revision): (String, u32) = conn.query_row("SELECT s.root_path, COALESCE(p.revision, 0) FROM sources s LEFT JOIN source_policy p ON p.source_id = s.source_id WHERE s.source_id = ?1", [source.0], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or_else(|| CatalogError::NotFound(format!("source {source}")))?;
        let sensitive = Self::source_is_case_sensitive(conn, source)?;
        {
            let cache = self.policy_engines.lock();
            if let Some(cached) = cache.get(&source) {
                if cached.revision == revision
                    && cached.sensitive == sensitive
                    && cached.root == root
                {
                    return Ok(cached.engine.clone());
                }
            }
        }
        let engine = std::sync::Arc::new(engine_conn(conn, source)?);
        self.policy_engines.lock().insert(
            source,
            CachedPolicy {
                revision,
                sensitive,
                root,
                engine: engine.clone(),
            },
        );
        Ok(engine)
    }

    pub fn policy_applying(&self, source: SourceId) -> Result<bool> {
        self.with_reader(|conn| applying_conn(conn, source))
    }

    pub fn protected_lister<'a>(
        &self,
        source: SourceId,
        inner: &'a dyn eidos_scanner::DirectoryLister,
    ) -> Result<ProtectedLister<'a>> {
        let root = self
            .get_source(source)?
            .ok_or_else(|| CatalogError::NotFound(format!("source {source}")))?
            .root_path;
        Ok(ProtectedLister {
            root: normalized_absolute(&root),
            engine: self.exclusion_engine(source)?,
            inner,
        })
    }
    pub fn exclusion_engine(&self, source: SourceId) -> Result<PolicyEngine> {
        self.with_reader(|conn| engine_conn(conn, source))
    }

    pub fn exclusion_policy(&self, source: SourceId) -> Result<ExclusionPolicy> {
        self.with_reader(|conn| {
            let engine = self.cached_policy_conn(conn, source)?;
            let mut view = ExclusionPolicy {
                revision: engine.revision, engine_version: engine.version,
                rules: engine.rules.iter().map(|r| r.rule.clone()).collect(), phase: "applied".into(), processed: 0, changed: 0, error: None,
                protected_directories: engine.protected.clone(), case_sensitive: engine.case_sensitive,
            };
            if let Some((phase, processed, changed, error)) = conn.query_row("SELECT phase, processed, changed, error FROM source_policy WHERE source_id = ?1", [source.0], |r| Ok((r.get(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get(3)?))).optional()? {
                view.phase = phase; view.processed = processed as u64; view.changed = changed as u64; view.error = error;
            }
            Ok(view)
        })
    }

    pub fn preview_exclusions(
        &self,
        source: SourceId,
        request: &PreviewExclusions,
    ) -> Result<Vec<ExclusionPreview>> {
        let mut engine = self.exclusion_engine(source)?;
        engine.rules = compile(&request.rules, engine.case_sensitive)?;
        engine.revision += 1;
        if request.paths.len() > 50 {
            return Err(invalid("preview accepts at most 50 file paths"));
        }
        request
            .paths
            .iter()
            .map(|path| {
                relative_path(path)?;
                let object = self
                    .resolve_relative(source, path)?
                    .map(|id| self.get_object(id))
                    .transpose()?
                    .flatten();
                let decision = engine.path_decision(
                    path,
                    object.as_ref().map(|o| o.attributes).unwrap_or_default(),
                    object.as_ref().map(|o| o.reparse_tag).unwrap_or(0),
                );
                let (reason, rule, _) = engine.explanation(decision);
                Ok(ExclusionPreview {
                    path: path.clone(),
                    state: decision.initial_state(),
                    reason: reason.as_str().into(),
                    rule,
                    catalogued: object.is_some(),
                })
            })
            .collect()
    }

    pub fn apply_exclusions(
        &self,
        source: SourceId,
        request: &ApplyExclusions,
    ) -> Result<ExclusionPolicy> {
        self.with_writer(|conn| {
            let tx = conn.transaction()?;
            let src = crate::read::get_source_conn(&tx, source)?.ok_or_else(|| CatalogError::NotFound(format!("source {source}")))?;
            if src.kind.is_remote() { return Err(invalid("edit exclusions on the source's origin node")); }
            let engine = engine_conn(&tx, source)?;
            compile(&request.rules, engine.case_sensitive)?;
            if request.expected_revision != engine.revision { return Err(invalid("policy changed; reload before applying")); }
            if applying_conn(&tx, source)? { return Err(invalid("policy application is already in progress; retry its error or wait for completion")); }
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM scan_generations WHERE source_id = ?1 AND state = 'open')", [source.0], |r| r.get::<_, bool>(0))? { return Err(invalid("a scan is open; finish or cancel it before applying exclusions")); }
            tx.execute("INSERT INTO source_policy (source_id, revision, rules) VALUES (?1, ?2, ?3)
                ON CONFLICT(source_id) DO UPDATE SET revision = excluded.revision, rules = excluded.rules, phase = 'applying', cursor = 0, processed = 0, changed = 0, restart_requested = 0, error = NULL",
                params![source.0, request.expected_revision + 1, serde_json::to_string(&request.rules)?])?;
            tx.execute("UPDATE sources SET policy_version = ?2 WHERE source_id = ?1", params![source.0, engine.version])?;
            tx.commit()?;
            Ok(())
        })?;
        self.exclusion_policy(source)
    }

    /// Called at service startup, before any source work. Canonical and lexical
    /// paths protect configured aliases without resolving every corpus file.
    pub fn configure_protected_paths(&self, paths: &[PathBuf]) -> Result<()> {
        let mut roots = Vec::new();
        for path in paths {
            let absolute = std::path::absolute(path)?;
            roots.push(normalized_absolute(&absolute.to_string_lossy()));
            if let Ok(canonical) = absolute.canonicalize() {
                roots.push(normalized_absolute(&canonical.to_string_lossy()));
            }
        }
        roots.sort();
        roots.dedup();
        self.with_writer(|conn| {
            let tx = conn.transaction()?;
            let old = tx.prepare("SELECT path FROM protected_paths ORDER BY path")?.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if old == roots { return Ok(()); }
            let sources = tx.prepare("SELECT source_id FROM sources WHERE kind != 'remote'")?.query_map([], |r| r.get::<_, i64>(0).map(SourceId))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let before = sources.iter().map(|s| Ok((*s, engine_conn(&tx, *s)?.protected))).collect::<Result<Vec<_>>>()?;
            tx.execute("DELETE FROM protected_paths", [])?;
            for root in roots { tx.execute("INSERT INTO protected_paths(path) VALUES (?1)", [root])?; }
            // Resume through the ordinary bounded apply mechanism; don't scan
            // or purge a large catalog inside the startup transaction.
            for (source, previous) in before {
                if previous == engine_conn(&tx, source)?.protected { continue; }
                tx.execute("INSERT INTO source_policy (source_id, revision, rules) VALUES (?1, 1, '[]')
                    ON CONFLICT(source_id) DO UPDATE SET revision = revision + 1, phase = 'applying', cursor = 0, processed = 0, changed = 0, restart_requested = 0, error = NULL", [source.0])?;
                tx.execute("UPDATE sources SET policy_version = ?2 WHERE source_id = ?1", params![source.0, POLICY_VERSION])?;
            }
            tx.commit()?;
            Ok(())
        })?;
        self.policy_engines.lock().clear();
        Ok(())
    }

    /// Offline CLI scans know their data directory but must retain any
    /// separately configured log protection saved by the service.
    pub fn protect_data_directory(&self, path: &std::path::Path) -> Result<()> {
        let mut paths = self.with_reader(|conn| {
            Ok(conn
                .prepare("SELECT path FROM protected_paths")?
                .query_map([], |r| r.get::<_, String>(0).map(PathBuf::from))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        paths.push(path.to_path_buf());
        self.configure_protected_paths(&paths)
    }

    pub fn pending_policy_sources(&self) -> Result<Vec<SourceId>> {
        self.with_reader(|conn| Ok(conn.prepare_cached("SELECT source_id FROM source_policy WHERE phase != 'applied' AND error IS NULL ORDER BY source_id")?.query_map([], |r| r.get::<_, i64>(0).map(SourceId))?.collect::<rusqlite::Result<_>>()?))
    }

    pub fn set_policy_error(&self, source: SourceId, error: Option<&str>) -> Result<()> {
        self.with_writer(|conn| {
            conn.execute(
                "UPDATE source_policy SET error = ?2 WHERE source_id = ?1",
                params![source.0, error],
            )?;
            Ok(())
        })
    }

    /// Processes at most 128 catalog objects, no filesystem I/O. Wait for
    /// already claimed workers to drain before invalidating their generations.
    pub fn apply_policy_batch(&self, source: SourceId) -> Result<()> {
        self.with_writer(|conn| {
            let tx = conn.transaction()?;
            let row: Option<(String, i64, Option<String>)> = tx.query_row("SELECT phase, cursor, error FROM source_policy WHERE source_id = ?1", [source.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((phase, cursor, error)) = row else { return Ok(()); };
            if phase != "applying" || error.is_some() { return Ok(()); }
            let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE source_id = ?1 AND state = 'running' AND stage = 'content_text') OR EXISTS(SELECT 1 FROM scan_generations WHERE source_id = ?1 AND state = 'open')", [source.0], |r| r.get(0))?;
            if busy { return Ok(()); }
            let engine = self.cached_policy_conn(&tx, source)?;
            let ids = tx.prepare_cached(APPLY_OBJECTS)?.query_map(params![source.0, cursor], |r| r.get::<_, i64>(0).map(ObjectId))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let root: String = tx.query_row("SELECT root_path FROM sources WHERE source_id = ?1", [source.0], |r| r.get(0))?;
            let root = normalized_absolute(&root);
            let mut changed = 0;
            for id in &ids {
                let Some(path) = crate::read::render_path_conn(&tx, *id)? else { continue; };
                let absolute = normalized_absolute(&path);
                let relative = absolute.strip_prefix(&root).unwrap_or(&absolute).trim_start_matches('/');
                let (kind, old, attributes, tag): (String, String, u32, u32) = tx.query_row("SELECT kind, content_state, attributes, reparse_tag FROM objects WHERE object_id = ?1", [id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                if kind == "directory" {
                    if engine.is_protected(relative) { engine.record_boundary(&tx, *id)?; }
                    // A removed boundary remains a coverage gap until that
                    // directory is successfully enumerated again.
                    continue;
                }
                let decision = engine.path_decision(relative, FileAttributes(attributes), tag);
                engine.record(&tx, *id, decision)?;
                let next = decision.initial_state();
                let change = decision.changes_state(&old) || (next != ContentState::Pending && tx.query_row("SELECT EXISTS(SELECT 1 FROM content_records WHERE object_id = ?1)", [id.0], |r| r.get::<_, bool>(0))?);
                if !change { continue; }
                changed += 1;
                tx.execute("UPDATE objects SET generation = generation + 1, content_id = NULL WHERE object_id = ?1", [id.0])?;
                crate::content::flip_state(&tx, *id, next, None)?;
                crate::sync::touch_conn(&tx, source, *id)?;
                let stored = tx.execute("DELETE FROM chunks WHERE object_id = ?1", [id.0])?
                    + tx.execute("DELETE FROM content_records WHERE object_id = ?1", [id.0])?;
                crate::archive::retire_virtual_tree(&tx, *id, eidos_domain::UnixNanos::now().0)?;
                tx.execute("DELETE FROM archive_members WHERE object_id = ?1", [id.0])?;
                tx.execute("DELETE FROM archive_records WHERE object_id = ?1", [id.0])?;
                let generation: i64 = tx.query_row("SELECT generation FROM objects WHERE object_id = ?1", [id.0], |r| r.get(0))?;
                crate::jobs::outbox_append_conn(&tx, source, *id, "subtree", generation)?;
                tx.execute("UPDATE jobs SET state = 'superseded' WHERE object_id = ?1 AND stage = 'content_text' AND state = 'queued'", [id.0])?;
                // Chunk rows are written to the catalog before their documents
                // reach the derived index, so an object that stored neither
                // chunks nor a content record cannot have documents there.
                // Queuing the rest would commit a durable no-op delete page for
                // every 128 objects of a never-indexed corpus.
                if stored > 0 {
                    tx.execute("INSERT OR IGNORE INTO policy_cleanup (object_id, source_id) VALUES (?1, ?2)", params![id.0, source.0])?;
                }
            }
            tx.execute("UPDATE source_policy SET cursor = ?2, processed = processed + ?3, changed = changed + ?4, phase = ?5 WHERE source_id = ?1",
                params![source.0, ids.last().map(|id| id.0).unwrap_or(cursor), ids.len() as i64, changed, if ids.len() < 128 { "purging" } else { "applying" }])?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn policy_cleanup_batch(&self, source: SourceId) -> Result<Vec<ObjectId>> {
        self.with_reader(|conn| Ok(conn.prepare_cached("SELECT object_id FROM policy_cleanup WHERE source_id = ?1 ORDER BY object_id LIMIT 128")?.query_map([source.0], |r| r.get::<_, i64>(0).map(ObjectId))?.collect::<rusqlite::Result<_>>()?))
    }

    /// Call only after the content-index deletes have committed successfully.
    pub fn acknowledge_policy_cleanup(&self, source: SourceId, objects: &[ObjectId]) -> Result<()> {
        self.with_writer(|conn| {
            let tx = conn.transaction()?;
            for object in objects { tx.execute("DELETE FROM policy_cleanup WHERE object_id = ?1 AND source_id = ?2", params![object.0, source.0])?; }
            let ready: Option<bool> = tx.query_row(
                "SELECT restart_requested != 0 FROM source_policy
                 WHERE source_id = ?1 AND phase = 'purging' AND NOT EXISTS(SELECT 1 FROM policy_cleanup WHERE source_id = ?1)",
                [source.0], |r| r.get(0)).optional()?;
            match ready {
                // A path change arrived while this pass ran, so objects it
                // already walked may hold stale decisions. Run the queued pass
                // rather than reporting a transition that never observed them.
                Some(true) => { tx.execute("UPDATE source_policy SET phase = 'applying', cursor = 0, processed = 0, changed = 0, restart_requested = 0, error = NULL WHERE source_id = ?1", [source.0])?; }
                Some(false) => {
                    tx.execute("UPDATE source_policy SET phase = 'applied', error = NULL WHERE source_id = ?1", [source.0])?;
                    crate::content::refresh_source_content_state_conn(&tx, source)?;
                }
                None => {}
            }
            tx.commit()?;
            Ok(())
        })
    }
}

/// The walker still observes the boundary directory, but never enumerates
/// its children. ScanSession records the intentional coverage gap.
pub struct ProtectedLister<'a> {
    root: String,
    engine: PolicyEngine,
    inner: &'a dyn eidos_scanner::DirectoryLister,
}

impl eidos_scanner::DirectoryLister for ProtectedLister<'_> {
    fn list(
        &self,
        dir: &std::path::Path,
    ) -> std::result::Result<Vec<eidos_scanner::RawEntry>, eidos_scanner::ScanError> {
        let path = normalized_absolute(&dir.to_string_lossy());
        if let Some(relative) = path.strip_prefix(&self.root) {
            if self.engine.is_protected(relative.trim_start_matches('/')) {
                return Ok(Vec::new());
            }
        }
        self.inner.list(dir)
    }
    fn stat(
        &self,
        path: &std::path::Path,
    ) -> std::result::Result<eidos_scanner::RawEntry, eidos_scanner::ScanError> {
        self.inner.stat(path)
    }
    fn volume_info(
        &self,
        path: &std::path::Path,
    ) -> std::result::Result<eidos_scanner::VolumeInfo, eidos_scanner::ScanError> {
        self.inner.volume_info(path)
    }
    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_pages_seek_the_source_index_without_a_temporary_sort() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn).unwrap();
        let plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {APPLY_OBJECTS}"))
            .unwrap()
            .query_map([1, 900_000], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("; ");
        assert!(plan.contains("objects_policy_apply"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }

    struct NoIo;
    impl eidos_scanner::DirectoryLister for NoIo {
        fn list(
            &self,
            _: &std::path::Path,
        ) -> std::result::Result<Vec<eidos_scanner::RawEntry>, eidos_scanner::ScanError> {
            panic!("protected directory was enumerated")
        }
        fn stat(
            &self,
            _: &std::path::Path,
        ) -> std::result::Result<eidos_scanner::RawEntry, eidos_scanner::ScanError> {
            panic!("unexpected stat")
        }
        fn volume_info(
            &self,
            _: &std::path::Path,
        ) -> std::result::Result<eidos_scanner::VolumeInfo, eidos_scanner::ScanError> {
            panic!("unexpected volume probe")
        }
        fn name(&self) -> &'static str {
            "no-io"
        }
    }

    #[test]
    fn protected_walker_boundary_never_calls_the_underlying_lister() {
        use eidos_scanner::DirectoryLister;
        let mut engine = PolicyEngine::new();
        engine.protected = vec!["data".into()];
        let lister = ProtectedLister {
            root: "/fixture".into(),
            engine,
            inner: &NoIo,
        };
        assert!(lister
            .list(std::path::Path::new("/fixture/data"))
            .unwrap()
            .is_empty());
        assert!(lister
            .list(std::path::Path::new("/fixture/data/index"))
            .unwrap()
            .is_empty());
    }
    fn rule(id: &str, kind: RuleKind, pattern: &str, include: bool) -> ExclusionRule {
        ExclusionRule {
            id: id.into(),
            kind,
            pattern: pattern.into(),
            include,
        }
    }

    #[test]
    fn validation_bounds_work_and_precedence_is_component_and_case_aware() {
        let rules = vec![
            rule("exclude", RuleKind::Directory, "work", false),
            rule("include", RuleKind::Regex, r"^work/keep\.txt$", true),
        ];
        let mut engine = PolicyEngine::new();
        engine.rules = compile(&rules, false).unwrap();
        assert_eq!(
            engine
                .path_decision("work/drop.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(
            engine
                .path_decision("WORK/KEEP.TXT", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Pending
        );
        assert_eq!(
            engine
                .path_decision("work2/drop.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Pending
        );
        // `file` builds the relative path lazily; nested paths must still be
        // matched by folder rules, and the default engine must still classify
        // a deep path by extension without one.
        assert_eq!(
            engine
                .path_decision("work/a/b/deep.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(
            PolicyEngine::new()
                .path_decision("work/a/b/deep.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Pending
        );
        engine.rules.reverse();
        assert_eq!(
            engine
                .path_decision("work/keep.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Excluded
        );
        engine.case_sensitive = true;
        engine.rules = compile(&rules, true).unwrap();
        assert_eq!(
            engine
                .path_decision("Work/drop.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Pending
        );
        for pattern in [
            "../secret",
            "/absolute",
            "a/../b",
            "a//b",
            "C:/data",
            ".",
            "",
        ] {
            assert!(compile(&[rule("bad", RuleKind::Directory, pattern, false)], true).is_err());
        }
        assert!(compile(&[rules[0].clone(), rules[0].clone()], true).is_err());
        assert!(compile(&[rule("bad", RuleKind::Regex, "(?=x)", false)], true).is_err());
        assert!(compile(
            &[rule("bad", RuleKind::Regex, &"a".repeat(4097), false)],
            true
        )
        .is_err());
        assert!(compile(&vec![rules[0].clone(); 101], true).is_err());
    }

    #[test]
    fn include_overrides_defaults_but_never_read_safety_or_self_storage() {
        let mut engine = PolicyEngine::new();
        engine.rules = compile(&[rule("all", RuleKind::Regex, ".*", true)], false).unwrap();
        engine.protected = vec!["data".into()];
        assert_eq!(
            engine
                .path_decision("node_modules/code.js", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Pending
        );
        assert_eq!(
            engine
                .path_decision("data/log.txt", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(
            engine
                .path_decision("plain.txt", FileAttributes(FileAttributes::OFFLINE), 0)
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(
            engine
                .path_decision(
                    "link.txt",
                    FileAttributes(0),
                    crate::policy::reparse::SYMLINK
                )
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(
            engine
                .path_decision(
                    "cloud.txt",
                    FileAttributes(0),
                    crate::policy::reparse::CLOUD
                )
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(
            engine
                .path_decision("pagefile.sys", FileAttributes(0), 0)
                .initial_state(),
            ContentState::Excluded
        );
        assert_eq!(normalized_absolute(r"\\?\C:\Data\"), "C:/Data");
        assert_eq!(
            normalized_absolute(r"\\?\UNC\fileserver\share\Data"),
            "//fileserver/share/Data"
        );
        assert!(!within("C:/Database/a", "C:/Data", false));
        assert!(within("c:/DATA/a", "C:/Data", false));
    }
}
