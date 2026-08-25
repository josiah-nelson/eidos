# ADR-0015: The source sync ledger lives in the catalog, storing touches not images

Status: accepted
Date: 2026-08-24
Milestone: post-Sprint-1 hardening, track H2

## Context

[ADR-0013](0013-sync-core-under-deterministic-simulation.md) established the
sync data model under simulation: a per-source sequence, source epochs,
materialize-at-ship batches that carry the final row image per touched object,
per-consumer acknowledgement watermarks, compaction above the oldest
watermark, and tombstones retained until every consumer has crossed them. The
simulated `Outbox` keeps both a change log and a map of latest row images in
memory and serializes the whole structure after every operation.

The catalog already has an `outbox` table (migration 2). It is a notification
channel for in-process derived indexes: each row names `(source, object, op,
generation)`, a single global `seq` orders all sources together, the follower
records its position in `projection_state`, and rows are soft-marked with
`consumed_at` and never deleted. It cannot be the sync ledger:

- its sequence is global, while sync order is `(source, epoch, seq)`;
- it has one consumer position, while sync tracks a watermark per consumer;
- it carries pointers, while the applier on another host has no catalog to
  dereference them against;
- it is never compacted, while sync retention is defined by the oldest
  acknowledged watermark.

Two constraints decide where the ledger goes. A source-side batch may only
certify "every event through `through_seq` is represented" if the ledger
entry commits in the same SQLite transaction as the catalog mutation it
describes; a sidecar database cannot give that. And steady-state storage must
be proportional to live objects, not to history, or a busy source that is
offline for three weeks grows without bound (hardening gate H2).

## Decisions

1. **The ledger is a set of tables in `catalog.db`, added by one migration,
   written inside the existing catalog write transactions.** Change
   application, scan publication, archive/container cascades, and content
   state changes that alter a shipped row all stamp the ledger in the same
   transaction that mutates catalog rows. The existing `outbox` table and
   `projection_state` are unchanged and keep serving local projections.

2. **The ledger records touches, not images.** For each source it stores one
   row per object: the per-source sequence at which the object was last
   touched, the object generation, and whether the touch was a deletion. It
   does not store a copy of the object's data. The row image is materialized
   at ship time from the live catalog tables inside one read transaction that
   also reads the source head sequence, so the batch is a consistent snapshot.
   Because a batch already coalesces to the final image per object, "objects
   touched after the consumer's watermark" is exactly the set to ship, and the
   log of intermediate touches never needs to exist on disk.

   Tables (names indicative; the migration is the authority):

   - `sync_sources` — `source_id`, 16-byte `epoch`, `head_seq`,
     `compacted_through`, the native journal identity the epoch was minted
     against, and timestamps. One row per sync-enabled source.
   - `sync_rows` — `(source_id, object_id)` primary key, `seq`, `generation`,
     `deleted` flag. Indexed by `(source_id, seq)` for suffix scans.
   - `sync_consumers` — `(source_id, consumer_id)` primary key, `watermark`,
     `updated_at`. `consumer_id` is a stable typed device identity, not the
     simulator's node id (hardening H3.4).

3. **Sequence numbers are per source and minted in the writer transaction.**
   `head_seq` in `sync_sources` is incremented once per transaction that
   touches a source's rows; every row stamped in that transaction receives
   the new value. The same transaction may not stamp two different sequence
   values for one source.

4. **Retention follows ADR-0013 exactly, over rows instead of a log.** Live
   rows are the source image and are never compacted. A `deleted` row is
   removed only when its `seq` is at or below the oldest consumer watermark
   for that source, in bounded batches. `compacted_through` advances to the
   oldest watermark on each collection pass; a consumer whose cursor is below
   it takes the Merkle repair path, unchanged from ADR-0013.

5. **Enabling sync for an existing source is an epoch event.** Enabling mints
   an epoch and seeds `sync_rows` for every live object through a bounded,
   resumable backfill job; the central requests a full snapshot for the new
   epoch as it would for any epoch change. Nothing is shipped for a source
   whose backfill has not completed. Disabling removes the source's ledger
   rows; re-enabling is a new epoch.

6. **Row images are versioned.** The materialized image is a versioned
   projection of catalog columns (`sync-row/1`), and its version travels in
   the batch. Mixed-version peers fail closed or negotiate a tested fallback
   (hardening H5.5). Content bytes and chunk manifests never enter these
   tables (hardening H6).

7. **Deterministic failpoints are a non-default cargo feature**, following the
   fault-injection precedent from the catalog writer-fairness work: cut
   points around transaction begin, each ledger write, commit, WAL
   checkpoint, and reopen, so the SQLite backend can be driven by the same
   seeded universes as the in-memory oracle.

## Consequences

- Storage for sync is O(live objects + unacknowledged tombstones) per source
  and does not grow with edit history. Five hundred offline edits to one file
  cost one row update and ship once.
- Every catalog write path that changes a shippable row must stamp the ledger;
  a differential test (hardening H6) compares the local follower's derived
  state with materialize-at-ship → apply → follower for the same history, and
  any path that forgets to stamp shows up as divergence. Scan publication,
  which tombstones objects not re-observed, is the path most likely to be
  missed and is tested first.
- The catalog's tombstone purge, if one is added, must not remove an object
  row that a `deleted` ledger row still references; the ledger collection
  pass is the only thing allowed to release it.
- Write amplification is one small row upsert per touched object per
  transaction, only for sync-enabled sources. This is measured as part of the
  H2 scale curves before the feature is enabled by default.
- The legacy `outbox` table's unbounded growth is a separate defect: consumed
  rows should be deleted below the minimum `projection_state` position. That
  fix is small and independent of this decision.
- The simulator's whole-state serialization remains the reference oracle; the
  SQLite backend implements the same trait and must pass the same universes.
