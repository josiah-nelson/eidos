# ADR-0016: Generated universes, a ghost oracle, and history chain fencing

Status: accepted
Date: 2026-08-25
Milestone: post-Sprint-1 hardening, track H1

## Context

[ADR-0013](0013-sync-core-under-deterministic-simulation.md) put the sync
core under deterministic simulation, but the nightly million-seed soak
varied faults around one fixed four-object script with one source, and its
only end-state check was "the replica equals the script's final image". That
universe could not contain an epoch change, a restore from backup, a second
source, or a delete followed by a recreate, and nothing independent of the
protocol said what the central *should* hold after every event.

Two questions had to be answered before real transport is bound:

1. Does the harness find protocol bugs, or only confirm the happy path?
2. Is a same-epoch rewind actually fenced? ADR-0013 fenced it by comparing
   the source head to the central cursor, which a restored source that keeps
   writing will overtake.

## Decisions

### Generated, replayable workloads

- A universe is `(seed, FaultPlan, WorkloadPlan)`. `WorkloadPlan`
  (`eidos-sync-workload/1`) holds one script per source: upserts and deletes
  with skew and hot objects, recreates, `EpochChange` (a fresh incarnation
  whose history restarts from the live rows), and `Checkpoint`/`Rewind`
  (restore an older durable state and keep writing under the same epoch).
  Plans validate (monotonic generations, no orphan rewind) and are drawn
  deterministically from the seed.
- The replay record is now `eidos-sync-replay/2` and carries the workload;
  `/1` records still load. Minimization runs `ddmin` over fault events, then
  over each source's steps (skipping candidates that no longer validate),
  then drops whole sources, to a fixpoint. The nightly gate records the
  minimized universe, not the original.

### Ghost oracle

- A `GhostHistory` replays a source's script without the protocol and keeps
  the exact image after every sequence of every history: each incarnation,
  and each fork a rewind creates (sharing the prefix it was restored from).
- Invariant, checked after every simulated event against *durable* state:
  the central's replica for a source must be the image of one history at the
  central's own durable cursor and epoch. No row may lead the cursor, carry a
  different fork's content, or be live where that history has a delete.
  Tombstones may be absent (retention releases them below the oldest
  watermark) but never wrong.
- End state: every source the central did not have to fence must converge
  to its final incarnation's head with the same live-row count.

### History chain fencing

- Each source incarnation keeps a hash chain over its sequence:
  `chain(0)` is zero and `chain(n) = blake3(chain(n-1) || object ||
  generation || value-or-tombstone)`. The outbox retains hashes from the
  compaction floor to the head.
- Batches carry `after_chain` and `through_chain`; snapshots and repair
  offers carry `through_chain`; `Hello` carries `head_chain`. The central's
  admission state stores the chain hash certified at its cursor.
- Admission is exact-resume: a batch is applied only if `after_seq` equals
  the cursor **and** `after_chain` equals the stored hash. A batch cut before
  the cursor is answered with `Resume` at the cursor rather than merged. A
  matching cursor with a different hash, or a `Hello` whose head equals the
  cursor with a different hash, records a `HistoryFork` alarm and rejects;
  the source stays fenced until it changes epoch. A rewind whose new head
  never exceeds the cursor is still caught by the head/cursor comparison.
- A shipper that receives an acknowledgement or resume point beyond its own
  head does not answer with a batch. It reports its head on the next tick and
  lets the central alarm.

### Invariant precision

- The watermark-monotonic invariant is scoped per `(source, epoch)`: an
  epoch change legitimately restarts the sequence space.

## What the harness found on its first run

Twenty thousand generated universes, before any of the fixes above, produced
five minimized failures:

- the chain hash was computed from the wrong predecessor, so two histories
  that shared a row at sequence *n* shared its "chain" hash, and a fork was
  merged (caught by the oracle, root-caused from a seven-step reproducer);
- a rewind whose new head landed exactly on the central's cursor was never
  compared because no batch was ever sent (the `Hello` head hash);
- a stale batch answered with an acknowledgement beyond the source's head
  produced a timer-free message storm (the no-resend rule);
- the watermark invariant fired on a legitimate epoch change;
- the minimizer accepted "the simulation cannot be built" as a failure.

Each became a permanent regression: a hand-built rewind that overtakes the
watermark must alarm and leave the pre-rewind image, and the same universe
with chain verification disabled must fail the oracle with a merged fork.

## Consequences

- Sprint 2 transport must carry the chain hashes on every message that
  certifies a sequence point; they are part of the wire contract, not an
  optimization.
- The nightly soak now explores multi-source, multi-incarnation, forked
  histories. Its per-seed cost rose; one million universes still fit the
  scheduled budget with margin.
- Deliberate-mutant coverage (hardening H1.6) can now be stated as "the
  oracle fails within N seeds when knob X is flipped"; the chain-verification
  knob is the first such mutant.
- Rewinds below the central's cursor whose new head stays at or below it,
  and epoch changes, remain non-convergent by design: a fenced source needs
  an operator-visible alarm and a new epoch, not silent repair.
