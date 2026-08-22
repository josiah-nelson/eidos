//! Content index: one Tantivy document per stored chunk.
//!
//! Two indexed fields carry retrieval: `text` (tokenised, positions, BM25)
//! for ranked/phrase/proximity clauses, and `trigrams` (folded character
//! trigrams, doc ids only) for substring/exact/regex candidate retrieval.
//! Nothing textual is stored here — the catalog's `chunks` table owns the
//! original text, and every exact, case-sensitive, or regex clause is
//! verified against it (ARCHITECTURE invariant 10).

use crate::{Result, SearchError};
use eidos_catalog::Catalog;
use eidos_content::Chunk;
use eidos_domain::{ObjectId, SourceId, TextMode};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, PhraseQuery, Query as TQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

pub const CONTENT_SCHEMA_VERSION: u32 = 1;
pub const TRIGRAM_TOKENIZER: &str = "eidos_trigram";
pub const TEXT_TOKENIZER: &str = "eidos_text";
const META_FILE: &str = "eidos-content-schema.json";
/// Tokens longer than this are not indexed in `text` (base64 blobs, hashes).
pub const MAX_TOKEN_CHARS: usize = 64;
/// Threads used to verify candidate chunks against stored text.
pub const VERIFY_THREADS: usize = 8;

#[derive(Debug, Clone)]
pub struct ContentFields {
    pub object_id: Field,
    pub source_id: Field,
    pub generation: Field,
    pub ordinal: Field,
    pub text: Field,
    pub trigrams: Field,
}

pub fn build_schema() -> (Schema, ContentFields) {
    let mut b = Schema::builder();
    let id = NumericOptions::default()
        .set_indexed()
        .set_fast()
        .set_stored();
    let fast = NumericOptions::default().set_fast().set_stored();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TEXT_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let trigrams = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TRIGRAM_TOKENIZER)
            .set_index_option(IndexRecordOption::Basic),
    );
    let fields = ContentFields {
        object_id: b.add_u64_field("object_id", id.clone()),
        source_id: b.add_u64_field("source_id", id),
        generation: b.add_u64_field("generation", fast.clone()),
        ordinal: b.add_u64_field("ordinal", fast),
        text: b.add_text_field("text", text),
        trigrams: b.add_text_field("trigrams", trigrams),
    };
    (b.build(), fields)
}

// ----- trigram tokenizer ---------------------------------------------------

/// Folded character trigrams over the whole text (whitespace and
/// punctuation included, so `"a b"` and `"a-b"` are distinguishable).
#[derive(Clone, Default)]
pub struct TrigramTokenizer;

pub struct TrigramStream {
    folded: Vec<char>,
    idx: usize,
    token: Token,
}

impl Tokenizer for TrigramTokenizer {
    type TokenStream<'a> = TrigramStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> TrigramStream {
        TrigramStream {
            folded: fold_chars(text),
            idx: 0,
            token: Token::default(),
        }
    }
}

impl TokenStream for TrigramStream {
    fn advance(&mut self) -> bool {
        if self.idx + 3 > self.folded.len() {
            return false;
        }
        self.token.text.clear();
        self.token
            .text
            .extend(self.folded[self.idx..self.idx + 3].iter());
        self.token.offset_from = self.idx;
        self.token.offset_to = self.idx + 3;
        self.token.position = self.idx;
        self.token.position_length = 1;
        self.idx += 1;
        true
    }
    fn token(&self) -> &Token {
        &self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// Case fold one char at a time (first lowercase mapping), so positions
/// stay aligned with the original characters.
pub fn fold_chars(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Distinct folded trigrams of `text` (query side; must match the tokenizer).
pub fn trigrams(text: &str) -> Vec<String> {
    let f = fold_chars(text);
    let mut out: Vec<String> = Vec::new();
    if f.len() < 3 {
        return out;
    }
    for w in f.windows(3) {
        let t: String = w.iter().collect();
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

pub fn text_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_CHARS))
        .filter(LowerCaser)
        .build()
}

/// Query-side tokenisation matching `text_analyzer`.
pub fn text_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_TOKEN_CHARS)
        .map(|t| t.to_lowercase())
        .collect()
}

// ----- index ---------------------------------------------------------------

pub struct ContentIndex {
    dir: PathBuf,
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: ContentFields,
    /// Documents added since the last commit.
    uncommitted: AtomicU64,
    commits: AtomicU64,
}

impl std::fmt::Debug for ContentIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentIndex")
            .field("dir", &self.dir)
            .finish()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    schema_version: u32,
}

pub fn register_tokenizers(index: &Index) {
    index
        .tokenizers()
        .register(TRIGRAM_TOKENIZER, TrigramTokenizer);
    index.tokenizers().register(TEXT_TOKENIZER, text_analyzer());
}

impl ContentIndex {
    /// Open (or create / recreate on schema change) the content index.
    pub fn open(dir: impl AsRef<Path>) -> Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let (schema, fields) = build_schema();
        let meta_path = dir.join(META_FILE);
        let current: Option<Meta> = std::fs::read(&meta_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let needs_create = match current {
            Some(ref m) if m.schema_version == CONTENT_SCHEMA_VERSION => {
                !dir.join("meta.json").exists()
            }
            _ => true,
        };
        if needs_create {
            if dir.join("meta.json").exists() || current.is_some() {
                tracing::warn!(dir = %dir.display(), "recreating content index (schema changed or missing)");
                for entry in std::fs::read_dir(&dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            Index::create_in_dir(&dir, schema.clone())?;
            std::fs::write(
                &meta_path,
                serde_json::to_vec(&Meta {
                    schema_version: CONTENT_SCHEMA_VERSION,
                })
                .expect("meta"),
            )?;
        }
        let index = Index::open_in_dir(&dir)?;
        register_tokenizers(&index);
        let writer = index.writer_with_num_threads(4, 256 * 1024 * 1024)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Arc::new(Self {
            dir,
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
            uncommitted: AtomicU64::new(0),
            commits: AtomicU64::new(0),
        }))
    }

    /// Whether the content index was just (re)created: callers re-extract
    /// everything in that case.
    pub fn is_fresh(&self) -> bool {
        self.num_docs() == 0
    }

    pub fn fields(&self) -> &ContentFields {
        &self.fields
    }
    pub fn index(&self) -> &Index {
        &self.index
    }
    pub fn searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn uncommitted(&self) -> u64 {
        self.uncommitted.load(Ordering::Relaxed)
    }
    pub fn commits(&self) -> u64 {
        self.commits.load(Ordering::Relaxed)
    }

    /// Queue deletion of every chunk document of an object (all
    /// generations). Takes effect at the next commit, ordered before any
    /// documents added afterwards.
    pub fn delete_object(&self, object: ObjectId) {
        let f = &self.fields;
        self.writer()
            .delete_term(Term::from_field_u64(f.object_id, object.0 as u64));
    }

    pub fn delete_source(&self, source: SourceId) -> Result<()> {
        let f = &self.fields;
        self.writer()
            .delete_term(Term::from_field_u64(f.source_id, source.0 as u64));
        self.commit()?;
        Ok(())
    }

    /// Queue chunk documents for an object generation.
    pub fn add_chunks(
        &self,
        object: ObjectId,
        source: SourceId,
        generation: u32,
        chunks: &[Chunk],
    ) -> Result<()> {
        let f = &self.fields;
        let writer = self.writer();
        for c in chunks {
            let mut d = TantivyDocument::new();
            d.add_u64(f.object_id, object.0 as u64);
            d.add_u64(f.source_id, source.0 as u64);
            d.add_u64(f.generation, generation as u64);
            d.add_u64(f.ordinal, c.ordinal as u64);
            d.add_text(f.text, &c.text);
            d.add_text(f.trigrams, &c.text);
            writer.add_document(d)?;
        }
        self.uncommitted
            .fetch_add(chunks.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Commit queued adds/deletes and make them visible. Returns the number
    /// of documents added since the previous commit.
    pub fn commit(&self) -> Result<u64> {
        let n = self.uncommitted.swap(0, Ordering::Relaxed);
        self.writer().commit()?;
        self.reader.reload()?;
        self.commits.fetch_add(1, Ordering::Relaxed);
        Ok(n)
    }

    fn writer(&self) -> parking_lot::MutexGuard<'_, IndexWriter> {
        self.writer.lock()
    }
}

// ----- retrieval -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ContentClause {
    pub mode: TextMode,
    pub value: String,
    pub case_sensitive: bool,
    pub slop: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ContentOpts {
    /// Top-k chunks for ranked/phrase clauses.
    pub top_k: usize,
    /// Candidate chunks examined for verified clauses.
    pub max_verify: usize,
}

impl Default for ContentOpts {
    fn default() -> Self {
        Self {
            top_k: 5_000,
            max_verify: 20_000,
        }
    }
}

/// How a verified clause is matched against original chunk text; also used
/// for snippet highlighting of every clause kind.
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Folded whole tokens (ranked/phrase/proximity).
    Terms(Vec<String>),
    Literal {
        value: String,
        case_sensitive: bool,
        whole_word: bool,
    },
    Regex(regex::Regex),
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Matcher {
    /// Byte ranges of matches in `text` (at most `limit`).
    pub fn find(&self, text: &str, limit: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        match self {
            Matcher::Terms(terms) => {
                let mut start: Option<usize> = None;
                for (i, c) in text
                    .char_indices()
                    .chain(std::iter::once((text.len(), ' ')))
                {
                    if c.is_alphanumeric() {
                        if start.is_none() {
                            start = Some(i);
                        }
                    } else if let Some(s) = start.take() {
                        let w = &text[s..i];
                        if w.chars().count() <= MAX_TOKEN_CHARS {
                            let lw = w.to_lowercase();
                            if terms.contains(&lw) {
                                out.push((s, i));
                                if out.len() >= limit {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Matcher::Literal {
                value,
                case_sensitive,
                whole_word,
            } => {
                if value.is_empty() {
                    return out;
                }
                let check_word = |s: usize, e: usize| -> bool {
                    if !whole_word {
                        return true;
                    }
                    let before = text[..s].chars().next_back().is_some_and(is_word);
                    let after = text[e..].chars().next().is_some_and(is_word);
                    !before && !after
                };
                if *case_sensitive {
                    let mut from = 0;
                    while let Some(pos) = text[from..].find(value.as_str()) {
                        let s = from + pos;
                        let e = s + value.len();
                        if check_word(s, e) {
                            out.push((s, e));
                            if out.len() >= limit {
                                break;
                            }
                        }
                        from = s + value.chars().next().map_or(1, |c| c.len_utf8());
                    }
                } else {
                    let needle = fold_chars(value);
                    let hay: Vec<(usize, char)> = text
                        .char_indices()
                        .map(|(i, c)| (i, c.to_lowercase().next().unwrap_or(c)))
                        .collect();
                    if hay.len() < needle.len() {
                        return out;
                    }
                    let mut i = 0;
                    while i + needle.len() <= hay.len() {
                        if hay[i..i + needle.len()]
                            .iter()
                            .zip(needle.iter())
                            .all(|((_, a), b)| a == b)
                        {
                            let s = hay[i].0;
                            let e = hay
                                .get(i + needle.len())
                                .map(|(b, _)| *b)
                                .unwrap_or(text.len());
                            if check_word(s, e) {
                                out.push((s, e));
                                if out.len() >= limit {
                                    break;
                                }
                            }
                        }
                        i += 1;
                    }
                }
            }
            Matcher::Regex(re) => {
                for m in re.find_iter(text) {
                    if m.end() > m.start() {
                        out.push((m.start(), m.end()));
                    }
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out
    }

    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Matcher::Regex(re) => re.is_match(text),
            _ => !self.find(text, 1).is_empty(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkHit {
    pub object_id: ObjectId,
    pub generation: u32,
    pub ordinal: u32,
    pub score: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Retrieval {
    pub hits: Vec<ChunkHit>,
    pub candidates: u64,
    pub verified: Option<u64>,
    /// Candidate or top-k cap reached: the hit list is a subset.
    pub truncated: bool,
    /// No selective term existed; every chunk in scope was a candidate.
    pub broad: bool,
    pub description: String,
    pub elapsed_ms: f64,
}

/// Required literal fragments of a regex (all must occur in any match).
pub fn required_literals(pattern: &str) -> Vec<String> {
    use regex_syntax::hir::{Hir, HirKind};
    fn walk(h: &Hir, out: &mut Vec<String>) {
        match h.kind() {
            HirKind::Literal(l) => {
                if let Ok(s) = std::str::from_utf8(&l.0) {
                    out.push(s.to_string());
                }
            }
            HirKind::Concat(v) => v.iter().for_each(|c| walk(c, out)),
            HirKind::Capture(c) => walk(&c.sub, out),
            HirKind::Repetition(r) if r.min > 0 => walk(&r.sub, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(h) = regex_syntax::parse(pattern) {
        walk(&h, &mut out);
    }
    out
}

fn term_u64(f: Field, v: u64) -> Box<dyn TQuery> {
    Box::new(TermQuery::new(
        Term::from_field_u64(f, v),
        IndexRecordOption::Basic,
    ))
}
fn term_text(f: Field, v: &str) -> Box<dyn TQuery> {
    Box::new(TermQuery::new(
        Term::from_field_text(f, v),
        IndexRecordOption::WithFreqsAndPositions,
    ))
}

type IdColumns = (
    tantivy::columnar::Column<u64>,
    tantivy::columnar::Column<u64>,
    tantivy::columnar::Column<u64>,
);

struct IdReader<'a> {
    searcher: &'a tantivy::Searcher,
    cols: HashMap<u32, IdColumns>,
}

impl<'a> IdReader<'a> {
    fn new(searcher: &'a tantivy::Searcher) -> Self {
        Self {
            searcher,
            cols: HashMap::new(),
        }
    }
    fn read(&mut self, addr: DocAddress, score: f32) -> Result<ChunkHit> {
        let seg = addr.segment_ord;
        if !self.cols.contains_key(&seg) {
            let reader = self.searcher.segment_reader(seg);
            let ff = reader.fast_fields();
            let cols = (
                ff.u64("object_id")?,
                ff.u64("generation")?,
                ff.u64("ordinal")?,
            );
            self.cols.insert(seg, cols);
        }
        let (o, g, ord) = self.cols.get(&seg).expect("inserted");
        Ok(ChunkHit {
            object_id: ObjectId(o.first(addr.doc_id).unwrap_or(0) as i64),
            generation: g.first(addr.doc_id).unwrap_or(0) as u32,
            ordinal: ord.first(addr.doc_id).unwrap_or(0) as u32,
            score,
        })
    }
}

/// Build the matcher for a clause (used for verification and snippets).
pub fn matcher_for(
    clause: &ContentClause,
) -> std::result::Result<Matcher, eidos_domain::QueryError> {
    Ok(match clause.mode {
        TextMode::Ranked | TextMode::Phrase | TextMode::Proximity => {
            Matcher::Terms(text_tokens(&clause.value))
        }
        TextMode::Exact => Matcher::Literal {
            value: clause.value.clone(),
            case_sensitive: clause.case_sensitive,
            whole_word: true,
        },
        TextMode::Substring => Matcher::Literal {
            value: clause.value.clone(),
            case_sensitive: clause.case_sensitive,
            whole_word: false,
        },
        TextMode::Regex => Matcher::Regex(
            regex::RegexBuilder::new(&clause.value)
                .case_insensitive(!clause.case_sensitive)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| eidos_domain::QueryError::InvalidRegex {
                    message: e.to_string(),
                })?,
        ),
    })
}

/// Retrieve (and, where semantics require, verify) chunk hits for one
/// content clause. `scope` restricts sources; `exclude` removes sources.
pub fn retrieve(
    index: &ContentIndex,
    catalog: &Catalog,
    clause: &ContentClause,
    scope: Option<&[SourceId]>,
    exclude: &[SourceId],
    opts: &ContentOpts,
) -> Result<(Retrieval, Matcher)> {
    let started = Instant::now();
    let f = index.fields();
    let matcher = matcher_for(clause)?;
    let searcher = index.searcher();
    let mut ret = Retrieval::default();

    // Scope clauses.
    let mut scope_q: Vec<(Occur, Box<dyn TQuery>)> = Vec::new();
    if let Some(ids) = scope {
        if ids.is_empty() {
            ret.description = "no source in scope".into();
            return Ok((ret, matcher));
        }
        scope_q.push((
            Occur::Must,
            Box::new(BooleanQuery::new(
                ids.iter()
                    .map(|s| (Occur::Should, term_u64(f.source_id, s.0 as u64)))
                    .collect(),
            )),
        ));
    }
    for s in exclude {
        scope_q.push((Occur::MustNot, term_u64(f.source_id, s.0 as u64)));
    }
    let with_scope = |q: Box<dyn TQuery>| -> Box<dyn TQuery> {
        if scope_q.is_empty() {
            q
        } else {
            let mut v = scope_q.clone_boxed();
            v.push((Occur::Must, q));
            Box::new(BooleanQuery::new(v))
        }
    };

    match clause.mode {
        TextMode::Ranked | TextMode::Phrase | TextMode::Proximity => {
            let toks = text_tokens(&clause.value);
            if toks.is_empty() {
                ret.description = "no indexable terms".into();
                return Ok((ret, matcher));
            }
            let q: Box<dyn TQuery> = if clause.mode == TextMode::Ranked || toks.len() == 1 {
                Box::new(BooleanQuery::new(
                    toks.iter()
                        .map(|t| (Occur::Must, term_text(f.text, t)))
                        .collect(),
                ))
            } else {
                let terms: Vec<Term> = toks
                    .iter()
                    .map(|t| Term::from_field_text(f.text, t))
                    .collect();
                let mut pq = PhraseQuery::new(terms);
                if clause.mode == TextMode::Proximity {
                    pq.set_slop(clause.slop);
                }
                Box::new(pq)
            };
            let q = with_scope(q);
            let top =
                searcher.search(&q, &TopDocs::with_limit(opts.top_k.max(1)).order_by_score())?;
            ret.candidates = top.len() as u64;
            ret.truncated = top.len() >= opts.top_k;
            let mut ids = IdReader::new(&searcher);
            for (score, addr) in top {
                ret.hits.push(ids.read(addr, score)?);
            }
            ret.description = match clause.mode {
                TextMode::Ranked => format!(
                    "BM25 over chunk text for terms [{}], top {}",
                    toks.join(", "),
                    opts.top_k
                ),
                TextMode::Phrase => format!(
                    "phrase \"{}\" over chunk text, top {}",
                    toks.join(" "),
                    opts.top_k
                ),
                _ => format!(
                    "terms [{}] within {} positions, top {}",
                    toks.join(", "),
                    clause.slop,
                    opts.top_k
                ),
            };
        }
        TextMode::Exact | TextMode::Substring | TextMode::Regex => {
            // Candidate selection.
            let (cand_q, how): (Box<dyn TQuery>, String) = match clause.mode {
                TextMode::Regex => {
                    let lits = required_literals(&clause.value);
                    let mut tris: Vec<String> = Vec::new();
                    for l in &lits {
                        for t in trigrams(l) {
                            if !tris.contains(&t) {
                                tris.push(t);
                            }
                        }
                    }
                    if tris.is_empty() {
                        ret.broad = true;
                        (
                            Box::new(AllQuery),
                            "no required literal of 3+ characters; scanning every chunk in scope"
                                .into(),
                        )
                    } else {
                        let n = tris.len();
                        (
                            Box::new(BooleanQuery::new(
                                tris.iter()
                                    .map(|t| (Occur::Must, term_text(f.trigrams, t)))
                                    .collect(),
                            )),
                            format!("{n} required trigrams from literals [{}]", lits.join(", ")),
                        )
                    }
                }
                _ => {
                    let tris = trigrams(&clause.value);
                    if !tris.is_empty() {
                        let n = tris.len();
                        let mut clauses: Vec<(Occur, Box<dyn TQuery>)> = tris
                            .iter()
                            .map(|t| (Occur::Must, term_text(f.trigrams, t)))
                            .collect();
                        // A whole-word literal that is a single token must also
                        // appear as that token: far more selective than trigrams.
                        let toks = text_tokens(&clause.value);
                        let single_token = clause.mode == TextMode::Exact
                            && toks.len() == 1
                            && clause.value.chars().all(|c| c.is_alphanumeric());
                        if single_token {
                            clauses.push((Occur::Must, term_text(f.text, &toks[0])));
                        }
                        (
                            Box::new(BooleanQuery::new(clauses)),
                            if single_token {
                                format!("{n} folded trigrams + whole token")
                            } else {
                                format!("{n} folded trigrams")
                            },
                        )
                    } else {
                        let toks = text_tokens(&clause.value);
                        if toks.len() == 1 && clause.value.chars().all(|c| c.is_alphanumeric()) {
                            (
                                term_text(f.text, &toks[0]),
                                "short literal: whole-token term".into(),
                            )
                        } else {
                            ret.broad = true;
                            (
                                Box::new(AllQuery),
                                "literal shorter than 3 characters; scanning every chunk in scope"
                                    .into(),
                            )
                        }
                    }
                }
            };
            let q = with_scope(cand_q);
            let addrs = searcher.search(&q, &DocSetCollector)?;
            let mut list: Vec<DocAddress> = addrs.into_iter().collect();
            list.sort();
            ret.candidates = list.len() as u64;
            if list.len() > opts.max_verify {
                ret.truncated = true;
                list.truncate(opts.max_verify);
            }
            let mut ids = IdReader::new(&searcher);
            let mut cands: Vec<ChunkHit> = Vec::with_capacity(list.len());
            for a in list {
                cands.push(ids.read(a, 0.0)?);
            }
            // Verify against stored chunk text, grouped per object generation
            // and spread over a few threads (each uses a pooled reader).
            cands.sort_by_key(|c| (c.object_id.0, c.generation, c.ordinal));
            let mut groups: Vec<(ObjectId, u32, Vec<u32>)> = Vec::new();
            for c in &cands {
                match groups.last_mut() {
                    Some((o, g, ords)) if *o == c.object_id && *g == c.generation => {
                        ords.push(c.ordinal)
                    }
                    _ => groups.push((c.object_id, c.generation, vec![c.ordinal])),
                }
            }
            let threads = groups.len().clamp(1, VERIFY_THREADS);
            let per = groups.len().div_ceil(threads);
            let results: Vec<Result<Vec<ChunkHit>>> = std::thread::scope(|s| {
                let handles: Vec<_> = groups
                    .chunks(per.max(1))
                    .map(|part| {
                        let matcher = &matcher;
                        s.spawn(move || -> Result<Vec<ChunkHit>> {
                            let mut out = Vec::new();
                            for (obj, gen, ordinals) in part {
                                for row in catalog.chunks_for(*obj, *gen, ordinals)? {
                                    let n = matcher.find(&row.text, 64).len();
                                    if n > 0 {
                                        out.push(ChunkHit {
                                            object_id: *obj,
                                            generation: *gen,
                                            ordinal: row.ordinal,
                                            score: n as f32,
                                        });
                                    }
                                }
                            }
                            Ok(out)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("verification thread"))
                    .collect()
            });
            let mut verified = 0u64;
            for r in results {
                let hits = r?;
                verified += hits.len() as u64;
                ret.hits.extend(hits);
            }
            ret.verified = Some(verified);
            ret.description = format!("{how}; candidates verified against stored chunk text");
        }
    }
    ret.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok((ret, matcher))
}

trait CloneBoxed {
    fn clone_boxed(&self) -> Vec<(Occur, Box<dyn TQuery>)>;
}
impl CloneBoxed for Vec<(Occur, Box<dyn TQuery>)> {
    fn clone_boxed(&self) -> Vec<(Occur, Box<dyn TQuery>)> {
        self.iter().map(|(o, q)| (*o, q.box_clone())).collect()
    }
}

/// Convenience for tests and tools: every object with chunks in the index.
pub fn object_ids(index: &ContentIndex) -> Result<Vec<ObjectId>> {
    let searcher = index.searcher();
    let addrs = searcher.search(&AllQuery, &DocSetCollector)?;
    let mut ids = IdReader::new(&searcher);
    let mut out: Vec<ObjectId> = Vec::new();
    for a in addrs {
        let h = ids.read(a, 0.0)?;
        if !out.contains(&h.object_id) {
            out.push(h.object_id);
        }
    }
    out.sort();
    Ok(out)
}

impl From<regex::Error> for SearchError {
    fn from(e: regex::Error) -> Self {
        SearchError::Other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigram_tokenizer_matches_query_side() {
        let mut t = TrigramTokenizer;
        let mut s = t.token_stream("AbC d");
        let mut toks = Vec::new();
        while s.advance() {
            toks.push(s.token().text.clone());
        }
        assert_eq!(toks, vec!["abc", "bc ", "c d"]);
        assert_eq!(trigrams("AbC d"), toks);
        assert!(trigrams("ab").is_empty());
    }

    #[test]
    fn matcher_semantics() {
        let text = "The QzEndpoint and Qz; qz lower";
        let exact = Matcher::Literal {
            value: "Qz".into(),
            case_sensitive: true,
            whole_word: true,
        };
        assert_eq!(exact.find(text, 10), vec![(19, 21)]);
        let sub = Matcher::Literal {
            value: "Qz".into(),
            case_sensitive: true,
            whole_word: false,
        };
        assert_eq!(sub.find(text, 10).len(), 2);
        let ci = Matcher::Literal {
            value: "qz".into(),
            case_sensitive: false,
            whole_word: false,
        };
        assert_eq!(ci.find(text, 10).len(), 3);
        let terms = Matcher::Terms(vec!["lower".into(), "the".into()]);
        assert_eq!(terms.find(text, 10), vec![(0, 3), (26, 31)]);
        let re = Matcher::Regex(regex::Regex::new(r"Qz\w+").unwrap());
        assert_eq!(re.find(text, 10), vec![(4, 14)]);
    }

    #[test]
    fn regex_required_literals() {
        assert_eq!(
            required_literals(r"postgresql-.*\.log$"),
            vec!["postgresql-", ".log"]
        );
        assert!(required_literals(r"\d+").is_empty());
        assert_eq!(required_literals(r"(error|warn)ing"), vec!["ing"]);
    }
}
