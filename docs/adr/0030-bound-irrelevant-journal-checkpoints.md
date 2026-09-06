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

Only a *transient* snapshot failure retains the position. An access-denied,
privilege-not-held or unsupported object returns the same failure on every
replay, so it is counted as unreadable and skipped exactly as an unlistable
file is during a scan. Blocking on it instead would stall the live feed until
the journal wrapped and would abort every overlap replay, so one protected file
inside an indexed root could stop the source indefinitely.

The durable checkpoint remains the compare-and-swap fence. A scan/recovery or
journal/volume replacement discards old read-ahead and reopens the proper handle.
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
