# ADR-0012: Content sink failures are retryable and leave nothing behind

Status: accepted (amends ADR-0005)  
Date: 2026-08-23  
Milestone: 5

## Context

The extraction pipeline streams a file into chunks and hands each chunk to a
sink that writes it to the catalog's `chunks` table and to the content index
writer. Until now the sink returned an untyped `String` error, and the
extractor recorded any sink error as a deterministic extraction failure.

Two things followed from that. A SQLite write error, a full disk, or an
index writer error — none of which say anything about the file — became a
*terminal* content failure (`state:failed`, no retry) instead of a job the
scheduler would retry. And because chunks are flushed in batches of 64 while
the file streams, an attempt that failed after the first batch left chunk
rows and index documents for a generation whose content record said
`failed`, or said nothing at all: the coordinator's next commit made that
partial text searchable.

## Decisions

- The sink's error type is `eidos_content::SinkFailure { class, message }`.
  The extractor propagates it verbatim — the sink's `FailureClass` is never
  reclassified — and marks the outcome `sink_failed`, so a caller can tell a
  storage failure from a decode failure.
- The pipeline's sink error is typed: `SinkError { stage: Catalog | Index,
  object, generation, source }`, where `source` is the underlying
  `SearchError`. Its `chain()` renders the whole `error: cause: cause` chain
  into the job's `last_error`, which the activity view surfaces
  (`recent_failures`). Nothing is flattened to a bare message.
- Sink failures are classified `transient`: the job retries under the normal
  exponential backoff up to `MAX_TRANSIENT_ATTEMPTS`. Failures that *are*
  properties of the file — unreadable, corrupt, undecodable — keep their
  existing classes and stay terminal, with the reason on the content record.
- A failed attempt discards its own partial output before returning:
  the generation's chunk rows are deleted (`Catalog::delete_chunks`) and a
  delete of the object's documents is queued on the index writer. The
  delete is ordered after the attempt's adds, so the next commit removes
  them. Both halves are idempotent, so the retry — in this process, or
  after a restart, where `requeue_running_jobs` and
  `requeue_unfinished_content` bring the job back — can repeat them.
- Publication ordering is tightened rather than replaced. The last batch's
  documents reach the index writer *before* the `indexing` content record
  and the job completion are committed, so the record that
  `mark_content_indexed` publishes cannot exist unless every document of
  that generation is queued behind it. `store_content` also drops any chunk
  row of the same generation beyond the record's `chunk_count`, so a record
  never claims coverage the store does not have.
- Retryable failures write no content record. The object stays `pending`
  and the job carries the class and the error chain, as transient I/O
  failures already did; only a terminal outcome is recorded and published.

## Consequences

- Infrastructure problems no longer poison a catalog: once the disk or the
  writer recovers, the queued jobs drain and the affected files index
  normally. Only genuine content failures end as `state:failed`.
- Search never sees half a generation. The window ADR-0005 already had —
  documents committed by the coordinator before the record is written —
  stays, and is closed by the same commit that applies the cleanup delete.
- One extra `DELETE` per failed attempt, on the failure path only.
- `crates/eidos-search/tests/content_faults.rs` injects the three cases: a
  SQLite write error (a trigger installed on a second connection), an index
  write error after a flushed batch (`SinkFaults`), and the retry that
  succeeds and produces exactly one generation's chunks.
