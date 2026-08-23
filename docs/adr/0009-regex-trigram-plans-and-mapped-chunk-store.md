# ADR-0009: Trigram query plans for regexes and a memory-mapped chunk store

Status: accepted (amends ADR-0006 and ADR-0008)  
Date: 2026-08-23  
Milestone: 4

## Context

ADR-0008 left one shape of the "selective regex p95 < 250 ms" gate unmet:
`content:/timed? ?out after \d+/` took ≈530 ms on the full catalog and was
reported as a subset. Two causes, measured on the 1.82 M-chunk index:

1. **Weak candidate plans.** Regex candidates were the `AND` of the
   trigrams of every literal the HIR walk could see. `timed? ?out after`
   yielded `time` and `out after ` — both everyday log words — so tens of
   thousands of chunks qualified for five true files. Alternations
   collapsed the same way: `(error|warn)ing` kept only `ing`.
2. **Slow chunk fetches for the chunks that matter.** The earlier ≈70 µs
   per fetch came from `bench chunks`, whose sample was consecutive rows of
   a few objects — near-sequential pages. The candidates of a real query
   are spread over a 9 GB catalog, and long-line logs produce chunks of
   ≈84 KB of text (the chunker targets 16 KiB but keeps lines whole up to
   256 KiB). Such a fetch cost ≈300 µs serially and ≈150 µs wall across 8
   threads: SQLite read each overflow page through `ReadFile` inside its
   256 MB mapping limit, and those reads do not scale across connections.
   The page walk also issued one tiny read per object.

## Decisions

- **Regex candidates come from a trigram plan** (`regex_plan.rs`), the
  Cox analysis used by code-search trigram indexes: every HIR node is
  summarised by the set of strings it matches exactly (when bounded and at
  most 20 strings), the sets every match must start and end with, and a
  boolean plan of folded trigrams accumulated whenever a set is about to be
  dropped or trimmed. Concatenation crosses exact sets, alternation unions
  them, optional and bounded repetitions expand (at most three copies,
  classes of at most ten characters), unbounded repetitions and large
  classes reset to "any string". The final plan is an `AND`/`OR` tree of
  literals, each standing for all of its trigrams; `OR` branches must
  contain at least three characters, otherwise that branch is
  unconstrained. Implied parts are removed (`"time"` next to
  `"timeout" | "timed out"`). Examples:

  | pattern | before | plan |
  |---|---|---|
  | `timed? ?out after \d+` | `time & out after ` | `"time out after " \| "timed out after " \| "timedout after " \| "timeout after "` |
  | `(error\|warn)ing` | `ing` | `"erroring" \| "warning"` |
  | `abc.*def.*ghi` | `abc & def & ghi` | same |
  | `[A-Z][a-z]+Exception: ` | `Exception: ` | same |
  | `\d+`, `[0-9]{4}-[0-9]{2}`, `a\|b` | none | none (flagged as broad, as before) |

  The same plan serves name and path regexes (ADR-0006); substring and glob
  clauses go through it as an `AND` of their literal runs, unchanged in
  effect. Explain output prints the plan and its distinct trigram count.
- **The catalog is memory-mapped in full.** `PRAGMA mmap_size` is raised
  to 1 TiB (SQLite clamps it to the file) and the bundled SQLite is built
  with `SQLITE_MAX_MMAP_SIZE` lifted from its 2 GiB default via
  `.cargo/config.toml` (`LIBSQLITE3_FLAGS`), so page reads are memory
  accesses served by the OS file cache and scale across reader
  connections. Writes still go through the pager's write path. The known
  trade-off of mapped I/O — an I/O error on a mapped page faults the
  process instead of returning an error — is accepted for a local catalog.
- **Page-driven verification batches across objects.** A verification
  thread now fetches the next batch of every unfinished object it holds in
  one catalog read per round (`verify_objects`), instead of one read per
  object; batches still grow per object from 8 to 256 chunks and stop at 8
  matching chunks. Verification threads scale with the host:
  `available_parallelism` clamped to 4–16 (was a fixed 8).
- Decoded chunk text is moved out of the decompressor without a second
  copy, and the binary uses `mimalloc`: together ≈10 % per fetch.
- `eidos bench chunks --query <clause>` times the catalog fetches and the
  matcher over the *actual* candidate chunks of a content clause (serial,
  parallel, cold/warm), so store latency is measured on the rows a query
  touches; `eidos bench search --family` restricts a run to named
  families.

## Consequences

Full catalog (4 sources, 4.15 M entries, 1.82 M chunks), 30 iterations,
release build, idle host (`eidos bench search`):

| Query | ADR-0008 p95 | **now p95** | total |
|---|---:|---:|---|
| `content:/timed? ?out after \d+/` | 532 ms (subset) | **174 ms** | 5, exact |
| `content:/[A-Z][a-z]+Exception: /c ext:log` | 78 ms | **60 ms** | 350+ |
| `content:~localhost:` | 123 ms | **56 ms** | 239+ |
| `content:=Exception` | 30 ms | **19 ms** | 2,607 |
| `content:/(error\|warn)ing: \w+/ ext:log` | 194 ms | **54 ms** | 61 |
| ranked/phrase content family | 55 ms | **19 ms** | |
| `content:/connection (refused\|reset)/` | 1,049 ms | 309 ms | 1,519+ |

- The gate query now collects 2,351 candidate chunks over 713 files
  (plan: four alternatives, 20 trigrams) and examines all of them, because
  only five files match; at ≈50 µs per fetch that is the whole cost.
  Totals are exact again for this shape.
- Every content and name clause that touches the catalog benefits from
  the mapping: ranked/phrase content p95 fell from 55 to 19 ms and exact
  tokens from 30 to 19 ms without any planner change.
- The remaining slow shape is a **selective literal with many false
  trigram candidates**: `connection (refused|reset)` plans well, but
  15.9 k chunks contain all of its trigrams while only 52 of 1,280
  examined files contain the phrase — a consequence of trigram documents
  being whole chunks of up to 256 KiB. The lever for that family is
  finer-grained trigram documents (fixed-size blocks inside a chunk, with
  the ranked text still per chunk), which changes the content index layout
  and is deferred to a rebuild-level change with its own record; it is the
  roadmap's "integrated versus sidecar trigram storage" decision.
- `bench chunks` without `--query` still samples consecutive rows and
  reports the optimistic number; use `--query` for anything latency-related.
- Building now requires the flag in `.cargo/config.toml` (checked in); a
  build that drops it silently returns to the 2 GiB mapping and the old
  fetch cost, which `bench chunks --query` would show as ≈150 µs wall per
  chunk at 8 threads instead of ≈60 µs.
