# ADR-0030: Bound checkpoint writes for irrelevant Windows journal activity

Date: 2026-09-06. Status: implemented; full Windows local gate and bounded
before/after measurements passed, review and workload qualification pending.

## Evidence

A two-root synthetic crawl on one OS disk stayed within its reader ceiling,
but its 15-second idle window still processed 113 watcher batches and reported
26 events despite no fixture mutation. The process read 8.49 MB and wrote
0.45 MB. The Windows journal covers the whole volume. Translation emitted
deletions even for identities absent from a source, and the watcher persisted
checkpoints for irrelevant batches. Catalog/checkpoint/log writes on that same
volume can then become input to the watcher. Reader ceilings do not bound this.

## Decision

Filter deletions absent from the source before creating change events. Keep a
volatile read cursor over successfully translated, irrelevant batches. Relevant
source events still apply immediately, atomically with the durable checkpoint;
failed translation or a failed write must retain the position needed to retry.

Flush irrelevant progress when an advancing feed reaches 30 seconds since its
last checkpoint or 16 MiB of journal offset. This is not a periodic idle timer:
if the journal stops producing records, the remaining ignored tail can stay
volatile. One read batch may cross the byte threshold before it is persisted.
Do not log each irrelevant batch, since logging can itself create journal work.

A snapshot failure retains the position unless replaying the record cannot
change its outcome. Access-denied, privilege-not-held, unsupported and
unrepresentable-name objects are counted as unreadable and skipped exactly as
an unlistable file is during a scan; blocking on them instead would stall the
live feed until the journal wrapped and would abort every overlap replay, so
one protected file inside an indexed root could stop the source indefinitely.
Every other failure — including codes the classifier does not recognise, such
as ERROR_IO_DEVICE — keeps the checkpoint, because acknowledging one would drop
a change a retry could still have read. The match is exhaustive so that a new
error kind must choose a side rather than default into dropping updates.

Retrying is bounded. A batch that has failed to translate for two minutes is
not going to become readable by reading it again, so the watcher stops retrying
that position and takes the reconciliation an invalid journal already takes:
Degraded state with the reason, a cleared checkpoint and a recovery scan. That
keeps a failing device from being re-read every two seconds forever, and lets a
scan record the unreadable object as coverage loss instead of the feed silently
stalling. Bounding the retry never shortens it below the window, so a genuinely
transient failure still clears on its own.

The durable checkpoint remains the compare-and-swap fence. A scan/recovery or
journal/volume replacement discards old read-ahead, reopens the proper handle
and clears the failed-batch window, since the next batch is a different
position that must not inherit the old one's failures.
Restart replays the ignored tail; a wrapped/replaced journal still follows the
existing reconciliation path. Never skip unapplied source events to reduce I/O.

An empty batch proves the volume and journal are readable, so an Offline source
takes the same fenced durable turn and state restoration there as on a batch
carrying events. Because the reader parks on journal activity rather than a
timer, that recovery still waits for the volume's next record.

## Limits and qualification

This bounds one source of Windows watcher writes, not all process I/O or physical
disk traffic. Per-source journal reads still observe other volume activity.
The reported watcher position remains the durable position, so it can lag
irrelevant records by design. macOS FSEvents behavior is unchanged.

Require deterministic cursor/deferral/failure/fencing tests, existing live native
change/restart/overflow tests, and comparable before/after synthetic measurements.
Measured resource presets, longer idle and installed qualification remain owed.
