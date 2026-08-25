# ADR-0018: macOS change feed

Status: accepted
Date: 2026-08-25

## Context

macOS sources were kept fresh only by periodic reconciliation scans: correct,
but it re-reads a whole tree to notice one edited file, and the catalog is
stale for as long as the interval. The Windows agent has had a live feed since
Milestone 2, driven by the USN journal.

FSEvents is the macOS equivalent in role but not in shape, and the differences
decide the design:

| | USN journal | FSEvents |
|---|---|---|
| unit | one record per change, with reason flags | one notification per *path* that changed at least once |
| identity | file reference number in the record | none; the path is all you get |
| ordering | monotonic byte offsets in a per-volume journal | per-volume event ids, coalesced over a latency window |
| loss | journal wrap, detectable by journal id | explicit `MustScanSubDirs` / `UserDropped` / `KernelDropped` flags |
| history | bounded ring the agent must read before it ages out | durable per-volume store the agent can re-open at a stored id |

Pretending these are one abstraction would either force FSEvents to invent
records it does not have, or force the USN path to give up records it does.

## Decision

**Adapters, not a shared cursor.** `eidos-scanner::fsevents` owns the stream,
its cursor, and its loss semantics, the way `usn` owns the journal's. The
service holds one watcher loop per platform over the same catalog contract
(`ChangeEvent` was already identity-keyed and platform-neutral).

**The cursor carries the event store's identity.**
`FSEventsCopyUUIDForDevice` returns a UUID that changes whenever stored
history stops meaning anything — the store was purged, the disk erased, or the
id counter wrapped. A cursor is `{store_uuid, event_id}`, and a stream is only
resumed while the UUID still matches; otherwise the source degrades and
reconciles. A `NULL` UUID means the volume keeps no history at all (a
read-only volume, for instance), so no cursor is issued and the source stays
on periodic reconciliation. An event id alone would look perfectly valid while
silently skipping every change since the store was replaced.

**No overlap replay before publishing.** The Windows sequence reads the
records that overlapped enumeration *before* it publishes, because the journal
position keeps advancing and old records age out. FSEvents replays from a
stored id on demand, so the macOS sequence takes the cursor *before*
enumerating and publishes it with the generation. The cursor is then always
behind what the published generation contains, which is the safe direction:
replaying a change the walk already saw is idempotent, while skipping one is
data loss.

**The filesystem, not the notification, says what is true.** Each notified
path is re-read. A path that still exists becomes a `Link` under its parent's
identity, plus `Unlink`s for any catalog entry that named the same object
somewhere else — which is how a rename is recognised without the paired
"old name" record USN provides. Paths that no longer exist are resolved
against the catalog, but only *after* every existing path in the batch has been
processed: an object found living elsewhere in the same batch has moved, and
deleting it would take a live subtree with it. What remains genuinely gone
becomes an `Unlink` (a file may still have other links) or a `Delete` (a
directory takes its subtree).

**A directory the catalog has never seen is enumerated.** A subtree *moved
into* the tree produces one notification for its new root and none for the
children that came with it. Reading that subtree is bounded; a move too large
to describe as one batch becomes a reconciliation scan instead.

**A batch is only acknowledged when it was fully read.** A retryable failure
while re-reading a notified path means the batch describes less than it was
asked to. What was read is still applied — the filesystem said so — but the
cursor stays put, so a restart replays those paths; re-reading a path that has
not changed since is idempotent. A permanent failure (denied, malformed) does
not hold the cursor, because it would never clear.

**A native feed cursor is recognised by being one, not by its platform.**
Freshness and the reconciliation timer both asked whether the checkpoint was a
USN journal, which reports every macOS source as periodic and rescans it on a
timer while its feed is applying every change. Both now ask whether the
checkpoint is any native feed cursor.

**Every loss signal funnels into one path.** `MustScanSubDirs`, `UserDropped`,
`KernelDropped`, `EventIdsWrapped`, `RootChanged`, mount changes, and this
process failing to take delivery all degrade the source, clear the cursor, and
start a reconciliation scan. The bounded delivery queue drops batches rather
than blocking the FSEvents callback, because blocking there stalls delivery
for every client of `fseventsd` on the machine.

## Consequences

- macOS sources are live: an edit is in the catalog in about a second, and a
  restart replays what happened while the agent was down rather than
  re-reading the tree.
- Watcher status is feed-neutral. `WatcherView` reports which feed it is
  driven by and its position in that feed, instead of a field named for the
  USN journal, and the web UI names the feed it is actually watching.
- The stream is bound directly rather than through the observatory
  collector's async binding: the watcher is a blocking thread with no
  executor, and it needs the event-store identity, which that binding does not
  expose. Collapsing the two onto this one is worth doing once the collector's
  study window closes.
- The translator re-reads each notified path. That is more syscalls per change
  than the USN path, and it is what makes the design robust to coalescing: no
  intermediate state is assumed, only the current one is recorded.
- Sources on volumes without an event store are unchanged: they publish
  without a cursor and stay on periodic reconciliation, with the reason
  recorded on the source.

## Alternatives considered

- **`kqueue`/`FSEvents` per directory.** Descriptor-per-directory watching
  does not scale to a corpus-sized tree and has no history to resume from.
- **Endpoint Security as the agent's feed.** It is a notification firehose
  requiring a restricted entitlement, and the observatory already uses it for
  measurement. An indexing agent needs *what changed*, not *what was accessed*.
- **Trusting the flags instead of re-reading.** FSEvents flags describe what
  happened to a path at some point in the window, not what is true now; a
  create-then-delete inside one window would leave the catalog holding a file
  that does not exist.
