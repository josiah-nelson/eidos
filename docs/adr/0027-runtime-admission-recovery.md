# ADR-0027: explicit runtime admission ceilings during recovery

Status: accepted, partial recovery implementation

Date: 2026-09-05

## Context

The failed v0.5.0 rollout exposed missing operational controls and excessive
catalog work. A global content pool and per-source caps do not bound concurrent
metadata enumerations or coordinate physical devices. Large preclaimed batches
make pause misleading. Publication acknowledgement failures can strand content
until restart, while full-source aggregation extends each finalization's writer
hold with source size.

## Decision

Expose explicit metadata enumeration width/concurrency and a data-volume
free-space reserve in Activity, API and CLI. Save them with synced file contents
and atomic replacement alongside existing operator markers, before changing
effective settings. Keep the persistence lock separate from the hot status/
admission lock. Reject invalid/unreadable persisted settings at startup rather
than silently increasing work. Live work retains its reservation and drains;
queued enumeration is cancellable before a generation opens.

Reserve a scan slot before native capability/cursor probes and hold it through
replay/publication, so queue time cannot enlarge the journal overlap window.
Default to one concurrent scan and a 1 GiB free-space admission reserve;
retain the configured scan width within a 1–64 range. These are conservative
ceilings, not performance-qualified normal/background/initial-index profiles.

Claim one content file per worker. Preserve pending publication IDs on either
index or catalog failure, stop new extraction, and retry at the commit interval
without rereading source bytes. Determine source completion using the existing
content-state index and existence probes rather than whole-source sums.

Isolate slow OS discovery/free-space probes in single-flight background work.
A caller timeout does not reclaim the running probe's capacity. Do not open
remote/removable/optical roots merely to populate the drive picker. Bound the
advisory release HTTP request including its response body.

## Consequences and remaining limits

More frequent single-file claims add catalog transactions; retain synthetic
and real-workload measurements before selecting performance profiles. A file
or OS syscall already running can still take time. Space sampling is advisory
admission, not a quota: existing scans, native changes, publication and fleet
writes can consume additional space. A stalled sample eventually holds new work.

Per-source caps remain independent, including overlapping roots on the same
device. Shared-device admission, memory/cache visibility, writable exclusions,
cross-stage self-store protection and installed/signed acceptance are not
completed by these limits and remain mandatory recovery work.
