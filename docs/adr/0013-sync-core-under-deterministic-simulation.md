# ADR-0013: Build the sync core under deterministic simulation before transport

Status: accepted
Date: 2026-08-24
Milestone: v1 Sprint 1-B

## Context

Fleet sources are single-writer but routinely disconnect, restore old local
state, change USN journals, and replay messages. A transport-only integration
test cannot enumerate the crash windows around outbox compaction, durable
apply, acknowledgement, and restart. The sync data model must therefore be
executable without sockets, real clocks, or real files before Sprint 2 adds
QUIC and enrollment.

The catalog's current outbox is a notification channel: its rows name an
object, while the follower reads the actual projection state from SQLite.
Sending those pointer-like rows to a central without the source catalog would
not reproduce the state. The source must materialize row images at ship time.

## Decisions

- `eidos-sync` owns explicit clock, transport, timer, and filesystem seams.
  The single-threaded simulator injects loss, duplication, delay, partitions,
  crash/restart, lost timers, and discarded un-fsynced writes from one stable
  seed. Fault plans are validated, minimized with `ddmin`, and serialized as
  versioned Rust-pasteable replay expressions.
- Source order is `(source_id, source_epoch, seq)`. `source_id` is bound to a
  volume/root rather than its current host. `source_epoch` is a UUID fencing
  token. A restore, clone, rebuild, or USN journal-id change installs a fresh
  epoch. The central requests a full source snapshot on epoch change and
  rejects plus records an alarm for a same-epoch rewind.
- The shipper coalesces the unacknowledged suffix by object and sends the final
  materialized row image for each touched object. Thus 500 offline edits to
  one file ship once while the batch still certifies a contiguous sequence
  interval through its high watermark.
- The applier is idempotent under duplicate/overlapping delivery. It commits
  replicated effects and the per-source watermark in one atomic filesystem
  batch and sends the ACK only afterward. The watermark is both the
  correctness/retention gate now and the byte-credit release point in Sprint
  2.
- Acknowledgements are tracked per registered consumer. Acknowledged history
  is discarded only through the oldest watermark. Above that floor, only the
  latest materialized image per object is retained; a tombstone remains until
  every consumer has crossed it.
- If a requested cursor predates retained history, peers compare row-level
  Merkle state over 2^17–2^20 leaves and replace only divergent leaves. Empty
  authoritative leaves repair stale rows and deletions. Epoch changes still
  use an explicit full-source resync because they fence a different source
  incarnation.
- The ordinary suite runs focused and seeded acceptance tests. A scheduled
  release-mode workflow runs at least one million protocol universes, uploads
  every replay record it finds (capped at 20 per run to prevent issue storms),
  and files one deduplicated GitHub task for each recorded failing seed.

## Consequences

- Sprint 2 transport adapters cannot invent alternate ordering, durability,
  or compaction semantics; they carry these messages and implement these
  seams.
- A three-week simulated disconnect reconnects by cursor exchange and a
  compacted materialized suffix without a filesystem crawl. Truncated history
  follows the separately tested Merkle repair path.
- Stored seeds remain durable regression cases because the RNG stream, event
  ordering, fault-plan schema, and replay format are explicitly tested.
- The simulator models atomic durable batches rather than SQLite itself. Real
  adapters must map that operation to one SQLite transaction and preserve the
  ACK-after-commit ordering.
