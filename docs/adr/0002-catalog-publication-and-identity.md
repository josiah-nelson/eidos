# ADR-0002: Catalog publication model, identity resolution, and tombstoning

Status: accepted  
Date: 2026-08-22  
Milestone: 1

## Context

SPEC 7.3 requires a safe scan sequence with atomic publication, and SPEC 4.2
requires metadata to be visible before processing completes. ARCHITECTURE
invariants 2, 5, and 12 require object identity to be distinct from paths,
offline/unlistable data to be preserved, and renames/moves not to force
content re-extraction. These pull in different directions: progressive
visibility versus atomic completeness.

## Decisions

### Progressive visibility, atomic completeness

A scan opens a *scan generation* (`scan_generations` row, state `open`).
Ingestion commits batches (4,000 rows or 500 ms) so other connections see
new objects immediately while the source is in `enumerating` (first scan)
or `reconciling` (rescan). Completeness is *not* inferred from rows: the
source's `published_generation` and `state` flip only in the final
transaction of `ScanSession::finish`, which also tombstones, recounts links,
rebuilds aggregates, and resolves stale errors. Consumers read
`SourceCompleteness` with every listing.

A generation left `open` by a crash is marked `aborted` at startup
(`Catalog::recover`); the source becomes `degraded` (if it had a published
generation) or `new`. It is never published.

### Tombstoning rule

Entries are tombstoned at `finish` only when they were not re-observed
**and their parent directory was successfully listed in this generation**
(`objects.listed_generation = G`). A directory that failed to list keeps its
previous children; its aggregate and all ancestors' aggregates are flagged
`complete = 0`, and the count of such directories is reported as
`listing_errors` in completeness. Deletions cascade by fixpoint: objects with
no live entry die, then entries under dead directories die, until stable.

If the **root** directory cannot be listed, `finish` aborts instead of
publishing an empty generation (observed in practice when a malformed root
path was registered).

### Identity resolution

- Native identity (`volume serial + 128-bit file ID`, confidence `native`;
  64-bit IDs are `weak`) is the primary key for objects. Renames and subtree
  moves re-point entries; the object row, its `generation`, and any content
  records survive untouched.
- Sources without file IDs (`FileFullDirectoryInfo` fallback) use
  path-derived identity: the live entry `(parent, name)` *is* the object
  lookup. A rename therefore creates a new object; this is recorded as
  `identity_confidence = path_derived` so the UI and later deduplication can
  treat it accordingly.
- Hard links are multiple live entries for one object; `link_count` is
  recomputed at `finish` from live entries. Apparent size is counted once per
  entry in directory aggregates (WinDirStat semantics); allocated bytes are
  likewise per-entry in v0.5 (noted as a limitation).

### Content-change detection

The object `generation` increments when **size or LastWriteTime** change.
`ChangeTime` is deliberately excluded: NTFS updates it on renames and
attribute edits, which would force re-extraction on every move (invariant
12). USN reasons (Milestone 2) and BLAKE3 (Milestone 4) refine this.

### Policy storage

Only non-default decisions are stored (`policy_decisions` rows for
exclusions), with stage, stable reason code, rule name, and policy version.
Inventory includes everything in v0.5; content exclusions are derived
per-file from extension kind, root-level swap files, offline/recall
attributes, and inherited directory rules (`node_modules`, `$RECYCLE.BIN` at
root only, `.nuget\packages`, …). `bin` is never excluded by name.

### Aggregates

Initial aggregates are rebuilt in Rust from live entries (single SQL pass,
reverse-BFS merge) inside the publish transaction. `apply_delta` propagates
signed deltas up the parent chain for incremental changes; subtree moves use
`AggDelta::from_subtree` to subtract/add a whole subtree without recomputing
descendants (exercised from Milestone 2).

`newest_modified`/`oldest_modified` are not summable, so the delta also
carries the timestamps entering and leaving the subtree. Raising an extremum
is a comparison; removing the entry that *provided* one leaves the stored
value unknown, so `apply_delta` re-derives that single directory from its
direct children (one query, same definition the rebuild uses) and continues
upward only while an ancestor's extremum is invalidated too. The incremental
result equals the rebuild result by test.

## Consequences

- A first-time scan shows growing counts in the UI with a "partial" banner;
  a rescan shows the last published truth until the new generation lands.
- Offset pagination is used for directory children (`list_children`); stable
  cursors arrive with the search API in Milestone 3.
- Measured: G: (61,675 entries) imports in 1.7 s and rescans idempotently in
  2.1 s; both volumes occupy 44 MB of catalog.
