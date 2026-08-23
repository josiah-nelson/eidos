# Architecture and Invariants

Status: approved planning baseline  
Date: 2026-08-22

## 1. Architectural shape

```text
Windows / macOS / Linux / SMB sources
                    |
             per-host scanner
                    |
       canonical catalog + durable outbox
                    |
        classifier and bounded scheduler
          /          |             \
     metadata    literal text    archive manifest
          \          |             /
       versioned derived documents and aggregates
                    |
     catalog index + content index + trigram candidates
                    |
      query planner / HTTP API / CLI / MCP
                    |
                  web UI
```

Standalone v0.5 colocates all components in one process or service boundary but
retains interfaces that allow the scanner/agent and central query/index service
to separate in v1.

## 2. Non-negotiable invariants

1. The catalog, not a search index, is canonical filesystem state.
2. File object identity is distinct from path identity.
3. Every derived artifact is versioned and rebuildable.
4. Source completeness is part of every search response.
5. Offline is not deleted.
6. Inventory, content, and enrichment policies are independent.
7. Expensive work never blocks metadata publication.
8. No untrusted file is parsed in the core catalog/query process.
9. No stage may require an entire arbitrary file or archive in memory.
10. Exact and case-sensitive semantics are verified against original text.
11. The query AST is the public semantic contract; UI syntax and NLQ compile to
    it rather than bypassing it.
12. A rename or subtree move must not force content re-extraction.

## 3. Proposed implementation stack

### Core

- Rust stable toolchain
- Tokio for service coordination and network I/O
- Dedicated bounded blocking pools for filesystem and parsing work
- SQLite WAL for the standalone catalog, durable jobs, and graph edges
- Tantivy for lexical and structured search
- A compact trigram candidate field/index for substring and regex planning
- BLAKE3 for content hashes read during content processing
- zstd for stored snippets, extraction cache, and transport where beneficial
- Axum for the HTTP API
- A shared typed query/domain crate used by server and CLI

### Web

- TypeScript and React
- Server-state query library and virtualized tables
- Canvas or WebGL treemap for large directory visualizations
- Generated API types from the Rust/OpenAPI or schema contract

Exact libraries should be verified against current official documentation at
implementation time. Avoid framework proliferation when a simpler component
meets measured requirements.

## 4. Suggested workspace boundaries

The names are recommendations, not mandatory crate boundaries:

```text
crates/
  domain/             IDs, catalog records, processing states
  catalog/            SQLite schema, transactions, migrations
  scanner-core/       platform-neutral scan/change contracts
  scanner-windows/    NTFS/USN and Windows directory/SMB adapters
  scheduler/          durable jobs, priorities, backpressure
  content/            sniffing, decoding, chunking, hashing
  archive/            virtual entries and archive budgets
  search/             Tantivy schemas, indexing, retrieval
  query/              AST, parser, planner, explanations
  api/                HTTP contracts and handlers
  cli/                command-line client
  service/            standalone composition and lifecycle
web/                   first-class browser application
```

Do not create a crate merely to mirror this diagram. Prefer a few cohesive
crates initially and split only when platform isolation or dependency control
benefits are concrete. The actual layout (eight `eidos-*` crates) and the
reasons for the merges are recorded in
[ADR-0001](adr/0001-implementation-stack-and-workspace.md).

## 5. Identity model

Use opaque application IDs externally. Preserve native identity internally.

```text
HostId
SourceId
VolumeIdentity
ObjectId(source + native identity or fallback identity)
EntryId(parent object + exact name + observed generation)
ContentId(BLAKE3 bytes)
VirtualObjectId(container generation + member chain)
ChunkId(object generation + extraction version + ordinal)
```

On Windows, native identity is volume serial plus 128-bit file ID when
available. NTFS IDs are stable across ordinary renames until deletion. Fallback
sources use a confidence-bearing composite identity and must tolerate path-based
replacement.

Hard links produce multiple entries referencing one object. Content is indexed
once per object generation. Symlinks/reparse points are entries with explicit
target metadata; traversal is policy-controlled and cycle-safe.

## 6. Catalog schema concepts

Minimum logical tables:

- `hosts`
- `sources`
- `volumes`
- `objects`
- `entries`
- `object_generations`
- `source_checkpoints`
- `scan_generations`
- `directory_aggregates`
- `directory_extension_counts`
- `content_records`
- `chunks`
- `virtual_entries`
- `jobs`
- `outbox`
- `policy_decisions`
- `errors`
- later `entities`, `edges`, `symbols`, and `occurrences`

Database migrations are monotonic, tested, and backed up before destructive
changes. Large mechanical reindexing is a derived-index operation, not a
catalog migration when avoidable.

## 7. Scan and publication state machine

### Initial scan

```text
new
  -> watcher/checkpoint established
  -> enumeration generation opened
  -> entries streamed into catalog
  -> directory aggregates reduced
  -> overlapping events replayed
  -> generation validated
  -> metadata generation published
  -> content jobs drain independently
```

### Incremental mutation

```text
native event
  -> normalize/coalesce
  -> catalog transaction
  -> aggregate delta
  -> durable derived-index job
  -> search commit
  -> checkpoint acknowledgement
```

Checkpoints advance only when enough durable state exists to replay or repair
all downstream work.

### Directory aggregate invariants

Whatever a full reconciliation would compute is the definition; the
incremental path must land on the same values, and tests assert that
equality after every kind of mutation.

- Counts and byte totals are per live entry of the subtree, so a hard-linked
  file counts once under each directory that links it.
- `newest_modified`/`oldest_modified` are the extrema of the subtree's
  **non-directory** entries. A directory's own modification time never
  contributes; its aggregate row carries its children's extrema instead.
- Virtual archive members hang off their container object, which owns no
  aggregate row, so they never reach a physical directory's totals or
  extrema. The container counts as the one file it is.
- Counts are a monoid, so incremental changes add signed deltas along the
  ancestor chain. Extrema are not: when a mutation removes the entry that
  provided a directory's extremum, that one directory is recomputed from its
  direct children, and the walk keeps recomputing upward only while an
  ancestor's extremum is likewise invalidated. Propagation never stops on a
  swallowed error — a failed ancestor lookup aborts the transaction.

### Overflow or invalid checkpoint

Mark the source degraded, preserve existing results as stale, reconcile the
smallest safe scope, then restore completeness. Never clear the source merely
because a change feed cannot continue.

## 8. Scheduler and backpressure

Jobs carry source, object generation, stage, priority, estimated cost, retry
state, and idempotency key.

Initial priority classes:

1. catalog-critical changes and deletes
2. newly discovered metadata/search projection
3. small text files
4. normal text files
5. large text files
6. archive manifests
7. recursive extraction and later enrichments

Apply limits per physical volume/source so a fast NVMe and a slower HDD can make
progress independently. Separate CPU, local-disk, network, decompression, and
future GPU budgets.

A per-source budget is only meaningful if it cannot be raced. Capacity is
reserved atomically as part of claiming: the claim transaction offers the
candidate source to the scheduler, which either hands back a reservation or
declines, and only an admitted source has its jobs marked `running`. The
reservation is an RAII guard held for the whole batch, so capacity returns on
every exit path, including an empty claim, an error, cancellation, shutdown,
and a panic. A source at its budget is skipped in favour of the next eligible
one rather than blocking the pool.

Retries distinguish transient source errors, unsupported content, deterministic
parse failure, resource-limit failure, and corrupt input. Deterministic failures
do not retry forever.

Failures of the stores a stage writes to — a database write error, a full disk,
an index writer error — are classified separately from failures of the input.
They are transient by definition and retry under the same backoff, and the
attempt discards whatever it already wrote for that object generation so no
later commit can publish partial output. The failure reason keeps the whole
underlying error chain (ADR-0012).

## 9. Content records and chunks

Content processing is streaming:

```text
open -> sniff -> decode -> chunk -> hash -> enqueue index docs -> finalize
```

A content record stores:

- source/object generation;
- detected and declared type;
- decoder and extraction version;
- byte coverage (`full`, `prefix`, `tail`, `sample`, `none`);
- line coverage where meaningful;
- hash status and content ID;
- total decoded characters;
- failure, truncation, or exclusion state.

Chunks should generally be line-aware and bounded by decoded byte/character
size. Exact defaults are benchmark decisions. Every chunk retains original byte
and line ranges so snippets and file reads are verifiable.

Stored chunk text may be compressed. Search indexes contain only fields needed
for retrieval and snippets; the catalog/extraction cache owns durable processing
metadata.

## 10. Search architecture

### Catalog index

Contains file and directory result documents:

- IDs and source/host
- filename and normalized name
- extension/type
- current cached path fields
- timestamps and sizes
- directory aggregate fields
- processing/exclusion/source states
- archive/container fields

### Content index

Contains chunk documents:

- chunk/object IDs and generation
- ranked text with positions
- folded trigram terms without unnecessary positions
- exact entity fields in v1
- language/type fields
- byte and line ranges

### Query execution

1. Parse syntax or accept a typed AST.
2. Validate scope, regex cost, and clause limits.
3. Resolve directory/source filters.
4. Retrieve metadata and/or content candidates.
5. For substring/regex clauses, intersect trigrams then verify stored text.
6. Group chunks by object, drop candidates whose generation the catalog has
   superseded, and select diverse snippets from what remains.
7. Join current paths, aggregates, and source completeness from the catalog.
8. Return results, cursors, timing, completeness, and explanation.

A future central deployment may use a different distributed search backend, but
the AST, result schema, exact semantics, and completeness contract remain.

## 11. Directory predicates and fingerprints

v0.5 maintains exact sparse extension counts per directory subtree. This makes
queries such as `.idb AND .cs under the same tree` direct and also powers facets.

v1 exact directory fingerprints are bottom-up Merkle values over normalized
child names, kinds, and available content/directory hashes. Fingerprint state
must show whether all relevant children have strong hashes.

v1.5 similarity is separate from exact identity. Candidate sketches may combine:

- normalized relative paths;
- file sizes and types;
- content hashes;
- text shingles;
- archive virtual paths.

Similarity results must explain their major overlap/difference signals.

## 12. Archive architecture

Archive members reuse the entry/object abstraction in a virtual namespace.
Nested members form a container chain. Paths are normalized for display but raw
member names are retained.

Archive parsers never write members into the source tree. All extraction occurs
through bounded streams or private temporary storage with path traversal checks.

An archive budget is inherited down the nested chain so many individually legal
members cannot exceed the parent job's global limits.

## 13. Standalone to fleet evolution

v0.5 uses an in-process agent contract. v1 moves it behind an authenticated,
versioned transport:

```text
host agent
  catalog/change feed
  local content reader
  durable outbox
        |
        v
central control plane and search service
  source leases
  normalized ingest
  global search
  offline preservation
```

Agents perform I/O beside the authoritative storage. They send metadata,
processing state, and extracted chunks, not arbitrary original bytes. Remote
open/download is a separate authorized endpoint.

Transport operations are idempotent by `(source, object generation, operation)`.
Protocol negotiation reports feature and schema versions.

## 14. UI architecture

The API provides cursor pagination, server-side aggregation, stable IDs, and
progress events. The UI never infers completeness from absence of results.

Search state should be URL-serializable. Advanced and natural-language queries
display the compiled AST or readable equivalent. Directory tables and treemaps
load children/tiles progressively.

The initial UI design should contain these surfaces:

- global search
- directory explorer/treemap
- source health
- indexing activity
- exclusions and failures
- settings/policies

## 15. Observability

At minimum record:

- enumeration entries and bytes per second;
- event/checkpoint lag;
- catalog transaction latency;
- queue depth/age per stage and source;
- bytes read and decoded;
- content success/partial/error/exclusion counts;
- index commit/merge duration and bytes;
- query p50/p95/p99 by query family;
- regex candidate and verification counts;
- reconciliation discrepancies;
- source state transitions.

Support structured logs and a web diagnostics view. Benchmarks emit
machine-readable results suitable for regression comparison.

## 16. Test strategy

- Pure unit tests for identity, normalization, ASTs, chunking, policies, and
  aggregate deltas
- Temporary-filesystem integration tests for rename, hard link, subtree move,
  deletion, symlink/reparse behavior, and restart
- Synthetic journal overflow/checkpoint-invalid tests
- Search semantic golden tests, including exact case and regex verification
- Fault injection between catalog commit and search publication
- ZIP traversal/bomb/nesting fixtures
- Read-only measured benchmarks on the reference local volumes and SMB shares
- Concurrent ingest/query benchmarks
- Migration and derived-index rebuild tests

No test may mutate the measured user corpus.

