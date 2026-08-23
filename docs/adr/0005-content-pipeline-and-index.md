# ADR-0005: Literal-text content pipeline, chunk index, and verified content queries

Status: accepted  
Date: 2026-08-22  
Milestone: 4

## Context

SPEC 7.6 requires streaming, bounded literal-text processing with exact byte
and line ranges, hash-while-reading, size-tiered scheduling, and visible
coverage. SPEC 7.8 and ARCHITECTURE 10 require exact and case-sensitive
content semantics to be verified against original text, trigram candidates
for substring/regex, chunk→file grouping with snippets, and content
completeness in every response. ARCHITECTURE invariant 7 forbids content work
from blocking metadata publication; invariant 9 forbids whole-file buffering.

## Decisions

### Extraction (`eidos-content`)

- `open → sniff (8 KiB) → decode → chunk → hash → sink`. The read buffer is
  1 MiB; the chunker holds at most one partial line plus one chunk. A 2.29 GB
  file and a 20 MB file use the same memory.
- Sniffing accepts UTF-8 (with or without BOM), UTF-16 LE/BE (BOM or
  NUL-pattern heuristic), and Windows-1252; a NUL or control-character
  density above the threshold classifies the file as binary →
  `unsupported` (not `failed`, not `excluded`).
- Chunks split on newline *code units in the byte domain* of the detected
  encoding, target 16 KiB of decoded text, and force-split lines longer than
  256 KiB at a character boundary (UTF-8 sequences and UTF-16 surrogate
  pairs are never cut). Every chunk carries `[byte_start, byte_end)` and the
  zero-based line range it covers, so snippets and range reads are
  verifiable against the file.
- BLAKE3 runs over the bytes as they are read; `hash_complete` is true only
  when the whole file was consumed. Files beyond `max_full_bytes` (4 GiB by
  default) are indexed as a prefix with `coverage = prefix`.

### Stored chunks live in the catalog (`chunks` table)

Chunk text is stored zstd-compressed (level 1) in SQLite next to its exact
ranges. The catalog is therefore the extraction cache: the content index
stores no text, verification and snippets read the original, and the index
can be rebuilt without touching source files.

### Content index (`eidos-search::content`)

One Tantivy document per chunk with `object_id`, `source_id`, `generation`,
`ordinal` (fast fields) and two indexed, unstored text fields:

- `text`: simple tokenizer + lowercase + 64-char token limit, positions
  recorded — BM25 ranking, phrases, proximity.
- `trigrams`: folded character trigrams over the whole chunk including
  whitespace and punctuation, doc ids only — candidate retrieval for
  exact/substring/regex.

This settles the roadmap's "integrated versus sidecar trigram" gate for
content: trigrams are a field of the same index. They share commits and
deletes with the ranked field, and the separate `Basic` record option keeps
their postings small.

### Clause semantics

| Clause | Retrieval | Verification |
|---|---|---|
| `content:word`, `content:"a b"` | tokenised term / phrase query | none (tokenised semantics) |
| `content:=X` | trigrams of folded `X` (`text` term when `X` < 3 chars) | case-sensitive whole-word literal |
| `content:~X` | same | case-sensitive substring |
| `content:/re/` (`/c`) | trigrams of the regex's required literals (HIR `Concat`/`Literal`, `Repetition{min>0}`, `Capture`) | the regex, case-insensitive unless `/c` |

Clauses with no selective trigram (short literals, `/\d+/`) scan every chunk
in scope; they are executed but flagged in `warnings`. Ranked/phrase clauses
return the top 5,000 chunks; verified clauses examine up to 20,000
candidates. Either cap marks the response `total.exact = false` with a
warning.

### Composition with metadata

A content clause runs eagerly during compilation and becomes a
`TermSetQuery` over `object_id` in the catalog index, so it composes with
every other clause — `AND`, `OR`, and `NOT` — without a second join layer.
Source scope from top-level `source:` clauses and retired sources are
applied on the content side first. When a content clause is present the
executor takes the collect-all path, scores files from their best chunks
(max + 0.1 × each additional chunk), and builds up to three diverse
line-aware snippets per hit from the highest-scoring chunks, highlighting
every clause's matches. Hits whose object generation moved past the
retrieved generation get no snippets rather than wrong ones.

### Workers and two-phase publication

- Jobs: `content_text` per object generation with priority by size tier
  (< 256 KiB small, < 16 MiB normal, else large). The coordinator tops the
  queue up from `content_state IN ('pending','stale')` every 5 s (10,000 at
  a time, low-water 2,000) and change application enqueues directly.
- Per-source concurrency (`sources.content_concurrency`, default 2) bounds
  how many workers read one volume; `sources.content_enabled` turns
  extraction off per source (queued jobs are dropped, re-enabling re-queues).
  A worker takes one unit of that budget *inside* the claiming transaction,
  before any job of the batch is marked `running`, and holds it in an RAII
  reservation for the batch, so workers racing on a stale count cannot
  oversubscribe a source and the unit comes back on an empty claim, an
  error, cancellation, shutdown, or a panic. A source with no free capacity
  is skipped and the next eligible source is claimed instead, so a
  saturated HDD or share never starves the rest of the pool. Live
  reservations and their high-water marks are reported by `GET
  /api/activity` (`workers.concurrency`, and `content_reserved` per source).
- Publication is two-phase: the worker writes chunks and the content record
  as `indexing`, adds documents to the index writer, and completes the job;
  the coordinator commits every 2 s or 20,000 documents and only then
  `mark_content_indexed` flips object states (aggregates + outbox, so the
  catalog index follows). A crash between the two phases leaves `indexing`
  records that `requeue_unfinished_content` re-queues at startup; an empty
  content index at startup resets every indexed object to `pending`.
- Transient I/O failures retry with the job backoff; binary files and
  deterministic failures are terminal and visible (`state:unsupported`,
  `state:failed`, with the reason in the hit).

### Reparse-tag policy

File reparse points are catalogued with real sizes but content policy
excludes symlinks/app-execution aliases (`symlink`), sockets and Linux
device nodes (`special_file`), and cloud/projected/tiering placeholders
(`placeholder`) so extraction never hydrates or follows them. WIM-, dedup-,
and WCI-backed files read normally.

## Consequences

- Metadata search is never blocked by content work; completeness reports
  `content_pending` per source until the queue drains.
- Verification cost is bounded by the candidate cap and by chunk size
  (≈16 KiB zstd-decompressed per candidate).
- Phrases cannot match across a chunk boundary; chunk boundaries are line
  ends, so this only affects phrases spanning lines.
- The content index is rebuildable from `chunks` without re-reading files.
