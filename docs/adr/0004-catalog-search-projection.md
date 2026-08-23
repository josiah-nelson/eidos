# ADR-0004: Catalog search projection, query execution, and syntax

Status: accepted  
Date: 2026-08-22  
Milestone: 3

## Context

SPEC 7.8 requires a typed AST shared by every surface, filename/path exact,
substring, glob, and regex search, structured filters, directory
predicates, explanations, and completeness in every response. ARCHITECTURE
10 expects a Tantivy catalog index with current path and source state joined
from the catalog. The roadmap's decision gates ask for benchmarks before
choosing integrated versus sidecar trigram storage.

## Decisions

### One document per live entry

The catalog index (`eidos-search::CatalogIndex`) holds one Tantivy document
per live directory entry (so hard links are searchable by either name).
Stored fields: ids, name, path, kind, extension, sizes, times, directory
aggregate fields, `ancestors` (all ancestor object ids), and `desc_ext`
(extensions present anywhere beneath a directory). `name_folded`,
`path_folded`, `extension`, `kind`, and `content_state` are raw `STRING |
FAST` fields so they sort, facet, and feed term-dictionary automata;
`name` and `path_tokens` are tokenised for BM25 ranking and phrases.

The catalog remains canonical: each hit is re-joined with the current
object row (size, content state, link count) and the source's state. The
stored path is used as-is because the follower keeps it within ≈0.5 s of
the catalog; the response says when it does not (`warnings`, completeness
notes).

### Regex and substring without a trigram index (for names and paths)

Name and path substring/regex/glob queries run as finite-automaton walks
over the folded term dictionary (`RegexQuery` on `name_folded` /
`path_folded`). The dictionary is small (≈60 k unique names, ≈125 k paths
for G:+R:), so a full walk costs tens of milliseconds — measured p95 40 ms
for `name:config`, 73 ms for regexes, well inside the 250 ms gate — and
requires no extra index. Case-insensitive matching is exact on the folded
dictionary; case-sensitive modes (`=`, `~`, `/re/c`) re-check the stored
original text ("verify" plan step). Trigram candidate retrieval is therefore
reserved for **content** (Milestone 4), where chunk text is large. This
answers the "integrated versus sidecar trigram" decision for the catalog:
neither is needed.

Patterns that compile to automata without a required literal (e.g. `\d+`)
execute but are flagged in `warnings`. Regexes are validated with the
`regex` crate (1 MiB size limit) before compilation.

### Two execution paths

- **Top-k**: when no verification or tight-ranking is needed, Tantivy's
  collectors do everything (BM25 or fast-field sorts, `Count`), and only
  the page is fetched.
- **Collect-all**: when a clause needs verification (case-sensitive text,
  `has:ext>=N` counts, depth limits) or directory tightness ranking, all
  candidates (capped at 100 000, with a truncation warning) are fetched,
  verified, ranked, sorted in Rust, and paged. `total.exact` is `false`
  when truncated.

Cursors encode the offset (`o:N`); they are stable between identical
requests while the index does not change, and the index reports its lag.

### Directory results and Q-4 ranking rule

`has:idb has:cs` in Directories mode matches every directory whose subtree
contains both extensions (`desc_ext` terms). Among matches, a directory is
*tight* when no matching directory lies beneath it; tight directories
rank first (score 1.0), ancestors that only match through a child rank
second (0.5). The rule is stated in the plan's `rank` step.

### Facets

Facets are Tantivy aggregations over fast fields (`terms` for source,
extension, kind, content state, parent directory; `range` for size and
modified buckets) computed on the same scoped query. Source and directory
keys are labelled from the catalog.

### Projection follower and rebuilds

A follower thread rebuilds a source when its published generation differs
from the projection's recorded generation (bulk: G: 61 676 docs in 0.9 s,
R: 63 075 in 0.8 s), then drains the outbox (`upsert`/`content`/`delete`/
`subtree`) in 2 000-row batches: delete-by-object then re-add from the
catalog — idempotent, so a crash between index commit and position update
only repeats work. The position is recorded in the catalog
(`projection_state`) after the index commit.

Both paths read the catalog in batches of `PROJECTION_BATCH` (1 024) rows.
A rebuild loads the source's path nodes once — directories, virtual
directories, and archive containers, which are ordinary files that own
virtual members — and then resolves the descendant extensions of a whole
batch of directories in one query instead of one query per directory; it
holds one batch of rows plus that path-node map, nothing proportional to
the source otherwise. An outbox batch is coalesced before anything is
written: duplicate and nested `subtree` rows collapse into a single
rebuild, every affected object is deleted exactly once (subtree roots also
by ancestry, which catches documents from a superseded generation), and the
re-adds then run through one batched read that walks each ancestor chain
once for the batch rather than once per descendant. Deleting before adding
is what keeps a subtree row from removing documents added earlier in the
same batch. Because containers are path nodes, a rebuilt member document
renders under its container (`…\tool.zip\src\lib\mod.rs`) — the path the
incremental route always produced.

### Query syntax

`eidos-query` parses an Everything-style syntax (`docs/QUERY_SYNTAX.md`) into
the AST and renders any AST back to text so the UI can show and edit the
interpretation. Bare absolute paths scope by prefix; bare words are ranked
name/path terms; `field:value` clauses cover every AST family. Source names
are carried on the AST (`Source { ids, names }`) and resolved by the
executor.

## Consequences

- Metadata and directory queries: p95 ≈ 4–5 ms on the G:+R: catalog (30
  iterations each, release build), unchanged under a concurrent rescan
  (p95 4.6 ms, p99 7.6 ms).
- Name substring/glob p95 ≈ 40–50 ms; regex p95 ≈ 73–77 ms; the widest
  path regex (`path:/\\bin\\/`) ≈ 80 ms. A future trigram field over names
  could cut the dictionary walk if corpora grow by an order of magnitude.
- Content clauses are rejected with an explicit message until Milestone 4
  — never silently ignored.
- Batched projection reads on a synthetic 36 005-entry source (6 000
  directories six levels down, release build): a full rebuild goes from
  6 007 to 9 catalog queries at the same wall time (95 ms → 99 ms) and
  ~0.6 MB more peak heap for the row batch it holds; rebuilding the whole
  subtree goes from 252 006 queries and 886 ms to 83 queries and 142 ms,
  for ~2.3 MB of path/ancestor caches (bounded by the directories in the
  batch's chains, not by the files under them).
