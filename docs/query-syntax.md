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

## Semantics worth knowing

- Name/path matching is case-insensitive unless a case-sensitive modifier is
  used; case-sensitive modes are verified against the stored original text.
- Regexes are unanchored unless you write `^`/`$`. They run over the folded
  name/path dictionary; patterns without any required literal are allowed
  but flagged as broad.
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
- Regexes with no required literal of three or more characters (for example
  `/\d+/`) scan every chunk in scope; they are allowed but flagged.
