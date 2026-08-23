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
| `-term`, `NOT term`, `-(a b)` | negation of a term or of a whole group |
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
| `subtree_mtime:` | `subtree_mtime:>=2026-01-01`, `subtree_mtime:7d` | newest modification time anywhere beneath a directory (same date forms as `mtime:`) |
| `attr:` | `attr:hidden`, `attr:hidden,system`, `-attr:reparse` | readonly hidden system archive temporary sparse reparse compressed offline encrypted |
| `source:` | `source:G`, `source:G,R`, `source:2` | configured source name or id |
| `in:` | `in:o:123`, `in:o:123~2` | under an object id (optional max depth) |
| `content:` | `content:zephyr`, `content:"build 4.2"`, `content:=ErrCode`, `content:~Err`, `content:/re/`, `content:/re/c` | literal text of indexed files. Plain value: tokenised phrase (case-insensitive; one word = term); `=` whole word, case-sensitive; `~` substring, case-sensitive; `/re/` regex (case-insensitive unless `/c`). Exact, substring, and regex clauses are verified against the stored original text and return line-aware snippets |

## Result mode and sort

Result mode (`files`, `directories`, `both`) and sort (`relevance`, `name`,
`path`, `size`, `allocated_size`, `subtree_size`, `modified`, `created`)
are request parameters, not syntax. Directory predicates (`has:`, `files:`,
`subtree:`, `subtree_mtime:`) only produce results in `directories`/`both`
mode.

## Facet buckets

The `size_bucket` and `modified_bucket` facets return, next to the count,
the exact boundaries of each bucket and the query text that selects or
excludes it, so a client turns a click into a filter without re-deriving
anything:

```json
{ "value": "1048576-16777216", "count": 42,
  "label": "≥ 1 MiB, < 16 MiB",
  "range": { "from": 1048576, "to": 16777216,
             "clause": "size:>=1M size:<16M",
             "exclude": "-(size:>=1M size:<16M)" } }
```

- Buckets are half-open: `from` is inclusive, `to` is exclusive, and the
  first and last bucket are open-ended (`size:<4k`, `size:>=1G`). Labels
  spell both boundaries; the `range` bounds are bytes for sizes and Unix
  nanoseconds for times.
- Modification-time boundaries are UTC midnights, so the clause is an
  absolute date (`mtime:>=2026-08-16 mtime:<2026-08-23`) that means the same
  thing whenever it is re-run — unlike a relative window such as `mtime:7d`.
  Labels name the days they cover and say `UTC`; a bucket therefore shifts
  by up to a day against a viewer's local calendar.
- Which field the clause uses follows the result mode, because the facet
  measures whatever the row shows: in `files` mode the file's own size and
  modification time (`size:`, `mtime:`), in `directories` mode the subtree
  size and newest descendant modification time (`subtree:`,
  `subtree_mtime:`). In `both` mode the buckets mix the two, no single
  clause reproduces them, and `range` is omitted — the buckets are counts
  only.
- Clauses combine with AND like everything else, so appending one only ever
  narrows the query. The web UI applies a click this way, and a client
  should do the same:
  - Selecting a bucket drops any other bucket of the same facet, since
    adjacent buckets are disjoint and intersecting them can only give
    nothing, and drops exclusions of those buckets, which the selection
    already implies.
  - Excluding a bucket removes only its own inclusion; exclusions of
    different buckets are independent and accumulate.
  - Only a bucket's own clause is ever removed. Any other bound stays:
    `size:>=100` plus the `< 4 KiB` bucket is the intersection of both. A
    clause you typed that reads exactly like a bucket counts as that
    bucket — nothing distinguishes the two, and keeping both would leave
    no results.
  - Because AND binds tighter than OR, a query that already contains a
    top-level `OR` is parenthesised first, so the clause filters every
    branch: `a OR b` becomes `(a OR b) size:<4k`.

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
