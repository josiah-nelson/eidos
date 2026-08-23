# Overview

What eidos is trying to be, and the requirements that shape it. This is the
behavioral authority for the project; [architecture.md](architecture.md)
holds the invariants and boundaries, and [roadmap.md](roadmap.md) the
sequence and gates.

## 1. Purpose

A modern, high-performance filesystem indexer for Windows, macOS, and Linux
that unifies:

- near-instant filename, path, and metadata search;
- indexed literal and extracted file content;
- nested archive discovery and, later, content extraction;
- apparent and allocated folder-size analytics;
- reliable multi-host search through local agents;
- a first-class web UI, CLI, and MCP server;
- later directory similarity, document relationships, code intelligence, and
  hybrid semantic retrieval.

The initial deployment model is a personal, privileged service: one user,
several machines, full read access. Multi-user document-level security is not
required early. The architecture must not preclude adding authorization later,
but early releases do not pay its implementation cost.

## 2. Thesis

Existing tools optimize isolated portions of the problem:

- Everything is exceptionally responsive for Windows names and metadata but
  has weak content and fleet ergonomics.
- dtSearch is a strong content-search reference with an aging product model.
- Storage analyzers understand size but not content or relationships.
- Code-search and vector-search tools lack a correct general filesystem model.

The differentiator is not a new inverted-index algorithm. It is a coherent,
observable system that keeps a correct catalog of mutable files, makes basic
results available immediately, processes expensive content safely in the
background, and exposes directory-aware and cross-host queries.

## 3. Target environment

- Windows hosts with several local NTFS volumes first; a handful more Windows
  hosts, then macOS, then a Debian-based NAS.
- Mostly NVMe / SAS SSD storage, with at least one important 15K SAS HDD
  volume, so scheduling has to be volume-aware.
- Agents installed on target hosts are preferred over central SMB crawling;
  SMB remains a v0.5 fallback and benchmark workload.
- The reference corpus is roughly 3.8 TB across two local volumes with about
  116,000 visible files, dominated by VM disks and archives by bytes and by
  source code, logs, JSON, and HTML by count; plus two SMB shares with about
  four million entries. See [benchmarks.md](benchmarks.md).

## 4. Goals

### 4.1 Trustworthy catalog

- Preserve stable file identity across renames where the filesystem permits.
- Represent paths separately from file objects so hard links and renames are
  not mistaken for new content.
- Recover correctly after interruption, event overflow, journal rollover, or
  temporary source unavailability.
- Never silently treat an incomplete source as a complete zero-result search.
- Preserve explicitly configured offline-host indexes indefinitely.

### 4.2 Progressive availability

- Publish metadata before content processing finishes.
- Prioritize small, high-value text over large logs and archives.
- Expose per-file and per-source processing completeness.
- Bound memory, CPU, decompression, and I/O for every pipeline stage.

### 4.3 High-quality exact and lexical search

- Preserve case-sensitive exact matching for short identifiers.
- Support filename/path regex and selective content regex.
- Support Boolean, phrase, proximity, and ranked lexical queries.
- Treat IPs, versions, paths, GUIDs, and code identifiers as important exact
  values rather than incidental punctuation-separated words.
- Filter across host, source, path, type, size, and time.

### 4.4 Directory-aware search

- Return files or directories as first-class result types.
- Query properties of descendants, not only direct children.
- Maintain apparent and allocated subtree sizes.
- Support exact directory copies and diffs (v1) and near-copy ranking (v1.5).

### 4.5 Excellent web UX

- The web UI receives production-level attention in every release.
- Search, browsing, indexing health, exclusions, errors, size analytics, and
  comparisons must be understandable without reading logs.
- Large result sets and trees are virtualized and incrementally loaded.

## 5. Non-goals for early releases

- Multi-user ACL filtering or prevention of privileged-index leaks
- OCR
- Replacing original files with a document-management repository
- Writing a new general-purpose inverted-index engine
- A graph database before graph traversal is a demonstrated bottleneck
- Embedding all content by default
- Perfect reclaimable-space calculation across snapshots, reflinks, and
  filesystem-level deduplication
- Linux feature parity before Windows and macOS are stable

## 6. Representative scenarios

These shape releases; each has a golden test on synthetic fixtures (see the
`q1`…`q6` tests in `crates/eidos-search/tests/search_golden.rs`) and a gate
in the roadmap.

| Id | Scenario | Primitives required |
|---|---|---|
| Q-1 | Find a Markdown diagnostic on one of several hosts, bounded by modified time, with ranked product/version terms and an exact content literal | host filter, `mtime` range, extension filter, ranked + exact content, completeness reporting |
| Q-2 | Find a recent analysis containing a short case-sensitive identifier when the filename and machine are unknown | exact case-sensitive content, time filter, line-aware snippets |
| Q-3 | Find an old spreadsheet containing a known IP address; it may be loose or nested inside archives | spreadsheet extraction, IP-aware indexing, recursive archives (v1) |
| Q-4 | Find a directory tree containing both IDA database artifacts and C# sources, even when the two kinds sit in different nested folders | directory topology, descendant predicates, extension rollups, directory ranking |
| Q-5 | Find exact or near copies of a folder of scripts, Markdown, JSON, and TSV, then diff one named Markdown file across copies | content hashes, directory fingerprints, copy grouping, text diffs (v1) |
| Q-6 | Find directories named like an IP address with an optional ` (NN)` copy suffix, pick the largest, rank by overlap | name regex, subtree size, virtual archive topology, similarity (v1.5) |
| Q-7 | List every `.dmp` file across all systems, including hosts that are currently offline | durable agents, offline preservation, extension filter, fleet health (v1) |
| Q-8 | Express the above conversationally and get a visible, editable structured query | typed AST, NLQ-to-AST adapter, explanation (v1) |

## 7. Functional requirements

### 7.1 Sources

A *source* is a configured filesystem scope with a stable `source_id`
independent of drive letter, mount point, agent, and network alias. It
records its owning host, platform and filesystem capabilities, canonical root
and aliases, policy version, state and state reason, last complete scan and
change-feed checkpoint, preservation policy, and aggregate counts.

Source states: `new`, `enumerating`, `metadata_complete`, `content_pending`,
`complete`, `degraded`, `offline`, `stale`, `reconciling`, `retired`. A
missing heartbeat is never interpreted as source deletion.

### 7.2 Canonical catalog

The catalog distinguishes filesystem **objects** (stable identity, type,
logical and allocated size, timestamps, attributes, hard-link count, content
hash and processing state) from directory **entries** (parent object, child
object, exact display name and normalized lookup name, virtual/archive
status, observed generation, tombstone state). Paths are rendered by following
parents; a cached materialized path is a projection, not identity.

### 7.3 Enumeration and changes

Local NTFS/ReFS sources use native enumeration and the USN change journal.
Other roots use directory enumeration and, where possible, directory-change
notifications. The safe scan sequence is:

1. establish or checkpoint the change feed;
2. enumerate into a scan generation;
3. replay changes that overlapped enumeration;
4. publish the generation atomically;
5. reconcile periodically and on overflow or checkpoint invalidation.

SMB notifications are hints only; generic remote sources are reconciled on a
schedule and surface their weaker freshness guarantee.

### 7.4 Size analytics

Per directory: logical/apparent bytes, allocated bytes, file and directory
counts, sparse counts by extension, newest/oldest descendant modification
time, content-processing state counts, and archive compressed/declared sizes.
Initial aggregates are computed bottom-up; changes propagate deltas through
ancestors; subtree moves subtract and add a whole subtree aggregate. The UI
labels apparent and allocated size explicitly.

### 7.5 Exclusions and classification

Every item independently receives three policy outcomes — inventory
inclusion, content inclusion, enrichment inclusion — each with a stable
reason code, rule, and policy version. Content-excluded files still count in
folder sizes and metadata search. Default content exclusions cover VM disk
images, swap/hibernation files, recycle-bin contents, caches, dependency
caches, and obvious binary data. Rules are context-sensitive: a name like
`bin` is never globally excluded.

### 7.6 Literal-text content processing

Text-like files and extensionless text are processed in a streaming pipeline
that recognizes UTF-8/UTF-16/BOMs/common Windows encodings, rejects binary
data with bounded sniffing, splits on line boundaries, preserves chunk byte
and line ranges, hashes with BLAKE3 while bytes are being read, schedules
small files before large ones, and records deterministic error and
truncation metadata. There is no arbitrary silent size cutoff; large files go
to a slower tier, and results expose whether the full file, a prefix, a tail,
or nothing was indexed.

### 7.7 Archives

Archive members are virtual `objects`/`entries` rows linked to their container.
v0.5 indexes ZIP central-directory names, topology, and declared sizes without
extracting content. Physical and archive declared/compressed aggregate bytes
remain separate. v1 adds recursive ZIP/RAR/7z/tar with limits on depth, member
count, expanded bytes, compression ratio, time, and memory.

### 7.8 Search

One typed AST is shared by the UI, CLI, MCP, saved queries, and the NLQ
adapter. Clause families: Boolean; text (ranked, exact, phrase, proximity,
substring, regex); identity (host, source, volume, object, directory); path
(exact, prefix, glob, regex, descendant-of); metadata (extension, kind, size,
allocated size, timestamps, attributes); processing state; directory
predicates (descendant counts, subtree size, later fingerprints and
similarity); archive clauses. The planner pushes structured filters before
scoring, uses trigram candidates plus verification for substring/regex,
verifies exact case against original text, flags pathologically broad regex
scans, groups chunks into file results with diverse snippets, and returns a
machine- and human-readable explanation with every response.

### 7.9 Web UI

Every release ships a coherent UI: search with an editable interpretation,
virtualized results, snippets with line/byte locations, facets, directory
browser and treemap, source completeness and health, backlog and throughput,
exclusions and errors, and availability-aware open/download actions. The UI
never requests or renders millions of rows.

### 7.10 CLI and MCP

The CLI uses the same query API and output schema as the web UI, with
structured JSON output, stable cursors, explicit completeness, and nonzero
exit status for invalid or incomplete operations. The v1 MCP server exposes
bounded read-only tools (`search`, `stat`, `list_children`, `read_range`,
`explain_match`, `compare_files`, `find_copies`).

## 8. Reliability requirements

- Catalog updates are transactional and idempotent.
- Every derived record carries source/object generation and schema version.
- Search publication uses atomic generations or commits.
- A crash between catalog and search publication is repairable from durable
  job state without a full filesystem scan.
- Event overflow and journal reset are first-class states, not log lines.
- Removing a source requires an explicit lifecycle transition.
- All failures and exclusions are queryable and visible in the UI.
- Search responses state which requested sources are complete, pending,
  degraded, stale, or offline.

## 9. Performance targets (v0.5, reference corpus)

- progressive metadata visible within 2 s; complete local metadata scan
  within 30 s;
- metadata query p95 < 50 ms; ordinary content query p95 < 150 ms; selective
  regex p95 < 250 ms;
- incremental metadata visibility within 2 s;
- initial ≈26 GB literal-text indexing within 30 minutes;
- no rebuild on restart while the USN checkpoint remains valid;
- steady-state memory below ≈2 GB; catalog/index/cache budget 32–64 GB.

Concurrent indexing benchmarks must measure query p95/p99 at the same time.

## 10. Security posture

Early versions run as a privileged personal service. Still required from the
beginning: no credential storage in source control or plaintext
configuration; secret injection through OS credential facilities or
environment indirection; parser isolation; path-traversal protection for
virtual members and file-serving endpoints; read-only scanning; explicit
opt-in before exposing the service beyond loopback; validation of every
file-open request against the configured source.

## 11. Definition of done

A feature is complete only when it has documented behavior and failure
semantics, unit tests for core logic, integration tests covering
restart/idempotency where relevant, measured performance on an appropriate
fixture, observable status and errors, a usable web presentation if
user-facing, and no silent fallback that changes completeness or matching
semantics.
