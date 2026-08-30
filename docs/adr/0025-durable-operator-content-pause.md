# ADR-0025: An operator pause on content extraction is durable and stops claiming only

Status: accepted (amends ADR-0005)  
Date: 2026-08-30  
Milestone: 5

## Context

Content extraction is the only background work in the service that reads the
source volumes continuously and at full speed. An operator watching a busy
HDD, a saturated SMB share, or a machine that has to give its I/O to
something else for an hour needs a way to say "stop for now" — and the only
switch that existed was `--no-content`, a process setting chosen at start-up.
Using it meant restarting the service, which is exactly the wrong tool: it
also drops every in-flight batch.

The pipeline already had three things a control has to respect. Workers
claim a batch inside a transaction that reserves per-source concurrency
(ADR-0005 and `source_budget`), so a batch in flight has *paid* for capacity
that a control must not confiscate. The coordinator owns commits and
publication, so extracted text that is not yet committed is not yet
searchable. And a content-index rebuild owns the index writer, during which
claiming is already suspended for an unrelated reason.

Separately, "what is the content pipeline doing" was answered in four places
that could disagree: `/api/health` said nothing, `/api/activity` exposed a
bare `content_enabled` boolean, `eidos activity` printed `content: on|off`,
and the web Activity page rendered its own badge from the same boolean. None
of them could distinguish "extracting", "nothing queued", "draining a claimed
batch", or "waiting for a rebuild" — the states an operator actually needs to
tell apart before concluding something is stuck.

## Decisions

- **The pause gates claiming, and nothing else.** The process switch, pause,
  and rebuild state are checked inside `reserve_and_claim`, while a shared
  admission mutex is held through the catalog claim transaction. Pause and
  resume transitions hold that same mutex through their durable mutation and
  response status. A completed `stopped` response therefore cannot be
  followed by a worker that passed an earlier check. A batch admitted before
  the pause runs to completion in `run_batch`, is committed by the
  coordinator, and is published normally. No job is abandoned, none is left
  `running` for `requeue_running_jobs` to repair, and no extraction that was
  already paid for is thrown away. Interrupting workers mid-batch was
  rejected: it buys a faster stop and costs re-extraction of every file in
  flight.
- **Queue top-up stops with claiming; commits do not.** `top_up_queue` walks
  every source and writes new job rows — the catalog load an operator paused
  to stop. Its pause check and catalog work use the shared admission mutex, so
  top-up cannot act on a stale pre-pause check after the response. The backlog
  is durable, so it is still there at resume. The coordinator keeps committing
  and publishing, because a draining worker's output must reach search rather
  than sit uncommitted until the pause ends.
- **The pause is durable.** It is recorded in `content-pause.json` in the
  data directory, written to a temporary file and renamed into place, and
  removed on resume; the file's presence is the flag. The catalog has no
  settings table, and this mirrors the content index's `rebuild.json`
  (ADR-0005) so both durable operator/recovery states live beside the data
  they describe. A pause that lapsed at the next start was rejected: an
  operator who paused because a volume was busy would silently get the load
  back after a restart or a crash, which is when they least expect it.
  Resuming is always explicit.
- **The marker is written before the flag flips.** If the marker cannot be
  written the call fails and the in-memory flag is untouched, so what the
  operator is told and what a restart will do never diverge. A marker that
  exists but cannot be parsed still means paused; the timestamp inside it is
  advisory and falls back to the load time. Concurrent controls are
  serialised with the same admission mutex, including their temporary-file
  mutation, so identical controls stay idempotent and opposite controls leave
  the marker and in-memory flag in the same order. Startup distinguishes a
  missing marker from any other metadata error: an unreadable marker or data
  directory fails closed as paused instead of silently restoring source I/O.
- **A paused backlog does not defer reconciliation.** Automatic rescans are
  deferred while a content crawl is active (`content crawl active (n queued,
  m running)`). A queue nothing is going to claim must not hold rescans off
  indefinitely, so the deferral now requires the pause to be off as well as
  `--no-content` and the per-source policy to be on. Jobs already `running`
  still defer: those are draining and touching the volume a scan would walk.
  The automatic-scan decision and scan registration use the shared admission
  mutex too, and worker admission rejects a source with an active registered
  scan. If resume wins the mutex, its queued backlog defers the scan; if the
  scan wins, the resumed backlog waits until that scan finishes.
- **One function derives the reported state.**
  `content_control::content_status` returns a `flow`
  (`disabled | stopped | draining | waiting | running`), a
  `search` state (`ready | rebuilding | failed | disabled`), the rebuild
  status, the pause and its timestamp, and a sentence explaining both.
  `/api/health`, `/api/activity`, `eidos content status`, `eidos activity`,
  and the Activity page all render that one value, so they cannot disagree.
  It reads atomics, the budget table, and the rebuild mutex only — no
  catalog or index reads. Health/status reads need no admission gate; a
  control deliberately keeps the gate until this value has been derived so
  its response describes the state that control produced.
- **The reasons are ordered by what the operator can act on.**
  `--no-content` outranks everything, and a rebuild outranks a pause. Resuming
  a `--no-content` process changes nothing, and resuming while a rebuild owns
  the writer still claims nothing, so naming the pause in either case would
  send the operator after the wrong switch. The pause remains recorded in the
  status and becomes the operative reason after the rebuild finishes.

## Consequences

- `POST /api/content/pause`, `POST /api/content/resume`, and
  `GET /api/content/status` answer with the resulting `ContentStatusView`,
  so a caller that pauses a busy service learns in the same response that it
  is `draining` rather than `stopped`. `eidos content pause|resume|status`
  is the CLI over the same three.
- `ActivityView` gains `content`; `content_enabled` is kept, unchanged, for
  compatibility within this schema version (see docs/development.md). `Health`
  gains `content`, making a health check able to report a paused pipeline.
- A pause is machine-wide, not per-source. Per-source control already exists
  as `content_enabled` on the source policy, which is durable in the catalog;
  a second per-source axis would have two switches meaning nearly the same
  thing.
- The marker is a file in the data directory, so a stuck pause can be cleared
  by deleting `content-pause.json` with the service stopped. That is
  deliberate: the same property makes the state inspectable during an
  incident.
- `crates/eidos-service/tests/content_pause.rs` covers pause → drain →
  stopped → resume with a live reservation held across the pause, durability
  of both directions across a restart with the backlog intact, a marker
  present before the service opens, and the reason ordering.
  `watcher.rs` covers the reconciliation deferral for both switches.
