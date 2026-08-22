# Release Roadmap and Acceptance Gates

Status: approved planning baseline  
Date: 2026-08-22

The acceptance cases Q-1 … Q-8 referenced below are defined in
[overview.md §6](overview.md#6-representative-scenarios). Implementation
progress against the v0.5 gates is summarized in the README and measured in
[benchmarks.md](benchmarks.md).

## 1. Roadmap philosophy

The implementation sequence follows shared hot paths and complete vertical
slices. Novel features may be front-loaded when they exercise foundational
models needed later. This is why directory descendant predicates and selective
regex search appear in v0.5, while OCR does not.

Each release has explicit non-goals. A release is not complete because the UI
shows a plausible demo; its correctness, crash recovery, completeness reporting,
and performance gates must pass.

## 2. v0.5: trustworthy single-host Windows indexer

Product statement:

> Prefer this over Everything on one Windows host because it retains comparable
> metadata responsiveness while adding trustworthy literal-content search,
> folder analytics, directory predicates, and a modern web UI.

### 2.1 Milestone 0: repository and benchmark foundation

Deliverables:

- Rust workspace and web workspace
- formatting, lint, unit/integration test commands
- structured tracing and benchmark result format
- domain IDs and processing/source state enums
- typed query AST skeleton and serialization tests
- read-only corpus profiler/benchmark command
- Windows CI or local reproducible build instructions
- `docs/STATUS.md` created and maintained by implementation sessions

Exit gates:

- clean build/test/lint from documented commands;
- no dependency on measured user paths for ordinary tests;
- read-only profiler can summarize a temporary tree and G:/R: when explicitly
  requested;
- benchmark output is machine-readable.

### 2.2 Milestone 1: canonical catalog and generic walker

Deliverables:

- SQLite WAL catalog and migrations
- host/source/volume/object/entry schema
- generic Windows directory walker
- stable scan generations and transactional publication
- logical and allocated sizes
- bottom-up directory aggregates
- exclusion decision schema with reason/version
- source completeness API

Exit gates:

- temporary-fixture rename, hard-link, deletion, and subtree-move tests pass;
- interrupted enumeration never publishes a false complete state;
- apparent and allocated totals match controlled fixtures;
- G:/R: metadata can be imported read-only;
- initial web source-health and directory-browse screens work.

### 2.3 Milestone 2: native Windows fast path and durability

Deliverables:

- NTFS/ReFS capability detection
- native file identity and fast enumeration
- USN checkpoint and event normalization
- durable job/outbox processing
- event coalescing without losing final state
- reconciliation and invalid-checkpoint recovery
- generic SMB crawler with explicit freshness semantics

Exit gates:

- restart continues from a valid checkpoint without full rebuild;
- synthetic overflow/reset tests mark degraded and reconcile correctly;
- source disconnection preserves last-known results;
- native scan meets the provisional 30-second G:/R: gate;
- metadata changes are normally visible within 2 seconds.

### 2.4 Milestone 3: catalog search and storage UI

Deliverables:

- catalog search projection
- filename/path exact, substring, glob, and regex
- host/source/path/type/time/size filters
- file and directory result modes
- descendant extension counts and predicates
- search/query explanation
- virtualized results, facets, tree browser, and treemap

Exit gates:

- Q-4 works against synthetic fixtures;
- Q-6 name/size portion works without similarity;
- `.dmp` metadata search works locally and over configured SMB roots;
- metadata query p95 is below 50 ms on the measured catalog;
- UI always displays source completeness.

### 2.5 Milestone 4: streaming literal-text indexing

Deliverables:

- bounded sniffing and binary rejection
- UTF-8/UTF-16/common Windows decoding
- streaming line-aware chunks
- hash-while-reading BLAKE3
- content states and byte/line coverage
- small/normal/large priority tiers
- Tantivy ranked text and positions
- folded trigram candidate retrieval and exact verification
- snippets and current-path joins

Exit gates:

- Q-1 and Q-2 work locally;
- multi-GB G:/R: fixtures are indexed with bounded memory;
- exact case-sensitive matching is correct;
- selective regex p95 is below 250 ms;
- complete measured candidate content indexes within the provisional 30-minute
  gate;
- ordinary content query p95 is below 150 ms during steady state;
- partial, failed, pending, and excluded content are visible.

### 2.6 Milestone 5: ZIP manifest and operational polish

Deliverables:

- ZIP central-directory member inventory
- virtual archive paths and folder topology
- archive declared-size accounting
- archive-member filename/path search
- policies/settings UI
- exclusions/errors/activity views
- search bookmarks or basic saved-query persistence if inexpensive
- packaging as a Windows service plus local web application

Exit gates:

- archive member names in Q-6 are discoverable without content extraction;
- archive traversal and corrupt-central-directory tests are safe;
- service restart retains catalog/index state;
- the reference SMB shares can be used read-only as fixtures when credentials
  are provisioned externally (never stored in the repository);
- all v0.5 performance and reliability gates pass.

### 2.7 v0.5 non-goals

- central fleet service and agents
- macOS/Linux
- recursive archive content
- PDF/Office/email extraction
- NLQ and MCP
- exact folder-copy grouping/diff
- relationship graph, ASTs, vectors, OCR
- multi-user security

## 3. v1: personal fleet search

Product statement:

> Search all personal Windows and macOS hosts as one durable collection, even
> when some machines are offline, with recursive archives, useful native
> documents, exact directory copies, NLQ, CLI, and MCP.

### 3.1 Agent and central service

- Split the v0.5 scanner contract into a Windows agent.
- Add versioned, idempotent agent transport and durable local outbox.
- Add central source leases, ingest, global search, and host health.
- Preserve offline sources until explicit age-out/retirement.
- Add remote file-open/download routing.
- Add fleet/source/backlog web screens.

Gate: Q-1, Q-2, and Q-7 work across multiple Windows agents including one
offline-preserved agent.

### 3.2 macOS agent

- FSEvents and snapshot/reconciliation adapter
- stable native identity where available
- apparent/allocated size semantics documented
- literal text and archive manifest parity
- macOS file-open integration

Gate: the catalog/completeness/search contract passes the common platform suite.

### 3.3 Recursive archives and native documents

- recursive ZIP, RAR, 7z, and tar
- inherited archive resource budgets
- XLSX, DOCX, native-text PDF, and PPTX extraction
- no OCR
- entity extraction for IPs, URLs, versions, hashes, GUIDs, hostnames, and paths

Gate: Q-3 succeeds through supported nested archives with bounded resources.

### 3.4 Exact copies and diffs

- complete/partial content-hash semantics
- bottom-up exact directory fingerprints
- exact-copy grouping
- file and directory comparison APIs
- text diff UI

Gate: Q-5 identifies exact copies and shows correct `X.md` differences.

### 3.5 NLQ, CLI, and MCP

- production CLI over the public API
- bounded read-only MCP tools
- NLQ adapter compiling to the typed AST
- visible/editable interpretation and execution plan
- model provider remains pluggable and optional

Gate: Q-8 compiles representative natural-language cases without bypassing
structured validation, scopes, or completeness reporting.

### 3.6 v1 non-goals

- near-directory similarity beyond exact hashes
- full relationship/backlink UI
- Tree-sitter/SCIP intelligence
- dense-vector retrieval
- OCR
- full Linux feature parity
- multi-user security

## 4. v1.5: similarity and knowledge layer

Product statement:

> Find and understand related material when names, locations, and exact wording
> are incomplete: near-copy directories, references, code structure, and hybrid
> semantic retrieval.

### 4.1 Directory similarity

- relative-path/content sketches
- overlap and divergence scores
- renamed-copy and archive/directory comparison
- similarity explanations

Gate: Q-6 ranks the intended largest/high-overlap candidate correctly.

### 4.2 Entities and document relationships

- typed `entities` and `edges`
- file/path links, URLs, version/build/session associations
- exact/near duplicate relationships
- backlinks and related-document UI
- confidence, provenance, and resolver versions

### 4.3 Code intelligence

- Tree-sitter structural extraction and chunks
- functions/classes/imports/endpoints/constants
- SCIP ingestion where available
- definitions, references, and code-aware comparison

### 4.4 Hybrid retrieval

- pluggable embeddings scoped by source/type/policy
- quantized vector storage where appropriate
- lexical/vector fusion and optional reranking
- model/version-aware rebuilds
- quality evaluation against labeled personal queries

### 4.5 Linux agent

- Debian-oriented fanotify/inotify/reconciliation adapter
- NAS source semantics and network-mount policy
- common agent/search contract parity

### 4.6 Still deferred unless reprioritized

- OCR
- enterprise multi-user authorization
- exact reclaimable space across clones/snapshots/deduplication
- autonomous LLM content rewriting or summaries as canonical data

## 5. Release-level acceptance table

| Use case | v0.5 | v1 | v1.5 |
|---|---|---|---|
| Q-1 Markdown diagnostic | local | fleet | enriched |
| Q-2 case-sensitive endpoint | local | fleet | structural/semantic options |
| Q-3 spreadsheet IP in archive | no | complete | enriched |
| Q-4 mixed descendant extensions | complete | fleet | structural refinements |
| Q-5 exact copies and diff | no | complete | near-copy extension |
| Q-6 name/size/overlap | name + size | exact copies | similarity complete |
| Q-7 all dumps across systems | local/SMB | complete | Linux included |
| Q-8 NLQ | AST foundation | complete | semantic augmentation |

## 6. Decision gates

Do not settle these by preference alone:

- chunk size and stored-text layout: benchmark multi-GB logs and snippets;
- SQLite schema/index choices: benchmark catalog and aggregate updates;
- integrated versus sidecar trigram storage: benchmark size and regex latency;
- central v1 search backend: decide after observing multi-agent corpus and update
  rates;
- extraction library mix: run a representative correctness/performance corpus;
- vector engine/model: decide only after labeled retrieval evaluation;
- directory similarity features: measure Q-6 quality and storage cost.

Record consequential choices as ADRs under `docs/adr/` during implementation.

