# ADR-0031: Park drained content workers behind work-specific wakeups

Date: 2026-09-06. Status: implemented; review pending.

## Evidence

After the native-journal fix, a four-worker synthetic candidate had zero idle
writes but used 1.243% of one CPU core. Its 15-second idle still took 126 catalog
writer permits. Every worker attempted an empty claim every 500 ms; each
completed writer permit also woke the catalog follower. The cost grew with
the configured pool despite no queued work or relevant source changes.

## Decision

Idle and surplus workers park on a content-specific epoch/condition variable.
Capture the epoch before checking shutdown, pool size and admission, so a
notification between the work check and parking cannot be lost. Retain a
30-second fallback and the existing bounded error backoff.

The existing coordinator checks for due jobs every 500 ms, off the writer
path. Seven exact priority-range seeks use the existing jobs_queue index;
neither future retries nor historical jobs require a whole-queue aggregate or
a new migration. This also wakes future retries when their due time arrives,
without depending on an unrelated write. Workers still atomically claim jobs
and enforce source/device/policy/global admission; the readiness result is
only a hint. A due backlog blocked by a source or policy can still produce
periodic admission attempts: this change specifically removes per-worker
writer polling after drain, not every coordinator check during active work.

Pause/resume, resizing, explicit retry, resource/source control changes and
shutdown notify the pool explicitly. Do not reuse the general catalog-write
signal: empty claims complete writer turns too, and would cause workers to
wake one another. Do not introduce another idle timer or thread.

## Validation and limits

Require lost-wakeup/broadcast/timeout tests, bounded read-only readiness tests
with large future queues, parked-pool/no-writer-claim checks, newly queued work,
pause/resume/shutdown and the existing worker/retry/admission regressions.
Repeat the same four-worker two-root executable measurement and restart check.

The initial scoped tests and all-target clippy passed. The rebuilt required-web
candidate repeated that fixture: idle writer acquisitions fell from 126 to 6;
CPU from 0.1875 to 0.0625 seconds (1.243% to 0.414% of one core). Both runs had
zero process write bytes and no native source events. The after run read 65,536
process bytes. Pause, saved controls, retained results, forced temporary restart
and explicit resume passed. See the exact hashes, conditions and small query
sample counts in [benchmarks](../benchmarks.md).

The full Windows local gate then passed format, all-target clippy, generated
API, Rust tests/docs, 43 web utility tests, 32 rendered UI tests and the web
production build. The final service regression also covers a future retry
becoming due with no new write/control notification. That follow-up adds tests
and removes a duplicate blocked-reason check; the recorded hash identifies the
preceding measured executable, not a subsequently rebuilt binary.

This does not imply zero CPU, zero I/O, a hard resource quota or a qualified
performance preset. The coordinator, real source changes and maintenance still
perform work. Installed and real-media qualification remain required.
