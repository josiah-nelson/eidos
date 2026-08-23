# ADR-0003: Change feed, checkpoints, durable jobs, and reconciliation

Status: accepted  
Date: 2026-08-22  
Milestone: 2

## Context

SPEC 7.3 and ARCHITECTURE 7 require: a change-feed checkpoint established
*before* enumeration, replay of overlapping events, atomic publication,
checkpoints that only advance once downstream work is durable, and
overflow/invalid-checkpoint handling that never clears a source. SPEC 9
requires incremental metadata visibility within 2 seconds and no rebuild on
restart while the checkpoint is valid.

## Decisions

### "Native fast path" = batched directory enumeration + USN journal

The generic lister already uses the native Win32 batch enumeration
(`FileIdExtdDirectoryInfo`) and returns 128-bit IDs, allocation sizes, and
all timestamps; it walks G: (61,675 entries) in 49 ms warm. MFT enumeration
(`FSCTL_ENUM_USN_DATA`) would give names and parents faster cold, but no
sizes or timestamps, forcing a second pass per file. It is therefore not
used in v0.5; the roadmap's "native fast path" is satisfied by the batched
walker plus the USN journal for changes. Revisit only if cold-cache scans
miss the 30-second gate.

### Checkpoint model

`sources.checkpoint_kind = "usn"`, `checkpoint_json = {journal_id,
next_usn, volume_root}`. The native scan sequence is:

1. `FSCTL_QUERY_USN_JOURNAL` → pending checkpoint `(journal_id, next_usn)`.
2. Full enumeration into an open generation (Milestone 1 path,
   `kind = reconcile` when a generation already exists). The source stays
   `enumerating`/`reconciling`.
3. Replay records from the pending USN to the present **into the open
   generation** (`Catalog::apply_changes` stamps rows it creates or
   re-observes with the open generation so the publish step does not
   tombstone them as unobserved; no checkpoint is stored yet).
4. Publish the generation and store the checkpoint **in one transaction**
   (`ScanSession::finish_with`); start/continue the watcher.

The source is therefore never advertised as complete while overlapping
changes are still pending, and a checkpoint is never durable without the
publication it belongs to. Failure windows:

- the journal wraps during enumeration → the enumerated generation is
  published `degraded` ("another reconciliation is required") with the
  checkpoint cleared, so the watcher reconciles again;
- the feed cannot be read, or a batch cannot be applied → the generation is
  aborted; the previous published generation and its checkpoint (if any)
  stay in force and the source is `degraded` with the reason. Rows written
  by replay batches that did succeed remain visible, exactly like rows
  ingested by the enumeration itself (ADR-0002: rows become visible
  progressively; publication, not row visibility, is what the generation
  flip guarantees). They are observed filesystem truth — rolling them back
  would, for example, resurrect a file the feed saw deleted — and the
  `degraded` state tells consumers the source is not complete until the
  next reconciliation. A watcher that has to re-establish a checkpoint
  waits 30 s after a failed reconciliation before starting another;
- a crash during replay → crash recovery aborts the open generation exactly
  as for any interrupted scan.

For live batches the checkpoint is written **in the same SQLite
transaction** as the catalog rows, aggregate deltas, and outbox rows it
covers (`Catalog::apply_changes`). A crash cannot leave the checkpoint ahead
of durable state. Derived indexes (Milestone 3) consume the outbox, so
"enough durable state exists to replay downstream work" holds without
coupling the feed to index commits.

### Event normalisation

USN records are coalesced per file reference number in USN order (final
state wins), then each affected file is re-read by ID (`OpenFileById` +
`FileIdInfo/BasicInfo/StandardInfo/AttributeTagInfo`). The record supplies
topology (`parent FRN`, name, `RENAME_OLD_NAME` names); the snapshot
supplies truth (sizes, times, link count). `HARD_LINK_CHANGE` triggers a
full link resync via `FindFirstFileNameW`. The feed-neutral vocabulary is
`Link / Unlink / Update / Delete` keyed by `(volume serial, 128-bit id)`, so
FSEvents and fanotify adapters can reuse `apply_changes` unchanged.

Scope: a `Link` whose parent is neither in the catalog nor created earlier
in the same batch is out of scope (another source's subtree, or outside a
sub-directory source) and is skipped *before* any I/O.

Renames/moves are `Unlink` + `Link` of the same object: identity,
generation, and (Milestone 4) content records survive; subtree aggregates
are moved with `AggDelta::from_subtree` (subtract on the old chain, add on
the new), not recomputed. Objects left with no live entry at batch end are
tombstoned (orphan cleanup); directories cascade to their subtree.

### Overflow, journal change, and unavailability

- `ERROR_JOURNAL_ENTRY_DELETED` (overflow) or `ERROR_INVALID_PARAMETER`
  with a stale journal ID → source `degraded` with reason, checkpoint
  cleared, reconciliation scan started (which re-establishes a checkpoint).
  Existing rows stay visible as stale throughout.
- Journal not active / access denied / unsupported → checkpoint cleared,
  watcher stops, source keeps its state with a reason; freshness becomes
  `periodic` and the reconciler rescans on the source's interval.
- Repeated I/O failures on the volume handle → `offline`; a successful
  batch later restores `metadata_complete`/`content_pending`.
- A scan whose **root** is unreachable (not found / transient) aborts and
  sets `offline`; access denied sets `degraded`. Either way the previous
  generation and its rows are preserved.

`freshness = live` is reported only while a watcher is actually consuming
the checkpoint.

### Durable jobs and outbox (migration 2)

`jobs` rows carry source, object generation, stage, priority (1–7 per
ARCHITECTURE 8), attempts, idempotency key, payload, and failure class.
`claim_job` takes the lowest priority number due now under
`BEGIN IMMEDIATE`; transient failures re-queue with exponential backoff
(10 s · 2^attempts, max 6 attempts); deterministic/unsupported/corrupt/
resource-limit failures are terminal; a newer generation supersedes queued
jobs of older generations; `running` jobs are re-queued at startup.
`outbox` rows (`upsert | content | delete`, seq-ordered) are written by
incremental change application; full scans do not write outbox rows — the
search projection (Milestone 3) bulk-builds from the published generation
and then follows the outbox.

### Polling

The watcher polls `FSCTL_READ_USN_JOURNAL` every 500 ms with
`BytesToWaitFor = 0` rather than blocking, so shutdown and cancellation are
prompt. Measured end-to-end visibility (file create → catalog row) is
≈0.5 s; restart catch-up of changes made while stopped is ≈50 ms.

### SMB and generic sources

No live feed. `reconcile_interval_s` (default 6 h) drives periodic rescans
by the reconciler thread; `stale` is set when a scan is overdue by 2×. The
UI and API report `freshness = periodic` so the weaker guarantee is never
hidden.

## Consequences

- A source scanned by the service gets a checkpoint automatically; sources
  scanned by the in-process CLI (`eidos source scan`) have none until the
  service's watcher runs a reconciliation (observed: G:/R: reconciled at
  service start in 1.3–1.7 s).
- Per-event cost is one `OpenFileById` plus a handful of indexed SQLite
  statements; busy volumes are handled in 1 MiB journal batches with one
  transaction per batch.
- Hard-link enumeration needs the file's final path from the handle; files
  without a resolvable path fall back to the single latest name.
