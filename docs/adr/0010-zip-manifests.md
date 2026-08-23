# ADR-0010: ZIP manifests from the central directory

Status: accepted  
Date: 2026-08-23  
Milestone: 5

## Context

The roadmap's v0.5 archive scope is an inventory, not extraction: member
names, topology, declared sizes, and member metadata of ZIP containers,
discoverable without reading member data (Q-6 places a candidate inside an
archive). Until now every `.zip`-family file was marked `unsupported` at
scan time and never touched again. A manifest needs a parser that is safe
on hostile input, a place in the catalog, and a way to run under the same
budgets and visibility as text extraction.

## Decisions

- **`eidos-archive` reads only the end records and the central directory.**
  It finds the end-of-central-directory record by scanning the last
  ≤ 65,577 bytes for a signature whose comment length reaches the end of
  the file exactly, follows the ZIP64 locator and record when the 16/32-bit
  fields are saturated, validates that the directory lies before its end
  record, and streams central file headers through a 256 KiB buffer. No
  local header is read and nothing is inflated. Multi-volume archives are
  rejected as corrupt. Budgets: 1,000,000 explicit members, 256 MiB of
  directory, 4 KiB names, 256 path segments, 1,000,000 implicit
  directories, and 256 MiB of retained implicit-directory paths; reaching
  a budget truncates the inventory (`Partial`, reason recorded) instead of
  failing it. Structural inconsistencies — directory past the end record,
  entry signatures missing, a claimed entry count the directory cannot
  hold, or missing ZIP64 size replacements — fail deterministically with
  the reason.
- **Member names are normalised, never trusted.** `\` becomes `/`, leading
  `/` and drive letters are stripped, `.` and `..` segments are dropped,
  control characters removed, empty names replaced; each of these sets a
  flag on the member and the archive counts flagged members. Names are
  UTF-8 when the flag or a CRC-matched Info-ZIP Unicode Path extra says so,
  or when the bytes are valid UTF-8, otherwise CP437. Timestamps come from
  the NTFS or extended-timestamp extras when present, else the DOS fields
  (read as UTC, 2-second resolution). Directories missing from the archive
  but implied by member paths are synthesised as *implicit* members, so the
  virtual tree is complete.
- **Manifests ride the content pipeline.** The scan policy now classifies
  ZIP-family extensions (`zip jar war ear aar apk ipa nupkg vsix xpi crx
  whl egg epub …`, never Office Open XML) as content candidates; their jobs
  queue at `ArchiveManifest` priority, behind every text tier. Hard-linked
  objects use the extension of the same deterministic entry whose path the
  worker renders. The content worker routes containers to the parser,
  stages `archive_members` in bounded 1,024-row transactions, then
  generation-checks and atomically publishes the `archive_records` and
  content rows. A newer object generation rejects the stale result, while
  a retry of an already-published generation is an idempotent no-op so it
  cannot mutate the member set readers are using. The content state is
  `indexed` with the reason "zip manifest: N members, D
  directories", `partial` when truncated, `failed` when corrupt. A file
  with a container extension that has no end record falls back to text
  extraction (a misnamed text file still indexes as text; binary ends
  `unsupported` as before) and leaves an `unsupported` marker record so
  the fallback is remembered. Activity views therefore show archives with
  no new pipeline.
- **Catalog migration 5** adds the two tables. `reset_content_for_reindex`
  clears them with the chunks. `Catalog::requeue_archives` queues a
  manifest job for every container by extension whose current generation
  has no archive record — the path for catalogs crawled before this
  change, exposed as `POST /api/sources/{id}/archives` and
  `eidos archive requeue <source>`. Requeue selection and mutation run in
  bounded 256-object transactions so other catalog writers can progress.
- **Reading a manifest**: `GET /api/objects/{id}/archive` returns the
  record and one page of members, either the children of a virtual
  directory (`parent=`, directories first) or every member under a path
  prefix; `eidos archive show <object>` prints it, marking flagged names.
- **Archive members materialize as ordinary catalog topology.** Every member
  has an `objects` row of kind `virtual_file` or `virtual_directory` and an
  `entries` row beneath the container object. Provenance columns bind its
  identity to container id, container generation, and member ordinal. Rows
  are inserted as hidden staging data in the same bounded writer windows as
  member metadata, then the complete tree replaces the previous generation
  atomically. Duplicate normalized paths remain separate ordinal identities;
  descendants attach to the first directory with that path.
- **One subtree event publishes a virtual-tree replacement.** Index followers
  delete the former tree by its container ancestor before projecting current
  descendants, so a manifest does not create one outbox row per member.
  Full-scan changes, change-feed updates, container deletion, and content-index
  reset all retire materialized descendants. Existing manifests without
  virtual rows are selected by `archive requeue` for backfill.
- **Archive expansion never masquerades as physical storage.** Virtual files
  and directories participate in subtree and extension counts, but their
  sizes reduce into dedicated `archive_declared_bytes` and
  `archive_compressed_bytes` aggregate fields. Physical `logical_bytes` and
  `allocated_bytes` count the container once and do not include expanded
  members. Incremental publication, hard-link changes, reset, and full
  reconciliation maintain the same split.
- The fixture builder (`eidos_archive::fixture`) synthesises archives —
  stored members, ZIP64 end records, arbitrary extra fields — so tests
  exercise truncated tails, bogus directory offsets, bomb-shaped entry
  counts, traversal and absolute names, encodings, and budgets without
  committing binary files.

## Consequences

- Member names and paths participate in ordinary browse and catalog-index
  search; `has:` sees virtual descendant extensions. Browse and search expose
  archive declared/compressed totals separately from physical size.
- Archives that are text in disguise cost one extra tail read before the
  text path; archives that are binary in disguise cost the same read before
  the existing sniff.
- Inventories are generation-bound like chunks: a changed container gets a
  fresh job from the change feed and its members are replaced atomically.
- v1's recursive archives (nested containers, RAR/7z/tar) build on the same
  record/member shape with a container chain; nothing here reads member
  bytes, so no extraction budget exists yet.
