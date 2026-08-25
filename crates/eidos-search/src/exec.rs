//! Query execution: typed AST → Tantivy query → verified, joined results.
//!
//! Execution follows ARCHITECTURE 10: validate, resolve scope, retrieve
//! candidates, verify exact/case-sensitive clauses against stored originals,
//! join current state and completeness from the catalog, and explain.

use crate::content::{
    self, object_score, verify_objects, verify_threads, ContentClause, ContentIndex, ContentOpts,
    Matcher, VerifyJob,
};
use crate::facets::RangeBucket;
use crate::regex_plan::TrigramPlan;
use crate::schema::{attr_bit, attr_name, canonical_path, fold, Fields};
use crate::{CatalogIndex, Result, SearchError, PROJECTION_NAME};
use eidos_catalog::content::ChunkRow;
use eidos_catalog::Catalog;
use eidos_domain::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::time::Instant;
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query as TQuery, RangeQuery,
    RegexQuery, TermQuery, TermSetQuery,
};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocAddress, Order, Searcher, TantivyDocument, Term};

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub limits: QueryLimits,
    /// Candidate over-fetch factor when verification is required.
    pub oversample: usize,
    /// Hard cap on candidates examined in collect-all mode.
    pub max_candidates: usize,
    pub content: ContentOpts,
    /// Snippets returned per file hit.
    pub max_snippets: usize,
    /// Snippet window in characters.
    pub snippet_chars: usize,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            limits: QueryLimits::default(),
            oversample: 8,
            max_candidates: 100_000,
            content: ContentOpts::default(),
            max_snippets: 3,
            snippet_chars: 240,
        }
    }
}

/// Per-object result of one content clause.
#[derive(Debug, Clone)]
pub struct ObjectMatch {
    pub score: f32,
    pub generation: u32,
    /// `(chunk ordinal, score)`
    pub chunks: Vec<(u32, f32)>,
    /// Chunk rows already fetched while verifying (page-driven path), so
    /// snippets need not fetch them again.
    pub rows: Vec<ChunkRow>,
}

/// Everything one content clause retrieved, keyed by object.
pub struct ContentSet {
    /// Verified matches. For a deferred clause this fills up as the page
    /// walk verifies objects.
    pub by_object: HashMap<ObjectId, ObjectMatch>,
    pub matcher: Matcher,
    pub truncated: bool,
    /// Unverified candidate chunk ordinals per object (newest generation),
    /// when verification was deferred to the page walk.
    pub deferred: Option<HashMap<ObjectId, (u32, Vec<u32>)>>,
}

impl ContentSet {
    fn is_deferred(&self) -> bool {
        self.deferred.is_some()
    }
    /// Upper bound on this clause's contribution to an object's score.
    fn bound(&self, object: ObjectId) -> f32 {
        if let Some(m) = self.by_object.get(&object) {
            return m.score;
        }
        match &self.deferred {
            Some(c) => c
                .get(&object)
                .map_or(0.0, |(_, ords)| object_score(ords.len())),
            None => 0.0,
        }
    }
}

/// Stored fields of a hit candidate.
#[derive(Debug, Clone)]
pub struct Stored {
    pub entry_id: i64,
    pub object_id: ObjectId,
    pub source_id: SourceId,
    pub parent_id: Option<ObjectId>,
    pub ancestors: Vec<ObjectId>,
    pub name: String,
    pub path: String,
    pub kind: ObjectKind,
    pub extension: String,
    pub size: u64,
    pub allocated: u64,
    pub modified: i64,
    pub created: i64,
    pub subtree_logical: u64,
    pub subtree_allocated: u64,
    pub newest_modified: i64,
    pub file_count: u64,
    pub dir_count: u64,
    pub score: f32,
}

type Verifier = Box<dyn Fn(&Stored, &Catalog) -> bool + Send + Sync>;
/// Verifier over the folded name and folded path fast fields; runs before
/// any stored document is fetched.
type FastVerifier = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// Sort keys and identity of a candidate read from fast fields. Folded
/// strings are dictionary-encoded and cost microseconds to resolve, so they
/// are read lazily (`resolve`).
struct FastRow {
    addr: DocAddress,
    entry_id: i64,
    object_id: ObjectId,
    name_ord: u64,
    path_ord: u64,
    name_folded: Option<String>,
    path_folded: Option<String>,
    size: u64,
    alloc: u64,
    modified: i64,
    created: i64,
    score: f32,
}

struct FastCols {
    name: tantivy::columnar::StrColumn,
    path: tantivy::columnar::StrColumn,
    entry: tantivy::columnar::Column<u64>,
    object: tantivy::columnar::Column<u64>,
    size: tantivy::columnar::Column<u64>,
    alloc: tantivy::columnar::Column<u64>,
    modified: tantivy::columnar::Column<i64>,
    created: tantivy::columnar::Column<i64>,
}

impl FastCols {
    fn open(searcher: &Searcher, segment: u32) -> Result<Self> {
        let ff = searcher.segment_reader(segment).fast_fields();
        let str_col = |name: &str| -> Result<tantivy::columnar::StrColumn> {
            ff.str(name)?
                .ok_or_else(|| SearchError::Other(format!("fast field {name} missing")))
        };
        Ok(Self {
            name: str_col("name_folded")?,
            path: str_col("path_folded")?,
            entry: ff.u64("entry_id")?,
            object: ff.u64("object_id")?,
            size: ff.u64("subtree_logical")?,
            alloc: ff.u64("subtree_allocated")?,
            modified: ff.i64("newest_modified")?,
            created: ff.i64("created")?,
        })
    }
    /// Numeric keys and term ordinals only (cheap column reads).
    fn row(&self, addr: DocAddress) -> FastRow {
        let d = addr.doc_id;
        FastRow {
            addr,
            entry_id: self.entry.first(d).unwrap_or(0) as i64,
            object_id: ObjectId(self.object.first(d).unwrap_or(0) as i64),
            name_ord: self.name.term_ords(d).next().unwrap_or(0),
            path_ord: self.path.term_ords(d).next().unwrap_or(0),
            name_folded: None,
            path_folded: None,
            size: self.size.first(d).unwrap_or(0),
            alloc: self.alloc.first(d).unwrap_or(0),
            modified: self.modified.first(d).unwrap_or(0),
            created: self.created.first(d).unwrap_or(0),
            score: 1.0,
        }
    }
    /// Resolve the folded name and path of a row (idempotent).
    fn resolve(&self, r: &mut FastRow) {
        if r.name_folded.is_none() {
            let mut s = String::new();
            let _ = self.name.ord_to_str(r.name_ord, &mut s);
            r.name_folded = Some(s);
        }
        if r.path_folded.is_none() {
            let mut s = String::new();
            let _ = self.path.ord_to_str(r.path_ord, &mut s);
            r.path_folded = Some(s);
        }
    }
    fn passes(&self, r: &mut FastRow, verifiers: &[FastVerifier]) -> bool {
        if verifiers.is_empty() {
            return true;
        }
        self.resolve(r);
        let (n, p) = (
            r.name_folded.as_deref().unwrap_or(""),
            r.path_folded.as_deref().unwrap_or(""),
        );
        verifiers.iter().all(|v| v(n, p))
    }
}

/// Candidate sets larger than this are verified lazily in sort order, and
/// the total becomes an upper bound (`exact = false`) unless the walk
/// finishes.
pub const LAZY_VERIFY_MIN: usize = 2_000;

fn fast_sort_cmp(a: &FastRow, b: &FastRow, sort: Sort) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ord = match sort.field {
        SortField::Relevance => b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal),
        SortField::Name => a.name_folded.cmp(&b.name_folded),
        SortField::Path => a.path_folded.cmp(&b.path_folded),
        // (resolved before sorting; `None` sorts first)
        SortField::Size | SortField::SubtreeSize => a.size.cmp(&b.size),
        SortField::AllocatedSize => a.alloc.cmp(&b.alloc),
        SortField::Modified => a.modified.cmp(&b.modified),
        SortField::Created => a.created.cmp(&b.created),
    };
    let ord = if sort.field == SortField::Relevance || !sort.descending {
        ord
    } else {
        ord.reverse()
    };
    ord.then_with(|| a.entry_id.cmp(&b.entry_id))
}

/// Rows in folded name/path order across segments: per-segment term
/// ordinals give the order within a segment; segments are merged by
/// comparing resolved heads, so only rows that are actually consumed get
/// their strings resolved.
struct MergeByString<'a> {
    streams: Vec<std::vec::IntoIter<FastRow>>,
    heads: Vec<Option<FastRow>>,
    cols: &'a HashMap<u32, FastCols>,
    by_name: bool,
    desc: bool,
}

impl<'a> MergeByString<'a> {
    fn new(
        rows: Vec<FastRow>,
        cols: &'a HashMap<u32, FastCols>,
        by_name: bool,
        desc: bool,
    ) -> Self {
        let mut per_seg: HashMap<u32, Vec<FastRow>> = HashMap::new();
        for r in rows {
            per_seg.entry(r.addr.segment_ord).or_default().push(r);
        }
        let mut streams: Vec<std::vec::IntoIter<FastRow>> = per_seg
            .into_values()
            .map(|mut v| {
                v.sort_by(|a, b| {
                    let k = if by_name {
                        a.name_ord.cmp(&b.name_ord)
                    } else {
                        a.path_ord.cmp(&b.path_ord)
                    };
                    let k = if desc { k.reverse() } else { k };
                    k.then(a.entry_id.cmp(&b.entry_id))
                });
                v.into_iter()
            })
            .collect();
        let heads = streams
            .iter_mut()
            .map(|it| it.next().map(|r| Self::resolved(cols, r)))
            .collect();
        Self {
            streams,
            heads,
            cols,
            by_name,
            desc,
        }
    }
    fn resolved(cols: &HashMap<u32, FastCols>, mut r: FastRow) -> FastRow {
        cols.get(&r.addr.segment_ord)
            .expect("opened")
            .resolve(&mut r);
        r
    }
}

impl Iterator for MergeByString<'_> {
    type Item = FastRow;
    fn next(&mut self) -> Option<FastRow> {
        let mut best: Option<usize> = None;
        for (i, h) in self.heads.iter().enumerate() {
            if let Some(r) = h {
                let better = match best {
                    None => true,
                    Some(j) => {
                        let o = self.heads[j].as_ref().expect("head");
                        let k = if self.by_name {
                            r.name_folded.cmp(&o.name_folded)
                        } else {
                            r.path_folded.cmp(&o.path_folded)
                        };
                        let k = if self.desc { k.reverse() } else { k };
                        k.then(r.entry_id.cmp(&o.entry_id)) == std::cmp::Ordering::Less
                    }
                };
                if better {
                    best = Some(i);
                }
            }
        }
        let i = best?;
        let r = self.heads[i].take().expect("head");
        self.heads[i] = self.streams[i].next().map(|n| Self::resolved(self.cols, n));
        Some(r)
    }
}

/// `AND` of the folded trigrams of every literal (None when no literal has
/// three characters).
fn trigram_query(field: tantivy::schema::Field, literals: &[String]) -> Option<Box<dyn TQuery>> {
    TrigramPlan::all_literals(literals.iter().map(String::as_str)).query(&|t| term_text(field, t))
}

/// Literal runs of a glob (between `*` / `?`).
fn glob_literals(glob: &str) -> Vec<String> {
    glob.split(['*', '?'])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

struct Ctx<'a> {
    f: &'a Fields,
    searcher: &'a Searcher,
    catalog: &'a Catalog,
    verifiers: Vec<Verifier>,
    steps: Vec<PlanStep>,
    readable: Vec<String>,
    warnings: Vec<String>,
    scope_sources: Option<Vec<SourceId>>,
    tight_dirs: bool,
    fast_verifiers: Vec<FastVerifier>,
    /// Greater than zero while compiling inside `OR` / `NOT`: verifiers
    /// (which filter the whole candidate set) would be wrong there, so
    /// clauses must compile to exact index queries or fail explicitly.
    neg_depth: u32,
    content: Option<&'a ContentIndex>,
    content_opts: ContentOpts,
    content_sets: Vec<ContentSet>,
    /// Source scope known before compilation (top-level `source:` clauses).
    content_scope: Option<Vec<SourceId>>,
    retired: Vec<SourceId>,
}

fn term_u64(f: tantivy::schema::Field, v: u64) -> Box<dyn TQuery> {
    Box::new(TermQuery::new(
        Term::from_field_u64(f, v),
        IndexRecordOption::Basic,
    ))
}
fn term_text(f: tantivy::schema::Field, v: &str) -> Box<dyn TQuery> {
    Box::new(TermQuery::new(
        Term::from_field_text(f, v),
        IndexRecordOption::Basic,
    ))
}
fn any_of(qs: Vec<Box<dyn TQuery>>) -> Box<dyn TQuery> {
    match qs.len() {
        0 => Box::new(EmptyQuery),
        1 => qs.into_iter().next().expect("one"),
        _ => Box::new(BooleanQuery::new(
            qs.into_iter().map(|q| (Occur::Should, q)).collect(),
        )),
    }
}
fn all_of(qs: Vec<Box<dyn TQuery>>) -> Box<dyn TQuery> {
    match qs.len() {
        0 => Box::new(AllQuery),
        1 => qs.into_iter().next().expect("one"),
        _ => Box::new(BooleanQuery::new(
            qs.into_iter().map(|q| (Occur::Must, q)).collect(),
        )),
    }
}
fn not(q: Box<dyn TQuery>) -> Box<dyn TQuery> {
    Box::new(BooleanQuery::new(vec![
        (Occur::Must, Box::new(AllQuery) as Box<dyn TQuery>),
        (Occur::MustNot, q),
    ]))
}

pub fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Convert a glob (`*`, `?`, `**`) into an anchored regex.
pub fn glob_to_regex(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len() * 2);
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                out.push_str(".*");
                while i + 1 < chars.len() && chars[i + 1] == '*' {
                    i += 1;
                }
            }
            '?' => out.push('.'),
            _ => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

/// Build the FST regex for a user pattern: unanchored unless the user
/// anchored it; case-insensitive via `(?i)` when requested.
fn fst_regex(pattern: &str, case_sensitive: bool) -> std::result::Result<String, QueryError> {
    // Validate with the regex crate first (bounded size).
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| QueryError::InvalidRegex {
            message: e.to_string(),
        })?;
    let mut p = pattern.to_string();
    let anchored_start = p.starts_with('^');
    let anchored_end = p.ends_with('$') && !p.ends_with("\\$");
    if anchored_start {
        p.remove(0);
    }
    if anchored_end {
        p.pop();
    }
    let mut out = String::new();
    if !case_sensitive {
        out.push_str("(?i)");
    }
    if !anchored_start {
        out.push_str(".*");
    }
    out.push_str("(?:");
    out.push_str(&p);
    out.push(')');
    if !anchored_end {
        out.push_str(".*");
    }
    Ok(out)
}

fn make_regex(
    field: tantivy::schema::Field,
    fst: &str,
) -> std::result::Result<Box<dyn TQuery>, QueryError> {
    RegexQuery::from_pattern(fst, field)
        .map(|q| Box::new(q) as Box<dyn TQuery>)
        .map_err(|e| QueryError::RegexTooBroad {
            message: e.to_string(),
        })
}

fn u64_range(field: tantivy::schema::Field, min: Option<u64>, max: Option<u64>) -> Box<dyn TQuery> {
    let lo = min.map_or(Bound::Unbounded, |v| {
        Bound::Included(Term::from_field_u64(field, v))
    });
    let hi = max.map_or(Bound::Unbounded, |v| {
        Bound::Included(Term::from_field_u64(field, v))
    });
    Box::new(RangeQuery::new(lo, hi))
}
fn i64_range(
    field: tantivy::schema::Field,
    after: Option<i64>,
    before: Option<i64>,
) -> Box<dyn TQuery> {
    let lo = after.map_or(Bound::Unbounded, |v| {
        Bound::Included(Term::from_field_i64(field, v))
    });
    let hi = before.map_or(Bound::Unbounded, |v| {
        Bound::Excluded(Term::from_field_i64(field, v))
    });
    Box::new(RangeQuery::new(lo, hi))
}

fn compile(q: &Query, ctx: &mut Ctx<'_>) -> std::result::Result<Box<dyn TQuery>, QueryError> {
    let f = ctx.f;
    Ok(match q {
        Query::All => Box::new(AllQuery),
        Query::And { clauses } => {
            let mut qs = Vec::new();
            for c in clauses {
                qs.push(compile(c, ctx)?);
            }
            all_of(qs)
        }
        Query::Or { clauses } => {
            ctx.neg_depth += 1;
            let mut qs = Vec::new();
            for c in clauses {
                qs.push(compile(c, ctx)?);
            }
            ctx.neg_depth -= 1;
            any_of(qs)
        }
        Query::Not { clause } => {
            ctx.neg_depth += 1;
            let q = compile(clause, ctx)?;
            ctx.neg_depth -= 1;
            not(q)
        }
        Query::Text {
            field,
            mode,
            value,
            case_sensitive,
            slop,
        } => compile_text(*field, *mode, value, *case_sensitive, *slop, ctx)?,
        Query::Host { ids } => {
            if ids.iter().any(|h| h.0 == 1) {
                Box::new(AllQuery)
            } else {
                ctx.warnings.push("no matching host; v0.5 has a single local host".into());
                Box::new(EmptyQuery)
            }
        }
        Query::Source { ids, names } => {
            let mut all: Vec<SourceId> = ids.clone();
            for n in names {
                match ctx.catalog.find_source_by_name(n).map_err(|e| QueryError::Other { message: e.to_string() })? {
                    Some(s) => all.push(s.id),
                    None => {
                        return Err(QueryError::Other {
                            message: format!("unknown source name: {n}"),
                        })
                    }
                }
            }
            ctx.readable.push(format!(
                "source in [{}]",
                all.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")
            ));
            ctx.scope_sources = Some(match ctx.scope_sources.take() {
                Some(existing) => existing.into_iter().filter(|s| all.contains(s)).collect(),
                None => all.clone(),
            });
            any_of(all.iter().map(|s| term_u64(f.source_id, s.0 as u64)).collect())
        }
        Query::Object { ids } => any_of(ids.iter().map(|o| term_u64(f.object_id, o.0 as u64)).collect()),
        Query::Path {
            mode,
            value,
            case_sensitive,
        } => compile_path(*mode, value, *case_sensitive, ctx)?,
        Query::DescendantOf { directory, max_depth } => {
            ctx.readable.push(format!("under directory {directory}"));
            let base = term_u64(f.ancestors, directory.0 as u64);
            if let Some(d) = max_depth {
                needs_positive(ctx, "a depth-limited `in:` clause")?;
                let dir = *directory;
                let d = *d as usize;
                ctx.verifiers.push(Box::new(move |s, _| {
                    s.ancestors.iter().rposition(|a| *a == dir).is_some_and(|pos| s.ancestors.len() - pos <= d)
                }));
            }
            base
        }
        Query::Extension { values } => {
            ctx.readable.push(format!("extension in [{}]", values.join(", ")));
            any_of(
                values
                    .iter()
                    .map(|v| term_text(f.extension, &fold(v.trim_start_matches('.'))))
                    .collect(),
            )
        }
        Query::Kind { values } => any_of(values.iter().map(|k| term_text(f.kind, k.as_str())).collect()),
        Query::Size { field, min, max } => {
            ctx.readable.push(format!("{field:?} size in [{min:?}, {max:?}]"));
            let fld = match field {
                SizeField::Logical => f.size,
                SizeField::Allocated => f.allocated,
            };
            u64_range(fld, *min, *max)
        }
        Query::Time { field, after, before } => {
            let fld = match field {
                TimeField::Modified => f.modified,
                TimeField::Created => f.created,
                TimeField::SubtreeModified => f.newest_modified,
                TimeField::Changed | TimeField::Accessed => {
                    return Err(QueryError::Other {
                        message: format!("{field:?} time is not indexed"),
                    })
                }
            };
            ctx.readable.push(format!(
                "{field:?} between {} and {}",
                after.map_or("-∞".to_string(), |t| t.to_rfc3339()),
                before.map_or("∞".to_string(), |t| t.to_rfc3339())
            ));
            let range = i64_range(fld, after.map(|t| t.0), before.map(|t| t.0));
            if *field == TimeField::SubtreeModified {
                // A directory predicate, like `subtree:` and `files:`.
                all_of(vec![term_text(f.kind, "directory"), range])
            } else {
                range
            }
        }
        Query::Attributes { all_of: a, none_of: n } => {
            let mut clauses: Vec<(Occur, Box<dyn TQuery>)> = vec![(Occur::Must, Box::new(AllQuery))];
            for bit in 0..32u32 {
                let mask = 1u32 << bit;
                if a & mask != 0 {
                    if let Some(name) = attr_name(mask) {
                        clauses.push((Occur::Must, term_text(f.attrs, name)));
                    }
                }
                if n & mask != 0 {
                    if let Some(name) = attr_name(mask) {
                        clauses.push((Occur::MustNot, term_text(f.attrs, name)));
                    }
                }
            }
            Box::new(BooleanQuery::new(clauses))
        }
        Query::ContentState { states } => {
            any_of(states.iter().map(|s| term_text(f.content_state, s.as_str())).collect())
        }
        Query::DescendantExtension {
            extension,
            min_count,
            max_count,
        } => {
            let ext = fold(extension.trim_start_matches('.'));
            ctx.readable.push(format!("directory containing ≥{min_count} .{ext} files anywhere beneath"));
            ctx.tight_dirs = true;
            let base = all_of(vec![term_text(f.kind, "directory"), term_text(f.desc_ext, &ext)]);
            if *min_count > 1 || max_count.is_some() {
                needs_positive(ctx, "a `has:` clause with a count")?;
                let (min, max, ext) = (*min_count, *max_count, ext.clone());
                ctx.verifiers.push(Box::new(move |s, catalog| {
                    let n = catalog
                        .extension_counts(s.object_id, 10_000)
                        .ok()
                        .and_then(|v| v.into_iter().find(|e| e.extension == ext).map(|e| e.count))
                        .unwrap_or(0);
                    n >= min && max.is_none_or(|m| n <= m)
                }));
            }
            base
        }
        Query::SubtreeSize { field, min, max } => {
            let fld = match field {
                SizeField::Logical => f.subtree_logical,
                SizeField::Allocated => f.subtree_allocated,
            };
            all_of(vec![term_text(f.kind, "directory"), u64_range(fld, *min, *max)])
        }
        Query::DescendantCount { min, max } => {
            all_of(vec![term_text(f.kind, "directory"), u64_range(f.file_count, *min, *max)])
        }
        Query::Archive { .. } => {
            return Err(QueryError::Other {
                message: "archive clauses are not available until the ZIP manifest index exists (Milestone 5)".into(),
            })
        }
    })
}

fn compile_text(
    field: TextField,
    mode: TextMode,
    value: &str,
    case_sensitive: bool,
    slop: u32,
    ctx: &mut Ctx<'_>,
) -> std::result::Result<Box<dyn TQuery>, QueryError> {
    let f = ctx.f;
    if field == TextField::Content {
        return compile_content(mode, value, case_sensitive, slop, ctx);
    }
    let (tok_field, folded_field, tri_field, label) = match field {
        TextField::Name => (f.name, f.name_folded, f.name_tri, "name"),
        TextField::Path => (f.path_tokens, f.path_folded, f.path_tri, "path"),
        TextField::Content => unreachable!(),
    };
    let stored_of = move |s: &Stored| -> String {
        match field {
            TextField::Name => s.name.clone(),
            _ => s.path.clone(),
        }
    };
    Ok(match mode {
        TextMode::Ranked => {
            let toks = tokens(value);
            ctx.readable
                .push(format!("{label} has terms [{}]", toks.join(", ")));
            let mut qs: Vec<Box<dyn TQuery>> = Vec::new();
            for t in toks {
                if field == TextField::Name {
                    qs.push(any_of(vec![
                        term_text(f.name, &t),
                        term_text(f.path_tokens, &t),
                    ]));
                } else {
                    qs.push(term_text(tok_field, &t));
                }
            }
            all_of(qs)
        }
        TextMode::Phrase | TextMode::Proximity => {
            let toks = tokens(value);
            ctx.readable
                .push(format!("{label} phrase \"{}\"", toks.join(" ")));
            if toks.len() < 2 {
                return Ok(any_of(
                    toks.iter().map(|t| term_text(tok_field, t)).collect(),
                ));
            }
            let terms: Vec<Term> = toks
                .iter()
                .map(|t| Term::from_field_text(tok_field, t))
                .collect();
            let mut pq = PhraseQuery::new(terms);
            if mode == TextMode::Proximity {
                pq.set_slop(slop);
            }
            Box::new(pq)
        }
        TextMode::Exact => {
            ctx.readable.push(format!(
                "{label} is exactly \"{value}\"{}",
                if case_sensitive {
                    " (case-sensitive)"
                } else {
                    ""
                }
            ));
            if case_sensitive {
                needs_positive(ctx, "a case-sensitive clause")?;
                let v = value.to_string();
                ctx.verifiers.push(Box::new(move |s, _| stored_of(s) == v));
            }
            term_text(folded_field, &fold(value))
        }
        TextMode::Substring => {
            ctx.readable.push(format!(
                "{label} contains \"{value}\"{}",
                if case_sensitive {
                    " (case-sensitive)"
                } else {
                    ""
                }
            ));
            if case_sensitive {
                needs_positive(ctx, "a case-sensitive clause")?;
                let v = value.to_string();
                ctx.verifiers
                    .push(Box::new(move |s, _| stored_of(s).contains(&v)));
            }
            let needle = fold(value);
            let candidates = if ctx.neg_depth == 0 {
                trigram_query(tri_field, std::slice::from_ref(&needle))
            } else {
                None
            };
            match candidates {
                Some(q) => {
                    let is_name = field == TextField::Name;
                    let n = needle.clone();
                    ctx.fast_verifiers.push(Box::new(move |name, path| {
                        if is_name {
                            name.contains(&n)
                        } else {
                            path.contains(&n)
                        }
                    }));
                    ctx.steps.push(PlanStep {
                        stage: "candidates".into(),
                        description: format!(
                            "folded trigrams of \"{needle}\" on {label}; candidates verified on the folded {label} fast field"
                        ),
                        candidates: None,
                        verified: None,
                        elapsed_ms: None,
                    });
                    q
                }
                None => {
                    let pat = format!(".*{}.*", regex::escape(&needle));
                    ctx.steps.push(PlanStep {
                        stage: "candidates".into(),
                        description: format!(
                            "substring automaton over the {label} term dictionary"
                        ),
                        candidates: None,
                        verified: None,
                        elapsed_ms: None,
                    });
                    make_regex(folded_field, &pat)?
                }
            }
        }
        TextMode::Regex => {
            ctx.readable.push(format!(
                "{label} matches /{value}/{}",
                if case_sensitive {
                    " (case-sensitive)"
                } else {
                    " (case-insensitive)"
                }
            ));
            // Folded text is always checked case-insensitively; case
            // sensitivity is enforced against the stored original.
            let re_folded = regex::RegexBuilder::new(value)
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| QueryError::InvalidRegex {
                    message: e.to_string(),
                })?;
            if case_sensitive {
                needs_positive(ctx, "a case-sensitive regex")?;
                let re = regex::RegexBuilder::new(value)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| QueryError::InvalidRegex {
                        message: e.to_string(),
                    })?;
                ctx.verifiers
                    .push(Box::new(move |s, _| re.is_match(&stored_of(s))));
            }
            let is_name = field == TextField::Name;
            let plan = TrigramPlan::for_regex_with_case(value, case_sensitive);
            let positive = ctx.neg_depth == 0;
            let candidates = if positive {
                plan.query(&|t| term_text(tri_field, t))
            } else {
                None
            };
            match candidates {
                Some(q) => {
                    ctx.fast_verifiers.push(Box::new(move |name, path| {
                        re_folded.is_match(if is_name { name } else { path })
                    }));
                    ctx.steps.push(PlanStep {
                        stage: "candidates".into(),
                        description: format!(
                            "trigram plan {plan} on {label}; candidates verified on the folded {label} fast field"
                        ),
                        candidates: None,
                        verified: None,
                        elapsed_ms: None,
                    });
                    q
                }
                None => {
                    if positive {
                        ctx.warnings.push(format!(
                            "regex /{value}/ has no required literal of 3+ characters; it scans the whole {label} dictionary"
                        ));
                    }
                    match fst_regex(value, false).and_then(|fst| make_regex(folded_field, &fst)) {
                        Ok(q) => {
                            ctx.steps.push(PlanStep {
                                stage: "candidates".into(),
                                description: format!(
                                    "regex automaton over the {label} term dictionary"
                                ),
                                candidates: None,
                                verified: None,
                                elapsed_ms: None,
                            });
                            q
                        }
                        Err(_) => {
                            // Too complex for the dictionary automaton: walk
                            // the folded dictionary with the regex crate and
                            // match the resulting terms exactly.
                            let t = Instant::now();
                            let (terms, truncated) = dictionary_scan(
                                ctx.searcher,
                                folded_field,
                                &re_folded,
                                DICTIONARY_SCAN_TERMS,
                            )?;
                            ctx.warnings.push(format!(
                                "regex /{value}/ is too complex for the dictionary automaton; the whole {label} dictionary was walked"
                            ));
                            if truncated {
                                ctx.warnings.push(format!(
                                    "regex /{value}/ matched more than {DICTIONARY_SCAN_TERMS} distinct {label}s; results are a subset"
                                ));
                            }
                            ctx.steps.push(PlanStep {
                                stage: "candidates".into(),
                                description: format!(
                                    "{} distinct {label}s matched by walking the folded dictionary",
                                    terms.len()
                                ),
                                candidates: Some(terms.len() as u64),
                                verified: None,
                                elapsed_ms: Some(t.elapsed().as_secs_f64() * 1000.0),
                            });
                            if terms.is_empty() {
                                Box::new(EmptyQuery)
                            } else {
                                Box::new(TermSetQuery::new(terms))
                            }
                        }
                    }
                }
            }
        }
    })
}

/// A content clause runs against the content index eagerly and compiles to
/// a set of object ids over the catalog index, so it composes with every
/// other clause (including `OR` and `NOT`).
fn compile_content(
    mode: TextMode,
    value: &str,
    case_sensitive: bool,
    slop: u32,
    ctx: &mut Ctx<'_>,
) -> std::result::Result<Box<dyn TQuery>, QueryError> {
    let index = ctx.content.ok_or_else(|| QueryError::Other {
        message: "content search is not available: the service has no content index open".into(),
    })?;
    let clause = ContentClause {
        mode,
        value: value.to_string(),
        case_sensitive,
        slop,
    };
    let desc = match mode {
        TextMode::Ranked => format!(
            "content has terms [{}]",
            content::text_tokens(value).join(", ")
        ),
        TextMode::Phrase => format!("content phrase \"{value}\""),
        TextMode::Proximity => format!(
            "content terms [{}] within {slop}",
            content::text_tokens(value).join(", ")
        ),
        TextMode::Exact => format!(
            "content contains the word \"{value}\"{}",
            if case_sensitive {
                " (case-sensitive)"
            } else {
                ""
            }
        ),
        TextMode::Substring => format!(
            "content contains \"{value}\"{}",
            if case_sensitive {
                " (case-sensitive)"
            } else {
                ""
            }
        ),
        TextMode::Regex => format!(
            "content matches /{value}/{}",
            if case_sensitive {
                " (case-sensitive)"
            } else {
                " (case-insensitive)"
            }
        ),
    };
    ctx.readable.push(desc);
    let mut copts = ctx.content_opts;
    if ctx.neg_depth > 0 {
        // Page-driven verification filters the final candidate set, which is
        // only right in positive `AND` context; inside `OR`/`NOT` the clause
        // must be exact, so verify eagerly under the fetch budget.
        copts.lazy_min = usize::MAX;
        copts.max_candidates = copts.max_candidates.min(copts.max_verify);
    }
    let (ret, matcher) = content::retrieve(
        index,
        ctx.catalog,
        &clause,
        ctx.content_scope.as_deref(),
        &ctx.retired,
        &copts,
    )
    .map_err(|e| QueryError::Other {
        message: e.to_string(),
    })?;
    if ret.broad {
        ctx.warnings.push(format!(
            "content clause \"{value}\" has no selective literal; every chunk in scope was examined"
        ));
    }
    if ret.truncated {
        ctx.warnings.push(format!(
            "content clause \"{value}\" matched more chunks than the limit; results are a subset — narrow the query"
        ));
    }
    let verified_mode = matches!(
        mode,
        TextMode::Exact | TextMode::Substring | TextMode::Regex
    );
    let mut by_object: HashMap<ObjectId, ObjectMatch> = HashMap::new();
    let mut deferred: Option<HashMap<ObjectId, (u32, Vec<u32>)>> = None;
    if ret.deferred {
        // Candidates only: group per object, newest generation wins.
        let mut cands: HashMap<ObjectId, (u32, Vec<u32>)> = HashMap::new();
        for h in &ret.hits {
            let e = cands
                .entry(h.object_id)
                .or_insert((h.generation, Vec::new()));
            if h.generation > e.0 {
                *e = (h.generation, Vec::new());
            }
            if h.generation == e.0 {
                e.1.push(h.ordinal);
            }
        }
        for (_, ords) in cands.values_mut() {
            ords.sort_unstable();
        }
        deferred = Some(cands);
    } else {
        for h in &ret.hits {
            let e = by_object.entry(h.object_id).or_insert(ObjectMatch {
                score: 0.0,
                generation: h.generation,
                chunks: Vec::new(),
                rows: Vec::new(),
            });
            // Newer generation wins if both are somehow present.
            if h.generation > e.generation {
                e.generation = h.generation;
                e.chunks.clear();
                e.score = 0.0;
            }
            if h.generation == e.generation {
                e.chunks.push((h.ordinal, h.score));
                e.score = if verified_mode {
                    // Matching chunks, capped: the same score the page-driven
                    // path produces, so both paths rank alike.
                    object_score(e.chunks.len())
                } else {
                    e.score.max(h.score) + h.score * 0.1
                };
            }
        }
    }
    let stale = drop_stale_generations(ctx.catalog, &mut by_object, &mut deferred)?;
    let objects = deferred.as_ref().map_or(by_object.len(), |c| c.len());
    ctx.steps.push(PlanStep {
        stage: "content".into(),
        description: format!(
            "{} → {} file(s){}{}",
            ret.description,
            objects,
            if ret.deferred { " (unverified)" } else { "" },
            if stale > 0 {
                format!("; {stale} of an older generation dropped")
            } else {
                String::new()
            }
        ),
        candidates: Some(ret.candidates),
        verified: ret.verified,
        elapsed_ms: Some(ret.elapsed_ms),
    });
    if stale > 0 && ret.truncated {
        // Candidates are cut before their generation is known, so a chunk of
        // a superseded generation can occupy the budget that the same file's
        // current chunk would have used. The result is a subset either way
        // (the truncation warning above says so); name the second cause.
        ctx.warnings.push(format!(
            "content clause \"{value}\": {stale} file(s) matched only on a superseded generation. \
             The candidate list was truncated, so a current-generation match of theirs may have \
             been cut with it — narrow the query"
        ));
    }
    let ids: Vec<ObjectId> = match &deferred {
        Some(c) => c.keys().copied().collect(),
        None => by_object.keys().copied().collect(),
    };
    let terms: Vec<Term> = ids
        .iter()
        .map(|o| Term::from_field_u64(ctx.f.object_id, o.0 as u64))
        .collect();
    let q: Box<dyn TQuery> = if terms.is_empty() {
        Box::new(EmptyQuery)
    } else {
        Box::new(TermSetQuery::new(terms))
    };
    ctx.content_sets.push(ContentSet {
        by_object,
        matcher,
        truncated: ret.truncated,
        deferred,
    });
    Ok(q)
}

/// Drop content matches whose generation is not the object's current one,
/// returning how many objects were removed.
///
/// The content index is committed asynchronously (ADR-0005): a queued
/// deletion of an object's old chunk documents becomes visible only at the
/// next commit, and between a file changing and its re-extraction the index
/// legitimately still holds the previous generation. Such a document
/// describes text the file no longer contains, so it must not compose into a
/// file hit at all — filtering here (before the object set reaches the
/// catalog-index query) also keeps stale objects out of totals and facets,
/// and covers both the eager and the page-driven path of ADR-0008, whose
/// candidates are grouped per generation as well.
fn drop_stale_generations(
    catalog: &Catalog,
    by_object: &mut HashMap<ObjectId, ObjectMatch>,
    deferred: &mut Option<HashMap<ObjectId, (u32, Vec<u32>)>>,
) -> std::result::Result<usize, QueryError> {
    let ids: Vec<ObjectId> = match deferred {
        Some(c) => c.keys().copied().collect(),
        None => by_object.keys().copied().collect(),
    };
    if ids.is_empty() {
        return Ok(0);
    }
    let current = catalog
        .object_generations(&ids)
        .map_err(|e| QueryError::Other {
            message: e.to_string(),
        })?;
    let before = ids.len();
    match deferred {
        // An object gone from the catalog (`None`) is stale too.
        Some(c) => c.retain(|o, (g, _)| current.get(o) == Some(g)),
        None => by_object.retain(|o, m| current.get(o) == Some(&m.generation)),
    }
    let after = deferred.as_ref().map_or(by_object.len(), |c| c.len());
    Ok(before - after)
}

/// Source scope visible before compilation: `source:` clauses that are
/// top-level `AND` conjuncts.
fn pre_scope(q: &Query, catalog: &Catalog) -> Option<Vec<SourceId>> {
    fn collect(q: &Query, catalog: &Catalog, out: &mut Vec<SourceId>) -> bool {
        match q {
            Query::Source { ids, names } => {
                out.extend(ids.iter().copied());
                for n in names {
                    if let Ok(Some(s)) = catalog.find_source_by_name(n) {
                        out.push(s.id);
                    }
                }
                true
            }
            Query::And { clauses } => {
                let mut any = false;
                for c in clauses {
                    any |= collect(c, catalog, out);
                }
                any
            }
            _ => false,
        }
    }
    let mut out = Vec::new();
    if collect(q, catalog, &mut out) {
        Some(out)
    } else {
        None
    }
}

/// Distinct dictionary terms a regex may match before the result is cut.
pub const DICTIONARY_SCAN_TERMS: usize = 50_000;

/// Every term of `field`'s dictionary (all segments) that the regex
/// matches, as index terms. Linear in the dictionary size; used only when
/// the FST automaton cannot be built.
fn dictionary_scan(
    searcher: &Searcher,
    field: tantivy::schema::Field,
    re: &regex::Regex,
    limit: usize,
) -> std::result::Result<(Vec<Term>, bool), QueryError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut truncated = false;
    'outer: for seg in searcher.segment_readers() {
        let inv = seg.inverted_index(field).map_err(|e| QueryError::Other {
            message: e.to_string(),
        })?;
        let dict = inv.terms();
        let mut stream = dict.stream().map_err(|e| QueryError::Other {
            message: e.to_string(),
        })?;
        while stream.advance() {
            if let Ok(t) = std::str::from_utf8(stream.key()) {
                if re.is_match(t) && !seen.contains(t) {
                    if seen.len() >= limit {
                        truncated = true;
                        break 'outer;
                    }
                    seen.insert(t.to_string());
                }
            }
        }
    }
    Ok((
        seen.into_iter()
            .map(|t| Term::from_field_text(field, &t))
            .collect(),
        truncated,
    ))
}

fn needs_positive(ctx: &Ctx<'_>, what: &str) -> std::result::Result<(), QueryError> {
    if ctx.neg_depth > 0 {
        return Err(QueryError::Other {
            message: format!("{what} cannot be used inside OR or NOT; move it to the top level"),
        });
    }
    Ok(())
}

fn content_score(sets: &[ContentSet], object: ObjectId) -> f32 {
    sets.iter()
        .filter_map(|s| s.by_object.get(&object).map(|m| m.score))
        .sum()
}

/// Upper bound on `content_score` before deferred clauses are verified.
fn content_bound(sets: &[ContentSet], object: ObjectId) -> f32 {
    sets.iter().map(|s| s.bound(object)).sum()
}

/// `(content set index, object, generation, verdict)` per verified object.
type Verdicts = Vec<(usize, ObjectId, u32, content::ObjectVerdict)>;

/// Page-driven content verification: verifies objects of deferred clauses
/// on demand, in batches across threads, under one chunk-fetch budget.
struct LazyContent<'a> {
    sets: &'a mut Vec<ContentSet>,
    catalog: &'a Catalog,
    budget: std::sync::atomic::AtomicUsize,
    fetched: usize,
    rejected: HashSet<ObjectId>,
    /// The budget ran out: some objects could not be decided.
    exhausted: bool,
}

impl<'a> LazyContent<'a> {
    fn new(sets: &'a mut Vec<ContentSet>, catalog: &'a Catalog, budget: usize) -> Self {
        Self {
            sets,
            catalog,
            budget: std::sync::atomic::AtomicUsize::new(budget),
            fetched: 0,
            rejected: HashSet::new(),
            exhausted: false,
        }
    }

    fn decided(&self, object: ObjectId) -> bool {
        self.rejected.contains(&object)
            || self
                .sets
                .iter()
                .all(|s| !s.is_deferred() || s.by_object.contains_key(&object))
    }

    /// Verify every undecided object in `objects` so that `accepts` can
    /// answer for each. Work is spread over threads; the budget is shared.
    fn prepare(&mut self, objects: &[ObjectId]) -> Result<()> {
        if self.exhausted {
            return Ok(());
        }
        // (set index, object, generation, ordinals)
        let mut work: Vec<(usize, ObjectId, u32, Vec<u32>)> = Vec::new();
        let mut seen: HashSet<ObjectId> = HashSet::new();
        for o in objects {
            if !seen.insert(*o) || self.decided(*o) {
                continue;
            }
            for (i, set) in self.sets.iter().enumerate() {
                if let Some(c) = &set.deferred {
                    if !set.by_object.contains_key(o) {
                        match c.get(o) {
                            Some((g, ords)) => work.push((i, *o, *g, ords.clone())),
                            None => {
                                self.rejected.insert(*o);
                            }
                        }
                    }
                }
            }
        }
        if work.is_empty() {
            return Ok(());
        }
        let threads = work.len().clamp(1, verify_threads());
        let per = work.len().div_ceil(threads).max(1);
        let catalog = self.catalog;
        let budget = &self.budget;
        let sets: &Vec<ContentSet> = self.sets;
        let results: Vec<Result<Verdicts>> = std::thread::scope(|sc| {
            let handles: Vec<_> = work
                .chunks(per)
                .map(|part| {
                    sc.spawn(move || {
                        let jobs: Vec<VerifyJob<'_>> = part
                            .iter()
                            .map(|(i, o, g, ords)| VerifyJob {
                                matcher: &sets[*i].matcher,
                                object: *o,
                                generation: *g,
                                ordinals: ords,
                            })
                            .collect();
                        let verdicts = verify_objects(catalog, &jobs, budget)?;
                        Ok(part
                            .iter()
                            .zip(verdicts)
                            .map(|((i, o, g, _), v)| (*i, *o, *g, v))
                            .collect())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("content verification thread"))
                .collect()
        });
        for r in results {
            for (i, o, g, v) in r? {
                self.fetched += v.fetched;
                if v.budget_short {
                    self.exhausted = true;
                }
                if v.undecided {
                    continue;
                }
                if v.matched.is_empty() {
                    self.rejected.insert(o);
                    continue;
                }
                let mut rows = Vec::with_capacity(v.matched.len());
                let mut chunks = Vec::with_capacity(v.matched.len());
                for (row, n) in v.matched {
                    chunks.push((row.ordinal, n as f32));
                    rows.push(row);
                }
                self.sets[i].by_object.insert(
                    o,
                    ObjectMatch {
                        score: object_score(chunks.len()),
                        generation: g,
                        chunks,
                        rows,
                    },
                );
            }
        }
        Ok(())
    }

    /// Whether every deferred clause matched the object (after `prepare`).
    fn accepts(&self, object: ObjectId) -> bool {
        !self.rejected.contains(&object)
            && self
                .sets
                .iter()
                .all(|s| !s.is_deferred() || s.by_object.contains_key(&object))
    }
}

/// A row of the page walk: an entry with its sort keys and object.
trait WalkRow {
    fn object(&self) -> ObjectId;
    fn score(&self) -> f32;
    fn set_score(&mut self, s: f32);
    fn entry(&self) -> i64;
}

impl WalkRow for FastRow {
    fn object(&self) -> ObjectId {
        self.object_id
    }
    fn score(&self) -> f32 {
        self.score
    }
    fn set_score(&mut self, s: f32) {
        self.score = s;
    }
    fn entry(&self) -> i64 {
        self.entry_id
    }
}

impl WalkRow for Stored {
    fn object(&self) -> ObjectId {
        self.object_id
    }
    fn score(&self) -> f32 {
        self.score
    }
    fn set_score(&mut self, s: f32) {
        self.score = s;
    }
    fn entry(&self) -> i64 {
        self.entry_id
    }
}

/// Rows verified per batch before the page walk checks whether it may stop.
const WALK_BATCH: usize = 64;

struct WalkOutcome<T> {
    verified: Vec<T>,
    examined: usize,
    /// The walk reached the end of the candidates: totals are exact.
    exhausted: bool,
}

fn by_score_then_entry<T: WalkRow>(a: &T, b: &T) -> std::cmp::Ordering {
    b.score()
        .partial_cmp(&a.score())
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.entry().cmp(&b.entry()))
}

/// Verify candidates in sort order only until `want` survive. `pass` is the
/// cheap per-row check (fast-field verifiers); `content` verifies deferred
/// content clauses in batches. With `by_bound` (relevance sort over deferred
/// content) rows arrive in descending score-bound order and the walk stops
/// once the next bound cannot beat the `want`-th best verified score, so the
/// page order is exact; `verified` is then sorted by score.
fn lazy_walk<T: WalkRow>(
    rows: impl Iterator<Item = T>,
    want: usize,
    mut pass: impl FnMut(&mut T) -> bool,
    mut content: Option<&mut LazyContent<'_>>,
    by_bound: bool,
) -> Result<WalkOutcome<T>> {
    let mut rows = rows.peekable();
    let mut verified: Vec<T> = Vec::with_capacity(want);
    let mut examined = 0usize;
    let exhausted;
    loop {
        if rows.peek().is_none() {
            exhausted = true;
            break;
        }
        if verified.len() >= want {
            let stop = match (by_bound, rows.peek()) {
                (true, Some(next)) => {
                    verified.sort_by(by_score_then_entry);
                    // Rows arrive in (bound desc, entry asc) order and a
                    // verified score never exceeds its bound, so the walk
                    // may stop once the next row would sort after the
                    // page boundary even at its best — including the
                    // entry-id tie-break on equal scores.
                    let kth = &verified[want - 1];
                    next.score() < kth.score()
                        || (next.score() == kth.score() && next.entry() > kth.entry())
                }
                _ => true,
            };
            if stop {
                exhausted = false;
                break;
            }
        }
        // Cheap checks on a batch, then one parallel content pass.
        let mut batch: Vec<T> = Vec::with_capacity(WALK_BATCH);
        while batch.len() < WALK_BATCH {
            match rows.next() {
                Some(mut r) => {
                    examined += 1;
                    if pass(&mut r) {
                        batch.push(r);
                    }
                }
                None => break,
            }
        }
        match content.as_deref_mut() {
            Some(lc) => {
                let objects: Vec<ObjectId> = batch.iter().map(|r| r.object()).collect();
                lc.prepare(&objects)?;
                for mut r in batch {
                    if lc.accepts(r.object()) {
                        r.set_score(content_score(lc.sets, r.object()));
                        verified.push(r);
                    }
                }
                if lc.exhausted {
                    exhausted = false;
                    break;
                }
            }
            None => verified.extend(batch),
        }
    }
    if by_bound {
        verified.sort_by(by_score_then_entry);
    }
    Ok(WalkOutcome {
        verified,
        examined,
        exhausted,
    })
}

fn compile_path(
    mode: PathMode,
    value: &str,
    case_sensitive: bool,
    ctx: &mut Ctx<'_>,
) -> std::result::Result<Box<dyn TQuery>, QueryError> {
    let f = ctx.f;
    // A path is stored the way its source spells it; matching happens in one
    // canonical spelling so a query written with either separator finds it.
    let norm = canonical_path(value);
    Ok(match mode {
        PathMode::Exact => {
            ctx.readable.push(format!("path is \"{norm}\""));
            if case_sensitive {
                needs_positive(ctx, "a case-sensitive clause")?;
                let v = norm.clone();
                ctx.verifiers
                    .push(Box::new(move |s, _| canonical_path(&s.path) == v));
            }
            term_text(f.path_folded, &fold(&norm))
        }
        PathMode::Prefix => {
            let trimmed = norm.trim_end_matches('/').to_string();
            ctx.readable.push(format!("path under \"{trimmed}\""));
            let pat = format!("{}(/.*)?", regex::escape(&fold(&trimmed)));
            if case_sensitive {
                needs_positive(ctx, "a case-sensitive clause")?;
                let v = trimmed.clone();
                ctx.verifiers.push(Box::new(move |s, _| {
                    let path = canonical_path(&s.path);
                    path == v || path.starts_with(&format!("{v}/"))
                }));
            }
            make_regex(f.path_folded, &pat)?
        }
        PathMode::Glob => {
            ctx.readable.push(format!("path matches glob \"{norm}\""));
            let folded = fold(&norm);
            let re = glob_to_regex(&folded);
            let pat = if norm.contains('/') {
                re
            } else {
                format!(".*{re}")
            };
            let candidates = if ctx.neg_depth == 0 {
                trigram_query(f.path_tri, &glob_literals(&folded))
            } else {
                None
            };
            match candidates {
                Some(q) => {
                    let anchored = regex::RegexBuilder::new(&format!("^{pat}$"))
                        .size_limit(1 << 20)
                        .build()
                        .map_err(|e| QueryError::InvalidRegex {
                            message: e.to_string(),
                        })?;
                    ctx.fast_verifiers
                        .push(Box::new(move |_, path| anchored.is_match(path)));
                    ctx.steps.push(PlanStep {
                        stage: "candidates".into(),
                        description: "trigrams of the glob's literal parts on path; candidates verified on the folded path fast field".into(),
                        candidates: None,
                        verified: None,
                        elapsed_ms: None,
                    });
                    q
                }
                None => make_regex(f.path_folded, &pat)?,
            }
        }
        PathMode::Regex => compile_text(
            TextField::Path,
            TextMode::Regex,
            value,
            case_sensitive,
            0,
            ctx,
        )?,
    })
}

fn read_stored(f: &Fields, doc: &TantivyDocument, score: f32) -> Stored {
    let u = |field| doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0);
    let i = |field| doc.get_first(field).and_then(|v| v.as_i64()).unwrap_or(0);
    let s = |field| {
        doc.get_first(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let parent = u(f.parent_id);
    Stored {
        entry_id: u(f.entry_id) as i64,
        object_id: ObjectId(u(f.object_id) as i64),
        source_id: SourceId(u(f.source_id) as i64),
        parent_id: if parent == 0 {
            None
        } else {
            Some(ObjectId(parent as i64))
        },
        ancestors: doc
            .get_all(f.ancestors)
            .filter_map(|v| v.as_u64())
            .map(|a| ObjectId(a as i64))
            .collect(),
        name: s(f.name),
        path: s(f.path),
        kind: ObjectKind::parse(&s(f.kind)).unwrap_or(ObjectKind::File),
        extension: s(f.extension),
        size: u(f.size),
        allocated: u(f.allocated),
        modified: i(f.modified),
        created: i(f.created),
        subtree_logical: u(f.subtree_logical),
        subtree_allocated: u(f.subtree_allocated),
        newest_modified: i(f.newest_modified),
        file_count: u(f.file_count),
        dir_count: u(f.dir_count),
        score,
    }
}

fn sort_key_cmp(a: &Stored, b: &Stored, sort: Sort) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ord = match sort.field {
        SortField::Relevance => b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal),
        SortField::Name => fold(&a.name).cmp(&fold(&b.name)),
        SortField::Path => fold(&a.path).cmp(&fold(&b.path)),
        SortField::Size | SortField::SubtreeSize => a.subtree_logical.cmp(&b.subtree_logical),
        SortField::AllocatedSize => a.subtree_allocated.cmp(&b.subtree_allocated),
        SortField::Modified => a.newest_modified.cmp(&b.newest_modified),
        SortField::Created => a.created.cmp(&b.created),
    };
    let ord = if sort.field == SortField::Relevance {
        ord
    } else if sort.descending {
        ord.reverse()
    } else {
        ord
    };
    ord.then_with(|| a.entry_id.cmp(&b.entry_id))
}

/// Extra candidates fetched beyond the page so that documents the catalog
/// no longer knows (the projection is slightly behind) can be replaced on
/// the same page instead of shortening it. The eager paths hold every
/// candidate and the top-k path re-collects with a growing limit, so only
/// the page-driven lazy path (bounded by its chunk-fetch budget) can still
/// return a short page behind a longer run of stale documents.
pub const STALE_SLACK: usize = 8;

/// Page cursor: `o:<consumed candidates>;g:<index generation>;q:<query
/// fingerprint>[;t:<total>;x:<exact>];s:<signature>`. The offset counts every candidate
/// the previous pages consumed — including stale projection documents that
/// produced no hit — so a page never re-examines what an earlier page
/// skipped. `g` is the Tantivy searcher generation the cursor was issued
/// from (a change is reported as a warning: offsets are not stable across
/// commits); `q` binds the cursor to the query, mode, sort, and scope it
/// was issued for; `t`/`x` carry the first page's total and whether it was
/// exact, so later pages of a top-k walk do not recount while the
/// generation is unchanged. `s` authenticates the full structured cursor,
/// preventing a caller from changing the carried total or any paging state.
/// The legacy `o:<offset>` form is still accepted without any of the checks
/// and cannot carry a total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    offset: usize,
    generation: Option<u64>,
    fingerprint: Option<u64>,
    total: Option<(u64, bool)>,
}

const MAX_CURSOR_BYTES: usize = 256;

fn decode_cursor(
    c: &Option<String>,
    signing_key: &[u8; 32],
) -> std::result::Result<Cursor, QueryError> {
    let s = match c {
        None => {
            return Ok(Cursor {
                offset: 0,
                generation: None,
                fingerprint: None,
                total: None,
            })
        }
        Some(s) => s,
    };
    if s.len() > MAX_CURSOR_BYTES {
        return Err(QueryError::InvalidCursor);
    }
    let (unsigned, signature) = match s.rsplit_once(";s:") {
        Some((unsigned, signature)) if !signature.is_empty() => (unsigned, Some(signature)),
        _ => (s.as_str(), None),
    };
    let mut cursor = Cursor {
        offset: 0,
        generation: None,
        fingerprint: None,
        total: None,
    };
    let mut seen_offset = false;
    let mut seen_generation = false;
    let mut seen_fingerprint = false;
    let mut seen_total = false;
    let mut seen_exact = false;
    let (mut total, mut exact): (Option<u64>, Option<bool>) = (None, None);
    for part in unsigned.split(';') {
        let (key, value) = part.split_once(':').ok_or(QueryError::InvalidCursor)?;
        match key {
            "o" => {
                if seen_offset {
                    return Err(QueryError::InvalidCursor);
                }
                cursor.offset = value.parse().map_err(|_| QueryError::InvalidCursor)?;
                seen_offset = true;
            }
            "g" => {
                if seen_generation {
                    return Err(QueryError::InvalidCursor);
                }
                cursor.generation = Some(value.parse().map_err(|_| QueryError::InvalidCursor)?);
                seen_generation = true;
            }
            "q" => {
                if seen_fingerprint {
                    return Err(QueryError::InvalidCursor);
                }
                cursor.fingerprint =
                    Some(u64::from_str_radix(value, 16).map_err(|_| QueryError::InvalidCursor)?);
                seen_fingerprint = true;
            }
            "t" => {
                if seen_total {
                    return Err(QueryError::InvalidCursor);
                }
                total = Some(value.parse().map_err(|_| QueryError::InvalidCursor)?);
                seen_total = true;
            }
            "x" => {
                if seen_exact {
                    return Err(QueryError::InvalidCursor);
                }
                exact = Some(match value {
                    "1" => true,
                    "0" => false,
                    _ => return Err(QueryError::InvalidCursor),
                });
                seen_exact = true;
            }
            _ => return Err(QueryError::InvalidCursor),
        }
    }
    let structured = cursor.generation.is_some();
    if !seen_offset
        || structured != cursor.fingerprint.is_some()
        || total.is_some() != exact.is_some()
        || (total.is_some() && !structured)
        || structured != signature.is_some()
    {
        // Either the legacy `o:<n>` form or the full structured form; a
        // cursor carrying only part of a check is not something the service
        // ever issued.
        return Err(QueryError::InvalidCursor);
    }
    if let Some(signature) = signature {
        let expected = blake3::keyed_hash(signing_key, unsigned.as_bytes());
        if expected.to_hex().as_str() != signature {
            return Err(QueryError::InvalidCursor);
        }
    }
    cursor.total = total.zip(exact);
    Ok(cursor)
}

fn encode_cursor(
    offset: usize,
    generation: u64,
    fingerprint: u64,
    total: Option<(u64, bool)>,
    signing_key: &[u8; 32],
) -> String {
    let mut s = format!("o:{offset};g:{generation};q:{fingerprint:016x}");
    if let Some((t, exact)) = total {
        s.push_str(&format!(";t:{t};x:{}", u8::from(exact)));
    }
    let signature = blake3::keyed_hash(signing_key, s.as_bytes());
    s.push_str(&format!(";s:{signature}"));
    s
}

/// Stable (process-independent) fingerprint of everything that determines
/// the candidate order a cursor walks: query, mode, sort, and scope.
fn query_fingerprint(req: &SearchRequest) -> u64 {
    let key = serde_json::to_string(&(&req.query, req.mode, req.sort, req.include_retired))
        .unwrap_or_default();
    // FNV-1a: small, dependency-free, and identical across builds.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Candidates in page order; each resolves lazily to its stored fields.
type Candidates<'a> = Box<dyn Iterator<Item = Result<Stored>> + 'a>;

/// Top-k results that re-collect with a doubled limit (bounded by the
/// match count) when consumed past what was collected. Re-collection is
/// deterministic for one searcher, so continuing from the previous position
/// yields the same order.
type Collector<'a> = Box<dyn Fn(usize) -> Result<Vec<(f32, DocAddress)>> + 'a>;
/// Top-k addresses plus the exact match count when it was taken.
type Collected = (Vec<(f32, DocAddress)>, Option<usize>);

struct Regrowing<'a> {
    collect: Collector<'a>,
    buf: Vec<(f32, DocAddress)>,
    pos: usize,
    n: usize,
    total: usize,
}

impl Iterator for Regrowing<'_> {
    type Item = Result<(f32, DocAddress)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            if self.n >= self.total || self.buf.len() < self.n {
                return None; // everything that matches has been collected
            }
            self.n = (self.n * 2).min(self.total);
            match (self.collect)(self.n) {
                Ok(b) => self.buf = b,
                Err(e) => return Some(Err(e)),
            }
            if self.pos >= self.buf.len() {
                return None;
            }
        }
        let item = self.buf[self.pos];
        self.pos += 1;
        Some(Ok(item))
    }
}

/// Execute a search request without a content index (content clauses are
/// rejected with an explanation).
pub fn search(
    index: &CatalogIndex,
    catalog: &Catalog,
    req: &SearchRequest,
    opts: &ExecOptions,
) -> Result<SearchResponse> {
    search_with_content(index, None, catalog, req, opts)
}

/// Execute a search request.
pub fn search_with_content(
    index: &CatalogIndex,
    content_index: Option<&ContentIndex>,
    catalog: &Catalog,
    req: &SearchRequest,
    opts: &ExecOptions,
) -> Result<SearchResponse> {
    let t0 = Instant::now();
    req.validate(&opts.limits)?;
    let cursor = decode_cursor(&req.cursor, index.cursor_key())?;
    let fingerprint = query_fingerprint(req);
    if let Some(q) = cursor.fingerprint {
        if q != fingerprint {
            return Err(QueryError::CursorMismatch {
                message: "the cursor was issued for a different query, mode, sort, or scope".into(),
            }
            .into());
        }
    }
    let offset = cursor.offset;
    let limit = req.limit.max(1) as usize;
    let f = index.fields();
    let all_sources = catalog.list_sources()?;
    let retired: Vec<SourceId> = if req.include_retired {
        Vec::new()
    } else {
        all_sources
            .iter()
            .filter(|s| s.state == SourceState::Retired)
            .map(|s| s.id)
            .collect()
    };
    let searcher = index.searcher();
    let mut ctx = Ctx {
        f,
        searcher: &searcher,
        catalog,
        verifiers: Vec::new(),
        steps: Vec::new(),
        readable: Vec::new(),
        warnings: Vec::new(),
        scope_sources: None,
        tight_dirs: false,
        fast_verifiers: Vec::new(),
        neg_depth: 0,
        content: content_index,
        content_opts: opts.content,
        content_sets: Vec::new(),
        content_scope: pre_scope(&req.query, catalog),
        retired,
    };
    let mut root = compile(&req.query, &mut ctx)?;

    // Scope: result mode and retired sources.
    let mut scope: Vec<(Occur, Box<dyn TQuery>)> = vec![(Occur::Must, root)];
    match req.mode {
        ResultMode::Files => scope.push((
            Occur::Must,
            any_of(vec![
                term_text(f.kind, "file"),
                term_text(f.kind, "reparse"),
                term_text(f.kind, "virtual_file"),
            ]),
        )),
        ResultMode::Directories => scope.push((
            Occur::Must,
            any_of(vec![
                term_text(f.kind, "directory"),
                term_text(f.kind, "virtual_directory"),
            ]),
        )),
        ResultMode::Both => {}
    }
    let in_scope: Vec<&eidos_catalog::SourceRecord> = all_sources
        .iter()
        .filter(|s| req.include_retired || s.state != SourceState::Retired)
        .filter(|s| {
            ctx.scope_sources
                .as_ref()
                .is_none_or(|ids| ids.contains(&s.id))
        })
        .collect();
    for s in &all_sources {
        if !req.include_retired && s.state == SourceState::Retired {
            scope.push((Occur::MustNot, term_u64(f.source_id, s.id.0 as u64)));
        }
    }
    root = Box::new(BooleanQuery::new(scope));
    let plan_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let tight = ctx.tight_dirs && req.mode == ResultMode::Directories;
    let has_content = !ctx.content_sets.is_empty();
    let content_truncated = ctx.content_sets.iter().any(|s| s.truncated);
    let lazy_content = ctx.content_sets.iter().any(|s| s.is_deferred());
    let mut content_sets = std::mem::take(&mut ctx.content_sets);
    let collect_all =
        !ctx.verifiers.is_empty() || !ctx.fast_verifiers.is_empty() || tight || has_content;
    let t1 = Instant::now();
    let mut warnings = ctx.warnings.clone();
    let index_generation = searcher.generation().generation_id();
    let same_generation = cursor.generation == Some(index_generation);
    if cursor.generation.is_some_and(|g| g != index_generation) {
        warnings.push(
            "the index changed since this cursor was issued; page boundaries may have shifted, so a result may repeat or be skipped"
                .into(),
        );
    }
    // A total carried by the cursor is reused only while the index is the
    // one it was counted on, and only under the default policy.
    let carried_total = match req.count {
        CountPolicy::Auto if same_generation => cursor.total,
        _ => None,
    };
    let mut steps = ctx.steps.clone();
    let mut verify_ms = 0.0;

    let (page, total, total_exact, total_origin): (Candidates<'_>, Option<u64>, bool, TotalOrigin) =
        if collect_all {
            let addrs: HashSet<DocAddress> = searcher.search(&root, &DocSetCollector)?;
            let mut truncated = false;
            let mut list: Vec<DocAddress> = addrs.into_iter().collect();
            list.sort();
            if list.len() > opts.max_candidates {
                truncated = true;
                list.truncate(opts.max_candidates);
                warnings.push(format!(
                    "more than {} candidates; results are truncated — narrow the query",
                    opts.max_candidates
                ));
            }
            let candidates = list.len() as u64;
            // Fast-field pass: numeric keys and term ordinals for every
            // candidate (cheap). Folded strings are resolved only when a verifier
            // or a name/path sort needs them.
            let tv = Instant::now();
            let mut cols: HashMap<u32, FastCols> = HashMap::new();
            for a in &list {
                if let std::collections::hash_map::Entry::Vacant(e) = cols.entry(a.segment_ord) {
                    e.insert(FastCols::open(&searcher, a.segment_ord)?);
                }
            }
            let mut rows: Vec<FastRow> = list
                .iter()
                .map(|a| cols.get(&a.segment_ord).expect("opened").row(*a))
                .collect();
            drop(list);
            if has_content {
                // Deferred clauses contribute their upper bound until verified;
                // the page walk replaces it with the verified score.
                for r in rows.iter_mut() {
                    r.score = content_bound(&content_sets, r.object_id);
                }
            }
            let fast = &ctx.fast_verifiers;
            let need_stored = !ctx.verifiers.is_empty() || tight;
            let sort_by_string = matches!(req.sort.field, SortField::Name | SortField::Path);
            let lazy = !need_stored
                && (lazy_content
                    || (rows.len() > LAZY_VERIFY_MIN && (!fast.is_empty() || sort_by_string)));
            let by_bound = lazy_content && req.sort.field == SortField::Relevance && !tight;
            let mut lazy_cx = if lazy_content {
                Some(LazyContent::new(
                    &mut content_sets,
                    catalog,
                    opts.content.max_verify,
                ))
            } else {
                None
            };
            let want = offset + limit + STALE_SLACK;

            // Explain and warn for a page walk; returns (total, exact).
            let report_walk = |steps: &mut Vec<PlanStep>,
                               warnings: &mut Vec<String>,
                               outcome_len: usize,
                               examined: usize,
                               exhausted: bool,
                               lc: Option<&LazyContent<'_>>,
                               elapsed_ms: f64|
             -> (u64, bool) {
                let mut desc =
                    format!("lazy: {examined} of {candidates} candidates examined in sort order");
                if !fast.is_empty() {
                    desc.push_str(" on folded fast fields");
                }
                if let Some(lc) = lc {
                    desc.push_str(&format!(
                        "; {} chunks fetched for content verification",
                        lc.fetched
                    ));
                }
                if !exhausted {
                    desc.push_str("; total is an upper bound");
                }
                steps.push(PlanStep {
                    stage: "verify".into(),
                    description: desc,
                    candidates: Some(candidates),
                    verified: Some(outcome_len as u64),
                    elapsed_ms: Some(elapsed_ms),
                });
                if lc.is_some_and(|lc| lc.exhausted) {
                    warnings.push(format!(
                    "content verification stopped after {} chunk fetches; results are a subset — narrow the query",
                    opts.content.max_verify
                ));
                } else if !exhausted {
                    warnings.push(format!(
                    "{candidates} candidates; the total and facet counts are upper bounds — hits shown are verified"
                ));
                }
                let total = if exhausted {
                    outcome_len as u64
                } else {
                    candidates
                };
                (total, exhausted && !truncated && !content_truncated)
            };

            if !lazy {
                // Eager: resolve strings where needed and verify everything.
                if !fast.is_empty() || sort_by_string {
                    let threads = (rows.len() / 2_000).clamp(1, verify_threads());
                    let per = rows.len().div_ceil(threads).max(1);
                    let cols_ref = &cols;
                    let mut parts: Vec<Vec<FastRow>> = Vec::new();
                    let mut chunks: Vec<Vec<FastRow>> = Vec::new();
                    let mut it = rows.into_iter().peekable();
                    while it.peek().is_some() {
                        chunks.push(it.by_ref().take(per).collect());
                    }
                    std::thread::scope(|sc| {
                        let handles: Vec<_> = chunks
                            .into_iter()
                            .map(|mut part| {
                                sc.spawn(move || {
                                    part.retain_mut(|r| {
                                        let c = cols_ref.get(&r.addr.segment_ord).expect("opened");
                                        c.resolve(r);
                                        c.passes(r, fast)
                                    });
                                    part
                                })
                            })
                            .collect();
                        for h in handles {
                            parts.push(h.join().expect("fast-field thread"));
                        }
                    });
                    rows = parts.into_iter().flatten().collect();
                    if !fast.is_empty() {
                        steps.push(PlanStep {
                            stage: "verify".into(),
                            description: format!(
                                "{candidates} candidates verified on folded fast fields"
                            ),
                            candidates: Some(candidates),
                            verified: Some(rows.len() as u64),
                            elapsed_ms: Some(tv.elapsed().as_secs_f64() * 1000.0),
                        });
                    }
                }
                if need_stored {
                    let threads = (rows.len() / 500).clamp(1, verify_threads());
                    let per = rows.len().div_ceil(threads).max(1);
                    let searcher_ref = &searcher;
                    let parts: Vec<Result<Vec<Stored>>> = std::thread::scope(|sc| {
                        let handles: Vec<_> = rows
                            .chunks(per)
                            .map(|part| {
                                sc.spawn(move || -> Result<Vec<Stored>> {
                                    let mut out = Vec::with_capacity(part.len());
                                    for r in part {
                                        let doc: TantivyDocument = searcher_ref.doc(r.addr)?;
                                        out.push(read_stored(f, &doc, r.score));
                                    }
                                    Ok(out)
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| h.join().expect("store thread"))
                            .collect()
                    });
                    let mut stored: Vec<Stored> = Vec::with_capacity(rows.len());
                    for part in parts {
                        stored.extend(part?);
                    }
                    let fast_candidates = rows.len() as u64;
                    drop(rows);
                    let tv2 = Instant::now();
                    let mut verified: Vec<Stored> = stored
                        .into_iter()
                        .filter(|s| ctx.verifiers.iter().all(|v| v(s, catalog)))
                        .collect();
                    if tight {
                        // Rank the tightest containers first: a directory is "loose"
                        // when a candidate beneath it also satisfies the predicate.
                        let ids: HashSet<ObjectId> = verified.iter().map(|s| s.object_id).collect();
                        let mut loose: HashSet<ObjectId> = HashSet::new();
                        for s in &verified {
                            for a in &s.ancestors {
                                if ids.contains(a) {
                                    loose.insert(*a);
                                }
                            }
                        }
                        for s in verified.iter_mut() {
                            s.score = if loose.contains(&s.object_id) {
                                0.5
                            } else {
                                1.0
                            };
                        }
                        steps.push(PlanStep {
                        stage: "rank".into(),
                        description: "tightest containing directories first; ancestors that only contain matches via a child are ranked second".into(),
                        candidates: Some(verified.len() as u64),
                        verified: Some(verified.iter().filter(|s| s.score >= 1.0).count() as u64),
                        elapsed_ms: None,
                    });
                    }
                    verify_ms = tv.elapsed().as_secs_f64() * 1000.0;
                    if !ctx.verifiers.is_empty() {
                        steps.push(PlanStep {
                            stage: "verify".into(),
                            description: format!(
                                "{fast_candidates} candidates verified against stored originals"
                            ),
                            candidates: Some(fast_candidates),
                            verified: Some(verified.len() as u64),
                            elapsed_ms: Some(tv2.elapsed().as_secs_f64() * 1000.0),
                        });
                    }
                    let sort = if tight && req.sort.field == SortField::Relevance {
                        Sort {
                            field: SortField::Relevance,
                            descending: true,
                        }
                    } else {
                        req.sort
                    };
                    verified.sort_by(|a, b| {
                        if tight {
                            b.score
                                .partial_cmp(&a.score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| {
                                    sort_key_cmp(
                                        a,
                                        b,
                                        if sort.field == SortField::Relevance {
                                            Sort {
                                                field: SortField::Name,
                                                descending: false,
                                            }
                                        } else {
                                            sort
                                        },
                                    )
                                })
                        } else {
                            sort_key_cmp(a, b, sort)
                        }
                    });
                    if let Some(lc) = lazy_cx.as_mut() {
                        // Stored verifiers ran eagerly; content is still verified
                        // only as far as the page needs.
                        let tw = Instant::now();
                        let n = verified.len();
                        let outcome =
                            lazy_walk(verified.into_iter(), want, |_| true, Some(lc), by_bound)?;
                        verify_ms = tv.elapsed().as_secs_f64() * 1000.0;
                        let (total, exact) = report_walk(
                            &mut steps,
                            &mut warnings,
                            outcome.verified.len(),
                            outcome.examined.min(n),
                            outcome.exhausted,
                            Some(lc),
                            tw.elapsed().as_secs_f64() * 1000.0,
                        );
                        let page: Candidates<'_> =
                            Box::new(outcome.verified.into_iter().skip(offset).map(Ok));
                        (page, Some(total), exact, TotalOrigin::Counted)
                    } else {
                        let total = verified.len() as u64;
                        let page: Candidates<'_> =
                            Box::new(verified.into_iter().skip(offset).map(Ok));
                        (
                            page,
                            Some(total),
                            !truncated && !content_truncated,
                            TotalOrigin::Counted,
                        )
                    }
                } else {
                    verify_ms = tv.elapsed().as_secs_f64() * 1000.0;
                    rows.sort_by(|a, b| fast_sort_cmp(a, b, req.sort));
                    let total = rows.len() as u64;
                    let page: Candidates<'_> = Box::new(rows.into_iter().skip(offset).map(|r| {
                        let doc: TantivyDocument = searcher.doc(r.addr)?;
                        Ok(read_stored(f, &doc, r.score))
                    }));
                    (
                        page,
                        Some(total),
                        !truncated && !content_truncated,
                        TotalOrigin::Counted,
                    )
                }
            } else {
                // Lazy: order candidates by cheap keys, then resolve and verify
                // in that order only until the page is filled. Every returned hit
                // is verified; the total is an upper bound unless the walk ended.
                let cols_ref = &cols;
                let pass = |r: &mut FastRow| {
                    cols_ref
                        .get(&r.addr.segment_ord)
                        .expect("opened")
                        .passes(r, fast)
                };
                let outcome = if sort_by_string {
                    let merged = MergeByString::new(
                        rows,
                        cols_ref,
                        req.sort.field == SortField::Name,
                        req.sort.descending,
                    );
                    lazy_walk(merged, want, pass, lazy_cx.as_mut(), by_bound)?
                } else {
                    rows.sort_by(|a, b| fast_sort_cmp(a, b, req.sort));
                    lazy_walk(rows.into_iter(), want, pass, lazy_cx.as_mut(), by_bound)?
                };
                verify_ms = tv.elapsed().as_secs_f64() * 1000.0;
                let (total, exact) = report_walk(
                    &mut steps,
                    &mut warnings,
                    outcome.verified.len(),
                    outcome.examined,
                    outcome.exhausted,
                    lazy_cx.as_ref(),
                    verify_ms,
                );
                let page: Candidates<'_> =
                    Box::new(outcome.verified.into_iter().skip(offset).map(|r| {
                        let doc: TantivyDocument = searcher.doc(r.addr)?;
                        Ok(read_stored(f, &doc, r.score))
                    }));
                (page, Some(total), exact, TotalOrigin::Counted)
            }
        } else {
            let order = if req.sort.descending {
                Order::Desc
            } else {
                Order::Asc
            };
            // Collect the top `n` in sort order. The candidate stream below asks
            // for more (doubling, bounded by the match count) when a run of
            // stale documents exhausts what was collected, so a page is short
            // only when the matches themselves run out. The exact count is a
            // full pass over the matches, so it is taken only when the policy
            // asks for it: on the first page (`Auto`), every page (`Exact`), or
            // never (`None`).
            let searcher_ref = &searcher;
            let root_ref: &dyn TQuery = root.as_ref();
            let sort = req.sort;
            let collect = move |n: usize, count: bool| -> Result<Collected> {
                let top = TopDocs::with_limit(n.max(1));
                Ok(match sort.field {
                    SortField::Relevance => {
                        let (hits, count) =
                            top_and_count(searcher_ref, root_ref, top.order_by_score(), count)?;
                        (hits, count)
                    }
                    SortField::Name | SortField::Path => {
                        let field = if sort.field == SortField::Name {
                            "name_folded"
                        } else {
                            "path_folded"
                        };
                        let (hits, count) = top_and_count(
                            searcher_ref,
                            root_ref,
                            top.order_by_string_fast_field(field, order),
                            count,
                        )?;
                        (hits.into_iter().map(|(_, a)| (1.0, a)).collect(), count)
                    }
                    SortField::Modified | SortField::Created => {
                        let field = if sort.field == SortField::Modified {
                            "newest_modified"
                        } else {
                            "created"
                        };
                        let (hits, count) = top_and_count(
                            searcher_ref,
                            root_ref,
                            top.order_by_fast_field::<i64>(field, order),
                            count,
                        )?;
                        (hits.into_iter().map(|(_, a)| (1.0, a)).collect(), count)
                    }
                    SortField::Size | SortField::SubtreeSize | SortField::AllocatedSize => {
                        let field = if sort.field == SortField::AllocatedSize {
                            "subtree_allocated"
                        } else {
                            "subtree_logical"
                        };
                        let (hits, count) = top_and_count(
                            searcher_ref,
                            root_ref,
                            top.order_by_fast_field::<u64>(field, order),
                            count,
                        )?;
                        (hits.into_iter().map(|(_, a)| (1.0, a)).collect(), count)
                    }
                })
            };
            let n = offset + limit + STALE_SLACK;
            let need_count = match req.count {
                CountPolicy::Exact => true,
                CountPolicy::Auto => carried_total.is_none(),
                CountPolicy::None => false,
            };
            let (addrs, count) = collect(n, need_count)?;
            let known: Option<(u64, bool, TotalOrigin)> = match count {
                Some(c) => Some((c as u64, true, TotalOrigin::Counted)),
                None => carried_total.map(|(t, exact)| (t, exact, TotalOrigin::Cursor)),
            };
            steps.push(PlanStep {
                stage: "retrieve".into(),
                description: match (req.sort.field, known) {
                    (SortField::Relevance, Some((_, _, TotalOrigin::Counted))) => {
                        "BM25 ranked retrieval, top-k + exact count".into()
                    }
                    (SortField::Relevance, _) => "BM25 ranked retrieval, top-k (no count)".into(),
                    (other, Some((_, _, TotalOrigin::Counted))) => {
                        format!("fast-field sorted retrieval by {other:?}, top-k + exact count")
                    }
                    (other, _) => {
                        format!("fast-field sorted retrieval by {other:?}, top-k (no count)")
                    }
                },
                candidates: known.map(|(t, _, _)| t),
                verified: None,
                elapsed_ms: None,
            });
            let regrowing = Regrowing {
                collect: Box::new(move |n| collect(n, false).map(|(hits, _)| hits)),
                buf: addrs,
                pos: 0,
                n,
                total: known.map(|(t, _, _)| t as usize).unwrap_or(usize::MAX),
            };
            let page: Candidates<'_> = Box::new(regrowing.skip(offset).map(|r| {
                let (score, a) = r?;
                let doc: TantivyDocument = searcher.doc(a)?;
                Ok(read_stored(f, &doc, score))
            }));
            match known {
                Some((t, exact, origin)) => (page, Some(t), exact, origin),
                None => (page, None, false, TotalOrigin::Bound),
            }
        };
    let retrieve_ms = t1.elapsed().as_secs_f64() * 1000.0 - verify_ms;
    let t2 = Instant::now();
    let (hits, facets, consumed, more) = build_hits(
        index,
        catalog,
        req,
        &searcher,
        page,
        limit,
        root.as_ref(),
        &content_sets,
        opts,
    )?;
    let join_ms = t2.elapsed().as_secs_f64() * 1000.0;
    let (mut completeness, index_lag) =
        completeness_for(catalog, &in_scope.iter().map(|s| s.id).collect::<Vec<_>>())?;
    let content_index_rebuilding = content_index.and_then(|c| c.content_incomplete_reason());
    if content_index_rebuilding.is_some() {
        for c in completeness.iter_mut() {
            c.content_complete = false;
        }
    }
    // Advance by candidates consumed, not hits returned: stale documents
    // that produced no hit are never re-examined by the next page, and a
    // page of nothing but stale documents still makes progress.
    let consumed_total = offset + consumed;
    let (total, total_exact) = match total {
        Some(t) => (t, total_exact),
        // Not counted: what this walk has seen so far is a lower bound.
        None => (consumed_total as u64 + u64::from(more), false),
    };
    let has_more = match total_origin {
        TotalOrigin::Bound => more,
        _ => consumed_total < total as usize,
    };
    let next_cursor = if has_more {
        Some(encode_cursor(
            consumed_total,
            index_generation,
            fingerprint,
            (total_origin != TotalOrigin::Bound).then_some((total, total_exact)),
            index.cursor_key(),
        ))
    } else {
        None
    };
    let coverage = CoverageEnvelope::derive(
        &completeness,
        &ResponseSignals {
            index_lag,
            content_query: has_content,
            content_index_rebuilding,
            total_is_bound: !total_exact,
        },
        UnixNanos::now(),
    );
    Ok(SearchResponse {
        schema_version: SCHEMA_VERSION,
        hits,
        next_cursor,
        total: TotalCount {
            value: total,
            exact: total_exact,
            origin: total_origin,
        },
        timing: Timing {
            total_ms: t0.elapsed().as_secs_f64() * 1000.0,
            plan_ms,
            retrieve_ms,
            verify_ms,
            join_ms,
        },
        completeness,
        coverage,
        explanation: if req.explain {
            Some(Explanation {
                readable: ctx.readable.join(" AND "),
                steps,
            })
        } else {
            None
        },
        facets,
        warnings,
    })
}

/// Join candidates with the catalog in page order until `limit` hits are
/// assembled or the candidates run out. Returns the hits, facets, the
/// number of candidates consumed (stale ones included) so the cursor can
/// advance past everything this page examined, and whether at least one
/// more candidate exists beyond them.
#[allow(clippy::too_many_arguments)]
fn build_hits(
    index: &CatalogIndex,
    catalog: &Catalog,
    req: &SearchRequest,
    searcher: &Searcher,
    candidates: Candidates<'_>,
    limit: usize,
    facet_query: &dyn TQuery,
    content_sets: &[ContentSet],
    opts: &ExecOptions,
) -> Result<(Vec<Hit>, Vec<Facet>, usize, bool)> {
    let sources: HashMap<SourceId, eidos_catalog::SourceRecord> = catalog
        .list_sources()?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let mut hits = Vec::with_capacity(limit);
    let mut consumed = 0usize;
    let mut candidates = candidates.peekable();
    let mut more = false;
    for s in candidates.by_ref() {
        if hits.len() >= limit {
            more = true;
            break;
        }
        let s = s?;
        consumed += 1;
        let obj = match catalog.get_object(s.object_id)? {
            Some(o) if o.deleted_at.is_none() => o,
            _ => continue, // index slightly ahead/behind the catalog
        };
        let src = sources.get(&s.source_id);
        let directory = if obj.kind.is_directory_like() {
            catalog.directory_aggregate(s.object_id)?.map(|a| {
                let ext: BTreeMap<String, u64> = catalog
                    .extension_counts(s.object_id, 8)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| (e.extension, e.count))
                    .collect();
                DirectorySummary {
                    file_count: a.file_count,
                    directory_count: a.dir_count,
                    logical_bytes: a.logical_bytes,
                    allocated_bytes: a.allocated_bytes,
                    newest_modified: a.newest_modified,
                    oldest_modified: a.oldest_modified,
                    extension_counts: ext,
                    complete: a.complete,
                }
            })
        } else {
            None
        };
        let (content, snippets) = if obj.kind == ObjectKind::File {
            let rec = catalog.content_record(s.object_id)?;
            let summary = ContentSummary {
                state: obj.content_state,
                coverage: rec
                    .as_ref()
                    .filter(|r| r.generation == obj.generation)
                    .map(|r| r.coverage)
                    .unwrap_or(Coverage::None),
                // The generation the stored chunks belong to, which may lag
                // the object when a change is waiting for re-extraction.
                generation: rec.as_ref().map(|r| r.generation),
                indexed_bytes: rec
                    .as_ref()
                    .filter(|r| r.generation == obj.generation)
                    .map(|r| r.indexed_bytes),
                content_id: obj.content_id.map(|c| c.to_hex()),
                reason: rec
                    .as_ref()
                    .and_then(|r| r.reason.clone().or_else(|| r.error.clone())),
            };
            let snippets = if content_sets.is_empty() || !req.snippets {
                Vec::new()
            } else {
                snippets_for(catalog, content_sets, s.object_id, obj.generation, opts)?
            };
            (summary, snippets)
        } else {
            (ContentSummary::not_applicable(), Vec::new())
        };
        hits.push(Hit {
            object_id: s.object_id,
            entry_id: Some(EntryId(s.entry_id)),
            source_id: s.source_id,
            host_id: src.map(|x| x.host_id).unwrap_or(HostId(1)),
            kind: obj.kind,
            name: s.name,
            path: Some(s.path),
            parent_id: s.parent_id,
            extension: s.extension,
            size: obj.size,
            allocated_size: obj.allocated,
            modified: obj.modified,
            created: obj.created,
            changed: obj.changed,
            attributes: obj.attributes,
            hard_link_count: obj.link_count,
            content,
            score: Some(s.score),
            snippets,
            directory,
            archive: None,
            source_state: src.map(|x| x.state).unwrap_or(SourceState::New),
        });
    }
    if !more {
        more = candidates.peek().is_some();
    }
    let facets = if req.facets.is_empty() {
        Vec::new()
    } else {
        facets_for(
            index,
            searcher,
            facet_query,
            &req.facets,
            req.mode,
            &sources,
            catalog,
        )?
    };
    Ok((hits, facets, consumed, more))
}

/// Run a top-k collector, with the exact match count only when asked for.
fn top_and_count<C: tantivy::collector::Collector>(
    searcher: &Searcher,
    query: &dyn TQuery,
    top: C,
    count: bool,
) -> Result<(C::Fruit, Option<usize>)> {
    if count {
        let (fruit, n) = searcher.search(query, &(top, Count))?;
        Ok((fruit, Some(n)))
    } else {
        Ok((searcher.search(query, &top)?, None))
    }
}

/// Diverse line-aware snippets for one file: the best-scoring chunks across
/// every content clause, one window per chunk around the first match.
fn snippets_for(
    catalog: &Catalog,
    sets: &[ContentSet],
    object: ObjectId,
    generation: u32,
    opts: &ExecOptions,
) -> Result<Vec<Snippet>> {
    let mut per_ordinal: BTreeMap<u32, f32> = BTreeMap::new();
    for set in sets {
        if let Some(m) = set.by_object.get(&object) {
            if m.generation != generation {
                continue; // stale: the object changed since retrieval
            }
            for (ord, score) in &m.chunks {
                *per_ordinal.entry(*ord).or_insert(0.0) += score;
            }
        }
    }
    if per_ordinal.is_empty() {
        return Ok(Vec::new());
    }
    let mut ranked: Vec<(u32, f32)> = per_ordinal.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked.truncate(opts.max_snippets.max(1));
    // Rows the page walk already fetched need no second trip to the store.
    let mut by_ordinal: BTreeMap<u32, ChunkRow> = BTreeMap::new();
    let mut missing: Vec<u32> = Vec::new();
    for (ord, _) in &ranked {
        let cached = sets.iter().find_map(|s| {
            s.by_object
                .get(&object)
                .filter(|m| m.generation == generation)
                .and_then(|m| m.rows.iter().find(|r| r.ordinal == *ord))
        });
        match cached {
            Some(r) => {
                by_ordinal.insert(*ord, r.clone());
            }
            None => missing.push(*ord),
        }
    }
    if !missing.is_empty() {
        for r in catalog.chunks_for(object, generation, &missing)? {
            by_ordinal.insert(r.ordinal, r);
        }
    }
    let matchers: Vec<&Matcher> = sets.iter().map(|s| &s.matcher).collect();
    let mut out = Vec::new();
    // Best-scoring chunk first, as ranked.
    for (ord, _) in &ranked {
        if let Some(row) = by_ordinal.get(ord) {
            if let Some(s) = make_snippet(row, &matchers, opts.snippet_chars) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

fn make_snippet(
    row: &eidos_catalog::content::ChunkRow,
    matchers: &[&Matcher],
    window_chars: usize,
) -> Option<Snippet> {
    let text = row.text.as_str();
    let mut matches: Vec<(usize, usize)> = Vec::new();
    for m in matchers {
        matches.extend(m.find(text, 32));
    }
    if matches.is_empty() {
        return None;
    }
    matches.sort();
    let (ms, me) = matches[0];
    // The line containing the first match.
    let line_start = text[..ms].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[ms..].find('\n').map(|i| ms + i).unwrap_or(text.len());
    let line_index = row.line_start + text[..line_start].matches('\n').count() as u64;
    // Bound the window around the match (char-aware).
    let mut ws = line_start;
    let mut we = line_end;
    if text[ws..we].chars().count() > window_chars {
        let before = window_chars / 3;
        let mut s = ms;
        let mut n = 0;
        while s > ws && n < before {
            s -= 1;
            while s > ws && !text.is_char_boundary(s) {
                s -= 1;
            }
            n += 1;
        }
        ws = s;
        let mut e = me.max(ms);
        let mut n = text[ws..e].chars().count();
        while e < we && n < window_chars {
            e += 1;
            while e < we && !text.is_char_boundary(e) {
                e += 1;
            }
            n += 1;
        }
        we = e;
    }
    let window = text[ws..we].trim_end_matches('\r');
    let we = ws + window.len();
    let char_at = |b: usize| text[ws..b].chars().count() as u32;
    let highlights: Vec<[u32; 2]> = matches
        .iter()
        .filter(|(s, e)| *s >= ws && *e <= we)
        .map(|(s, e)| [char_at(*s), char_at(*e)])
        .collect();
    Some(Snippet {
        chunk_ordinal: row.ordinal,
        byte_start: row.byte_start,
        byte_end: row.byte_end,
        line_start: line_index,
        line_end: line_index,
        text: window.to_string(),
        highlights,
    })
}

#[allow(clippy::too_many_arguments)]
fn facets_for(
    index: &CatalogIndex,
    searcher: &Searcher,
    query: &dyn TQuery,
    requests: &[FacetRequest],
    mode: ResultMode,
    sources: &HashMap<SourceId, eidos_catalog::SourceRecord>,
    catalog: &Catalog,
) -> Result<Vec<Facet>> {
    use tantivy::aggregation::agg_req::Aggregations;
    use tantivy::aggregation::AggregationCollector;
    let _ = index;
    let now = UnixNanos::now();
    // Bucket tables for the range facets, kept so the response can be built
    // from the same boundaries the aggregation was asked for.
    let mut tables: HashMap<FacetField, Vec<RangeBucket>> = HashMap::new();
    let mut spec = serde_json::Map::new();
    for r in requests {
        let size = r.limit.clamp(1, 500);
        let v = match r.field {
            FacetField::Source => {
                serde_json::json!({"terms": {"field": "source_id", "size": size}})
            }
            FacetField::Extension => {
                serde_json::json!({"terms": {"field": "extension", "size": size}})
            }
            FacetField::Kind => serde_json::json!({"terms": {"field": "kind", "size": size}}),
            FacetField::ContentState => {
                serde_json::json!({"terms": {"field": "content_state", "size": size}})
            }
            FacetField::TopDirectory => {
                serde_json::json!({"terms": {"field": "parent_id", "size": size}})
            }
            FacetField::SizeBucket => {
                let buckets = crate::facets::size_buckets(mode);
                let spec = range_spec("subtree_logical", &buckets);
                tables.insert(r.field, buckets);
                spec
            }
            FacetField::ModifiedBucket => {
                let buckets = crate::facets::time_buckets(now, mode);
                let spec = range_spec("newest_modified", &buckets);
                tables.insert(r.field, buckets);
                spec
            }
        };
        spec.insert(format!("{:?}", r.field).to_lowercase(), v);
    }
    let aggs: Aggregations = serde_json::from_value(serde_json::Value::Object(spec))
        .map_err(|e| SearchError::Other(format!("facet spec: {e}")))?;
    let context = tantivy::aggregation::AggContextParams {
        limits: Default::default(),
        tokenizers: index.index().tokenizers().clone(),
    };
    let collector = AggregationCollector::from_aggs(aggs, context);
    let result = searcher.search(query, &collector)?;
    let value = serde_json::to_value(&result).map_err(|e| SearchError::Other(e.to_string()))?;
    let mut out = Vec::new();
    for r in requests {
        let key = format!("{:?}", r.field).to_lowercase();
        let buckets = value
            .get(&key)
            .and_then(|v| v.get("buckets"))
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default();
        let truncated = value
            .get(&key)
            .and_then(|v| v.get("sum_other_doc_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0;
        let mut values = Vec::new();
        // Range facets are emitted from the bucket table, in its display
        // order, so every value carries the boundaries and clauses the
        // aggregation was built from.
        if let Some(table) = tables.remove(&r.field) {
            for bucket in table {
                let Some(b) = buckets.iter().find(|b| {
                    bound_eq(b.get("from"), bucket.from) && bound_eq(b.get("to"), bucket.to)
                }) else {
                    continue;
                };
                let count = b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
                if count == 0 {
                    continue;
                }
                values.push(FacetValue {
                    value: b
                        .get("key")
                        .and_then(|k| k.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    count,
                    label: Some(bucket.label),
                    range: bucket.range,
                });
            }
            out.push(Facet {
                field: r.field,
                values,
                truncated,
            });
            continue;
        }
        for b in buckets {
            let count = b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0);
            if count == 0 {
                continue;
            }
            let raw_key = b.get("key").cloned().unwrap_or(serde_json::Value::Null);
            let (value_s, label) = match r.field {
                FacetField::Source => {
                    let id = raw_key
                        .as_f64()
                        .map(|x| x as i64)
                        .or_else(|| raw_key.as_i64())
                        .unwrap_or(0);
                    (
                        id.to_string(),
                        sources.get(&SourceId(id)).map(|s| s.name.clone()),
                    )
                }
                FacetField::TopDirectory => {
                    let id = raw_key
                        .as_f64()
                        .map(|x| x as i64)
                        .or_else(|| raw_key.as_i64())
                        .unwrap_or(0);
                    (
                        id.to_string(),
                        catalog.render_path(ObjectId(id)).ok().flatten(),
                    )
                }
                _ => {
                    let s = raw_key.as_str().unwrap_or("").to_string();
                    let label = if s.is_empty() {
                        Some("(none)".to_string())
                    } else {
                        None
                    };
                    (s, label)
                }
            };
            values.push(FacetValue {
                value: value_s,
                count,
                label,
                range: None,
            });
        }
        out.push(Facet {
            field: r.field,
            values,
            truncated,
        });
    }
    Ok(out)
}

/// The aggregation request for one bucket table.
fn range_spec(field: &str, buckets: &[RangeBucket]) -> serde_json::Value {
    let ranges: Vec<serde_json::Value> = buckets
        .iter()
        .map(|b| {
            let mut m = serde_json::Map::new();
            if let Some(f) = b.from {
                m.insert("from".into(), (f as f64).into());
            }
            if let Some(t) = b.to {
                m.insert("to".into(), (t as f64).into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    serde_json::json!({"range": {"field": field, "ranges": ranges}})
}

/// Match a returned bucket boundary against the table it was built from.
/// Boundaries are whole bytes or UTC-midnight nanoseconds, both exactly
/// representable as `f64`, so the comparison is exact.
fn bound_eq(returned: Option<&serde_json::Value>, want: Option<i64>) -> bool {
    match (returned.and_then(|v| v.as_f64()), want) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b as f64,
        _ => false,
    }
}

/// Per-source completeness for the sources in scope, plus the number of
/// outbox rows the index follower has not applied yet (index lag).
fn completeness_for(
    catalog: &Catalog,
    scope: &[SourceId],
) -> Result<(Vec<SourceCompleteness>, u64)> {
    let mut out = Vec::new();
    let pending_outbox = catalog.outbox_pending()?;
    for sid in scope {
        let mut c = catalog.source_completeness(*sid)?;
        if let Some(src) = catalog.get_source(*sid)? {
            let built = catalog.projection_source(PROJECTION_NAME, *sid)?;
            match (src.published_generation, built) {
                (Some(g), Some(b)) if b.generation == g => {}
                (Some(_), Some(b)) => {
                    c.metadata_complete = false;
                    c.note = Some(format!(
                        "search index holds generation {}; catalog published a newer one — rebuilding",
                        b.generation
                    ));
                }
                (Some(_), None) => {
                    c.metadata_complete = false;
                    c.note = Some("search index has not been built for this source yet".into());
                }
                (None, _) => {}
            }
        }
        out.push(c);
    }
    Ok((out, pending_outbox))
}

/// Attribute names accepted by the query syntax.
pub fn attribute_bit(name: &str) -> Option<u32> {
    attr_bit(name)
}

#[cfg(test)]
mod cursor_property_tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, RngSeed};

    const KEY: [u8; 32] = [0x27; 32];

    fn signed(unsigned: &str) -> String {
        let signature = blake3::keyed_hash(&KEY, unsigned.as_bytes());
        format!("{unsigned};s:{signature}")
    }

    #[test]
    fn duplicate_fields_and_oversized_cursors_are_rejected() {
        for cursor in [
            "o:1;o:2".to_string(),
            signed("o:1;g:2;g:3;q:0000000000000004"),
            signed("o:1;g:2;q:0000000000000004;q:5"),
            signed("o:1;g:2;q:0000000000000004;t:6;t:7;x:1"),
            signed("o:1;g:2;q:0000000000000004;t:6;x:1;x:0"),
            format!("o:1{}", ";padding".repeat(MAX_CURSOR_BYTES)),
        ] {
            assert_eq!(
                decode_cursor(&Some(cursor), &KEY),
                Err(QueryError::InvalidCursor)
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 512,
            rng_seed: RngSeed::Fixed(0xe1d0_c027),
            max_shrink_iters: 20_000,
            ..ProptestConfig::default()
        })]

        #[test]
        fn structured_cursor_round_trips_numeric_extremes(
            offset in any::<usize>(),
            generation in any::<u64>(),
            fingerprint in any::<u64>(),
            total in prop::option::of((any::<u64>(), any::<bool>())),
        ) {
            let encoded = encode_cursor(offset, generation, fingerprint, total, &KEY);
            let decoded = decode_cursor(&Some(encoded), &KEY).expect("encoded cursor is valid");
            prop_assert_eq!(
                decoded,
                Cursor {
                    offset,
                    generation: Some(generation),
                    fingerprint: Some(fingerprint),
                    total,
                }
            );
        }

        #[test]
        fn changing_any_signed_payload_byte_is_rejected(
            offset in any::<usize>(),
            generation in any::<u64>(),
            fingerprint in any::<u64>(),
            index in any::<usize>(),
        ) {
            let encoded = encode_cursor(offset, generation, fingerprint, None, &KEY);
            let unsigned_len = encoded.find(";s:").expect("signature separator");
            let mut bytes = encoded.into_bytes();
            let at = index % unsigned_len;
            bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
            let changed = String::from_utf8(bytes).expect("cursor is ASCII");
            prop_assert_eq!(
                decode_cursor(&Some(changed), &KEY),
                Err(QueryError::InvalidCursor)
            );
        }

        #[test]
        fn arbitrary_bounded_cursor_text_never_panics(
            cursor in prop::collection::vec(any::<char>(), 0..=MAX_CURSOR_BYTES),
        ) {
            let cursor: String = cursor.into_iter().collect();
            let _ = decode_cursor(&Some(cursor), &KEY);
        }
    }
}
