//! Query execution: typed AST → Tantivy query → verified, joined results.
//!
//! Execution follows ARCHITECTURE 10: validate, resolve scope, retrieve
//! candidates, verify exact/case-sensitive clauses against stored originals,
//! join current state and completeness from the catalog, and explain.

use crate::schema::{attr_bit, attr_name, fold, Fields};
use crate::{CatalogIndex, Result, SearchError, PROJECTION_NAME};
use eidos_catalog::Catalog;
use eidos_domain::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::time::Instant;
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query as TQuery, RangeQuery,
    RegexQuery, TermQuery,
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
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            limits: QueryLimits::default(),
            oversample: 8,
            max_candidates: 100_000,
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

struct Ctx<'a> {
    f: &'a Fields,
    catalog: &'a Catalog,
    verifiers: Vec<Verifier>,
    steps: Vec<PlanStep>,
    readable: Vec<String>,
    warnings: Vec<String>,
    scope_sources: Option<Vec<SourceId>>,
    tight_dirs: bool,
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

fn regex_is_broad(pattern: &str) -> bool {
    use regex_syntax::hir::{Hir, HirKind};
    fn has_literal(h: &Hir) -> bool {
        match h.kind() {
            HirKind::Literal(_) => true,
            HirKind::Concat(v) => v.iter().any(has_literal),
            HirKind::Alternation(v) => v.iter().all(has_literal),
            HirKind::Capture(c) => has_literal(&c.sub),
            HirKind::Repetition(r) => r.min > 0 && has_literal(&r.sub),
            _ => false,
        }
    }
    match regex_syntax::parse(pattern) {
        Ok(h) => !has_literal(&h),
        Err(_) => true,
    }
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
            let mut qs = Vec::new();
            for c in clauses {
                qs.push(compile(c, ctx)?);
            }
            any_of(qs)
        }
        Query::Not { clause } => not(compile(clause, ctx)?),
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
            i64_range(fld, after.map(|t| t.0), before.map(|t| t.0))
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
        return Err(QueryError::Other {
            message: "content search is not available until the content index exists (Milestone 4); name and path clauses work".into(),
        });
    }
    let (tok_field, folded_field, label) = match field {
        TextField::Name => (f.name, f.name_folded, "name"),
        TextField::Path => (f.path_tokens, f.path_folded, "path"),
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
            let pat = format!(".*{}.*", regex::escape(&fold(value)));
            if case_sensitive {
                let v = value.to_string();
                ctx.verifiers
                    .push(Box::new(move |s, _| stored_of(s).contains(&v)));
            }
            ctx.steps.push(PlanStep {
                stage: "candidates".into(),
                description: format!("substring automaton over the {label} term dictionary"),
                candidates: None,
                verified: None,
                elapsed_ms: None,
            });
            make_regex(folded_field, &pat)?
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
            // The dictionary is folded, so candidates are always matched
            // case-insensitively; case sensitivity is enforced by the verifier.
            let fst = fst_regex(value, false)?;
            if regex_is_broad(value) {
                ctx.warnings.push(format!(
                    "regex /{value}/ has no required literal; it scans the whole {label} dictionary"
                ));
            }
            if case_sensitive {
                let re = regex::RegexBuilder::new(value)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| QueryError::InvalidRegex {
                        message: e.to_string(),
                    })?;
                ctx.verifiers
                    .push(Box::new(move |s, _| re.is_match(&stored_of(s))));
            }
            ctx.steps.push(PlanStep {
                stage: "candidates".into(),
                description: format!("regex automaton over the {label} term dictionary"),
                candidates: None,
                verified: None,
                elapsed_ms: None,
            });
            make_regex(folded_field, &fst)?
        }
    })
}

fn compile_path(
    mode: PathMode,
    value: &str,
    case_sensitive: bool,
    ctx: &mut Ctx<'_>,
) -> std::result::Result<Box<dyn TQuery>, QueryError> {
    let f = ctx.f;
    let norm = value.replace('/', "\\");
    Ok(match mode {
        PathMode::Exact => {
            ctx.readable.push(format!("path is \"{norm}\""));
            if case_sensitive {
                let v = norm.clone();
                ctx.verifiers.push(Box::new(move |s, _| s.path == v));
            }
            term_text(f.path_folded, &fold(&norm))
        }
        PathMode::Prefix => {
            let trimmed = norm.trim_end_matches('\\').to_string();
            ctx.readable.push(format!("path under \"{trimmed}\""));
            let pat = format!("{}(\\\\.*)?", regex::escape(&fold(&trimmed)));
            if case_sensitive {
                let v = trimmed.clone();
                ctx.verifiers.push(Box::new(move |s, _| {
                    s.path == v || s.path.starts_with(&format!("{v}\\"))
                }));
            }
            make_regex(f.path_folded, &pat)?
        }
        PathMode::Glob => {
            ctx.readable.push(format!("path matches glob \"{norm}\""));
            let re = glob_to_regex(&fold(&norm));
            let pat = if norm.contains('\\') {
                re
            } else {
                format!(".*{re}")
            };
            make_regex(f.path_folded, &pat)?
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

fn decode_cursor(c: &Option<String>) -> std::result::Result<usize, QueryError> {
    match c {
        None => Ok(0),
        Some(s) => s
            .strip_prefix("o:")
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or(QueryError::InvalidCursor),
    }
}

/// Execute a search request.
pub fn search(
    index: &CatalogIndex,
    catalog: &Catalog,
    req: &SearchRequest,
    opts: &ExecOptions,
) -> Result<SearchResponse> {
    let t0 = Instant::now();
    req.validate(&opts.limits)?;
    let offset = decode_cursor(&req.cursor)?;
    let limit = req.limit.max(1) as usize;
    let f = index.fields();
    let mut ctx = Ctx {
        f,
        catalog,
        verifiers: Vec::new(),
        steps: Vec::new(),
        readable: Vec::new(),
        warnings: Vec::new(),
        scope_sources: None,
        tight_dirs: false,
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
    let all_sources = catalog.list_sources()?;
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
    let collect_all = !ctx.verifiers.is_empty() || tight;
    let searcher = index.searcher();
    let t1 = Instant::now();
    let mut warnings = ctx.warnings.clone();
    let mut steps = ctx.steps.clone();
    let mut verify_ms = 0.0;

    let (page, total, total_exact): (Vec<Stored>, u64, bool) = if collect_all {
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
        let mut stored: Vec<Stored> = Vec::with_capacity(list.len());
        for a in list {
            let doc: TantivyDocument = searcher.doc(a)?;
            stored.push(read_stored(f, &doc, 1.0));
        }
        let tv = Instant::now();
        let mut verified: Vec<Stored> = stored
            .into_iter()
            .filter(|s| ctx.verifiers.iter().all(|v| v(s, catalog)))
            .collect();
        if tight {
            // Rank the tightest containers first: a directory is "loose" when
            // a candidate beneath it also satisfies the predicate.
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
        steps.push(PlanStep {
            stage: "verify".into(),
            description: format!(
                "{} candidates verified against stored originals",
                candidates
            ),
            candidates: Some(candidates),
            verified: Some(verified.len() as u64),
            elapsed_ms: Some(verify_ms),
        });
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
        let total = verified.len() as u64;
        let page: Vec<Stored> = verified.into_iter().skip(offset).take(limit).collect();
        (page, total, !truncated)
    } else {
        let n = (offset + limit).max(1);
        let top = TopDocs::with_limit(n);
        let order = if req.sort.descending {
            Order::Desc
        } else {
            Order::Asc
        };
        let (addrs, count): (Vec<(f32, DocAddress)>, usize) = match req.sort.field {
            SortField::Relevance => {
                let (hits, count) = searcher.search(&root, &(top.order_by_score(), Count))?;
                (hits, count)
            }
            SortField::Name | SortField::Path => {
                let field = if req.sort.field == SortField::Name {
                    "name_folded"
                } else {
                    "path_folded"
                };
                let (hits, count) = searcher.search(
                    &root,
                    &(top.order_by_string_fast_field(field, order), Count),
                )?;
                (hits.into_iter().map(|(_, a)| (1.0, a)).collect(), count)
            }
            SortField::Modified | SortField::Created => {
                let field = if req.sort.field == SortField::Modified {
                    "newest_modified"
                } else {
                    "created"
                };
                let (hits, count) = searcher.search(
                    &root,
                    &(top.order_by_fast_field::<i64>(field, order), Count),
                )?;
                (hits.into_iter().map(|(_, a)| (1.0, a)).collect(), count)
            }
            SortField::Size | SortField::SubtreeSize | SortField::AllocatedSize => {
                let field = if req.sort.field == SortField::AllocatedSize {
                    "subtree_allocated"
                } else {
                    "subtree_logical"
                };
                let (hits, count) = searcher.search(
                    &root,
                    &(top.order_by_fast_field::<u64>(field, order), Count),
                )?;
                (hits.into_iter().map(|(_, a)| (1.0, a)).collect(), count)
            }
        };
        steps.push(PlanStep {
            stage: "retrieve".into(),
            description: match req.sort.field {
                SortField::Relevance => "BM25 ranked retrieval, top-k".into(),
                other => format!("fast-field sorted retrieval by {other:?}, top-k"),
            },
            candidates: Some(count as u64),
            verified: None,
            elapsed_ms: None,
        });
        let mut page = Vec::with_capacity(limit);
        for (score, a) in addrs.into_iter().skip(offset) {
            let doc: TantivyDocument = searcher.doc(a)?;
            page.push(read_stored(f, &doc, score));
        }
        (page, count as u64, true)
    };
    let retrieve_ms = t1.elapsed().as_secs_f64() * 1000.0 - verify_ms;
    let t2 = Instant::now();
    let (hits, facets) = build_hits(index, catalog, req, &searcher, page, root.as_ref())?;
    let join_ms = t2.elapsed().as_secs_f64() * 1000.0;
    let completeness = completeness_for(
        catalog,
        &in_scope.iter().map(|s| s.id).collect::<Vec<_>>(),
        &mut warnings,
    )?;
    let next_cursor = if (offset + hits.len()) < total as usize {
        Some(format!("o:{}", offset + hits.len()))
    } else {
        None
    };
    Ok(SearchResponse {
        schema_version: SCHEMA_VERSION,
        hits,
        next_cursor,
        total: TotalCount {
            value: total,
            exact: total_exact,
        },
        timing: Timing {
            total_ms: t0.elapsed().as_secs_f64() * 1000.0,
            plan_ms,
            retrieve_ms,
            verify_ms,
            join_ms,
        },
        completeness,
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

fn build_hits(
    index: &CatalogIndex,
    catalog: &Catalog,
    req: &SearchRequest,
    searcher: &Searcher,
    page: Vec<Stored>,
    facet_query: &dyn TQuery,
) -> Result<(Vec<Hit>, Vec<Facet>)> {
    let sources: HashMap<SourceId, eidos_catalog::SourceRecord> = catalog
        .list_sources()?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let mut hits = Vec::with_capacity(page.len());
    for s in page {
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
        let content = if obj.kind == ObjectKind::File {
            ContentSummary {
                state: obj.content_state,
                coverage: Coverage::None,
                indexed_bytes: None,
                content_id: obj.content_id.map(|c| c.to_hex()),
                reason: None,
            }
        } else {
            ContentSummary::not_applicable()
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
            snippets: Vec::new(),
            directory,
            archive: None,
            source_state: src.map(|x| x.state).unwrap_or(SourceState::New),
        });
    }
    let facets = if req.facets.is_empty() {
        Vec::new()
    } else {
        facets_for(index, searcher, facet_query, &req.facets, &sources, catalog)?
    };
    Ok((hits, facets))
}

fn facets_for(
    index: &CatalogIndex,
    searcher: &Searcher,
    query: &dyn TQuery,
    requests: &[FacetRequest],
    sources: &HashMap<SourceId, eidos_catalog::SourceRecord>,
    catalog: &Catalog,
) -> Result<Vec<Facet>> {
    use tantivy::aggregation::agg_req::Aggregations;
    use tantivy::aggregation::AggregationCollector;
    let _ = index;
    let now = UnixNanos::now().0;
    let day = 86_400_000_000_000i64;
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
                serde_json::json!({"range": {"field": "subtree_logical", "ranges": [
                {"to": 4096.0}, {"from": 4096.0, "to": 65536.0}, {"from": 65536.0, "to": 1048576.0},
                {"from": 1048576.0, "to": 16777216.0}, {"from": 16777216.0, "to": 268435456.0},
                {"from": 268435456.0, "to": 1073741824.0}, {"from": 1073741824.0}]}})
            }
            FacetField::ModifiedBucket => {
                serde_json::json!({"range": {"field": "newest_modified", "ranges": [
                {"from": (now - day) as f64}, {"from": (now - 7 * day) as f64, "to": (now - day) as f64},
                {"from": (now - 30 * day) as f64, "to": (now - 7 * day) as f64},
                {"from": (now - 365 * day) as f64, "to": (now - 30 * day) as f64}, {"to": (now - 365 * day) as f64}]}})
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
                FacetField::SizeBucket | FacetField::ModifiedBucket => {
                    let from = b.get("from").and_then(|v| v.as_f64());
                    let to = b.get("to").and_then(|v| v.as_f64());
                    (
                        raw_key.as_str().unwrap_or("").to_string(),
                        Some(bucket_label(r.field, from, to, now)),
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

fn bucket_label(field: FacetField, from: Option<f64>, to: Option<f64>, now: i64) -> String {
    match field {
        FacetField::SizeBucket => {
            let h = |v: f64| -> String {
                let b = v as u64;
                if b >= 1 << 30 {
                    format!("{}G", b >> 30)
                } else if b >= 1 << 20 {
                    format!("{}M", b >> 20)
                } else if b >= 1 << 10 {
                    format!("{}K", b >> 10)
                } else {
                    format!("{b}")
                }
            };
            match (from, to) {
                (None, Some(t)) => format!("< {}", h(t)),
                (Some(f), Some(t)) => format!("{} – {}", h(f), h(t)),
                (Some(f), None) => format!("≥ {}", h(f)),
                _ => "all".into(),
            }
        }
        FacetField::ModifiedBucket => {
            let days = |v: f64| ((now as f64 - v) / 86_400_000_000_000.0).round() as i64;
            match (from, to) {
                (Some(f), None) => format!("last {} day(s)", days(f)),
                (Some(f), Some(t)) => format!("{}–{} days ago", days(t), days(f)),
                (None, Some(t)) => format!("older than {} days", days(t)),
                _ => "all".into(),
            }
        }
        _ => String::new(),
    }
}

fn completeness_for(
    catalog: &Catalog,
    scope: &[SourceId],
    warnings: &mut Vec<String>,
) -> Result<Vec<SourceCompleteness>> {
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
    if pending_outbox > 0 {
        warnings.push(format!(
            "{pending_outbox} catalog changes are not yet reflected in the search index"
        ));
    }
    Ok(out)
}

/// Attribute names accepted by the query syntax.
pub fn attribute_bit(name: &str) -> Option<u32> {
    attr_bit(name)
}
