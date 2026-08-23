# Query Syntax

The web search box, `eidos search`, and `POST /api/search` (`q`) accept this
syntax. Everything compiles to the typed AST (`eidos_domain::Query`), which
`POST /api/search` also accepts directly as `query`; the response returns
the compiled AST and its rendering so interpretations are visible and
editable.

## Terms

| Input | Meaning |
|---|---|
| `readme notes` | ranked name/path terms; all words required |
| `"release notes"` | name phrase |
| `*.cs`, `setup?.exe` | glob on the name (case-insensitive) |
| `G:\Tools`, `\\server\share\x` | bare absolute path scopes to that directory |
| `-term`, `NOT term` | negation |
| `a OR b`, `(a b) OR c` | disjunction and grouping; adjacent terms are AND |

## Fields

| Field | Examples | Notes |
|---|---|---|
| `name:` | `name:config`, `name:=README.md`, `name:~Err`, `name:/^v\d+/`, `name:*.json`, `name:"my file"` | default is contains (case-insensitive); `=` exact case-sensitive; `=i` exact case-insensitive; `~` contains case-sensitive; `/re/` regex (`/re/c` case-sensitive) |
| `path:` | `path:G:\Tools`, `path:*\bin\*`, `path:/\\obj\\/`, `path:=G:\x\y.txt`, `path:node_modules` | absolute → prefix; glob; regex; exact; plain word → contains |
| `ext:` | `ext:cs`, `ext:cs,idb`, `ext:none` | case-insensitive, no dot; `none` = no extension |
| `kind:` | `kind:file`, `kind:dir`, `kind:reparse` | |
| `size:` / `alloc:` | `size:>1M`, `size:<=4k`, `size:1M..10M`, `size:=0` | binary units k/M/G/T |
| `mtime:` / `ctime:` | `mtime:>=2026-01-01`, `mtime:7d`, `mtime:2026-02`, `mtime:2026-01-01..2026-03-01`, `mtime:today` | dates are UTC days/months/years; relative: `m h d w mo y` |
| `state:` | `state:pending`, `state:excluded,failed` | content-processing state |
| `has:` | `has:idb`, `has:cs>=3`, `has:log:10..100` | directory contains N files with that extension anywhere beneath (use Directories mode) |
| `files:` | `files:>1000` | directory descendant file count |
| `subtree:` / `subtree_alloc:` | `subtree:>1G` | directory subtree size |
| `attr:` | `attr:hidden`, `attr:hidden,system`, `-attr:reparse` | readonly hidden system archive temporary sparse reparse compressed offline encrypted |
| `source:` | `source:G`, `source:G,R`, `source:2` | configured source name or id |
| `in:` | `in:o:123`, `in:o:123~2` | under an object id (optional max depth) |
| `content:` | `content:zephyr`, `content:"build 4.2"`, `content:=ErrCode`, `content:~Err`, `content:/re/`, `content:/re/c` | literal text of indexed files. Plain value: tokenised phrase (case-insensitive; one word = term); `=` whole word, case-sensitive; `~` substring, case-sensitive; `/re/` regex (case-insensitive unless `/c`). Exact, substring, and regex clauses are verified against the stored original text and return line-aware snippets |

## Result mode and sort

Result mode (`files`, `directories`, `both`) and sort (`relevance`, `name`,
`path`, `size`, `allocated_size`, `subtree_size`, `modified`, `created`)
are request parameters, not syntax. Directory predicates (`has:`, `files:`,
`subtree:`) only produce results in `directories`/`both` mode.

## Pagination cursors

A response whose results continue carries `next_cursor`; pass it back as
`cursor` with the *same* query, mode, sort, and scope to get the next page.
Cursors are opaque, but their shape is documented so that behaviour is
predictable:

```text
o:<consumed>;g:<index generation>;q:<query fingerprint>
```

- `o` counts every candidate the previous pages consumed in sort order,
  including index documents that no longer had a live catalog row (the
  projection lags the catalog briefly after changes). Such documents are
  skipped, the page is refilled from the next candidates, and the cursor
  moves past them, so no result is repeated or skipped because of them and a
  page of nothing but stale documents still makes progress. Only the
  page-driven content-verification path can return a short page behind a
  long run of stale documents (its refill is bounded by the chunk-fetch
  budget); the cursor still advances.
- `q` binds the cursor to the request it was issued for. Reusing it with a
  different query, mode, sort, or scope is rejected with a structured
  `query` error ("cursor does not belong to this request … restart from the
  first page").
- `g` is the index generation the cursor was issued from. Offsets are not
  stable across index commits: if the index changed between pages the
  request still succeeds, but the response carries a warning that a result
  may repeat or be skipped. Restart from the first page when exactness
  matters.
- A malformed cursor is rejected with `invalid cursor`; so is a structured
  cursor carrying only one of `g` and `q`. The legacy `o:<n>` form is still
  accepted without the query or generation checks.
- `t`/`x` carry the first page's total and whether it was exact. Under the
  default count policy (`count=auto`) later pages of a top-k walk reuse that
  total instead of recounting (`total.origin = "cursor"`) while the index
  generation is unchanged; a changed generation recounts. `count=exact`
  recounts on every page; `count=none` never counts — totals that are not a
  by-product of the retrieval are then a lower bound (`exact: false`,
  `origin: "bound"`) and `next_cursor` is present exactly when another
  candidate exists. Queries whose retrieval already holds every candidate
  (verified name/path/content clauses, directory ranking) report a counted
  total on every page at no extra cost.


## Semantics worth knowing

- Name/path matching is case-insensitive unless a case-sensitive modifier is
  used; case-sensitive modes are verified against the stored original text.
- Regexes are unanchored unless you write `^`/`$`. Substring, glob, and
  regex clauses on names and paths find candidates through folded trigrams
  and verify them. A regex is planned as a boolean combination of the
  strings every match must contain — alternations and optional pieces
  expand (`colou?r` → `color | colour`, `(error|warn)ing` → `erroring |
  warning`), and the explanation shows the plan. Patterns without any
  required string of three or more characters walk the folded dictionary
  instead and are flagged as broad. `*.ext` is an extension filter.
- Inside `OR` and `-`/`NOT`, substring/glob/regex clauses run as exact
  dictionary automata (correct, slower on large catalogs), and clauses that
  need verification — case-sensitive modes (`=`, `~`, `/re/c`), `in:` with a
  depth, `has:` with a count — are rejected with an explanation rather
  than applied to the wrong set. Put them at the top level.
- Every response carries per-source completeness. Results from an
  `enumerating`, `degraded`, `offline`, or `stale` source are partial and the
  UI/CLI say so; the CLI exits with status 2 in that case.
- `has:` ranking: the tightest directories (those with no matching directory
  beneath them) come first; ancestors that match only through a child come
  second.
- Content clauses run against the chunk index first and then compose with
  every other clause (including `OR` and `-`) as a set of matching files.
  Ranked and phrase clauses return the best 5,000 chunks; exact/substring/
  regex clauses examine up to 20,000 candidate chunks. When either cap is
  hit the response says so and `total.exact` is false — narrow the query
  with metadata filters. Phrases do not match across chunk boundaries
  (chunks are ≈16 KB, split on line ends).
- Content regexes use the same trigram plan to pick candidate chunks and
  are then verified against the stored text. Regexes with no required
  string of three or more characters (for example `/\d+/`) scan every
  chunk in scope; they are allowed but flagged.
- Broad name/path substring, glob, and regex clauses (more than 2,000
  candidates) verify results lazily in sort order: the hits you see are all
  verified, but the reported total is an upper bound (`exact: false`,
  rendered as "N+") until a page reaches the end of the result set.
