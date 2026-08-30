# ADR 0020: Windows observatory collector

## Status

Accepted (2026-08-25).

## Context

The 30-day workload study needs real Windows evidence for the DST feedback
loop, DP-2 (content identity and dedup economics), and DP-3 (repair
economics): change shape and rates, which processes actually consume
files, content chunk behaviour under edits, enumeration cost, and host and
storage shape across physical hosts, VMs, VHD hosts, and servers. Most of
the fleet is Windows, so this lane set has to be the strong one. The
`eidos-observe` contract crate (ADR-0014) already fixes the privacy
boundary: keyed tokens, closed buckets, a bounded spool, and an exact
pre-export inspector.

## Decision

1. Extend the `eidos-observation/1` schema additively rather than fork it:
   Windows capabilities (USN, ETW, SYSTEM, elevation, and study-key
   availability as independent facts), volume inventory, feed health,
   per-interval rate and reason summaries with coalescing windows, access
   telemetry per process class, sampled content economics, enumeration
   probes, and resource samples that carry the lane states in force.
   Aggregate counts are exact because they name no object; every per-object
   scalar stays bucketed and every identity stays a keyed token.
   The later explicit `collector` process-class value advances the serialized
   contract to `eidos-observation/2`; adding an enum value under the v1 label
   would make a v1 reader reject a bundle that still claimed to be v1. V2
   readers retain support for the additive v1 bundle shape.
2. Run the collector as a LocalSystem service (`eidos-collector`) because
   reading every local USN journal needs volume-management rights, and
   drive it over a local named pipe with bounded JSON frames. The same
   daemon runs in the foreground for testing. No listener, upload, or
   remote control exists.
3. Protect the study key with DPAPI in machine scope so the service and an
   elevated `observe init` share it; allow importing a cohort key so
   content fingerprints compare across hosts.
4. Derive object identity from `(volume GUID, file reference number)` and
   never resolve a file path. Directory depth comes from one attribute-only
   open of the parent by id with `OPEN_REPARSE_POINT`, memoised and skipped
   for oversized batches, so a burst cannot become a burst of opens.
5. Treat every USN discontinuity as a recorded capture gap (overflow,
   recreation, unavailable feed, unclean shutdown, clock jump) and resume
   from the oldest retained record rather than the head.
6. Use ETW for access telemetry (the Windows analogue of the macOS Endpoint
   Security lane), decoded through TDH metadata so payload layouts come
   from the provider manifest at run time, attributed to coarse process
   classes, and run only in randomized windows so control periods exist and
   the workload cannot align with the observation.
7. Measure content on a deterministic sample of closed-after-write files
   under a byte budget, refusing placeholder, offline, and reparse-point
   objects on the basis of a non-hydrating attribute snapshot, and record
   chunk reuse against the previous observation of the same object.

## Consequences

- Windows and macOS bundles share one schema and inspector; the platform
  differences are visible as capability declarations and record families,
  not hidden by a pretend-common cursor.
- The USN lane is always on and cheap (one blocking ioctl per batch, one
  spool transaction per batch); ETW, content, and enumeration are explicit
  L2 lanes whose observer effect is measurable against control periods.
- The collector's own spool writes appear in the access telemetry under the
  indexer class; that is honest data, since eidos itself will run on these
  hosts.
- Hard-link counts are not part of the enumeration probe (the lister does
  not report them) and per-file size lookups are skipped for oversized
  batches, so those fields can read as unknown under bursts.
- This is a private-fleet study artifact, not a release deliverable; it is
  installed from a local build.

## Rejected alternatives

Resolving paths to classify subtrees (identity must not depend on names,
and resolution costs an open per record). Hand-written ETW payload layouts
(version-dependent; TDH describes them). Continuous ETW by default (no
control period, and the observed workload could align with the
observation). Hydrating placeholders to measure content (measurement must
not cause the workload). A spool transaction per USN record (a durable
commit per record cannot keep up with a build).
